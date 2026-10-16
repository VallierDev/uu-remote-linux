//! CONTROL lifecycle regressions: an early Echo must be answered while the Portal
//! session is still pending, capture settings must be deferred (never dropped, never
//! blocking Echo/unlock) until the desktop is ready, and tool peers must apply
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
}

async fn connect(desktop: bool, portal: Option<Arc<crate::desktop_session::PortalDesktop>>)
    -> (WebRtcSession, Viewer) {
    let input = Arc::new(InputInjector::without_desktop());
    let screen = Arc::new(ScreenCapture::unavailable("synthetic pending desktop".into()));
    let host = WebRtcSession::new(screen, input, crate::signaling::SignalingClient::disconnected(),
        desktop, crate::session_ice::SessionIce::local(), portal).await.unwrap();
    let pc = Arc::new(APIBuilder::new().build().new_peer_connection(RTCConfiguration::default()).await.unwrap());
    let (tx, messages) = mpsc::unbounded_channel();
    let mut channels = HashMap::new();
    for label in ["CONTROL_DATA_CHANNEL"] {
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
    (host, Viewer { pc, channels, messages })
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
fn ready_screen() -> Arc<ScreenCapture> {
    let mut screen = ScreenCapture::unavailable("synthetic ready desktop".into());
    screen.backend = crate::desktop_backend::DesktopBackend::Portal;
    screen.init_error = None;
    screen.display = "wayland:test".into();
    screen.width = 1920;
    screen.height = 1080;
    Arc::new(screen)
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
        viewer.channels["CONTROL_DATA_CHANNEL"].send(&capture_rpc(77, 2, &setting).into()).await.unwrap();
        assert!(receive(&mut viewer.messages, 22, Duration::from_millis(700)).await.is_none(),
            "capture setting answered before the desktop was ready");

        // Readiness publishes the real geometry; the queued request is applied and answered.
        portal.publish_test_screen(ready_screen());
        let response = receive(&mut viewer.messages, 22, Duration::from_secs(5)).await.expect("deferred capture setting was never answered");
        let header = body(&response, 22).and_then(|r| body(r, 1)).expect("RpcResponse header");
        assert_eq!(varint_of(header, 1), Some(77), "deferred reply lost the request id");
        let sync = receive(&mut viewer.messages, 25, Duration::from_secs(5)).await.expect("CaptureSettingSync");
        let sync = body(&sync, 25).expect("CaptureSettingSync payload");
        assert_eq!(varint_of(sync, 1), Some(30));

        // CONTROL still serves Echo after readiness, and no timer-driven flood appears.
        viewer.channels["CONTROL_DATA_CHANNEL"].send(&echo_request(9).into()).await.unwrap();
        assert!(receive(&mut viewer.messages, 3, Duration::from_secs(3)).await.is_some());
        assert!(receive(&mut viewer.messages, 3, Duration::from_millis(700)).await.is_none(),
            "Echo was re-sent without a capability change");

        viewer.pc.close().await.unwrap();
        host.pc.close().await.unwrap();
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
    }).await.expect("tool capture-setting regression exceeded deadline");
}
