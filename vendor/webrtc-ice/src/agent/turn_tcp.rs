//! TURN over TCP/TLS (RFC 6062 framing as used by RFC 5766 §2.1), optionally through an HTTP
//! CONNECT proxy. Local patch for uu-remote-linux: upstream only gathers relays over UDP.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use util::Conn;

/// How relay connections over TCP reach the TURN server. Set once by the application.
#[derive(Clone, Debug, Default)]
pub struct RelayTransport {
    /// HTTP proxy `host:port` for CONNECT; None dials the TURN server directly.
    pub http_proxy: Option<String>,
}

static TRANSPORT: RwLock<Option<RelayTransport>> = RwLock::new(None);

pub fn set_relay_transport(transport: RelayTransport) {
    *TRANSPORT.write().unwrap() = Some(transport);
}

fn transport() -> RelayTransport {
    TRANSPORT.read().unwrap().clone().unwrap_or_default()
}

/// TCP/TLS relays are gathered only after the application opts in; UDP-only by default.
pub fn enabled() -> bool {
    TRANSPORT.read().unwrap().is_some()
}

trait Stream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Stream for T {}

/// Datagram view of a framed TURN stream: one STUN message or ChannelData per read.
pub struct FramedConn {
    reader: Mutex<ReadHalf<Box<dyn Stream>>>,
    writer: Mutex<WriteHalf<Box<dyn Stream>>>,
    local: SocketAddr,
    server: SocketAddr,
}

/// Opens TCP (optionally through the proxy) and TLS when `tls`, then frames it.
pub async fn dial(host: &str, port: u16, server: SocketAddr, tls: bool) -> Result<FramedConn, String> {
    let settings = transport();
    let target = format!("{host}:{port}");
    let tcp = match &settings.http_proxy {
        Some(proxy) => connect_via_proxy(proxy, &target).await?,
        None => TcpStream::connect(server).await.map_err(|e| format!("TURN TCP connect: {e}"))?,
    };
    let local = tcp.local_addr().map_err(|e| format!("TURN TCP local address: {e}"))?;
    let stream: Box<dyn Stream> = if tls {
        Box::new(tls_connect(tcp, host).await?)
    } else {
        Box::new(tcp)
    };
    let (reader, writer) = tokio::io::split(stream);
    Ok(FramedConn { reader: Mutex::new(reader), writer: Mutex::new(writer), local, server })
}

async fn connect_via_proxy(proxy: &str, target: &str) -> Result<TcpStream, String> {
    let mut tcp = TcpStream::connect(proxy).await.map_err(|e| format!("TURN proxy connect: {e}"))?;
    let request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
    tcp.write_all(request.as_bytes()).await.map_err(|e| format!("TURN proxy write: {e}"))?;
    // Read byte by byte up to the blank line so no tunnelled bytes are consumed.
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 8192 { return Err("TURN proxy response too large".into()); }
        let mut byte = [0u8; 1];
        if tcp.read(&mut byte).await.map_err(|e| format!("TURN proxy read: {e}"))? == 0 {
            return Err("TURN proxy closed the connection".into());
        }
        head.push(byte[0]);
    }
    let status = String::from_utf8_lossy(&head).lines().next().unwrap_or("").to_owned();
    match status.split_whitespace().nth(1) {
        Some("200") => Ok(tcp),
        code => Err(format!("TURN proxy CONNECT refused: status={}", code.unwrap_or("none"))),
    }
}

async fn tls_connect(tcp: TcpStream, host: &str)
    -> Result<tokio_rustls::client::TlsStream<TcpStream>, String> {
    use tokio_rustls::rustls::{self, client::WebPkiServerVerifier, pki_types::ServerName};
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    // System store first (it holds any CA the host's administrator added, e.g. for a TLS-inspecting
    // proxy, as OpenSSL would use), then the bundled Mozilla roots.
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let inner = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone()).build()
        .map_err(|e| format!("TURN TLS setup: {e}"))?;
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions().map_err(|e| format!("TURN TLS setup: {e}"))?
        .dangerous().with_custom_certificate_verifier(Arc::new(IpTolerant(inner))).with_no_client_auth();
    let name = match host.parse::<IpAddr>() {
        Ok(ip) => ServerName::IpAddress(ip.into()),
        Err(_) => ServerName::try_from(host.to_owned()).map_err(|_| "TURN TLS: invalid server name".to_owned())?,
    };
    tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp).await
        .map_err(|e| format!("TURN TLS handshake: {e}"))
}

/// Full chain verification; only a name mismatch is accepted, and only when the TURN URL is an
/// IP literal (cloud TURN URLs are IPs while certificates name the service). Media is protected
/// end to end by DTLS-SRTP with signalled fingerprints, not by this hop.
#[derive(Debug)]
struct IpTolerant(Arc<tokio_rustls::rustls::client::WebPkiServerVerifier>);

impl tokio_rustls::rustls::client::danger::ServerCertVerifier for IpTolerant {
    fn verify_server_cert(&self, end_entity: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        intermediates: &[tokio_rustls::rustls::pki_types::CertificateDer<'_>],
        server_name: &tokio_rustls::rustls::pki_types::ServerName<'_>, ocsp: &[u8],
        now: tokio_rustls::rustls::pki_types::UnixTime)
        -> Result<tokio_rustls::rustls::client::danger::ServerCertVerified, tokio_rustls::rustls::Error> {
        use tokio_rustls::rustls::{CertificateError, Error, pki_types::ServerName};
        match self.0.verify_server_cert(end_entity, intermediates, server_name, ocsp, now) {
            Err(Error::InvalidCertificate(CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. }))
                if matches!(server_name, ServerName::IpAddress(_)) =>
                Ok(tokio_rustls::rustls::client::danger::ServerCertVerified::assertion()),
            other => other,
        }
    }
    fn verify_tls12_signature(&self, message: &[u8], cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        dss: &tokio_rustls::rustls::DigitallySignedStruct)
        -> Result<tokio_rustls::rustls::client::danger::HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }
    fn verify_tls13_signature(&self, message: &[u8], cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        dss: &tokio_rustls::rustls::DigitallySignedStruct)
        -> Result<tokio_rustls::rustls::client::danger::HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

/// Total on-the-wire length of the frame starting with `header` (4 bytes), including
/// ChannelData padding to 4 bytes, which TCP carries (RFC 5766 §11.5).
pub fn frame_len(header: [u8; 4]) -> Option<usize> {
    let length = u16::from_be_bytes([header[2], header[3]]) as usize;
    match header[0] >> 6 {
        0 => Some(20 + length),         // STUN: 20-byte header + attributes
        1 => Some(4 + (length + 3) / 4 * 4), // ChannelData 0x4000..0x7FFF
        _ => None,
    }
}

/// An HTTP reply inside the relay stream means a proxy is inspecting the (TLS) content;
/// "HT" would otherwise parse as a ChannelData header.
pub fn is_http_reply(header: [u8; 4]) -> bool {
    &header == b"HTTP"
}

#[async_trait]
impl Conn for FramedConn {
    async fn connect(&self, _: SocketAddr) -> util::Result<()> {
        Err(util::Error::Other("TURN TCP connection is already connected".into()))
    }
    async fn recv(&self, buf: &mut [u8]) -> util::Result<usize> {
        self.recv_from(buf).await.map(|(n, _)| n)
    }
    async fn recv_from(&self, buf: &mut [u8]) -> util::Result<(usize, SocketAddr)> {
        let mut reader = self.reader.lock().await;
        let mut header = [0u8; 4];
        reader.read_exact(&mut header).await?;
        if is_http_reply(header) {
            return Err(util::Error::Other(
                "TURN TCP: got an HTTP response instead of TURN; a proxy is inspecting the TLS content, TURN cannot pass through it".into(),
            ));
        }
        let total = frame_len(header).ok_or_else(|| util::Error::Other("TURN TCP: unknown frame type".into()))?;
        let mut frame = vec![0u8; total];
        frame[..4].copy_from_slice(&header);
        reader.read_exact(&mut frame[4..]).await?;
        if frame.len() > buf.len() {
            return Err(util::Error::Other("TURN TCP: frame larger than the read buffer".into()));
        }
        buf[..frame.len()].copy_from_slice(&frame);
        Ok((frame.len(), self.server))
    }
    async fn send(&self, buf: &[u8]) -> util::Result<usize> {
        let mut writer = self.writer.lock().await;
        writer.write_all(buf).await?;
        writer.flush().await?;
        Ok(buf.len())
    }
    async fn send_to(&self, buf: &[u8], _: SocketAddr) -> util::Result<usize> {
        self.send(buf).await
    }
    fn local_addr(&self) -> util::Result<SocketAddr> {
        Ok(self.local)
    }
    fn remote_addr(&self) -> Option<SocketAddr> {
        Some(self.server)
    }
    async fn close(&self) -> util::Result<()> {
        let _ = self.writer.lock().await.shutdown().await;
        Ok(())
    }
}
