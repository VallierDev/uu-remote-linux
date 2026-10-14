//! Desktop sound for the viewer's 声音 menu: the default output's monitor (PulseAudio or
//! PipeWire-pulse) is encoded to Opus in this process and sent on the negotiated audio m-line.
//! PipeWire monitors ignore the sink's mute by default, so 被控端静音运行 keeps the stream audible.
//! `UU_AUDIO=0` disables it; `UU_AUDIO_SOURCE` picks another Pulse source.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use webrtc::media::Sample;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

/// One Opus packet per 20 ms frame.
const FRAME: Duration = Duration::from_millis(20);

pub fn enabled() -> bool { std::env::var("UU_AUDIO").map_or(true, |v| v != "0") }

fn closed(pc: &RTCPeerConnection) -> bool {
    matches!(pc.connection_state(), RTCPeerConnectionState::Closed | RTCPeerConnectionState::Failed)
}

/// Streams desktop sound to one viewer until its connection closes; restarts a failed encoder.
pub async fn stream_audio(pc: Arc<RTCPeerConnection>, track: Arc<TrackLocalStaticSample>) {
    while pc.connection_state() != RTCPeerConnectionState::Connected {
        if closed(&pc) { return; }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let source = std::env::var("UU_AUDIO_SOURCE").unwrap_or_else(|_| "@DEFAULT_MONITOR@".into());
    let mut failures = 0u32;
    while !closed(&pc) {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(25);
        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let stop = stop.clone();
            let source = source.clone();
            tokio::task::spawn_blocking(move || crate::inproc_encoder::run_audio(&source, tx, stop))
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
                _ = health.tick() => if closed(&pc) { break },
            }
        }
        // Stopping before dropping the receiver lets the encoder thread exit at its next packet.
        stop.store(true, Ordering::Relaxed);
        drop(rx);
        let result = reader.await.unwrap_or_else(|e| Err(e.to_string()));
        if closed(&pc) { break; }
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
    let stop = Arc::new(AtomicBool::new(false));
    let worker = {
        let stop = stop.clone();
        std::thread::spawn(move || crate::inproc_encoder::run_audio(&source, tx, stop))
    };
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
    stop.store(true, Ordering::Relaxed);
    drop(rx);
    let elapsed = started.elapsed().as_secs_f64();
    println!("Desktop audio probe: packets={packets} bytes={bytes} kbps={:.1} packets_per_s={:.1} max_gap_ms={}",
        bytes as f64 * 8.0 / 1000.0 / elapsed, packets as f64 / elapsed, max_gap.as_millis());
    worker.join().map_err(|_| "audio worker panicked".to_string())?
}
