//! CONTROL lifecycle regressions: most viewers get an early Echo while Portal is
//! pending. Mac desktop viewers on an unlocked Portal briefly wait (with a bounded
//! fallback) for a usable Portal clipboard without blocking other CONTROL messages;
//! locked connects answer immediately (unblocking auto-unlock) using any remembered
//! Portal clipboard grant; unknown/unavailable fall back immediately; and the first
//! decision is latched. Capture settings wait for the desktop, and tool peers apply
//! settings against the nominal screen without waiting for a desktop at all.
//! Local SCTP peers only; no cloud, desktop consent, audio or power operations.
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

async fn connect(desktop: bool, portal: Option<Arc<crate::desktop_session::PortalDesktop>>)
    -> (WebRtcSession, Viewer) {
    connect_channels(desktop, portal, &["CONTROL_DATA_CHANNEL"]).await
}

async fn connect_channels(desktop: bool, portal: Option<Arc<crate::desktop_session::PortalDesktop>>, labels: &[&str])
    -> (WebRtcSession, Viewer) {
    connect_as(desktop, portal, labels, crate::input::Controller::Other).await
}

async fn connect_as(desktop: bool, portal: Option<Arc<crate::desktop_session::PortalDesktop>>, labels: &[&str],
    controller: crate::input::Controller) -> (WebRtcSession, Viewer) {
    let input = Arc::new(InputInjector::without_desktop());
    let (host_setting, viewer_setting, router) = crate::test_network::virtual_network().await;
    let screen = Arc::new(ScreenCapture::unavailable("synthetic pending desktop".into()));
    let host = WebRtcSession::new(screen, input, crate::signaling::SignalingClient::disconnected(),
        desktop, crate::session_ice::SessionIce::virtual_net(host_setting), portal,
        controller).await.unwrap();
    let pc = Arc::new(APIBuilder::new().with_setting_engine(viewer_setting).build().new_peer_connection(RTCConfiguration::default()).await.unwrap());
    let (tx, messages) = mpsc::unbounded_channel();
    let mut channels = HashMap::new();
    for &label in labels {
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
fn varint_of(message: &[u8], number: u32) -> Option<u64> {
    decode_wire(message).ok()?.into_iter().find_map(|field| match (field.number, field.value) {
        (n, WireValue::Varint(value)) if n == number => Some(value), _ => None,
    })
}
fn put_varint(mut value: usize, out: &mut Vec<u8>) {
    while value >= 128 { out.push((value as u8 & 127) | 128); value >>= 7; }
    out.push(value as u8);
}
fn field_bytes(number: u32, value: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(((number << 3) | 2) as usize, &mut out);
    put_varint(value.len(), &mut out);
    out.extend_from_slice(value);
    out
}
fn field_varint(number: u32, value: u64, out: &mut Vec<u8>) {
    put_varint((number << 3) as usize, out);
    put_varint(value as usize, out);
}
/// Message 21 RpcRequest {1 header{1 request_id}, <case>}, the verified viewer shape.
fn capture_rpc(request_id: u64, case: u32, setting: &[u8]) -> Vec<u8> {
    let mut header = Vec::new();
    field_varint(1, request_id, &mut header);
    let mut rpc = field_bytes(1, &header);
    rpc.extend(field_bytes(case, setting));
    crate::initialization::envelope(11, 0, 21, &rpc)
}
/// Message 3 SimpleAction action 0: the viewer's FeatureFlag Echo request.
fn echo_request(seq: u64) -> Vec<u8> {
    crate::initialization::envelope(seq, 0, 3, &[8, 0])
}
async fn receive(messages: &mut Messages, field: u32, within: Duration) -> Option<Vec<u8>> {
    tokio::time::timeout(within, async {
        loop {
            let (channel, message) = messages.recv().await?;
            if channel == "CONTROL_DATA_CHANNEL" && body(&message, field).is_some() { return Some(message); }
        }
    }).await.unwrap_or(None)
}
/// The Echo response advertises ff_clipboard (FeatureFlag field 8) only when true.
fn echo_advertises_clipboard(message: &[u8]) -> bool {
    body(message, 3).and_then(|action| decode_wire(action).ok()).is_some_and(|action| {
        action.iter().any(|field| match (field.number, &field.value) {
            (4, WireValue::Bytes(flags)) => decode_wire(flags).ok().is_some_and(|flags| {
                flags.iter().any(|flag| flag.number == 8)
            }),
            _ => false,
        })
    })
}
/// The `seq` echoed in SimpleAction.args JSON (`{"seq":N}`).
fn echo_request_seq(message: &[u8]) -> Option<u64> {
    let args = body(message, 3).and_then(|action| decode_wire(action).ok())
        .and_then(|action| action.into_iter().find_map(|field| match (field.number, field.value) {
            (2, WireValue::Bytes(bytes)) => Some(bytes), _ => None,
        }))?;
    serde_json::from_slice::<serde_json::Value>(args).ok()?.get("seq")?.as_u64()
}

fn ready_screen() -> Arc<ScreenCapture> {
    let mut screen = ScreenCapture::unavailable("synthetic ready desktop".into());
    screen.backend = crate::desktop_backend::DesktopBackend::Portal;
    screen.init_error = None;
    screen.display = "wayland:test".into();
    screen.width = 1920;
    screen.height = 1080;
    Arc::new(screen)
}

async fn local_control_pair() -> (
    Arc<RTCPeerConnection>,
    Arc<RTCDataChannel>,
    Arc<RTCPeerConnection>,
    Messages,
    Arc<Mutex<webrtc::util::vnet::router::Router>>,
) {
    let (host_setting, viewer_setting, router) = crate::test_network::virtual_network().await;
    let host_pc = Arc::new(APIBuilder::new().with_setting_engine(host_setting).build()
        .new_peer_connection(RTCConfiguration::default()).await.unwrap());
    let viewer_pc = Arc::new(APIBuilder::new().with_setting_engine(viewer_setting).build()
        .new_peer_connection(RTCConfiguration::default()).await.unwrap());
    let (host_tx, mut host_rx) = mpsc::unbounded_channel();
    host_pc.on_data_channel(Box::new(move |channel| {
        let host_tx = host_tx.clone();
        Box::pin(async move { let _ = host_tx.send(channel); })
    }));
    let (msg_tx, messages) = mpsc::unbounded_channel();
    let viewer_dc = viewer_pc.create_data_channel("CONTROL_DATA_CHANNEL", None).await.unwrap();
    viewer_dc.on_message(Box::new(move |message| {
        let _ = msg_tx.send(("CONTROL_DATA_CHANNEL".into(), message.data.to_vec()));
        Box::pin(async {})
    }));
    let mut gathered = viewer_pc.gathering_complete_promise().await;
    viewer_pc.set_local_description(viewer_pc.create_offer(None).await.unwrap()).await.unwrap();
    gathered.recv().await;
    host_pc.set_remote_description(viewer_pc.local_description().await.unwrap()).await.unwrap();
    let mut gathered = host_pc.gathering_complete_promise().await;
    host_pc.set_local_description(host_pc.create_answer(None).await.unwrap()).await.unwrap();
    gathered.recv().await;
    viewer_pc.set_remote_description(host_pc.local_description().await.unwrap()).await.unwrap();
    let host_dc = tokio::time::timeout(Duration::from_secs(8), host_rx.recv()).await
        .expect("host CONTROL channel timed out").expect("host CONTROL channel missing");
    tokio::time::timeout(Duration::from_secs(8), async {
        while host_dc.ready_state() != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open
            || viewer_dc.ready_state() != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("local SCTP CONTROL pair did not open");
    (host_pc, host_dc, viewer_pc, messages, router)
}

#[test]
fn portal_clipboard_echo_decision_covers_ready_lock_unavailable_and_waiting_states() {
    let pending = crate::desktop_session::State {
        screen: Arc::new(ScreenCapture::unavailable("pending".into())),
        waiting: true,
    };
    let terminal = crate::desktop_session::State {
        screen: Arc::new(ScreenCapture::unavailable("denied".into())),
        waiting: false,
    };
    let ready_no_portal = crate::desktop_session::State {
        screen: ready_screen(),
        waiting: false,
    };
    assert_eq!(
        portal_clipboard_echo_decision(None, &PortalClipboardState::Pending, &pending, true),
        MacPortalEchoDecision::Reply { clipboard: false, reason: "lock_unknown" },
    );
    assert_eq!(
        portal_clipboard_echo_decision(Some(true), &PortalClipboardState::Pending, &pending, false),
        MacPortalEchoDecision::Reply { clipboard: false, reason: "lock_state" },
    );
    assert_eq!(
        portal_clipboard_echo_decision(Some(true), &PortalClipboardState::Pending, &pending, true),
        MacPortalEchoDecision::Reply { clipboard: true, reason: "locked_remembered_clipboard" },
    );
    assert_eq!(
        portal_clipboard_echo_decision(Some(true), &PortalClipboardState::Unavailable, &pending, true),
        MacPortalEchoDecision::Reply { clipboard: false, reason: "bridge_unavailable" },
    );
    assert_eq!(
        portal_clipboard_echo_decision(Some(true), &PortalClipboardState::Pending, &terminal, true),
        MacPortalEchoDecision::Reply { clipboard: false, reason: "portal_unavailable" },
    );
    assert_eq!(
        portal_clipboard_echo_decision(Some(false), &PortalClipboardState::Pending, &pending, true),
        MacPortalEchoDecision::Wait,
    );
    assert_eq!(
        portal_clipboard_echo_decision(Some(false), &PortalClipboardState::Unavailable, &pending, false),
        MacPortalEchoDecision::Reply { clipboard: false, reason: "bridge_unavailable" },
    );
    assert_eq!(
        portal_clipboard_echo_decision(Some(false), &PortalClipboardState::Pending, &terminal, false),
        MacPortalEchoDecision::Reply { clipboard: false, reason: "portal_unavailable" },
    );
    assert_eq!(
        portal_clipboard_echo_decision(Some(false), &PortalClipboardState::Pending, &ready_no_portal, true),
        MacPortalEchoDecision::Reply { clipboard: false, reason: "no_portal_session" },
    );

    // Exercise the native-session branches of the real decision function directly.
    let base = PortalClipboardSnapshot {
        locked: Some(false),
        waiting: false,
        init_error: false,
        has_portal: true,
        clipboard_supported: true,
        remembered_clipboard: true,
        bridge_unavailable: false,
        bridge_matches_current: true,
        clipboard_ready: true,
    };
    assert_eq!(
        decide_mac_portal_echo(base),
        MacPortalEchoDecision::Reply { clipboard: true, reason: "bridge_ready" },
    );
    assert_eq!(
        decide_mac_portal_echo(PortalClipboardSnapshot { locked: Some(true), ..base }),
        MacPortalEchoDecision::Reply { clipboard: true, reason: "locked_remembered_clipboard" },
    );
    assert_eq!(
        decide_mac_portal_echo(PortalClipboardSnapshot { locked: Some(true), remembered_clipboard: false, ..base }),
        MacPortalEchoDecision::Reply { clipboard: false, reason: "lock_state" },
    );
    assert_eq!(
        decide_mac_portal_echo(PortalClipboardSnapshot { locked: Some(true), clipboard_supported: false, ..base }),
        MacPortalEchoDecision::Reply { clipboard: false, reason: "clipboard_unsupported" },
    );
    assert_eq!(
        decide_mac_portal_echo(PortalClipboardSnapshot { clipboard_supported: false, ..base }),
        MacPortalEchoDecision::Reply { clipboard: false, reason: "clipboard_unsupported" },
    );
    assert_eq!(
        decide_mac_portal_echo(PortalClipboardSnapshot { bridge_matches_current: false, ..base }),
        MacPortalEchoDecision::Wait,
    );
    assert_eq!(
        decide_mac_portal_echo(PortalClipboardSnapshot { clipboard_ready: false, ..base }),
        MacPortalEchoDecision::Wait,
    );
    assert_eq!(
        decide_mac_portal_echo(PortalClipboardSnapshot { waiting: true, ..base }),
        MacPortalEchoDecision::Wait,
    );

    let (tx, rx) = tokio::sync::watch::channel(PortalClipboardState::Pending);
    {
        let _guard = PortalClipboardPendingGuard(Some(tx));
        assert!(matches!(*rx.borrow(), PortalClipboardState::Pending));
    }
    assert!(matches!(*rx.borrow(), PortalClipboardState::Unavailable));
}

#[tokio::test]
async fn mac_echo_answers_locked_and_unknown_immediately_while_unlocked_waits() {
    tokio::time::timeout(Duration::from_secs(25), async {
        // 1. Unknown lock state (None) must fail closed without hanging Echo, even with a remembered grant.
        {
            let (_lock_state, locked) = tokio::sync::watch::channel(None);
            let portal = Arc::new(crate::desktop_session::PortalDesktop::with_lock_state(
                Arc::new(ScreenCapture::unavailable("synthetic pending desktop".into())), locked)
                .with_remembered_clipboard(true));
            let (host, mut viewer) = connect_as(true, Some(portal), &["CONTROL_DATA_CHANNEL"],
                crate::input::Controller::Mac).await;
            viewer.channels["CONTROL_DATA_CHANNEL"].send(&echo_request(3).into()).await.unwrap();
            let echo = receive(&mut viewer.messages, 3, Duration::from_secs(3)).await
                .expect("unknown lock state stalled Mac Echo");
            assert_eq!(echo_request_seq(&echo), Some(3));
            assert!(!echo_advertises_clipboard(&echo));
            viewer.pc.close().await.unwrap();
            host.pc.close().await.unwrap();
            viewer.router.lock().await.stop().await.unwrap();
        }

        // 2. Locked cold connection without a remembered clipboard grant answers immediately with false
        //    so Mac can proceed to auto-unlock without waiting.
        {
            let (_lock_state, locked) = tokio::sync::watch::channel(Some(true));
            let portal = Arc::new(crate::desktop_session::PortalDesktop::with_lock_state(
                Arc::new(ScreenCapture::unavailable("synthetic pending desktop".into())), locked));
            let (host, mut viewer) = connect_as(true, Some(portal), &["CONTROL_DATA_CHANNEL"],
                crate::input::Controller::Mac).await;
            viewer.channels["CONTROL_DATA_CHANNEL"].send(&echo_request(4).into()).await.unwrap();
            let echo = receive(&mut viewer.messages, 3, Duration::from_secs(3)).await
                .expect("locked desktop without remembered grant stalled Mac Echo");
            assert_eq!(echo_request_seq(&echo), Some(4));
            assert!(!echo_advertises_clipboard(&echo));
            viewer.pc.close().await.unwrap();
            host.pc.close().await.unwrap();
            viewer.router.lock().await.stop().await.unwrap();
        }

        // 3. Locked cold connection WITH a remembered Portal clipboard grant answers immediately with
        //    clipboard=true (unblocking auto-unlock and letting Mac latch ff_clipboard) and latches it.
        {
            let (_lock_state, locked) = tokio::sync::watch::channel(Some(true));
            let portal = Arc::new(crate::desktop_session::PortalDesktop::with_lock_state(
                Arc::new(ScreenCapture::unavailable("synthetic pending desktop".into())), locked)
                .with_remembered_clipboard(true));
            let (host, mut viewer) = connect_as(true, Some(portal), &["CONTROL_DATA_CHANNEL"],
                crate::input::Controller::Mac).await;
            let control = viewer.channels["CONTROL_DATA_CHANNEL"].clone();
            control.send(&echo_request(5).into()).await.unwrap();
            let echo = receive(&mut viewer.messages, 3, Duration::from_secs(3)).await
                .expect("locked desktop with remembered grant stalled Mac Echo");
            assert_eq!(echo_request_seq(&echo), Some(5));
            assert!(echo_advertises_clipboard(&echo),
                "remembered locked Portal grant must advertise ff_clipboard immediately");
            control.send(&echo_request(6).into()).await.unwrap();
            let follow = receive(&mut viewer.messages, 3, Duration::from_secs(3)).await
                .expect("post-latch locked Echo was not answered");
            assert_eq!(echo_request_seq(&follow), Some(6));
            assert!(echo_advertises_clipboard(&follow));
            viewer.pc.close().await.unwrap();
            host.pc.close().await.unwrap();
            viewer.router.lock().await.stop().await.unwrap();
        }

        // 4. Unlocked pending Portal waits without blocking CONTROL queries, coalesces
        //    requests, falls back once clipboard is known unavailable, and latches.
        {
            let (_lock_state, locked) = tokio::sync::watch::channel(Some(false));
            let portal = Arc::new(crate::desktop_session::PortalDesktop::with_lock_state(
                Arc::new(ScreenCapture::unavailable("synthetic pending desktop".into())), locked));
            let (host, mut viewer) = connect_as(true, Some(portal.clone()), &["CONTROL_DATA_CHANNEL"],
                crate::input::Controller::Mac).await;
            let control = viewer.channels["CONTROL_DATA_CHANNEL"].clone();
            assert!(receive(&mut viewer.messages, 15, Duration::from_secs(3)).await.is_some());
            control.send(&echo_request(3).into()).await.unwrap();
            control.send(&echo_request(4).into()).await.unwrap();
            assert!(receive(&mut viewer.messages, 3, Duration::from_millis(250)).await.is_none());
            let query = crate::initialization::envelope(5, 0, 16, &[8, 1]);
            control.send(&query.into()).await.unwrap();
            assert!(receive(&mut viewer.messages, 15, Duration::from_secs(3)).await.is_some(),
                "waiting for clipboard blocked secure desktop query");

            portal.publish_test_screen(ready_screen());
            let echo = receive(&mut viewer.messages, 3, Duration::from_secs(3)).await
                .expect("unavailable Portal clipboard stalled Mac Echo");
            assert_eq!(echo_request_seq(&echo), Some(4));
            assert!(!echo_advertises_clipboard(&echo));
            assert!(receive(&mut viewer.messages, 3, Duration::from_millis(200)).await.is_none(),
                "stale coalesced Echo request was also answered");

            control.send(&echo_request(6).into()).await.unwrap();
            let follow = receive(&mut viewer.messages, 3, Duration::from_secs(3)).await
                .expect("post-latch Echo was not answered");
            assert_eq!(echo_request_seq(&follow), Some(6));
            assert!(!echo_advertises_clipboard(&follow));
            viewer.pc.close().await.unwrap();
            host.pc.close().await.unwrap();
            viewer.router.lock().await.stop().await.unwrap();
        }

        // 5. Locking mid-wait immediately releases the held Echo using the remembered grant flag.
        for remembered in [false, true] {
            let (lock_state, locked) = tokio::sync::watch::channel(Some(false));
            let portal = Arc::new(crate::desktop_session::PortalDesktop::with_lock_state(
                Arc::new(ScreenCapture::unavailable("synthetic pending desktop".into())), locked)
                .with_remembered_clipboard(remembered));
            let (host, mut viewer) = connect_as(true, Some(portal), &["CONTROL_DATA_CHANNEL"],
                crate::input::Controller::Mac).await;
            let control = viewer.channels["CONTROL_DATA_CHANNEL"].clone();
            control.send(&echo_request(8).into()).await.unwrap();
            assert!(receive(&mut viewer.messages, 3, Duration::from_millis(200)).await.is_none());
            lock_state.send_replace(Some(true));
            let echo = receive(&mut viewer.messages, 3, Duration::from_secs(3)).await
                .expect("lock mid-wait did not release Mac Echo");
            assert_eq!(echo_request_seq(&echo), Some(8));
            assert_eq!(echo_advertises_clipboard(&echo), remembered);
            viewer.pc.close().await.unwrap();
            host.pc.close().await.unwrap();
            viewer.router.lock().await.stop().await.unwrap();
        }
    }).await.expect("Mac Portal Echo fallback regression exceeded deadline");
}

#[tokio::test]
async fn mac_portal_echo_coalesces_to_latest_and_latches_when_bridge_ready() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    tokio::time::timeout(Duration::from_secs(15), async {
        let (host_pc, host_dc, viewer_pc, mut messages, router) = local_control_pair().await;
        let (lock_state, locked) = tokio::sync::watch::channel(Some(false));
        let portal = Arc::new(crate::desktop_session::PortalDesktop::with_lock_state(
            Arc::new(ScreenCapture::unavailable("synthetic pending desktop".into())), locked));
        let (_bridge_tx, bridge_rx) = tokio::sync::watch::channel(PortalClipboardState::Pending);
        let (req_tx, req_rx) = tokio::sync::mpsc::unbounded_channel();
        let ready = Arc::new(AtomicBool::new(false));
        let ready_check = ready.clone();
        let task = tokio::spawn(reply_when_portal_clipboard_ready_with(
            host_dc,
            Arc::new(AtomicU64::new(1)),
            req_rx,
            bridge_rx,
            portal,
            Duration::from_secs(5),
            Duration::from_secs(10),
            move |locked, _, _| {
                if locked != Some(false) {
                    MacPortalEchoDecision::Reply { clipboard: false, reason: "lock_state" }
                } else if ready_check.load(Ordering::Acquire) {
                    MacPortalEchoDecision::Reply { clipboard: true, reason: "bridge_ready" }
                } else {
                    MacPortalEchoDecision::Wait
                }
            },
        ));

        req_tx.send(3).unwrap();
        req_tx.send(4).unwrap();
        req_tx.send(6).unwrap();
        assert!(receive(&mut messages, 3, Duration::from_millis(200)).await.is_none());

        // Flip readiness without touching any watch: the periodic poll must still wake it.
        ready.store(true, Ordering::Release);
        let echo = receive(&mut messages, 3, Duration::from_secs(3)).await
            .expect("ready bridge did not trigger coalesced Echo reply");
        assert_eq!(echo_request_seq(&echo), Some(6));
        assert!(echo_advertises_clipboard(&echo), "ready Mac Portal Echo must advertise ff_clipboard");
        assert!(receive(&mut messages, 3, Duration::from_millis(200)).await.is_none(),
            "coalesced Echo emitted more than one reply");

        // Mid-session lock or transient clipboard loss must not stall subsequent Echoes.
        ready.store(false, Ordering::Release);
        lock_state.send_replace(Some(true));
        req_tx.send(7).unwrap();
        req_tx.send(8).unwrap();
        let first = receive(&mut messages, 3, Duration::from_secs(3)).await
            .expect("post-latch Echo 7 stalled");
        let second = receive(&mut messages, 3, Duration::from_secs(3)).await
            .expect("post-latch Echo 8 stalled");
        assert_eq!(echo_request_seq(&first), Some(7));
        assert_eq!(echo_request_seq(&second), Some(8));
        assert!(echo_advertises_clipboard(&first));
        assert!(echo_advertises_clipboard(&second));

        drop(req_tx);
        let _ = task.await;
        viewer_pc.close().await.unwrap();
        host_pc.close().await.unwrap();
        router.lock().await.stop().await.unwrap();
    }).await.expect("Mac Portal ready Echo regression exceeded deadline");
}

#[tokio::test]
async fn mac_portal_echo_uses_two_stage_timeout_for_consent_then_bridge() {
    use std::sync::atomic::AtomicU64;
    tokio::time::timeout(Duration::from_secs(15), async {
        // 1. While Portal consent is pending (state.waiting), bridge_wait does not fire;
        //    once Portal becomes ready (!state.waiting), bridge_wait starts and falls back.
        {
            let (host_pc, host_dc, viewer_pc, mut messages, router) = local_control_pair().await;
            let (_lock_state, locked) = tokio::sync::watch::channel(Some(false));
            let portal = Arc::new(crate::desktop_session::PortalDesktop::with_lock_state(
                Arc::new(ScreenCapture::unavailable("synthetic pending desktop".into())), locked));
            let (_bridge_tx, bridge_rx) = tokio::sync::watch::channel(PortalClipboardState::Pending);
            let (req_tx, req_rx) = tokio::sync::mpsc::unbounded_channel();
            let task = tokio::spawn(reply_when_portal_clipboard_ready_with(
                host_dc,
                Arc::new(AtomicU64::new(1)),
                req_rx,
                bridge_rx,
                portal.clone(),
                Duration::from_millis(120),
                Duration::from_secs(5),
                |_, _, _| MacPortalEchoDecision::Wait,
            ));

            req_tx.send(11).unwrap();
            // Longer than bridge_wait (120 ms): must stay pending while Portal consent waits.
            assert!(receive(&mut messages, 3, Duration::from_millis(260)).await.is_none(),
                "bridge_wait expired while Portal consent was still waiting");

            // Portal becomes ready (!waiting): bridge_wait (120 ms) now expires and falls back.
            req_tx.send(12).unwrap();
            portal.publish_test_screen(ready_screen());
            let echo = receive(&mut messages, 3, Duration::from_secs(2)).await
                .expect("post-ready bridge_wait did not fall back");
            assert_eq!(echo_request_seq(&echo), Some(12));
            assert!(!echo_advertises_clipboard(&echo));

            req_tx.send(13).unwrap();
            let follow = receive(&mut messages, 3, Duration::from_secs(2)).await
                .expect("post-timeout Echo was not answered immediately");
            assert_eq!(echo_request_seq(&follow), Some(13));
            assert!(!echo_advertises_clipboard(&follow));

            drop(req_tx);
            let _ = task.await;
            viewer_pc.close().await.unwrap();
            host_pc.close().await.unwrap();
            router.lock().await.stop().await.unwrap();
        }

        // 2. Total timeout still bounds an unanswered Portal consent dialog.
        {
            let (host_pc, host_dc, viewer_pc, mut messages, router) = local_control_pair().await;
            let (_lock_state, locked) = tokio::sync::watch::channel(Some(false));
            let portal = Arc::new(crate::desktop_session::PortalDesktop::with_lock_state(
                Arc::new(ScreenCapture::unavailable("synthetic pending desktop".into())), locked));
            let (_bridge_tx, bridge_rx) = tokio::sync::watch::channel(PortalClipboardState::Pending);
            let (req_tx, req_rx) = tokio::sync::mpsc::unbounded_channel();
            let task = tokio::spawn(reply_when_portal_clipboard_ready_with(
                host_dc,
                Arc::new(AtomicU64::new(1)),
                req_rx,
                bridge_rx,
                portal,
                Duration::from_secs(5),
                Duration::from_millis(150),
                |_, _, _| MacPortalEchoDecision::Wait,
            ));

            req_tx.send(21).unwrap();
            let echo = receive(&mut messages, 3, Duration::from_secs(2)).await
                .expect("total consent timeout did not fall back");
            assert_eq!(echo_request_seq(&echo), Some(21));
            assert!(!echo_advertises_clipboard(&echo));

            drop(req_tx);
            let _ = task.await;
            viewer_pc.close().await.unwrap();
            host_pc.close().await.unwrap();
            router.lock().await.stop().await.unwrap();
        }
    }).await.expect("Mac Portal two-stage timeout regression exceeded deadline");
}

#[tokio::test]
async fn mac_portal_echo_worker_exits_on_channel_close_even_while_sender_lives() {
    use std::sync::atomic::AtomicU64;
    tokio::time::timeout(Duration::from_secs(15), async {
        // 1. Before any Echo arrives.
        {
            let (host_pc, host_dc, viewer_pc, _messages, router) = local_control_pair().await;
            let (_lock_state, locked) = tokio::sync::watch::channel(Some(false));
            let portal = Arc::new(crate::desktop_session::PortalDesktop::with_lock_state(
                Arc::new(ScreenCapture::unavailable("synthetic pending desktop".into())), locked));
            let (_bridge_tx, bridge_rx) = tokio::sync::watch::channel(PortalClipboardState::Pending);
            let (_req_tx, req_rx) = tokio::sync::mpsc::unbounded_channel();
            let task = tokio::spawn(reply_when_portal_clipboard_ready_with(
                host_dc,
                Arc::new(AtomicU64::new(1)),
                req_rx,
                bridge_rx,
                portal,
                Duration::from_secs(5),
                Duration::from_secs(10),
                |_, _, _| MacPortalEchoDecision::Wait,
            ));
            viewer_pc.close().await.unwrap();
            host_pc.close().await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), task).await
                .expect("pre-Echo worker leaked after channel close").unwrap();
            router.lock().await.stop().await.unwrap();
        }

        // 2. After the first Echo has been latched.
        {
            let (host_pc, host_dc, viewer_pc, mut messages, router) = local_control_pair().await;
            let (_lock_state, locked) = tokio::sync::watch::channel(Some(false));
            let portal = Arc::new(crate::desktop_session::PortalDesktop::with_lock_state(
                Arc::new(ScreenCapture::unavailable("synthetic pending desktop".into())), locked));
            let (_bridge_tx, bridge_rx) = tokio::sync::watch::channel(PortalClipboardState::Pending);
            let (req_tx, req_rx) = tokio::sync::mpsc::unbounded_channel();
            let task = tokio::spawn(reply_when_portal_clipboard_ready_with(
                host_dc,
                Arc::new(AtomicU64::new(1)),
                req_rx,
                bridge_rx,
                portal,
                Duration::from_secs(5),
                Duration::from_secs(10),
                |_, _, _| MacPortalEchoDecision::Reply { clipboard: true, reason: "bridge_ready" },
            ));
            req_tx.send(1).unwrap();
            let echo = receive(&mut messages, 3, Duration::from_secs(3)).await
                .expect("initial Echo reply missing");
            assert_eq!(echo_request_seq(&echo), Some(1));
            assert!(echo_advertises_clipboard(&echo));
            // Keep `req_tx` alive to mimic `on_message` retaining the sender if `on_close` misses.
            viewer_pc.close().await.unwrap();
            host_pc.close().await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), task).await
                .expect("post-latch worker leaked after channel close").unwrap();
            drop(req_tx);
            router.lock().await.stop().await.unwrap();
        }
    }).await.expect("Mac Portal Echo worker exit regression exceeded deadline");
}

/// A Portal screen whose session reports clipboard support, for the refresh path.
/// `PortalSession` is a real native handle type, so this uses the manager's own
/// test publish hook with a session-less screen and asserts the negative path:
/// a ready screen without a Portal session must not advertise ff_clipboard.
#[tokio::test]
async fn ready_without_portal_session_does_not_advertise_clipboard() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let (_, locked) = tokio::sync::watch::channel(Some(false));
        let portal = Arc::new(crate::desktop_session::PortalDesktop::with_lock_state(
            Arc::new(ScreenCapture::unavailable("synthetic pending desktop".into())), locked));
        let (host, mut viewer) = connect(true, Some(portal.clone())).await;
        // Establish the early Echo while the desktop is still pending: no clipboard claim.
        viewer.channels["CONTROL_DATA_CHANNEL"].send(&echo_request(3).into()).await.unwrap();
        let echo = receive(&mut viewer.messages, 3, Duration::from_secs(3)).await.expect("early Echo");
        assert!(!echo_advertises_clipboard(&echo));
        // Readiness with no Portal session keeps the capability false and must not
        // manufacture an unsolicited Echo response from a timer tick.
        portal.publish_test_screen(ready_screen());
        assert!(receive(&mut viewer.messages, 3, Duration::from_millis(800)).await.is_none(),
            "Echo was re-sent without an established clipboard capability");
        viewer.pc.close().await.unwrap();
        host.pc.close().await.unwrap();
        viewer.router.lock().await.stop().await.unwrap();
    }).await.expect("clipboard capability regression exceeded deadline");
}

#[tokio::test]
async fn pending_portal_answers_echo_immediately_and_defers_capture_settings() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let (_, locked) = tokio::sync::watch::channel(Some(false));
        let portal = Arc::new(crate::desktop_session::PortalDesktop::with_lock_state(
            Arc::new(ScreenCapture::unavailable("synthetic pending desktop".into())), locked));
        let (host, mut viewer) = connect(true, Some(portal.clone())).await;

        // Echo must be answered while the placeholder screen is still pending, and it
        // must not claim a clipboard bridge that is not established yet.
        viewer.channels["CONTROL_DATA_CHANNEL"].send(&echo_request(7).into()).await.unwrap();
        let echo = receive(&mut viewer.messages, 3, Duration::from_secs(3)).await.expect("early Echo was not answered while Portal was pending");
        assert!(!echo_advertises_clipboard(&echo));
        let args = body(&echo, 3).and_then(|action| decode_wire(action).ok())
            .and_then(|action| action.into_iter().find_map(|field| match (field.number, field.value) {
                (2, WireValue::Bytes(bytes)) => Some(bytes), _ => None,
            }));
        assert_eq!(args, Some(br#"{"seq":7}"#.as_slice()));

        // A capture setting arriving during the same pending window is queued, not dropped.
        let mut setting = Vec::new();
        field_varint(2, 2, &mut setting);  // quality = General (高清 2M)
        field_varint(18, 30, &mut setting); // fpsCount = 30
        field_varint(5, 1920, &mut setting);
        field_varint(6, 1080, &mut setting);
        viewer.channels["CONTROL_DATA_CHANNEL"].send(&capture_rpc(77, 2, &setting).into()).await.unwrap();
        assert!(receive(&mut viewer.messages, 22, Duration::from_millis(700)).await.is_none(),
            "capture setting answered before the desktop was ready");

        // Readiness publishes the real geometry; the queued request is applied and answered.
        let mut screen = ready_screen();
        let native = Arc::get_mut(&mut screen).unwrap();
        native.width = 3840;
        native.height = 2160;
        portal.publish_test_screen(screen);
        let response = receive(&mut viewer.messages, 22, Duration::from_secs(5)).await.expect("deferred capture setting was never answered");
        let header = body(&response, 22).and_then(|r| body(r, 1)).expect("RpcResponse header");
        assert_eq!(varint_of(header, 1), Some(77), "deferred reply lost the request id");
        let inventory = receive(&mut viewer.messages, 7, Duration::from_secs(5)).await.expect("updated ScreenSources");
        let display = body(&inventory, 7).and_then(|sources| body(sources, 1)).expect("display");
        // v4 advertises pixels even when the recovered desktop's native size differs.
        for (field, size) in [(4, (1920, 1080)), (6, (3840, 2160))] {
            let rect = body(display, field).expect("display rectangle");
            assert_eq!((varint_of(rect, 3), varint_of(rect, 4)), (Some(size.0), Some(size.1)));
            assert_eq!((varint_of(rect, 5), varint_of(rect, 6)), (Some(size.0), Some(size.1)));
        }
        assert!(decode_wire(display).unwrap().iter().any(|field|
            field.number == 8 && field.value == WireValue::Fixed64(1.0f64.to_le_bytes())));
        let sync = receive(&mut viewer.messages, 25, Duration::from_secs(5)).await.expect("CaptureSettingSync");
        let sync = body(&sync, 25).expect("CaptureSettingSync payload");
        assert_eq!(varint_of(sync, 1), Some(30));
        assert_eq!((varint_of(sync, 5), varint_of(sync, 6)), (Some(1920), Some(1080)));

        // CONTROL still serves Echo after readiness, and no timer-driven flood appears.
        viewer.channels["CONTROL_DATA_CHANNEL"].send(&echo_request(9).into()).await.unwrap();
        assert!(receive(&mut viewer.messages, 3, Duration::from_secs(3)).await.is_some());
        assert!(receive(&mut viewer.messages, 3, Duration::from_millis(700)).await.is_none(),
            "Echo was re-sent without a capability change");

        viewer.pc.close().await.unwrap();
        host.pc.close().await.unwrap();
        viewer.router.lock().await.stop().await.unwrap();
    }).await.expect("pending-Portal lifecycle regression exceeded deadline");
}

#[tokio::test]
async fn tool_peer_applies_capture_settings_without_a_desktop() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let (host, mut viewer) = connect(false, None).await;
        let mut setting = Vec::new();
        field_varint(1, 1280, &mut setting);
        field_varint(2, 720, &mut setting);
        viewer.channels["CONTROL_DATA_CHANNEL"].send(&capture_rpc(5, 6, &setting).into()).await.unwrap();
        let response = receive(&mut viewer.messages, 22, Duration::from_secs(3)).await.expect("tool capture setting was not answered");
        let header = body(&response, 22).and_then(|r| body(r, 1)).expect("RpcResponse header");
        assert_eq!(varint_of(header, 1), Some(5));
        let sync = receive(&mut viewer.messages, 25, Duration::from_secs(3)).await.expect("CaptureSettingSync");
        let sync = body(&sync, 25).expect("CaptureSettingSync payload");
        assert_eq!((varint_of(sync, 5), varint_of(sync, 6)), (Some(1280), Some(720)));
        viewer.pc.close().await.unwrap();
        host.pc.close().await.unwrap();
        viewer.router.lock().await.stop().await.unwrap();
    }).await.expect("tool capture-setting regression exceeded deadline");
}

/// Exercise the locally patched notifications and close handler through real SCTP,
/// alongside the upstream CONTROL drain. No cloud or live host session is used.
#[tokio::test]
async fn terminal_notifications_and_disconnect_preserve_control_channel() {
    use crate::terminal::{self, TerminalConn};
    tokio::time::timeout(Duration::from_secs(20), async {
        let id = u32::MAX - 100;
        struct Cleanup(u32);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let (tx, _rx) = mpsc::unbounded_channel();
                TerminalConn::new(tx).handle(&terminal::encode(terminal::KILL_REQ, 999,
                    serde_json::json!({"session_id": self.0}).to_string().as_bytes()));
            }
        }
        let _cleanup = Cleanup(id);
        let (host, mut viewer) = connect_channels(false, None,
            &["CONTROL_DATA_CHANNEL", "BINARY_DATA_CHANNEL"]).await;
        viewer.channels["BINARY_DATA_CHANNEL"].send(&terminal::encode(terminal::OPEN, id,
            br#"{"cols":80,"rows":24,"shell":"/bin/sh"}"#).into()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            let (mut opened, mut notified) = (false, false);
            while !(opened && notified) {
                let (label, bytes) = viewer.messages.recv().await.expect("terminal channel closed");
                if label != "BINARY_DATA_CHANNEL" { continue; }
                if let Some(frame) = terminal::decode(&bytes) {
                    if frame.kind == terminal::OPEN_RESP && frame.session == id {
                        assert_eq!(serde_json::from_slice::<serde_json::Value>(&frame.payload).unwrap()["code"], 0);
                        opened = true;
                    }
                } else if body(&bytes, 28) == Some(&[][..]) { notified = true; }
            }
        }).await.expect("open response and list notification must both reach the viewer");
        let state = || {
            let (tx, mut rx) = mpsc::unbounded_channel();
            TerminalConn::new(tx).handle(&terminal::encode(terminal::LIST_REQ, 998, &[]));
            let frame = terminal::decode(&rx.try_recv().unwrap()).unwrap();
            serde_json::from_slice::<Vec<serde_json::Value>>(&frame.payload).unwrap().into_iter()
                .find(|s| s["session_id"] == id).unwrap()["state"].as_str().unwrap().to_owned()
        };
        assert_eq!(state(), "running");
        viewer.channels["BINARY_DATA_CHANNEL"].close().await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while state() != "detached" { tokio::time::sleep(Duration::from_millis(20)).await; }
        }).await.expect("channel closure must detach its terminal session");
        viewer.channels["CONTROL_DATA_CHANNEL"].send(&echo_request(123).into()).await.unwrap();
        assert!(receive(&mut viewer.messages, 3, Duration::from_secs(3)).await.is_some(),
            "terminal closure must leave CONTROL responsive");
        viewer.pc.close().await.unwrap();
        host.pc.close().await.unwrap();
        viewer.router.lock().await.stop().await.unwrap();
    }).await.expect("terminal lifecycle regression exceeded deadline");
}
