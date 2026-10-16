//! Lightweight Native Engine.IO v4 / Socket.IO Signaling Client

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

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
    ConnectRequest { capture_type: u64, ice_id: Option<String>, transport:crate::session_ice::SessionIce, controller_mac: bool },
    Soac(SoacMessage),
    Released(Value),
    Left(Value),
    Disconnected,
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

        let (ws_stream, _) = connect_async(req)
            .await
            .map_err(|e| format!("WS connect failed: {}", e))?;

        log::info!("Connected to Engine.IO / Socket.IO");
        let (mut write, mut read) = ws_stream.split();
        let (tx_outgoing, mut rx_outgoing) = mpsc::channel::<String>(32);

        // 发送协程
        let writer=tokio::spawn(async move {
            while let Some(msg) = rx_outgoing.recv().await {
                if let Err(e) = write.send(Message::Text(msg)).await {
                    let _ = e;
                    log::warn!("WS send failed (details omitted)");
                    break;
                }
            }
        });

        let tx_out_clone = tx_outgoing.clone();
        static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let generation=GENERATION.fetch_add(1,std::sync::atomic::Ordering::Relaxed);
        let reconnect_key=Arc::new(std::sync::Mutex::new(None));
        let refreshed=reconnect_key.clone();

        // 接收与状态机协程
        tokio::spawn(async move {
            let mut req_counter = 10;
            let started=std::time::Instant::now();
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
                        let _ = event_sender.send(SignalingEvent::Disconnected).await;
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
                        if tx_out_clone.send(packet).await.is_err() {
                            let _=event_sender.send(SignalingEvent::Disconnected).await;break;
                        }
                        next_refresh=tokio::time::Instant::now()+std::time::Duration::from_secs(30);
                        log::info!("Signaling reconnect-key refresh requested: generation={} age_s={}",generation,started.elapsed().as_secs());
                        continue;
                    },
                    msg=read.next()=>match msg {
                        Some(msg)=>msg,
                        None=>{let _=event_sender.send(SignalingEvent::Disconnected).await;break;}
                    },
                };
                match msg_res {
                    Ok(Message::Text(txt)) => {
                        // Engine.IO 报文处理
                        if txt.starts_with('0') {
                            if let Ok(open)=serde_json::from_str::<Value>(&txt[1..]) {
                                log::info!("Signaling heartbeat config: generation={} ping_interval_ms={:?} ping_timeout_ms={:?}",generation,open.get("pingInterval").and_then(Value::as_u64),open.get("pingTimeout").and_then(Value::as_u64));
                                if let (Some(interval),Some(timeout))=(open.get("pingInterval").and_then(Value::as_u64),open.get("pingTimeout").and_then(Value::as_u64)) {
                                    ping_window=std::time::Duration::from_millis(interval.saturating_add(timeout).clamp(5_000,300_000));
                                    ping_deadline=tokio::time::Instant::now()+ping_window;
                                }
                            }
                            // Open packet -> 发送 Socket.IO connect
                            let _ = tx_out_clone.send("40".into()).await;
                        } else if txt == "2" {
                            // Ping -> 回复 Pong
                            let gap=last_ping.elapsed().as_secs();last_ping=std::time::Instant::now();pings+=1;
                            ping_deadline=tokio::time::Instant::now()+ping_window;
                            if tx_out_clone.send("3".into()).await.is_ok() { pong_queued+=1; }
                            if pings==1 || pings%12==0 {
                                log::info!("Signaling heartbeat progress: generation={} age_s={} pings={} pong_queued={} gap_s={}",generation,started.elapsed().as_secs(),pings,pong_queued,gap);
                            }
                        } else if txt.starts_with("40") {
                            connected=true;
                            next_refresh=tokio::time::Instant::now()+std::time::Duration::from_secs(30);
                            // Socket.IO connected -> 发送 room_info 请求
                            req_counter += 1;
                            let room_req = format!("42{}[\"room_info\"]", req_counter);
                            let _ = tx_out_clone.send(room_req).await;
                            let _ = event_sender.send(SignalingEvent::Connected).await;
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
                                } else { let _ = event_sender.send(SignalingEvent::RoomInfoAck(v)).await; }
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
                                            let _ = event_sender
                                                .send(SignalingEvent::DeviceInfoChanged(data.clone()))
                                                .await;
                                        }
                                        "be-controlled" => {
                                            let _ = event_sender
                                                .send(SignalingEvent::BeControlled(data.clone()))
                                                .await;
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
                                                    let _ = event_sender
                                                        .send(SignalingEvent::Soac(soac))
                                                        .await;
                                                }
                                                Err(e) => {
                                                    let _ = e;
                                                    log::error!("Failed to deserialize SoacMessage (payload omitted)");
                                                }
                                            }
                                        }
                                        "released" => {
                                            log::info!("Cloud peer event: generation={} event=released age_s={} pings={} last_ping_gap_s={} ice_id_present={}",generation,started.elapsed().as_secs(),pings,last_ping.elapsed().as_secs(),data.get("ice_id").is_some());
                                            let _ = event_sender
                                                .send(SignalingEvent::Released(data.clone()))
                                                .await;
                                        }
                                        "left" | "leave" => {
                                            log::info!("Cloud peer event: generation={} event={} age_s={} pings={} last_ping_gap_s={} ice_id_present={}",generation,event_name,started.elapsed().as_secs(),pings,last_ping.elapsed().as_secs(),data.get("ice_id").is_some());
                                            let ice_id = data.get("ice_id").and_then(|v| v.as_str()).unwrap_or("");
                                            if ice_id.is_empty() {
                                                log::warn!("Cloud room leave without peer identifier; renewing signaling (expiry cause unknown)");
                                                let _ = event_sender.send(SignalingEvent::Disconnected).await;
                                            } else {
                                                let _ = event_sender
                                                    .send(SignalingEvent::Left(data.clone()))
                                                    .await;
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
                                let controller_mac = platform.is_some_and(|v| v.as_i64() == Some(4)
                                    || v.as_str().is_some_and(|s| s == "4" || s.to_ascii_lowercase().contains("mac") || s.to_ascii_lowercase().contains("darwin")));
                                log::info!("Controller platform: {} mac={}", platform.map(|v| v.to_string()).unwrap_or_default().chars().take(24).collect::<String>(), controller_mac);
                                let _ = event_sender.send(SignalingEvent::ConnectRequest { capture_type, ice_id, transport:crate::session_ice::SessionIce::from_header(&header), controller_mac }).await;
                            }
                        }
                    }
                    Ok(Message::Close(frame)) => {
                        log::warn!("Signaling socket closed: generation={} age_s={} close_code={:?} pings={} last_ping_gap_s={}",generation,started.elapsed().as_secs(),frame.map(|f|u16::from(f.code)),pings,last_ping.elapsed().as_secs());
                        let _ = event_sender.send(SignalingEvent::Disconnected).await;
                        break;
                    }
                    Err(_) => {
                        log::warn!("Signaling socket receive failed: generation={} age_s={} pings={} last_ping_gap_s={}",generation,started.elapsed().as_secs(),pings,last_ping.elapsed().as_secs());
                        let _ = event_sender.send(SignalingEvent::Disconnected).await;
                        break;
                    }
                    _ => {}
                }
            }
            log::info!("Signaling receiver stopped: generation={} age_s={}",generation,started.elapsed().as_secs());
            // Live peer callbacks can retain the old SignalingClient sender.
            // Explicitly drop its write half too when this room stops reading.
            writer.abort();
        });

        Ok(Self { tx_outgoing, _reconnect_key:reconnect_key })
    }

    pub async fn emit_event(&self, event: &str, payload: serde_json::Value) -> Result<(), String> {
        let packet = format!("42{}", serde_json::json!([event, payload]));
        log::info!("Emitting signaling event: {}", event);
        self.tx_outgoing.send(packet).await.map_err(|e| e.to_string())
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
        self.tx_outgoing
            .send(msg)
            .await
            .map_err(|e| format!("Send soac error: {}", e))
    }
}
