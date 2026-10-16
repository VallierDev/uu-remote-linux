//! Per-connection cloud ICE configuration. Never derive Debug or log credentials.
use serde_json::Value;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::{configuration::RTCConfiguration,policy::ice_transport_policy::RTCIceTransportPolicy};

#[derive(Clone, Default)]
pub struct SessionIce {
    servers: Vec<RTCIceServer>,
    relay_only: bool,
}

impl SessionIce {
    #[cfg(test)]
    pub fn local() -> Self {
        // A supplied empty URL list disables the public STUN defaults in local peer tests.
        Self { servers: vec![RTCIceServer::default()], relay_only: false }
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
}
