//! Black video keeps locked Portal initialization reachable by the iPhone viewer.
use std::time::Duration;
use bytes::Bytes;
use webrtc::{media::Sample, peer_connection::{RTCPeerConnection, peer_connection_state::RTCPeerConnectionState}};
use crate::{capture_setting::VideoCodec, desktop_session::PortalDesktop, video_track::VideoTrack};

async fn black_frame(codec: VideoCodec) -> Result<Bytes, String> {
    let mut command = crate::video::ffmpeg_command();
    command.args(["-hide_banner", "-loglevel", "error", "-nostdin",
        "-f", "lavfi", "-i", "color=c=black:s=1280x720:r=1", "-frames:v", "1",
        "-an", "-pix_fmt", "yuv420p", "-threads", "1", "-preset", "ultrafast", "-tune", "zerolatency"]);
    match codec {
        VideoCodec::H264 => { command.args(["-c:v", "libx264", "-profile:v", "baseline",
            "-x264-params", "keyint=1:repeat-headers=1:aud=1", "-f", "h264"]); },
        VideoCodec::H265 => { command.args(["-c:v", "libx265",
            "-x265-params", "keyint=1:repeat-headers=1:aud=1:pools=1:frame-threads=1:log-level=error", "-f", "hevc"]); },
    }
    command.arg("pipe:1").kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(5), command.output()).await
        .map_err(|_| "Locked desktop video encoder timed out".to_string())?
        .map_err(|error| format!("Locked desktop video encoder: {error}"))?;
    if !output.status.success() || output.stdout.is_empty() {
        return Err(format!("Locked desktop video encoder failed: status={}", output.status));
    }
    Ok(output.stdout.into())
}

pub async fn run(pc: &RTCPeerConnection, track: &VideoTrack, desktop: &PortalDesktop, selection: crate::video_select::Selection) -> Result<(), String> {
    run_with_encoder(pc, track, desktop, selection, black_frame).await
}

async fn run_with_encoder<F, Fut>(pc: &RTCPeerConnection, track: &VideoTrack, desktop: &PortalDesktop, selection: crate::video_select::Selection, mut encode: F) -> Result<(), String>
where F: FnMut(VideoCodec) -> Fut, Fut: std::future::Future<Output = Result<Bytes, String>> {
    // #29: iPhone 4.42.0 cold ON/OFF/ON tests only sent action15 after black video.
    // Keep CONTROL permissions truthful and use the existing video track until real capture is ready.
    let mut updates = desktop.subscribe();
    let mut locked = desktop.lock_state();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut frame = None;
    let mut sent = 0u64;
    let mut attempts = 0u64;
    loop {
        let state = updates.borrow_and_update().clone();
        let _ = locked.borrow_and_update();
        if state.ready() || !state.waiting || crate::video::transport_closed(pc) {
            if attempts > 0 { log::info!("Locked desktop video ended: sent={sent} attempts={attempts}"); }
            return Ok(());
        }
        tokio::select! {
            biased;
            changed = updates.changed() => if changed.is_err() { return Ok(()); },
            changed = locked.changed() => if changed.is_err() { return Ok(()); },
            _ = tick.tick() => {
                if !updates.borrow().waiting || *locked.borrow() != Some(true)
                    || pc.connection_state() != RTCPeerConnectionState::Connected { continue; }
                if frame.is_none() {
                    let codec = crate::capture_setting::preferred_codec_for(
                        track.supports(VideoCodec::H264), track.supports(VideoCodec::H265), selection);
                    let encoded = tokio::select! {
                        biased;
                        _ = updates.changed() => continue,
                        _ = locked.changed() => continue,
                        result = encode(codec) => result?,
                    };
                    frame = Some((codec, encoded));
                }
                if crate::video::transport_closed(pc) { return Ok(()); }
                // Encoding can finish in the same poll that publishes unlock/readiness.
                if !updates.borrow().waiting || *locked.borrow() != Some(true)
                    || pc.connection_state() != RTCPeerConnectionState::Connected { continue; }
                let (codec, data) = frame.as_ref().unwrap();
                let sample = Sample { data: data.clone(), duration: Duration::from_secs(1), ..Default::default() };
                let extensions = [crate::rtp_metadata::capture_index(0), crate::rtp_metadata::new_frame()];
                let revision = track.revision();
                // No monitor inventory or permission claims: 1280x720 is only this synthetic bitstream.
                tokio::select! {
                    biased;
                    _ = updates.changed() => continue,
                    _ = locked.changed() => continue,
                    result = tokio::time::timeout(Duration::from_secs(2),
                        track.write_sample_with_extensions(*codec, &sample, &extensions)) => {
                        if crate::video::transport_closed(pc) { return Ok(()); }
                        result.map_err(|_| "Locked desktop video send timed out".to_string())?
                            .map_err(|error| format!("Locked desktop video send: {error}"))?;
                    },
                }
                attempts += 1;
                if track.revision() > revision { sent += 1; }
                tick.reset();
                if attempts == 1 || attempts % 10 == 0 {
                    log::info!("Locked desktop video: sent={sent} codec={codec:?} synthetic_size=1280x720 bytes={} attempts={attempts}", data.len());
                }
            },
        }
    }
}

#[cfg(test)]
#[path = "locked_video_tests.rs"]
mod tests;
