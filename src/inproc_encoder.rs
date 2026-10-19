//! In-process X11 capture and H.264/HEVC encoding through libavdevice/libavcodec.
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
use tokio::sync::{Semaphore, OwnedSemaphorePermit};
use std::collections::VecDeque;

use crate::damage_capture::DamageCapture;

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
    pub encoder: crate::video::Encoder,
    pub codec: crate::capture_setting::VideoCodec,
    /// Capture on XDamage changes (XShm) instead of x11grab; `UU_CAPTURE=grab` turns it off.
    pub damage: bool,
}

/// `UU_VIDEO_PIPELINE=process` keeps the original ffmpeg child-process path.
pub fn disabled() -> bool {
    std::env::var("UU_VIDEO_PIPELINE").is_ok_and(|v| v == "process")
}

/// Damage-driven capture is on unless `UU_CAPTURE=grab` asks for x11grab.
pub fn damage_enabled() -> bool {
    std::env::var("UU_CAPTURE").map_or(true, |v| v != "grab")
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

pub struct EncodedUnit { pub data: Vec<u8>, pub permit: OwnedSemaphorePermit, pub completed_at: Instant }

fn acquire(budget: &Arc<Semaphore>, units: &mpsc::Sender<EncodedUnit>) -> Option<OwnedSemaphorePermit> {
    tokio::runtime::Handle::current().block_on(async {
        tokio::select! {
            permit = budget.clone().acquire_owned() => permit.ok(),
            _ = units.closed() => None,
        }
    })
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    #[tokio::test]
    async fn closing_encoder_output_wakes_a_blocked_capture() {
        let budget=Arc::new(Semaphore::new(0));
        let (tx,rx)=mpsc::channel(1);
        let worker=tokio::task::spawn_blocking(move || acquire(&budget,&tx));
        tokio::task::yield_now().await;
        drop(rx);
        assert!(tokio::time::timeout(Duration::from_secs(1),worker).await.unwrap().unwrap().is_none());
    }
}

fn drain(encoder: &mut encoder::Video, packet: &mut Packet, units: &mpsc::Sender<EncodedUnit>, permits: &mut VecDeque<OwnedSemaphorePermit>) -> Result<bool, String> {
    loop {
        match encoder.receive_packet(packet) {
            Ok(()) => {
                let data = packet.data().unwrap_or_default().to_vec();
                let permit = permits.pop_front().ok_or("Encoder returned an unbudgeted access unit")?;
                if units.blocking_send(EncodedUnit {data, permit, completed_at: Instant::now()}).is_err() { return Ok(false); }
            }
            Err(ff::Error::Other { errno }) if errno == libc::EAGAIN => return Ok(true),
            Err(error) => return Err(text(error)),
        }
    }
}

/// Opens the selected video encoder for `config`. Returns the encoder, its input pixel format and output size.
fn open_encoder(config: &Config) -> Result<(encoder::Video, Pixel, u32, u32), String> {
    // H.264/HEVC 4:2:0 and NVENC need even dimensions; an odd native capture is padded (see run_damage).
    let (width, height) = config.output.unwrap_or(config.input);
    let (width, height) = (width + width % 2, height + height % 2);
    let codec = encoder::find_by_name(config.encoder.name_for(config.codec))
        .ok_or_else(|| format!("{} is not available in this FFmpeg build", config.encoder.name_for(config.codec)))?;
    // Allocate for this codec so FFmpeg fills its defaults; libx264 rejects a context allocated without one.
    let mut context = unsafe { codec::context::Context::wrap(ff::sys::avcodec_alloc_context3(codec.as_ptr()), None) }.encoder().video().map_err(text)?;
    context.set_width(width);
    context.set_height(height);
    // NVENC takes packed BGR0 directly, so the GPU path skips the CPU colour conversion to YUV420P.
    let pixel = match config.encoder { crate::video::Encoder::Nvenc => Pixel::BGRZ, crate::video::Encoder::Vaapi => Pixel::NV12, _ => Pixel::YUV420P };
    context.set_format(if config.encoder == crate::video::Encoder::Vaapi { Pixel::VAAPI } else { pixel });
    if config.encoder == crate::video::Encoder::Vaapi { attach_vaapi(&mut context, width, height)?; }
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
    let hevc = config.codec == crate::capture_setting::VideoCodec::H265;
    if config.encoder == crate::video::Encoder::Vaapi {
        for (key,value) in [("profile", if hevc { "main" } else { "constrained_baseline" }), ("rc_mode", "VBR"), ("async_depth", "1"), ("aud", "1")] { options.set(key,value); }
        options.set("qmin", &cq);
    } else if config.encoder == crate::video::Encoder::Nvenc {
        for (key, value) in [("preset", "p4"), ("tune", "ull"), ("profile", if hevc { "main" } else { "baseline" }), ("rc", "vbr"),
            ("rc-lookahead", "0"), ("zerolatency", "1"), ("delay", "0"), ("aud", "1"), ("forced-idr", "1")] {
            options.set(key, value);
        }
        options.set("cq", &cq);
    } else if hevc {
        for (key, value) in [("preset", "superfast"), ("tune", "zerolatency"), ("profile", "main"), ("threads", "4"), ("forced-idr", "1")] {
            options.set(key, value);
        }
        options.set("crf", &cq);
        options.set("x265-params", &crate::video::x265_params(&gop));
    } else {
        let x264 = format!("aud=1:repeat-headers=1:scenecut=0:keyint={gop}:min-keyint={gop}");
        for (key, value) in [("preset", "superfast"), ("tune", "zerolatency"), ("profile", "baseline"), ("threads", "4")] {
            options.set(key, value);
        }
        options.set("crf", &cq);
        options.set("x264-params", &x264);
    }
    let encoder = context.open_as_with(codec, options).map_err(text)?;
    Ok((encoder, pixel, width, height))
}

// Each codec owns a reference to its hardware frame pool. Every error path releases
// the temporary device/pool refs; AVCodecContext releases its own ref on drop.
fn attach_vaapi(context: &mut encoder::video::Video, width: u32, height: u32) -> Result<(), String> {
    use ff::sys::*;
    let device_name = CString::new(crate::video::vaapi_device()).map_err(|_| "Invalid VAAPI device")?;
    unsafe {
        let mut device = std::ptr::null_mut();
        let result = av_hwdevice_ctx_create(&mut device, AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI, device_name.as_ptr(), std::ptr::null_mut(), 0);
        if result < 0 { return Err(format!("VAAPI device initialization failed: {}", text(ff::Error::from(result)))); }
        let mut pool = av_hwframe_ctx_alloc(device);
        av_buffer_unref(&mut device);
        if pool.is_null() { return Err("VAAPI frame pool allocation failed".into()); }
        let frames = (*pool).data as *mut AVHWFramesContext;
        (*frames).format = AVPixelFormat::AV_PIX_FMT_VAAPI;
        (*frames).sw_format = AVPixelFormat::AV_PIX_FMT_NV12;
        (*frames).width = width as i32;
        (*frames).height = height as i32;
        (*frames).initial_pool_size = 8;
        let result = av_hwframe_ctx_init(pool);
        if result < 0 { av_buffer_unref(&mut pool); return Err(format!("VAAPI frame pool initialization failed: {}", text(ff::Error::from(result)))); }
        (*context.as_mut_ptr()).hw_frames_ctx = pool;
    }
    Ok(())
}

fn send_video_frame(encoder: &mut encoder::Video, frame: &frame::Video, kind: crate::video::Encoder) -> Result<(), String> {
    if kind != crate::video::Encoder::Vaapi { return encoder.send_frame(frame).map_err(text); }
    let mut uploaded = frame::Video::empty();
    unsafe {
        let result = ff::sys::av_hwframe_get_buffer((*encoder.as_ptr()).hw_frames_ctx, uploaded.as_mut_ptr(), 0);
        if result < 0 { return Err(text(ff::Error::from(result))); }
        let result = ff::sys::av_hwframe_transfer_data(uploaded.as_mut_ptr(), frame.as_ptr(), 0);
        if result < 0 { return Err(text(ff::Error::from(result))); }
        let result = ff::sys::av_frame_copy_props(uploaded.as_mut_ptr(), frame.as_ptr());
        if result < 0 { return Err(text(ff::Error::from(result))); }
    }
    encoder.send_frame(&uploaded).map_err(text)
}

/// Runs until `stop` is set or the receiver is dropped. A set `idr` flag forces the next frame to be an IDR.
pub fn run(config: Config, units: mpsc::Sender<EncodedUnit>, stop: Arc<AtomicBool>, idr: Arc<AtomicBool>, budget: Arc<Semaphore>) -> Result<(), String> {
    ensure_init()?;
    if config.damage {
        match DamageCapture::open(&config.display) {
            Ok(capture) => return run_damage(config, capture, units, stop, idr, budget),
            Err(error) => log::warn!("Damage-driven capture unavailable, using x11grab: {error}"),
        }
    }
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

    let (mut encoder, pixel, width, height) = open_encoder(&config)?;

    let mut scaler: Option<(scaling::Context, (Pixel, u32, u32))> = None;
    let mut decoded = frame::Video::empty();
    let mut converted = frame::Video::empty();
    let mut packet = Packet::empty();
    let started = Instant::now();
    let frame_interval = Duration::from_nanos(1_000_000_000 / u64::from(config.fps));
    let mut last_at: Option<Duration> = None;
    let mut last_slot: i64 = -1;
    let mut permits = VecDeque::with_capacity(4);
    let mut packets = input.packets();
    loop {
        if stop.load(Ordering::Relaxed) { return Ok(()); }
        let Some(permit) = acquire(&budget, &units) else { return Ok(()); };
        let Some((stream, input_packet)) = packets.next() else { return Ok(()); };
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

        // Pass the capture buffer straight through when it already matches the encoder input.
        let frame = if decoded.format() == pixel && decoded.width() == width && decoded.height() == height {
            &mut decoded
        } else {
            let key = (decoded.format(), decoded.width(), decoded.height());
            if scaler.as_ref().map(|(_, k)| *k) != Some(key) {
                let context = scaling::Context::get(key.0, key.1, key.2, pixel, width, height, scaling::Flags::BILINEAR).map_err(text)?;
                scaler = Some((context, key));
            }
            if let Some((context, _)) = scaler.as_mut() { context.run(&decoded, &mut converted).map_err(text)?; }
            &mut converted
        };

        frame.set_pts(Some(slot));
        frame.set_kind(if idr.swap(false, Ordering::AcqRel) { picture::Type::I } else { picture::Type::None });
        send_video_frame(&mut encoder, frame, config.encoder)?;
        permits.push_back(permit);
        if !drain(&mut encoder, &mut packet, &units, &mut permits)? { return Ok(()); }
    }
}

/// Damage-driven loop. Damage only marks the picture dirty; a dirty picture is captured and encoded on
/// the next fixed frame tick, so the stream keeps a steady cadence while the screen moves. The viewer's
/// jitter buffer grows with irregular arrivals, and irregular sends were what raised its latency.
fn run_damage(config: Config, mut capture: DamageCapture, units: mpsc::Sender<EncodedUnit>, stop: Arc<AtomicBool>, idr: Arc<AtomicBool>, budget: Arc<Semaphore>) -> Result<(), String> {
    let (mut encoder, pixel, width, height) = open_encoder(&config)?;
    let (cap_width, cap_height) = capture.size();
    // Native size uses the even encoder size. The extra row/column is never overwritten, so it stays black.
    let native = config.output.is_none();
    let (frame_width, frame_height) = if native { (width, height) } else { (cap_width, cap_height) };
    let mut captured = frame::Video::new(Pixel::BGRZ, frame_width, frame_height);
    if native { captured.data_mut(0).fill(0); }
    let mut converted = frame::Video::empty();
    let mut scaler = None;
    let mut packet = Packet::empty();
    let started = Instant::now();
    let frame_interval = Duration::from_nanos(1_000_000_000 / u64::from(config.fps));
    // Keepalive gaps are random between 15 and 25 fps: a steady 1 fps reads as a stall to the viewer.
    let mut seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.subsec_nanos()) | 1;
    let mut keepalive = random_keepalive(&mut seed);
    let mut dirty = true;
    let mut next_tick = Instant::now();
    let mut last_send = Instant::now();
    let mut last_slot: i64 = -1;
    let mut permits = VecDeque::with_capacity(4);
    let locked = crate::session_lock::watch_locked();
    let mut last_locked = *locked.borrow();
    loop {
        if stop.load(Ordering::Relaxed) { return Ok(()); }
        let deadline = if dirty { next_tick } else { last_send + keepalive };
        let wait = deadline.saturating_duration_since(Instant::now());
        // A change marks the picture dirty and falls through, so a client that damages the root constantly
        // cannot keep an overdue tick waiting.
        if capture.wait(wait.max(Duration::from_millis(1)))? { dirty = true; }
        let now = Instant::now();
        if dirty && now < next_tick { continue; }
        if !dirty && now < last_send + keepalive { continue; }
        let Some(permit) = acquire(&budget, &units) else { return Ok(()); };
        if stop.load(Ordering::Relaxed) { return Ok(()); }
        dirty |= capture.wait(Duration::ZERO)?;
        let state = *locked.borrow();
        if state != last_locked { dirty = true; last_locked = state; }

        if dirty {
            let source = capture.capture()?;
            let bytes_per_line = cap_width as usize * 4;
            let stride = captured.stride(0);
            let row = bytes_per_line.min(stride);
            for (y, src) in source.chunks_exact(bytes_per_line).enumerate() {
                captured.data_mut(0)[y * stride..y * stride + row].copy_from_slice(&src[..row]);
            }
        }

        let slot = ((started.elapsed().as_micros() * u128::from(config.fps) + 500_000) / 1_000_000) as i64;
        let slot = slot.max(last_slot + 1);
        last_slot = slot;

        let frame = if pixel == Pixel::BGRZ && width == captured.width() && height == captured.height() {
            &mut captured
        } else {
            if scaler.is_none() {
                scaler = Some(scaling::Context::get(Pixel::BGRZ, captured.width(), captured.height(), pixel, width, height, scaling::Flags::BILINEAR).map_err(text)?);
            }
            if dirty {
                if let Some(context) = scaler.as_mut() { context.run(&captured, &mut converted).map_err(text)?; }
            }
            &mut converted
        };
        frame.set_pts(Some(slot));
        frame.set_kind(if idr.swap(false, Ordering::AcqRel) { picture::Type::I } else { picture::Type::None });
        send_video_frame(&mut encoder, frame, config.encoder)?;
        permits.push_back(permit);
        if !drain(&mut encoder, &mut packet, &units, &mut permits)? { return Ok(()); }
        let now = Instant::now();
        if dirty {
            // Keep the tick phase; after a long idle, restart the phase from now instead of bursting to catch up.
            let next = next_tick + frame_interval;
            next_tick = if next > now { next } else { now + frame_interval };
            dirty = false;
        }
        last_send = now;
        keepalive = random_keepalive(&mut seed);
    }
}

/// Picks the next keepalive gap in 40–67 ms (15–25 fps) with a small xorshift generator.
fn random_keepalive(seed: &mut u32) -> Duration {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 17;
    *seed ^= *seed << 5;
    Duration::from_micros(40_000 + u64::from(*seed % 26_700))
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
