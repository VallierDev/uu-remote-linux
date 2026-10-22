use super::{black_frame, run, run_with_encoder};
use crate::{
    capture::ScreenCapture,
    capture_setting::VideoCodec,
    desktop_backend::DesktopBackend,
    desktop_session::PortalDesktop,
    video_track::VideoTrack,
};
use std::{process::Stdio, sync::Arc, time::Duration};
use tokio::{io::AsyncWriteExt, sync::{mpsc, watch}};
use webrtc::{
    api::{media_engine::MediaEngine, APIBuilder},
    media::Sample,
    peer_connection::{
        configuration::RTCConfiguration,
        peer_connection_state::RTCPeerConnectionState,
        RTCPeerConnection,
    },
    rtp::packet::Packet,
    rtp_transceiver::{
        rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType},
    },
    util::vnet::router::Router,
};

async fn decode_gray(data: &[u8], demuxer: &str) -> (Vec<u8>, String) {
    let mut command = crate::video::ffmpeg_command();
    command.args([
        "-hide_banner", "-loglevel", "info", "-nostdin", "-f", demuxer, "-i", "pipe:0",
        "-frames:v", "1", "-vf", "scale=in_range=tv:out_range=pc,format=gray,showinfo",
        "-pix_fmt", "gray", "-f", "rawvideo", "pipe:1",
    ]);
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = command.spawn().expect("start FFmpeg decoder");
    let mut stdin = child.stdin.take().expect("FFmpeg decoder stdin");
    let output = tokio::time::timeout(Duration::from_secs(10), async move {
        stdin.write_all(data).await.expect("write encoded frame to FFmpeg");
        drop(stdin);
        child.wait_with_output().await.expect("wait for FFmpeg decoder")
    }).await.expect("FFmpeg decoder timed out");
    assert!(output.status.success(), "FFmpeg decoder failed: status={}", output.status);
    (output.stdout, String::from_utf8_lossy(&output.stderr).into_owned())
}

async fn black_frame_decodes_as_1280x720_black(codec: VideoCodec, demuxer: &str) {
    let encoded = black_frame(codec).await.expect("encode black frame");
    assert!(!encoded.is_empty());
    let (gray, diagnostics) = decode_gray(&encoded, demuxer).await;
    assert!(diagnostics.contains("s:1280x720"), "decoded dimensions were not 1280x720");
    assert_eq!(gray.len(), 1280 * 720, "expected one 1280x720 gray frame");
    assert!(gray.iter().all(|&pixel| pixel <= 1), "decoded frame was not black");
}

#[tokio::test]
#[ignore = "requires FFmpeg libx264/libx265"]
async fn h264_black_frame_decodes_to_1280x720_black() {
    black_frame_decodes_as_1280x720_black(VideoCodec::H264, "h264").await;
}

#[tokio::test]
#[ignore = "requires FFmpeg libx264/libx265"]
async fn h265_black_frame_decodes_to_1280x720_black() {
    black_frame_decodes_as_1280x720_black(VideoCodec::H265, "hevc").await;
}

fn ready_screen() -> Arc<ScreenCapture> {
    let mut screen = ScreenCapture::unavailable("synthetic ready desktop".into());
    screen.backend = DesktopBackend::Portal;
    screen.init_error = None;
    screen.display = "wayland:test".into();
    screen.width = 1280;
    screen.height = 720;
    Arc::new(screen)
}

async fn receive_frame(packets: &mut mpsc::UnboundedReceiver<Packet>, within: Duration) -> Vec<Packet> {
    tokio::time::timeout(within, async {
        let mut frame = Vec::new();
        loop {
            let packet = packets.recv().await.expect("RTP packet receiver closed");
            let marker = packet.header.marker;
            frame.push(packet);
            if marker {
                return frame;
            }
        }
    }).await.expect("timed out waiting for an RTP frame")
}

async fn assert_no_rtp(packets: &mut mpsc::UnboundedReceiver<Packet>, within: Duration) {
    assert!(tokio::time::timeout(within, packets.recv()).await.is_err(),
        "locked video task sent RTP while transmission was disabled");
}

fn assert_single_rtp_frame(frame: &[Packet]) {
    let first = frame.first().expect("RTP frame has packets");
    let last = frame.last().unwrap();
    assert!(last.header.marker, "RTP frame ends with a marker packet");
    for pair in frame.windows(2) {
        assert_eq!(pair[0].header.ssrc, pair[1].header.ssrc);
        assert_eq!(pair[0].header.timestamp, pair[1].header.timestamp);
        assert_eq!(pair[1].header.sequence_number, pair[0].header.sequence_number.wrapping_add(1));
    }
    assert!(frame.iter().all(|packet| packet.header.ssrc == first.header.ssrc));
}

struct VirtualPeers {
    host: Arc<RTCPeerConnection>,
    remote: Arc<RTCPeerConnection>,
    _control: Arc<webrtc::data_channel::RTCDataChannel>,
    router: Arc<tokio::sync::Mutex<Router>>,
    track: Arc<VideoTrack>,
    packets: mpsc::UnboundedReceiver<Packet>,
}

impl VirtualPeers {
    async fn new(codec: VideoCodec) -> Self {
        let (remote_setting, host_setting, router) = crate::test_network::virtual_network().await;

        let mut remote_media = MediaEngine::default();
        match codec {
            VideoCodec::H264 => remote_media.register_codec(RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: VideoCodec::H264.mime().into(),
                    clock_rate: 90_000,
                    sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".into(),
                    ..Default::default()
                },
                payload_type: 102,
                ..Default::default()
            }, RTPCodecType::Video).unwrap(),
            VideoCodec::H265 => crate::video_track::register_hevc(&mut remote_media).unwrap(),
        }
        let remote = Arc::new(APIBuilder::new().with_setting_engine(remote_setting)
            .with_media_engine(remote_media).build()
            .new_peer_connection(RTCConfiguration::default()).await.unwrap());
        remote.add_transceiver_from_kind(RTPCodecType::Video, None).await.unwrap();
        let control = remote.create_data_channel("CONTROL_DATA_CHANNEL", None).await.unwrap();

        let mut host_media = MediaEngine::default();
        host_media.register_default_codecs().unwrap();
        crate::video_track::register_hevc(&mut host_media).unwrap();
        let host = Arc::new(APIBuilder::new().with_setting_engine(host_setting)
            .with_media_engine(host_media).build()
            .new_peer_connection(RTCConfiguration::default()).await.unwrap());
        let track = Arc::new(VideoTrack::new());
        host.add_track(track.clone()).await.unwrap();

        let (packet_tx, packets) = mpsc::unbounded_channel();
        remote.on_track(Box::new(move |remote_track, _, _| {
            let packet_tx = packet_tx.clone();
            Box::pin(async move {
                tokio::spawn(async move {
                    while let Ok((packet, _)) = remote_track.read_rtp().await {
                        if packet_tx.send(packet).is_err() { break; }
                    }
                });
            })
        }));

        Self { host, remote, _control: control, router, track, packets }
    }

    async fn connect(&self, codec: VideoCodec) {
        let offer = self.remote.create_offer(None).await.unwrap();
        let mut gathered = self.remote.gathering_complete_promise().await;
        self.remote.set_local_description(offer).await.unwrap();
        gathered.recv().await;
        self.host.set_remote_description(self.remote.local_description().await.unwrap()).await.unwrap();
        let answer = self.host.create_answer(None).await.unwrap();
        let mut gathered = self.host.gathering_complete_promise().await;
        self.host.set_local_description(answer).await.unwrap();
        gathered.recv().await;
        self.remote.set_remote_description(self.host.local_description().await.unwrap()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            while self.host.connection_state() != RTCPeerConnectionState::Connected
                || !self.track.supports(codec)
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("virtual WebRTC peers did not connect with the requested codec");
        assert!(!self.track.supports(match codec {
            VideoCodec::H264 => VideoCodec::H265,
            VideoCodec::H265 => VideoCodec::H264,
        }));
    }

    async fn close(self) {
        self.host.close().await.unwrap();
        self.remote.close().await.unwrap();
        self.router.lock().await.stop().await.unwrap();
    }
}

async fn exercise_locked_video(codec: VideoCodec) {
    tokio::time::timeout(Duration::from_secs(35), async {
        let mut peers = VirtualPeers::new(codec).await;
        let (lock_tx, lock_rx) = watch::channel(Some(true));
        let desktop = Arc::new(PortalDesktop::with_lock_state(
            Arc::new(ScreenCapture::unavailable("synthetic pending desktop".into())),
            lock_rx,
        ));
        assert!(desktop.snapshot().waiting);
        assert!(!desktop.snapshot().ready());

        let run_host = peers.host.clone();
        let run_track = peers.track.clone();
        let run_desktop = desktop.clone();
        let task = tokio::spawn(async move { run(&run_host, &run_track, &run_desktop, crate::video_select::selection()).await });

        assert_ne!(peers.host.connection_state(), RTCPeerConnectionState::Connected);
        assert_no_rtp(&mut peers.packets, Duration::from_millis(150)).await;

        peers.connect(codec).await;

        let locked_frame = receive_frame(&mut peers.packets, Duration::from_secs(5)).await;
        assert_single_rtp_frame(&locked_frame);
        let last_locked_packet = locked_frame.last().unwrap().clone();

        lock_tx.send_replace(None);
        assert_no_rtp(&mut peers.packets, Duration::from_millis(1200)).await;
        lock_tx.send_replace(Some(false));
        assert_no_rtp(&mut peers.packets, Duration::from_millis(1200)).await;

        desktop.publish_test_screen(ready_screen());
        tokio::time::timeout(Duration::from_secs(2), task).await
            .expect("locked video task did not stop when the desktop became ready")
            .expect("locked video task panicked")
            .expect("locked video task returned an error");

        let live_data = black_frame(codec).await.expect("encode live test sample");
        peers.track.write_sample_with_extensions(codec, &Sample {
            data: live_data,
            duration: Duration::from_millis(20),
            ..Default::default()
        }, &[]).await.unwrap();
        let live_frame = receive_frame(&mut peers.packets, Duration::from_secs(5)).await;
        assert_single_rtp_frame(&live_frame);
        let first_live_packet = &live_frame[0];
        assert_eq!(first_live_packet.header.ssrc, last_locked_packet.header.ssrc);
        assert_eq!(first_live_packet.header.sequence_number,
            last_locked_packet.header.sequence_number.wrapping_add(1));
        assert_eq!(first_live_packet.header.timestamp,
            last_locked_packet.header.timestamp.wrapping_add(90_000));

        peers.close().await;
    }).await.expect("locked video WebRTC test exceeded its deadline");
}

#[tokio::test]
#[ignore = "requires FFmpeg libx264/libx265"]
async fn locked_video_gates_on_pending_locked_connected_and_preserves_video_timeline() {
    exercise_locked_video(VideoCodec::H264).await;
    exercise_locked_video(VideoCodec::H265).await;
}

#[tokio::test]
async fn locked_video_rechecks_state_when_encoding_finishes() {
    #[derive(Clone, Copy)]
    enum Transition { Unlocked, Unknown, Ready }
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut peers = VirtualPeers::new(VideoCodec::H264).await;
        peers.connect(VideoCodec::H264).await;
        for transition in [Transition::Unlocked, Transition::Unknown, Transition::Ready] {
            let (lock_tx, lock_rx) = watch::channel(Some(true));
            let desktop = Arc::new(PortalDesktop::with_lock_state(
                Arc::new(ScreenCapture::unavailable("synthetic pending desktop".into())),
                lock_rx,
            ));
            let run_host = peers.host.clone();
            let run_track = peers.track.clone();
            let run_desktop = desktop.clone();
            let (encoded_tx, mut encoded_rx) = mpsc::unbounded_channel();
            let revision = peers.track.revision();
            let task = tokio::spawn(async move {
                run_with_encoder(&run_host, &run_track, &run_desktop, crate::video_select::selection(), |_| {
                    let desktop = run_desktop.clone();
                    let lock_tx = lock_tx.clone();
                    let encoded_tx = encoded_tx.clone();
                    async move {
                        // Change state during the encoder's final poll, before it returns Ready.
                        match transition {
                            Transition::Unlocked => { lock_tx.send_replace(Some(false)); },
                            Transition::Unknown => { lock_tx.send_replace(None); },
                            Transition::Ready => desktop.publish_test_screen(ready_screen()),
                        }
                        encoded_tx.send(()).unwrap();
                        Ok(bytes::Bytes::from_static(&[0, 0, 1, 65, 1, 2]))
                    }
                }).await
            });
            tokio::time::timeout(Duration::from_secs(2), encoded_rx.recv()).await
                .expect("test encoder was not polled").expect("test encoder channel closed");
            assert_no_rtp(&mut peers.packets, Duration::from_millis(250)).await;
            assert_eq!(peers.track.revision(), revision, "stale state allowed a synthetic frame");
            desktop.publish_test_screen(ready_screen());
            tokio::time::timeout(Duration::from_secs(2), task).await
                .expect("locked video task did not stop after readiness")
                .expect("locked video task panicked")
                .expect("locked video task returned an error");
        }
        peers.close().await;
    }).await.expect("locked video concurrent-transition test exceeded its deadline");
}

#[tokio::test]
async fn locked_video_stops_on_capture_rejection_and_terminal_peer_close() {
    tokio::time::timeout(Duration::from_secs(25), async {
        let mut peers = VirtualPeers::new(VideoCodec::H264).await;
        peers.connect(VideoCodec::H264).await;
        let (_lock_tx, lock_rx) = watch::channel(Some(false));

        let rejected_desktop = Arc::new(PortalDesktop::with_lock_state(
            Arc::new(ScreenCapture::unavailable("synthetic pending desktop".into())),
            lock_rx.clone(),
        ));
        let run_host = peers.host.clone();
        let run_track = peers.track.clone();
        let run_desktop = rejected_desktop.clone();
        let rejection_task = tokio::spawn(async move { run(&run_host, &run_track, &run_desktop, crate::video_select::selection()).await });
        rejected_desktop.publish_test_screen(
            Arc::new(ScreenCapture::unavailable("synthetic capture rejection".into())),
        );
        assert_eq!(rejected_desktop.snapshot().permission(true),
            crate::system_metrics::Permission::Reject);
        tokio::time::timeout(Duration::from_secs(2), rejection_task).await
            .expect("locked video task did not stop after capture rejection")
            .expect("locked video task panicked")
            .expect("locked video task returned an error");

        let pending_desktop = Arc::new(PortalDesktop::with_lock_state(
            Arc::new(ScreenCapture::unavailable("synthetic pending desktop".into())),
            lock_rx,
        ));
        let run_host = peers.host.clone();
        let run_track = peers.track.clone();
        let run_desktop = pending_desktop;
        let close_task = tokio::spawn(async move { run(&run_host, &run_track, &run_desktop, crate::video_select::selection()).await });
        peers.host.close().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), close_task).await
            .expect("locked video task did not stop after terminal peer close")
            .expect("locked video task panicked")
            .expect("locked video task returned an error");

        peers.packets.close();
        assert!(peers.packets.try_recv().is_err(), "locked video task sent RTP while unlocked");
        peers.remote.close().await.unwrap();
        peers.router.lock().await.stop().await.unwrap();
    }).await.expect("locked video lifecycle test exceeded its deadline");
}
