//! Native Rust WebRTC Engine (PeerConnection, DataChannels, Input & Video)

use crate::capture::ScreenCapture;
use crate::input::InputInjector;
use crate::signaling::SoacMessage;

use base64::Engine;
use flate2::read::GzDecoder;
use std::io::Read;
use std::sync::Arc;
use tokio::sync::Mutex;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::media::Sample;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::dtls_transport::dtls_role::DTLSRole;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocalWriter;

fn is_capture_setting_message(data: &[u8]) -> bool {
    use crate::protocol::WireValue;
    let Ok(fields) = crate::protocol::decode_wire(data) else { return false; };
    fields.iter().find_map(|field| match (field.number, &field.value) {
        (21, WireValue::Bytes(rpc)) => crate::protocol::decode_wire(rpc).ok(),
        _ => None,
    }).is_some_and(|rpc| rpc.iter().any(|field| matches!(field.number, 2 | 3 | 4 | 6)))
}

#[derive(Debug, Default, Clone)]
pub struct SessionContext {
    pub client_id: Option<String>,
    pub ice_id: Option<serde_json::Value>,
    pub app_control_id: Option<serde_json::Value>,
    pub bundle_media: Option<(String, u16)>,
}

/// Desktop role of one peer. A tool peer (file transfer, terminal, port mapping) gains it
/// when the viewer opens the desktop on that same connection with SimpleAction 4
/// capture_desktop instead of a new desktop offer (#99).
pub struct DesktopRole {
    active: tokio::sync::watch::Sender<bool>,
    input: std::sync::Mutex<Option<Arc<crate::input::DesktopInputSession>>>,
    stream: std::sync::Mutex<Option<DesktopStream>>,
    /// The running stream of a promoted peer; aborted on demotion so a quick re-promotion
    /// never leaves two encoders on one track.
    stream_task: std::sync::Mutex<Option<tokio::task::AbortHandle>>,
    /// Capture reinitialized at promotion; the peer's own capture was unavailable when offered.
    screen: std::sync::Mutex<Option<Arc<ScreenCapture>>>,
    /// One session tracker per peer follows every promotion and demotion.
    tracking: std::sync::atomic::AtomicBool,
    request: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>>,
}
impl DesktopRole {
    fn new(desktop: bool) -> Self {
        Self { active: tokio::sync::watch::channel(desktop).0, input: Default::default(),
            stream: Default::default(), stream_task: Default::default(), screen: Default::default(), tracking: Default::default(), request: Default::default() }
    }
    pub fn is_desktop(&self) -> bool { *self.active.borrow() }
    /// A promoted peer whose video ended on its own (capture failed while locked or off, #114);
    /// capture_desktop on it starts the video again.
    fn stream_ended(&self) -> bool { self.stream_task.lock().unwrap().as_ref().is_some_and(|task| task.is_finished()) }
    fn input(&self) -> Option<Arc<crate::input::DesktopInputSession>> { self.input.lock().unwrap().clone() }
    fn screen_or(&self, screen: &Arc<ScreenCapture>) -> Arc<ScreenCapture> {
        self.screen.lock().unwrap().clone().unwrap_or_else(|| screen.clone())
    }
    /// Waits until this peer is a desktop; false once the channel closes first.
    async fn wait_desktop(&self, channel: &RTCDataChannel) -> bool {
        let mut active = self.active.subscribe();
        loop {
            if *active.borrow_and_update() { return true; }
            tokio::select! {
                changed = active.changed() => if changed.is_err() { return false; },
                _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {
                    if matches!(channel.ready_state(), webrtc::data_channel::data_channel_state::RTCDataChannelState::Closing
                        | webrtc::data_channel::data_channel_state::RTCDataChannelState::Closed) { return false; }
                }
            }
        }
    }
}

/// Live desktop video for one peer; a tool peer keeps it unstarted until it becomes a desktop.
#[derive(Clone)]
struct DesktopStream {
    pc: std::sync::Weak<RTCPeerConnection>,
    track: Arc<crate::video_track::VideoTrack>,
    portal_desktop: Option<Arc<crate::desktop_session::PortalDesktop>>,
    capture_size: (u32, u32),
    capture_display: String,
    portal: Option<Arc<crate::portal::PortalSession>>,
    settings_rx: tokio::sync::watch::Receiver<crate::capture_setting::VideoSettings>,
    settings: Arc<tokio::sync::watch::Sender<crate::capture_setting::VideoSettings>>,
    idr_request: Arc<std::sync::atomic::AtomicBool>,
    selection: crate::video_select::Selection,
    chroma_sync: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    reload_at_session: u64,
}

pub struct WebRtcSession {
    pub pc: Arc<RTCPeerConnection>,
    pub capture: Arc<ScreenCapture>,
    pub input: Arc<InputInjector>,
    /// The role this peer was offered with; `role` also covers a later capture_desktop.
    pub desktop: bool,
    pub role: Arc<DesktopRole>,
    pub controller: crate::input::Controller,
    pub context: Arc<Mutex<SessionContext>>,
    actions: Arc<crate::system_actions::PeerActions>,
}

/// Counts a desktop session once it is connected (a failed offer is not a session), while the
/// peer is the desktop. Started once per peer, so a quick demote and re-promote cannot end the
/// count of a session that is still streaming.
fn track_session(end_pc: Arc<RTCPeerConnection>, actions: Arc<crate::system_actions::PeerActions>, role: &DesktopRole) {
    if role.tracking.swap(true, std::sync::atomic::Ordering::AcqRel) { return; }
    let active = role.active.subscribe();
    tokio::spawn(async move {
        use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState as State;
        loop {
            while end_pc.connection_state() != State::Connected || !*active.borrow() {
                if matches!(end_pc.connection_state(), State::Closed | State::Failed) { return; }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            let starting = actions.clone();
            let _ = tokio::task::spawn_blocking(move || starting.session_started()).await;
            while !matches!(end_pc.connection_state(), State::Closed | State::Failed) && *active.borrow() {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            let closed = matches!(end_pc.connection_state(), State::Closed | State::Failed);
            let ending = actions.clone();
            let _ = tokio::task::spawn_blocking(move || ending.session_ended()).await;
            if closed { return; }
        }
    });
}

type EchoReplay = (Arc<std::sync::atomic::AtomicU64>, Arc<std::sync::atomic::AtomicBool>, Arc<std::sync::atomic::AtomicBool>);

/// A promoted tool peer starts its TEXT clipboard after the viewer's echo was answered without
/// ff_clipboard; declare it again on CONTROL once the bridge is ready, as the Portal path does.
fn replay_clipboard_echo(channel: Arc<RTCDataChannel>, sequence: Arc<std::sync::atomic::AtomicU64>,
    (echo_request, echo_seen, clipboard_ready): EchoReplay, role: Arc<DesktopRole>) {
    tokio::spawn(async move {
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if !role.is_desktop() || channel.ready_state() != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open { return; }
            if !clipboard_ready.load(std::sync::atomic::Ordering::Acquire) { continue; }
            // echo_seen is published after the request seq, so reading it first gives a current seq.
            if echo_seen.load(std::sync::atomic::Ordering::Acquire) {
                let request_seq = echo_request.load(std::sync::atomic::Ordering::Acquire);
                let timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                let response = crate::control_protocol::encode_echo_response_with_clipboard(
                    sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed), timestamp, request_seq, true);
                let _ = channel.send(&bytes::Bytes::from(response)).await;
                log::info!("CONTROL clipboard capability declared after capture_desktop");
            }
            return;
        }
    });
}

impl DesktopStream {
    /// Encode current frames only while the peer is connected and still the desktop.
    fn spawn(self, screen: Option<Arc<ScreenCapture>>, mut active: tokio::sync::watch::Receiver<bool>) -> Option<tokio::task::AbortHandle> {
        let streaming_pc = self.pc.upgrade()?;
        let DesktopStream { track: video_track, portal_desktop, capture_size, capture_display, portal,
            settings_rx, settings, idr_request, selection, chroma_sync, reload_at_session, .. } = self;
        let (capture_size, capture_display, portal) = screen.map_or((capture_size, capture_display, portal),
            |screen| ((screen.width, screen.height), screen.display.clone(), screen.portal.clone()));
        Some(tokio::spawn(async move { tokio::select! {
            _ = active.wait_for(|active| !*active) => log::info!("Live desktop stream stopped: a newer desktop took over"),
            _ = async move {
            let screen = if let Some(desktop) = portal_desktop {
                if let Err(error) = crate::locked_video::run(&streaming_pc, &video_track, &desktop, selection).await {
                    log::warn!("Locked desktop video unavailable: {error}");
                }
                let mut updates = desktop.subscribe();
                loop {
                    let state = updates.borrow_and_update().clone();
                    if state.ready() { break Some(state.screen); }
                    if !state.waiting { return; }
                    if crate::video::transport_closed(&streaming_pc) { return; }
                    tokio::select! {
                        changed = updates.changed() => if changed.is_err() { return; },
                        _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {},
                    }
                }
            } else { None };
            let (capture_size, capture_display, portal) = screen.as_ref().map_or(
                (capture_size, capture_display, portal),
                |screen| ((screen.width, screen.height), screen.display.clone(), screen.portal.clone()));
            let state_pc = streaming_pc.clone();
            if let Err(e) = crate::video::stream_desktop_at(streaming_pc, video_track, capture_size, capture_display, portal, settings_rx, settings, idr_request, selection, chroma_sync, reload_at_session).await {
                // A send racing the viewer's close fails with a misleading track error.
                if matches!(state_pc.connection_state(), RTCPeerConnectionState::Closed | RTCPeerConnectionState::Failed) {
                    log::info!("Live desktop stream ended: peer connection closed ({})", e);
                } else {
                    log::error!("Live desktop stream stopped: {}", e);
                }
            }
            } => {},
        } }).abort_handle())
    }
}

impl WebRtcSession {
    pub async fn new(
        capture: Arc<ScreenCapture>,
        input: Arc<InputInjector>,
        signaling_client: Arc<crate::signaling::SignalingClient>,
        desktop: bool,
        transport:crate::session_ice::SessionIce,
        portal_desktop: Option<Arc<crate::desktop_session::PortalDesktop>>,
    ) -> Result<Self, String> {
        let mut m = MediaEngine::default();
        m.register_default_codecs()
            .map_err(|e| format!("Register standard codecs failed: {}", e))?;

        // Encoder, codec preference and 4:4:4 stay fixed for this peer (codec is negotiated at CONTROL open).
        // Read once before the HEVC probe so a concurrent set-encoder/set-codec cannot split capability from CONTROL.
        // reload_at_session is the generation at that snapshot. A probe that finishes later is not a reload.
        let reload_at_session = crate::video_select::reload_generation_value();
        let video_selection = crate::video_select::selection();
        let hevc = desktop && crate::video::hevc_available(video_selection.encoder).await;
        if hevc { crate::video_track::register_hevc(&mut m).map_err(|e| format!("Register HEVC failed: {e}"))?; }

        m.register_header_extension(
            webrtc::rtp_transceiver::rtp_codec::RTCRtpHeaderExtensionCapability { uri: crate::rtp_metadata::CAPTURE_INDEX_URI.into() },
            webrtc::rtp_transceiver::rtp_codec::RTPCodecType::Video, None,
        ).map_err(|e| format!("Register UU capture metadata failed: {}", e))?;
        m.register_header_extension(
            webrtc::rtp_transceiver::rtp_codec::RTCRtpHeaderExtensionCapability { uri: crate::rtp_metadata::NEW_FRAME_URI.into() },
            webrtc::rtp_transceiver::rtp_codec::RTPCodecType::Video, None,
        ).map_err(|e| format!("Register UU fresh-frame metadata failed: {}", e))?;
        let registry = webrtc::api::interceptor_registry::register_default_interceptors(
            webrtc::interceptor::registry::Registry::new(), &mut m,
        ).map_err(|e| format!("Register RTP/RTCP interceptors failed: {}", e))?;
        let mut se = webrtc::api::setting_engine::SettingEngine::default();
        #[cfg(test)]
        if let Some(setting) = &transport.setting_engine { se = setting.clone(); }
        se.set_answering_dtls_role(DTLSRole::Server)
            .map_err(|e| format!("Configure DTLS answer role failed: {}", e))?;
        se.set_ice_multicast_dns_mode(webrtc::ice::mdns::MulticastDnsMode::Disabled);
        // The viewer picked a ZeroTier pair even on the same LAN (36-81 ms, Wi-Fi + overlay);
        // offering only physical interfaces keeps LAN direct, else srflx/relay like official hosts.
        se.set_interface_filter(Box::new(|name: &str| ice_interface_allowed(name)));

        let api = APIBuilder::new()
            .with_media_engine(m)
            .with_setting_engine(se)
            .with_interceptor_registry(registry)
            .build();

        let config = transport.configuration()?;

        let pc = Arc::new(
            api.new_peer_connection(config)
                .await
                .map_err(|e| format!("Create PeerConnection failed: {}", e))?,
        );

        let role = Arc::new(DesktopRole::new(desktop));
        if desktop { *role.input.lock().unwrap() = Some(input.desktop_session()); }
        let context = Arc::new(Mutex::new(SessionContext::default()));
        crate::network_diagnostics::start(Arc::downgrade(&pc));

        // 监听本地 ICE Candidate 并自动发回信令服务器与客户端
        let context_ice = context.clone();
        let sig_ice = signaling_client.clone();
        pc.on_ice_candidate(Box::new(move |c: Option<webrtc::ice_transport::ice_candidate::RTCIceCandidate>| {
            let ctx = context_ice.clone();
            let sig = sig_ice.clone();
            Box::pin(async move {
                if let Some(cand) = c {
                    if let Ok(init) = cand.to_json() {
                        let summary=crate::network_diagnostics::candidate_summary(&init.candidate);
                        log::info!("Local ICE candidate gathered: protocol={} type={}",summary.protocol,summary.kind);
                        let lock = ctx.lock().await;
                        if let Some(cid) = &lock.client_id {
                            let Some((mid, mline)) = lock.bundle_media.clone() else {
                                log::warn!("Local ICE candidate has no negotiated media section");
                                return;
                            };

                            let cand_msg = SoacMessage {
                                msg_type: "candidate".into(),
                                sdp: None,
                                gzip_sdp: None,
                                candidate: Some(serde_json::json!({
                                    "candidate": init.candidate,
                                    "sdpMid": mid,
                                    "sdpMLineIndex": mline,
                                })),
                                ice_id: lock.ice_id.clone(),
                                sdp_mid: None,
                                sdp_mline_index: None,
                                client_id: Some(cid.clone()),
                                app_control_id: lock.app_control_id.clone(),
                            };
                            if let Err(e) = sig.emit_soac(cand_msg).await {
                                log::error!("ICE signaling send failed: {}", e);
                            }


                        } else {
                            log::warn!("Local ICE candidate gathered but client_id not set yet");
                        }
                    }
                } else {
                    log::info!("Local ICE candidate gathering completed.");
                }
            })
        }));

        pc.on_ice_connection_state_change(Box::new(|state| {
            log::info!("ICE Connection State changed: {:?}", state);
            Box::pin(async move {})
        }));

        let connected_context = context.clone();
        let connection_role=role.clone();
        let connected_signal = signaling_client.clone();
        let notified = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let connected_selection = video_selection;
        pc.on_peer_connection_state_change(Box::new(move |state| {
            let ctx = connected_context.clone();
            let input=connection_role.input();
            let sig = connected_signal.clone();
            let notified = notified.clone();
            let video_selection = connected_selection;
            Box::pin(async move {
                log::info!("PeerConnection State changed: {:?}", state);
                if let Some(session)=input.as_ref() {
                    if state==RTCPeerConnectionState::Connected {session.connected();}
                    if matches!(state,RTCPeerConnectionState::Closed|RTCPeerConnectionState::Failed|RTCPeerConnectionState::Disconnected) {session.disconnected();}
                }
                if state == RTCPeerConnectionState::Connected && !notified.swap(true, std::sync::atomic::Ordering::AcqRel) {
                    tokio::spawn(async move {
                    let ctx = ctx.lock().await.clone();
                    if let (Some(client), Some(ice)) = (&ctx.client_id, ctx.ice_id.as_ref().and_then(|v| v.as_str())) {
                        let encoder = video_selection.encoder;
                        let codec_impl = if encoder == crate::video::Encoder::Nvenc { crate::capability::CODEC_IMPL_NVENC } else { 5 };
                        let payload = crate::capability::device_capability(client, ice, 0, crate::video::display_fps() as i32, codec_impl,
                            hevc, crate::video::full_chroma_for(encoder));
                        if let Err(e) = sig.emit_event(crate::capability::FORWARD_SETTING_EVENT, payload).await {
                            log::error!("Device capability notification failed: {}", e);
                        }
                        let capture_impl = match crate::desktop_backend::detect() {
                            Ok(crate::desktop_backend::DesktopBackend::Portal) => "PipeWire",
                            Ok(crate::desktop_backend::DesktopBackend::Wayland) => "Wayland",
                            _ => "X11",
                        };
                        let encoder_impl = match encoder {
                            crate::video::Encoder::Nvenc => "NVIDIA NVENC",
                            crate::video::Encoder::Vaapi => "VAAPI",
                            crate::video::Encoder::Cpu => "Software",
                        };
                        if let Err(e) = sig.emit_event(crate::capability::FORWARD_SETTING_EVENT,
                            crate::capability::sender_para_info(client, ice, capture_impl, encoder_impl)).await {
                            log::error!("Sender parameter notification failed: {}", e);
                        }
                        if let Some(control) = ctx.app_control_id.as_ref().and_then(|v| v.as_str()) {
                            if let Err(e) = sig.emit_event(crate::capability::ICE_FINISHED_EVENT,
                                crate::capability::ice_finished(ice, control)).await {
                                log::error!("Connection completion notification failed: {}", e);
                            }
                        }
                    }
                    });
                }
            })
        }));

        // 注册视频轨
        let video_track = Arc::new(crate::video_track::VideoTrack::new());

        // Set by PLI/FIR; the desktop encoder answers with an IDR on its next frame.
        let idr_request = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sender = pc.add_track(video_track.clone()).await
            .map_err(|e| format!("Add video track failed: {}", e))?;
        let idr_on_feedback = idr_request.clone();
        tokio::spawn(async move {
            let mut feedback = [0u8; 1500];
            let mut reports = 0usize;
            let mut feedback_counts = FeedbackCounts::default();
            while let Ok((packets, _)) = sender.read(&mut feedback).await {
                for packet in packets {
                    feedback_counts.observe(packet.as_any());
                    if packet.as_any().is::<webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication>()
                        || packet.as_any().is::<webrtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest>() {
                        // Dedupe only while an IDR is pending; the encoder clears the flag when it emits one,
                        // so a lost IDR does not block the next recovery request.
                        if !idr_on_feedback.load(std::sync::atomic::Ordering::Acquire) {
                            idr_on_feedback.store(true, std::sync::atomic::Ordering::Release);
                        }
                    }
                    if let Some(report) = packet.as_any().downcast_ref::<webrtc::rtcp::receiver_report::ReceiverReport>() {
                        reports += 1;
                        if reports == 1 || reports % 10 == 0 {
                            for r in &report.reports {
                                let now=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
                                let ntp=(((now.as_secs()+2_208_988_800)&0xffff)<<16) as u32 | ((u64::from(now.subsec_nanos())*65536/1_000_000_000) as u32);
                                let elapsed=ntp.wrapping_sub(r.last_sender_report).wrapping_sub(r.delay);
                                let rtt=if r.last_sender_report!=0&&elapsed<65536*30 {Some(f64::from(elapsed)*1000./65536.)}else{None};
                                log::info!("Video receiver report: ssrc={} last_sequence={} lost={} fraction_lost={} rtcp_rtt_ms={:?}",r.ssrc,r.last_sequence_number,r.total_lost,r.fraction_lost,rtt);
                            }
                            log::info!("Video feedback totals: nack_packets={} pli={} fir={}",feedback_counts.nack_packets,feedback_counts.pli,feedback_counts.fir);
                        }
                    }
                }
            }
        });

        // Desktop sound on the viewer's audio m-line (it always offers Opus).
        if desktop && crate::audio::enabled() && !cfg!(test) {
            let audio_track = Arc::new(TrackLocalStaticSample::new(
                RTCRtpCodecCapability { mime_type: "audio/opus".to_owned(), clock_rate: 48000, channels: 2, ..Default::default() },
                "audio_0".to_owned(), "audio_0".to_owned(),
            ));
            match pc.add_track(audio_track.clone()).await {
                Ok(audio_sender) => {
                    // Drain RTCP so the interceptors keep running.
                    tokio::spawn(async move { let mut rtcp = [0u8; 1500]; while audio_sender.read(&mut rtcp).await.is_ok() {} });
                    tokio::spawn(crate::audio::stream_audio(pc.clone(), audio_track));
                }
                Err(e) => log::warn!("Add audio track failed: {}", e),
            }
        }

        // Encode current X11 frames only while a peer is connected.
        let streaming_pc = pc.clone();
        let capture_size = (capture.width, capture.height);
        let capture_display = capture.display.clone();
        // Per-connection 画质/帧率, changed by the viewer's capture-setting RPCs.
        let (video_settings, video_settings_rx) = tokio::sync::watch::channel(crate::capture_setting::VideoSettings::default());
        let video_settings = Arc::new(video_settings);
        // Per-connection mute / lock-after-session settings (安全 menu).
        let peer_actions = Arc::new(crate::system_actions::PeerActions::default());
        let session_actions = peer_actions.clone();
        let desktop_sound = desktop && crate::audio::enabled();
        if desktop && !cfg!(test) { track_session(pc.clone(), peer_actions.clone(), &role); }
        // Live encoder switch (SIGUSR2) sends CaptureSettingSync when chroma is downgraded.
        let (chroma_sync_tx, chroma_sync_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let chroma_sync_rx = Arc::new(std::sync::Mutex::new((!cfg!(test)).then_some(chroma_sync_rx)));
        // File/terminal/port-mapping connections carry no desktop video until capture_desktop.
        let control_video_track = video_track.clone();
        let stream = DesktopStream {
            pc: Arc::downgrade(&streaming_pc), track: video_track, portal_desktop: portal_desktop.clone(),
            capture_size, capture_display, portal: capture.portal.clone(), settings_rx: video_settings_rx,
            settings: video_settings.clone(), idr_request, selection: video_selection,
            chroma_sync: (!cfg!(test)).then_some(chroma_sync_tx), reload_at_session,
        };
        drop(streaming_pc);
        if cfg!(test) { drop(stream); }
        else if desktop { let _ = stream.spawn(None, role.active.subscribe()); }
        else { *role.stream.lock().unwrap() = Some(stream); }
        let input_clone = input.clone();
        let control_capture = capture.clone();
        let desktop_pc = Arc::downgrade(&pc);
        let control_sequence = Arc::new(std::sync::atomic::AtomicU64::new(1));
        // Capability belongs to the granted session, including while GNOME is
        // locked. Echo can precede TEXT readiness and CONTROL auto-unlock;
        // advertising false here would disable clipboard for the entire peer.
        let portal_clipboard_supported = desktop && portal_desktop.as_ref().map_or_else(
            || capture.portal.as_ref().is_some_and(|session| session.clipboard_supported()),
            |manager| {
                let state = manager.snapshot();
                state.ready()
                    && state.screen.portal.as_ref().is_some_and(|session| session.clipboard_supported())
            });
        let text_clipboard_ready = Arc::new(std::sync::atomic::AtomicBool::new(portal_clipboard_supported));
        let last_echo_request = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let echo_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Same watcher the desktop session uses: the manager's when Portal owns the
        // session, otherwise the global lock watcher. Unknown state fails closed below.
        let channel_lock_state = portal_desktop.as_ref().map_or_else(
            crate::session_lock::watch_locked, |manager| manager.lock_state());
        // One file-transfer session per peer. The viewer may send FileTransferFtp RPCs on CONTROL,
        // but it receives file traffic only through streamerDidReceiveFileData, so every reply and
        // host-initiated request goes out on FILE_DATA_CHANNEL.
        let file_channel: Arc<std::sync::Mutex<Option<Arc<RTCDataChannel>>>> = Arc::new(std::sync::Mutex::new(None));
        let (file_tx, mut file_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let peer_files = crate::file_trans::FileSession::new(file_tx, control_sequence.clone());
        // 端口映射 frames (Message 27) also travel on FILE_DATA_CHANNEL; one session per peer.
        let port_mapping = crate::port_mapping::Session::new(file_channel.clone(), control_sequence.clone());
        {
            let (end_pc, mapping) = (pc.clone(), port_mapping.clone());
            tokio::spawn(async move {
                use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState as State;
                while !matches!(end_pc.connection_state(), State::Closed | State::Failed) {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
                mapping.shutdown();
            });
        }
        {
            let slot = file_channel.clone();
            tokio::spawn(async move {
                while let Some(message) = file_rx.recv().await {
                    let mut channel = slot.lock().unwrap().clone();
                    for _ in 0..100 {
                        if channel.is_some() { break; }
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        channel = slot.lock().unwrap().clone();
                    }
                    let Some(channel) = channel else { log::warn!("File transfer reply dropped: no FILE_DATA_CHANNEL"); continue; };
                    if let Err(e) = channel.send(&bytes::Bytes::from(message)).await { log::warn!("FILE_DATA_CHANNEL send failed: {}", e); }
                }
            });
        }

        // Settings arrive on CONTROL or TEXT; serialize both queues for this peer.
        let settings_serial=Arc::new(Mutex::new(()));
        let data_role = role.clone();
        // 监听 DataChannel
        pc.on_data_channel(Box::new(move |d: Arc<RTCDataChannel>| {
            let label = d.label().to_string();
            log::info!("New DataChannel received: {}", label);

            let peer_settings_serial=settings_serial.clone();
            let input_inst = input_clone.clone();
            let channel_role=data_role.clone();
            let screen = control_capture.clone();
            let portal_desktop = portal_desktop.clone();
            let desktop_pc = desktop_pc.clone();
            let out_sequence = control_sequence.clone();
            let shared_clipboard_ready = text_clipboard_ready.clone();
            let last_echo_request = last_echo_request.clone();
            let echo_seen = echo_seen.clone();
            let channel_lock_state = channel_lock_state.clone();
            let peer_files = peer_files.clone();
            let peer_port_mapping = port_mapping.clone();
            let peer_video_settings = video_settings.clone();
            let peer_video_track = control_video_track.clone();
            let peer_actions = peer_actions.clone();
            let peer_selection = video_selection;
            let chroma_sync_rx = chroma_sync_rx.clone();
            if label == "FILE_DATA_CHANNEL" { *file_channel.lock().unwrap() = Some(d.clone()); }

            Box::pin(async move {
                let lbl = label.clone();
                let inp = input_inst.clone();
                let d_reply = d.clone();
                let reply_sequence = out_sequence.clone();
                let channel_video_settings = peer_video_settings.clone();
                let channel_actions = peer_actions.clone();
                let clipboard_input = Arc::new(std::sync::Mutex::new(None::<tokio::sync::watch::Sender<Option<String>>>));
                let clipboard_assembly = Arc::new(std::sync::Mutex::new(crate::clipboard_protocol::Assembly::default()));
                let rich_state=Arc::new(std::sync::Mutex::new(crate::clipboard_rich::State::default()));
                let clipboard_enabled = shared_clipboard_ready.clone();
                if label=="CONTROL_DATA_CHANNEL" {
                    let closing=channel_role.clone();
                    d.on_close(Box::new(move || {
                        let closing=closing.input();
                        Box::pin(async move {if let Some(session)=closing {session.disconnected();}})
                    }));
                }
                // Ordered outbound queue for protocol replies produced outside this callback.
                // webrtc-rs announces the channel before handle_open (ready_state Connecting);
                // send() returns ErrClosedPipe until Open. Draining CONTROL here would drop
                // Message 25 and every later reply. Start CONTROL from the on_open below
                // (one handler; a second on_open replaces it). Receiver taken once.
                let mut control_drain: Option<Box<dyn (FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>) + Send + Sync>> = None;
                let (outbound, outbound_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
                let outbound_rx = Arc::new(tokio::sync::Mutex::new(Some(outbound_rx)));
                if label == "CONTROL_DATA_CHANNEL" {
                    if let Some(mut chroma_sync_rx) = chroma_sync_rx.lock().unwrap().take() {
                        let sync_out = outbound.clone();
                        let sync_seq = reply_sequence.clone();
                        tokio::spawn(async move {
                            while let Some(payload) = chroma_sync_rx.recv().await {
                                let timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                                let seq = sync_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                let _ = sync_out.send(crate::initialization::envelope(seq, timestamp, 25, &payload));
                            }
                        });
                    }
                }
                {
                    let out_channel = d.clone();
                    let out_label = label.clone();
                    let outbound_rx = outbound_rx.clone();
                    let start_drain = move || {
                        let out_channel = out_channel.clone();
                        let out_label = out_label.clone();
                        let outbound_rx = outbound_rx.clone();
                        Box::pin(async move {
                            let Some(mut outbound_rx) = take_outbound_once(&outbound_rx).await else { return; };
                            tokio::spawn(async move {
                                while let Some(message) = outbound_rx.recv().await {
                                    if let Err(e) = out_channel.send(&bytes::Bytes::from(message)).await {
                                        log::warn!("{} send failed: {}", out_label, e); break;
                                    }
                                }
                            });
                        }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
                    };
                    if label != "CONTROL_DATA_CHANNEL" { tokio::spawn(start_drain()); }
                    else { control_drain = Some(Box::new(start_drain)); }
                }
                let (capture_tx, mut capture_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
                if matches!(label.as_str(),"CONTROL_DATA_CHANNEL"|"TEXT_DATA_CHANNEL") {
                    let settings = channel_video_settings.clone();
                    let track = peer_video_track.clone();
                    let response_tx = outbound.clone();
                    let sequence = reply_sequence.clone();
                    let manager = portal_desktop.clone();
                    let channel = Arc::downgrade(&d);
                    let initial_screen = screen.clone();
                    let capture_role=channel_role.clone();
                    let settings_serial=peer_settings_serial.clone();
                    let capture_selection = peer_selection;
                    tokio::spawn(async move {
                        let mut health = tokio::time::interval(std::time::Duration::from_millis(100));
                        loop {
                            let request = tokio::select! {
                                request = capture_rx.recv() => match request { Some(request) => request, None => break },
                                _ = health.tick() => {
                                    if channel.upgrade().is_none_or(|channel| matches!(
                                        channel.ready_state(),
                                        webrtc::data_channel::data_channel_state::RTCDataChannelState::Closing
                                            | webrtc::data_channel::data_channel_state::RTCDataChannelState::Closed
                                    )) { break; }
                                    continue;
                                }
                            };
                            let _serial=settings_serial.lock().await;
                            let (desktop,switch_owner)=(capture_role.is_desktop(),capture_role.input());
                            if desktop && switch_owner.as_ref().is_none_or(|owner|!owner.is_current()) {continue;}
                            let reject=|reason:&str| {
                                if let Some(response)=crate::capture_setting::rejected_request(&request,reason) {
                                    let ts=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                                    let _=response_tx.send(crate::initialization::envelope(sequence.fetch_add(1,std::sync::atomic::Ordering::Relaxed),ts,22,&response));
                                }
                            };
                            let source_before=crate::wayland::selected();
                            let settings_before=*settings.borrow();
                            if desktop && !crate::wayland::outputs().is_empty() {
                                if let Some(id)=crate::wayland::requested_output(&request) {
                                    if crate::wayland::source(id).is_none() {reject("Unknown screen");continue;}
                                    if id!=source_before {
                                        let mut changed=false;
                                        if let Some(owner)=&switch_owner {owner.with_current(|| {
                                            owner.release_input();
                                            match crate::wayland::select(id) {
                                                Ok(())=>{
                                                    let (source_id,source_generation)=crate::wayland::selection();
                                                    settings.send_modify(|s|{s.source_id=source_id;s.source_generation=source_generation;s.output=None;});
                                                    changed=true;
                                                },
                                                Err(error)=>{log::warn!("Screen switch rejected: {}",error);reject(&error);},
                                            }
                                        });}
                                        if !changed {continue;}
                                    }
                                }
                            }
                            let active_screen = if desktop {
                                if let Some(manager) = &manager {
                                    loop {
                                        let state = manager.snapshot();
                                        if !state.waiting && state.screen.init_error.is_some() {
                                            log::warn!("Capture setting request dropped: Portal desktop unavailable");
                                            break None;
                                        }
                                        let live = state.screen.init_error.is_none()
                                            && (cfg!(test) || state.screen.portal.as_ref().is_some_and(|portal| portal.is_alive()))
                                            && *manager.lock_state().borrow() == Some(false);
                                        if live { break Some(state.screen); }
                                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                                        if channel.upgrade().is_none_or(|channel| channel.ready_state() != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open) { return; }
                                    }
                                } else {
                                    Some(capture_role.screen_or(&initial_screen))
                                }
                            } else {
                                Some(capture_role.screen_or(&initial_screen))
                            };
                            let Some(active_screen) = active_screen else { continue; };
                            let input_size = if active_screen.init_error.is_none() {
                                (active_screen.width, active_screen.height)
                            } else { (1920, 1080) };
                            let input_size=crate::wayland::input_size(input_size);
                            let source_changed=source_before!=crate::wayland::selected();
                            if source_changed {
                                let target=crate::wayland::selection();
                                let deadline=tokio::time::Instant::now()+std::time::Duration::from_secs(10);
                                while crate::wayland::selection()==target && !crate::wayland::input_ready()
                                    && !crate::wayland::switch_failed() && tokio::time::Instant::now()<deadline {
                                    if channel.upgrade().is_none_or(|c|c.ready_state()!=webrtc::data_channel::data_channel_state::RTCDataChannelState::Open)
                                        || switch_owner.as_ref().is_none_or(|owner|!owner.is_current()) {return;}
                                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                                }
                                if switch_owner.as_ref().is_none_or(|owner|!owner.is_current()) || crate::wayland::selection()!=target {continue;}
                                if !crate::wayland::input_ready() {
                                    if let Some(owner)=&switch_owner {owner.with_current(|| {
                                        if crate::wayland::selection()==target && crate::wayland::select(source_before).is_ok() {
                                            let (source_id,source_generation)=crate::wayland::selection();
                                            settings.send_replace(crate::capture_setting::VideoSettings {source_id,source_generation,..settings_before});
                                        }
                                    });}
                                    log::warn!("Screen switch failed; previous source requested");
                                    reject("Screen switch failed; restoring previous screen");
                                    continue;
                                }
                                let ts=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                                if let Ok(message)=crate::capture_setting::screen_sources(sequence.fetch_add(1,std::sync::atomic::Ordering::Relaxed),ts,input_size,None,"Linux desktop") {let _=response_tx.send(message);}
                                if let Ok(message)=crate::initialization::encode_capture_change(sequence.fetch_add(1,std::sync::atomic::Ordering::Relaxed),ts,0,crate::wayland::selected() as i32,"Linux desktop") {let _=response_tx.send(message);}
                            }
                            let Ok(fields) = crate::protocol::decode_wire(&request) else { continue; };
                            use crate::protocol::WireValue;
                            let Some(rpc) = fields.iter().find_map(|f| match (f.number, &f.value) {
                                (21, WireValue::Bytes(rpc)) => Some(*rpc), _ => None,
                            }) else { continue; };
                            let Ok(rpc_fields) = crate::protocol::decode_wire(rpc) else { continue; };
                            // X11: 分辨率 changes the display mode itself, like the official Windows host; the stream
                            // then carries the new native size instead of a scaled one (desktop_mode). With
                            // UU_X11_MULTISCREEN the monitor layout stays as it is and the stream is scaled.
                            let resized = if desktop && active_screen.init_error.is_none() && active_screen.portal.is_none()
                                && active_screen.backend == crate::desktop_backend::DesktopBackend::X11 && crate::desktop_mode::enabled()
                                && !crate::wayland::x11_multiscreen() {
                                match crate::capture_setting::requested_resolution(&rpc_fields) {
                                    Some(request) => {
                                        let display = active_screen.display.clone();
                                        tokio::task::spawn_blocking(move || crate::desktop_mode::apply(&display, request)).await.ok().flatten()
                                            .filter(|size| *size != input_size)
                                    },
                                    None => None,
                                }
                            } else { None };
                            let input_size = resized.unwrap_or(input_size);
                            let before = *settings.borrow();
                            let output_before = before.output;
                            let revision = track.revision();
                            let apply=|| crate::capture_setting::handle_rpc_for(
                                &rpc_fields, &settings, input_size,
                                track.supports(crate::capture_setting::VideoCodec::H264),
                                track.supports(crate::capture_setting::VideoCodec::H265), capture_selection);
                            let response=if let Some(owner)=&switch_owner {
                                let mut response=None;owner.with_current(||response=apply());response
                            } else {apply()};
                            let Some(response)=response else {continue;};
                            // Restart capture even when the encoded settings are unchanged (native size, no scaling).
                            if resized.is_some() { settings.send_modify(|s| s.source_generation = s.source_generation.wrapping_add(1)); }
                            let codec_changed = settings.borrow().codec != before.codec;
                            if desktop && codec_changed && !track.wait_for_applied(revision, &settings).await {
                                log::warn!("Codec switch did not produce a transmitted frame; success reply withheld");
                                continue;
                            }
                            let timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default().as_secs();
                            let next = || sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let _ = response_tx.send(crate::initialization::envelope(next(), timestamp, 22, &response));
                            let current = *settings.borrow();
                            if current.output != output_before || resized.is_some() {
                                match crate::capture_setting::screen_sources(next(), timestamp, input_size, current.output, "Linux desktop") {
                                    Ok(screens) => { let _ = response_tx.send(screens); }
                                    Err(error) => log::warn!("Screen sources update failed: {}", error),
                                }
                            }
                            let sync = crate::capture_setting::sync_payload(&current, input_size);
                            let _ = response_tx.send(crate::initialization::envelope(next(), timestamp, 25, &sync));
                        }
                    });
                }
                // The viewer sends FileTransferFtp RPCs (Message 21/22) on CONTROL; accept them on TEXT/FILE too.
                let file_session = if matches!(label.as_str(), "CONTROL_DATA_CHANNEL" | "TEXT_DATA_CHANNEL" | "FILE_DATA_CHANNEL") {
                    Some(peer_files.clone())
                } else { None };
                // CT_TERMINAL: TERM frames and terminal RPCs (check_env/rename) on BINARY.
                let terminal_conn = if label == "BINARY_DATA_CHANNEL" {
                    Some(Arc::new(crate::terminal::TerminalConn::new(outbound.clone())))
                } else { None };
                if label == "CONTROL_DATA_CHANNEL" || label == "TEXT_DATA_CHANNEL" {
                    let channel = d.clone();
                    let initial_sequence = out_sequence.clone();
                    let clipboard_sink = clipboard_input.clone();
                    let clipboard_ready = clipboard_enabled.clone();
                    let last_echo_request = last_echo_request.clone();
                    let echo_seen = echo_seen.clone();
                    let rich_outgoing=rich_state.clone();
                    let open_actions = channel_actions.clone();
                    let open_video = channel_video_settings.clone();
                    let open_track = peer_video_track.clone();
                    let open_desktop = portal_desktop.clone();
                    let open_owner = channel_role.input();
                    let open_role = channel_role.clone();
                    let open_screen = screen.clone();
                    let open_selection = peer_selection;
                    d.on_open(Box::new(move || {
                        if let Some(start_drain) = control_drain { tokio::spawn(start_drain()); }
                        let init_actions = open_actions.clone();
                        let init_video = open_video.clone();
                        let init_track = open_track.clone();
                        let channel = channel.clone();
                        let sequence = initial_sequence.clone();
                        let screen = open_screen.clone();
                        let clipboard_sink = clipboard_sink.clone();
                        let clipboard_ready = clipboard_ready.clone();
                        let rich_outgoing=rich_outgoing.clone();
                        let portal_desktop = open_desktop.clone();
                        let owner = open_owner.clone();
                        let role = open_role.clone();
                        let pc = desktop_pc.upgrade();
                        Box::pin(async move {
                          // Permission dialogs and first-frame waits must not occupy the
                          // channel-open callback or delay CONTROL message dispatch.
                          tokio::spawn(async move {
                          if channel.label() == "TEXT_DATA_CHANNEL" {
                            loop {
                              if !role.is_desktop() && !role.wait_desktop(&channel).await { return; }
                              let screen = if let Some(desktop) = &portal_desktop {
                                  let mut updates = desktop.subscribe();
                                  let Some(_) = crate::desktop_session::wait_ready(&mut updates, &channel).await else { return; };
                                  let mut locked = desktop.lock_state();
                                  loop {
                                      let state = desktop.snapshot();
                                      if !state.waiting && state.screen.init_error.is_some() { return; }
                                      if let Some(session) = state.screen.portal.as_ref() {
                                          if session.is_alive() && *locked.borrow() == Some(false) {
                                              break state.screen;
                                          }
                                      }
                                      tokio::select! {
                                          _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {},
                                          changed = locked.changed() => if changed.is_err() { return; },
                                          _ = async {
                                              loop {
                                                  if matches!(channel.ready_state(), webrtc::data_channel::data_channel_state::RTCDataChannelState::Closing
                                                      | webrtc::data_channel::data_channel_state::RTCDataChannelState::Closed) { return; }
                                                  tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                                              }
                                          } => return,
                                      }
                                  }
                              } else { role.screen_or(&screen) };
                              let bridge = if let Some(session)=screen.portal.clone() { crate::clipboard::start_portal(session) } else { crate::clipboard::start(screen.display.clone()) };
                              *clipboard_sink.lock().unwrap() = Some(bridge.incoming);
                              let clipboard_channel = channel.clone();
                              let clipboard_sequence = sequence.clone();
                              let clipboard_cleanup = clipboard_sink.clone();
                              let rich_outgoing = rich_outgoing.clone();
                              let ready = bridge.ready.await.unwrap_or(false);
                              clipboard_ready.store(ready,std::sync::atomic::Ordering::Relaxed);
                              {
                                  if !ready {
                                      *clipboard_cleanup.lock().unwrap() = None;
                                      log::warn!("Native text clipboard unavailable"); return;
                                  }
                                  log::info!("Native text clipboard bridge ready");
                                  let mut updates = bridge.outgoing;
                                  let mut health = tokio::time::interval(std::time::Duration::from_millis(200));
                                  let mut active = role.active.subscribe();
                                  loop {
                                      tokio::select! {
                                          _ = health.tick() => {
                                              if clipboard_channel.ready_state() != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open { break; }
                                          },
                                          _ = async { let _ = active.wait_for(|active| !*active).await; } => break,
                                          changed = updates.changed() => {
                                              if changed.is_err() { break; }
                                              let text = updates.borrow_and_update().clone();
                                              let Some(text) = text else { continue; };
                                              {
                                                  let request_id=clipboard_sequence.fetch_add(1,std::sync::atomic::Ordering::Relaxed);
                                                  let payload = match rich_outgoing.lock().unwrap().offer(request_id,text.clone()) {
                                                      Ok(payload)=>payload,Err(reason)=>{log::warn!("Clipboard offer rejected: {}",reason);continue;}
                                                  };
                                                  let timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                                                  let message = crate::initialization::envelope(clipboard_sequence.fetch_add(1,std::sync::atomic::Ordering::Relaxed),timestamp,21,&payload);
                                                  if !matches!(tokio::time::timeout(std::time::Duration::from_secs(2),clipboard_channel.send(&bytes::Bytes::from(message))).await,Ok(Ok(_))) {
                                                      log::warn!("Native clipboard RPC send failed"); break;
                                                  }
                                              }
                                              log::info!("Native clipboard text format offered: bytes={}",text.len());
                                          }
                                      }
                                  }
                                  *clipboard_cleanup.lock().unwrap() = None;
                                  clipboard_ready.store(false,std::sync::atomic::Ordering::Relaxed);
                              }
                              if channel.ready_state() != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open { return; }
                              // A newer desktop took over this promoted tool peer; capture_desktop restarts the clipboard.
                              log::info!("Native text clipboard stopped: a newer desktop took over");
                            }
                          }
                            let replay = (last_echo_request.clone(), echo_seen.clone(), clipboard_ready.clone());
                            let early_desktop_selection = desktop && portal_desktop.is_some();
                            if early_desktop_selection {
                                // Android 4.43.0's unlock presenter requires CaptureChange
                                // CT_DESKTOP/CT_WINDOW as well as SecureDesktopState before
                                // sending UnlockRemoteSystem, even with no video frames.
                                let timestamp = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                                let message = crate::initialization::encode_capture_change(
                                    sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                                    timestamp, 0, crate::wayland::selected() as i32, "Linux desktop").expect("desktop capture type");
                                if let Err(error) = channel.send(&bytes::Bytes::from(message)).await {
                                    log::error!("CONTROL desktop selection failed: {error}");
                                    return;
                                }
                                log::info!("CONTROL initialization field 8 sent (desktop selection before capture)");
                            }
                            if desktop {
                                // Report secure-desktop state before waiting for capture,
                                // so AUTO_UNLOCK_SYSTEM can arrive on this same CONTROL.
                                let state = portal_desktop.as_ref().map_or_else(
                                    crate::session_lock::watch_locked, |desktop| desktop.lock_state());
                                tokio::spawn(crate::session_lock::report_states(channel.clone(), sequence.clone(), state));
                            }
                            if let Some(manager) = &portal_desktop {
                                let mut updates = manager.subscribe();
                                let channel = channel.clone();
                                let sequence = sequence.clone();
                                let echo_request = last_echo_request.clone();
                                let clipboard_ready = clipboard_ready.clone();
                                tokio::spawn(async move {
                                    let mut health = tokio::time::interval(std::time::Duration::from_millis(100));
                                    loop {
                                        tokio::select! {
                                            changed = updates.changed() => if changed.is_err() { break; },
                                            _ = health.tick() => {
                                                if channel.ready_state() != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open { break; }
                                                continue;
                                            }
                                        }
                                        let state = updates.borrow_and_update().clone();
                                        let supported = state.ready()
                                            && state.screen.portal.as_ref().is_some_and(|portal| portal.clipboard_supported());
                                        clipboard_ready.store(supported, std::sync::atomic::Ordering::Release);
                                        // echo_seen is published after the request seq, so reading it
                                        // first guarantees a non-stale sequence for the replay.
                                        if supported && echo_seen.load(std::sync::atomic::Ordering::Acquire) {
                                            let request_seq = echo_request.load(std::sync::atomic::Ordering::Acquire);
                                            let timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                                            let response = crate::control_protocol::encode_echo_response_with_clipboard(
                                                sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                                                timestamp, request_seq, true);
                                            let _ = channel.send(&bytes::Bytes::from(response)).await;
                                        }
                                        if !state.waiting && state.screen.init_error.is_some() { break; }
                                    }
                                });
                            }
                            let screen = if let Some(portal) = &portal_desktop {
                                if !cfg!(test) {
                                    if let (Some(owner), Some(pc)) = (owner, pc) {
                                        tokio::spawn(portal.clone().run(pc, channel.clone(), owner));
                                    }
                                }
                                let snapshot = portal.snapshot();
                                if !snapshot.ready() {
                                    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                                    let message = crate::initialization::envelope(
                                        sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed), ts, 13,
                                        &init_actions.metrics_payload(desktop_sound, snapshot.permission(true)));
                                    if channel.send(&bytes::Bytes::from(message)).await.is_err() { return; }
                                    let mut updates = portal.subscribe();
                                    match crate::desktop_session::wait_ready(&mut updates, &channel).await {
                                        Some(screen) => screen,
                                        None => {
                                            if channel.ready_state() == webrtc::data_channel::data_channel_state::RTCDataChannelState::Open {
                                                let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                                                let message = crate::initialization::envelope(
                                                    sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed), ts, 13,
                                                    &init_actions.metrics_payload(desktop_sound, crate::system_metrics::Permission::Reject));
                                                let _ = channel.send(&bytes::Bytes::from(message)).await;
                                            }
                                            return;
                                        }
                                    }
                                } else { snapshot.screen }
                            } else { screen };
                            let next_seq = || sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            // Default file-transfer folder: the desktop user's home directory.
                            let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
                            let (mut desktop, mut promoted, mut lock_reported) = (desktop, false, desktop);
                            let mut screen = screen;
                            loop {
                            loop {
                                let timestamp = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                                // Tool peers are permitted without a desktop; the port-mapping viewer aborts on Reject.
                                let desktop_permission=if screen.init_error.is_none() || !desktop {
                                    crate::system_metrics::Permission::Allow
                                } else { crate::system_metrics::Permission::Reject };
                                // Without a desktop (headless/portal before consent) the port-mapping tool still waits
                                // for ScreenSources/CaptureChange and aborts otherwise: tool peers get a nominal screen.
                                let screen_size=crate::wayland::input_size(if screen.init_error.is_none() { (screen.width, screen.height) } else { (1920, 1080) });
                                let messages = [
                                    (8, crate::initialization::encode_capture_change(next_seq(), timestamp, 0, crate::wayland::selected() as i32, "Linux desktop")),
                                    (13, Ok(crate::initialization::envelope(next_seq(), timestamp, 13, &init_actions.metrics_payload(desktop_sound, desktop_permission)))),
                                    (26, Ok(crate::initialization::envelope(next_seq(), timestamp, 26,
                                        &crate::system_metrics::remote_download_path(&home)))),
                                    (18, Ok(crate::initialization::envelope(next_seq(), timestamp, 18,
                                        &crate::capture_setting::codec_negotiation(screen_size, crate::video::display_fps(), init_track.supports(crate::capture_setting::VideoCodec::H265))))),
                                    (7, crate::capture_setting::screen_sources(next_seq(), timestamp, screen_size, init_video.borrow().output, "Linux desktop")),
                                ];
                                if desktop && screen.init_error.is_none() {
                                    let codec = crate::capture_setting::preferred_codec_for(init_track.supports(crate::capture_setting::VideoCodec::H264),
                                        init_track.supports(crate::capture_setting::VideoCodec::H265), open_selection);
                                    init_video.send_if_modified(|current| if current.codec != codec {
                                        log::info!("Desktop codec selected: {:?}", codec); current.codec = codec; true } else { false });
                                }
                                for (field, message) in messages {
                                    if early_desktop_selection && field == 8 { continue; }
                                    if promoted && field == 26 { continue; }
                                    if (!desktop || screen.init_error.is_some()) && field == 18 { continue; }
                                    if desktop && screen.init_error.is_some() && matches!(field,7|8) { continue; }
                                    let message = match message {
                                        Ok(message) => message,
                                        Err(e) => { log::error!("CONTROL initialization encode failed: {}", e); return; }
                                    };
                                    if let Err(e) = channel.send(&bytes::Bytes::from(message)).await {
                                        log::error!("CONTROL initialization field {} failed: {}", field, e);
                                        return;
                                    }
                                    log::info!("CONTROL initialization field {} sent", field);
                                }
                                if desktop { break; }
                                // The viewer opened the desktop on this tool connection (#99): repeat the
                                // desktop initialization once the host has started its video and input.
                                if !role.wait_desktop(&channel).await { return; }
                                (desktop, promoted) = (true, true);
                                screen = role.screen_or(&screen);
                                replay_clipboard_echo(channel.clone(), sequence.clone(), replay.clone(), role.clone());
                            }
                            if !lock_reported {
                                lock_reported = true;
                                tokio::spawn(crate::session_lock::report_states(channel.clone(), sequence.clone(), crate::session_lock::watch_locked()));
                            }
                            // Mirror the Linux pointer shape; the video itself carries no pointer.
                            let mut shapes = crate::cursor::monitor(&screen.display);
                            let mut health = tokio::time::interval(std::time::Duration::from_millis(200));
                            let mut sent = 0usize;
                            let mut active = role.active.subscribe();
                            'cursor: loop {
                                let current = shapes.borrow_and_update().clone();
                                if let Some(cursor) = current {
                                    let timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                                    let message = crate::cursor::encode_cursor_shape(next_seq(), timestamp, &cursor);
                                    if channel.send(&bytes::Bytes::from(message)).await.is_err() { return; }
                                    sent += 1;
                                    if sent <= 3 || sent % 100 == 0 {
                                        log::info!("Cursor shape sent: count={} size={}x{} hotspot={},{} png_bytes={}", sent, cursor.width, cursor.height, cursor.hot_x, cursor.hot_y, cursor.png.len());
                                    }
                                }
                                loop {
                                    tokio::select! {
                                        changed = shapes.changed() => { if changed.is_err() { return; } break; },
                                        _ = active.wait_for(|active| !*active) => break 'cursor,
                                        _ = health.tick() => {
                                            if channel.ready_state() != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open { return; }
                                        },
                                    }
                                }
                            }
                            // A newer desktop took over this promoted tool peer; wait for capture_desktop again.
                            if !role.wait_desktop(&channel).await { return; }
                            screen = role.screen_or(&screen);
                            replay_clipboard_echo(channel.clone(), sequence.clone(), replay.clone(), role.clone());
                            }
                          });
                        })
                    }));
                }


                d.on_message(Box::new(move |msg: DataChannelMessage| {
                    // Keep the whole state: a cached screen waiting for a closed Portal is not ready for input.
                    let (screen, desktop_ready) = portal_desktop.as_ref().map_or_else(|| (channel_role.screen_or(&screen), true), |desktop| {
                        let state = desktop.snapshot();
                        let ready = state.ready();
                        (state.screen, ready)
                    });
                    let channel_name = lbl.clone();
                    let input_worker = inp.clone();
                    let message_input_session=channel_role.input();
                    let message_role=channel_role.clone();
                    let terminal_worker = terminal_conn.clone();
                    let rpc_out = outbound.clone();
                    let reply_chan = d_reply.clone();
                    let out_sequence = reply_sequence.clone();
                    let clipboard_sink = clipboard_input.clone();
                    let clipboard_assembly = clipboard_assembly.clone();
                    let clipboard_ready = clipboard_enabled.clone();
                    let rich_incoming=rich_state.clone();
                    let file_worker = file_session.clone();
                    let mapping = peer_port_mapping.clone();
                    let actions = channel_actions.clone();
                    let portal_for_message = portal_desktop.clone();
                    let lock_for_message = channel_lock_state.clone();
                    let echo_for_message = last_echo_request.clone();
                    let echo_seen_for_message = echo_seen.clone();
                    let capture_requests = capture_tx.clone();

                    Box::pin(async move {
                        let input_size = crate::wayland::input_size((screen.width, screen.height));
                        // After a RandR resize the stream restarts capture at the new size; map the pointer to it.
                        let live_x11 = screen.init_error.is_none() && screen.portal.is_none() && screen.backend == crate::desktop_backend::DesktopBackend::X11;
                        if let Some(term) = &terminal_worker {
                            if crate::terminal::is_frame(&msg.data) { term.handle(&msg.data); return; }
                        }
                        if matches!(channel_name.as_str(),"CONTROL_DATA_CHANNEL"|"TEXT_DATA_CHANNEL")
                            && (is_capture_setting_message(&msg.data) || (!crate::wayland::outputs().is_empty() && crate::wayland::requested_output(&msg.data).is_some())) {
                            // Back-pressure like the old inline handler instead of dropping the latest setting.
                            if capture_requests.send(msg.data.to_vec()).await.is_err() {
                                log::warn!("Capture setting queue closed; request dropped");
                            }
                            return;
                        }
                        if let Ok(fields) = crate::protocol::decode_wire(&msg.data) {
                            use crate::protocol::WireValue;
                            let rpc = fields.iter().find_map(|f| match (f.number, &f.value) { (21, WireValue::Bytes(b)) => Some(*b), _ => None });
                            let rpc_fields = rpc.and_then(|r| crate::protocol::decode_wire(r).ok());
                            if let Some(response) = rpc_fields.as_deref().and_then(|r| crate::system_actions::plugin_response(r).or_else(|| crate::system_actions::session_rpc_response(r))) {
                                let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                                let _ = rpc_out.send(crate::initialization::envelope(out_sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed), ts, 22, &response));
                                return;
                            }
                            if let Some(response) = rpc.and_then(|r| crate::protocol::decode_wire(r).ok()).and_then(|r| crate::terminal::rpc_response(&r)) {
                                let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                                let _ = rpc_out.send(crate::initialization::envelope(out_sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed), ts, 22, &response));
                                return;
                            }
                        }
                        if let Ok(fields) = crate::protocol::decode_wire(&msg.data) {
                            if mapping.handle(&fields, &channel_name) { return; }
                        }
                        if let Some(files) = &file_worker {
                            if let Ok(fields) = crate::protocol::decode_wire(&msg.data) {
                                if files.handle(&fields) {
                                    static ROUTE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
                                    if ROUTE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 20 {
                                        log::info!("File transfer RPC received on {}", channel_name);
                                    }
                                    return;
                                }
                            }
                        }
                        // Temporary bounded clipboard routing diagnostics: schema only,
                        // never text values, payload bytes, key values or clipboard content.
                        if channel_name == "TEXT_DATA_CHANNEL" || channel_name == "BINARY_DATA_CHANNEL" || channel_name == "FILE_DATA_CHANNEL" {
                            static CLIPBOARD_SHAPES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
                            static TOOL_SHAPES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
                            let counter = if channel_name == "TEXT_DATA_CHANNEL" { &CLIPBOARD_SHAPES } else { &TOOL_SHAPES };
                            if counter.fetch_add(1,std::sync::atomic::Ordering::Relaxed) < 40 {
                                let shape = if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&msg.data) {
                                    fn shape(v:&serde_json::Value,depth:usize)->serde_json::Value {
                                        use serde_json::{Value,json};
                                        match v {
                                            Value::Object(o) if depth<2 => Value::Object(o.iter().take(24).filter(|(k,_)|k.len()<=48&&k.bytes().all(|b|b.is_ascii_alphanumeric()||b==b'_')).map(|(k,v)|(k.clone(),shape(v,depth+1))).collect()),
                                            Value::String(_)=>json!("string"), Value::Number(_)=>json!("number"), Value::Bool(_)=>json!("bool"),
                                            Value::Array(_)=>json!("array"),Value::Object(_)=>json!("object"),Value::Null=>json!("null"),
                                        }
                                    }
                                    shape(&value,0).to_string()
                                } else if let Ok(fields) = crate::protocol::decode_wire(&msg.data) {
                                    format!("protobuf fields={:?}",fields.iter().map(|f|f.number).collect::<Vec<_>>())
                                } else { "opaque".to_string() };
                                log::info!("Clipboard channel schema: channel={} bytes={} schema={}",channel_name,msg.data.len(),shape);
                            }
                        }
                        if channel_name == "CONTROL_DATA_CHANNEL" || channel_name == "TEXT_DATA_CHANNEL" {
                            if let Ok(fields) = crate::protocol::decode_wire(&msg.data) {
                                use crate::protocol::WireValue;
                                for field in &fields {
                                    if let (22,WireValue::Bytes(payload))=(field.number,&field.value) {
                                        let update=rich_incoming.lock().unwrap().response(payload);
                                        match update {
                                            Ok(Some(text))=>{if let Some(sink)=clipboard_sink.lock().unwrap().as_ref(){log::info!("Native desktop clipboard text received: bytes={}",text.len());sink.send_replace(Some(text));}},
                                            Err(reason)=>log::warn!("Clipboard desktop response rejected: {}",reason),
                                            _=>{},
                                        }
                                        if let Ok(response)=crate::protocol::decode_wire(payload) {
                                            if let Some(WireValue::Bytes(status))=response.iter().find(|f|f.number==6).map(|f|&f.value) {
                                                if let Ok(status)=crate::protocol::decode_wire(status) {
                                                    let success=status.iter().any(|f|f.number==1&&f.value==WireValue::Varint(1));
                                                    log::info!("Native clipboard RPC text acknowledged: success={}",success);
                                                }
                                            }
                                        }
                                    }
                                }
                                for field in &fields {
                                    if let (21, WireValue::Bytes(payload)) = (field.number,&field.value) {
                                        let ids=crate::clipboard_rich::format_ids(payload);
                                        if !ids.is_empty(){log::info!("Clipboard offered format IDs: {:?}",ids);}
                                        let rich_result={
                                            let mut next=||out_sequence.fetch_add(1,std::sync::atomic::Ordering::Relaxed);
                                            rich_incoming.lock().unwrap().request(payload,&mut next)
                                        };
                                        match rich_result {
                                            Ok(result)=>{
                                                if let Some(text)=result.text {if let Some(sink)=clipboard_sink.lock().unwrap().as_ref(){log::info!("Native desktop clipboard text received: bytes={}",text.len());sink.send_replace(Some(text));}}
                                                let count=result.messages.len();
                                                for (kind,payload) in result.messages {
                                                    let timestamp=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                                                    let message=crate::initialization::envelope(out_sequence.fetch_add(1,std::sync::atomic::Ordering::Relaxed),timestamp,kind,&payload);
                                                    if !matches!(tokio::time::timeout(std::time::Duration::from_secs(2),reply_chan.send(&bytes::Bytes::from(message))).await,Ok(Ok(_))){log::warn!("Clipboard desktop reply failed");break;}
                                                }
                                                if count>0{log::info!("Clipboard desktop protocol replies: count={}",count);}
                                            }
                                            Err(reason)=>log::warn!("Clipboard desktop request rejected: {}",reason),
                                        }
                                        let received=crate::clipboard_protocol::read_text_request(payload);
                                        match received {
                                            Ok(Some((header,text))) => {
                                                let sink=clipboard_sink.lock().unwrap().clone();
                                                if let Some(sink)=sink {
                                                    log::info!("Native clipboard RPC text received: bytes={}",text.len());
                                                    sink.send_replace(Some(text));
                                                    let payload=crate::clipboard_protocol::text_response(&header);
                                                    let timestamp=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                                                    let message=crate::initialization::envelope(out_sequence.fetch_add(1,std::sync::atomic::Ordering::Relaxed),timestamp,22,&payload);
                                                    let _=reply_chan.send(&bytes::Bytes::from(message)).await;
                                                }
                                            }
                                            Ok(None)=> {if let Ok(nested)=crate::protocol::decode_wire(payload){log::info!("Clipboard RPC request schema: fields={:?}",nested.iter().map(|f|f.number).collect::<Vec<_>>());}},
                                            Err(reason)=>log::warn!("Clipboard RPC rejected: {}",reason),
                                        }
                                    }
                                }
                                for field in &fields {
                                    if let (17, WireValue::Bytes(payload)) = (field.number, &field.value) {
                                        match clipboard_assembly.lock().unwrap().accept(payload) {
                                            Ok(Some(text)) => {
                                                if let Some(sink) = clipboard_sink.lock().unwrap().as_ref() {
                                                    log::info!("Native clipboard text received: bytes={}",text.len());
                                                    sink.send_replace(Some(text));
                                                }
                                            }
                                            Ok(None) => {},
                                            Err(reason) => log::warn!("Native clipboard message rejected: {}",reason),
                                        }
                                    }
                                }
                                if fields.iter().any(|f|f.number==20) {
                                    static INPUT_SHAPE:std::sync::atomic::AtomicUsize=std::sync::atomic::AtomicUsize::new(0);
                                    if INPUT_SHAPE.fetch_add(1,std::sync::atomic::Ordering::Relaxed)<12 {
                                        let shape:Vec<(u32,&str,usize)>=fields.iter().map(|f|match &f.value {
                                            WireValue::Bytes(b)=>(f.number,"bytes",b.len()),
                                            _=>(f.number,"number",0)
                                        }).collect();
                                        log::info!("CONTROL protobuf input field shape: {:?}",shape);
                                    }
                                }

                                let seq = fields.iter().find_map(|f| match (&f.value, f.number) { (WireValue::Varint(v), 1) => Some(*v), _ => None });
                                let _timestamp = fields.iter().find_map(|f| match (&f.value, f.number) { (WireValue::Varint(v), 2) => Some(*v), _ => None });
                                let simple = fields.iter().find_map(|f| match (&f.value, f.number) { (WireValue::Bytes(v), 3) => Some(*v), _ => None });
                                // Diagnostics: which non-input CONTROL messages the viewer sends (numbers only, no contents).
                                if !fields.iter().any(|f| f.number == 20) {
                                    static SEEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
                                    if SEEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 300 {
                                        let numbers: Vec<u32> = fields.iter().map(|f| f.number).collect();
                                        let action = simple.and_then(|b| crate::protocol::decode_wire(b).ok()).and_then(|a| a.iter().find_map(|f| match (&f.value, f.number) { (WireValue::Varint(v), 1) => Some(*v), _ => None }));
                                        log::info!("CONTROL message: fields={:?} simple_action={:?}", numbers, action);
                                    }
                                }
                                // QuerySystemState { 1: SS_ID_SECURE_DESKTOP_STATE = 1 } asks for the lock state now.
                                let query = fields.iter().find_map(|f| match (&f.value, f.number) { (WireValue::Bytes(v), 16) => Some(*v), _ => None });
                                if let Some(query) = query {
                                    let query_fields = crate::protocol::decode_wire(query).ok();
                                    let id = query_fields.as_ref().and_then(|q| q.iter().find_map(|f| match (&f.value, f.number) { (WireValue::Varint(v), 1) => Some(*v), _ => None }));
                                    let shape = query_fields.as_ref().map(|fields| fields.iter().map(|field| match &field.value {
                                        WireValue::Bytes(bytes) => (field.number, "bytes", bytes.len()),
                                        _ => (field.number, "number", 0),
                                    }).collect::<Vec<_>>());
                                    log::info!("CONTROL system-state query: fields={:?} secure_desktop={}", shape, id == Some(1));
                                    if id == Some(1) {
                                        let locked = *lock_for_message.borrow();
                                        // Unknown (no logind session, e.g. Xvfb) stays unlocked, as before.
                                        crate::session_lock::send_state(&reply_chan, &out_sequence, locked == Some(true)).await;
                                    }
                                }
                                if let Some(simple) = simple {
                                    let seq = seq.unwrap_or(0);
                                    if let Ok(action_fields) = crate::protocol::decode_wire(simple) {
                                        let action = action_fields.iter().find_map(|f| match (&f.value, f.number) { (WireValue::Varint(v), 1) => Some(*v), _ => None }).unwrap_or(0);
                                        if action == 15 {
                                            // AUTO_UNLOCK_SYSTEM: args JSON {"key": saved password}; never logged.
                                            let args = action_fields.iter().find_map(|f| match (&f.value, f.number) { (WireValue::Bytes(v), 2) => Some(*v), _ => None }).unwrap_or(&[]);
                                            let password = crate::session_lock::password_from_args(args);
                                            let (chan, seq_out) = (reply_chan.clone(), out_sequence.clone());
                                            tokio::spawn(crate::session_lock::handle_auto_unlock(password, chan, seq_out));
                                        }
                                        use crate::system_actions as sa;
                                        if matches!(action, sa::SHOW_DESKTOP | sa::REBOOT_SYSTEM | sa::LOCK_SYSTEM | sa::SHUTDOWN_SYSTEM | sa::SHOW_TASKMGR
                                            | sa::MUTE_SYSTEM | sa::MUTE_RESTORE | sa::MUTE_PERMANENT | sa::MUTE_TEMPORARY | sa::SET_AUTO_LOCK) {
                                            let value = action_fields.iter().find_map(|f| match (&f.value, f.number) { (WireValue::Varint(v), 5) => Some(*v), _ => None });
                                            let args = action_fields.iter().find_map(|f| match (&f.value, f.number) { (WireValue::Bytes(v), 2) => Some(v.to_vec()), _ => None }).unwrap_or_default();
                                            log::info!("SimpleAction {} ({}) received: value={:?} args={}", action,
                                                crate::control_protocol::simple_action_name(action).unwrap_or("?"), value, sa::args_shape(&args));
                                            let (actions, input, chan, seq_out) = (actions.clone(), input_worker.clone(), reply_chan.clone(), out_sequence.clone());
                                            let settings_portal = portal_for_message.clone();
                                            tokio::spawn(async move {
                                                let worker = actions.clone();
                                                let report = tokio::task::spawn_blocking(move || worker.handle(action, value, &args, || {
                                                    // GNOME/KDE "show desktop" shortcut (Super+D) through the desktop's own input path.
                                                    input.windows_key_event(0x5B, true); input.windows_key_event(0x44, true);
                                                    input.windows_key_event(0x44, false); input.windows_key_event(0x5B, false);
                                                })).await.unwrap_or(false);
                                                if report {
                                                    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                                                    let permission = settings_portal.as_ref().map_or(
                                                        crate::system_metrics::Permission::Allow,
                                                        |manager| manager.snapshot().permission(desktop));
                                                    let message = crate::initialization::envelope(seq_out.fetch_add(1, std::sync::atomic::Ordering::Relaxed), ts, 13, &actions.metrics_payload(desktop_sound, permission));
                                                    if let Err(e) = chan.send(&bytes::Bytes::from(message)).await { log::warn!("Settings report failed: {}", e); }
                                                }
                                            });
                                        } else if action == sa::CAPTURE_DESKTOP && channel_name == "CONTROL_DATA_CHANNEL" {
                                            if message_role.is_desktop() && !message_role.stream_ended() {
                                                log::info!("SimpleAction 4 (capture_desktop) received on a desktop connection");
                                            } else if let Some(request) = message_role.request.get() {
                                                if message_role.is_desktop() {
                                                    log::info!("SimpleAction 4 (capture_desktop) received on a tool connection whose desktop video ended; restarting it");
                                                } else {
                                                    log::info!("SimpleAction 4 (capture_desktop) received on a tool connection; starting the desktop");
                                                }
                                                request();
                                            } else {
                                                log::info!("SimpleAction 4 (capture_desktop) not handled: tool connection without a desktop route");
                                            }
                                        } else if action > 1 && action != 15 {
                                            let args = action_fields.iter().find_map(|f| match (&f.value, f.number) { (WireValue::Bytes(v), 2) => Some(*v), _ => None }).unwrap_or(&[]);
                                            log::info!("SimpleAction {} ({}) not handled: args={}", action, crate::control_protocol::simple_action_name(action).unwrap_or("?"), sa::args_shape(args));
                                        }
                                        if action == 0 {
                                            echo_for_message.store(seq, std::sync::atomic::Ordering::Release);
                                            echo_seen_for_message.store(true, std::sync::atomic::Ordering::Release);
                                            let response = crate::control_protocol::encode_echo_response_with_clipboard(
                                                out_sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                                                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs(), seq,
                                                clipboard_ready.load(std::sync::atomic::Ordering::Relaxed));
                                            if let Err(e) = reply_chan.send(&bytes::Bytes::from(response)).await {
                                                log::error!("CONTROL echo response failed: {}", e);
                                            } else {
                                                log::info!("CONTROL protobuf echo request answered");
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if let Ok(txt) = std::str::from_utf8(&msg.data) {
                            log::debug!("DataChannel [{}] received UTF-8 data: {} bytes", channel_name, txt.len());
                            if let Ok(val) = serde_json::from_str::<serde_json::Value>(txt) {
                                if !channel_name.contains("CONTROL") {
                                    // Input JSON on another channel (mobile input-method text was missing from CONTROL, #13/#46):
                                    // record action names only, once per name, and accept text_input wherever it arrives.
                                    if let Some(action) = val.get("action").and_then(serde_json::Value::as_str) {
                                        static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
                                        let mut seen = SEEN.lock().unwrap();
                                        if seen.len() < 32 && action.len() <= 40 && action.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') && !seen.iter().any(|s| s == action) {
                                            seen.push(action.to_owned());
                                            log::info!("JSON action on {} channel: action={}", if channel_name.contains("STREAMER") {"STREAMER"} else if channel_name.contains("TEXT") {"TEXT"} else {"other"}, action);
                                        }
                                    }
                                    if let Ok(crate::control_protocol::ControlMessage::Input(crate::control_protocol::ControlInput::TextInput {text})) = crate::control_protocol::decode_control(&msg.data) {
                                        if let Some(session)=message_input_session.as_ref() { session.with_input(|| {if crate::wayland::input_ready() {input_worker.text_input(&text);}}); }
                                    }
                                }
                                if channel_name.contains("CONTROL") {
                                    match crate::control_protocol::decode_control(&msg.data) {
                                        Ok(crate::control_protocol::ControlMessage::Input(event)) => {
                                            if screen.backend == crate::desktop_backend::DesktopBackend::Portal
                                                && (screen.init_error.is_some() || !desktop_ready || *lock_for_message.borrow() != Some(false)) { return; }
                                            if !crate::wayland::input_ready() {return;}
                                            use crate::control_protocol::ControlInput;
                                            // Separate receipt budgets keep motion floods from hiding clicks.
                                            static RECEIPTS: [std::sync::atomic::AtomicUsize; 3] = [const { std::sync::atomic::AtomicUsize::new(0) }; 3];
                                            let (slot, kind) = match &event {
                                                ControlInput::MouseButton {..} | ControlInput::MouseClick {..} => (0, "button"),
                                                ControlInput::Keyboard {..} | ControlInput::KeyboardClick {..} | ControlInput::TextInput {..} => (1, "keyboard"),
                                                _ => (2, "motion"),
                                            };
                                            if RECEIPTS[slot].fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 16 {
                                                log::info!("CONTROL input received: kind={}", kind);
                                            }
                                            if let Some(session)=message_input_session.as_ref() {session.with_input(|| {
                                            if !crate::wayland::input_ready() {return;}
                                            match &event {
                                                ControlInput::MouseAbsolute {x,y} => input_worker.mouse_absolute_normalized(*x,*y,crate::wayland::input_size(if live_x11 {crate::capture::x11_last_size().unwrap_or(input_size)} else {input_size})),
                                                ControlInput::MouseRelative {dx,dy} => input_worker.mouse_relative(*dx,*dy),
                                                ControlInput::MouseScroll {dx,dy} => input_worker.mouse_scroll(*dx,*dy),
                                                ControlInput::Keyboard {key, pressed} => input_worker.windows_key_event(*key, *pressed),
                                                ControlInput::TextInput {text} => input_worker.text_input(text),
                                                ControlInput::KeyboardClick {key} => {input_worker.windows_key_event(*key, true);input_worker.windows_key_event(*key, false);},
                                                ControlInput::MouseButton {button,pressed} => input_worker.wire_mouse_button(*button,*pressed),
                                                ControlInput::MouseClick {button} => {input_worker.wire_mouse_button(*button,true);input_worker.wire_mouse_button(*button,false);},
                                                _ => {}
                                            }
                                            });}

                                        },
                                        Ok(crate::control_protocol::ControlMessage::Unsupported(v)) => {
                                            // Action name and key names only, never values (an IME text event would carry typed text).
                                            static UNSUPPORTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
                                            if UNSUPPORTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 16 {
                                                let action = v.get("action").and_then(serde_json::Value::as_str)
                                                    .filter(|a| a.len() <= 40 && a.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')).unwrap_or("?");
                                                let keys: Vec<&str> = v.as_object().map(|o| o.keys().map(String::as_str).take(12).collect()).unwrap_or_default();
                                                log::info!("Unsupported CONTROL input: action={action} keys={keys:?}");
                                            }
                                        },
                                        _ => {}
                                    }
                                } else if channel_name.contains("STREAMER") {
                                    log::debug!("STREAMER channel JSON received");
                                    if let Some(event)=val.get("connection_period_stats_event") {
                                        let nums=crate::viewer_metrics::collect(event);
                                        if nums.is_empty(){
                                            static SHAPE:std::sync::atomic::AtomicUsize=std::sync::atomic::AtomicUsize::new(0);
                                            if SHAPE.fetch_add(1,std::sync::atomic::Ordering::Relaxed)<3 {
                                                log::info!("Viewer telemetry schema: {:?}",crate::viewer_metrics::safe_shape(event));
                                            }
                                        }
                                        log::info!("Viewer delay/frame numeric metrics: {:?}",nums);
                                    }

                                    // 检查是否有 ping 或握手请求并回应
                                    if val.get("type").and_then(|t| t.as_str()) == Some("ping") {
                                        let _ = reply_chan.send(&bytes::Bytes::from_static(b"{\"type\":\"pong\"}")).await;
                                    }
                                } else if channel_name.contains("TEXT") {
                                    log::debug!("TEXT channel JSON received");
                                }
                            }
                        } else {
                            log::debug!("DataChannel [{}] received binary data: len={}", channel_name, msg.data.len());


                        }

                    })
                }));

            })
        }));

        Ok(Self {
            pc,
            capture,
            input,
            desktop,
            role,
            controller: crate::input::Controller::Other,
            context,
            actions: session_actions,
        })
    }

    /// Route for capture_desktop on this tool peer; the caller decides whether it may become the desktop.
    pub fn on_desktop_request(&self, request: impl Fn() + Send + Sync + 'static) {
        let _ = self.role.request.set(Box::new(request));
    }

    /// Makes this tool peer the desktop: it takes the input lease, starts the video it negotiated
    /// (H.264; audio was never negotiated) and repeats the desktop CONTROL initialization.
    /// `screen` replaces a capture that was unavailable when the peer was offered.
    /// A promoted peer whose video ended keeps its input lease and only restarts the video.
    pub fn promote(&self, screen: Option<Arc<ScreenCapture>>) {
        let restart = self.role.is_desktop();
        if restart && !self.role.stream_ended() { return; }
        if let Some(screen) = screen { *self.role.screen.lock().unwrap() = Some(screen); }
        if !restart {
            let session = self.input.desktop_session();
            if self.pc.connection_state() == RTCPeerConnectionState::Connected { session.connected(); }
            *self.role.input.lock().unwrap() = Some(session);
            self.role.active.send_replace(true);
        }
        let stream = self.role.stream.lock().unwrap().clone();
        let task = stream.and_then(|stream| stream.spawn(self.role.screen.lock().unwrap().clone(), self.role.active.subscribe()));
        if let Some(old) = std::mem::replace(&mut *self.role.stream_task.lock().unwrap(), task) { old.abort(); }
        if restart { log::info!("Desktop video restarted on a tool connection after its stream ended"); return; }
        if !cfg!(test) { track_session(self.pc.clone(), self.actions.clone(), &self.role); }
        log::info!("Desktop started on an existing tool connection");
    }

    /// The capture this peer's desktop uses: its own, or the one reinitialized at promotion.
    pub fn desktop_capture(&self) -> Arc<ScreenCapture> { self.role.screen_or(&self.capture) }

    /// A newer desktop took over a promoted tool peer: stop its video, input and session count,
    /// keep the connection's tools. capture_desktop on it promotes it again.
    pub fn demote(&self) {
        if self.desktop || !self.role.is_desktop() { return; }
        self.role.active.send_replace(false);
        if let Some(task) = self.role.stream_task.lock().unwrap().take() { task.abort(); }
        if let Some(session) = self.role.input.lock().unwrap().take() { session.disconnected(); }
        log::info!("Desktop on a tool connection stopped: a newer desktop took over");
    }

    /// 应用远端 Offer，生成并返回 Answer
    pub async fn handle_offer(&self, soac: &SoacMessage) -> Result<String, String> {
        // 同步当前的客户端与房间会话上下文
        {
            let mut ctx = self.context.lock().await;
            ctx.client_id = soac.client_id.clone();
            ctx.ice_id = soac.ice_id.clone();
            ctx.app_control_id = soac.app_control_id.clone();
            log::info!("Updated WebRTC session context: client_id_present={} ice_id_present={}", ctx.client_id.is_some(), ctx.ice_id.is_some());
        }
        let mut sdp_str = soac.sdp.clone().unwrap_or_default();
        if sdp_str.is_empty() {
            if let Some(gzip_sdp) = &soac.gzip_sdp {
                if let Ok(compressed) = base64::engine::general_purpose::STANDARD.decode(gzip_sdp) {
                    let mut decoder = GzDecoder::new(&compressed[..]);
                    let mut decompressed = String::new();
                    if decoder.read_to_string(&mut decompressed).is_ok() {
                        sdp_str = decompressed;
                    }
                }
            }
        }

        if sdp_str.is_empty() {
            return Err("Offer SDP is empty".into());
        }

        let quic_value = sdp_str.lines().find_map(|line| line.strip_prefix("a=x-uuremote-quic"));
        let quic_summary = match quic_value.map(str::trim) {
            None => "absent", Some("") => "present", Some(":0") => "0", Some(":1") => "1",
            Some(":false") => "false", Some(":true") => "true", _ => "present non-boolean value (not logged)",
        };
        log::info!("Offer x-uuremote-quic: {}", quic_summary);
        log::info!("Offer includes UU capture-index extension: {}", sdp_str.lines().any(|l| l.starts_with("a=extmap:") && l.contains(crate::rtp_metadata::CAPTURE_INDEX_URI)));
        let mut codecs: Vec<&str> = sdp_str.lines().filter_map(|l| l.strip_prefix("a=rtpmap:")).filter_map(|l| l.split_whitespace().nth(1))
            .filter_map(|c| c.split('/').next()).collect();
        codecs.dedup();
        log::info!("Offer codecs: {:?}", codecs);
        // SDP structure only: attribute names per section and public extmap URIs; never ufrag,
        // password, fingerprint, candidate or fmtp values.
        let mut sections: Vec<String> = Vec::new();
        let mut current = String::from("session");
        let mut names: std::collections::BTreeSet<String> = Default::default();
        let mut flush = |current: &str, names: &mut std::collections::BTreeSet<String>, sections: &mut Vec<String>| {
            sections.push(format!("{current}[{}]", names.iter().cloned().collect::<Vec<_>>().join(",")));
            names.clear();
        };
        for line in sdp_str.lines() {
            if let Some(m) = line.strip_prefix("m=") {
                flush(&current, &mut names, &mut sections);
                let mut parts = m.split_whitespace();
                let kind = parts.next().unwrap_or("?");
                current = format!("{kind}/{}pt", parts.skip(2).count());
            } else if let Some(a) = line.strip_prefix("a=") {
                let name = a.split([':', ' ']).next().unwrap_or("");
                if name == "extmap" { if let Some(uri) = a.split_whitespace().nth(1) { names.insert(format!("extmap={}", uri.rsplit('/').next().unwrap_or(uri))); } }
                else if !name.is_empty() && name.len() <= 32 { names.insert(name.to_owned()); }
            }
        }
        flush(&current, &mut names, &mut sections);
        log::info!("Offer SDP structure: {}", sections.join(" | "));
        let offer = RTCSessionDescription::offer(sdp_str)
            .map_err(|e| format!("Invalid offer: {}", e))?;

        self.pc
            .set_remote_description(offer)
            .await
            .map_err(|e| format!("Set remote desc failed: {}", e))?;

        let answer = self
            .pc
            .create_answer(None)
            .await
            .map_err(|e| format!("Create answer failed: {}", e))?;

        log::info!("Answer includes x-uuremote-quic: {}", answer.sdp.lines().any(|l| l == "a=x-uuremote-quic"));
        log::info!("Answer media sections: {:?}", answer.sdp.lines().filter(|l| l.starts_with("m=")).collect::<Vec<_>>());
        let bundle_media = negotiated_bundle_media(&answer.sdp)
            .ok_or("Answer has no active BUNDLE transport")?;
        self.context.lock().await.bundle_media = Some(bundle_media);
        self.pc
            .set_local_description(answer.clone())
            .await
            .map_err(|e| format!("Set local desc failed: {}", e))?;
        // webrtc-rs omits a=max-message-size, so the viewer (libwebrtc) assumes 64 KB and refuses
        // larger data-channel messages (port-mapping uploads stalled after the first frame).
        // Only the copy sent to the viewer is amended: webrtc-rs rejects a local description that
        // differs from the answer it generated. Our receive buffer (1 MB) accepts 256 KB messages.
        Ok(with_max_message_size(&answer.sdp))
    }

    /// 接收远端 ICE Candidate
    pub async fn add_candidate(&self, soac: &SoacMessage) -> Result<(), String> {
        if let Some(cand_val) = &soac.candidate {
            let (cand_str, sdp_mid, sdp_mline) = if let Some(s) = cand_val.as_str() {
                (s.to_string(), soac.sdp_mid.clone().or_else(|| Some("0".to_string())), soac.sdp_mline_index.map(|i| i as u16).or(Some(0)))
            } else {
                let cs = cand_val.get("candidate").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let mid = cand_val.get("sdpMid").and_then(|v| v.as_str()).map(|s| s.to_string())
                    .or_else(|| soac.sdp_mid.clone())
                    .or_else(|| Some("0".to_string()));
                let mline = cand_val.get("sdpMLineIndex").and_then(|v| v.as_u64()).map(|i| i as u16)
                    .or_else(|| soac.sdp_mline_index.map(|i| i as u16))
                    .or(Some(0));
                (cs, mid, mline)
            };

            if !cand_str.is_empty() {
                let init = RTCIceCandidateInit {
                    candidate: cand_str.clone(),
                    sdp_mid,
                    sdp_mline_index: sdp_mline,
                    ..Default::default()
                };

                let summary=crate::network_diagnostics::candidate_summary(&cand_str);
                log::info!("Injecting Remote ICE Candidate: protocol={} type={}",summary.protocol,summary.kind);

                self.pc
                    .add_ice_candidate(init)
                    .await
                    .map_err(|e| format!("Add candidate error: {}", e))?;
            }
        }
        Ok(())
    }
}


/// Max SCTP message size we accept (libwebrtc's default, as the official host advertises).
const MAX_MESSAGE_SIZE: usize = 262_144;

/// Adds `a=max-message-size` after `a=sctp-port` in the data-channel section if missing.
fn with_max_message_size(sdp: &str) -> String {
    if sdp.contains("a=max-message-size:") { return sdp.to_owned(); }
    let eol = if sdp.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(sdp.len() + 32);
    for line in sdp.split_inclusive('\n') {
        out.push_str(line);
        if line.trim_end().starts_with("a=sctp-port:") {
            out.push_str(&format!("a=max-message-size:{MAX_MESSAGE_SIZE}{eol}"));
        }
    }
    out
}

fn negotiated_bundle_media(sdp: &str) -> Option<(String, u16)> {
    let preferred: Vec<&str> = sdp.lines().find_map(|l| l.strip_prefix("a=group:BUNDLE "))?
        .split_whitespace().collect();
    let mut sections: Vec<(bool, Option<String>)> = Vec::new();
    for line in sdp.lines() {
        if let Some(media) = line.strip_prefix("m=") {
            let port = media.split_whitespace().nth(1)?;
            sections.push((port != "0", None));
        } else if let Some(mid) = line.strip_prefix("a=mid:") {
            if let Some(section) = sections.last_mut() { section.1 = Some(mid.to_string()); }
        }
    }
    for mid in preferred {
        for (index, (active, section_mid)) in sections.iter().enumerate() {
            if *active && section_mid.as_deref() == Some(mid) {
                return Some((mid.to_string(), index.try_into().ok()?));
            }
        }
    }
    None
}

/// First drain owns the outbound receiver; a later open must not start another.
async fn take_outbound_once<T>(slot: &tokio::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<T>>>) -> Option<tokio::sync::mpsc::UnboundedReceiver<T>> {
    slot.lock().await.take()
}

#[cfg(test)]
mod tests {
    use super::negotiated_bundle_media;
    #[test]
    fn advertises_max_message_size_once_in_the_data_channel_section() {
        use super::with_max_message_size;
        let sdp = "v=0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=sctp-port:5000\r\n";
        let out = with_max_message_size(sdp);
        assert!(out.contains("a=sctp-port:5000\r\na=max-message-size:262144\r\n"));
        assert_eq!(with_max_message_size(&out), out);
    }

    #[test]
    fn skips_rejected_audio_for_ice_transport() {
        let sdp = "a=group:BUNDLE 1 2\r\nm=audio 0 RTP 0\r\na=mid:0\r\nm=video 9 RTP 98\r\na=mid:1\r\nm=application 9 SCTP 0\r\na=mid:2\r\n";
        assert_eq!(negotiated_bundle_media(sdp), Some(("1".into(), 1)));
    }
    #[test]
    fn follows_bundle_order_not_first_media() {
        let sdp = "a=group:BUNDLE data video\nm=video 9 RTP 98\na=mid:video\nm=application 9 SCTP 0\na=mid:data\n";
        assert_eq!(negotiated_bundle_media(sdp), Some(("data".into(), 1)));
    }
    #[tokio::test]
    async fn outbound_receiver_is_taken_once() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let slot = std::sync::Arc::new(tokio::sync::Mutex::new(Some(rx)));
        let first = super::take_outbound_once(&slot).await;
        let second = super::take_outbound_once(&slot).await;
        assert!(first.is_some());
        assert!(second.is_none());
        drop(tx);
    }
}

#[cfg(test)]
#[path = "desktop_control_tests.rs"]
mod desktop_control_tests;

#[cfg(test)]
#[path = "webrtc_lifecycle_tests.rs"]
mod webrtc_lifecycle_tests;

/// Virtual/overlay interfaces never carry the UU video path (ZeroTier, containers).
fn ice_interface_allowed(name: &str) -> bool {
    !["zt", "docker", "br-", "veth", "virbr", "lo"].iter().any(|prefix| name.starts_with(prefix))
}

/// Viewer feedback seen on the video sender: retransmission and keyframe requests.
#[derive(Default)]
pub(crate) struct FeedbackCounts { pub nack_packets: u64, pub pli: u64, pub fir: u64 }
impl FeedbackCounts {
    pub(crate) fn observe(&mut self, packet: &dyn std::any::Any) {
        use webrtc::rtcp::{payload_feedbacks::{full_intra_request::FullIntraRequest, picture_loss_indication::PictureLossIndication}, transport_feedbacks::transport_layer_nack::TransportLayerNack};
        if let Some(nack) = packet.downcast_ref::<TransportLayerNack>() {
            self.nack_packets += nack.nacks.iter().map(|pair| 1 + u64::from(pair.lost_packets.count_ones())).sum::<u64>();
        } else if packet.is::<PictureLossIndication>() { self.pli += 1; }
        else if packet.is::<FullIntraRequest>() { self.fir += 1; }
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;
    #[test]
    fn ice_skips_overlay_and_container_interfaces_only() {
        for name in ["ztfp6jdvog", "docker0", "br-1a2b", "veth12", "virbr0", "lo"] { assert!(!ice_interface_allowed(name), "{name}"); }
        for name in ["eno1", "enp5s0", "wlp3s0", "eth0"] { assert!(ice_interface_allowed(name), "{name}"); }
    }
    #[test]
    fn feedback_counts_nacked_packets_and_keyframe_requests() {
        use webrtc::rtcp::{payload_feedbacks::picture_loss_indication::PictureLossIndication, transport_feedbacks::transport_layer_nack::{NackPair, TransportLayerNack}};
        let mut counts = FeedbackCounts::default();
        counts.observe(&TransportLayerNack { nacks: vec![NackPair { packet_id: 10, lost_packets: 0b101 }], ..Default::default() });
        counts.observe(&PictureLossIndication::default());
        assert_eq!((counts.nack_packets, counts.pli, counts.fir), (3, 1, 0));
    }
}
