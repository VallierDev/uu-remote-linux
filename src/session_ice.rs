//! Per-connection cloud ICE configuration. Never derive Debug or log credentials.
use serde_json::Value;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::{configuration::RTCConfiguration,policy::ice_transport_policy::RTCIceTransportPolicy};

#[derive(Clone, Default)]
pub struct SessionIce {
    servers: Vec<RTCIceServer>,
    relay_only: bool,
    #[cfg(test)]
    pub(crate) setting_engine: Option<webrtc::api::setting_engine::SettingEngine>,
}

impl SessionIce {
    #[cfg(test)]
    pub fn local() -> Self {
        // A supplied empty URL list disables the public STUN defaults in local peer tests.
        Self { servers: vec![RTCIceServer::default()], ..Default::default() }
    }

    #[cfg(test)]
    pub fn virtual_net(setting_engine: webrtc::api::setting_engine::SettingEngine) -> Self {
        Self { setting_engine: Some(setting_engine), ..Self::local() }
    }

    pub fn from_header(header:&Value)->Self {
        let data=header.get("data").unwrap_or(header);
        let field=|name:&str|header.get(name).or_else(||data.get(name));
        let mut result=Self { relay_only:field("force_relay").and_then(Value::as_bool).unwrap_or(false),..Default::default() };
        if let Some(servers)=field("iceServers").and_then(Value::as_array) {
            for server in servers.iter().take(16) {
                let urls=server.get("urls").or_else(||server.get("url"));
                let urls:Vec<String>=match urls {
                    Some(Value::String(url))=>vec![url.clone()],
                    Some(Value::Array(urls))=>urls.iter().filter_map(Value::as_str).take(16).map(str::to_owned).collect(),
                    _=>Vec::new(),
                }.into_iter().filter(|url|url.len()<=2048 && ["stun:","stuns:","turn:","turns:"].iter().any(|prefix|url.starts_with(prefix))).collect();
                if urls.is_empty() { continue; }
                let string=|name|server.get(name).and_then(Value::as_str).filter(|s|s.len()<=4096).unwrap_or("").to_owned();
                result.servers.push(RTCIceServer { urls,username:string("username"),credential:string("credential"),
                    credential_type:webrtc::ice_transport::ice_credential_type::RTCIceCredentialType::Password });
            }
        }
        result
    }
    pub fn configuration(self)->Result<RTCConfiguration,String> {
        let has_turn=self.servers.iter().any(|server|server.urls.iter().any(|url|url.starts_with("turn:")||url.starts_with("turns:")));
        if self.relay_only && !has_turn { return Err("Relay-only session requested without usable TURN configuration".into()); }
        let supplied=!self.servers.is_empty();
        let servers=if supplied { self.servers } else { vec![RTCIceServer {
            urls:vec!["stun:stun.l.google.com:19302".into(),"stun:stun1.l.google.com:19302".into(),"stun:stun.miwifi.com:3478".into()],..Default::default()
        }] };
        log::info!("Session ICE configuration: cloud_supplied={} server_count={} turn_available={} relay_only={}",supplied,servers.len(),has_turn,self.relay_only);
        Ok(RTCConfiguration { ice_servers:servers,
            ice_transport_policy:if self.relay_only { RTCIceTransportPolicy::Relay } else { RTCIceTransportPolicy::All },..Default::default() })
    }
}

/// With `UU_USE_PROXY=1`, also gather TURN relays over TCP/TLS (through the HTTP proxy when
/// one is configured); hosts whose UDP cannot reach the TURN servers then still get a relay.
pub fn configure_relay_transport() {
    if !crate::auth::proxy_enabled() { return; }
    let value = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"].iter().find_map(|name| std::env::var(name).ok()).filter(|v| !v.is_empty());
    let proxy = match value.as_deref().map(relay_proxy) {
        Some(Ok(proxy)) => Some(proxy),
        Some(Err(reason)) => { log::warn!("TURN over TCP/TLS disabled: {reason}"); return; }
        None => None,
    };
    log::info!("TURN over TCP/TLS enabled: via_http_proxy={}", proxy.is_some());
    webrtc::ice::agent::turn_tcp::set_relay_transport(webrtc::ice::agent::turn_tcp::RelayTransport { http_proxy: proxy });
}

/// `host:port` of an unauthenticated HTTP proxy URL; never echoes the value.
fn relay_proxy(value: &str) -> Result<String, &'static str> {
    let url = reqwest::Url::parse(&if value.contains("://") { value.to_owned() } else { format!("http://{value}") })
        .map_err(|_| "invalid proxy URL")?;
    if url.scheme() != "http" { return Err("only http:// proxies are supported for TURN"); }
    if !url.username().is_empty() || url.password().is_some() { return Err("proxy authentication is not supported for TURN"); }
    let host = url.host_str().ok_or("invalid proxy URL")?;
    let port = url.port_or_known_default().ok_or("invalid proxy URL")?;
    Ok(format!("{host}:{port}")) // host_str() keeps IPv6 brackets
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cloud_credentials_are_scoped_to_peer_and_relay_policy_is_honored() {
        let config=SessionIce::from_header(&serde_json::json!({"data":{"iceServers":[{"urls":["turn:example.invalid:3478?transport=udp"],"username":"test-user","credential":"test-secret"}],"force_relay":true}})).configuration().unwrap();
        assert_eq!(config.ice_transport_policy,RTCIceTransportPolicy::Relay);
        assert_eq!(config.ice_servers[0].username,"test-user");
        assert_eq!(config.ice_servers[0].credential,"test-secret");
    }
    #[test]
    fn unsupported_server_urls_are_rejected_and_relay_never_silently_falls_back() {
        assert!(SessionIce::from_header(&serde_json::json!({"force_relay":true,"iceServers":[{"urls":"https://example.invalid"}]})).configuration().is_err());
        let config=SessionIce::default().configuration().unwrap();
        assert_eq!(config.ice_transport_policy,RTCIceTransportPolicy::All);
        assert_eq!(config.ice_servers.len(),1);
        assert!(config.ice_servers[0].urls.iter().all(|s|s.starts_with("stun:")));
    }

    #[test]
    fn relay_proxy_accepts_plain_http_only() {
        assert_eq!(super::relay_proxy("http://proxy.invalid:8889").unwrap(), "proxy.invalid:8889");
        assert_eq!(super::relay_proxy("proxy.invalid:8889/").unwrap(), "proxy.invalid:8889");
        assert_eq!(super::relay_proxy("http://[::1]:3128").unwrap(), "[::1]:3128");
        for bad in ["socks5://proxy.invalid:1080", "https://proxy.invalid:443", "http://user:pass@proxy.invalid:8889", "::"] {
            assert!(super::relay_proxy(bad).is_err());
        }
    }

    #[test]
    fn turn_tcp_frames_stun_and_padded_channel_data() {
        use webrtc::ice::agent::turn_tcp::frame_len;
        assert_eq!(frame_len([0x01, 0x01, 0x00, 0x08]), Some(28));
        assert_eq!(frame_len([0x40, 0x00, 0x00, 0x05]), Some(12));
        assert_eq!(frame_len([0x40, 0x00, 0x00, 0x08]), Some(12));
        assert_eq!(frame_len([0x80, 0x00, 0x00, 0x00]), None);
        assert!(webrtc::ice::agent::turn_tcp::is_http_reply(*b"HTTP"));
        assert!(!webrtc::ice::agent::turn_tcp::is_http_reply([0x40, 0x00, 0x00, 0x05]));
    }

    #[tokio::test]
    async fn turn_tcp_connects_through_the_proxy_and_frames_the_stream() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use webrtc::ice::agent::turn_tcp::{dial, set_relay_transport, RelayTransport};
        use webrtc::util::Conn;
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = proxy.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") { let mut b = [0u8; 1]; socket.read_exact(&mut b).await.unwrap(); head.push(b[0]); }
            assert!(String::from_utf8_lossy(&head).starts_with("CONNECT 192.0.2.1:2483 HTTP/1.1\r\n"));
            socket.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").await.unwrap();
            let mut frames = [0u8; 20 + 12];
            socket.read_exact(&mut frames).await.unwrap();
            socket.write_all(&frames).await.unwrap();
        });
        set_relay_transport(RelayTransport { http_proxy: Some(proxy_addr.to_string()) });
        let target: std::net::SocketAddr = "192.0.2.1:2483".parse().unwrap();
        let conn = dial("192.0.2.1", 2483, target, false).await.unwrap();
        let mut stun = [0u8; 20]; stun[1] = 0x01;
        let channel = [0x40, 0x00, 0x00, 0x05, 1, 2, 3, 4, 5, 0, 0, 0];
        conn.send_to(&stun, target).await.unwrap();
        conn.send_to(&channel, target).await.unwrap();
        let mut buf = [0u8; 64];
        assert_eq!(conn.recv_from(&mut buf).await.unwrap(), (20, target));
        assert_eq!(conn.recv_from(&mut buf).await.unwrap(), (12, target));
        assert_eq!(&buf[..12], &channel);
        server.await.unwrap();
    }
}
