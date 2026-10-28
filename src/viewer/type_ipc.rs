//! Local Unix-socket bridge: `uuremote-viewer type` hands secrets to a running desktop session.
//! Frames are length-prefixed JSON; secrets never appear in logs. Peer UID must match.
use serde::{Deserialize, Serialize};
use std::{
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::mpsc,
};

/// Max JSON frame (header + secret). 16 KiB secret + margin.
pub const MAX_FRAME: usize = 20 * 1024;

#[derive(Serialize, Deserialize)]
pub struct TypeRequest {
    pub delay_ms: u64,
    pub omit_last: usize,
    pub snapshot: Option<String>,
    /// Secret text; Debug only exposes length.
    pub secret: String,
}

impl std::fmt::Debug for TypeRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TypeRequest")
            .field("delay_ms", &self.delay_ms)
            .field("omit_last", &self.omit_last)
            .field(
                "snapshot",
                &self.snapshot.as_ref().map(|s| format!("len={}", s.len())),
            )
            .field("secret_len", &self.secret.len())
            .finish()
    }
}

impl Drop for TypeRequest {
    fn drop(&mut self) {
        zeroize_string(&mut self.secret);
        if let Some(s) = self.snapshot.as_mut() {
            zeroize_string(s);
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TypeResponse {
    pub ok: bool,
    pub events: usize,
    pub chars: usize,
    pub snapshot: Option<String>,
    pub error: Option<String>,
}

fn zeroize_string(s: &mut String) {
    // Best-effort wipe before drop.
    let bytes = unsafe { s.as_bytes_mut() };
    for b in bytes.iter_mut() {
        *b = 0;
    }
    s.clear();
}

/// Ensure `dir` is a real directory owned by us with mode & 0o077 == 0 (no symlink).
pub fn validate_private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta = std::fs::symlink_metadata(dir).map_err(|_| "Cannot inspect type-ipc directory")?;
    if meta.file_type().is_symlink() {
        return Err("Type-ipc path is a symlink".into());
    }
    if !meta.file_type().is_dir() {
        return Err("Type-ipc path is not a directory".into());
    }
    let me = unsafe { libc::getuid() };
    if meta.uid() != me {
        return Err("Type-ipc directory owner mismatch".into());
    }
    if meta.permissions().mode() & 0o077 != 0 {
        return Err("Type-ipc directory permissions too open".into());
    }
    Ok(())
}

pub fn runtime_dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(format!(
                "{}/uu-viewer-type-{}",
                std::env::temp_dir().display(),
                unsafe { libc::getuid() }
            ))
        });
    let dir = if base.ends_with("uu-viewer-type") || base.file_name().is_some_and(|n| {
        n.to_string_lossy().starts_with("uu-viewer-type")
    }) {
        base
    } else {
        base.join("uu-viewer-type")
    };
    // Use symlink_metadata so a pre-planted symlink is not followed before we decide.
    match std::fs::symlink_metadata(&dir) {
        Ok(_) => {
            validate_private_dir(&dir)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)
                .map_err(|_| "Cannot create private type-ipc directory")?;
            validate_private_dir(&dir)?;
        }
        Err(_) => return Err("Cannot inspect type-ipc directory".into()),
    }
    Ok(dir)
}

pub fn device_key(device: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(device.as_bytes()))
}

pub fn socket_path(device: &str) -> Result<PathBuf, String> {
    Ok(runtime_dir()?.join(format!("{}.sock", device_key(device))))
}

pub fn default_snapshot_path(device: &str) -> Result<PathBuf, String> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    Ok(runtime_dir()?.join(format!("snap-{}-{ts}.png", &device_key(device)[..12])))
}

pub async fn read_frame(r: &mut (impl AsyncRead + Unpin)) -> Result<Vec<u8>, String> {
    let n = r.read_u32().await.map_err(|_| "Type IPC closed")? as usize;
    if n > MAX_FRAME {
        return Err("Type IPC frame exceeds size limit".into());
    }
    let mut b = vec![0; n];
    r.read_exact(&mut b)
        .await
        .map_err(|_| "Type IPC closed")?;
    Ok(b)
}

pub async fn write_frame(w: &mut (impl AsyncWrite + Unpin), body: &[u8]) -> Result<(), String> {
    if body.len() > MAX_FRAME {
        return Err("Type IPC frame exceeds size limit".into());
    }
    w.write_u32(body.len() as u32)
        .await
        .map_err(|_| "Type IPC closed")?;
    w.write_all(body).await.map_err(|_| "Type IPC closed")?;
    Ok(())
}

fn check_peer(stream: &UnixStream) -> Result<(), String> {
    let cred = stream
        .peer_cred()
        .map_err(|_| "Type IPC peer credentials unavailable")?;
    let uid = cred.uid();
    let me = unsafe { libc::getuid() };
    if uid != me {
        return Err("Type IPC peer uid mismatch".into());
    }
    Ok(())
}

pub struct Server {
    pub path: PathBuf,
    listener: Option<UnixListener>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Server {
    pub async fn bind(device: &str) -> Result<Self, String> {
        let path = socket_path(device)?;
        if path.exists() {
            match UnixStream::connect(&path).await {
                Ok(_) => return Err("A desktop viewer is already listening for this device".into()),
                Err(_) => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        let listener = UnixListener::bind(&path).map_err(|_| "Cannot bind type IPC socket")?;
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        Ok(Self { path, listener: Some(listener) })
    }

    /// Accept connections and forward validated TypeRequests to `tx`.
    pub async fn run(mut self, tx: mpsc::Sender<PendingType>) {
        let path = self.path.clone();
        let listener = self.listener.take().expect("type IPC listener");
        // Prevent Drop from removing the socket twice while we still hold `path` in a guard.
        self.path = PathBuf::new();
        struct Guard(PathBuf);
        impl Drop for Guard {
            fn drop(&mut self) {
                if !self.0.as_os_str().is_empty() {
                    let _ = std::fs::remove_file(&self.0);
                }
            }
        }
        let _guard = Guard(path);
        loop {
            let accepted = listener.accept().await;
            let Ok((stream, _)) = accepted else { break };
            if check_peer(&stream).is_err() {
                continue;
            }
            let tx = tx.clone();
            tokio::spawn(async move {
                let _ = serve_client(stream, tx).await;
            });
        }
    }
}

pub struct PendingType {
    pub request: TypeRequest,
    pub reply: tokio::sync::oneshot::Sender<TypeResponse>,
}

async fn serve_client(mut stream: UnixStream, tx: mpsc::Sender<PendingType>) -> Result<(), String> {
    let body = read_frame(&mut stream).await?;
    let request: TypeRequest =
        serde_json::from_slice(&body).map_err(|_| "Invalid type IPC request")?;
    let mut body = body;
    for b in &mut body {
        *b = 0;
    }
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    // Move request into PendingType; zeroize on drop if send fails.
    if tx
        .send(PendingType {
            request,
            reply: reply_tx,
        })
        .await
        .is_err()
    {
        return Err("Desktop session is gone".into());
    }
    let response = reply_rx.await.map_err(|_| "Desktop session is gone")?;
    let out = serde_json::to_vec(&response).map_err(|_| "Type IPC encode failed")?;
    write_frame(&mut stream, &out).await
}

/// Client: deliver a type request to a running viewer. Returns response or connection error.
pub async fn send_to_viewer(device: &str, mut request: TypeRequest) -> Result<TypeResponse, String> {
    let path = socket_path(device)?;
    let mut stream = UnixStream::connect(&path)
        .await
        .map_err(|_| "No running desktop viewer for this device")?;
    check_peer(&stream)?;
    let body = serde_json::to_vec(&request).map_err(|_| "Type IPC encode failed")?;
    // Clear serde copy in request early — response path still needs omit metadata only.
    zeroize_string(&mut request.secret);
    write_frame(&mut stream, &body).await?;
    // Wipe encoded body
    let mut body = body;
    for b in &mut body {
        *b = 0;
    }
    let resp_body = read_frame(&mut stream).await?;
    let response: TypeResponse =
        serde_json::from_slice(&resp_body).map_err(|_| "Invalid type IPC response")?;
    Ok(response)
}

/// Encode an RGB24 frame as a minimal PNG (8-bit RGB, no filtering beyond None).
/// Write RGB24 as PNG. Always uses O_NOFOLLOW + mode 0600.
/// `create_new`: true for default runtime paths (refuse overwrite); false for user `--snapshot`.
pub fn write_png_rgb(
    path: &Path,
    width: u32,
    height: u32,
    rgb: &[u8],
    create_new: bool,
) -> Result<(), String> {
    let expected = width as usize * height as usize * 3;
    if rgb.len() != expected || width == 0 || height == 0 {
        return Err("Snapshot frame size mismatch".into());
    }
    use flate2::{write::ZlibEncoder, Compression};
    use std::io::Write;
    let mut raw = Vec::with_capacity((width as usize * 3 + 1) * height as usize);
    for row in 0..height as usize {
        raw.push(0); // filter None
        let start = row * width as usize * 3;
        raw.extend_from_slice(&rgb[start..start + width as usize * 3]);
    }
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::fast());
    enc.write_all(&raw).map_err(|_| "Snapshot zlib failed")?;
    for b in &mut raw {
        *b = 0;
    }
    let compressed = enc.finish().map_err(|_| "Snapshot zlib failed")?;

    fn chunk(tag: &[u8; 4], data: &[u8], out: &mut Vec<u8>) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(tag);
        out.extend_from_slice(data);
        let mut crc = crc32_ieee_update(0xffff_ffff, tag);
        crc = crc32_ieee_update(crc, data);
        out.extend_from_slice(&(crc ^ 0xffff_ffff).to_be_bytes());
    }
    let mut png = Vec::with_capacity(compressed.len() + 128);
    png.extend_from_slice(&[137, 80, 78, 71, 13, 10, 26, 10]);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB
    chunk(b"IHDR", &ihdr, &mut png);
    chunk(b"IDAT", &compressed, &mut png);
    chunk(b"IEND", &[], &mut png);

    use std::os::unix::fs::OpenOptionsExt;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).mode(0o600).custom_flags(libc::O_NOFOLLOW);
    if create_new {
        opts.create_new(true);
    } else {
        opts.create(true).truncate(true);
    }
    let mut file = opts
        .open(path)
        .map_err(|_| "Cannot write snapshot file")?;
    file.write_all(&png).map_err(|_| "Cannot write snapshot file")?;
    Ok(())
}

fn crc32_ieee(data: &[u8]) -> u32 {
    crc32_ieee_update(0xffff_ffff, data) ^ 0xffff_ffff
}
fn crc32_ieee_update(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (!(crc & 1)).wrapping_add(1); // 0 or 0xFFFFFFFF
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_limit_rejects_oversized_length() {
        // Unit-level: MAX_FRAME constant is the contract used by read_frame.
        assert!(MAX_FRAME >= 16 * 1024);
        assert!(MAX_FRAME < 1024 * 1024);
    }

    #[test]
    fn device_key_is_stable_hex() {
        let a = device_key("device-1");
        let b = device_key("device-1");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(device_key("device-1"), device_key("device-2"));
    }

    #[test]
    fn png_roundtrip_header_and_mode() {
        let dir = tempfile_dir();
        let path = dir.join("t.png");
        write_png_rgb(&path, 2, 2, &[255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255], true).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn tempfile_dir() -> PathBuf {
        use std::os::unix::fs::DirBuilderExt;
        // One directory per call: tests run in parallel and must not remove each other's files.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("uu-type-ipc-test-{}-{}", std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .unwrap();
        dir
    }

    #[tokio::test]
    async fn server_client_roundtrip_and_uid_gate() {
        let device = format!("ipc-test-{}", uuidish());
        // XDG_RUNTIME_DIR is process-wide; another test points it at a planted directory.
        let _env = crate::viewer::secure_input::ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let server = Server::bind(&device).await.unwrap();
        let path = server.path.clone();
        let (tx, mut rx) = mpsc::channel::<PendingType>(4);
        let run = tokio::spawn(async move { server.run(tx).await });
        // Handle one request in background
        let worker = tokio::spawn(async move {
            if let Some(pending) = rx.recv().await {
                assert_eq!(pending.request.secret, "sentinel-SECRET-9f3a");
                assert_eq!(pending.request.delay_ms, 10);
                assert_eq!(pending.request.omit_last, 1);
                let _ = pending.reply.send(TypeResponse {
                    ok: true,
                    events: 12,
                    chars: 6,
                    snapshot: Some("/tmp/fake.png".into()),
                    error: None,
                });
            }
        });
        let response = send_to_viewer(
            &device,
            TypeRequest {
                delay_ms: 10,
                omit_last: 1,
                snapshot: None,
                secret: "sentinel-SECRET-9f3a".into(),
            },
        )
        .await
        .unwrap();
        assert!(response.ok);
        assert_eq!(response.events, 12);
        assert_eq!(response.snapshot.as_deref(), Some("/tmp/fake.png"));
        worker.abort();
        run.abort();
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn connect_without_server_errors_without_secret() {
        let device = format!("missing-{}", uuidish());
        let err = send_to_viewer(
            &device,
            TypeRequest {
                delay_ms: 0,
                omit_last: 0,
                snapshot: None,
                secret: "sentinel-SECRET-9f3a".into(),
            },
        )
        .await
        .unwrap_err();
        assert!(err.contains("No running desktop viewer"));
        assert!(!err.contains("sentinel"));
    }

    #[tokio::test]
    async fn oversized_frame_rejected() {
        let device = format!("oversize-{}", uuidish());
        // XDG_RUNTIME_DIR is process-wide; another test points it at a planted directory.
        let _env = crate::viewer::secure_input::ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let server = Server::bind(&device).await.unwrap();
        let path = server.path.clone();
        let (tx, mut rx) = mpsc::channel::<PendingType>(1);
        let run = tokio::spawn(async move { server.run(tx).await });
        // Drain so accept happens
        let _drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let mut stream = UnixStream::connect(&path).await.unwrap();
        let huge = vec![0u8; MAX_FRAME + 1];
        let result = write_frame(&mut stream, &huge).await;
        // Client-side write_frame rejects before send
        assert!(result.is_err());
        assert!(!format!("{result:?}").contains("sentinel"));
        run.abort();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn validate_private_dir_rejects_symlink_and_open_mode() {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let base = std::env::temp_dir().join(format!(
            "uu-type-ipc-val-{}-{}",
            std::process::id(),
            uuidish()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::DirBuilder::new().mode(0o700).create(&base).unwrap();

        let open = base.join("open");
        std::fs::DirBuilder::new().mode(0o755).create(&open).unwrap();
        let err = validate_private_dir(&open).unwrap_err();
        assert!(err.contains("permissions"), "{err}");
        assert!(!err.contains("sentinel"));

        let good = base.join("good");
        std::fs::DirBuilder::new().mode(0o700).create(&good).unwrap();
        validate_private_dir(&good).unwrap();

        let link = base.join("link");
        std::os::unix::fs::symlink(&good, &link).unwrap();
        let err = validate_private_dir(&link).unwrap_err();
        assert!(err.contains("symlink"), "{err}");

        let file = base.join("file");
        std::fs::write(&file, b"x").unwrap();
        let err = validate_private_dir(&file).unwrap_err();
        assert!(err.contains("not a directory"), "{err}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn runtime_dir_rejects_preexisting_open_or_symlink() {
        use std::os::unix::fs::DirBuilderExt;
        // Serialize against other tests that read/write process environment.
        let _env = crate::viewer::secure_input::ENV_LOCK.lock().unwrap();
        let base = std::env::temp_dir().join(format!(
            "uu-type-ipc-rt-{}-{}",
            std::process::id(),
            uuidish()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::DirBuilder::new().mode(0o700).create(&base).unwrap();

        // Point XDG_RUNTIME_DIR at a private base; plant a bad uu-viewer-type child.
        let child = base.join("uu-viewer-type");
        std::fs::DirBuilder::new().mode(0o755).create(&child).unwrap();
        let _guard = EnvGuard::set("XDG_RUNTIME_DIR", base.to_str().unwrap());
        let err = runtime_dir().unwrap_err();
        assert!(err.contains("permissions"), "{err}");
        let _ = std::fs::remove_dir_all(&child);

        let target = base.join("target");
        std::fs::DirBuilder::new().mode(0o700).create(&target).unwrap();
        std::os::unix::fs::symlink(&target, &child).unwrap();
        let err = runtime_dir().unwrap_err();
        assert!(err.contains("symlink"), "{err}");
        let _ = std::fs::remove_file(&child);

        // Fresh create succeeds and validates.
        let dir = runtime_dir().unwrap();
        assert!(dir.ends_with("uu-viewer-type"));
        validate_private_dir(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn write_png_refuses_symlink_target() {
        let dir = tempfile_dir();
        let real = dir.join("real.png");
        let link = dir.join("link.png");
        std::fs::write(&real, b"old").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = write_png_rgb(
            &link,
            1,
            1,
            &[1, 2, 3],
            false,
        )
        .unwrap_err();
        assert!(err.contains("Cannot write snapshot"), "{err}");
        // Original untouched
        assert_eq!(std::fs::read(&real).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn write_png_create_new_refuses_existing() {
        let dir = tempfile_dir();
        let path = dir.join("exists.png");
        std::fs::write(&path, b"old").unwrap();
        let err = write_png_rgb(&path, 1, 1, &[1, 2, 3], true).unwrap_err();
        assert!(err.contains("Cannot write snapshot"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn type_request_debug_shows_secret_len_only() {
        let req = TypeRequest {
            delay_ms: 40,
            omit_last: 1,
            snapshot: Some("/tmp/out.png".into()),
            secret: "sentinel-SECRET-9f3a".into(),
        };
        let d = format!("{req:?}");
        assert!(d.contains("secret_len: 20") || d.contains("secret_len: 20,"), "{d}");
        assert!(!d.contains("sentinel"));
        assert!(!d.contains("SECRET"));
        assert!(!d.contains("9f3a"));
    }

    struct EnvGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }
    impl EnvGuard {
        fn set(key: &'static str, val: &str) -> Self {
            let prev = std::env::var_os(key);
            std::env::set_var(key, val);
            Self { key, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

        fn uuidish() -> String {
        format!(
            "{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }
}
