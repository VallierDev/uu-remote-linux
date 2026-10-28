//! Real local SCTP peers; no cloud, desktop consent, audio or power operations.
use super::*;
use crate::protocol::{decode_wire, WireValue};
use std::{collections::HashMap, time::Duration};
use tokio::sync::mpsc;

type Messages = mpsc::UnboundedReceiver<(String, Vec<u8>)>;
struct Viewer {
    pc: Arc<RTCPeerConnection>,
    channels: HashMap<String, Arc<RTCDataChannel>>,
    messages: Messages,
    router: Arc<Mutex<webrtc::util::vnet::router::Router>>,
}

async fn connect(input: Arc<InputInjector>, desktop: bool,
    portal: Option<Arc<crate::desktop_session::PortalDesktop>>, labels: &[&str]) -> (WebRtcSession, Viewer) {
    let (host_setting, viewer_setting, router) = crate::test_network::virtual_network().await;
    let screen = Arc::new(ScreenCapture::unavailable("synthetic locked desktop".into()));
    let host = WebRtcSession::new(screen, input, crate::signaling::SignalingClient::disconnected(),
        desktop, crate::session_ice::SessionIce::virtual_net(host_setting), portal).await.unwrap();
    let pc = Arc::new(APIBuilder::new().with_setting_engine(viewer_setting).build().new_peer_connection(RTCConfiguration::default()).await.unwrap());
    let (tx, messages) = mpsc::unbounded_channel();
    let mut channels = HashMap::new();
    for label in labels {
        let channel = pc.create_data_channel(label, None).await.unwrap();
        let (tx, name) = (tx.clone(), label.to_string());
        channel.on_message(Box::new(move |message| {
            let _ = tx.send((name.clone(), message.data.to_vec()));
            Box::pin(async {})
        }));
        channels.insert(label.to_string(), channel);
    }
    let mut gathered = pc.gathering_complete_promise().await;
    pc.set_local_description(pc.create_offer(None).await.unwrap()).await.unwrap();
    gathered.recv().await;
    host.pc.set_remote_description(pc.local_description().await.unwrap()).await.unwrap();
    let mut gathered = host.pc.gathering_complete_promise().await;
    host.pc.set_local_description(host.pc.create_answer(None).await.unwrap()).await.unwrap();
    gathered.recv().await;
    pc.set_remote_description(host.pc.local_description().await.unwrap()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(8), async {
        while channels.values().any(|channel| channel.ready_state() != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("local SCTP channels did not open");
    (host, Viewer { pc, channels, messages, router })
}

fn body(message: &[u8], number: u32) -> Option<&[u8]> {
    decode_wire(message).ok()?.into_iter().find_map(|field| match (field.number, field.value) {
        (n, WireValue::Bytes(bytes)) if n == number => Some(bytes), _ => None,
    })
}
fn bytes(number: u32, value: &[u8]) -> Vec<u8> {
    fn varint(mut value: usize, out: &mut Vec<u8>) {
        while value >= 128 { out.push((value as u8 & 127) | 128); value >>= 7; }
        out.push(value as u8);
    }
    let mut out = Vec::new();
    varint(((number << 3) | 2) as usize, &mut out);
    varint(value.len(), &mut out);
    out.extend_from_slice(value);
    out
}
async fn receive(messages: &mut Messages, label: &str, field: u32) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (channel, message) = messages.recv().await.expect("peer stopped responding");
            if channel == label && body(&message, field).is_some() { return message; }
        }
    }).await.expect("expected protocol response timed out")
}
async fn send(viewer: &Viewer, label: &str, message: Vec<u8>) {
    viewer.channels[label].send(&message.into()).await.unwrap();
}
fn mapping(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![8, 1, 16, 2, 24, 3, 32, kind];
    frame.extend(bytes(5, payload));
    crate::initialization::envelope(1, 0, 27, &frame)
}

#[tokio::test]
async fn closing_unnegotiated_peers_releases_callback_state() {
    for desktop in [false, true] {
        let input = Arc::new(InputInjector::without_desktop());
        let capture = Arc::new(ScreenCapture::unavailable("synthetic desktop".into()));
        let peer = WebRtcSession::new(capture, input,
            crate::signaling::SignalingClient::disconnected(), desktop,
            crate::session_ice::SessionIce::local(), None).await.unwrap();
        let weak = Arc::downgrade(&peer.pc);
        peer.pc.close().await.unwrap();
        drop(peer);
        tokio::time::timeout(Duration::from_secs(4), async {
            while weak.upgrade().is_some() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("closed peer retained by callbacks");
    }
}

#[tokio::test]
async fn locked_control_unlock_requests_and_tools_work_without_a_video_frame() {
    tokio::time::timeout(Duration::from_secs(35), async {
        let input = Arc::new(InputInjector::without_desktop());
        let (lock, locked) = tokio::sync::watch::channel(Some(true));
        let portal = Arc::new(crate::desktop_session::PortalDesktop::with_lock_state(
            Arc::new(ScreenCapture::unavailable("synthetic locked desktop".into())), locked));
        let (desktop, mut viewer) = connect(input.clone(), true, Some(portal.clone()), &["CONTROL_DATA_CHANNEL"]).await;
        // Official Android 4.43.0's unlock presenter requires CaptureChange
        // (desktop/window) before it will use the secure state to send action15.
        let selected = receive(&mut viewer.messages, "CONTROL_DATA_CHANNEL", 8).await;
        let selected = decode_wire(body(&selected, 8).unwrap()).unwrap();
        let capture_type = selected.iter().find_map(|f| match (f.number, &f.value) {
            (1, WireValue::Varint(value)) => Some(*value), _ => None,
        }).unwrap_or(0);
        assert_eq!(capture_type, 0);
        let state = receive(&mut viewer.messages, "CONTROL_DATA_CHANNEL", 15).await;
        let secure = body(body(&state, 15).unwrap(), 1).unwrap();
        assert!(decode_wire(secure).unwrap().iter().any(|f| f.number == 1 && f.value == WireValue::Varint(1)));
        assert!(portal.snapshot().screen.init_error.is_some());
        // A capture request waiting for unlock must not occupy CONTROL dispatch.
        let mut capture = bytes(1, &[8, 31]);
        capture.extend(bytes(2, &[16, 2, 144, 1, 60]));
        send(&viewer, "CONTROL_DATA_CHANNEL", crate::initialization::envelope(10, 0, 21, &capture)).await;
        send(&viewer, "CONTROL_DATA_CHANNEL", crate::initialization::envelope(91, 0, 3, &[8, 0])).await;
        let echo = receive(&mut viewer.messages, "CONTROL_DATA_CHANNEL", 3).await;
        let echo: serde_json::Value = serde_json::from_slice(body(body(&echo, 3).unwrap(), 2).unwrap()).unwrap();
        assert_eq!(echo["seq"], 91);
        send(&viewer, "CONTROL_DATA_CHANNEL",
            crate::initialization::envelope(92, 0, 16, &[8, 1])).await;
        let state = receive(&mut viewer.messages, "CONTROL_DATA_CHANNEL", 15).await;
        assert!(body(body(&state, 15).unwrap(), 1).is_some());
        // Missing password exercises the real AUTO_UNLOCK_SYSTEM failure route
        // without invoking PAM or unlocking the local user's desktop.
        send(&viewer, "CONTROL_DATA_CHANNEL", crate::initialization::envelope(1, 0, 3, &[8, 15])).await;
        receive(&mut viewer.messages, "CONTROL_DATA_CHANNEL", 12).await;

        let (terminal, mut terminal_viewer) = connect(input.clone(), false, None,
            &["CONTROL_DATA_CHANNEL", "BINARY_DATA_CHANNEL"]).await;
        send(&terminal_viewer, "BINARY_DATA_CHANNEL",
            crate::terminal::encode(crate::terminal::LIST_REQ, 71, &[])).await;
        let frame = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let (channel, message) = terminal_viewer.messages.recv().await.unwrap();
                if channel == "BINARY_DATA_CHANNEL" { break crate::terminal::decode(&message).unwrap(); }
            }
        }).await.unwrap();
        assert_eq!((frame.kind, frame.session), (crate::terminal::LIST_RESP, 71));

        let (files, mut file_viewer) = connect(input.clone(), false, None,
            &["CONTROL_DATA_CHANNEL", "FILE_DATA_CHANNEL"]).await;
        let directory = std::env::temp_dir().join(format!("uu-control-empty-{}-{}", std::process::id(), rand::random::<u64>()));
        std::fs::create_dir(&directory).unwrap();
        let mut read = bytes(1, &[8, 7]);
        read.extend(bytes(2, directory.to_str().unwrap().as_bytes()));
        let mut request = bytes(1, &[8, 99]);
        request.extend(bytes(8, &bytes(10, &read)));
        let query = crate::initialization::envelope(5, 0, 21, &request);
        send(&file_viewer, "CONTROL_DATA_CHANNEL", query.clone()).await;
        let response = receive(&mut file_viewer.messages, "FILE_DATA_CHANNEL", 22).await;
        let listing = body(body(body(&response, 22).unwrap(), 4).unwrap(), 7).unwrap();
        assert!(decode_wire(listing).unwrap().iter().any(|f| f.number == 4 && f.value == WireValue::Varint(1)));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let echo = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let (mut read, mut write) = socket.into_split();
            tokio::io::copy(&mut read, &mut write).await.unwrap();
        });
        let (ports, mut port_viewer) = connect(input, false, None,
            &["CONTROL_DATA_CHANNEL", "FILE_DATA_CHANNEL"]).await;
        let target = serde_json::json!({"version":1,"target_host":"127.0.0.1","target_port":port}).to_string();
        send(&port_viewer, "FILE_DATA_CHANNEL", mapping(0, target.as_bytes())).await;
        let ack = receive(&mut port_viewer.messages, "FILE_DATA_CHANNEL", 27).await;
        let ack: serde_json::Value = serde_json::from_slice(body(body(&ack, 27).unwrap(), 5).unwrap()).unwrap();
        assert_eq!(ack["ok"], true);

        let mut sessions = HashMap::from([
            ("desktop".into(), desktop), ("terminal".into(), terminal), ("files".into(), files),
            ("ports".into(), ports),
        ]);
        assert!(crate::superseded_peer_ids(&sessions, "another-tool", false).is_empty());
        assert_eq!(crate::superseded_peer_ids(&sessions, "next-desktop", true), ["desktop"]);
        for id in crate::superseded_peer_ids(&sessions, "next-desktop", true) {
            sessions.remove(&id).unwrap().pc.close().await.unwrap();
        }
        assert_eq!(sessions["terminal"].pc.connection_state(), RTCPeerConnectionState::Connected);
        assert_eq!(sessions["files"].pc.connection_state(), RTCPeerConnectionState::Connected);
        // A desktop close must not cancel the independent file-transfer responder.
        send(&file_viewer, "CONTROL_DATA_CHANNEL", query).await;
        receive(&mut file_viewer.messages, "FILE_DATA_CHANNEL", 22).await;
        send(&port_viewer, "FILE_DATA_CHANNEL", mapping(2, b"local echo")).await;
        let echoed = receive(&mut port_viewer.messages, "FILE_DATA_CHANNEL", 27).await;
        assert_eq!(body(body(&echoed, 27).unwrap(), 5), Some(b"local echo".as_slice()));
        send(&terminal_viewer, "BINARY_DATA_CHANNEL",
            crate::terminal::encode(crate::terminal::LIST_REQ, 72, &[])).await;
        let frame = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let (channel, message) = terminal_viewer.messages.recv().await.unwrap();
                if channel == "BINARY_DATA_CHANNEL" { break crate::terminal::decode(&message).unwrap(); }
            }
        }).await.unwrap();
        assert_eq!((frame.kind, frame.session), (crate::terminal::LIST_RESP, 72));
        std::fs::remove_dir(directory).unwrap();
        for (_, peer) in sessions { peer.pc.close().await.unwrap(); }
        viewer.pc.close().await.unwrap();
        terminal_viewer.pc.close().await.unwrap();
        file_viewer.pc.close().await.unwrap();
        port_viewer.pc.close().await.unwrap();
        for router in [&viewer.router, &terminal_viewer.router, &file_viewer.router, &port_viewer.router] {
            router.lock().await.stop().await.unwrap();
        }
        echo.abort();
        drop(lock);
    }).await.expect("locked bootstrap/tool regression exceeded deadline");
}

#[tokio::test]
async fn capture_desktop_on_a_port_mapping_peer_promotes_it_once() {
    tokio::time::timeout(Duration::from_secs(25), async {
        let input = Arc::new(InputInjector::without_desktop());
        let (host, mut viewer) = connect(input.clone(), false, None, &["CONTROL_DATA_CHANNEL", "FILE_DATA_CHANNEL"]).await;
        let (requests, mut requested) = mpsc::unbounded_channel();
        host.on_desktop_request(move || { let _ = requests.send(()); });
        receive(&mut viewer.messages, "CONTROL_DATA_CHANNEL", 13).await;
        assert!(!host.role.is_desktop() && host.role.input().is_none());
        // The Mac viewer opens the desktop on its existing port-mapping peer (#99).
        send(&viewer, "CONTROL_DATA_CHANNEL", crate::initialization::envelope(5, 0, 3, &[8, 4])).await;
        tokio::time::timeout(Duration::from_secs(5), requested.recv()).await.unwrap().unwrap();
        host.promote(None);
        assert!(host.role.is_desktop());
        assert!(host.role.input().is_some_and(|session| session.is_current()));
        // CONTROL repeats the desktop initialization for the same viewer.
        receive(&mut viewer.messages, "CONTROL_DATA_CHANNEL", 13).await;
        // The port-mapping session on this peer still answers.
        send(&viewer, "FILE_DATA_CHANNEL", mapping(2, b"still mapped")).await;
        receive(&mut viewer.messages, "FILE_DATA_CHANNEL", 27).await;
        // A repeated capture_desktop does not ask again, and a later promote is a no-op.
        send(&viewer, "CONTROL_DATA_CHANNEL", crate::initialization::envelope(6, 0, 3, &[8, 4])).await;
        send(&viewer, "CONTROL_DATA_CHANNEL", crate::initialization::envelope(7, 0, 3, &[8, 0])).await;
        receive(&mut viewer.messages, "CONTROL_DATA_CHANNEL", 3).await;
        assert!(requested.try_recv().is_err());
        let lease = host.role.input().unwrap();
        host.promote(None);
        assert!(lease.is_current());
        // Its video ended on its own (capture failed while locked, #114): capture_desktop restarts it
        // on the same input lease.
        let ended = tokio::spawn(async {});
        *host.role.stream_task.lock().unwrap() = Some(ended.abort_handle());
        ended.await.unwrap();
        send(&viewer, "CONTROL_DATA_CHANNEL", crate::initialization::envelope(9, 0, 3, &[8, 4])).await;
        tokio::time::timeout(Duration::from_secs(5), requested.recv()).await.unwrap().unwrap();
        host.promote(None);
        assert!(host.role.is_desktop() && !host.role.stream_ended() && lease.is_current());
        // A newer desktop demotes it; the port mapping stays, and capture_desktop promotes it again.
        host.demote();
        assert!(!host.role.is_desktop() && host.role.input().is_none());
        send(&viewer, "FILE_DATA_CHANNEL", mapping(2, b"after demotion")).await;
        receive(&mut viewer.messages, "FILE_DATA_CHANNEL", 27).await;
        send(&viewer, "CONTROL_DATA_CHANNEL", crate::initialization::envelope(8, 0, 3, &[8, 4])).await;
        tokio::time::timeout(Duration::from_secs(5), requested.recv()).await.unwrap().unwrap();
        host.promote(None);
        assert!(host.role.is_desktop() && host.role.input().is_some_and(|session| session.is_current()));
        receive(&mut viewer.messages, "CONTROL_DATA_CHANNEL", 13).await;
        host.pc.close().await.unwrap();
        viewer.pc.close().await.unwrap();
        viewer.router.lock().await.stop().await.unwrap();
    }).await.expect("capture_desktop promotion exceeded deadline");
}
