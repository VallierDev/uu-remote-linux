use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio::{io::{AsyncRead, AsyncReadExt, AsyncWriteExt}, net::{TcpListener, TcpStream}};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

fn request(url: &str) -> Request {
    let mut request = url.into_client_request().unwrap();
    request.headers_mut().insert("x-nrd-auth", "fixture-room-secret".parse().unwrap());
    request
}

fn test_client() -> reqwest::ClientBuilder {
    reqwest::Client::builder().no_proxy().http1_only()
        .redirect(reqwest::redirect::Policy::none()).retry(reqwest::retry::never())
}

async fn read_headers<S: AsyncRead + Unpin>(socket: &mut S) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        socket.read_exact(&mut byte).await.unwrap();
        bytes.push(byte[0]);
        assert!(bytes.len() < 16384);
    }
    String::from_utf8(bytes).unwrap()
}

fn header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    request.lines().filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.trim())
}

async fn accept_upgrade(listener: TcpListener, proxy: bool) -> TcpStream {
    let (mut socket, _) = listener.accept().await.unwrap();
    let received = read_headers(&mut socket).await;
    assert!(received.starts_with(if proxy { "GET http://" } else { "GET /" }));
    assert!(header(&received, "x-nrd-auth") == Some("fixture-room-secret"));
    if !proxy { assert!(header(&received, "proxy-authorization").is_none()); }
    let accept = derive_accept_key(header(&received, "sec-websocket-key").unwrap().as_bytes());
    let mut response = format!("HTTP/1.1 101 Switching Protocols\r\nConnection: keep-alive, Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").into_bytes();
    // The first Engine.IO frame shares a single write with the HTTP response.
    response.extend_from_slice(&[0x81, 3, b'0', b'{', b'}']);
    socket.write_all(&response).await.unwrap();
    socket
}

async fn no_connection(listener: &TcpListener) {
    assert!(tokio::time::timeout(Duration::from_millis(150), listener.accept()).await.is_err());
}

#[test]
fn strict_upgrade_validation_accepts_tokens_and_rejects_unsolicited_or_ambiguous_headers() {
    let mut headers = HeaderMap::new();
    headers.insert(CONNECTION, "keep-alive, Upgrade".parse().unwrap());
    headers.insert(UPGRADE, "WebSocket".parse().unwrap());
    headers.insert(SEC_WEBSOCKET_ACCEPT, "fixture-accept".parse().unwrap());
    assert!(verify_upgrade(reqwest::Version::HTTP_11, &headers, "fixture-accept").is_ok());
    headers.append(CONNECTION, "keep-alive".parse().unwrap());
    assert!(verify_upgrade(reqwest::Version::HTTP_11, &headers, "fixture-accept").is_ok());
    assert!(verify_upgrade(reqwest::Version::HTTP_2, &headers, "fixture-accept").is_err());
    assert!(verify_upgrade(reqwest::Version::HTTP_11, &headers, "wrong").is_err());
    for name in [CONNECTION, UPGRADE, SEC_WEBSOCKET_ACCEPT] {
        let mut missing = headers.clone(); missing.remove(name);
        assert!(verify_upgrade(reqwest::Version::HTTP_11, &missing, "fixture-accept").is_err());
    }
    for name in [UPGRADE, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_EXTENSIONS, SEC_WEBSOCKET_PROTOCOL] {
        let mut invalid = headers.clone(); invalid.append(name, "fixture-server-secret".parse().unwrap());
        let error = verify_upgrade(reqwest::Version::HTTP_11, &invalid, "fixture-accept").unwrap_err();
        assert!(!error.contains("fixture"));
    }
}

#[test]
fn proxy_configuration_errors_never_echo_proxy_values() {
    for proxy in ["", "127.0.0.1:3128", "http://127.0.0.1:3128", "https://fixture-user:fixture-password@127.0.0.1:3128"] {
        assert!(validate_proxy_url(proxy).is_ok());
    }
    for proxy in ["socks5://fixture-user:fixture-password@127.0.0.1:3128", "http://127.0.0.1:3128/?fixture-token", "http://127.0.0.1:3128/#fixture-token", "http://127.0.0.1:3128/fixture-secret", "http://[fixture-invalid", " http://127.0.0.1:3128", "http://127.0.0.1:3128 ", "http://例子.invalid:3128"] {
        let error = validate_proxy_url(proxy).unwrap_err();
        assert!(!error.contains("fixture"));
        assert!(!error.contains("127.0.0.1"));
    }
}

#[tokio::test]
async fn upgrade_keeps_prefetched_frame_and_does_not_limit_socket_lifetime() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/socket.io/?fixture-query-secret", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let socket = accept_upgrade(listener, false).await;
        let mut socket = WebSocketStream::from_raw_socket(socket, Role::Server, None).await;
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(matches!(socket.next().await, Some(Ok(Message::Text(value))) if value == "fixture-client-frame"));
        socket.send(Message::Text("fixture-late-frame".into())).await.unwrap();
    });
    let http = test_client().build().unwrap();
    let mut socket = tokio::time::timeout(Duration::from_secs(1), connect_with_client(&http, request(&url))).await.unwrap().unwrap();
    assert!(matches!(socket.next().await, Some(Ok(Message::Text(value))) if value == "0{}"));
    // A one-second establishment deadline must not remain attached to the upgraded stream.
    tokio::time::sleep(Duration::from_millis(1300)).await;
    socket.send(Message::Text("fixture-client-frame".into())).await.unwrap();
    let message = tokio::time::timeout(Duration::from_secs(3), socket.next()).await.unwrap();
    assert!(matches!(message, Some(Ok(Message::Text(value))) if value == "fixture-late-frame"));
    server.await.unwrap();
}

#[tokio::test]
async fn proxy_refusal_never_falls_back_to_origin_or_echoes_server_content() {
    let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = proxy.accept().await.unwrap();
        let received = read_headers(&mut socket).await;
        assert!(received.starts_with("GET http://"));
        assert!(header(&received, "proxy-authorization").is_some());
        socket.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 19\r\n\r\nfixture-body-secret").await.unwrap();
    });
    let proxy = reqwest::Proxy::all(proxy_url).unwrap().basic_auth("fixture-user", "fixture-proxy-secret");
    let http = test_client().proxy(proxy).build().unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5),
        connect_with_client(&http, request(&format!("ws://{}/fixture-path-secret", origin.local_addr().unwrap()))))
        .await.expect("proxy refusal must not wait on a fallback origin");
    let error = result.err().expect("proxy refusal must fail");
    assert!(error.contains("407")); assert!(!error.contains("fixture"));
    no_connection(&origin).await;
    server.await.unwrap();
}

#[tokio::test]
async fn redirects_are_not_followed_with_room_credentials() {
    let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let destination = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/socket.io/", origin.local_addr().unwrap());
    let location = format!("http://{}/fixture-redirect-secret", destination.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = origin.accept().await.unwrap();
        let _ = read_headers(&mut socket).await;
        socket.write_all(format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n").as_bytes()).await.unwrap();
    });
    let result = tokio::time::timeout(Duration::from_secs(5),
        connect_with_client(&test_client().build().unwrap(), request(&url)))
        .await.expect("redirect must not wait on its destination");
    let error = result.err().expect("redirect must fail");
    assert!(error.contains("302")); assert!(!error.contains("fixture"));
    no_connection(&destination).await;
    server.await.unwrap();
}

// No global environment mutation: the subprocess has only synthetic loopback configuration.
#[test]
fn environment_policy_probe() {
    let Ok(url) = std::env::var("UU_WS_TEST_ENDPOINT") else { return; };
    assert!(url.starts_with("ws://127.0.0.1:"));
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        let mut socket = connect(request(&url), Duration::from_secs(5)).await.expect("fixture connection failed");
        let message = tokio::time::timeout(Duration::from_secs(3), socket.next()).await.unwrap();
        assert!(matches!(message, Some(Ok(Message::Text(value))) if value == "0{}"));
    });
}

#[tokio::test]
async fn environment_proxy_opt_in_and_no_proxy_match_rest_policy() {
    for (opt_in, bypass, via_proxy) in [(false, false, false), (true, false, true), (true, true, false)] {
        let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/socket.io/", origin.local_addr().unwrap());
        let proxy_url = format!("http://fixture-user:fixture-proxy-secret@{}", proxy.local_addr().unwrap());
        let (selected, unused) = if via_proxy { (proxy, origin) } else { (origin, proxy) };
        let server = tokio::spawn(async move { let _socket = accept_upgrade(selected, via_proxy).await; });
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap());
        child.args(["--exact", "ws_transport::tests::environment_policy_probe", "--nocapture"])
            .env_clear().env("UU_WS_TEST_ENDPOINT", url).env("HTTP_PROXY", proxy_url);
        if opt_in { child.env("UU_USE_PROXY", "1"); }
        if bypass { child.env("NO_PROXY", "127.0.0.1"); }
        let output = tokio::time::timeout(Duration::from_secs(10), child.output()).await.unwrap().unwrap();
        assert!(output.status.success(), "isolated proxy-policy subprocess failed");
        assert!(!output.stdout.windows(b"fixture-room-secret".len()).any(|s| s == b"fixture-room-secret"));
        assert!(!output.stderr.windows(b"fixture-proxy-secret".len()).any(|s| s == b"fixture-proxy-secret"));
        no_connection(&unused).await;
        server.await.unwrap();
    }
}

#[tokio::test]
async fn wss_connect_keeps_origin_auth_inside_tls_and_requires_trusted_certificate() {
    use std::sync::Arc;
    use tokio_rustls::{rustls, TlsAcceptor};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    // A fresh test certificate/key, kept only in memory. The mock does not relay anywhere:
    // it accepts CONNECT and then speaks TLS + WebSocket on that same loopback socket.
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_der = cert.serialize_der().unwrap();
    let root = reqwest::Certificate::from_der(&cert_der).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions().unwrap().with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(cert_der)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.serialize_private_key_der()))).unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    for trusted in [false, true] {
        let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = origin.local_addr().unwrap().port();
        let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
        let acceptor = acceptor.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = proxy.accept().await.unwrap();
            let received = read_headers(&mut socket).await;
            assert!(received.starts_with(&format!("CONNECT localhost:{port} HTTP/1.1\r\n")));
            assert!(header(&received, "x-nrd-auth").is_none());
            assert!(header(&received, "proxy-authorization").is_some());
            assert!(!received.contains("fixture-room-secret"));
            socket.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.unwrap();
            let tls = acceptor.accept(socket).await;
            if !trusted { assert!(tls.is_err()); return; }
            let mut tls = tls.expect("fixture TLS handshake failed");
            let received = read_headers(&mut tls).await;
            assert!(received.starts_with("GET /socket.io/"));
            assert!(header(&received, "x-nrd-auth") == Some("fixture-room-secret"));
            assert!(header(&received, "proxy-authorization").is_none());
            assert!(!received.contains("fixture-proxy-secret"));
            let accept = derive_accept_key(header(&received, "sec-websocket-key").unwrap().as_bytes());
            let mut response = format!("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").into_bytes();
            response.extend_from_slice(&[0x81, 3, b'0', b'{', b'}']);
            tls.write_all(&response).await.unwrap();
        });
        let proxy = reqwest::Proxy::all(proxy_url).unwrap().basic_auth("fixture-user", "fixture-proxy-secret");
        let mut builder = test_client().proxy(proxy);
        if trusted { builder = builder.add_root_certificate(root.clone()); }
        let client = builder.build().unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5),
            connect_with_client(&client, request(&format!("wss://localhost:{port}/socket.io/?fixture-query-secret")))).await.unwrap();
        if trusted {
            let mut socket = result.expect("trusted fixture WebSocket failed");
            let message = tokio::time::timeout(Duration::from_secs(3), socket.next()).await.unwrap();
            assert!(matches!(message, Some(Ok(Message::Text(value))) if value == "0{}"));
        } else {
            let error = result.err().expect("untrusted fixture certificate must fail");
            assert!(!error.contains("fixture")); assert!(!error.contains("localhost"));
        }
        no_connection(&origin).await;
        tokio::time::timeout(Duration::from_secs(5), server).await.unwrap().unwrap();
    }
}
