//! UU Remote Official Authentication & REST Client

use hmac::{Hmac, Mac};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

/// Request-signing key built into the official UU PC client (x-param-plat=1); the same for
/// every user. Config "sign_key" / UU_SIGN_KEY override it if NetEase rotates the key.
pub const DEFAULT_SIGN_KEY: &str = "alWiSzXZTLu3WfFnw13uBru3";
/// API code for an invalid or expired login token ("Token无效或已过期", HTTP 422).
pub const TOKEN_INVALID: i64 = 1120;

#[derive(Debug)]
pub enum RoomError { LoginExpired, Other(String) }
impl std::fmt::Display for RoomError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self { RoomError::LoginExpired => f.write_str("login token invalid or expired"), RoomError::Other(e) => f.write_str(e) }
    }
}

pub const BASE_URL: &str = "https://api.nrd.nie.163.com";

/// Account binding and request-signing key. Never committed: loaded at startup from
/// a private config file or environment variables (see config.example.json).
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct Credentials {
    pub user_id: String,
    pub device_id: String,
    pub client_id: String,
    pub system_id: String,
    pub token: String,
    pub csrf: String,
    /// HMAC-SHA256 key for x-param-sign.
    pub sign_key: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credentials { .. }")
    }
}

impl Credentials {
    /// Environment variables win over the file (UU_USER_ID, UU_DEVICE_ID, UU_CLIENT_ID,
    /// UU_SYSTEM_ID, UU_TOKEN, UU_CSRF, UU_SIGN_KEY).
    pub fn load(file: Option<&serde_json::Value>) -> Result<Self, String> {
        let get = |key: &str| -> String {
            std::env::var(format!("UU_{}", key.to_uppercase())).ok().filter(|v| !v.is_empty())
                .or_else(|| file.and_then(|f| f.get(key)).and_then(|v| v.as_str()).map(str::to_owned))
                .unwrap_or_default()
        };
        let creds = Self {
            user_id: get("user_id"), device_id: get("device_id"), client_id: get("client_id"),
            system_id: get("system_id"), token: get("token"), csrf: get("csrf"),
            sign_key: Some(get("sign_key")).filter(|k| !k.is_empty()).unwrap_or_else(|| DEFAULT_SIGN_KEY.into()),
        };
        let missing: Vec<&str> = [("user_id", &creds.user_id), ("device_id", &creds.device_id), ("client_id", &creds.client_id),
            ("system_id", &creds.system_id), ("token", &creds.token), ("csrf", &creds.csrf), ("sign_key", &creds.sign_key)]
            .iter().filter(|(_, v)| v.is_empty()).map(|(k, _)| *k).collect();
        if missing.is_empty() { Ok(creds) } else { Err(format!("missing credential settings: {}", missing.join(", "))) }
    }
}

#[derive(Debug, Deserialize)]
pub struct ApiResponse<T> {
    pub code: i32,
    pub msg: String,
    pub data: Option<T>,
}

#[derive(Clone, Deserialize)]
pub struct RoomData {
    pub signaling_server: String,
    pub token: String,
    pub signaling_list: Option<Vec<String>>,
    pub ws_connect_timeout_ms: Option<u64>,
}

impl std::fmt::Debug for RoomData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("RoomData { .. }") }
}

pub struct UUAuthClient {
    pub creds: Credentials,
    http: reqwest::Client,
    guest_id: Option<String>,
}

/// QR failures are safe to display: never retain a URL, source error or response body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QrRequestErrorKind { Timeout, Connect, Decode, Body, Request }
impl QrRequestErrorKind {
    fn from_reqwest(error: &reqwest::Error) -> Self {
        if error.is_timeout() { Self::Timeout }
        else if error.is_connect() { Self::Connect }
        else if error.is_decode() { Self::Decode }
        else if error.is_body() { Self::Body }
        else { Self::Request }
    }
    fn label(self) -> &'static str {
        match self { Self::Timeout => "timeout", Self::Connect => "connect", Self::Decode => "decode",
            Self::Body => "body", Self::Request => "request" }
    }
}
#[derive(Debug)]
pub struct QrRequestError {
    pub kind: QrRequestErrorKind,
    phase: &'static str,
    elapsed_ms: u128,
    http_status: Option<u16>,
}
impl std::fmt::Display for QrRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "phase={} kind={} elapsed_ms={} http_status={:?}",
            self.phase, self.kind.label(), self.elapsed_ms, self.http_status)
    }
}
impl QrRequestError {
    pub fn response_received(&self) -> bool { self.http_status.is_some() }
    fn new(kind: QrRequestErrorKind, phase: &'static str, started: std::time::Instant, http_status: Option<u16>) -> Self {
        Self { kind, phase, elapsed_ms: started.elapsed().as_millis(), http_status }
    }
}
async fn send_qr_request(request: reqwest::RequestBuilder, started: std::time::Instant) -> Result<(u16, serde_json::Value), QrRequestError> {
    let resp = request.send().await.map_err(|e| QrRequestError::new(QrRequestErrorKind::from_reqwest(&e), "send", started, None))?;
    let status = resp.status().as_u16();
    let value = resp.json().await.map_err(|e| QrRequestError::new(QrRequestErrorKind::from_reqwest(&e), "response", started, Some(status)))?;
    Ok((status, value))
}

/// Transport failures name only the outer request; the cause (certificate, proxy, DNS,
/// timeout) lives in the source chain. URL user-info (proxy credentials) and query
/// strings are removed before the chain is shown.
pub fn transport_error(error: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if parts.last() != Some(&text) { parts.push(text); }
        source = cause.source();
        if parts.len() >= 8 { break; }
    }
    redact_urls(&parts.join(": "))
}
fn redact_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let end = tail.find(|c: char| c.is_whitespace() || c == ')' || c == '"' || c == '\'').unwrap_or(tail.len());
        let url = &tail[..end];
        let url = url.rsplit_once('@').map_or(url, |(_, host)| host);
        out.push_str(url.split(['?', '#']).next().unwrap_or(""));
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// Cloud API and signaling clients ignore ambient HTTP/HTTPS proxy settings by default: a local
/// proxy that intercepts TLS breaks login and signaling. `UU_USE_PROXY=1` opts back
/// into reqwest's standard proxy environment handling.
pub fn http_client_builder() -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder();
    if proxy_enabled() {
        builder
    } else {
        builder.no_proxy()
    }
}
pub(crate) fn proxy_enabled() -> bool {
    std::env::var("UU_USE_PROXY").ok().as_deref().is_some_and(proxy_requested)
}
fn proxy_requested(value: &str) -> bool {
    matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

impl UUAuthClient {
    pub fn new(creds: Credentials) -> Self {
        let http = http_client_builder()
            .timeout(std::time::Duration::from_secs(12))
            .build()
            .unwrap();
        Self { creds, http, guest_id: None }
    }

    /// Explicit guest identity: never inherits account Authorization/cookies.
    /// Official ApiDefaultHeaders 0x1003853ac/0x1003853f8 (see assist evidence).
    pub fn new_guest(mut device: Credentials, guest_id: String, token: String) -> Self {
        device.user_id.clear(); device.csrf.clear(); device.token = token;
        let mut client = Self::new(device);
        client.guest_id = Some(guest_id);
        client
    }

    /// 计算网易 NRD HMAC-SHA256 签名
    pub fn sign_request(
        &self,
        method: &str,
        path: &str,
        headers: &BTreeMap<String, String>,
        body_str: &str,
    ) -> String {
        let mut filtered: BTreeMap<String, String> = BTreeMap::new();
        for (k, v) in headers {
            let kl = k.to_lowercase();
            if kl.starts_with("x-param-") && kl != "x-param-sign" {
                filtered.insert(kl, v.clone());
            }
        }
        let params_str = filtered
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("&");

        let string_to_sign = format!("{}{}{}{}", method.to_uppercase(), path, params_str, body_str);
        let mut mac = HmacSha256::new_from_slice(self.creds.sign_key.as_bytes()).expect("HMAC can take key");
        mac.update(string_to_sign.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    pub fn build_headers(
        &self,
        method: &str,
        path: &str,
        body_str: &str,
    ) -> HeaderMap {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let ts = now.to_string();

        let mut param_map = BTreeMap::new();
        param_map.insert("x-param-plat".into(), "1".into());
        param_map.insert("x-param-vn".into(), "4.42.0.2770".into());
        param_map.insert("x-param-vc".into(), "2770".into());
        param_map.insert("x-param-pkgn".into(), "com.netease.gameviewer".into());
        param_map.insert("x-param-chn".into(), "gwqd".into());
        param_map.insert("x-param-lang".into(), "zh-CN".into());
        param_map.insert("x-param-cnt".into(), "CN".into());
        // Before login some identifiers do not exist yet; absent ones are not sent or signed.
        for (key, value) in [("x-param-client-id", &self.creds.client_id), ("x-param-device-id", &self.creds.device_id),
            ("x-param-system-id", &self.creds.system_id), ("x-param-user-id", &self.creds.user_id)] {
            if !value.is_empty() { param_map.insert(key.into(), value.clone()); }
        }
        if let Some(id) = &self.guest_id { param_map.insert("x-param-guest-id".into(), id.clone()); }
        param_map.insert("x-param-ts".into(), ts);

        let sign = self.sign_request(method, path, &param_map, body_str);

        let mut headers = HeaderMap::new();
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));
        headers.insert("Accept", HeaderValue::from_static("application/json"));
        headers.insert(
            "User-Agent",
            HeaderValue::from_static("UURemote/4.42.0 (Windows NT 10.0; Win64; x64)"),
        );
        if !self.creds.token.is_empty() {
            if let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", self.creds.token)) { headers.insert("Authorization", v); }
            if self.guest_id.is_none() {
                if let Ok(v) = HeaderValue::from_str(&format!("nrd_access_token={}; nrd_csrf_access_token={}", self.creds.token, self.creds.csrf)) {
                    headers.insert("Cookie", v);
                }
                if let Ok(v) = HeaderValue::from_str(&self.creds.csrf) { headers.insert("X-CSRF-TOKEN", v); }
            }
        }

        for (k, v) in param_map {
            if let Ok(name) = HeaderName::from_bytes(k.as_bytes()) {
                if let Ok(val) = HeaderValue::from_str(&v) {
                    headers.insert(name, val);
                }
            }
        }
        headers.insert("X-Param-SIGN", HeaderValue::from_str(&sign).unwrap());

        headers
    }

    /// Signed JSON POST; returns (HTTP status, decoded body).
    pub async fn post_json(&self, path: &str, body: &serde_json::Value) -> Result<(u16, serde_json::Value), String> {
        let body_str = serde_json::to_string(body).map_err(|e| e.to_string())?;
        let resp = self.http.post(format!("{}{}", BASE_URL, path))
            .headers(self.build_headers("POST", path, &body_str)).body(body_str)
            .send().await.map_err(|e| format!("HTTP post {path} failed: {}", transport_error(&e)))?;
        let status = resp.status().as_u16();
        let value = resp.json().await.unwrap_or(serde_json::Value::Null);
        Ok((status, value))
    }

    /// Signed POST with an optional JSON body (None = empty body, like Moya `requestPlain`) and an
    /// explicit timeout. The official clients long-poll the QR status with a 60 s request timeout.
    pub async fn post_with_timeout(&self, path: &str, body: Option<&serde_json::Value>, timeout: std::time::Duration) -> Result<(u16, serde_json::Value), QrRequestError> {
        let started = std::time::Instant::now();
        let body_str = body.map(serde_json::to_string).transpose()
            .map_err(|_| QrRequestError::new(QrRequestErrorKind::Request, "encode", started, None))?.unwrap_or_default();
        let request = self.http.post(format!("{}{}", BASE_URL, path)).timeout(timeout)
            .headers(self.build_headers("POST", path, &body_str)).body(body_str);
        send_qr_request(request, started).await
    }

    /// Signed JSON POST to another API host (the signature covers the path only).
    pub async fn post_json_url(&self, base: &str, path: &str, body: &serde_json::Value) -> Result<(u16, serde_json::Value), String> {
        let body_str = serde_json::to_string(body).map_err(|e| e.to_string())?;
        let resp = self.http.post(format!("{base}{path}"))
            .headers(self.build_headers("POST", path, &body_str)).body(body_str)
            .send().await.map_err(|e| format!("HTTP post {path} failed: {}", transport_error(&e)))?;
        let status = resp.status().as_u16();
        Ok((status, resp.json().await.unwrap_or(serde_json::Value::Null)))
    }

    /// Guest APIs use requestPlain for empty requests; sign the actual empty body.
    /// Errors intentionally exclude response text and URL (both can contain secrets).
    pub async fn assist_post(&self, path: &str, body: Option<&serde_json::Value>) -> Result<(u16, serde_json::Value), String> {
        self.assist_send(self.assist_request(path, body)?).await
    }
    /// Account share info is a GET (POST answers 405, verified live).
    pub async fn assist_get(&self, path: &str) -> Result<(u16, serde_json::Value), String> {
        let request = self.http.get(format!("{BASE_URL}{path}")).headers(self.build_headers("GET", path, "")).build()
            .map_err(|_| "assist request construction failed")?;
        self.assist_send(request).await
    }
    async fn assist_send(&self, request: reqwest::Request) -> Result<(u16, serde_json::Value), String> {
        let response = self.http.execute(request)
            .await.map_err(|e| format!("assist request transport failed: {}", transport_error(&e)))?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await.map_err(|_| "assist response read failed")?;
        if bytes.len() > 65536 { return Err("assist response too large".into()); }
        let value = serde_json::from_slice(&bytes).map_err(|_| "assist response malformed")?;
        Ok((status, value))
    }

    fn assist_request(&self, path: &str, body: Option<&serde_json::Value>) -> Result<reqwest::Request, String> {
        let body = body.map(serde_json::to_string).transpose().map_err(|_| "assist request encoding failed")?.unwrap_or_default();
        self.http.post(format!("{BASE_URL}{path}"))
            .headers(self.build_headers("POST", path, &body)).body(body).build()
            .map_err(|_| "assist request construction failed".into())
    }

    /// 向网易服务器请求创建房间，获取最新的信令服务器与鉴权 Token
    pub async fn create_room(&self) -> Result<RoomData, RoomError> {
        let (status, body) = self.post_json("/api/v1/room/create", &serde_json::json!({})).await.map_err(RoomError::Other)?;
        let code = body.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code == TOKEN_INVALID { return Err(RoomError::LoginExpired); }
        if status == 200 && code == 0 {
            if let Some(data) = body.get("data") {
                let room: RoomData = serde_json::from_value(data.clone()).map_err(|e| RoomError::Other(format!("room data decode failed: {e}")))?;
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                log::info!("Room token lifetime: remaining_s={:?}", room_token_lifetime(&room.token, now));
                return Ok(room);
            }
        }
        Err(RoomError::Other(format!("create_room error http {status} code {code}: {}",
            body.get("msg").and_then(|m| m.as_str()).unwrap_or(""))))
    }
}

// Report only the remaining lifetime; never return or log token/claim contents.
fn room_token_lifetime(token: &str, now: u64) -> Option<i64> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    if payload.len() > 16384 { return None; }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload)).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let remaining = claims.get("exp")?.as_i64()?.saturating_sub(i64::try_from(now).ok()?);
    (remaining.unsigned_abs() <= 7 * 24 * 3600).then_some(remaining)
}

#[cfg(test)]
mod room_lifetime_tests {
    use super::room_token_lifetime;
    use base64::Engine as _;
    #[test]
    fn exposes_only_remaining_seconds_and_handles_opaque_tokens() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"exp":2200,"ignored":"test-only"}"#);
        assert_eq!(room_token_lifetime(&format!("h.{payload}.s"),1000),Some(1200));
        assert_eq!(room_token_lifetime("opaque",1000),None);
        assert_eq!(room_token_lifetime("h.invalid.s",1000),None);
    }
}

#[cfg(test)]
mod guest_tests {
    use super::*;
    #[test]
    fn official_request_plain_body_and_sanitized_room_debug() {
        let client = UUAuthClient::new_guest(Credentials { sign_key: "fixture-key".into(), ..Default::default() },
            "fixture-guest".into(), "fixture-token".into());
        let request = client.assist_request("/api/v1/guest/create", None).unwrap();
        assert_eq!(request.method(), reqwest::Method::POST);
        assert_eq!(request.body().unwrap().as_bytes(), Some(&b""[..]));
        let other = client.assist_request("/api/v1/guest/create", Some(&serde_json::json!({}))).unwrap();
        assert_eq!(other.body().unwrap().as_bytes(), Some(&b"{}"[..]));
        assert_ne!(request.headers()["x-param-sign"], other.headers()["x-param-sign"]);
        let room: RoomData = serde_json::from_value(serde_json::json!({"signaling_server":"wss://fixture.invalid", "token":"synthetic-room-secret"})).unwrap();
        assert!(!format!("{room:?}").contains("synthetic-room-secret"));
        assert!(serde_json::from_value::<RoomData>(serde_json::json!({"token":42})).is_err());
    }
    #[test]
    fn guest_signs_guest_id_and_never_inherits_account_identity() {
        let account = Credentials { user_id: "fixture-account".into(), token: "fixture-account-token".into(),
            csrf: "fixture-account-csrf".into(), client_id: "fixture-client".into(),
            device_id: "fixture-device".into(), sign_key: "fixture-sign-key".into(), ..Default::default() };
        let client = UUAuthClient::new_guest(account, "fixture-guest".into(), "fixture-guest-token".into());
        let headers = client.build_headers("POST", "/api/v1/guest/room/create", "");
        assert_eq!(headers["authorization"], "Bearer fixture-guest-token");
        assert_eq!(headers["x-param-guest-id"], "fixture-guest");
        for key in ["x-param-user-id", "cookie", "x-csrf-token"] { assert!(!headers.contains_key(key)); }
        let params: BTreeMap<String,String> = headers.iter().filter_map(|(key, value)|
            key.as_str().starts_with("x-param-").then(|| (key.to_string(), value.to_str().unwrap().to_owned()))).collect();
        assert_eq!(headers["x-param-sign"], client.sign_request("POST", "/api/v1/guest/room/create", &params, ""));
        let mut without_guest = params; without_guest.remove("x-param-guest-id");
        assert_ne!(headers["x-param-sign"], client.sign_request("POST", "/api/v1/guest/room/create", &without_guest, ""));
    }

    #[test]
    fn transport_error_redacts_proxy_credentials_and_queries() {
        let text = super::redact_urls("proxy http://user:secret@proxy.example:3128 failed for (https://api.example/p?token=abc)");
        assert_eq!(text, "proxy http://proxy.example:3128 failed for (https://api.example/p)");
    }

    #[test]
    fn proxy_opt_in_accepts_only_explicit_truthy_values() {
        for value in ["1", "true", "TRUE", " yes ", "On"] { assert!(super::proxy_requested(value)); }
        for value in ["", "0", "false", "no", "off", "2"] { assert!(!super::proxy_requested(value)); }
    }
}

#[cfg(test)]
mod qr_request_tests {
    use super::*;
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Synthetic, loopback-only responses: no account, device, proxy or live login API.
    async fn response(reply: &'static [u8], stall: bool) -> QrRequestError {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            socket.write_all(reply).await.unwrap();
            if stall { std::future::pending::<()>().await; }
        });
        let request = reqwest::Client::builder().no_proxy().build().unwrap()
            .post(format!("http://fixture-user:fixture-password@{addr}/fixture-path?token=fixture-ticket"))
            .timeout(Duration::from_secs(2));
        let error = send_qr_request(request, Instant::now()).await.unwrap_err();
        server.abort();
        let output = format!("{error} {error:?}");
        for secret in ["fixture-user", "fixture-password", "fixture-path", "fixture-ticket", "fixture-body", "127.0.0.1", "http://"] {
            assert!(!output.contains(secret));
        }
        error
    }

    #[tokio::test]
    async fn qr_request_distinguishes_send_timeout_body_timeout_and_bad_json_without_secrets() {
        let error = response(b"", true).await;
        assert_eq!((error.kind, error.phase, error.http_status), (QrRequestErrorKind::Timeout, "send", None));
        assert!(!error.response_received());
        let error = response(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n", true).await;
        assert_eq!((error.kind, error.phase, error.http_status), (QrRequestErrorKind::Timeout, "response", Some(200)));
        assert!(error.response_received());
        let error = response(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 12\r\n\r\nfixture-body", false).await;
        assert_eq!((error.kind, error.phase, error.http_status), (QrRequestErrorKind::Decode, "response", Some(502)));
        assert!(error.response_received());
    }

    #[tokio::test]
    async fn qr_connection_failure_is_not_a_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let request = reqwest::Client::builder().no_proxy().build().unwrap()
            .post(format!("http://{addr}/fixture-ticket")).timeout(Duration::from_secs(2));
        let error = send_qr_request(request, Instant::now()).await.unwrap_err();
        assert_eq!((error.kind, error.phase, error.http_status), (QrRequestErrorKind::Connect, "send", None));
        assert!(!format!("{error} {error:?}").contains("fixture-ticket"));
    }
}
