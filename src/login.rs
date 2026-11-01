//! `uuremote-server login`: register this machine as a UU PC host and bind it to an account
//! by scanning a QR code with the UU mobile app, the same flow as the official clients:
//!   1. POST /api/v1/device/windows/init (anonymous, hardware summary) -> device_id
//!   2. POST /api/v1/qrcode/gen/login -> qrcode_jump_url (QR content), token (login ticket)
//!   3. POST /api/v1/qrcode/login/status {token, qrcode_jump_url, login_status}: long poll, 1..4
//!   4. POST /api/v1/login/by_qrcode {token, qrcode_jump_url} -> user_id, token (JWT carries csrf)
//! Field names: server 422 validation and the official Mac client's Codable models
//! (analysis/tools-fix-20261013/viewer-fieldmd.json). Results are written to the private
//! config file (chmod 600). Tokens and tickets are never printed or logged.

use crate::auth::{Credentials, QrRequestErrorKind, UUAuthClient};
use crate::login_ui::{LoginUi, Tone};
use serde_json::{json, Value};
use std::time::Duration;

fn read(path: &str) -> String { std::fs::read_to_string(path).map(|s| s.trim().to_string()).unwrap_or_default() }

fn random_uuid() -> String {
    let mut b = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") { use std::io::Read; let _ = f.read_exact(&mut b); }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// /etc/machine-id as a GUID, the closest Linux analogue of Windows MachineGuid.
fn machine_guid() -> String {
    let id = read("/etc/machine-id");
    if id.len() == 32 && id.bytes().all(|c| c.is_ascii_hexdigit()) {
        format!("{}-{}-{}-{}-{}", &id[..8], &id[8..12], &id[12..16], &id[16..20], &id[20..])
    } else { random_uuid() }
}

fn os_name() -> String {
    read("/etc/os-release").lines().find_map(|l| l.strip_prefix("PRETTY_NAME=")).map(|v| v.trim_matches('"').to_string())
        .unwrap_or_else(|| "Linux".into())
}

fn cpu_name() -> String {
    read("/proc/cpuinfo").lines().find_map(|l| l.strip_prefix("model name").and_then(|r| r.split_once(':')).map(|(_, v)| v.trim().to_string()))
        .unwrap_or_else(|| std::env::consts::ARCH.into())
}

fn base_board() -> String {
    let v = [read("/sys/class/dmi/id/board_vendor"), read("/sys/class/dmi/id/board_name")].join(" ");
    if v.trim().is_empty() { "Unknown".into() } else { v.trim().to_string() }
}

fn video() -> Vec<String> {
    if let Ok(out) = std::process::Command::new("lspci").output() {
        let list: Vec<String> = String::from_utf8_lossy(&out.stdout).lines()
            .filter(|l| l.contains("VGA compatible controller") || l.contains("3D controller") || l.contains("Display controller"))
            .filter_map(|l| l.splitn(2, ": ").nth(1).map(str::to_owned)).collect();
        if !list.is_empty() { return list; }
    }
    let mut list = Vec::new();
    if let Ok(dir) = std::fs::read_dir("/sys/class/drm") {
        for e in dir.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with("card") && !n.contains('-') {
                let dev = e.path().join("device");
                list.push(format!("PCI {}:{}", read(&dev.join("vendor").to_string_lossy()).trim_start_matches("0x"),
                    read(&dev.join("device").to_string_lossy()).trim_start_matches("0x")));
            }
        }
    }
    if list.is_empty() { list.push("Unknown".into()); }
    list
}

/// Short GPU name for display: lspci "Vendor Corporation Chip [Marketing name] (rev x)" -> "Vendor Marketing name".
fn gpu_label(line: &str) -> String {
    let vendor = match line.split_whitespace().next().unwrap_or("") { "Advanced" => "AMD", v => v };
    match (line.rfind('['), line.rfind(']')) {
        (Some(a), Some(b)) if a < b && &line[a + 1..b] != "AMD/ATI" => format!("{vendor} {}", &line[a + 1..b]),
        _ => line.split(" (rev").next().unwrap_or(line).to_string(),
    }
}

/// MAC of the first physical interface (one backed by a device), uppercase dash form like Windows.
fn mac() -> String {
    let mut names: Vec<String> = std::fs::read_dir("/sys/class/net").map(|d| d.flatten()
        .filter(|e| e.path().join("device").exists()).map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    names.sort();
    names.iter().map(|n| read(&format!("/sys/class/net/{n}/address"))).find(|m| !m.is_empty() && m != "00:00:00:00:00:00")
        .map(|m| m.to_uppercase().replace(':', "-")).unwrap_or_default()
}

fn memory_mb() -> u64 {
    read("/proc/meminfo").lines().find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok()).map(|kb| kb / 1024).unwrap_or(0)
}

fn screen() -> String {
    crate::capture::ScreenCapture::new().map(|c| format!("{}x{}", c.width, c.height)).unwrap_or_else(|_| "1920x1080".into())
}

fn hostname() -> String {
    let h = read("/proc/sys/kernel/hostname");
    if h.is_empty() { "Linux".into() } else { h }
}

/// csrf claim inside the JWT access token.
fn csrf_from_token(token: &str) -> Option<String> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()?.get("csrf")?.as_str().map(str::to_owned)
}

fn data<'a>(path: &str, status: u16, body: &'a Value) -> Result<&'a Value, String> {
    let code = body.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if status == 200 && code == 0 { return Ok(body.get("data").unwrap_or(&Value::Null)); }
    let fields: Vec<String> = body.get("data").and_then(|d| d.as_array()).map(|a| a.iter()
        .filter_map(|e| e.get("loc").map(|l| l.to_string())).collect()).unwrap_or_default();
    Err(format!("{path}: http {status} code {code} {} {}", body.get("msg").and_then(|m| m.as_str()).unwrap_or(""),
        if fields.is_empty() { String::new() } else { format!("fields={}", fields.join(",")) }))
}

fn render_qr(content: &str) -> Result<String, String> {
    let code = qrcode::QrCode::new(content.as_bytes()).map_err(|e| format!("QR encode failed: {e}"))?;
    Ok(code.render::<qrcode::render::unicode::Dense1x2>().quiet_zone(true)
        .dark_color(qrcode::render::unicode::Dense1x2::Light).light_color(qrcode::render::unicode::Dense1x2::Dark).build())
}

/// API code for an expired login QR code.
const QR_EXPIRED: i64 = 1190;

/// QR API errors must not echo server messages/validation fields: they may contain credentials.
fn qr_data<'a>(stage: &str, status: u16, body: &'a Value) -> Result<&'a Value, String> {
    let code = body.get("code").and_then(Value::as_i64);
    if status == 200 && code == Some(0) { return Ok(body.get("data").unwrap_or(&Value::Null)); }
    Err(format!("{stage}: http_status={status} api_code={code:?}"))
}

#[derive(Debug, PartialEq, Eq)]
enum QrPollAction { Wait(i64), Scanned, Confirmed, Refresh(Duration), Failed(&'static str) }

/// Preserve the official refresh policy; a failed query says nothing about QR expiry.
fn qr_poll_action(last_status: i64, http_status: u16, body: &Value) -> QrPollAction {
    if body.get("code").and_then(Value::as_i64) == Some(QR_EXPIRED) {
        return QrPollAction::Failed(crate::i18n::s("二维码已失效","QR code expired"));
    }
    let Ok(data) = qr_data("qrcode status", http_status, body) else {
        return QrPollAction::Failed(crate::i18n::s("二维码状态查询失败","QR status check failed"));
    };
    match data.get("status").and_then(Value::as_i64) {
        Some(1) => QrPollAction::Wait(1),
        Some(2) if last_status != 2 => QrPollAction::Scanned,
        Some(2) => QrPollAction::Wait(2),
        Some(0) => QrPollAction::Refresh(Duration::from_millis(350)),
        Some(3) => QrPollAction::Refresh(Duration::ZERO),
        Some(4) => QrPollAction::Confirmed,
        _ => QrPollAction::Failed(crate::i18n::s("二维码状态响应异常","Unexpected QR status response")),
    }
}

/// QR modules as RGBA pixels: `scale` pixels per module, `quiet` light modules around the code.
fn qr_rgba(content: &str, scale: usize, quiet: usize) -> Result<(Vec<u8>, usize), String> {
    let code = qrcode::QrCode::new(content.as_bytes()).map_err(|e| format!("QR encode failed: {e}"))?;
    let edge = (code.width() + 2 * quiet) * scale;
    let mut rgba = vec![255; edge * edge * 4];
    for y in 0..code.width() { for x in 0..code.width() {
        if code[(x, y)] == qrcode::Color::Dark {
            for dy in 0..scale { for dx in 0..scale {
                let offset = (((y + quiet) * scale + dy) * edge + (x + quiet) * scale + dx) * 4;
                rgba[offset..offset + 3].fill(0);
            }}
        }
    }}
    Ok((rgba, edge))
}

pub(crate) fn write_config(cfg: &Value) -> Result<std::path::PathBuf, String> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let path = crate::config::path();
    let dir = path.parent().ok_or("config path has no parent")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let tmp = path.with_extension("json.tmp");
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    f.write_all(serde_json::to_string_pretty(cfg).unwrap_or_default().as_bytes()).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("replace {}: {e}", path.display()))?;
    Ok(path)
}

/// One login at a time per config file (a manual `login` and the service's window would race
/// for the same QR session). The lock is released when the returned file is dropped.
fn single_instance() -> Result<std::fs::File, String> {
    use std::os::unix::{fs::OpenOptionsExt, io::AsRawFd};
    let path = crate::config::path().with_file_name("login.lock");
    if let Some(dir) = path.parent() { std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?; }
    let file = std::fs::OpenOptions::new().create(true).write(true).mode(0o600).open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("another login is already in progress (login window already open)".into());
    }
    Ok(file)
}

/// Service side: no usable login. Like the official host, show the login window, as a `login`
/// child process so GTK never runs inside the daemon, when a display is available and
/// UU_LOGIN_WINDOW is not 0. Returns once the config holds credentials whose token differs
/// from `stale_token` (written by that window or by a manual `uuremote-server login`).
pub async fn wait_for_credentials(stale_token: Option<String>) -> Credentials {
    let display = std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
    let mut window = None;
    if display && std::env::var("UU_LOGIN_WINDOW").map_or(true, |v| v != "0") {
        // stdout carries the terminal QR (a login ticket): never into the journal.
        match std::env::current_exe().and_then(|exe| tokio::process::Command::new(exe).arg("login")
            .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).spawn()) {
            Ok(child) => { log::info!("Opened the QR login window on the desktop"); window = Some(child); }
            Err(e) => log::error!("Opening the login window failed: {e}"),
        }
    }
    if window.is_none() {
        log::error!("UU login required: run `uuremote-server login` and scan the QR code; watching {}", crate::config::path().display());
    }
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if let Some(child) = window.as_mut() {
            if let Ok(Some(status)) = child.try_wait() {
                log::info!("Login window exited ({status}); run `uuremote-server login` to sign in later");
                window = None;
            }
        }
        if let Ok(creds) = crate::config::read_fresh().and_then(|f| Credentials::load(f.as_ref())) {
            if stale_token.as_deref() != Some(creds.token.as_str()) {
                log::info!("UU login found in {}; going online", crate::config::path().display());
                return creds; // a still-open window keeps showing its success page until closed
            }
        }
    }
}

/// `gui`: also show the GTK login window when one can be opened (the terminal QR is always printed).
pub async fn run(gui: bool) -> Result<(), String> {
    let _lock = single_instance().map_err(|e| { eprintln!("login failed: {e}"); e })?;
    let ui = if gui { LoginUi::open(&hostname(), &os_name(), &cpu_name(), &video().iter().map(|v| gpu_label(v)).collect::<Vec<_>>().join("\n")) } else { LoginUi::none() };
    let result = tokio::select! {
        r = flow(&ui) => r,
        _ = ui.wait_closed() => Err("login window closed".into()),
    };
    match &result {
        Ok(name) => {
            ui.done(&crate::tr!("登录成功\n{name}","Signed in\n{name}"));
            ui.status(crate::i18n::s("登录信息已保存到本机配置，可以关闭此窗口","Sign-in saved to the local config; you can close this window"), Tone::Ok);
        }
        Err(e) => { eprintln!("login failed: {e}"); ui.status(&crate::tr!("登录失败：{e}","Sign-in failed: {e}"), Tone::Error); }
    }
    if ui.shown() { ui.wait_closed().await; }
    result.map(|_| ())
}

/// Local installation identifiers only; guest creation does not require device/init.
pub fn prepare_identity() -> Result<(Value, Credentials), String> {
    let mut cfg = crate::config::file()?.cloned().unwrap_or_else(|| json!({}));
    let get = |cfg: &Value, k: &str| std::env::var(format!("UU_{}", k.to_uppercase())).ok().filter(|v| !v.is_empty())
        .or_else(|| cfg.get(k).and_then(|v| v.as_str()).map(str::to_owned)).unwrap_or_default();
    let sign_key = Some(get(&cfg, "sign_key")).filter(|k| !k.is_empty()).unwrap_or_else(|| crate::auth::DEFAULT_SIGN_KEY.into());
    // Stable host identity: reused across logins so the account keeps one device.
    for (key, make) in [("client_id", random_uuid as fn() -> String), ("system_id", || random_uuid().to_uppercase()), ("machine_guid", machine_guid)] {
        if get(&cfg, key).is_empty() { cfg[key] = json!(make()); }
    }
    let creds = Credentials { client_id: get(&cfg, "client_id"), system_id: get(&cfg, "system_id"), device_id: get(&cfg, "device_id"), sign_key, ..Default::default() };

    write_config(&cfg)?;
    Ok((cfg, creds))
}

/// Holds an exclusive lock beside config.json while a device identity is created: the host's own QR
/// window and the viewer's sign-in can run at once, and two `device/windows/init` calls would leave the
/// account bound to one device while config.json keeps the other (the host then shows as offline).
struct IdentityLock(std::fs::File);
impl IdentityLock {
    fn acquire() -> Result<Self, String> {
        use std::os::unix::io::AsRawFd;
        let path = crate::config::path().with_extension("lock");
        if let Some(dir) = path.parent() { let _ = std::fs::create_dir_all(dir); }
        let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path).map_err(|_| "Cannot open the identity lock")?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 { return Err("Cannot lock the device identity".into()) }
        Ok(Self(file))
    }
}
impl Drop for IdentityLock { fn drop(&mut self) { use std::os::unix::io::AsRawFd; unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN); } } }

pub async fn prepare_device() -> Result<(Value, Credentials), String> {
    // Read the configuration only after taking the lock, so a waiting process sees the device the
    // first one registered instead of creating its own.
    let _lock = tokio::task::spawn_blocking(IdentityLock::acquire).await.map_err(|_| "Identity lock task failed")??;
    let (mut cfg, mut creds) = prepare_identity()?;
    let get = |cfg: &Value, k: &str| std::env::var(format!("UU_{}", k.to_uppercase())).ok().filter(|v| !v.is_empty())
        .or_else(|| cfg.get(k).and_then(|v| v.as_str()).map(str::to_owned)).unwrap_or_default();
    if get(&cfg, "device_id").is_empty() {
        let body = json!({
            "name": hostname(), "client_id": creds.client_id, "system_id": creds.system_id, "machine_guid": get(&cfg, "machine_guid"),
            "os": os_name(), "base_board": base_board(), "cpu": cpu_name(), "video": video(), "mac": mac(),
            "memory": memory_mb(), "screen": screen(),
        });
        let (status, resp) = UUAuthClient::new(creds.clone()).post_json("/api/v1/device/windows/init", &body).await?;
        let d = data("device init", status, &resp)?;
        let id = d.get("device_id").and_then(|v| v.as_str()).ok_or("device init returned no device_id")?;
        log::info!("Registered local device identity");
        cfg["device_id"] = json!(id);
        write_config(&cfg)?; // keep the device even if the QR step is abandoned
    }
    creds.device_id = get(&cfg, "device_id");
    Ok((cfg, creds))
}

/// Returns the account nickname (or user id) on success.
pub(crate) async fn flow(ui: &LoginUi) -> Result<String, String> {
    let (mut cfg, creds) = prepare_device().await?;
    let client = UUAuthClient::new(creds.clone());
    // Mirrors the official Mac client (LoginViewController / LoginManager, UURemote 4.42): gen/login has
    // no body; the status long poll sends gen's `status_query_ticket` under `token` with the last
    // status, re-asks as soon as an answer arrives (60 s request timeout), regenerates on status 3
    // at once and on status 0 after 0.35 s, logs in on 4, and on any error (e.g. HTTP 400 code 1190)
    // stops until the user asks for a new QR. Sending gen's older `token` field made the cloud reject
    // every QR within seconds (#68).
    let timeout = Duration::from_secs(60);
    loop {
        let (status, resp) = match client.post_with_timeout("/api/v1/qrcode/gen/login", None, timeout).await {
            Ok(r) => r,
            Err(e) if e.response_received() => {
                eprintln!("QR generation response failed: {e}");
                wait_for_refresh(ui, crate::i18n::s("二维码加载失败","QR code failed to load")).await;
                continue;
            }
            Err(e) => return Err(format!("QR generation failed: {e}")),
        };
        let d = match qr_data("qrcode gen", status, &resp) {
            Ok(d) => d.clone(),
            Err(e) => { println!("QR code could not be loaded ({e})"); wait_for_refresh(ui, crate::i18n::s("二维码加载失败","QR code failed to load")).await; continue; }
        };
        let url = d.get("qrcode_jump_url").and_then(|v| v.as_str()).ok_or("qrcode gen returned no qrcode_jump_url")?.to_string();
        let ticket = qr_ticket(&d).ok_or("qrcode gen returned no status ticket")?.to_string();
        ui.offer_refresh(false);
        if ui.shown() {
            let modules = qrcode::QrCode::new(url.as_bytes()).map_err(|e| e.to_string())?.width() + 2;
            let (rgba, edge) = qr_rgba(&url, (224 / modules).max(2), 1)?;
            ui.qr(&rgba, edge);
            ui.status(crate::i18n::s("等待扫码…","Waiting for scan…"), Tone::Info);
        }
        if let Some(path)=std::env::var_os("UU_LOGIN_QR_PNG") {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let (rgba, edge) = qr_rgba(&url, 8, 4)?;
            let mut file=std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path).map_err(|e|e.to_string())?;
            file.write_all(&crate::cursor::encode_png(edge as u32,edge as u32,&rgba)).map_err(|e|e.to_string())?;
        }
        if !ui.quiet() { println!("\nScan with the UU Remote mobile app (网易UU远程 App 扫码登录):\n{}", render_qr(&url)?); }
        let mut login_status = 1i64;
        let failure = loop {
            let query = json!({"token": ticket, "qrcode_jump_url": url, "login_status": login_status});
            let (status, resp) = match client.post_with_timeout("/api/v1/qrcode/login/status", Some(&query), timeout).await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("QR status query failed: {e}; last_status={login_status}");
                    break match e.kind {
                        QrRequestErrorKind::Timeout => crate::i18n::s("二维码状态查询超时（不代表二维码已失效）","QR status check timed out (the QR code may still be valid)"),
                        _ => crate::i18n::s("二维码状态查询失败（不代表二维码已失效）","QR status check failed (the QR code may still be valid)"),
                    };
                }
            };
            match qr_poll_action(login_status, status, &resp) {
                QrPollAction::Wait(st) => {
                    if st == 1 && login_status != 1 { ui.status(crate::i18n::s("等待扫码…","Waiting for scan…"), Tone::Info); }
                    login_status = st;
                }
                QrPollAction::Scanned => {
                    login_status = 2;
                    println!("已扫码，请在手机上确认 (scanned, confirm on the phone)");
                    ui.status(crate::i18n::s("已扫码，请在手机上确认登录","Scanned, confirm the sign-in on your phone"), Tone::Ok);
                }
                QrPollAction::Refresh(delay) => {
                    if delay.is_zero() { println!("QR code expired; generating a new one"); }
                    else { tokio::time::sleep(delay).await; }
                    break "";
                }
                QrPollAction::Confirmed => {
                    println!("已确认，正在完成登录 (confirmed, completing login)");
                    ui.status(crate::i18n::s("已确认，正在完成登录…","Confirmed, finishing sign-in…"), Tone::Info);
                    let body = json!({"token": ticket, "qrcode_jump_url": url});
                    let (status, resp) = match client.post_with_timeout("/api/v1/login/by_qrcode", Some(&body), timeout).await {
                        Ok(r) => r,
                        Err(e) if e.response_received() => {
                            eprintln!("QR login exchange response failed: {e}");
                            break crate::i18n::s("登录失败","Sign-in failed");
                        }
                        Err(e) => return Err(format!("QR login exchange failed: {e}")),
                    };
                    let d = match qr_data("qrcode login", status, &resp) {
                        Ok(d) => d,
                        Err(e) => { println!("QR login not granted ({e})"); break crate::i18n::s("登录失败","Sign-in failed"); }
                    };
                    let token = d.get("token").and_then(|v| v.as_str()).ok_or("login returned no token")?;
                    let user = d.get("user_id").and_then(|v| v.as_str()).ok_or("login returned no user_id")?;
                    cfg["token"] = json!(token);
                    cfg["user_id"] = json!(user);
                    cfg["csrf"] = json!(csrf_from_token(token).ok_or("token carries no csrf claim")?);
                    let nickname = d.get("nickname").and_then(|v| v.as_str()).unwrap_or("");
                    if !nickname.is_empty() { cfg["account_name"] = json!(nickname); }
                    // Keep settings saved meanwhile (e.g. from the assist card): merge into the fresh file.
                    if let Ok(Some(Value::Object(fresh))) = crate::config::read_fresh() {
                        for (k, v) in fresh { if cfg.get(&k).is_none() { cfg[k] = v; } }
                    }
                    let path = write_config(&cfg)?;
                    println!("Logged in as {} ({}); saved to {}", nickname, user, path.display());
                    return Ok(if nickname.is_empty() { user.to_string() } else { nickname.to_string() });
                }
                QrPollAction::Failed(reason) => {
                    eprintln!("QR status query stopped: http_status={status} api_code={:?} status={:?} last_status={login_status}",
                        resp.get("code").and_then(Value::as_i64), resp.get("data").and_then(|d| d.get("status")).and_then(Value::as_i64));
                    break reason;
                }
            }
        };
        if !failure.is_empty() { wait_for_refresh(ui, failure).await; }
    }
}

/// gen/login's status ticket: `status_query_ticket` (current cloud and official clients), else the
/// older `token` field for servers that do not send it.
fn qr_ticket(gen: &Value) -> Option<&str> {
    gen.get("status_query_ticket").or_else(|| gen.get("token")).and_then(|v| v.as_str()).filter(|t| !t.is_empty())
}

/// Stop like the official client and wait for the user: the 刷新二维码 button, Enter in a terminal,
/// or (no window and no terminal, e.g. a service) one minute.
async fn wait_for_refresh(ui: &LoginUi, reason: &str) {
    use std::io::IsTerminal;
    ui.status(&crate::tr!("{reason}，请点击「刷新二维码」","{reason}. Click \"Refresh QR code\"."), Tone::Error);
    ui.offer_refresh(true);
    let terminal = !ui.quiet() && std::io::stdin().is_terminal();
    if terminal { println!("{reason}. Press Enter to show a new QR code (按回车刷新二维码)"); }
    else { println!("{reason}; waiting for 刷新二维码{}", if ui.shown() { "" } else { " (a new QR code follows in 60 s)" }); }
    let mut line = terminal.then(|| tokio::task::spawn_blocking(|| { let mut s = String::new(); let _ = std::io::stdin().read_line(&mut s); }));
    let started = std::time::Instant::now();
    loop {
        if ui.take_refresh() || (ui.shown() && ui.closed()) { break; }
        if let Some(handle) = line.as_mut() { if handle.is_finished() { break; } }
        if !ui.shown() && !terminal && started.elapsed() >= Duration::from_secs(60) { break; }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    ui.offer_refresh(false);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn qr_poll_keeps_waiting_scanned_and_confirmed_distinct() {
        let reply = |status| json!({"code": 0, "data": {"status": status}});
        assert_eq!(qr_poll_action(1, 200, &reply(1)), QrPollAction::Wait(1));
        assert_eq!(qr_poll_action(1, 200, &reply(2)), QrPollAction::Scanned);
        assert_eq!(qr_poll_action(2, 200, &reply(2)), QrPollAction::Wait(2));
        assert_eq!(qr_poll_action(2, 200, &reply(4)), QrPollAction::Confirmed);
        assert_eq!(qr_poll_action(1, 200, &reply(0)), QrPollAction::Refresh(Duration::from_millis(350)));
        assert_eq!(qr_poll_action(2, 200, &reply(3)), QrPollAction::Refresh(Duration::ZERO));
        assert_eq!(qr_poll_action(2, 400, &json!({"code": 1190})), QrPollAction::Failed(crate::i18n::s("二维码已失效","QR code expired")));
    }

    #[test]
    fn qr_poll_failures_do_not_imply_expiry_or_confirmation() {
        for last in [1, 2] {
            for body in [json!(null), json!({"code": 1}), json!({"code": 0, "data": {"status": 4}})] {
                assert_eq!(qr_poll_action(last, 503, &body), QrPollAction::Failed(crate::i18n::s("二维码状态查询失败","QR status check failed")));
            }
            assert_eq!(qr_poll_action(last, 200, &json!({"code": 1})), QrPollAction::Failed(crate::i18n::s("二维码状态查询失败","QR status check failed")));
            for data in [json!(null), json!({}), json!({"status": "4"}), json!({"status": 99}), json!({"status": -1})] {
                assert_eq!(qr_poll_action(last, 200, &json!({"code": 0, "data": data})), QrPollAction::Failed(crate::i18n::s("二维码状态响应异常","Unexpected QR status response")));
            }
        }
    }

    #[test]
    fn qr_api_errors_do_not_echo_server_content() {
        let body = json!({"code": 1001, "msg": "fixture-token", "data": [{"loc": "fixture-ticket"}]});
        assert_eq!(qr_data("qrcode gen", 400, &body).unwrap_err(), "qrcode gen: http_status=400 api_code=Some(1001)");
    }

    #[test]
    fn status_ticket_prefers_status_query_ticket() {
        assert_eq!(qr_ticket(&json!({"token": "old", "status_query_ticket": "new"})), Some("new"));
        assert_eq!(qr_ticket(&json!({"token": "old"})), Some("old"));
        assert_eq!(qr_ticket(&json!({"status_query_ticket": ""})), None);
    }
    #[test]
    fn service_waits_until_a_new_token_is_saved() {
        let _env = crate::config::test_env_lock().lock().unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
        let dir = std::env::temp_dir().join(format!("uu-login-wait-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let creds = |token: &str| json!({"user_id": "u", "device_id": "d", "client_id": "c", "system_id": "s", "token": token, "csrf": "x"});
        std::fs::write(&path, creds("old").to_string()).unwrap();
        std::env::set_var("UU_CONFIG", &path);
        std::env::set_var("UU_LOGIN_WINDOW", "0");
        let writer = path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(2500)).await;
            std::fs::write(&writer, creds("new").to_string()).unwrap();
        });
        let got = tokio::time::timeout(Duration::from_secs(10), wait_for_credentials(Some("old".into()))).await.unwrap();
        assert_eq!(got.token, "new"); // the expired token in the file is not accepted
        let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn csrf_is_read_from_jwt_payload() {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"sub":"u","csrf":"c-1"}"#);
        assert_eq!(csrf_from_token(&format!("h.{payload}.s")), Some("c-1".into()));
        assert_eq!(csrf_from_token("bad"), None);
    }
    #[test]
    fn generated_ids_are_uuid_shaped() {
        let u = random_uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(u.as_bytes()[14], b'4');
        assert!(render_qr("https://example.com/x").unwrap().lines().count() > 10);
        assert_eq!(gpu_label("NVIDIA Corporation GA102 [GeForce RTX 3090] (rev a1)"), "NVIDIA GeForce RTX 3090");
        assert_eq!(gpu_label("Advanced Micro Devices, Inc. [AMD/ATI] Navi 21 [Radeon RX 6800] (rev c1)"), "AMD Radeon RX 6800");
        assert_eq!(gpu_label("Advanced Micro Devices, Inc. [AMD/ATI] Navi 21 (rev c1)"), "Advanced Micro Devices, Inc. [AMD/ATI] Navi 21");
        let (rgba, edge) = qr_rgba("https://example.com/x", 3, 1).unwrap();
        assert_eq!(rgba.len(), edge * edge * 4);
        assert_eq!(&rgba[..4], &[255, 255, 255, 255]); // quiet zone is light
        assert_eq!(&rgba[(3 * edge + 3) * 4..(3 * edge + 3) * 4 + 3], &[0, 0, 0]); // finder corner is dark
    }
}
