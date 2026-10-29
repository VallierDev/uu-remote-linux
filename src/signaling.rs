//! Lightweight Native Engine.IO v4 / Socket.IO Signaling Client

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

/// A peer that stops reading must not park the heartbeat loop on a full queue.
/// Kept below the Engine.IO window so the protocol deadline still fires.
const SEND_STALL_TIMEOUT: Duration = Duration::from_secs(10);
// DNS/TCP/TLS/WebSocket upgrade must finish in bounded time: after a network
// change the handshake can stall forever and the reconnect loop never retries.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// How often the watchdog checks the last inbound frame.
const WATCHDOG_TICK: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoacMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sdp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gzip_sdp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ice_id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sdp_mid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sdp_mline_index: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_control_id: Option<Value>,
}

pub enum SignalingEvent {
    Stop,
    AssistCommand(crate::assist::Command),
    AssistTick,
    Connected,
    RoomInfoAck(Value),
    DeviceInfoChanged(Value),
    BeControlled(Value),
    /// be-controlled with its binary app_data (ConnectOptions): capture type and the
    /// ice_id it announces, when present.
    ConnectRequest { capture_type: u64, ice_id: Option<String>, transport:crate::session_ice::SessionIce, controller: crate::input::Controller },
    Soac(SoacMessage),
    Released(Value),
    Left(Value),
    Disconnected,
    /// capture_desktop on this tool peer (ice_id); raised by the peer, not by signaling.
    DesktopRequested(String),
}

pub struct SignalingClient {
    tx_outgoing: mpsc::Sender<String>,
    // Belongs to this room only; fresh-room reconnect must not reuse it.
    _reconnect_key: Arc<std::sync::Mutex<Option<String>>>,
}

#[cfg(test)]
impl SignalingClient {
    pub fn disconnected() -> Arc<Self> {
        Arc::new(Self { tx_outgoing: mpsc::channel(1).0, _reconnect_key: Arc::new(std::sync::Mutex::new(None)) })
    }
}

fn refreshed_key(ack:&Value)->Option<String> {
    let values=ack.as_array()?;
    if values.first()?.as_str()? != "success" { return None; }
    let key=values.get(1)?.get("reconnect_key")?.as_str()?;
    (!key.is_empty() && key.len()<=16384).then(||key.to_owned())
}

/// Deliver an event without letting a stalled consumer hide a dead socket.
async fn send_event(sender: &mpsc::Sender<SignalingEvent>, event: SignalingEvent) -> bool {
    matches!(tokio::time::timeout(SEND_STALL_TIMEOUT, sender.send(event)).await, Ok(Ok(())))
}

#[cfg(test)]
mod refresh_tests {
    use super::refreshed_key;
    use serde_json::json;
    #[test]
    fn accepts_only_successful_complete_reconnect_key_ack() {
        assert_eq!(refreshed_key(&json!(["success",{"reconnect_key":"synthetic-key"}])),Some("synthetic-key".into()));
        for ack in [json!(["failure",{"reconnect_key":"synthetic-key"}]),json!(["success"]),json!(["success",{"reconnect_key":""}]),json!(["success",{"reconnect_key":123}]),json!({"reconnect_key":"synthetic-key"})] {
            assert!(refreshed_key(&ack).is_none());
        }
    }
    #[tokio::test]
    async fn closes_obsolete_socket_even_if_peer_retains_client() {
        use futures_util::{SinkExt,StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address=listener.local_addr().unwrap();
        let server=tokio::spawn(async move {
            let (tcp,_)=listener.accept().await.unwrap();
            let mut socket=tokio_tungstenite::accept_async(tcp).await.unwrap();
            socket.send(Message::Text("0{}".into())).await.unwrap();
            assert_eq!(socket.next().await.unwrap().unwrap(),Message::Text("40".into()));
            socket.send(Message::Text("40{}".into())).await.unwrap();
            assert!(socket.next().await.unwrap().unwrap().into_text().unwrap().contains("room_info"));
            matches!(socket.next().await,None|Some(Err(_))|Some(Ok(Message::Close(_))))
        });
        let (sender,mut events)=tokio::sync::mpsc::channel(4);
        let retained=super::SignalingClient::connect(&format!("ws://{address}"),"synthetic-only",sender).await.unwrap();
        assert!(matches!(events.recv().await,Some(super::SignalingEvent::Connected)));
        drop(events);
        assert!(tokio::time::timeout(std::time::Duration::from_secs(2),server).await.unwrap().unwrap());
        // Retain it until after the server sees closure, just like PC callbacks.
        drop(retained);
    }
    #[tokio::test]
    async fn silent_half_open_socket_is_reported_disconnected() {
        use futures_util::{SinkExt,StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address=listener.local_addr().unwrap();
        let (hold_tx,hold_rx)=tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let (tcp,_)=listener.accept().await.unwrap();
            let mut socket=tokio_tungstenite::accept_async(tcp).await.unwrap();
            // Shortest accepted window (clamped to 5 s); the server then never pings.
            socket.send(Message::Text(r#"0{"pingInterval":1000,"pingTimeout":1000}"#.into())).await.unwrap();
            let _=socket.next().await;
            socket.send(Message::Text("40{}".into())).await.unwrap();
            let _=hold_rx.await; // keep the TCP connection open but silent
        });
        let (sender,mut events)=tokio::sync::mpsc::channel(8);
        let _client=super::SignalingClient::connect(&format!("ws://{address}"),"synthetic-only",sender).await.unwrap();
        let disconnected=tokio::time::timeout(std::time::Duration::from_secs(8),async {
            loop { if matches!(events.recv().await,Some(super::SignalingEvent::Disconnected)|None) { return; } }
        }).await;
        assert!(disconnected.is_ok(),"a socket without pings must be reported dead");
        drop(hold_tx);
    }
    #[tokio::test]
    async fn blocked_event_consumer_does_not_hide_a_dead_socket() {
        use futures_util::{SinkExt,StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address=listener.local_addr().unwrap();
        let (closed_tx,closed_rx)=tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let (tcp,_)=listener.accept().await.unwrap();
            let socket=tokio_tungstenite::accept_async(tcp).await.unwrap();
            let (mut sink,mut stream)=socket.split();
            // Shortest accepted window (clamped to 5 s); never ping, only events.
            sink.send(Message::Text(r#"0{"pingInterval":1000,"pingTimeout":1000}"#.into())).await.unwrap();
            let _=stream.next().await;
            sink.send(Message::Text("40{}".into())).await.unwrap();
            loop {
                tokio::select! {
                    r=stream.next()=>match r {
                        None|Some(Err(_))|Some(Ok(Message::Close(_)))=>{let _=closed_tx.send(());return;}
                        _=>{}
                    },
                    _=tokio::time::sleep(std::time::Duration::from_millis(50))=>{
                        if sink.send(Message::Text(r#"42["bmsg_push",{}]"#.into())).await.is_err() {
                            let _=closed_tx.send(());return;
                        }
                    }
                }
            }
        });
        // Capacity 1 and no reader: the first event fills the queue, so the
        // receive loop would park inside `send().await` without the watchdog.
        let (sender,_events)=tokio::sync::mpsc::channel(1);
        let _client=super::SignalingClient::connect(&format!("ws://{address}"),"synthetic-only",sender).await.unwrap();
        let closed=tokio::time::timeout(std::time::Duration::from_secs(15),closed_rx).await;
        assert!(closed.is_ok(),"a blocked consumer must not keep a dead signaling socket open");
    }
    #[tokio::test]
    async fn stalled_handshake_times_out() {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        // Accepts TCP but never answers the upgrade, like a stalled path after a network change.
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address=listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (_tcp,_)=listener.accept().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        let req=format!("ws://{address}/socket.io/").into_client_request().unwrap();
        let result=tokio::time::timeout(std::time::Duration::from_secs(5),super::open_socket(req,std::time::Duration::from_secs(1))).await;
        assert!(matches!(result,Ok(Err(_))),"a stalled handshake must fail within the connect timeout");
    }

    #[tokio::test]
    async fn stalled_event_delivery_still_reports_disconnected() {
        use futures_util::{SinkExt,StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address=listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (tcp,_)=listener.accept().await.unwrap();
            let socket=tokio_tungstenite::accept_async(tcp).await.unwrap();
            let (mut sink,mut stream)=socket.split();
            // A long window: the send stall must fire before the heartbeat deadline.
            sink.send(Message::Text(r#"0{"pingInterval":30000,"pingTimeout":30000}"#.into())).await.unwrap();
            let _=stream.next().await;
            sink.send(Message::Text("40{}".into())).await.unwrap();
            loop {
                tokio::select! {
                    r=stream.next()=>{ if matches!(r,None|Some(Err(_))|Some(Ok(Message::Close(_)))){return} }
                    _=tokio::time::sleep(std::time::Duration::from_millis(50))=>{
                        if sink.send(Message::Text(r#"42["bmsg_push",{}]"#.into())).await.is_err(){return}
                    }
                }
            }
        });
        let (sender,mut events)=tokio::sync::mpsc::channel(1);
        let _client=super::SignalingClient::connect(&format!("ws://{address}"),"synthetic-only",sender).await.unwrap();
        // Let SEND_STALL_TIMEOUT expire while nothing drains the queue.
        tokio::time::sleep(std::time::Duration::from_secs(12)).await;
        assert!(matches!(events.recv().await,Some(super::SignalingEvent::Connected)));
        let disconnected=tokio::time::timeout(std::time::Duration::from_secs(5),events.recv()).await;
        assert!(matches!(disconnected,Ok(Some(super::SignalingEvent::Disconnected))),"a stalled delivery must still report Disconnected");
    }
}

async fn open_socket(
    req: tokio_tungstenite::tungstenite::handshake::client::Request,
    limit: Duration,
) -> Result<crate::ws_transport::Socket, String> {
    crate::ws_transport::connect(req, limit).await
}

impl SignalingClient {
    pub async fn connect(
        url: &str,
        room_token: &str,
        event_sender: mpsc::Sender<SignalingEvent>,
    ) -> Result<Self, String> {
        let ws_url = format!(
            "{}/socket.io/?transport=websocket&EIO=4",
            url.trim_end_matches('/')
        );

        let mut req = ws_url
            .clone()
            .into_client_request()
            .map_err(|e| format!("Invalid WS request: {}", e))?;

        req.headers_mut().insert(
            "X-NRD-AUTH",
            room_token.parse().map_err(|_| "Invalid token header")?,
        );

        let ws_stream = open_socket(req, CONNECT_TIMEOUT).await?;

        log::info!("Connected to Engine.IO / Socket.IO");
        let (mut write, mut read) = ws_stream.split();
        let (tx_outgoing, mut rx_outgoing) = mpsc::channel::<String>(32);
        let started = Instant::now();
        let last_rx = Arc::new(AtomicU64::new(0));
        let ping_window_ms = Arc::new(AtomicU64::new(45_000));
        let receiver_done = Arc::new(AtomicBool::new(false));

        // 发送协程
        let writer=tokio::spawn(async move {
            while let Some(msg) = rx_outgoing.recv().await {
                match tokio::time::timeout(SEND_STALL_TIMEOUT, write.send(Message::Text(msg))).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) | Err(_) => {
                        // A peer that stopped reading must not park every producer.
                        log::warn!("WS send failed or stalled (details omitted)");
                        break;
                    }
                }
            }
        });
        let writer_abort = writer.abort_handle();

        let tx_out_clone = tx_outgoing.clone();
        static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let generation=GENERATION.fetch_add(1,std::sync::atomic::Ordering::Relaxed);
        let reconnect_key=Arc::new(std::sync::Mutex::new(None));
        let refreshed=reconnect_key.clone();

        // 接收与状态机协程
        let watchdog_events = event_sender.clone();
        let watchdog_last_rx = last_rx.clone();
        let watchdog_ping_window_ms = ping_window_ms.clone();
        let watchdog_done = receiver_done.clone();
        let receiver = tokio::spawn(async move {
            let mut req_counter = 10;
            let mut last_ping=started;
            let mut pings=0u64;
            let mut pong_queued=0u64;
            let mut connected=false;
            let mut next_refresh=tokio::time::Instant::now()+std::time::Duration::from_secs(30);
            let mut refresh_acks=std::collections::VecDeque::new();
            let mut accepted_refresh=0;
            // Socket.IO binary event awaiting attachments: (event name, header JSON).
            let mut pending_binary: Option<(String, Value)> = None;
            // Relation check only: the be-controlled ice_id versus the next offer's data.ice_id.
            let mut last_connect_ice: Option<String> = None;
            let mut last_connect_client: Option<String> = None;
            // Engine.IO 4: a client treats the connection as dead when no ping arrives within
            // pingInterval + pingTimeout. A half-open TCP socket (suspend, NAT drop) otherwise
            // blocks the read forever and the host silently stays offline.
            let mut ping_window=std::time::Duration::from_secs(25+20);
            let mut ping_deadline=tokio::time::Instant::now()+ping_window;
            loop {
                let msg_res=tokio::select! {
                    _=event_sender.closed()=>break,
                    _=tokio::time::sleep_until(ping_deadline) => {
                        log::warn!("Signaling heartbeat lost: generation={} age_s={} pings={} last_ping_gap_s={}",generation,started.elapsed().as_secs(),pings,last_ping.elapsed().as_secs());
                        let _ = send_event(&event_sender, SignalingEvent::Disconnected).await;
                        break;
                    },
                    // Official ControlledSession RefreshSignalTokenLoop: first
                    // 30s, success every ~300s, failure retry after 30s. Native
                    // libstreamer 0x885774 / 0x885e58; emit has NO payload args.
                    _=tokio::time::sleep_until(next_refresh), if connected => {
                        req_counter+=1;
                        refresh_acks.push_back(req_counter);
                        if refresh_acks.len()>16 { refresh_acks.pop_front(); }
                        let packet=format!("42{}[\"refresh_reconnect_key\"]",req_counter);
                        if tx_out_clone.try_send(packet).is_err() {
                            let _=send_event(&event_sender, SignalingEvent::Disconnected).await;break;
                        }
                        next_refresh=tokio::time::Instant::now()+std::time::Duration::from_secs(30);
                        log::info!("Signaling reconnect-key refresh requested: generation={} age_s={}",generation,started.elapsed().as_secs());
                        continue;
                    },
                    msg=read.next()=>match msg {
                        Some(msg)=>msg,
                        None=>{let _=send_event(&event_sender, SignalingEvent::Disconnected).await;break;}
                    },
                };
                // Any inbound frame proves the socket is alive. A reader parked on
                // event delivery stops updating this, which the watchdog catches.
                if msg_res.is_ok() {
                    last_rx.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                }
                match msg_res {
                    Ok(Message::Text(txt)) => {
                        // Engine.IO 报文处理
                        if txt.starts_with('0') {
                            if let Ok(open)=serde_json::from_str::<Value>(&txt[1..]) {
                                log::info!("Signaling heartbeat config: generation={} ping_interval_ms={:?} ping_timeout_ms={:?}",generation,open.get("pingInterval").and_then(Value::as_u64),open.get("pingTimeout").and_then(Value::as_u64));
                                if let (Some(interval),Some(timeout))=(open.get("pingInterval").and_then(Value::as_u64),open.get("pingTimeout").and_then(Value::as_u64)) {
                                    ping_window=std::time::Duration::from_millis(interval.saturating_add(timeout).clamp(5_000,300_000));
                                    ping_window_ms.store(ping_window.as_millis() as u64, Ordering::Relaxed);
                                    ping_deadline=tokio::time::Instant::now()+ping_window;
                                }
                            }
                            // Open packet -> 发送 Socket.IO connect
                            if tx_out_clone.try_send("40".into()).is_err() {
                                let _=send_event(&event_sender, SignalingEvent::Disconnected).await;break;
                            }
                        } else if txt == "2" {
                            // Ping -> 回复 Pong
                            let gap=last_ping.elapsed().as_secs();last_ping=std::time::Instant::now();pings+=1;
                            ping_deadline=tokio::time::Instant::now()+ping_window;
                            if tx_out_clone.try_send("3".into()).is_ok() { pong_queued+=1; }
                            else { let _=send_event(&event_sender, SignalingEvent::Disconnected).await;break; }
                            if pings==1 || pings%12==0 {
                                log::info!("Signaling heartbeat progress: generation={} age_s={} pings={} pong_queued={} gap_s={}",generation,started.elapsed().as_secs(),pings,pong_queued,gap);
                            }
                        } else if txt.starts_with("40") {
                            connected=true;
                            next_refresh=tokio::time::Instant::now()+std::time::Duration::from_secs(30);
                            // Socket.IO connected -> 发送 room_info 请求
                            req_counter += 1;
                            let room_req = format!("42{}[\"room_info\"]", req_counter);
                            if tx_out_clone.try_send(room_req).is_err() {
                                let _=send_event(&event_sender, SignalingEvent::Disconnected).await;break;
                            }
                            if !send_event(&event_sender, SignalingEvent::Connected).await { break; }
                        } else if txt.starts_with("43") {
                            // Socket.IO ack 回调
                            let begin=txt.find('[').unwrap_or(2);
                            let ack_id=txt[2..begin].parse::<u64>().ok();
                            let payload = &txt[begin..];
                            if let Ok(v) = serde_json::from_str::<Value>(payload) {
                                if let Some(id)=ack_id.filter(|id|refresh_acks.contains(id)) {
                                    refresh_acks.retain(|pending|*pending!=id);
                                    let key=refreshed_key(&v);
                                    let success=key.is_some();
                                    // Accept a delayed retry response, but never replace
                                    // a newer key with an older successful response.
                                    if success && id>accepted_refresh {
                                        accepted_refresh=id;
                                        *refreshed.lock().unwrap()=key;
                                        next_refresh=tokio::time::Instant::now()+std::time::Duration::from_secs(300);
                                    }
                                    log::info!("Signaling reconnect-key refresh acknowledged: generation={} age_s={} success={}",generation,started.elapsed().as_secs(),success);
                                } else if !send_event(&event_sender, SignalingEvent::RoomInfoAck(v)).await { break; }
                            }
                        } else if txt.starts_with("45") || txt.starts_with("46") {
                            // Socket.IO binary event/ack: header text, attachments follow as binary frames.
                            let body = txt.trim_start_matches(|c: char| c.is_ascii_digit() || c == '-');
                            let name = serde_json::from_str::<Value>(body).ok()
                                .and_then(|v| v.get(0).and_then(|n| n.as_str()).map(str::to_owned))
                                .unwrap_or_default();
                            let name: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').take(48).collect();
                            log::info!("Socket.IO binary packet: kind={} event={} header_bytes={}", &txt[..2], name, txt.len());
                            let header = serde_json::from_str::<Value>(body).ok().and_then(|v| v.get(1).cloned()).unwrap_or(Value::Null);
                            pending_binary = Some((name, header));
                        } else if txt.starts_with("42") {
                            // Socket.IO event
                            let payload = &txt[2..];
                            log::debug!("Socket.IO event received ({} bytes)", payload.len());
                            if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(payload) {
                                if arr.len() >= 2 {
                                    let event_name = arr[0].as_str().unwrap_or_default();
                                    let data = &arr[1];
                                    log::info!("Socket.IO event dispatch: name={}", event_name);

                                    match event_name {
                                        "bmsg_push" => {
                                            if !send_event(&event_sender, SignalingEvent::DeviceInfoChanged(data.clone())).await { break; }
                                        }
                                        "be-controlled" => {
                                            if !send_event(&event_sender, SignalingEvent::BeControlled(data.clone())).await { break; }
                                        }
                                        "soac" => {
                                            // Offers only: structure (keys, types, lengths), never SDP text or ids.
                                            let kind = data.get("data").unwrap_or(data).get("type").and_then(Value::as_str);
                                            if kind == Some("offer") {
                                                log::info!("SOAC offer shape: {}", crate::protocol::json_shape(data, 3));
                                                let offer_ice = data.get("data").unwrap_or(data).get("ice_id").and_then(Value::as_str);
                                                log::info!("SOAC offer ice_id equals be-controlled ice_id: {:?}", last_connect_ice.as_deref().zip(offer_ice).map(|(a, b)| a == b));
                                                let offer_client = data.get("client_id").and_then(Value::as_str);
                                                log::info!("SOAC offer client_id equals be-controlled client_id: {:?}", last_connect_client.as_deref().zip(offer_client).map(|(a, b)| a == b));
                                            }
                                            let (target_val, client_id_opt) = if let Some(inner) = data.get("data") {
                                                (inner.clone(), data.get("client_id").and_then(|v| v.as_str()).map(|s| s.to_string()))
                                            } else {
                                                (data.clone(), None)
                                            };

                                            match serde_json::from_value::<SoacMessage>(target_val.clone()) {
                                                Ok(mut soac) => {
                                                    if soac.client_id.is_none() {
                                                        soac.client_id = client_id_opt;
                                                    }
                                                    if !send_event(&event_sender, SignalingEvent::Soac(soac)).await { break; }
                                                }
                                                Err(e) => {
                                                    let _ = e;
                                                    log::error!("Failed to deserialize SoacMessage (payload omitted)");
                                                }
                                            }
                                        }
                                        "released" => {
                                            log::info!("Cloud peer event: generation={} event=released age_s={} pings={} last_ping_gap_s={} ice_id_present={}",generation,started.elapsed().as_secs(),pings,last_ping.elapsed().as_secs(),data.get("ice_id").is_some());
                                            if !send_event(&event_sender, SignalingEvent::Released(data.clone())).await { break; }
                                        }
                                        "left" | "leave" => {
                                            log::info!("Cloud peer event: generation={} event={} age_s={} pings={} last_ping_gap_s={} ice_id_present={}",generation,event_name,started.elapsed().as_secs(),pings,last_ping.elapsed().as_secs(),data.get("ice_id").is_some());
                                            let ice_id = data.get("ice_id").and_then(|v| v.as_str()).unwrap_or("");
                                            if ice_id.is_empty() {
                                                log::info!("Cloud room leave shape: {}", crate::protocol::json_shape(data, 3));
                                                log::warn!("Cloud room leave without peer identifier; renewing signaling (expiry cause unknown)");
                                                if !send_event(&event_sender, SignalingEvent::Disconnected).await { break; }
                                            } else {
                                                if !send_event(&event_sender, SignalingEvent::Left(data.clone())).await { break; }
                                            }
                                        }
                                        _ => {
                                            let keys: Vec<String> = data.as_object().map(|o| o.keys().take(16).cloned().collect()).unwrap_or_default();
                                            log::info!("Unhandled Socket.IO event: {} keys={:?}", event_name, keys);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Ok(Message::Binary(data)) => {
                        // Engine.IO v4 sends attachments raw; tolerate a legacy 0x04 prefix.
                        let data: &[u8] = if data.first() == Some(&4) && crate::protocol::decode_wire(&data).is_err() { &data[1..] } else { &data };
                        let fields = crate::protocol::decode_wire(data).ok();
                        let shape: Vec<(u32, Option<u64>)> = fields.as_ref().map(|f| f.iter().map(|x| (x.number, match x.value {
                            crate::protocol::WireValue::Varint(v) => Some(v), _ => None })).collect()).unwrap_or_default();
                        log::info!("Socket.IO binary attachment: bytes={} protobuf_fields={:?}", data.len(), shape);
                        if let Some((name, header)) = pending_binary.take() {
                            if name == "be-controlled" {
                                let capture_type = shape.iter().find(|(n, _)| *n == 1).and_then(|(_, v)| *v).unwrap_or(0);
                                let keys: Vec<String> = header.as_object().map(|o| o.keys().take(24).cloned().collect()).unwrap_or_default();
                                let ice_id = ["ice_id", "iceId"].iter().find_map(|k| header.get(*k).or_else(|| header.get("data").and_then(|d| d.get(*k))))
                                    .and_then(|v| v.as_str()).map(str::to_owned);
                                log::info!("be-controlled connect request: capture_type={} header_keys={:?} ice_id_present={}", capture_type, keys, ice_id.is_some());
                                last_connect_ice = ice_id.clone();
                                last_connect_client = header.get("client_id").and_then(Value::as_str).map(str::to_owned);
                                // Structure only (numbers, types, lengths): documents what the viewer and cloud send.
                                let header_shape: String = crate::protocol::json_shape(&header, 7).chars().take(8000).collect();
                                log::info!("Connect request shape: app_data={} header={}", crate::protocol::wire_shape(data, 3), header_shape);
                                // Relation only, never the ids themselves.
                                let control_id = header.get("streamer_data").and_then(Value::as_str)
                                    .and_then(|t| serde_json::from_str::<Value>(t).ok())
                                    .and_then(|v| v.get("control_id").and_then(Value::as_str).map(str::to_owned));
                                let app_control_id = header.get("app_control_id").and_then(Value::as_str);
                                log::info!("Connect request ids: streamer_control_id_equals_app_control_id={:?}",
                                    control_id.as_deref().zip(app_control_id).map(|(a, b)| a == b));
                                // ConnectOptions 4 decoder_cap_list: DecoderCap {1 fps, 2 codec_type (1 H264, 2 H265),
                                // 3 width, 4 height, 5 chroma_format (1 4:2:0, 3 4:4:4)}. Numbers only.
                                let caps: Vec<Vec<(u32, u64)>> = fields.iter().flatten().filter(|f| f.number == 4).filter_map(|f| match f.value {
                                    crate::protocol::WireValue::Bytes(b) => crate::protocol::decode_wire(b).ok().map(|cap| cap.iter().filter_map(|x| match x.value {
                                        crate::protocol::WireValue::Varint(v) => Some((x.number, v)), _ => None }).collect()),
                                    _ => None }).collect();
                                if !caps.is_empty() { log::info!("Viewer decoder caps (fps,codec,w,h,chroma fields): {:?}", caps); }
                                // Viewer platform (website enum: 1 windows, 2 android, 3 ios, 4 macos, 5 tv).
                                let platform = header.get("controller_platform").or_else(|| header.get("data").and_then(|d| d.get("controller_platform")));
                                let controller = crate::input::Controller::from_platform(platform);
                                log::info!("Controller platform: {} controller={:?}", platform.map(|v| v.to_string()).unwrap_or_default().chars().take(24).collect::<String>(), controller);
                                if !send_event(&event_sender, SignalingEvent::ConnectRequest { capture_type, ice_id, transport:crate::session_ice::SessionIce::from_header(&header), controller }).await { break; }
                            }
                        }
                    }
                    Ok(Message::Close(frame)) => {
                        log::warn!("Signaling socket closed: generation={} age_s={} close_code={:?} pings={} last_ping_gap_s={}",generation,started.elapsed().as_secs(),frame.map(|f|u16::from(f.code)),pings,last_ping.elapsed().as_secs());
                        let _ = send_event(&event_sender, SignalingEvent::Disconnected).await;
                        break;
                    }
                    Err(_) => {
                        log::warn!("Signaling socket receive failed: generation={} age_s={} pings={} last_ping_gap_s={}",generation,started.elapsed().as_secs(),pings,last_ping.elapsed().as_secs());
                        let _ = send_event(&event_sender, SignalingEvent::Disconnected).await;
                        break;
                    }
                    _ => {}
                }
            }
            // A stalled event queue or a writer that already stopped can end this
            // loop without a delivered Disconnected. Hand one off from a detached
            // task so the main loop always learns the room is dead; duplicates are
            // harmless because it breaks on the first one.
            let events = event_sender.clone();
            tokio::spawn(async move { let _ = events.send(SignalingEvent::Disconnected).await; });
            receiver_done.store(true, Ordering::Relaxed);
            log::info!("Signaling receiver stopped: generation={} age_s={}",generation,started.elapsed().as_secs());
            // Live peer callbacks can retain the old SignalingClient sender.
            // Explicitly drop its write half too when this room stops reading.
            writer.abort();
        });
        let receiver_abort = receiver.abort_handle();

        // Independent of the receive loop: a consumer that stops draining events
        // parks the loop inside an `.await`, where the ping deadline can no longer
        // be observed. The watchdog closes the socket anyway.
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(WATCHDOG_TICK);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                if watchdog_done.load(Ordering::Relaxed) || watchdog_events.is_closed() { return; }
                let window_ms = watchdog_ping_window_ms.load(Ordering::Relaxed);
                let idle_ms = (started.elapsed().as_millis() as u64)
                    .saturating_sub(watchdog_last_rx.load(Ordering::Relaxed));
                if idle_ms <= window_ms { continue; }
                log::warn!("Signaling heartbeat lost (watchdog): idle_s={} window_ms={}", idle_ms/1000, window_ms);
                // Deliver from a detached task: the receiver may be the one parked
                // on a full queue, and the main loop still needs this event.
                let events = watchdog_events.clone();
                tokio::spawn(async move { let _ = events.send(SignalingEvent::Disconnected).await; });
                receiver_abort.abort();
                writer_abort.abort();
                return;
            }
        });

        Ok(Self { tx_outgoing, _reconnect_key:reconnect_key })
    }

    pub async fn emit_event(&self, event: &str, payload: serde_json::Value) -> Result<(), String> {
        let packet = format!("42{}", serde_json::json!([event, payload]));
        log::info!("Emitting signaling event: {}", event);
        tokio::time::timeout(SEND_STALL_TIMEOUT, self.tx_outgoing.send(packet))
            .await
            .map_err(|_| "signaling send stalled".to_string())?
            .map_err(|e| e.to_string())
    }

    /// 发送 SOAC 消息（Answer 或 Candidate）
    pub async fn emit_soac(&self, soac: SoacMessage) -> Result<(), String> {
        let payload = if let Some(cid) = &soac.client_id {
            serde_json::json!({
                "client_id": cid,
                "data": soac
            }).to_string()
        } else {
            serde_json::to_string(&soac).map_err(|e| e.to_string())?
        };
        let msg = format!("42[\"soac\",{}]", payload);
        log::info!("Emitting soac packet: type={}", soac.msg_type);
        tokio::time::timeout(SEND_STALL_TIMEOUT, self.tx_outgoing.send(msg))
            .await
            .map_err(|_| "Send soac timed out".to_string())?
            .map_err(|e| format!("Send soac error: {}", e))
    }
}
