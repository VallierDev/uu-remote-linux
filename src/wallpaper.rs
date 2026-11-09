//! Official device-card background reporting. Protocol evidence: docs/wallpaper-protocol.md.
use std::{path::PathBuf, sync::Arc, time::Duration};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use crate::auth::UUAuthClient;

const LIMIT: usize = 10 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(20);

pub fn start(auth: Arc<UUAuthClient>) -> tokio::task::AbortHandle {
    tokio::spawn(async move {
        if auth.creds.user_id.is_empty() || crate::desktop_backend::headless()
            || std::env::var("UU_WALLPAPER_UPLOAD").as_deref() == Ok("0") { return; }
        let mut previous = None;
        loop {
            if let Some(path) = source().await {
                match read_image(&path).await {
                    Ok(image) => {
                        let hash = Sha256::digest(&image).to_vec();
                        if previous.as_ref() != Some(&hash) {
                            match publish(&auth, image).await {
                                Ok(()) => { previous = Some(hash); log::info!("Device wallpaper synchronized"); }
                                Err(stage) => log::warn!("Device wallpaper synchronization failed: stage={stage}; will retry"),
                            }
                        }
                    }
                    Err(()) => log::debug!("Device wallpaper unavailable or unsupported"),
                }
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    }).abort_handle()
}

async fn command(program: &str, args: &[&str]) -> Option<String> {
    let output = tokio::time::timeout(Duration::from_secs(3), tokio::process::Command::new(program)
        .args(args).kill_on_drop(true).output()).await.ok()?.ok()?;
    output.status.success().then(|| String::from_utf8(output.stdout).ok()).flatten()
}

async fn kde_config(programs: &[&str], args: &[&str]) -> Option<String> {
    for program in programs {
        if let Some(value) = command(program, args).await { return Some(value); }
    }
    None
}

async fn source() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("UU_WALLPAPER") {
        let path = PathBuf::from(path); return path.is_absolute().then_some(path);
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_lowercase();
    if desktop.split(':').any(|name| name == "hyprland") {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let xdg = |name, fallback| std::env::var_os(name).map(PathBuf::from)
            .filter(|p| p.is_absolute()).or_else(|| home.as_ref().map(|p| p.join(fallback)));
        return omarchy_background(xdg("XDG_STATE_HOME", ".local/state").as_deref(),
            xdg("XDG_CONFIG_HOME", ".config").as_deref()).await;
    }
    if desktop.contains("gnome") || desktop.contains("unity") || desktop.contains("cinnamon") {
        let schema = if desktop.contains("cinnamon") { "org.cinnamon.desktop.background" } else { "org.gnome.desktop.background" };
        if schema == "org.gnome.desktop.background" && command("gsettings", &["get", "org.gnome.desktop.interface", "color-scheme"])
            .await.is_some_and(|v| v.contains("prefer-dark")) {
            if let Some(path) = command("gsettings", &["get", schema, "picture-uri-dark"]).await.and_then(|v| gsettings_path(&v)) { return Some(path); }
        }
        return command("gsettings", &["get", schema, "picture-uri"]).await.and_then(|v| gsettings_path(&v));
    }
    if desktop.contains("kde") {
        let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
            .filter(|p| p.is_absolute()).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        let text = tokio::fs::read_to_string(config.join("plasma-org.kde.plasma.desktop-appletsrc")).await.unwrap_or_default();
        if let Some(path) = kde_image(&text) { return package_image(path).await; }
        // Plasma omits Image for the look-and-feel's default wallpaper.
        if !kde_default_allowed(&text) { return None; }
        let readers = ["kreadconfig6", "kreadconfig5"];
        let theme = kde_config(&readers, &["--file", "kdeglobals", "--group", "KDE", "--key", "LookAndFeelPackage"])
            .await.unwrap_or_default();
        let theme = if theme.trim().is_empty() { "org.kde.breeze.desktop" } else { theme.trim() };
        if theme.contains('/') || theme.contains("..") { return None; }
        let defaults = format!("/usr/share/plasma/look-and-feel/{theme}/contents/defaults");
        let image = kde_config(&readers, &["--file", &defaults, "--group", "Wallpaper", "--key", "Image"]).await?;
        let image = image.trim();
        if image.is_empty() || image.contains('/') || image.contains("..") { return None; }
        return package_image(PathBuf::from("/usr/share/wallpapers").join(image)).await;
    }
    if desktop.contains("xfce") {
        let text = command("xfconf-query", &["-c", "xfce4-desktop", "-lv"]).await?;
        for line in text.lines() {
            if let Some((key, value)) = line.trim().split_once(char::is_whitespace) {
                if key.ends_with("/last-image") { if let Some(path) = local_path(value) { return Some(path); } }
            }
        }
    }
    None
}

// Omarchy maintains this link when changing wallpaper/theme. Prefer its current
// XDG state location, while supporting older installations using XDG config.
async fn omarchy_background(state: Option<&std::path::Path>, config: Option<&std::path::Path>) -> Option<PathBuf> {
    for root in [state, config].into_iter().flatten() {
        let path = root.join("omarchy/current/background");
        if tokio::fs::metadata(&path).await.is_ok_and(|meta| meta.is_file()) { return Some(path); }
    }
    None
}

async fn package_image(path: PathBuf) -> Option<PathBuf> {
    if tokio::fs::metadata(&path).await.ok()?.is_file() { return Some(path); }
    let mut entries = tokio::fs::read_dir(path.join("contents/images")).await.ok()?;
    let mut candidates = Vec::new();
    while let Some(entry) = entries.next_entry().await.ok()? {
        if !tokio::fs::metadata(entry.path()).await.is_ok_and(|meta| meta.is_file()) { continue; }
        let path = entry.path();
        let Some((w,h)) = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.split_once('x'))
            .and_then(|(w,h)| Some((w.parse::<u32>().ok()?,h.parse::<u32>().ok()?))) else { continue; };
        candidates.push(((w < h, u64::from(w) * u64::from(h)), path));
    }
    candidates.sort_by_key(|(score, _)| *score);
    candidates.into_iter().next().map(|(_, path)| path)
}

fn kde_image(text: &str) -> Option<PathBuf> {
    let plugins = kde_plugins(text);
    let mut image_section = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            image_section = line.strip_prefix("[Containments][").and_then(|s| s.split_once(']'))
                .is_some_and(|(id, section)| section == "[Wallpaper][org.kde.image][General]"
                    && plugins.get(id).map_or(true, |plugin| *plugin == KdeWallpaper::Image));
        }
        if image_section { if let Some(value) = line.strip_prefix("Image=") { if let Some(path) = local_path(value) { return Some(path); } } }
    }
    None
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum KdeWallpaper { Image, Unsupported }

fn kde_plugins(text: &str) -> std::collections::HashMap<&str, KdeWallpaper> {
    let mut containment = None;
    let mut plugins = std::collections::HashMap::new();
    let mut panels = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            containment = line.strip_prefix("[Containments][").and_then(|s| s.split_once(']'))
                .filter(|(_, section)| section.is_empty()).map(|(id, _)| id);
        }
        if let Some(id) = containment {
            plugins.entry(id).or_insert(KdeWallpaper::Image);
            if line.strip_prefix("plugin=").is_some_and(|plugin| plugin.trim() == "org.kde.panel") { panels.push(id); }
            if let Some(plugin) = line.strip_prefix("wallpaperplugin=") {
                plugins.insert(id, if plugin.trim() == "org.kde.image" { KdeWallpaper::Image } else { KdeWallpaper::Unsupported });
            }
        }
    }
    // Panels can keep an image wallpaper setting, but do not supply a desktop background.
    for id in panels { plugins.insert(id, KdeWallpaper::Unsupported); }
    plugins
}

fn kde_default_allowed(text: &str) -> bool {
    let plugins = kde_plugins(text);
    if !plugins.is_empty() { return plugins.values().any(|plugin| *plugin == KdeWallpaper::Image); }
    !text.contains("[Wallpaper][") || text.contains("[Wallpaper][org.kde.image]")
}

fn gsettings_path(value: &str) -> Option<PathBuf> {
    // gsettings prints a GVariant string, with either quote style and C/Unicode escapes.
    // https://docs.gtk.org/glib/gvariant-text-format.html#strings
    let mut chars = value.trim().chars();
    let quote = chars.next()?;
    if quote != '\'' && quote != '"' { return None; }
    let mut decoded = String::new();
    while let Some(ch) = chars.next() {
        if ch == quote { return chars.as_str().is_empty().then(|| local_path(&decoded)).flatten(); }
        if ch != '\\' { decoded.push(ch); continue; }
        let escaped = chars.next()?;
        decoded.push(match escaped {
            'a' => '\x07', 'b' => '\x08', 'f' => '\x0c', 'n' => '\n',
            'r' => '\r', 't' => '\t', 'v' => '\x0b', '\\' => '\\', '\'' => '\'', '"' => '"',
            'u' | 'U' => {
                let mut code = 0u32;
                for _ in 0..if escaped == 'u' { 4 } else { 8 } { code = u32::checked_add(code.checked_mul(16)?, chars.next()?.to_digit(16)?)?; }
                char::from_u32(code).filter(|c| *c != '\0')?
            }
            '\n' => continue,
            _ => escaped,
        });
    }
    None
}

fn local_path(value: &str) -> Option<PathBuf> {
    let value = value.trim();
    let path = if value.starts_with("file:") {
        let url = reqwest::Url::parse(value).ok()?;
        if url.query().is_some() || url.fragment().is_some() { return None; }
        url.to_file_path().ok()?
    } else { PathBuf::from(value) };
    path.is_absolute().then_some(path)
}

async fn read_image(path: &std::path::Path) -> Result<Vec<u8>, ()> {
    if !tokio::fs::metadata(path).await.map_err(|_| ())?.is_file() { return Err(()); }
    let file = tokio::fs::File::open(path).await.map_err(|_| ())?;
    if !file.metadata().await.map_err(|_| ())?.is_file() { return Err(()); }
    let mut image = Vec::new();
    file.take((LIMIT + 1) as u64).read_to_end(&mut image).await.map_err(|_| ())?;
    if image.len() > LIMIT || image_type(&image).is_none() { return Err(()); }
    Ok(image)
}

fn image_type(image: &[u8]) -> Option<&'static str> {
    if image.starts_with(b"\x89PNG\r\n\x1a\n") { Some("image/png") }
    else if image.starts_with(b"\xff\xd8\xff") { Some("image/jpeg") }
    else if image.starts_with(b"RIFF") && image.get(8..12) == Some(b"WEBP") { Some("image/webp") }
    else { None }
}

fn api_data(status: u16, body: &serde_json::Value) -> Result<&serde_json::Value, ()> {
    if status != 200 || body.get("code").and_then(|v| v.as_i64()) != Some(0) { return Err(()); }
    body.get("data").ok_or(())
}

async fn publish(auth: &UUAuthClient, image: Vec<u8>) -> Result<(), &'static str> {
    let http = crate::auth::http_client_builder().redirect(reqwest::redirect::Policy::none()).build().map_err(|_| "http-client")?;
    publish_to(auth, image, crate::auth::BASE_URL, &http).await
}

async fn json(request: reqwest::RequestBuilder) -> Result<(u16, serde_json::Value), ()> {
    let mut response = request.timeout(TIMEOUT).send().await.map_err(|_| ())?;
    let status = response.status().as_u16();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        if bytes.len() + chunk.len() > 65536 { return Err(()); }
        bytes.extend_from_slice(&chunk);
    }
    Ok((status, serde_json::from_slice(&bytes).map_err(|_| ())?))
}

async fn publish_to(auth: &UUAuthClient, image: Vec<u8>, base: &str, http: &reqwest::Client) -> Result<(), &'static str> {
    let path = "/api/v1/tool/fp/token?filetype=1";
    let result = json(http.get(format!("{base}{path}")).headers(auth.build_headers("GET", path, ""))).await.map_err(|_| "token-request")?;
    let data = api_data(result.0, &result.1).map_err(|_| "token-response")?;
    let token = data.get("token").and_then(|v| v.as_str()).filter(|v| !v.is_empty()).ok_or("file-token")?;
    let url = data.get("req_url").and_then(|v| v.as_str()).ok_or("upload-url")?;
    let url = https_url(url).map_err(|_| "upload-url")?;
    let max = data.get("max_size").and_then(|v| v.as_u64()).filter(|n| *n > 0).unwrap_or(LIMIT as u64);
    if image.len() as u64 > max { return Err("file-size"); }
    // Filepicker takes raw bytes and its own Authorization value, never account headers.
    let response = json(http.post(url).header("Authorization", token)
        .header("Content-Type", image_type(&image).ok_or("file-type")?).body(image)
    ).await.map_err(|_| "file-upload")?;
    if response.0 != 200 { return Err("file-upload-status"); }
    let url = response.1.get("url").and_then(|v| v.as_str()).ok_or("file-response")?;
    https_url(url).map_err(|_| "file-response-url")?;
    let path = "/api/v1/device/wallpaper";
    let body = serde_json::json!({"wallpaper_url": url}).to_string();
    let result = json(http.post(format!("{base}{path}")).headers(auth.build_headers("POST", path, &body)).body(body)).await.map_err(|_| "report-request")?;
    if result.0 != 200 || result.1.get("code").and_then(|v| v.as_i64()) != Some(0) { return Err("report-response"); }
    Ok(())
}

fn https_url(value: &str) -> Result<reqwest::Url, ()> {
    let url = reqwest::Url::parse(value).map_err(|_| ())?;
    if url.scheme() != "https" || url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() { return Err(()); }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    #[tokio::test]
    async fn kde_config_uses_plasma5_when_plasma6_reader_is_unavailable() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("uu-kreadconfig-test-{}-{}", std::process::id(), rand::random::<u64>()));
        std::fs::create_dir(&root).unwrap();
        let five = root.join("kreadconfig5"); let six = root.join("kreadconfig6");
        std::fs::write(&five, "#!/bin/sh\ncase \"$*\" in *LookAndFeelPackage*) echo org.kde.breeze.desktop;; *Image*) echo Next;; esac\n").unwrap();
        std::fs::set_permissions(&five, std::fs::Permissions::from_mode(0o700)).unwrap();
        let readers = [six.to_str().unwrap(), five.to_str().unwrap()];
        assert_eq!(kde_config(&readers, &["--key", "LookAndFeelPackage"]).await.as_deref(), Some("org.kde.breeze.desktop\n"));
        assert_eq!(kde_config(&readers, &["--key", "Image"]).await.as_deref(), Some("Next\n"));
        std::fs::write(&six, "#!/bin/sh\necho Plasma6\n").unwrap();
        std::fs::set_permissions(&six, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(kde_config(&readers, &["--key", "Image"]).await.as_deref(), Some("Plasma6\n"));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn omarchy_follows_current_wallpaper_links_and_supports_legacy_location() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("uu-omarchy-{}-{}", std::process::id(), rand::random::<u64>()));
        let state = root.join("custom-state");
        let config = root.join("custom-config");
        for dir in [&state, &config] { std::fs::create_dir_all(dir.join("omarchy/current")).unwrap(); }
        let current = state.join("omarchy/current/background");
        let legacy = config.join("omarchy/current/background");
        let first = root.join("first.png");
        let second = root.join("second.jpg");
        std::fs::write(&first, b"\x89PNG\r\n\x1a\n").unwrap();
        std::fs::write(&second, b"\xff\xd8\xff").unwrap();
        symlink(&second, &legacy).unwrap();
        let detect = || omarchy_background(Some(&state), Some(&config));
        assert_eq!(detect().await, Some(legacy.clone()));
        symlink(&first, &current).unwrap();
        assert_eq!(detect().await, Some(current.clone()));
        assert_eq!(read_image(&detect().await.unwrap()).await.unwrap(), std::fs::read(&first).unwrap());
        std::fs::remove_file(&current).unwrap(); symlink(&second, &current).unwrap();
        assert_eq!(read_image(&detect().await.unwrap()).await.unwrap(), std::fs::read(&second).unwrap());
        std::fs::remove_file(&current).unwrap(); symlink(root.join("missing"), &current).unwrap();
        assert_eq!(detect().await, Some(legacy.clone()));
        std::fs::remove_file(&current).unwrap(); symlink(&root, &current).unwrap();
        assert_eq!(detect().await, Some(legacy.clone()));
        std::fs::remove_file(&legacy).unwrap();
        assert_eq!(detect().await, None);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn kde_packages_select_landscape_and_image_reads_are_bounded() {
        let root = std::env::temp_dir().join(format!("uu-wallpaper-test-{}-{}", std::process::id(), rand::random::<u64>()));
        let images = root.join("contents/images");
        std::fs::create_dir_all(&images).unwrap();
        for name in ["1440x2960.png", "5120x2880.png", "7680x2160.png"] {
            std::fs::write(images.join(name), b"\x89PNG\r\n\x1a\n").unwrap();
        }
        let image = package_image(root.clone()).await.unwrap();
        assert_eq!(image.file_name().unwrap(), "5120x2880.png");
        assert_eq!(package_image(image.clone()).await, Some(image.clone()));
        assert!(read_image(&image).await.is_ok());
        std::fs::write(&image, b"private text is not an image").unwrap();
        assert!(read_image(&image).await.is_err());
        std::fs::OpenOptions::new().write(true).open(&image).unwrap().set_len((LIMIT + 1) as u64).unwrap();
        assert!(read_image(&image).await.is_err());
        assert!(read_image(&root).await.is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn kde_packages_follow_image_symlinks_and_skip_invalid_targets() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("uu-wallpaper-links-test-{}-{}", std::process::id(), rand::random::<u64>()));
        let images = root.join("contents/images");
        std::fs::create_dir_all(&images).unwrap();
        let target = root.join("shared.png");
        std::fs::write(&target, b"\x89PNG\r\n\x1a\n").unwrap();
        let image = images.join("1920x1080.png");
        symlink("../../shared.png", &image).unwrap();
        symlink("../../missing.png", images.join("640x480.png")).unwrap();
        symlink(&root, images.join("800x600.png")).unwrap();
        assert_eq!(package_image(root.clone()).await, Some(image.clone()));
        assert!(read_image(&image).await.is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn reporting_protocol_keeps_file_token_separate_and_stops_on_failures() {
        use tokio_rustls::{rustls, TlsAcceptor};
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let der = cert.serialize_der().unwrap();
        let http = reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none())
            .add_root_certificate(reqwest::Certificate::from_der(&der).unwrap()).build().unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions().unwrap().with_no_client_auth()
            .with_single_cert(vec![CertificateDer::from(der)], PrivateKeyDer::Pkcs8(
                PrivatePkcs8KeyDer::from(cert.serialize_private_key_der()))).unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let auth = UUAuthClient::new(crate::auth::Credentials {
            token: "fixture-account-token".into(), csrf: "fixture-csrf".into(),
            sign_key: "fixture-key".into(), ..Default::default()
        });
        for scenario in ["success", "token-failed", "too-large", "upload-failed", "report-failed"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("https://localhost:{}", listener.local_addr().unwrap().port());
            let server_base = base.clone(); let acceptor = acceptor.clone();
            let steps = match scenario { "token-failed" | "too-large" => 1, "upload-failed" => 2, _ => 3 };
            let server = tokio::spawn(async move {
                for step in 0..steps {
                    let (socket, _) = listener.accept().await.unwrap();
                    let mut tls = acceptor.accept(socket).await.unwrap();
                    let mut bytes = Vec::new(); let mut byte = [0u8; 1];
                    while !bytes.ends_with(b"\r\n\r\n") {
                        tls.read_exact(&mut byte).await.unwrap(); bytes.push(byte[0]); assert!(bytes.len() < 16384);
                    }
                    let headers = String::from_utf8(bytes).unwrap().to_lowercase();
                    let length: usize = headers.lines().find_map(|l| l.strip_prefix("content-length:")
                        .and_then(|s| s.trim().parse().ok())).unwrap_or(0);
                    let mut body = vec![0; length]; tls.read_exact(&mut body).await.unwrap();
                    let (status, reply) = match step {
                        0 => {
                            assert!(headers.starts_with("get /api/v1/tool/fp/token?filetype=1 "));
                            assert!(headers.contains("authorization: bearer fixture-account-token\r\n"));
                            assert!(headers.contains("x-param-sign:")); assert!(body.is_empty());
                            (200, serde_json::json!({"code":if scenario=="token-failed" {1120}else{0},
                                "data":{"token":"fixture-file-token","req_url":format!("{server_base}/upload"),"ttl":60,
                                    "max_size":if scenario=="too-large" {1}else{1024}}}))
                        }
                        1 => {
                            assert!(headers.starts_with("post /upload "));
                            assert!(headers.contains("authorization: fixture-file-token\r\n"));
                            for secret in ["fixture-account-token", "fixture-csrf", "cookie:", "x-param-"] { assert!(!headers.contains(secret)); }
                            assert_eq!(body, b"\x89PNG\r\n\x1a\n");
                            (if scenario=="upload-failed" {500}else{200}, serde_json::json!({"url":"https://example.invalid/wallpaper.png","fsize":8,"md5":"fixture","mime":"image/png"}))
                        }
                        _ => {
                            assert!(headers.starts_with("post /api/v1/device/wallpaper "));
                            assert!(headers.contains("authorization: bearer fixture-account-token\r\n"));
                            assert!(!headers.contains("fixture-file-token"));
                            assert_eq!(serde_json::from_slice::<serde_json::Value>(&body).unwrap(), serde_json::json!({"wallpaper_url":"https://example.invalid/wallpaper.png"}));
                            (200, serde_json::json!({"code":if scenario=="report-failed" {1120}else{0}}))
                        }
                    };
                    let reply = reply.to_string();
                    tls.write_all(format!("HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).as_bytes()).await.unwrap();
                    tls.shutdown().await.unwrap();
                }
            });
            let result = tokio::time::timeout(Duration::from_secs(5), publish_to(&auth, b"\x89PNG\r\n\x1a\n".to_vec(), &base, &http)).await.unwrap();
            assert_eq!(result.is_ok(), scenario == "success", "{scenario}");
            tokio::time::timeout(Duration::from_secs(5), server).await.unwrap().unwrap();
        }
    }
    #[test] fn wallpaper_sources_are_local_and_scoped() {
        assert_eq!(gsettings_path("'file:///usr/share/a%20b.png'"), Some(PathBuf::from("/usr/share/a b.png")));
        for value in ["", "relative.png", "https://example.com/a.png", "file://remote/a.png", "file:///tmp/a.png?token=secret"] { assert!(local_path(value).is_none()); }
        assert_eq!(kde_image("[Other]\nImage=/wrong.png\n[Containments][2][Wallpaper][org.kde.image][General]\nImage=file:///right.png"), Some(PathBuf::from("/right.png")));
    }
    #[test] fn gsettings_decodes_quotes_backslashes_and_unicode() {
        assert_eq!(gsettings_path("\"file:///tmp/it's.png\""), Some(PathBuf::from("/tmp/it's.png")));
        assert_eq!(gsettings_path(r#"'file:///tmp/it\'s.png'"#), Some(PathBuf::from("/tmp/it's.png")));
        assert_eq!(gsettings_path(r#"'/tmp/a\\b\t\u00e9\U0001f600.png'"#), Some(PathBuf::from("/tmp/a\\b\té😀.png")));
        for value in ["'unterminated", "'/tmp/a' trailing", r#"'/tmp/\uD800'"#, r#"'/tmp/\Uffffffff'"#] { assert!(gsettings_path(value).is_none()); }
        assert_eq!(gsettings_path(r#"'/tmp/\x41'"#), Some(PathBuf::from("/tmp/x41")));
        assert_eq!(local_path("/tmp/it's.png"), Some(PathBuf::from("/tmp/it's.png")));
    }
    #[test] fn kde_default_survives_saved_image_settings_but_not_other_plugins() {
        let image = "[Containments][1]\nwallpaperplugin=org.kde.image\n[Containments][1][Wallpaper][org.kde.image][General]\nFillMode=1\n";
        assert!(kde_image(image).is_none()); assert!(kde_default_allowed(image));
        assert!(kde_default_allowed("[Containments][1][Wallpaper][org.kde.image][General]\nFillMode=1"));
        assert!(kde_default_allowed(""));
        assert!(!kde_default_allowed(&image.replace("wallpaperplugin=org.kde.image", "wallpaperplugin=org.kde.slideshow")));
        assert!(!kde_default_allowed("[Containments][1][Wallpaper][org.kde.slideshow][General]\nSlidePaths=/tmp"));
    }
    #[test] fn kde_stored_images_belong_to_the_same_active_image_containment() {
        let stale = "[Containments][1]\nwallpaperplugin=org.kde.slideshow\n[Containments][1][Wallpaper][org.kde.image][General]\nImage=/tmp/previous.png\n";
        assert!(kde_image(stale).is_none());
        assert!(!kde_default_allowed(stale));
        let mixed = format!("{stale}[Containments][2]\nwallpaperplugin=org.kde.image\n[Containments][2][Wallpaper][org.kde.image][General]\nImage=/tmp/current.png\n");
        assert_eq!(kde_image(&mixed), Some(PathBuf::from("/tmp/current.png")));
        assert_eq!(kde_image(&stale.replace("org.kde.slideshow", "org.kde.image")), Some(PathBuf::from("/tmp/previous.png")));
    }
    #[test] fn kde_panels_do_not_enable_default_wallpaper_or_supply_saved_images() {
        let panel = "[Containments][2]\nwallpaperplugin=org.kde.image\nplugin=org.kde.panel\n[Containments][2][Wallpaper][org.kde.image][General]\nImage=/tmp/panel.png\n";
        let slideshow = "[Containments][1]\nplugin=org.kde.plasma.folder\nwallpaperplugin=org.kde.slideshow\n";
        assert!(!kde_default_allowed(slideshow));
        assert!(!kde_default_allowed(&format!("{slideshow}{panel}")));
        assert!(kde_image(&format!("{slideshow}{panel}")).is_none());
        assert!(!kde_default_allowed(panel));
        let desktop = "[Containments][1]\nplugin=org.kde.plasma.folder\n";
        assert!(kde_default_allowed(&format!("{desktop}{panel}")));
    }
    #[test] fn only_supported_image_files_are_uploaded() {
        assert_eq!(image_type(b"\x89PNG\r\n\x1a\n"), Some("image/png"));
        assert_eq!(image_type(b"\xff\xd8\xff"), Some("image/jpeg"));
        assert_eq!(image_type(b"RIFF1234WEBP"), Some("image/webp"));
        assert_eq!(image_type(b"<wallpaper>private text</wallpaper>"), None);
    }
    #[test] fn failed_api_responses_and_unsafe_upload_urls_are_rejected() {
        assert!(api_data(200, &serde_json::json!({"code":0,"data":{}})).is_ok());
        assert!(api_data(422, &serde_json::json!({"code":1120})).is_err());
        assert!(api_data(200, &serde_json::json!({"data":{}})).is_err());
        for url in ["http://example.com", "file:///tmp/a", "https://secret@example.com"] { assert!(https_url(url).is_err()); }
    }
}
