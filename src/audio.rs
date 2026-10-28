//! Desktop sound for the viewer's 声音 menu: the default output's monitor (PulseAudio or
//! PipeWire-pulse) is encoded to Opus and sent on the negotiated audio m-line.
//! An FFmpeg child process does the encoding.
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
    let mut child = spawn_encoder(source)?;
    let stdout = child.stdout.take().ok_or("Encoder stdout missing")?;
    let job = tokio::task::spawn_blocking(move || read_packets(stdout, &tx));
    Ok((Stopper { stop, child: Some(child) }, job))
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
