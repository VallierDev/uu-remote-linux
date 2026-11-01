//! Socket.IO's WebSocket transport uses the same opt-in proxy policy as the REST client.
//! No proxy addresses, room headers, response text or underlying errors are displayed.
use std::time::Duration;
use reqwest::header::{HeaderMap, CONNECTION, UPGRADE, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY,
    SEC_WEBSOCKET_EXTENSIONS, SEC_WEBSOCKET_PROTOCOL};
use tokio_tungstenite::{WebSocketStream, tungstenite::{handshake::{client::Request, derive_accept_key}, protocol::Role}};

pub type Socket = WebSocketStream<reqwest::Upgraded>;

/// Validate configuration rather than let reqwest silently ignore an unsupported proxy URL.
/// Selection/NO_PROXY matching still belongs to reqwest, not a second proxy implementation.
fn validate_proxy_url(value: &str) -> Result<(), &'static str> {
    if value.is_empty() { return Ok(()); }
    // Match reqwest's system-proxy parser before URL normalization (which would accept
    // whitespace/Unicode that http::Uri rejects and the connector would silently ignore).
    let uri: tokio_tungstenite::tungstenite::http::Uri = value.parse().map_err(|_| "Invalid signaling proxy configuration")?;
    if uri.authority().is_none() { return Err("Invalid signaling proxy configuration"); }
    let url = if value.contains("://") { reqwest::Url::parse(value) }
        else { reqwest::Url::parse(&format!("http://{value}")) }
        .map_err(|_| "Invalid signaling proxy configuration")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none()
        || url.port_or_known_default().is_none() || url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err("Unsupported signaling proxy configuration (HTTP/HTTPS only)");
    }
    Ok(())
}

fn client() -> Result<reqwest::Client, String> {
    if crate::auth::proxy_enabled() {
        let mut configured = false;
        for names in [["HTTP_PROXY", "http_proxy"], ["HTTPS_PROXY", "https_proxy"], ["ALL_PROXY", "all_proxy"]] {
            if let Some(value) = names.iter().find_map(std::env::var_os) {
                let value = value.to_str().ok_or("Invalid signaling proxy configuration")?;
                validate_proxy_url(value)?;
                configured |= !value.is_empty();
            }
        }
        // reqwest disables environment proxies in CGI contexts; don't silently ignore an opt-in.
        if configured && std::env::var_os("REQUEST_METHOD").is_some() {
            return Err("Signaling environment proxies are disabled in CGI contexts".into());
        }
    }
    crate::auth::http_client_builder().http1_only().redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never()).build().map_err(|_| "Signaling HTTP client setup failed".into())
}

pub async fn connect(request: Request, timeout: Duration) -> Result<Socket, String> {
    let client = client()?;
    match tokio::time::timeout(timeout, connect_with_client(&client, request)).await {
        Ok(result) => result,
        Err(_) => Err("WS connect failed: timeout".into()),
    }
}

async fn connect_with_client(client: &reqwest::Client, request: Request) -> Result<Socket, String> {
    let (mut parts, _) = request.into_parts();
    if parts.method != reqwest::Method::GET || parts.headers.contains_key(SEC_WEBSOCKET_EXTENSIONS)
        || parts.headers.contains_key(SEC_WEBSOCKET_PROTOCOL) || parts.headers.contains_key("proxy-authorization") {
        return Err("Unsupported signaling WebSocket request".into());
    }
    let mut url = reqwest::Url::parse(&parts.uri.to_string()).map_err(|_| "Invalid signaling endpoint")?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err("Invalid signaling endpoint".into());
    }
    let scheme = match url.scheme() { "ws" => "http", "wss" => "https", _ => return Err("Unsupported signaling endpoint scheme".into()) };
    url.set_scheme(scheme).map_err(|_| "Invalid signaling endpoint")?;
    let key = parts.headers.get(SEC_WEBSOCKET_KEY).ok_or("Missing WebSocket handshake key")?;
    let accept = derive_accept_key(key.as_bytes());
    for name in ["x-nrd-auth", "authorization", "cookie"] {
        if let Some(value) = parts.headers.get_mut(name) { value.set_sensitive(true); }
    }
    // reqwest owns DNS/TCP/proxy CONNECT/TLS. No custom relay, direct fallback or redirect.
    let response = client.get(url).version(reqwest::Version::HTTP_11).headers(parts.headers).send().await
        .map_err(|e| if e.is_timeout() { "WS connect failed: timeout" } else if e.is_connect() { "WS connect failed: connection" } else { "WS connect failed: request" })?;
    if response.status() != reqwest::StatusCode::SWITCHING_PROTOCOLS {
        return Err(format!("WS upgrade refused: http_status={}", response.status().as_u16()));
    }
    verify_upgrade(response.version(), response.headers(), &accept)?;
    let stream = response.upgrade().await.map_err(|_| "WS upgrade failed")?;
    Ok(WebSocketStream::from_raw_socket(stream, Role::Client, None).await)
}

/// RFC 6455 §4.1: validate before handing upgraded bytes to the frame codec. Socket.IO
/// requests no extensions/subprotocols. Unlike a generic HTTP client we never follow redirects.
fn verify_upgrade(version: reqwest::Version, headers: &HeaderMap, accept: &str) -> Result<(), &'static str> {
    let single = |name: reqwest::header::HeaderName| {
        let mut values = headers.get_all(name).iter();
        let value = values.next()?.to_str().ok()?;
        values.next().is_none().then_some(value)
    };
    let connection_upgrade = headers.get_all(CONNECTION).iter().all(|value| value.to_str().is_ok())
        && headers.get_all(CONNECTION).iter().any(|value|
            value.to_str().is_ok_and(|value| value.split(',').any(|token| token.trim().eq_ignore_ascii_case("upgrade"))));
    if version != reqwest::Version::HTTP_11 || !connection_upgrade
        || !single(UPGRADE).is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
        || single(SEC_WEBSOCKET_ACCEPT) != Some(accept)
        || headers.contains_key(SEC_WEBSOCKET_EXTENSIONS) || headers.contains_key(SEC_WEBSOCKET_PROTOCOL) {
        return Err("Invalid signaling WebSocket upgrade response");
    }
    Ok(())
}

#[cfg(test)]
#[path = "ws_transport_tests.rs"]
mod tests;
