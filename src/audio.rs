//! Desktop sound for the viewer's 声音 menu: the default output's monitor (PulseAudio or
//! PipeWire-pulse) is encoded to Opus and sent on the negotiated audio m-line.
//! inproc-av uses libpulse/libopus directly; other builds and native failures use FFmpeg.
//! PipeWire monitors ignore the sink's mute by default, so 被控端静音运行 keeps the stream audible.
//! `UU_AUDIO=0` disables it; `UU_AUDIO_SOURCE` picks another Pulse source.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use webrtc::media::io::ogg_reader::OggReader;
use webrtc::media::Sample;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

/// One Opus packet per 20 ms frame.
const FRAME: Duration = Duration::from_millis(20);

pub fn enabled() -> bool { std::env::var("UU_AUDIO").map_or(true, |v| v != "0") }

/// Stops one running encoder when dropped: sets the in-process stop flag, or kills the FFmpeg child.
struct Stopper { stop: Arc<AtomicBool>, child: Option<Child> }
impl Drop for Stopper {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(child) = self.child.as_mut() { let _ = child.kill(); let _ = child.wait(); }
    }
}

fn spawn_encoder(source: &str) -> Result<Child, String> {
    let mut command = match crate::config::ffmpeg_root() {
        Some(root) => { let mut c = Command::new(root.join("bin/ffmpeg")); c.env("LD_LIBRARY_PATH", root.join("lib/x86_64-linux-gnu")); c }
        None => Command::new("ffmpeg"),
    };
    command.args(["-hide_banner", "-loglevel", "error", "-nostdin",
            "-f", "pulse", "-fragment_size", "1920", "-i", source, "-vn",
            "-c:a", "libopus", "-ar", "48000", "-ac", "2", "-b:a", "128k", "-application", "lowdelay",
            "-frame_duration", "20", "-page_duration", "20000", "-flush_packets", "1", "-f", "ogg", "pipe:1"])
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::inherit());
    command.spawn().map_err(|e| format!("Start audio encoder: {e}"))
}

/// Reads Opus packets from the FFmpeg child's Ogg output until it ends or `tx` closes. Blocking.
fn read_packets(stdout: impl Read, tx: &tokio::sync::mpsc::Sender<bytes::Bytes>) -> Result<(), String> {
    let (mut reader, _) = OggReader::new(stdout, false).map_err(|e| format!("Audio Ogg header: {e}"))?;
    loop {
        let (payload, _) = reader.parse_next_page().map_err(|e| format!("Audio Ogg page: {e}"))?;
        if payload.starts_with(b"OpusTags") { continue; }
        if tx.blocking_send(payload.freeze()).is_err() { return Ok(()); }
    }
}

/// Starts one encoder feeding `tx`. The job ends with its result once the stopper is dropped or `tx` closes.
fn start(source: &str, tx: tokio::sync::mpsc::Sender<bytes::Bytes>)
    -> Result<(Stopper, tokio::task::JoinHandle<Result<(), String>>), String> {
    let stop = Arc::new(AtomicBool::new(false));
    #[cfg(all(target_os = "linux", feature = "inproc-av"))]
    if !crate::inproc_encoder::disabled() {
        let source = std::ffi::CString::new(source).map_err(|_| "Audio source contains NUL")?;
        let worker_stop = stop.clone();
        let job = tokio::task::spawn_blocking(move || {
            match native_packets(&source, &tx, &worker_stop) {
                Ok(()) => Ok(()),
                Err(error) if !worker_stop.load(Ordering::Relaxed) && !tx.is_closed() => {
                    log::warn!("In-process audio unavailable: {error}; falling back to FFmpeg Opus");
                    let mut child = spawn_encoder(source.to_str().map_err(|_| "Invalid audio source")?)?;
                    let stdout = child.stdout.take().ok_or("Encoder stdout missing")?;
                    // Supervise the process separately so cancellation also interrupts a blocked read.
                    let (done_tx, done_rx) = std::sync::mpsc::channel();
                    let reader_tx = tx.clone();
                    let reader = std::thread::spawn(move || {
                        let result = read_packets(stdout, &reader_tx);
                        let _ = done_tx.send(result);
                    });
                    let result = loop {
                        match done_rx.recv_timeout(Duration::from_millis(20)) {
                            Ok(result) => break result,
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break Err("Audio reader ended".into()),
                            Err(_) => if worker_stop.load(Ordering::Relaxed) || tx.is_closed() { break Ok(()); },
                        }
                    };
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    result
                },
                Err(_) => Ok(()),
            }
        });
        return Ok((Stopper { stop, child: None }, job));
    }
    let mut child = spawn_encoder(source)?;
    let stdout = child.stdout.take().ok_or("Encoder stdout missing")?;
    let job = tokio::task::spawn_blocking(move || read_packets(stdout, &tx));
    Ok((Stopper { stop, child: Some(child) }, job))
}

#[cfg(all(target_os = "linux", feature = "inproc-av"))]
fn native_packets(source: &std::ffi::CStr, tx: &tokio::sync::mpsc::Sender<bytes::Bytes>, stop: &AtomicBool) -> Result<(), String> {
    extern "C" {
        fn uu_audio_open(source: *const libc::c_char) -> *mut libc::c_void;
        fn uu_audio_packet(audio: *mut libc::c_void, packet: *mut u8, capacity: libc::c_int) -> libc::c_int;
        fn uu_audio_close(audio: *mut libc::c_void);
    }
    struct Capture(*mut libc::c_void);
    impl Drop for Capture {
        fn drop(&mut self) { unsafe { uu_audio_close(self.0) }; }
    }
    let capture = Capture(unsafe { uu_audio_open(source.as_ptr()) });
    if capture.0.is_null() { return Err("Native audio initialization failed".into()); }
    let mut packet = [0; 4000];
    let mut last_packet = std::time::Instant::now();
    let mut announced = false;
    while !stop.load(Ordering::Relaxed) && !tx.is_closed() {
        let size = unsafe { uu_audio_packet(capture.0, packet.as_mut_ptr(), packet.len() as libc::c_int) };
        if size < 0 { return Err(format!("Native audio capture/encode failed: {size}")); }
        if size == 0 {
            if last_packet.elapsed() >= Duration::from_secs(5) { return Err("Native audio capture timed out".into()); }
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        if !announced { log::info!("Desktop audio pipeline=in-process opus 48k stereo"); announced = true; }
        last_packet = std::time::Instant::now();
        if tx.blocking_send(bytes::Bytes::copy_from_slice(&packet[..size as usize])).is_err() { break; }
    }
    Ok(())
}

fn closed(pc: &RTCPeerConnection) -> bool {
    matches!(pc.connection_state(), RTCPeerConnectionState::Closed | RTCPeerConnectionState::Failed)
}

/// Streams desktop sound to one viewer until its connection closes; restarts a failed encoder.
pub async fn stream_audio(pc: Arc<RTCPeerConnection>, track: Arc<TrackLocalStaticSample>) {
    while pc.connection_state() != RTCPeerConnectionState::Connected {
        if closed(&pc) { return; }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut failures = 0u32;
    while !closed(&pc) {
        let source = crate::virtual_audio::source();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(25);
        let (stopper, job) = match start(&source, tx) {
            Ok(started) => started,
            Err(e) => { log::warn!("{e}"); return; }
        };
        log::info!("Desktop audio started: opus 48k stereo");
        let mut packets = 0u64;
        let mut health = tokio::time::interval(Duration::from_millis(500));
        loop {
            tokio::select! {
                packet = rx.recv() => {
                    let Some(data) = packet else { break };
                    if let Err(e) = track.write_sample(&Sample { data, duration: FRAME, ..Default::default() }).await {
                        log::warn!("Audio send: {e}"); break;
                    }
                    packets += 1;
                },
                _ = health.tick() => if closed(&pc) || crate::virtual_audio::source() != source { break },
            }
        }
        // Dropping the receiver and the stopper ends the encoder at its next packet.
        drop(rx);
        drop(stopper);
        let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
        if closed(&pc) { break; }
        if crate::virtual_audio::source() != source { failures=0; continue; }
        log::warn!("Desktop audio stopped after {packets} packets: {:?}", result.err());
        failures = if packets > 250 { 0 } else { failures + 1 };
        if failures >= 5 { log::warn!("Desktop audio disabled for this connection"); return; }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    log::info!("Desktop audio ended");
}

/// Encodes the configured source for `seconds` and prints packet count, bitrate and frame spacing.
pub async fn probe(seconds: u64) -> Result<(), String> {
    let source = std::env::var("UU_AUDIO_SOURCE").unwrap_or_else(|_| "@DEFAULT_MONITOR@".into());
    let (tx, mut rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(256);
    let (stopper, job) = start(&source, tx)?;
    let started = std::time::Instant::now();
    let mut packets = 0u64;
    let mut bytes = 0u64;
    let mut max_gap = Duration::ZERO;
    let mut last = None::<std::time::Instant>;
    while started.elapsed() < Duration::from_secs(seconds) {
        match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Some(data)) => {
                let now = std::time::Instant::now();
                if let Some(prev) = last { max_gap = max_gap.max(now - prev); }
                last = Some(now);
                packets += 1;
                bytes += data.len() as u64;
            }
            _ => break,
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    drop(rx);
    drop(stopper);
    println!("Desktop audio probe: packets={packets} bytes={bytes} kbps={:.1} packets_per_s={:.1} max_gap_ms={}",
        bytes as f64 * 8.0 / 1000.0 / elapsed, packets as f64 / elapsed, max_gap.as_millis());
    // The encoder is stopped on purpose, so its end-of-stream error only matters if nothing arrived.
    let result = job.await.map_err(|e| e.to_string())?;
    if packets == 0 { result } else { Ok(()) }
}

#[cfg(all(test, target_os = "linux", feature = "inproc-av"))]
mod native_tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires a running PulseAudio or PipeWire-pulse server"]
    async fn native_opus_packets_decode_and_capture_stops_promptly() {
        extern "C" {
            fn opus_decoder_create(rate: i32, channels: i32, error: *mut i32) -> *mut libc::c_void;
            fn opus_decode(decoder: *mut libc::c_void, data: *const u8, length: i32, pcm: *mut i16, size: i32, fec: i32) -> i32;
            fn opus_decoder_destroy(decoder: *mut libc::c_void);
        }
        let source = std::ffi::CString::new("@DEFAULT_MONITOR@").unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let job = tokio::task::spawn_blocking(move || native_packets(&source, &tx, &worker_stop));
        let stopper = Stopper { stop, child: None };
        let packet = tokio::time::timeout(Duration::from_secs(6), rx.recv()).await.unwrap().unwrap();
        let mut error = 0;
        let decoder = unsafe { opus_decoder_create(48000, 2, &mut error) };
        assert_eq!(error, 0);
        assert!(!decoder.is_null());
        let mut pcm = [0; 960 * 2];
        let samples = unsafe { opus_decode(decoder, packet.as_ptr(), packet.len() as i32, pcm.as_mut_ptr(), 960, 0) };
        unsafe { opus_decoder_destroy(decoder) };
        assert_eq!(samples, 960);
        drop(rx);
        drop(stopper);
        tokio::time::timeout(Duration::from_secs(1), job).await.unwrap().unwrap().unwrap();
    }
}
