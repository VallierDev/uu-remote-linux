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

#[derive(Debug, Default, Clone)]
pub struct SessionContext {
    pub client_id: Option<String>,
    pub ice_id: Option<serde_json::Value>,
    pub app_control_id: Option<serde_json::Value>,
    pub bundle_media: Option<(String, u16)>,
}

pub struct WebRtcSession {
    pub pc: Arc<RTCPeerConnection>,
    pub capture: Arc<ScreenCapture>,
    pub input: Arc<InputInjector>,
    pub desktop: bool,
    pub context: Arc<Mutex<SessionContext>>,
}

impl WebRtcSession {
    pub async fn new(
        capture: Arc<ScreenCapture>,
        input: Arc<InputInjector>,
        signaling_client: Arc<crate::signaling::SignalingClient>,
        desktop: bool,
        transport:crate::session_ice::SessionIce,
    ) -> Result<Self, String> {
        let mut m = MediaEngine::default();
        m.register_default_codecs()
            .map_err(|e| format!("Register standard codecs failed: {}", e))?;

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
        let connection_input=input.clone();
        let connected_signal = signaling_client.clone();
        let notified = Arc::new(std::sync::atomic::AtomicBool::new(false));
        pc.on_peer_connection_state_change(Box::new(move |state| {
            let ctx = connected_context.clone();
            let input=connection_input.clone();
            let sig = connected_signal.clone();
            let notified = notified.clone();
            Box::pin(async move {
                log::info!("PeerConnection State changed: {:?}", state);
                if matches!(state,RTCPeerConnectionState::Closed|RTCPeerConnectionState::Failed|RTCPeerConnectionState::Disconnected) {input.release_held();}
                if state == RTCPeerConnectionState::Connected && !notified.swap(true, std::sync::atomic::Ordering::AcqRel) {
                    tokio::spawn(async move {
                    let ctx = ctx.lock().await.clone();
                    if let (Some(client), Some(ice)) = (&ctx.client_id, ctx.ice_id.as_ref().and_then(|v| v.as_str())) {
                        let payload = crate::capability::h264_device_capability(client, ice, 0, crate::video::FPS as i32, 5);
                        if let Err(e) = sig.emit_event(crate::capability::FORWARD_SETTING_EVENT, payload).await {
                            log::error!("Device capability notification failed: {}", e);
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
        let video_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: "video/H264".to_owned(),
                ..Default::default()
            },
            "video_0".to_owned(),
            "video_0".to_owned(),
        ));

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
        if desktop && crate::audio::enabled() {
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
        let desktop_sound = desktop && crate::audio::enabled();
        if desktop {
            let (end_pc, actions) = (pc.clone(), peer_actions.clone());
            tokio::spawn(async move {
                {
                    use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState as State;
                    // Count the session once it is actually connected (a failed offer is not a session).
                    while end_pc.connection_state() != State::Connected {
                        if matches!(end_pc.connection_state(), State::Closed | State::Failed) { return; }
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                    let starting = actions.clone();
                    let _ = tokio::task::spawn_blocking(move || starting.session_started()).await;
                }
                use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState as State;
                while !matches!(end_pc.connection_state(), State::Closed | State::Failed) {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
                drop(end_pc);
                let _ = tokio::task::spawn_blocking(move || actions.session_ended()).await;
            });
        }
        let portal=capture.portal.clone();
        // File/terminal/port-mapping connections carry no desktop video.
        if desktop {
            tokio::spawn(async move {
                if let Err(e) = crate::video::stream_desktop(streaming_pc, video_track, capture_size, capture_display, portal, video_settings_rx, idr_request).await {
                    log::error!("Live desktop stream stopped: {}", e);
                }
            });
        } else {
            drop((streaming_pc, video_track, capture_size, capture_display, video_settings_rx));
        }
        let input_clone = input.clone();
        let control_capture = capture.clone();
        let control_sequence = Arc::new(std::sync::atomic::AtomicU64::new(1));
        // Capability belongs to the granted session, including while GNOME is
        // locked. Echo can precede TEXT readiness and CONTROL auto-unlock;
        // advertising false here would disable clipboard for the entire peer.
        let portal_clipboard_supported = desktop && capture.portal.as_ref().is_some_and(|session| session.clipboard_supported());
        let text_clipboard_ready = Arc::new(std::sync::atomic::AtomicBool::new(portal_clipboard_supported));
        // One file-transfer session per peer. The viewer may send FileTransferFtp RPCs on CONTROL,
        // but it receives file traffic only through streamerDidReceiveFileData, so every reply and
        // host-initiated request goes out on FILE_DATA_CHANNEL.
        let file_channel: Arc<std::sync::Mutex<Option<Arc<RTCDataChannel>>>> = Arc::new(std::sync::Mutex::new(None));
        let (file_tx, mut file_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let peer_files = crate::file_trans::FileSession::new(file_tx, control_sequence.clone());
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

        // 监听 DataChannel
        pc.on_data_channel(Box::new(move |d: Arc<RTCDataChannel>| {
            let label = d.label().to_string();
            log::info!("New DataChannel received: {}", label);

            let input_inst = input_clone.clone();
            let screen = control_capture.clone();
            let out_sequence = control_sequence.clone();
            let shared_clipboard_ready = text_clipboard_ready.clone();
            let peer_files = peer_files.clone();
            let peer_video_settings = video_settings.clone();
            let peer_actions = peer_actions.clone();
            if label == "FILE_DATA_CHANNEL" { *file_channel.lock().unwrap() = Some(d.clone()); }

            Box::pin(async move {
                let lbl = label.clone();
                let inp = input_inst.clone();
                let d_reply = d.clone();
                let input_size = (screen.width, screen.height);
                let reply_sequence = out_sequence.clone();
                let channel_video_settings = peer_video_settings.clone();
                let channel_actions = peer_actions.clone();
                let clipboard_input = Arc::new(std::sync::Mutex::new(None::<tokio::sync::watch::Sender<Option<String>>>));
                let clipboard_assembly = Arc::new(std::sync::Mutex::new(crate::clipboard_protocol::Assembly::default()));
                let rich_state=Arc::new(std::sync::Mutex::new(crate::clipboard_rich::State::default()));
                let clipboard_enabled = shared_clipboard_ready.clone();
                // Ordered outbound queue for protocol replies produced outside this callback.
                let (outbound, mut outbound_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
                {
                    let out_channel = d.clone();
                    let out_label = label.clone();
                    tokio::spawn(async move {
                        while let Some(message) = outbound_rx.recv().await {
                            if let Err(e) = out_channel.send(&bytes::Bytes::from(message)).await {
                                log::warn!("{} send failed: {}", out_label, e); break;
                            }
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
                    let rich_outgoing=rich_state.clone();
                    let open_actions = channel_actions.clone();
                    let open_video = channel_video_settings.clone();
                    d.on_open(Box::new(move || {
                        let init_actions = open_actions.clone();
                        let init_video = open_video.clone();
                        let channel = channel.clone();
                        let sequence = initial_sequence.clone();
                        let screen = screen.clone();
                        let clipboard_sink = clipboard_sink.clone();
                        let clipboard_ready = clipboard_ready.clone();
                        let rich_outgoing=rich_outgoing.clone();
                        Box::pin(async move {
                          if channel.label() == "TEXT_DATA_CHANNEL" {
                            if !desktop { return; }
                            let bridge = if let Some(session)=screen.portal.clone() { crate::clipboard::start_portal(session) } else { crate::clipboard::start(screen.display.clone()) };
                            *clipboard_sink.lock().unwrap() = Some(bridge.incoming);
                            let clipboard_channel = channel.clone();
                            let clipboard_sequence = sequence.clone();
                            let clipboard_cleanup = clipboard_sink.clone();
                            let ready = bridge.ready.await.unwrap_or(false);
                            clipboard_ready.store(ready,std::sync::atomic::Ordering::Relaxed);
                            tokio::spawn(async move {
                                if !ready {
                                    *clipboard_cleanup.lock().unwrap() = None;
                                    log::warn!("Native text clipboard unavailable"); return;
                                }
                                log::info!("Native text clipboard bridge ready");
                                let mut updates = bridge.outgoing;
                                let mut health = tokio::time::interval(std::time::Duration::from_millis(200));
                                loop {
                                    tokio::select! {
                                        _ = health.tick() => {
                                            if clipboard_channel.ready_state() != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open { break; }
                                        },
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
                            });
                            return;
                          }
                            let timestamp = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                            let next_seq = || sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            // Default file-transfer folder: the desktop user's home directory.
                            let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
                            let desktop_permission=if screen.init_error.is_none() {
                                crate::system_metrics::Permission::Allow
                            } else { crate::system_metrics::Permission::Reject };
                            let messages = [
                                (8, crate::initialization::encode_capture_change(next_seq(), timestamp, 0, 0, "Linux desktop")),
                                (13, Ok(crate::initialization::envelope(next_seq(), timestamp, 13, &init_actions.metrics_payload(desktop_sound, desktop_permission)))),
                                (26, Ok(crate::initialization::envelope(next_seq(), timestamp, 26,
                                    &crate::system_metrics::remote_download_path(&home)))),
                                (7, crate::capture_setting::screen_sources(next_seq(), timestamp, (screen.width, screen.height), init_video.borrow().output, "Linux desktop")),
                            ];
                            for (field, message) in messages {
                                if screen.init_error.is_some() && matches!(field,7|8) { continue; }
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
                            // Mirror the Linux pointer shape; the video itself carries no pointer.
                            if !desktop { return; }
                            tokio::spawn(crate::session_lock::report_states(channel.clone(), sequence.clone()));
                            let mut shapes = crate::cursor::monitor(&screen.display);
                            let mut health = tokio::time::interval(std::time::Duration::from_millis(200));
                            let mut sent = 0usize;
                            loop {
                                let current = shapes.borrow_and_update().clone();
                                if let Some(cursor) = current {
                                    let timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                                    let message = crate::cursor::encode_cursor_shape(next_seq(), timestamp, &cursor);
                                    if channel.send(&bytes::Bytes::from(message)).await.is_err() { break; }
                                    sent += 1;
                                    if sent <= 3 || sent % 100 == 0 {
                                        log::info!("Cursor shape sent: count={} size={}x{} hotspot={},{} png_bytes={}", sent, cursor.width, cursor.height, cursor.hot_x, cursor.hot_y, cursor.png.len());
                                    }
                                }
                                loop {
                                    tokio::select! {
                                        changed = shapes.changed() => { if changed.is_err() { return; } break; },
                                        _ = health.tick() => {
                                            if channel.ready_state() != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open { return; }
                                        },
                                    }
                                }
                            }
                        })
                    }));
                }


                d.on_message(Box::new(move |msg: DataChannelMessage| {
                    let channel_name = lbl.clone();
                    let input_worker = inp.clone();
                    let terminal_worker = terminal_conn.clone();
                    let rpc_out = outbound.clone();
                    let reply_chan = d_reply.clone();
                    let out_sequence = reply_sequence.clone();
                    let clipboard_sink = clipboard_input.clone();
                    let clipboard_assembly = clipboard_assembly.clone();
                    let clipboard_ready = clipboard_enabled.clone();
                    let rich_incoming=rich_state.clone();
                    let file_worker = file_session.clone();
                    let video_settings = channel_video_settings.clone();
                    let actions = channel_actions.clone();

                    Box::pin(async move {
                        if let Some(term) = &terminal_worker {
                            if crate::terminal::is_frame(&msg.data) { term.handle(&msg.data); return; }
                        }
                        if let Ok(fields) = crate::protocol::decode_wire(&msg.data) {
                            use crate::protocol::WireValue;
                            let rpc = fields.iter().find_map(|f| match (f.number, &f.value) { (21, WireValue::Bytes(b)) => Some(*b), _ => None });
                            let rpc_fields = rpc.and_then(|r| crate::protocol::decode_wire(r).ok());
                            let output_before = video_settings.borrow().output;
                            if let Some(response) = rpc_fields.as_deref().and_then(|r| crate::capture_setting::handle_rpc(r, &video_settings, input_size)) {
                                let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                                let next = || out_sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                let _ = rpc_out.send(crate::initialization::envelope(next(), ts, 22, &response));
                                let current = *video_settings.borrow();
                                if current.output != output_before {
                                    match crate::capture_setting::screen_sources(next(), ts, input_size, current.output, "Linux desktop") {
                                        Ok(screens) => { let _ = rpc_out.send(screens); },
                                        Err(e) => log::warn!("Screen sources update failed: {}", e),
                                    }
                                }
                                let sync = crate::capture_setting::sync_payload(&current, input_size);
                                let _ = rpc_out.send(crate::initialization::envelope(next(), ts, 25, &sync));
                                return;
                            }
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
                                    let id = crate::protocol::decode_wire(query).ok().and_then(|q| q.iter().find_map(|f| match (&f.value, f.number) { (WireValue::Varint(v), 1) => Some(*v), _ => None }));
                                    if id == Some(1) {
                                        let locked = *crate::session_lock::watch_locked().borrow();
                                        crate::session_lock::send_state(&reply_chan, &out_sequence, locked.unwrap_or(false)).await;
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
                                            tokio::spawn(async move {
                                                let worker = actions.clone();
                                                let report = tokio::task::spawn_blocking(move || worker.handle(action, value, &args, || {
                                                    // GNOME/KDE "show desktop" shortcut (Super+D) through the desktop's own input path.
                                                    input.windows_key_event(0x5B, true); input.windows_key_event(0x44, true);
                                                    input.windows_key_event(0x44, false); input.windows_key_event(0x5B, false);
                                                })).await.unwrap_or(false);
                                                if report {
                                                    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                                                    let message = crate::initialization::envelope(seq_out.fetch_add(1, std::sync::atomic::Ordering::Relaxed), ts, 13, &actions.metrics_payload(desktop_sound, crate::system_metrics::Permission::Allow));
                                                    if let Err(e) = chan.send(&bytes::Bytes::from(message)).await { log::warn!("Settings report failed: {}", e); }
                                                }
                                            });
                                        } else if action > 1 && action != 15 {
                                            let args = action_fields.iter().find_map(|f| match (&f.value, f.number) { (WireValue::Bytes(v), 2) => Some(*v), _ => None }).unwrap_or(&[]);
                                            log::info!("SimpleAction {} ({}) not handled: args={}", action, crate::control_protocol::simple_action_name(action).unwrap_or("?"), sa::args_shape(args));
                                        }
                                        if action == 0 {
                                            let response = crate::control_protocol::encode_echo_response_with_clipboard(
                                                out_sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                                                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs(), seq,
                                                clipboard_ready.load(std::sync::atomic::Ordering::Relaxed));
                                            if let Err(e) = reply_chan.send(&bytes::Bytes::from(response)).await {
                                                log::error!("CONTROL echo response failed: {}", e);
                                            } else {
                                                static RESPONSES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
                                                if RESPONSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0 {
                                                    log::info!("CONTROL protobuf echo request answered");
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if let Ok(txt) = std::str::from_utf8(&msg.data) {
                            log::debug!("DataChannel [{}] received UTF-8 data: {} bytes", channel_name, txt.len());
                            if let Ok(val) = serde_json::from_str::<serde_json::Value>(txt) {
                                if channel_name.contains("CONTROL") {
                                    match crate::control_protocol::decode_control(&msg.data) {
                                        Ok(crate::control_protocol::ControlMessage::Input(event)) => {
                                            use crate::control_protocol::ControlInput;
                                            // Separate receipt budgets keep motion floods from hiding clicks.
                                            static RECEIPTS: [std::sync::atomic::AtomicUsize; 3] = [const { std::sync::atomic::AtomicUsize::new(0) }; 3];
                                            let (slot, kind) = match &event {
                                                ControlInput::MouseButton {..} | ControlInput::MouseClick {..} => (0, "button"),
                                                ControlInput::Keyboard {..} | ControlInput::KeyboardClick {..} => (1, "keyboard"),
                                                _ => (2, "motion"),
                                            };
                                            if RECEIPTS[slot].fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 16 {
                                                log::info!("CONTROL input received: kind={}", kind);
                                            }
                                            match &event {
                                                ControlInput::MouseAbsolute {x,y} => input_worker.mouse_absolute_normalized(*x,*y,input_size),
                                                ControlInput::MouseRelative {dx,dy} => input_worker.mouse_relative(*dx,*dy),
                                                ControlInput::MouseScroll {dx,dy} => input_worker.mouse_scroll(*dx,*dy),
                                                ControlInput::Keyboard {key, pressed} => input_worker.windows_key_event(*key, *pressed),
                                                ControlInput::KeyboardClick {key} => {input_worker.windows_key_event(*key, true);input_worker.windows_key_event(*key, false);},
                                                ControlInput::MouseButton {button,pressed} => input_worker.wire_mouse_button(*button,*pressed),
                                                ControlInput::MouseClick {button} => {input_worker.wire_mouse_button(*button,true);input_worker.wire_mouse_button(*button,false);},
                                                _ => {}
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
            context,
        })
    }

    /// 应用远端 Offer，生成并返回 Answer
    pub async fn handle_offer(&self, soac: &SoacMessage) -> Result<String, String> {
        // 同步当前的客户端与房间会话上下文
        {
            let mut ctx = self.context.lock().await;
            ctx.client_id = soac.client_id.clone();
            ctx.ice_id = soac.ice_id.clone();
            ctx.app_control_id = soac.app_control_id.clone();
            log::info!("Updated WebRTC session context: client_id={:?}, ice_id={:?}", ctx.client_id, ctx.ice_id);
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
        Ok(answer.sdp)
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

#[cfg(test)]
mod tests {
    use super::negotiated_bundle_media;
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
}

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
