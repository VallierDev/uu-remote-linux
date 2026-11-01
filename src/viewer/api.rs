use crate::auth::UUAuthClient;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

// No Debug: room/report tokens and signaling URLs are credentials.
#[derive(Deserialize)]
pub struct JoinData {
    #[serde(default)] pub token: String,
    #[serde(default)] pub signaling_list: Vec<String>,
    #[serde(default)] pub ws_connect_timeout_ms: i32,
    pub report_token: String,
    pub report_url: String,
    #[serde(default)] pub report_server_address: String,
    #[serde(default)] pub streamer_retry_delta_ms: i32,
    #[serde(default)] pub international_connect: bool,
    #[serde(default)] pub publisher_platform: Option<i32>,
}
fn device_platform(data:&Value,device:&str)->Option<i64>{
    ["my_binded_devices","others_shared_devices"].into_iter()
        .filter_map(|key|data.get(key).and_then(Value::as_array)).flatten()
        .find(|d|d.get("device_id").and_then(Value::as_str)==Some(device))
        .and_then(|d|d.get("platform")).and_then(Value::as_i64)
}
/// English for server messages observed in assistance and connection replies.
fn english_message(msg: &str) -> Option<&'static str> {
    Some(match msg {
        "验证码错误" => "Wrong verification code",
        "对方未开启协助" => "The partner has not turned on remote assistance",
        "无法对同账号的设备发起远程协助" => "Remote assistance between devices of the same account is not possible",
        "被控端正在被远控中，无法发起连接" => "This computer is already being controlled from another device",
        _ => return None,
    })
}
#[derive(Debug, PartialEq, Eq)]
pub enum Error { AuthenticationExpired, Api(i32, String), InvalidResponse, Transport, MissingRoom, InvalidDevice, InvalidAssist, UnknownKeyboardPlatform }
impl std::fmt::Display for Error {
    // The server's short message is user-facing (e.g. 2002 「被控端正在被远控中，无法发起连接」).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Observed 2002: the target already has a controller (another viewer, a phone).
            Error::Api(2002, _) => f.write_str(crate::i18n::s("被控端正在被远控中，无法发起连接（错误码 2002）",
                "This computer is already being controlled from another device; disconnect there and try again (error 2002)")),
            // Server messages are Chinese only: the English UI translates the known ones.
            Error::Api(code, msg) if !msg.is_empty() && !crate::i18n::english() => write!(f, "{msg} (code {code})"),
            Error::Api(code, msg) if english_message(msg).is_some() => write!(f, "{} (code {code})", english_message(msg).unwrap_or_default()),
            Error::AuthenticationExpired => f.write_str(crate::i18n::s("登录已过期，请重新登录","Sign-in expired, please sign in again")),
            Error::Transport => f.write_str(crate::i18n::s("网络连接失败","Network connection failed")),
            Error::InvalidAssist => f.write_str(crate::i18n::s("请输入对方的设备 ID 和有效验证码","Enter the partner's device ID and a valid code")),
            Error::InvalidResponse => f.write_str(crate::i18n::s("服务器响应格式无效","Invalid server response")),
            Error::MissingRoom => f.write_str(crate::i18n::s("服务器未返回可用的房间或信令信息","The server returned no usable room or signaling info")),
            Error::UnknownKeyboardPlatform => f.write_str(crate::i18n::s("无法确定目标键盘平台，不能启用控制","Unknown remote keyboard platform; control is disabled")),
            Error::Api(code, _) => f.write_str(&crate::tr!("连接请求失败（错误码 {code}）","Connection request failed (error {code})")),
            _ => write!(f, "viewer API: {self:?}"),
        }
    }
}

// Mirrors Windows 0x140b77cd0 / 0x140b7a6d0, not host RoomData.
/// Treat the caller's device string as one opaque path segment, never a URL/query.
fn path_segment(device: &str) -> Result<String, Error> {
    if device.is_empty() || device=="." || device==".." { return Err(Error::InvalidDevice); }
    Ok(device.bytes().map(|b|if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {(b as char).to_string()}else{format!("%{b:02X}")}).collect())
}
fn parse_ack(status: u16, body: Value) -> Result<(), Error> {
    let code = body.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code == 1120 { return Err(Error::AuthenticationExpired); }
    if status == 200 && code == 0 { return Ok(()); }
    let msg = body.get("msg").and_then(Value::as_str).unwrap_or("").chars().take(120).collect();
    Err(Error::Api(i32::try_from(code).unwrap_or(-1), msg))
}
pub fn parse_join(status: u16, value: Value) -> Result<JoinData, Error> {
    let code = value.get("code").and_then(Value::as_i64).and_then(|v| i32::try_from(v).ok()).ok_or(Error::InvalidResponse)?;
    let msg = value.get("msg").and_then(Value::as_str).ok_or(Error::InvalidResponse)?.chars().take(120).collect::<String>();
    if code == 1120 { return Err(Error::AuthenticationExpired); }
    let code = if code == 0 && status != 200 { i32::from(status) } else { code };
    // Error responses carry an empty data object (observed: 400 / 2002 with data {}); report the code.
    if code != 0 { return Err(Error::Api(code, msg)); }
    let data = match value.get("data") {
        Some(Value::Object(o)) => Some(serde_json::from_value::<JoinData>(Value::Object(o.clone())).map_err(|_| Error::InvalidResponse)?),
        _ => None,
    };
    data.ok_or(Error::MissingRoom)
}
fn parse_share(status: u16, mut body: Value, secrets: &[&str]) -> Result<JoinData, Error> {
    if let Some(Value::String(msg)) = body.get_mut("msg") {
        // A server error must not echo credentials to stderr or the launcher.
        for secret in secrets {
            if !secret.is_empty() { *msg = msg.replace(secret, "[已隐藏]"); }
        }
        *msg = msg.chars().filter(|c| !c.is_control()).collect();
    }
    let room = parse_join(status, body)?;
    if room.token.is_empty() || room.signaling_list.is_empty() { return Err(Error::MissingRoom); }
    Ok(room)
}
pub fn retry_delay(error: &Error, retries: usize, enabled: bool) -> Option<Duration> {
    match error {
        Error::Api(code, _) if enabled && retries < 5 && ![-1, 2002, 2006, 2007, 4042].contains(code) => Some(Duration::from_millis(3000)),
        _ => None,
    }
}

pub struct Api {
    signer: UUAuthClient,
    http: reqwest::Client,
    base: String,
}
impl Api {
    pub fn new(signer: UUAuthClient, base: &str) -> Self {
        Self { signer, base: base.trim_end_matches('/').into(), http: crate::auth::http_client_builder().timeout(Duration::from_secs(12)).build().unwrap() }
    }
    pub async fn join_share(&self, connect_id: &str, connect_code: &str) -> Result<JoinData, Error> {
        if connect_id.trim().is_empty() || connect_code.trim().is_empty() || connect_id.chars().chain(connect_code.chars()).any(char::is_control) { return Err(Error::InvalidAssist); }
        // Win LiteInner 0x140b6db90 sends plaintext connect_code; Mac uses the v1 path.
        let text = json!({"connect_id":connect_id,"connect_code":connect_code}).to_string();
        for version in [2, 1] {
            let path = format!("/api/v{version}/room/join/share/by_code");
            let response = self.http.post(format!("{}{path}", self.base))
                .headers(self.signer.build_headers("POST", &path, &text)).body(text.clone())
                .send().await.map_err(|_| Error::Transport)?;
            let status = response.status().as_u16();
            log::info!("Assist join response: status={status}");
            // A missing endpoint may return HTML; inspect HTTP status before decoding JSON.
            if version == 2 && matches!(status, 404 | 405) { continue; }
            let body = response.json().await.map_err(|_| Error::InvalidResponse)?;
            return parse_share(status, body, &[connect_code, &self.signer.creds.token, &self.signer.creds.csrf]);
        }
        unreachable!()
    }
    async fn request(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<(u16, Value), Error> {
        let text = body.map(|v| v.to_string()).unwrap_or_default();
        let response = self.http.request(method.clone(), format!("{}{path}", self.base))
            .headers(self.signer.build_headers(method.as_str(), path, &text)).body(text)
            .send().await.map_err(|_| Error::Transport)?;
        let status = response.status().as_u16();
        Ok((status, response.json().await.map_err(|_| Error::InvalidResponse)?))
    }
    pub async fn join(&self, device: &str, force_join: bool, retry: bool) -> Result<JoinData, Error> {
        let path = format!("/api/v1/room/join/by_device/{}", path_segment(device)?);
        let mut retries = 0;
        loop {
            let result = match self.request(reqwest::Method::POST, &path, Some(json!({"force_join":force_join}))).await {
                Ok((status, body)) => {
                    // Structure only (codes and key names), never the room token or URLs.
                    let shape = crate::protocol::json_shape(&body, 3);
                    let server_msg = body.get("msg").and_then(Value::as_str).map(|m| m.chars().take(120).collect::<String>());
                    let parsed = parse_join(status, body);
                    if parsed.is_err() {
                        // UU_VIEWER_DEBUG=1 adds the server's short error message (like UU_ASSIST_DEBUG on the host).
                        let msg = std::env::var("UU_VIEWER_DEBUG").is_ok_and(|v| v == "1").then_some(server_msg).flatten();
                        log::warn!("Join response: status={status} shape={shape} msg={msg:?}");
                    }
                    parsed
                }
                Err(e) => Err(e),
            };
            match result {
                Ok(data) => {
                    if data.token.is_empty() || data.signaling_list.is_empty() { return Err(Error::MissingRoom); }
                    return Ok(data);
                }
                Err(error) => match retry_delay(&error, retries, retry) {
                    Some(delay) => { tokio::time::sleep(delay).await; retries += 1; }
                    None => return Err(error),
                },
            }
        }
    }
    /// Remove a device from the account (official 「删除设备」). Paths from the Windows host strings
    /// `/api/v1/device/%s/unbind` and `/api/v1/device/share/%s/unbind` (own vs. shared-with-me device).
    /// POST verified live against an own device; the shared-device path is from strings only.
    pub async fn unbind(&self, device: &str, shared: bool) -> Result<(), Error> {
        let segment = path_segment(device)?;
        let path = if shared { format!("/api/v1/device/share/{segment}/unbind") } else { format!("/api/v1/device/{segment}/unbind") };
        let (status, body) = self.request(reqwest::Method::POST, &path, None).await?;
        parse_ack(status, body)
    }
    /// Structure-only probe of the device list (status, JSON keys/types/lengths, never values).
    pub async fn devices_shape(&self, method: reqwest::Method) -> Result<String, Error> {
        let path = "/api/v1/device/list";
        let response = self.http.request(method.clone(), format!("{}{path}", self.base))
            .headers(self.signer.build_headers(method.as_str(), path, "")).send().await.map_err(|_| Error::Transport)?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await.map_err(|_| Error::InvalidResponse)?;
        Ok(match serde_json::from_slice::<Value>(&bytes) {
            Ok(v) => format!("status={status} shape={}", crate::protocol::json_shape(&v, 7)),
            Err(_) => format!("status={status} non_json_bytes={}", bytes.len()),
        })
    }
    /// GET /api/v1/device/list → data {current_device, my_binded_devices[], others_shared_devices[]} (observed live).
    pub async fn device_list(&self) -> Result<Value, Error> {
        let (status, body) = self.request(reqwest::Method::GET, "/api/v1/device/list", None).await?;
        if body.get("code").and_then(Value::as_i64) == Some(1120) { return Err(Error::AuthenticationExpired); }
        if status != 200 || body.get("code").and_then(Value::as_i64) != Some(0) { return Err(Error::InvalidResponse); }
        body.get("data").cloned().ok_or(Error::InvalidResponse)
    }
    pub async fn target_is_mac(&self,room:&JoinData,device:Option<&str>)->Result<bool,Error>{
        let platform=match room.publisher_platform {
            Some(platform)=>Some(i64::from(platform)),
            None=>match device {
                Some(device)=>device_platform(&self.device_list().await?,device),
                None=>None,
            },
        };
        // Observed device/share platforms: 1 = Windows (also our Linux host), 4 = Mac.
        match platform {
            Some(1)=>Ok(false),
            Some(4)=>Ok(true),
            _=>Err(Error::UnknownKeyboardPlatform),
        }
    }
    /// Method and wire-key mapping remain unverified. Caller must select both explicitly.
    pub async fn devices(&self, method: reqwest::Method, array_pointer: &str) -> Result<Vec<Value>, Error> {
        let (status, body) = self.request(method, "/api/v1/device/list", None).await?;
        if body.get("code").and_then(Value::as_i64) == Some(1120) { return Err(Error::AuthenticationExpired); }
        if status != 200 || body.get("code").and_then(Value::as_i64) != Some(0) { return Err(Error::InvalidResponse); }
        body.pointer(array_pointer).and_then(Value::as_array).cloned().ok_or(Error::InvalidResponse)
    }
}

#[cfg(test)]
mod tests {
    #[test] fn unbind_ack_and_device_segment(){
        use super::*;
        assert_eq!(parse_ack(200,json!({"code":0,"msg":"ok"})),Ok(()));
        assert_eq!(parse_ack(422,json!({"code":1120})),Err(Error::AuthenticationExpired));
        assert_eq!(parse_ack(200,json!({"code":5003,"msg":"no such device"})),Err(Error::Api(5003,"no such device".into())));
        assert_eq!(parse_ack(404,json!({})),Err(Error::Api(-1,String::new())));
        assert_eq!(path_segment("a/b?c").unwrap(),"a%2Fb%3Fc");
        for bad in ["",".",".."]{assert_eq!(path_segment(bad),Err(Error::InvalidDevice));}
    }
    use super::*;
    #[test]
    fn target_platform_matches_only_the_selected_device(){
        let data=json!({"my_binded_devices":[{"device_id":"windows","platform":1},{"device_id":"mac","platform":4}],
            "others_shared_devices":[{"device_id":"shared-mac","platform":4}]});
        assert_eq!(device_platform(&data,"windows"),Some(1));
        assert_eq!(device_platform(&data,"mac"),Some(4));
        assert_eq!(device_platform(&data,"shared-mac"),Some(4));
        assert_eq!(device_platform(&data,"missing"),None);
        assert_eq!(device_platform(&json!({}),"mac"),None);
    }
    #[test]
    fn assistance_response_validation_and_safe_user_errors() {
        let mut fixture:Value=serde_json::from_str(include_str!("../../tests/fixtures/viewer/join.json")).unwrap();
        fixture["data"]["publisher_device_id"]=json!("synthetic-publisher");
        fixture["data"]["share_id"]=json!("synthetic-share");
        assert!(!parse_share(200,fixture.clone(),&[]).unwrap().token.is_empty());
        for key in ["token","signaling_list"] {
            let mut v=fixture.clone();v["data"].as_object_mut().unwrap().remove(key);
            assert_eq!(parse_share(200,v,&[]).err().unwrap(),Error::MissingRoom);
        }
        for msg in ["验证码错误","对方未开启协助","无法对同账号的设备发起远程协助"] {
            let shown=if crate::i18n::english() {english_message(msg).unwrap()} else {msg};
            assert_eq!(parse_share(422,json!({"code":9876,"msg":msg}),&[]).err().unwrap().to_string(),format!("{shown} (code 9876)"));
        }
        assert_eq!(parse_share(422,json!({"code":9876,"msg":"fixture-code\nfixture-token"}),&["fixture-code","fixture-token",""]).err().unwrap(),Error::Api(9876,"[已隐藏][已隐藏]".into()));
        assert_eq!(parse_share(422,json!({"code":1120,"msg":"expired"}),&[]).err().unwrap(),Error::AuthenticationExpired);
        assert_eq!(parse_share(200,json!({"code":0,"msg":"","data":{}}),&[]).err().unwrap(),Error::InvalidResponse);
    }
    #[test]
    fn join_schema_defaults_required_types_and_error_policy() {
        let fixture: Value = serde_json::from_str(include_str!("../../tests/fixtures/viewer/join.json")).unwrap();
        let data = parse_join(200, fixture.clone()).unwrap();
        assert_eq!(data.signaling_list.len(), 1);
        assert_eq!(data.publisher_platform,None);
        let mut empty=fixture.clone();empty["data"]["publisher_platform"]=Value::Null;
        assert_eq!(parse_join(200,empty).unwrap().publisher_platform,None);
        let mut mac=fixture.clone();mac["data"]["publisher_platform"]=json!(4);
        assert_eq!(parse_join(200,mac).unwrap().publisher_platform,Some(4));
        for key in ["report_token", "report_url"] {
            let mut v = fixture.clone(); v["data"].as_object_mut().unwrap().remove(key);
            assert!(matches!(parse_join(200,v), Err(Error::InvalidResponse)));
            let mut v = fixture.clone(); v["data"][key] = Value::Null;
            assert!(matches!(parse_join(200,v), Err(Error::InvalidResponse)));
        }
        let v = json!({"code":0,"msg":"","data":{"report_token":"","report_url":""}});
        let d = parse_join(200,v).unwrap();
        assert!(d.token.is_empty() && d.signaling_list.is_empty() && !d.international_connect);
        assert_eq!(d.ws_connect_timeout_ms,0); assert_eq!(d.streamer_retry_delta_ms,0);
        assert!(matches!(parse_join(503,fixture),Err(Error::Api(503, _))));
        for data in [Value::Null,json!([]),json!("ignored")] {
            assert!(matches!(parse_join(422,json!({"code":1120,"msg":"","data":data})),Err(Error::AuthenticationExpired)));
        }
        for code in [-1,2002,2006,2007,4042] { assert_eq!(retry_delay(&Error::Api(code,String::new()),0,true),None); }
        for retries in 0..5 { assert_eq!(retry_delay(&Error::Api(5000,String::new()),retries,true),Some(Duration::from_millis(3000))); }
        assert_eq!(retry_delay(&Error::Api(5000,String::new()),5,true),None);
        assert_eq!(retry_delay(&Error::Api(5000,String::new()),0,false),None);
        for error in [Error::Transport,Error::InvalidResponse,Error::AuthenticationExpired] { assert_eq!(retry_delay(&error,0,true),None); }
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;
    use crate::auth::Credentials;
    use tokio::{io::{AsyncReadExt,AsyncWriteExt},net::TcpListener};
    pub async fn server(replies:Vec<(u16,Value)>)->(String,tokio::task::JoinHandle<Vec<String>>) {
        server_raw(replies.into_iter().map(|(status,body)|(status,body.to_string())).collect()).await
    }
    async fn server_raw(replies:Vec<(u16,String)>)->(String,tokio::task::JoinHandle<Vec<String>>) {
        let listener=TcpListener::bind((std::net::Ipv4Addr::LOCALHOST,0)).await.unwrap();
        let url=format!("http://{}",listener.local_addr().unwrap());
        let job=tokio::spawn(async move {
            let mut requests=Vec::new();
            for (status,body) in replies {
                let(mut stream,_)=listener.accept().await.unwrap();let mut request=Vec::new();let mut buf=[0;1024];
                loop {
                    let n=stream.read(&mut buf).await.unwrap();assert_ne!(n,0);request.extend_from_slice(&buf[..n]);
                    if let Some(end)=request.windows(4).position(|w|w==b"\r\n\r\n") {
                        let headers=String::from_utf8_lossy(&request[..end]);
                        let length=headers.lines().find_map(|l|l.to_lowercase().strip_prefix("content-length:").map(str::trim).and_then(|s|s.parse::<usize>().ok())).unwrap_or(0);
                        if request.len()>=end+4+length {break}
                    }
                }
                requests.push(String::from_utf8(request).unwrap());
                let reply=format!("HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",body.len());
                stream.write_all(reply.as_bytes()).await.unwrap();
            }
            requests
        });
        (url,job)
    }
    fn signer()->UUAuthClient {UUAuthClient::new(Credentials{token:"synthetic-auth".into(),sign_key:"synthetic-key".into(),..Default::default()})}
    fn room(platform:Option<i32>)->JoinData {
        let mut room=parse_join(200,serde_json::from_str(include_str!("../../tests/fixtures/viewer/join.json")).unwrap()).unwrap();
        room.publisher_platform=platform;room
    }
    #[tokio::test]
    async fn keyboard_platform_uses_room_metadata_without_device_lookup(){
        let api=Api::new(signer(),"http://127.0.0.1:1");
        for device in [None,Some("synthetic-device")] {
            assert_eq!(api.target_is_mac(&room(Some(1)),device).await,Ok(false));
            assert_eq!(api.target_is_mac(&room(Some(4)),device).await,Ok(true));
            for platform in [0,-1,2,3,5,999,i32::MIN,i32::MAX] {
                assert_eq!(api.target_is_mac(&room(Some(platform)),device).await,Err(Error::UnknownKeyboardPlatform));
            }
        }
        assert_eq!(api.target_is_mac(&room(None),None).await,Err(Error::UnknownKeyboardPlatform));
    }
    #[tokio::test]
    async fn keyboard_platform_looks_up_only_the_selected_account_device(){
        for list in ["my_binded_devices","others_shared_devices"] {
            for (platform,mac) in [(1,false),(4,true)] {
                let data=json!({list:[{"device_id":"unrelated","platform":4},
                    {"device_id":"synthetic-device","platform":platform}]});
                let(url,job)=server(vec![(200,json!({"code":0,"data":data}))]).await;
                assert_eq!(Api::new(signer(),&url).target_is_mac(&room(None),Some("synthetic-device")).await,Ok(mac));
                let requests=job.await.unwrap();assert_eq!(requests.len(),1);
                assert!(requests[0].starts_with("GET /api/v1/device/list "));
            }
        }
    }
    #[tokio::test]
    async fn keyboard_platform_rejects_unknown_or_malformed_device_metadata(){
        let mut devices=vec![json!({"device_id":"unrelated","platform":4}),json!({"device_id":"synthetic-device"})];
        for platform in [json!(0),json!(-1),json!(999),json!("4"),json!(true),json!(4.5),json!(null),json!(u64::MAX)] {
            devices.push(json!({"device_id":"synthetic-device","platform":platform}));
        }
        for device in devices {
            let data=json!({"my_binded_devices":[device]});
            let(url,job)=server(vec![(200,json!({"code":0,"data":data}))]).await;
            assert_eq!(Api::new(signer(),&url).target_is_mac(&room(None),Some("synthetic-device")).await,Err(Error::UnknownKeyboardPlatform));
            assert_eq!(job.await.unwrap().len(),1);
        }
    }
    #[tokio::test]
    async fn keyboard_platform_propagates_device_lookup_failures(){
        for (status,body,error) in [
            (422,json!({"code":1120}),Error::AuthenticationExpired),
            (503,json!({"code":5000}),Error::InvalidResponse),
            (200,json!({"code":0}),Error::InvalidResponse),
        ] {
            let(url,job)=server(vec![(status,body)]).await;
            assert_eq!(Api::new(signer(),&url).target_is_mac(&room(None),Some("synthetic-device")).await,Err(error));
            assert_eq!(job.await.unwrap().len(),1);
        }
        let api=Api::new(signer(),"http://127.0.0.1:1");
        assert_eq!(api.target_is_mac(&room(None),Some("synthetic-device")).await,Err(Error::Transport));
    }
    #[tokio::test]
    async fn signed_join_and_explicit_device_list_use_only_loopback_http() {
        let fixture=serde_json::from_str(include_str!("../../tests/fixtures/viewer/join.json")).unwrap();
        let(url,job)=server(vec![(200,json!({"code":0,"data":{"synthetic_devices":[{"id":"synthetic-device","name":"fixture"}]}})),(200,fixture)]).await;
        let api=Api::new(signer(),&url);
        let devices=api.devices(reqwest::Method::GET,"/data/synthetic_devices").await.unwrap();assert_eq!(devices.len(),1);
        assert!(api.join("",true,true).await.is_err());
        api.join("synthetic-device",true,true).await.unwrap();
        let requests=job.await.unwrap();assert!(requests[0].starts_with("GET /api/v1/device/list "));
        assert!(requests[1].starts_with("POST /api/v1/room/join/by_device/synthetic-device "));
        assert!(requests[1].to_lowercase().contains("x-param-sign:"));
        assert_eq!(requests[1].split("\r\n\r\n").nth(1).unwrap(),r#"{"force_join":true}"#);
    }
    #[tokio::test]
    async fn assistance_uses_signed_json_and_only_http_endpoint_fallback() {
        let mut fixture:Value=serde_json::from_str(include_str!("../../tests/fixtures/viewer/join.json")).unwrap();
        // Extra share identity fields do not change the existing room parser.
        fixture["data"]["share_id"]=json!("synthetic-share");
        fixture["data"]["publisher_device_id"]=json!("synthetic-publisher");
        fixture["data"]["publisher_platform"]=json!(1);
        for status in [200,404,405] {
            let replies=if status==200 {vec![(200,fixture.to_string())]} else {vec![(status,"<html>endpoint unavailable</html>".into()),(200,fixture.to_string())]};
            let(url,job)=server_raw(replies).await;
            let room=Api::new(signer(),&url).join_share("synthetic-id","synthetic-code").await.unwrap();
            assert!(!room.token.is_empty());assert_eq!(room.signaling_list.len(),1);
            let requests=job.await.unwrap();assert_eq!(requests.len(),if status==200{1}else{2});
            for (i,request) in requests.iter().enumerate() {
                let version=if i==0{2}else{1};
                assert!(request.starts_with(&format!("POST /api/v{version}/room/join/share/by_code ")));
                let(headers,body)=request.split_once("\r\n\r\n").unwrap();
                assert!(!headers.contains("synthetic-code"));
                assert!(headers.to_lowercase().contains("x-param-sign:"));
                assert!(headers.to_lowercase().contains("content-type: application/json"));
                assert_eq!(serde_json::from_str::<Value>(body).unwrap(),json!({"connect_id":"synthetic-id","connect_code":"synthetic-code"}));
            }
        }
    }
    #[tokio::test]
    async fn assistance_reports_actual_errors_without_retrying_or_echoing_secrets() {
        for msg in ["验证码错误","对方未开启协助","无法对同账号的设备发起远程协助"] {
            let(url,job)=server(vec![(422,json!({"code":9876,"msg":msg}))]).await;
            let error=Api::new(signer(),&url).join_share("synthetic-id","synthetic-code").await.err().unwrap();
            let shown=if crate::i18n::english() {english_message(msg).unwrap()} else {msg};
            assert_eq!(error.to_string(),format!("{shown} (code 9876)"));assert_eq!(job.await.unwrap().len(),1);
        }
        for (status,body,expected) in [
            (200,json!({"code":405,"msg":"业务拒绝"}),Error::Api(405,"业务拒绝".into())),
            (500,json!({"code":5000,"msg":"服务忙"}),Error::Api(5000,"服务忙".into())),
            (422,json!({"code":1120,"msg":""}),Error::AuthenticationExpired),
            (200,json!({"code":0,"msg":"","data":{"report_url":"","report_token":""}}),Error::MissingRoom),
            (200,json!({"code":0,"msg":"","data":{}}),Error::InvalidResponse),
            (422,json!({"code":9876,"msg":"synthetic-code synthetic-auth"}),Error::Api(9876,"[已隐藏] [已隐藏]".into())),
        ] {
            let(url,job)=server(vec![(status,body)]).await;
            assert_eq!(Api::new(signer(),&url).join_share("synthetic-id","synthetic-code").await.err().unwrap(),expected);
            assert_eq!(job.await.unwrap().len(),1);
        }
        let api=Api::new(signer(),"http://127.0.0.1:1");
        for (id,code) in [("","fixture"),("id",""),("id","a\nb")] {assert_eq!(api.join_share(id,code).await.err().unwrap(),Error::InvalidAssist);}
    }
    #[tokio::test]
    async fn retry_stops_after_five_delays_and_terminal_errors_stop_immediately() {
        let(url,job)=server(vec![(503,json!({"code":5000,"msg":"fixture"}));6]).await;
        let start=tokio::time::Instant::now();
        assert!(matches!(Api::new(signer(),&url).join("synthetic-device",false,true).await,Err(Error::Api(5000, _))));
        assert!(start.elapsed()>=Duration::from_secs(15));assert_eq!(job.await.unwrap().len(),6);
        for code in [-1,1120,2002,2006,2007,4042] {
            let(url,job)=server(vec![(422,json!({"code":code,"msg":"fixture"}))]).await;
            let error=Api::new(signer(),&url).join("synthetic-device",false,true).await.err().unwrap();
            if code==1120 {assert_eq!(error,Error::AuthenticationExpired)} else {assert!(matches!(error,Error::Api(c,_) if c==code))}
            assert_eq!(job.await.unwrap().len(),1);
        }
    }
}

#[cfg(test)]
mod strict_tests {
    use super::*;
    #[test]
    fn rejects_wrong_json_types_and_missing_envelope_members() {
        let fixture:Value=serde_json::from_str(include_str!("../../tests/fixtures/viewer/join.json")).unwrap();
        for (key,bad) in [("token",json!(false)),("signaling_list",json!([7])),("ws_connect_timeout_ms",json!(2147483648u64)),("streamer_retry_delta_ms",json!(null)),("international_connect",json!(1)),("report_token",json!(null)),("report_url",json!(7)),("report_server_address",json!([]))] {
            let mut v=fixture.clone();v["data"][key]=bad;assert!(matches!(parse_join(200,v),Err(Error::InvalidResponse)));
        }
        for platform in [json!("4"),json!(true),json!(4.5),json!(2147483648u64),json!(-2147483649i64)] {
            let mut v=fixture.clone();v["data"]["publisher_platform"]=platform;
            assert!(matches!(parse_join(200,v),Err(Error::InvalidResponse)));
        }
        for key in ["code","msg"] {
            let mut v=fixture.clone();v.as_object_mut().unwrap().remove(key);assert!(matches!(parse_join(200,v),Err(Error::InvalidResponse)));
        }
        assert!(matches!(parse_join(200,json!({"code":0,"msg":"","data":{}})),Err(Error::InvalidResponse)));
        assert!(matches!(parse_join(200,json!({"code":2147483648u64,"msg":""})),Err(Error::InvalidResponse)));
    }
}
