//! Opt-in guest assistance. Protocol sources: AGENTS.md "Remote assistance".
//! Secret values never implement Display/Debug or enter diagnostics.
use crate::auth::{Credentials, RoomData, UUAuthClient};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::os::unix::{fs::OpenOptionsExt, io::AsRawFd};

const CREATE: &str = "/api/v1/guest/create";
const INFO: &str = "/api/v1/guest/share/info";
const CANCEL: &str = "/api/v1/guest/share/cancel";
const ROOM: &str = "/api/v1/guest/room/create";
const SIGN: &str = "/api/v1/guest/room/share/upload/sign";
const CONTROL_MODE: &str = "/api/v1/guest/room/share/upload_control_mode";
// Logged-in host: the same flow on the account identity (official Windows host NrdDevice/NrdBusinessEntry).
const ACCOUNT_INFO: &str = "/api/v1/device/share/info";
const ACCOUNT_CANCEL: &str = "/api/v1/device/share/cancel";
const ACCOUNT_SIGN: &str = "/api/v2/room/share/upload_sign";
const ACCOUNT_CONTROL_MODE: &str = "/api/v2/room/share/upload_control_mode";
const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// Guest mode uses its own device/config identity and never consumes an account token.
pub fn configure() -> Result<(), String> {
    if ["UU_USER_ID", "UU_TOKEN", "UU_CSRF"].iter().any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty())) {
        return Err("assist mode cannot use account credential environment variables".into());
    }
    if std::env::var_os("UU_CONFIG").is_none() {
        if crate::config::read_fresh()?.as_ref().is_some_and(|v| ["token", "user_id"].iter()
            .any(|k| v.get(k).and_then(Value::as_str).is_some_and(|s| !s.is_empty()))) {
            return Err("account identity already configured; guest assist needs an explicit separate UU_CONFIG".into());
        }
        let path = crate::config::path().with_file_name("assist").join("config.json");
        std::env::set_var("UU_CONFIG", path);
    }
    if crate::config::read_fresh()?.as_ref().is_some_and(|v| ["token", "user_id", "csrf"].iter()
        .any(|k| v.get(k).and_then(Value::as_str).is_some_and(|s| !s.is_empty()))) {
        return Err("assist requires a separate config without account credentials".into());
    }
    Ok(())
}

/// Official RecipientConfigModel.verificationType: temporary only, custom only, or both.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verify { Temporary = 0, Custom = 1, Both = 2 }
impl Verify {
    fn parse(v: &str) -> Option<Self> { match v { "temporary" => Some(Self::Temporary), "custom" => Some(Self::Custom), "both" => Some(Self::Both), _ => None } }
    fn name(self) -> &'static str { match self { Self::Temporary => "temporary", Self::Custom => "custom", Self::Both => "both" } }
    fn from_index(i: i64) -> Option<Self> { match i { 0 => Some(Self::Temporary), 1 => Some(Self::Custom), 2 => Some(Self::Both), _ => None } }
}
/// Official RecipientCodeRefreshType: after each session (closeConnect), hourly, 12 h, 24 h, never.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refresh { Session = 0, Hour = 1, Hour12 = 2, Hour24 = 3, Never = 4 }
impl Refresh {
    fn parse(v: &str) -> Option<Self> { match v { "session" => Some(Self::Session), "1h" => Some(Self::Hour), "12h" => Some(Self::Hour12), "24h" => Some(Self::Hour24), "never" => Some(Self::Never), _ => None } }
    fn name(self) -> &'static str { match self { Self::Session => "session", Self::Hour => "1h", Self::Hour12 => "12h", Self::Hour24 => "24h", Self::Never => "never" } }
    fn from_index(i: i64) -> Option<Self> { [Self::Session, Self::Hour, Self::Hour12, Self::Hour24, Self::Never].get(usize::try_from(i).ok()?).copied() }
    fn period(self) -> Option<std::time::Duration> {
        match self { Self::Hour => Some(3600), Self::Hour12 => Some(12 * 3600), Self::Hour24 => Some(24 * 3600), _ => None }.map(std::time::Duration::from_secs)
    }
}
/// Mainland mobile numbers are shown like the official client: 186******08.
pub fn display_account(name: &str) -> String {
    if name.len() == 11 && name.bytes().all(|b| b.is_ascii_digit()) { format!("{}******{}", &name[..3], &name[9..]) } else { name.to_owned() }
}
/// 8-16 ASCII letters/digits including both (official RecipientCustomCodeView rules).
fn valid_custom(code: &str) -> bool {
    (8..=16).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_alphanumeric())
        && code.bytes().any(|b| b.is_ascii_alphabetic()) && code.bytes().any(|b| b.is_ascii_digit())
}

pub enum Command { Allow, Deny, Reset, Stop, Verify(Verify), Refresh(Refresh), Custom(String), Logout, Quit }

#[cfg(uu_gtk)]
extern "C" {
    fn uu_assist_ui_start(hidden: libc::c_int, hide_on_close: libc::c_int) -> libc::c_int;
    fn uu_assist_ui_present();
    fn uu_ui_set_user(name: *const libc::c_char);
    fn uu_assist_ui_set(id: *const libc::c_char, code: *const libc::c_char, allowed: libc::c_int, verify: libc::c_int, refresh: libc::c_int, has_custom: libc::c_int);
    fn uu_assist_ui_take_command(arg: *mut libc::c_char, len: libc::size_t) -> libc::c_int;
    fn uu_assist_ui_close();
    fn uu_ui_set_problem(summary: *const libc::c_char, text: *const libc::c_char, notify: libc::c_int) -> libc::c_int;
}

/// A problem only the person at the desktop can fix: a red line in the host window (`None` clears it)
/// and, with `notify`, a desktop notification, or the host window opened when no notification daemon
/// answers. Returns whether the person was told now.
pub fn show_problem(summary: &str, text: Option<&str>, notify: bool) -> bool {
    #[cfg(uu_gtk)] {
        let summary = std::ffi::CString::new(summary).unwrap_or_default();
        let text = text.map(|t| std::ffi::CString::new(t).unwrap_or_default());
        let text = text.as_ref().map_or(std::ptr::null(), |t| t.as_ptr());
        unsafe { uu_ui_set_problem(summary.as_ptr(), text, i32::from(notify)) >= 0 }
    }
    #[cfg(not(uu_gtk))] { let _ = (summary, text, notify); false }
}

/// "243829726" -> "243 829 726", as the official client shows device IDs.
fn display_id(id: &str) -> String {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) { return id.to_owned(); }
    id.as_bytes().chunks(3).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join(" ")
}

/// Where the ID/code are shown and commands come from: a local GTK window (c_engine/assist_window.c)
/// when a display is available, otherwise /dev/tty, which bypasses stdout/stderr, journals,
/// redirected output and the logger. Without either, refuse startup rather than disclose the code.
pub enum LocalUi { Tty(tokio::io::unix::AsyncFd<std::fs::File>), #[cfg(uu_gtk)] Window }
impl LocalUi {
    /// Standalone `assist`: visible window (closing it stops assistance), else /dev/tty.
    pub fn open() -> Result<Self, String> { Self::open_visible(false) }
    /// `hide_on_close`: the not-logged-in host service (window stays reachable with `uuremote-server show`).
    pub fn open_visible(hide_on_close: bool) -> Result<Self, String> {
        #[cfg(not(uu_gtk))] let _ = hide_on_close;
        #[cfg(uu_gtk)]
        if std::env::var("UU_ASSIST_UI").ok().as_deref() != Some("tty") && Self::display() && unsafe { uu_assist_ui_start(0, i32::from(hide_on_close)) } == 0 {
            return Ok(Self::Window);
        }
        let tty = std::fs::OpenOptions::new().read(true).write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY).open("/dev/tty")
            .map_err(|_| "assist requires a desktop window or a local controlling terminal; no code was printed")?;
        if unsafe { libc::isatty(tty.as_raw_fd()) } != 1 { return Err("assist output is not a terminal".into()); }
        Ok(Self::Tty(tokio::io::unix::AsyncFd::new(tty).map_err(|_| "assist terminal registration failed")?))
    }
    fn display() -> bool { std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some() }
    /// Host service: a hidden window opened on demand (`uuremote-server show`); closing only hides it.
    /// No terminal fallback: a background service has nowhere safe to print the code.
    pub fn open_service() -> Result<Self, String> {
        #[cfg(uu_gtk)]
        if Self::display() && unsafe { uu_assist_ui_start(1, 1) } == 0 { return Ok(Self::Window); }
        Err("remote assistance needs a desktop window for the host service".into())
    }
    /// Account card: signed-in user (name already masked for display).
    pub fn set_user(&self, name: &str) {
        #[cfg(uu_gtk)]
        if matches!(self, Self::Window) { let name = std::ffi::CString::new(name).unwrap_or_default(); unsafe { uu_ui_set_user(name.as_ptr()) } }
        #[cfg(not(uu_gtk))] let _ = name;
    }
    pub fn present(&self) {
        #[cfg(uu_gtk)]
        if matches!(self, Self::Window) { unsafe { uu_assist_ui_present() } }
    }
    fn show(&self, id: &str, code: &str, allowed: bool, verify: Verify, refresh: Refresh, has_custom: bool) -> Result<(), String> {
        #[cfg(not(uu_gtk))] let _ = has_custom;
        match self {
            Self::Tty(tty) => {
                // Bound/control-character-validated server identifier; no terminal escape injection.
                let mut tty = tty.get_ref();
                write!(tty, "\n远程协助 / Remote assistance\n设备 ID: {}\n临时验证码: {code}\n允许他人远程协助: {allowed}\n验证方式: {} 自动刷新: {}\n仅向可信的人提供验证码。allow=允许，deny=不允许，reset=换码，mode temporary|custom|both，refresh session|1h|12h|24h|never，off=关闭并撤销\n", display_id(id), verify.name(), refresh.name())
                    .map_err(|_| "assist terminal display failed".into())
            }
            #[cfg(uu_gtk)]
            Self::Window => {
                let (id, code) = (std::ffi::CString::new(display_id(id)).unwrap_or_default(), std::ffi::CString::new(code).unwrap_or_default());
                unsafe { uu_assist_ui_set(id.as_ptr(), code.as_ptr(), i32::from(allowed), verify as i32, refresh as i32, i32::from(has_custom)) };
                Ok(())
            }
        }
    }
    pub async fn command(&self) -> Command {
        match self {
            Self::Tty(tty) => Self::tty_command(tty).await,
            #[cfg(uu_gtk)]
            Self::Window => loop {
                let mut arg = [0 as libc::c_char; 40];
                let cmd = unsafe { uu_assist_ui_take_command(arg.as_mut_ptr(), arg.len()) };
                let text = unsafe { std::ffi::CStr::from_ptr(arg.as_ptr()) }.to_string_lossy().into_owned();
                let index = text.parse::<i64>().unwrap_or(-1);
                match cmd {
                    1 => return Command::Allow, 2 => return Command::Deny, 3 => return Command::Reset, 4 => return Command::Stop,
                    5 => if let Some(v) = Verify::from_index(index) { return Command::Verify(v) },
                    6 => if let Some(r) = Refresh::from_index(index) { return Command::Refresh(r) },
                    7 => return Command::Custom(text),
                    8 => return Command::Logout,
                    9 => return Command::Quit,
                    _ => tokio::time::sleep(std::time::Duration::from_millis(150)).await,
                }
            },
        }
    }
    async fn tty_command(tty: &tokio::io::unix::AsyncFd<std::fs::File>) -> Command {
        let mut line = Vec::new();
        loop {
            let mut ready = match tty.readable().await { Ok(v) => v, Err(_) => return Command::Stop };
            let result = ready.try_io(|fd| { let mut byte = [0u8]; let mut tty = fd.get_ref(); tty.read(&mut byte).map(|n| (n, byte[0])) });
            match result {
                Ok(Ok((0, _))) | Ok(Err(_)) => return Command::Stop,
                Ok(Ok((_, byte))) if byte == b'\n' => {
                    let text = String::from_utf8_lossy(&line).trim().to_owned();
                    let cmd = match text.split_once(' ') {
                        Some(("mode", v)) => Verify::parse(v).map(Command::Verify),
                        Some(("refresh", v)) => Refresh::parse(v).map(Command::Refresh),
                        _ => match text.as_str() { "allow" => Some(Command::Allow), "deny" => Some(Command::Deny), "reset" => Some(Command::Reset), "off" | "q" => Some(Command::Stop), _ => None },
                    };
                    line.clear(); if let Some(cmd) = cmd { return cmd; }
                }
                Ok(Ok((_, byte))) => { if line.len() < 64 { line.push(byte); } },
                Err(_) => {},
            }
        }
    }
}
impl Drop for LocalUi {
    fn drop(&mut self) {
        #[cfg(uu_gtk)]
        if matches!(self, Self::Window) { unsafe { uu_assist_ui_close() } }
    }
}

fn response(status: u16, value: &Value) -> Result<&Value, String> {
    let code = value.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if status == 200 && code == 0 { return Ok(value.get("data").unwrap_or(&Value::Null)); }
    // No server message, validation payload, headers, URLs, token or code in errors. UU_ASSIST_DEBUG=1 adds
    // the server's short human-readable message (bounded) for diagnosing rejected guest calls.
    let message = (std::env::var("UU_ASSIST_DEBUG").ok().as_deref() == Some("1"))
        .then(|| value.get("message").or_else(|| value.get("msg")).and_then(Value::as_str)
            .map(|m| m.chars().filter(|c| !c.is_control()).take(120).collect::<String>()))
        .flatten().map(|m| format!(" message={m:?}")).unwrap_or_default();
    Err(format!("assist API rejected request: http={status} code={code}{message}"))
}
fn text(value: &Value, key: &str) -> Result<String, String> {
    value.get(key).and_then(Value::as_str).filter(|s| !s.is_empty() && s.len() <= 16384)
        .map(str::to_owned).ok_or_else(|| "assist response missing required string".into())
}
fn share_id(value: &Value) -> Result<String, String> {
    let id = text(value, "connect_id")?;
    if id.len() > 128 || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
        return Err("assist response invalid device identifier".into());
    }
    Ok(id)
}
fn generate_code() -> Result<String, String> {
    let mut random = std::fs::File::open("/dev/urandom").map_err(|_| "assist random source unavailable")?;
    let mut code = String::new();
    // Rejection sampling avoids modulo bias (alphabet has 32 entries).
    let limit = 256 / ALPHABET.len() * ALPHABET.len();
    while code.len() < 8 {
        let mut byte = [0u8]; random.read_exact(&mut byte).map_err(|_| "assist random source failed")?;
        if (byte[0] as usize) < limit { code.push(ALPHABET[byte[0] as usize % ALPHABET.len()] as char); }
    }
    Ok(code)
}
fn digest(salt: &str, code: &str) -> String {
    let mut hash = Sha256::new(); hash.update(salt.as_bytes()); hash.update(code.as_bytes()); hex::encode(hash.finalize())
}
/// Official: temporary-only signs the temporary code with an empty backup; both adds the custom code
/// as `backup_sign`; custom-only puts the custom code in the primary `sign`.
fn sign_body(control_id: &str, salt: &str, temporary: &str, custom: Option<&str>, verify: Verify, allow: bool) -> Value {
    let (primary, backup) = match (verify, custom) {
        (Verify::Custom, Some(c)) => (c, None),
        (Verify::Both, Some(c)) => (temporary, Some(c)),
        _ => (temporary, None),
    };
    json!({"can_remote_control": allow, "control_id": control_id,
        "sign": digest(salt, primary), "backup_sign": backup.map(|c| digest(salt, c)).unwrap_or_default()})
}
fn challenge(value: &Value) -> Option<(&str, &str)> {
    let id = value.get("control_id")?.as_str()?; let salt = value.get("salt")?.as_str()?;
    (!id.is_empty() && id.len() <= 256 && !salt.is_empty() && salt.len() <= 4096).then_some((id, salt))
}

#[derive(Default)]
struct RetryBackoff(u64);
impl RetryBackoff {
    fn next(&mut self) -> std::time::Duration { self.0 = (self.0 + 15).min(99); std::time::Duration::from_secs(self.0) }
}

pub struct Assist {
    _lock: std::fs::File,
    pub renewed: bool,
    device: Credentials,
    pub client: std::sync::Arc<UUAuthClient>,
    ui: std::sync::Arc<LocalUi>,
    code: String,
    custom: Option<String>,
    id: String,
    pub allowed: bool,
    verify: Verify,
    refresh: Refresh,
    code_at: std::time::Instant,
    /// Logged-in host: account endpoints and the host's own room (no guest identity).
    account: bool,
}
impl Assist {
    pub async fn start(shutdown: &mut tokio::sync::watch::Receiver<bool>, service: bool) -> Result<Self, String> {
        let ui = std::sync::Arc::new(LocalUi::open_visible(service)?);
        let lock = Self::lock()?;
        // guest/create rejects an unregistered device (HTTP 422 code 1001, verified live): register the
        // Windows device identity first, exactly as account login does (no account involved).
        let (config, device) = crate::login::prepare_device().await.map_err(|_| "assist device registration failed")?;
        let (custom, verify, refresh, allowed) = Self::settings(&config)?;
        let mut retry = RetryBackoff::default();
        let client = loop {
            if *shutdown.borrow() { return Err("assist stopped".into()); }
            let result = tokio::select! {
                result = Self::authenticate(&device) => result,
                _ = shutdown.changed() => return Err("assist stopped".into()),
                _ = ui.command() => return Err("assist stopped before guest identity was available".into()),
            };
            match result { Ok(client) => break client, Err(error) => {
                let delay = retry.next();
                // `error` carries only HTTP status / API code numbers or a fixed transport label.
                log::warn!("Guest identity request failed: {error}; retry_s={}", delay.as_secs());
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {},
                    _ = shutdown.changed() => return Err("assist stopped".into()),
                    _ = ui.command() => return Err("assist stopped before guest identity was available".into()),
                }
            }}
        };
        let this = Self { _lock: lock, renewed: false, device, client: std::sync::Arc::new(client), ui, code: generate_code()?, custom, id: String::new(),
            allowed, verify, refresh, code_at: std::time::Instant::now(), account: false };
        // guest/share/info answers 400/2004 until a guest room exists (verified live), so the ID/code
        // are published right after the first room(), which serve() requests before signaling.
        Ok(this)
    }
    fn settings(config: &Value) -> Result<(Option<String>, Verify, Refresh, bool), String> {
        let custom = config.get("assist_custom_code").and_then(Value::as_str).map(str::to_owned);
        if custom.as_deref().is_some_and(|c| !valid_custom(c)) {
            return Err("assist_custom_code requires 8-16 ASCII letters/digits, including both".into());
        }
        let setting = |key: &str| config.get(key).and_then(Value::as_str).map(str::to_owned);
        let verify = setting("assist_verify").as_deref().and_then(Verify::parse).filter(|v| *v == Verify::Temporary || custom.is_some()).unwrap_or(Verify::Temporary);
        let refresh = setting("assist_refresh").as_deref().and_then(Refresh::parse).unwrap_or(Refresh::Session);
        // Official default is "not allowed"; the switch state persists like isAllowRecipient.
        let allowed = std::env::var("UU_ASSIST_ALLOW").ok().as_deref() == Some("1")
            || config.get("assist_allow").and_then(Value::as_bool) == Some(true);
        Ok((custom, verify, refresh, allowed))
    }
    fn lock() -> Result<std::fs::File, String> {
        let lock_path = crate::config::path().with_file_name("assist.lock");
        if let Some(dir) = lock_path.parent() { std::fs::create_dir_all(dir).map_err(|_| "assist config directory unavailable")?; }
        let lock = std::fs::OpenOptions::new().create(true).write(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(lock_path)
            .map_err(|_| "assist lock unavailable")?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 { return Err("another assist host uses this config".into()); }
        Ok(lock)
    }
    /// Remote assistance on a logged-in host, shown in the service's hidden window.
    pub fn start_account(client: std::sync::Arc<UUAuthClient>, device: Credentials) -> Result<Self, String> {
        let lock = Self::lock()?;
        let ui = std::sync::Arc::new(LocalUi::open_service()?);
        let config = crate::config::read_fresh()?.unwrap_or_else(|| json!({}));
        let (custom, verify, refresh, allowed) = Self::settings(&config)?;
        let this = Self { _lock: lock, renewed: false, device, client, ui, code: generate_code()?, custom, id: String::new(),
            allowed, verify, refresh, code_at: std::time::Instant::now(), account: true };
        this.show()?;
        Ok(this)
    }
    pub fn is_account(&self) -> bool { self.account }
    /// Logged-in host: account name for the account card (saved at login, else /api/v1/user/info).
    pub async fn load_account_name(&self) {
        let saved = crate::config::read_fresh().ok().flatten()
            .and_then(|c| c.get("account_name").and_then(Value::as_str).map(str::to_owned)).filter(|n| !n.is_empty());
        let name = match saved {
            Some(name) => name,
            None => match self.client.assist_get("/api/v1/user/info").await {
                Ok((200, v)) => v.get("data").and_then(|d| ["nickname", "mobile", "phone"].iter()
                    .find_map(|k| d.get(*k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned)))
                    .unwrap_or_else(|| self.device.user_id.clone()),
                _ => self.device.user_id.clone(),
            },
        };
        self.ui.set_user(&display_account(&name));
    }
    /// Sign out: official /api/v1/user/logout (best effort), withdraw the share, drop local credentials.
    pub async fn logout(&mut self) {
        let _ = self.client.assist_post("/api/v1/user/logout", None).await;
        let _ = self.cancel().await;
        self.save_many(&[("token", Value::Null), ("csrf", Value::Null), ("user_id", Value::Null), ("account_name", Value::Null)]);
        log::info!("Signed out; restarting without an account");
    }
    pub fn ui(&self) -> std::sync::Arc<LocalUi> { self.ui.clone() }
    fn path(&self, guest: &'static str) -> &'static str {
        if !self.account { return guest; }
        match guest { INFO => ACCOUNT_INFO, CANCEL => ACCOUNT_CANCEL, SIGN => ACCOUNT_SIGN, CONTROL_MODE => ACCOUNT_CONTROL_MODE, other => other }
    }
    /// Logged-in host: publish the share ID/code once its own room exists (same ordering as guests).
    pub async fn ensure_published(&mut self) -> Result<(), String> {
        if self.id.is_empty() { self.publish().await } else { Ok(()) }
    }
    async fn authenticate(device: &Credentials) -> Result<UUAuthClient, String> {
        let client = UUAuthClient::new(device.clone());
        let (status, value) = client.assist_post(CREATE, None).await?;
        let data = response(status, &value)?;
        Ok(UUAuthClient::new_guest(device.clone(), text(data, "guest_id")?, text(data, "token")?))
    }
    async fn post(&mut self, path: &'static str, body: Option<&Value>) -> Result<Value, String> {
        let path = self.path(path);
        let (mut status, mut value) = if path == ACCOUNT_INFO { self.client.assist_get(path).await? } else { self.client.assist_post(path, body).await? };
        if self.account { return response(status, &value).map(Clone::clone); }
        // Official wrapper refreshes on 1120 and retries once, rather than on a guessed TTL.
        if value.get("code").and_then(Value::as_i64) == Some(crate::auth::TOKEN_INVALID) {
            self.client = std::sync::Arc::new(Self::authenticate(&self.device).await?);
            self.renewed = true;
            (status, value) = self.client.assist_post(path, body).await?;
        }
        response(status, &value).map(Clone::clone)
    }
    async fn publish(&mut self) -> Result<(), String> {
        self.id = share_id(&self.post(INFO, None).await?)?;
        self.show()
    }
    pub async fn room(&mut self) -> Result<RoomData, String> {
        let value = self.post(ROOM, None).await?;
        if self.renewed || self.id.is_empty() {
            if let Err(error) = self.publish().await { let _ = self.cancel().await; return Err(error); }
            self.renewed = false;
        }
        serde_json::from_value(value).map_err(|_| "assist room response malformed".into())
    }
    pub async fn challenge(&mut self, value: &Value) -> Result<(), String> {
        let (control, salt) = challenge(value).ok_or("assist challenge malformed")?;
        self.post(SIGN, Some(&sign_body(control, salt, &self.code, self.custom.as_deref(), self.verify, self.allowed))).await?;
        Ok(())
    }
    /// Answer the cloud's `get_control_mode` push (data {control_id}). Official Windows host
    /// (GameViewerServer RemoteAssistCode::respRemoteGetControlMode -> NrdBusinessEntry::uploadControlMode)
    /// posts {control_id, allow_control, control_mode: by_password|by_confirmation|password_confirmation}.
    /// Only the temporary/custom code mode is implemented, so this host reports `by_password`.
    pub async fn control_mode(&mut self, value: &Value) -> Result<(), String> {
        let control = value.get("control_id").and_then(Value::as_str).filter(|s| !s.is_empty() && s.len() <= 256)
            .ok_or("assist control mode request malformed")?;
        self.post(CONTROL_MODE, Some(&json!({"control_id": control, "allow_control": self.allowed, "control_mode": "by_password"}))).await.map(|_| ())
    }
    /// The code the controller types: the custom code in custom-only mode, else the temporary one.
    fn show(&self) -> Result<(), String> {
        let code = match (self.verify, &self.custom) { (Verify::Custom, Some(c)) => c.as_str(), _ => self.code.as_str() };
        self.ui.show(&self.id, code, self.allowed, self.verify, self.refresh, self.custom.is_some())
    }
    /// Saves one assist setting into the owner's 0600 config (custom code included, never logged).
    fn save_many(&self, entries: &[(&str, Value)]) {
        let mut cfg = std::fs::read(crate::config::path()).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .filter(Value::is_object).unwrap_or_else(|| json!({}));
        if let Some(map) = cfg.as_object_mut() { for (k, v) in entries { if v.is_null() { map.remove(*k); } else { map.insert((*k).to_owned(), v.clone()); } } }
        if crate::login::write_config(&cfg).is_err() { log::warn!("Account settings could not be saved"); }
    }
    fn save(&self, key: &str, value: Value) {
        // Re-read the file: config::file() is a startup snapshot that predates device registration,
        // so writing it back would drop device_id (and earlier saves) and change the device ID.
        let mut cfg = std::fs::read(crate::config::path()).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .filter(Value::is_object).unwrap_or_else(|| json!({}));
        cfg[key] = value;
        if crate::login::write_config(&cfg).is_err() { log::warn!("Assist setting could not be saved: key={key}"); }
    }
    pub fn reset(&mut self) -> Result<(), String> { self.code = generate_code()?; self.code_at = std::time::Instant::now(); self.show() }
    pub fn allow(&mut self) -> Result<(), String> { self.allowed = true; self.save("assist_allow", json!(true)); self.show() }
    pub fn deny(&mut self) -> Result<(), String> { self.allowed = false; self.save("assist_allow", json!(false)); self.show() }
    pub fn set_verify(&mut self, verify: Verify) -> Result<(), String> {
        if verify != Verify::Temporary && self.custom.is_none() { return self.show(); } // UI asks for a code first
        self.verify = verify; self.save("assist_verify", json!(verify.name())); self.show()
    }
    pub fn set_refresh(&mut self, refresh: Refresh) -> Result<(), String> {
        self.refresh = refresh; self.code_at = std::time::Instant::now(); self.save("assist_refresh", json!(refresh.name())); self.show()
    }
    pub fn set_custom(&mut self, code: String) -> Result<(), String> {
        if !valid_custom(&code) { return self.show(); }
        self.custom = Some(code.clone()); self.save("assist_custom_code", json!(code));
        if self.verify == Verify::Temporary { self.verify = Verify::Custom; self.save("assist_verify", json!(self.verify.name())); }
        self.show()
    }
    /// After the last desktop session: rotate only in "每次远控结束" mode.
    pub fn session_ended(&mut self) -> Result<(), String> { if self.refresh == Refresh::Session { self.reset() } else { Ok(()) } }
    /// Periodic rotation for the hourly/12 h/24 h settings.
    pub fn tick(&mut self) -> Result<(), String> {
        match self.refresh.period() { Some(p) if self.code_at.elapsed() >= p => self.reset(), _ => Ok(()) }
    }
    pub fn commands(&self, shutdown: tokio::sync::watch::Sender<bool>) -> (tokio::sync::mpsc::Receiver<Command>, tokio::task::JoinHandle<()>) {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let ui = self.ui.clone();
        let task = tokio::spawn(async move {
            loop {
                let command = tokio::select! { command = ui.command() => command, _ = tx.closed() => return };
                if matches!(command, Command::Stop) { let _ = shutdown.send(true); return; }
                match tx.try_send(command) {
                    Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {},
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return,
                }
            }
        });
        (rx, task)
    }
    pub async fn cancel(&mut self) -> Result<(), String> { self.post(CANCEL, None).await.map(|_| ()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn local_terminal_display_and_explicit_commands() {
        // Private PTY pair only, never the user's actual terminal; all values synthetic.
        let pair = nix::pty::openpty(None, None).unwrap();
        let slave = std::fs::File::from(pair.slave);
        unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK); }
        let ui = LocalUi::Tty(tokio::io::unix::AsyncFd::new(slave).unwrap());
        let mut master = std::fs::File::from(pair.master);
        ui.show("fixture-123", "FIXTURE8", false, Verify::Temporary, Refresh::Session, false).unwrap();
        // A PTY may deliver the banner in several chunks: read until complete (bounded).
        let mut text = String::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let complete = |t: &str| t.contains("fixture-123") && t.contains("FIXTURE8") && t.contains("false") && t.contains("off=");
        while !complete(&text) && std::time::Instant::now() < deadline {
            let mut bytes = [0u8; 1024];
            match master.read(&mut bytes) { Ok(n) if n > 0 => text.push_str(&String::from_utf8_lossy(&bytes[..n])), _ => break }
        }
        assert!(complete(&text));
        for (input, expected) in [(b"unknown\nallow\n".as_slice(), 0), (b"reset\n".as_slice(), 1), (b"off\n".as_slice(), 2)] {
            master.write_all(input).unwrap();
            let command = tokio::time::timeout(std::time::Duration::from_secs(2), ui.command()).await.unwrap();
            assert!(matches!((command, expected), (Command::Allow, 0) | (Command::Reset, 1) | (Command::Stop, 2)));
        }
    }
    #[test]
    fn official_challenge_sign_and_shape() {
        // Synthetic fixture, independently calculated SHA256("saltABC234XY").
        let body = sign_body("fixture-control", "salt", "ABC234XY", None, Verify::Temporary, false);
        assert_eq!(body.as_object().unwrap().len(), 4);
        assert_eq!(body["can_remote_control"], false);
        assert_eq!(body["control_id"], "fixture-control");
        assert_eq!(body["sign"], "dc747ee8adb0e591f5ff4ab6cabcaf3a0ec312851580f225d4ca140766229c94");
        assert_eq!(body["backup_sign"], "");
        let both = sign_body("fixture", "salt", "ABC234XY", Some("test1234"), Verify::Both, true);
        assert_ne!(both["sign"], both["backup_sign"]);
        assert_eq!(both["can_remote_control"], true);
    }
    #[test]
    fn rejects_missing_or_unbounded_challenges_and_terminal_injection() {
        for value in [json!({}), json!({"control_id":"x", "salt":""}), json!({"control_id":12,"salt":"s"}), json!({"control_id":"x","salt":"s".repeat(4097)})] {
            assert!(challenge(&value).is_none());
        }
        assert!(challenge(&json!({"control_id":"fixture", "salt":"fixture"})).is_some());
        assert!(share_id(&json!({"connect_id":"fixture-123"})).is_ok());
        for id in ["", "\u{1b}[2J", "a\nb", "x\u{7}"] { assert!(share_id(&json!({"connect_id":id})).is_err()); }
    }
    #[test]
    fn responses_do_not_disclose_untrusted_server_errors() {
        let error = response(422, &json!({"code":1120,"msg":"synthetic-secret","data":{"token":"synthetic-secret"}})).unwrap_err();
        assert!(!error.contains("synthetic-secret"));
        assert!(response(200, &json!({"code":0})).unwrap().is_null());
        assert!(text(&Value::Null, "token").is_err());
        assert_eq!(text(&json!({"guest_id":"fixture-guest"}), "guest_id").unwrap(), "fixture-guest");
    }
    #[test]
    fn official_guest_failure_retry_schedule() {
        let mut retry = RetryBackoff::default();
        let actual: Vec<_> = (0..9).map(|_| retry.next().as_secs()).collect();
        assert_eq!(actual, [15,30,45,60,75,90,99,99,99]);
    }
    #[test]
    fn secure_temporary_code_format() {
        for _ in 0..64 { let code = generate_code().unwrap(); assert_eq!(code.len(),8); assert!(code.bytes().all(|b| ALPHABET.contains(&b))); }
    }
}
