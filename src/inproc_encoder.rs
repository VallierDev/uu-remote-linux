//! In-process X11 capture and H.264 encoding through libavdevice/libavcodec.
//! It replaces the `ffmpeg` child process with the same codec options, so the viewer
//! still receives the same Annex-B access units (AUD, SPS/PPS, IDR, P).
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};
use ffmpeg_next as ff;
use ff::format::{format::Format, sample, Pixel, Sample};
use ff::ChannelLayout;
use ff::util::picture;
use ff::{codec, encoder, frame, software::scaling, Dictionary, Packet, Rational};
use tokio::sync::mpsc;

pub struct Config {
    pub display: String,
    pub input: (u32, u32),
    pub output: Option<(u32, u32)>,
    pub fps: u32,
    pub gop: u32,
    pub cq: u32,
    pub target_bps: i64,
    pub max_bps: i64,
    pub buffer_bits: i64,
    pub gpu: bool,
}

/// `UU_VIDEO_PIPELINE=process` keeps the original ffmpeg child-process path.
pub fn disabled() -> bool {
    std::env::var("UU_VIDEO_PIPELINE").is_ok_and(|v| v == "process")
}

/// In-process capture is used unless the process path is requested, or a bundled FFmpeg prefix
/// (`UU_FFMPEG_ROOT` / config `ffmpeg_root`) is set: that build was chosen on purpose, so keep using it.
pub fn selected() -> bool {
    !disabled() && crate::config::ffmpeg_root().is_none()
}

fn text(error: ff::Error) -> String { error.to_string() }

fn ensure_init() -> Result<(), String> {
    static INIT: Once = Once::new();
    let mut result = Ok(());
    INIT.call_once(|| {
        if let Err(error) = ff::init() { result = Err(text(error)); }
        ff::device::register_all();
    });
    result
}

/// Sends one access unit per encoder packet. Returns `Ok(false)` when the receiver is gone.
fn drain(encoder: &mut encoder::Video, packet: &mut Packet, units: &mpsc::Sender<Vec<u8>>) -> Result<bool, String> {
    loop {
        match encoder.receive_packet(packet) {
            Ok(()) => {
                let data = packet.data().unwrap_or_default().to_vec();
                if units.blocking_send(data).is_err() { return Ok(false); }
            }
            Err(ff::Error::Other { errno }) if errno == libc::EAGAIN => return Ok(true),
            Err(error) => return Err(text(error)),
        }
    }
}

/// Runs until `stop` is set or the receiver is dropped. A set `idr` flag forces the next frame to be an IDR.
pub fn run(config: Config, units: mpsc::Sender<Vec<u8>>, stop: Arc<AtomicBool>, idr: Arc<AtomicBool>) -> Result<(), String> {
    ensure_init()?;
    let name = CString::new("x11grab").map_err(|e| e.to_string())?;
    let raw = unsafe { ff::sys::av_find_input_format(name.as_ptr()) };
    if raw.is_null() { return Err("x11grab input is not available in this FFmpeg build".into()); }
    let input_format = Format::Input(unsafe { ff::format::format::Input::wrap(raw as *mut _) });
    let mut capture_options = Dictionary::new();
    capture_options.set("framerate", &config.fps.to_string());
    capture_options.set("video_size", &format!("{}x{}", config.input.0, config.input.1));
    // The viewer draws its own pointer; do not bake a second one into the video.
    capture_options.set("draw_mouse", "0");
    let mut input = ff::format::open_with(&config.display, &input_format, capture_options).map_err(text)?.input();

    let stream = input.streams().best(ff::media::Type::Video).ok_or("X11 capture has no video stream")?;
    let index = stream.index();
    let mut decoder = codec::context::Context::from_parameters(stream.parameters()).map_err(text)?.decoder().video().map_err(text)?;

    let (width, height) = config.output.unwrap_or(config.input);
    let codec = encoder::find_by_name(if config.gpu { "h264_nvenc" } else { "libx264" })
        .ok_or("H.264 encoder is not available in this FFmpeg build")?;
    // Allocate for this codec so FFmpeg fills its defaults; libx264 rejects a context allocated without one.
    let mut context = unsafe { codec::context::Context::wrap(ff::sys::avcodec_alloc_context3(codec.as_ptr()), None) }.encoder().video().map_err(text)?;
    context.set_width(width);
    context.set_height(height);
    context.set_format(Pixel::YUV420P);
    context.set_time_base(Rational::new(1, config.fps as i32));
    context.set_frame_rate(Some(Rational::new(config.fps as i32, 1)));
    context.set_gop(config.gop);
    context.set_max_b_frames(0);
    context.set_bit_rate(config.target_bps as usize);
    context.set_max_bit_rate(config.max_bps as usize);
    // The CLI's -maxrate/-bufsize map to these raw fields, which the encoder wrappers read.
    unsafe {
        let raw = context.as_mut_ptr();
        (*raw).rc_max_rate = config.max_bps;
        (*raw).rc_buffer_size = config.buffer_bits as i32;
    }
    let cq = config.cq.to_string();
    let gop = config.gop.to_string();
    let mut options = Dictionary::new();
    if config.gpu {
        for (key, value) in [("preset", "p4"), ("tune", "ull"), ("profile", "baseline"), ("rc", "vbr"),
            ("rc-lookahead", "0"), ("zerolatency", "1"), ("delay", "0"), ("aud", "1"), ("forced-idr", "1")] {
            options.set(key, value);
        }
        options.set("cq", &cq);
    } else {
        let x264 = format!("aud=1:repeat-headers=1:scenecut=0:keyint={gop}:min-keyint={gop}");
        for (key, value) in [("preset", "superfast"), ("tune", "zerolatency"), ("profile", "baseline"), ("threads", "4")] {
            options.set(key, value);
        }
        options.set("crf", &cq);
        options.set("x264-params", &x264);
    }
    let mut encoder = context.open_as_with(codec, options).map_err(text)?;

    let mut scaler: Option<(scaling::Context, (Pixel, u32, u32))> = None;
    let mut decoded = frame::Video::empty();
    let mut yuv = frame::Video::new(Pixel::YUV420P, width, height);
    let mut packet = Packet::empty();
    let started = Instant::now();
    let frame_interval = Duration::from_nanos(1_000_000_000 / u64::from(config.fps));
    let mut last_at: Option<Duration> = None;
    let mut last_slot: i64 = -1;
    for (stream, input_packet) in input.packets() {
        if stop.load(Ordering::Relaxed) { return Ok(()); }
        if stream.index() != index { continue; }
        decoder.send_packet(&input_packet).map_err(text)?;
        match decoder.receive_frame(&mut decoded) {
            Ok(()) => {}
            Err(ff::Error::Other { errno }) if errno == libc::EAGAIN => continue,
            Err(error) => return Err(text(error)),
        }
        // x11grab catches up after a stall by grabbing back-to-back. A frame arriving within half
        // a frame interval of the previous one is part of that burst and is dropped, as the old
        // `-fps_mode vfr` did. Normal jitter stays well above that threshold.
        let now = started.elapsed();
        if last_at.is_some_and(|previous| now - previous < frame_interval / 2) { continue; }
        last_at = Some(now);
        let slot = ((now.as_micros() * u128::from(config.fps) + 500_000) / 1_000_000) as i64;
        let slot = slot.max(last_slot + 1);
        last_slot = slot;

        let key = (decoded.format(), decoded.width(), decoded.height());
        if scaler.as_ref().map(|(_, k)| *k) != Some(key) {
            let context = scaling::Context::get(key.0, key.1, key.2, Pixel::YUV420P, width, height, scaling::Flags::BILINEAR).map_err(text)?;
            scaler = Some((context, key));
        }
        if let Some((context, _)) = scaler.as_mut() { context.run(&decoded, &mut yuv).map_err(text)?; }

        yuv.set_pts(Some(slot));
        yuv.set_kind(if idr.swap(false, Ordering::AcqRel) { picture::Type::I } else { picture::Type::None });
        encoder.send_frame(&yuv).map_err(text)?;
        if !drain(&mut encoder, &mut packet, &units)? { return Ok(()); }
    }
    Ok(())
}

/// Encodes a PulseAudio/PipeWire source to Opus in this process. Each Opus packet is sent as is.
pub fn run_audio(source: &str, units: mpsc::Sender<bytes::Bytes>, stop: Arc<AtomicBool>) -> Result<(), String> {
    ensure_init()?;
    let name = CString::new("pulse").map_err(|e| e.to_string())?;
    let raw = unsafe { ff::sys::av_find_input_format(name.as_ptr()) };
    if raw.is_null() { return Err("pulse input is not available in this FFmpeg build".into()); }
    let input_format = Format::Input(unsafe { ff::format::format::Input::wrap(raw as *mut _) });
    let mut capture_options = Dictionary::new();
    capture_options.set("fragment_size", "1920");
    let mut input = ff::format::open_with(&source, &input_format, capture_options).map_err(text)?.input();

    let stream = input.streams().best(ff::media::Type::Audio).ok_or("Pulse source has no audio stream")?;
    let index = stream.index();
    let mut decoder = codec::context::Context::from_parameters(stream.parameters()).map_err(text)?.decoder().audio().map_err(text)?;

    let codec = encoder::find_by_name("libopus").ok_or("Opus encoder is not available in this FFmpeg build")?;
    let mut context = unsafe { codec::context::Context::wrap(ff::sys::avcodec_alloc_context3(codec.as_ptr()), None) }.encoder().audio().map_err(text)?;
    context.set_rate(48_000);
    context.set_format(Sample::I16(sample::Type::Packed));
    // FFmpeg 6 reads the AVChannelLayout field; the deprecated channel_layout alone makes libopus fail with EINVAL.
    unsafe { ff::sys::av_channel_layout_default(&mut (*context.as_mut_ptr()).ch_layout, 2) };
    context.set_time_base(Rational::new(1, 48_000));
    context.set_bit_rate(128_000);
    let mut options = Dictionary::new();
    options.set("application", "lowdelay");
    options.set("frame_duration", "20");
    let mut encoder = context.open_as_with(codec, options).map_err(text)?;
    // libopus takes fixed-size frames: 960 samples per channel for 20 ms.
    let frame_samples = match encoder.frame_size() as usize { 0 => 960, n => n };

    let mut resampler = None;
    let mut decoded = frame::Audio::empty();
    let mut converted = frame::Audio::empty();
    let mut fifo: Vec<i16> = Vec::new();
    let mut pts: i64 = 0;
    let mut packet = Packet::empty();
    for (stream, input_packet) in input.packets() {
        if stop.load(Ordering::Relaxed) { return Ok(()); }
        if stream.index() != index { continue; }
        decoder.send_packet(&input_packet).map_err(text)?;
        loop {
            match decoder.receive_frame(&mut decoded) {
                Ok(()) => {}
                Err(ff::Error::Other { errno }) if errno == libc::EAGAIN => break,
                Err(error) => return Err(text(error)),
            }
            // The decoder may leave the deprecated channel_layout unset; derive it from the channel count.
            let source_layout = ChannelLayout::default(i32::from(decoded.channels()));
            if resampler.is_none() {
                resampler = Some(ff::software::resampling::Context::get(
                    decoded.format(), source_layout, decoded.rate(),
                    Sample::I16(sample::Type::Packed), ChannelLayout::STEREO, 48_000,
                ).map_err(text)?);
            }
            // Keep both layout fields of the decoded frame in step with the resampler's input layout.
            decoded.set_channel_layout(source_layout);
            unsafe { ff::sys::av_channel_layout_default(&mut (*decoded.as_mut_ptr()).ch_layout, i32::from(decoded.channels())) };
            if let Some(resampler) = resampler.as_mut() { resampler.run(&decoded, &mut converted).map_err(text)?; }
            let samples = converted.samples() * 2;
            fifo.extend(converted.data(0)[..samples * 2].chunks_exact(2).map(|b| i16::from_ne_bytes([b[0], b[1]])));

            while fifo.len() >= frame_samples * 2 {
                let mut out = frame::Audio::new(Sample::I16(sample::Type::Packed), frame_samples, ChannelLayout::STEREO);
                unsafe { ff::sys::av_channel_layout_default(&mut (*out.as_mut_ptr()).ch_layout, 2) };
                out.set_rate(48_000);
                out.set_pts(Some(pts));
                for (dst, src) in out.data_mut(0)[..frame_samples * 4].chunks_exact_mut(2).zip(fifo.drain(..frame_samples * 2)) {
                    dst.copy_from_slice(&src.to_ne_bytes());
                }
                pts += frame_samples as i64;
                encoder.send_frame(&out).map_err(text)?;
                loop {
                    match encoder.receive_packet(&mut packet) {
                        Ok(()) => {
                            let data = bytes::Bytes::copy_from_slice(packet.data().unwrap_or_default());
                            if units.blocking_send(data).is_err() { return Ok(()); }
                        }
                        Err(ff::Error::Other { errno }) if errno == libc::EAGAIN => break,
                        Err(error) => return Err(text(error)),
                    }
                }
            }
        }
    }
    Ok(())
}
