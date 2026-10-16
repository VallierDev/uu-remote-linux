//! One negotiated video m-line/SSRC, with codec-specific payload types.
//! A single RTP sequence/timestamp survives format changes (never two independent SSRC timelines).
use std::{any::Any, sync::atomic::{AtomicBool, Ordering}};
use async_trait::async_trait;
use tokio::sync::Mutex;
use webrtc::{error::{Error, Result}, media::Sample, rtp::{packet::Packet, header::Header, packetizer::Payloader}};
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType};
use webrtc::track::track_local::{TrackLocal, TrackLocalContext, track_local_static_rtp::TrackLocalStaticRTP};
use crate::capture_setting::VideoCodec;

/// Official SDK GetSupportedFormats constructs H265 with an empty parameter map.
pub fn register_hevc(media: &mut webrtc::api::media_engine::MediaEngine) -> Result<()> {
    media.register_codec(RTCRtpCodecParameters { capability: RTCRtpCodecCapability {
        mime_type: VideoCodec::H265.mime().into(), clock_rate: 90_000,
        rtcp_feedback: vec![webrtc::rtp_transceiver::RTCPFeedback { typ: "nack".into(), parameter: String::new() },
            webrtc::rtp_transceiver::RTCPFeedback { typ: "nack".into(), parameter: "pli".into() }],
        ..Default::default() }, payload_type: 126, ..Default::default() }, RTPCodecType::Video)
}

#[derive(Debug)]
struct Timeline { sequence: u16, timestamp: u32, h264: webrtc::rtp::codecs::h264::H264Payloader }
#[derive(Debug)]
pub struct VideoTrack {
    h264: TrackLocalStaticRTP, h265: TrackLocalStaticRTP,
    has_h264: AtomicBool, has_h265: AtomicBool, timeline: Mutex<Timeline>,
    progress: tokio::sync::watch::Sender<(u64, VideoCodec)>,
}
impl VideoTrack {
    pub fn new() -> Self {
        let track = |codec: VideoCodec| TrackLocalStaticRTP::new(
            RTCRtpCodecCapability { mime_type: codec.mime().into(), clock_rate: 90_000, ..Default::default() },
            "video_0".into(), "video_0".into());
        let (progress, _) = tokio::sync::watch::channel((0, VideoCodec::H264));
        Self { progress, h264: track(VideoCodec::H264), h265: track(VideoCodec::H265), has_h264: AtomicBool::new(false),
            has_h265: AtomicBool::new(false), timeline: Mutex::new(Timeline { sequence: rand::random(), timestamp: rand::random(), h264: Default::default() }) }
    }
    pub fn supports(&self, codec: VideoCodec) -> bool {
        match codec { VideoCodec::H264 => &self.has_h264, VideoCodec::H265 => &self.has_h265 }.load(Ordering::Acquire)
    }
    pub fn revision(&self) -> u64 { self.progress.borrow().0 }
    /// Format-change RPC success follows a transmitted frame, including a negotiated fallback.
    pub async fn wait_for_applied(&self, since: u64, settings: &tokio::sync::watch::Sender<crate::capture_setting::VideoSettings>) -> bool {
        let mut progress = self.progress.subscribe();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let (revision, codec) = *progress.borrow_and_update();
                if revision > since && codec == settings.borrow().codec { return true; }
                if progress.changed().await.is_err() { return false; }
            }
        }).await.unwrap_or(false)
    }
    pub async fn write_sample_with_extensions(&self, codec: VideoCodec, sample: &Sample, extensions: &[webrtc::rtp::extension::HeaderExtension]) -> Result<()> {
        if !self.supports(codec) { return Err(Error::ErrUnsupportedCodec); }
        let track = match codec { VideoCodec::H264 => &self.h264, VideoCodec::H265 => &self.h265 };
        if track.all_binding_paused().await { return Ok(()); }
        let mut state = self.timeline.lock().await;
        // Leave room for the RTP header and negotiated UU extensions below the 1200 byte MTU.
        let payloads = match codec {
            VideoCodec::H264 => state.h264.payload(1160, &sample.data).map_err(Error::Rtp)?,
            VideoCodec::H265 => crate::hevc::payload(1160, &sample.data).map_err(Error::new)?,
        };
        let count = payloads.len();
        let mut complete = count > 0;
        let timestamp = state.timestamp;
        state.timestamp = state.timestamp.wrapping_add((sample.duration.as_secs_f64() * 90_000.0) as u32);
        for (index, payload) in payloads.into_iter().enumerate() {
            let packet = Packet { header: Header { version: 2, sequence_number: state.sequence, timestamp,
                marker: index + 1 == count, ..Default::default() }, payload };
            state.sequence = state.sequence.wrapping_add(1);
            complete &= track.write_rtp_with_extensions(&packet, extensions).await? > 0;
        }
        if complete { self.progress.send_modify(|value| { value.0 += 1; value.1 = codec; }); }
        Ok(())
    }
}
#[async_trait]
impl TrackLocal for VideoTrack {
    async fn bind(&self, context: &TrackLocalContext) -> Result<RTCRtpCodecParameters> {
        let h264 = self.h264.bind(context).await.ok();
        let h265 = self.h265.bind(context).await.ok();
        self.has_h264.store(h264.is_some(), Ordering::Release);
        self.has_h265.store(h265.is_some(), Ordering::Release);
        log::info!("Video track negotiated: h264={} h265={}", h264.is_some(), h265.is_some());
        h264.or(h265).ok_or(Error::ErrUnsupportedCodec)
    }
    async fn unbind(&self, context: &TrackLocalContext) -> Result<()> {
        if self.has_h264.swap(false, Ordering::AcqRel) { self.h264.unbind(context).await?; }
        if self.has_h265.swap(false, Ordering::AcqRel) { self.h265.unbind(context).await?; }
        Ok(())
    }
    fn id(&self) -> &str { "video_0" }
    fn stream_id(&self) -> &str { "video_0" }
    fn kind(&self) -> RTPCodecType { RTPCodecType::Video }
    fn as_any(&self) -> &dyn Any { self }
}

#[cfg(test)]
mod tests {
    use super::*;
    use webrtc::{api::{APIBuilder, media_engine::MediaEngine}, peer_connection::configuration::RTCConfiguration};
    async fn negotiate(h264: bool, hevc: bool) {
        let mut remote_media = MediaEngine::default();
        if h264 { remote_media.register_codec(RTCRtpCodecParameters { capability: RTCRtpCodecCapability {
            mime_type: VideoCodec::H264.mime().into(), clock_rate: 90_000,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".into(), ..Default::default()
        }, payload_type: 102, ..Default::default() }, RTPCodecType::Video).unwrap(); }
        if hevc { register_hevc(&mut remote_media).unwrap(); }
        let mut setting = webrtc::api::setting_engine::SettingEngine::default();
        setting.set_ice_multicast_dns_mode(webrtc::ice::mdns::MulticastDnsMode::Disabled);
        let remote = APIBuilder::new().with_setting_engine(setting.clone()).with_media_engine(remote_media).build().new_peer_connection(RTCConfiguration::default()).await.unwrap();
        remote.add_transceiver_from_kind(RTPCodecType::Video, None).await.unwrap();
        let mut media = MediaEngine::default(); media.register_default_codecs().unwrap(); register_hevc(&mut media).unwrap();
        let host = APIBuilder::new().with_setting_engine(setting).with_media_engine(media).build().new_peer_connection(RTCConfiguration::default()).await.unwrap();
        let (packets_tx, mut packets_rx) = tokio::sync::mpsc::unbounded_channel();
        remote.on_track(Box::new(move |remote_track, _, _| {
            let packets_tx = packets_tx.clone();
            Box::pin(async move {
                tokio::spawn(async move {
                    while let Ok((packet, _)) = remote_track.read_rtp().await {
                        if packets_tx.send(packet).is_err() { break; }
                    }
                });
            })
        }));
        let track = std::sync::Arc::new(VideoTrack::new());
        host.add_track(track.clone()).await.unwrap();
        let offer = remote.create_offer(None).await.unwrap();
        let mut gathered = remote.gathering_complete_promise().await;
        remote.set_local_description(offer).await.unwrap();
        gathered.recv().await;
        host.set_remote_description(remote.local_description().await.unwrap()).await.unwrap();
        let answer = host.create_answer(None).await.unwrap();
        assert_eq!(answer.sdp.contains("H265/90000"), hevc);
        assert_eq!(answer.sdp.contains("H264/90000"), h264);
        let mut gathered = host.gathering_complete_promise().await;
        host.set_local_description(answer).await.unwrap();
        gathered.recv().await;
        remote.set_remote_description(host.local_description().await.unwrap()).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !track.supports(VideoCodec::H264) && !track.supports(VideoCodec::H265) { tokio::time::sleep(std::time::Duration::from_millis(10)).await; }
        }).await.unwrap();
        assert_eq!(track.supports(VideoCodec::H264), h264);
        assert_eq!(track.supports(VideoCodec::H265), hevc);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while host.connection_state() != webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Connected {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        let codecs = if !h264 { vec![VideoCodec::H265] } else if hevc { vec![VideoCodec::H264, VideoCodec::H265, VideoCodec::H264] } else { vec![VideoCodec::H264] };
        let mut previous: Option<Packet> = None;
        for codec in codecs {
            let data = match codec { VideoCodec::H264 => bytes::Bytes::from_static(&[0,0,1,65,1,2]),
                VideoCodec::H265 => bytes::Bytes::from_static(&[0,0,1,38,1,2]) };
            let (settings, _) = tokio::sync::watch::channel(crate::capture_setting::VideoSettings { codec, ..Default::default() });
            let applied = track.wait_for_applied(track.revision(), &settings);
            tokio::pin!(applied);
            assert!(tokio::time::timeout(std::time::Duration::from_millis(1), &mut applied).await.is_err());
            track.write_sample_with_extensions(codec, &Sample { data, duration: std::time::Duration::from_millis(20), ..Default::default() }, &[]).await.unwrap();
            assert!(applied.await);
            let packet = tokio::time::timeout(std::time::Duration::from_secs(3), packets_rx.recv()).await.unwrap().unwrap();
            assert_eq!(packet.header.payload_type, if codec == VideoCodec::H264 { 102 } else { 126 });
            assert!(packet.header.marker);
            if let Some(previous) = previous {
                assert_eq!(packet.header.ssrc, previous.header.ssrc);
                assert_eq!(packet.header.sequence_number, previous.header.sequence_number.wrapping_add(1));
                assert_eq!(packet.header.timestamp, previous.header.timestamp.wrapping_add(1800));
            }
            previous = Some(packet);
        }
        host.close().await.unwrap(); remote.close().await.unwrap();
    }
    #[tokio::test]
    async fn negotiated_codecs_only_and_legacy_h264_offer() { negotiate(true, false).await; negotiate(true, true).await; negotiate(false, true).await; }
}
