//! Official SimpleAction system commands from the viewer's 安全 / 更多 menus, mapped to the
//! Linux desktop session: lock (logind), show desktop, system monitor, reboot/power-off
//! (systemctl through logind/polkit), host mute (wpctl / pactl) and lock-after-session.
//! Declared through FeatureFlag ff_simple_action = FF_SIMPLE_ACTION_AUTO_LOCK (2, includes
//! MUTING) and ff_system_metrics = FF_SYSTEM_METRICS_SETTING_AUTO_LOCK (2).

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

pub const FF_SIMPLE_ACTION: u8 = 2;
pub const FF_SYSTEM_METRICS: u8 = 2;

pub const SHOW_DESKTOP: u64 = 2;
/// Opens the desktop on an existing connection (the viewer's port mapping reuses its peer, #99).
pub const CAPTURE_DESKTOP: u64 = 4;
pub const REBOOT_SYSTEM: u64 = 9;
pub const LOCK_SYSTEM: u64 = 10;
pub const SHUTDOWN_SYSTEM: u64 = 12;
pub const SHOW_TASKMGR: u64 = 13;
pub const MUTE_SYSTEM: u64 = 14;
pub const MUTE_RESTORE: u64 = 16;
pub const MUTE_PERMANENT: u64 = 17;
pub const MUTE_TEMPORARY: u64 = 18;
pub const SET_AUTO_LOCK: u64 = 19;

/// Host settings from the viewer's 安全 menu. The viewer sends them only when a switch is toggled
/// and shows what SystemMetrics reports, so they are host-wide and persisted (like the official
/// host), not per connection: a reconnect must still lock / mute.
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Settings {
    #[serde(default)] auto_lock: bool,
    #[serde(default)] keep_mute: bool,
    #[serde(default)] mute_permanent: bool,
    /// "auto" or absent does not override the environment. A non-string value becomes None so a bad
    /// video field cannot drop auto_lock / mute when the document is loaded.
    #[serde(default, deserialize_with = "lenient_string")] pub(crate) video_encoder: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")] pub(crate) video_codec: Option<String>,
}

fn lenient_string<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    use serde::Deserialize;
    match Option::<serde_json::Value>::deserialize(deserializer)? {
        Some(serde_json::Value::String(s)) => Ok(Some(s)),
        Some(_) => {
            log::warn!("Ignoring non-string video_encoder/video_codec in host settings");
            Ok(None)
        }
        None => Ok(None),
    }
}

struct HostState {
    settings: Settings,
    /// Host mute state before remote control muted it (restored by MUTE_RESTORE / last session end).
    restore: Option<bool>,
    desktop_sessions: usize,
}

pub(crate) fn settings_path() -> std::path::PathBuf { crate::config::path().with_file_name("host_settings.json") }

pub(crate) fn load_settings() -> Settings {
    std::fs::read(settings_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Re-read, apply `patch`, write via a temp file and rename (mode 0600).
pub(crate) fn merge_save(patch: impl FnOnce(&mut Settings)) -> Result<Settings, String> {
    merge_write(patch)?;
    let settings = load_settings();
    if let Ok(mut guard) = state().lock() { guard.settings = settings.clone(); }
    Ok(settings)
}

fn state() -> &'static Mutex<HostState> {
    static STATE: std::sync::OnceLock<Mutex<HostState>> = std::sync::OnceLock::new();
    STATE.get_or_init(|| {
        let settings = load_settings();
        Mutex::new(HostState { settings, restore: None, desktop_sessions: 0 })
    })
}

/// Re-read, copy only the security switches from `settings`, keep video_* from disk.
fn save(settings: &Settings) -> Result<(), String> {
    merge_write(|disk| {
        disk.auto_lock = settings.auto_lock;
        disk.keep_mute = settings.keep_mute;
        disk.mute_permanent = settings.mute_permanent;
    })
}

/// Held across read-patch-write-rename so set-* and a security-toggle save cannot clobber each other.
struct SettingsLock { file: std::fs::File }
impl SettingsLock {
    fn acquire(path: &std::path::Path) -> Result<Self, String> {
        use std::os::unix::fs::OpenOptionsExt;
        let lock_path = path.with_file_name("host_settings.json.lock");
        let file = std::fs::OpenOptions::new().read(true).write(true).create(true).mode(0o600).open(&lock_path).map_err(|e| e.kind().to_string())?;
        let rc = unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&file), libc::LOCK_EX) };
        if rc != 0 { return Err(std::io::Error::last_os_error().kind().to_string()); }
        Ok(Self { file })
    }
}
impl Drop for SettingsLock {
    fn drop(&mut self) {
        unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&self.file), libc::LOCK_UN); }
    }
}

/// NotFound starts from defaults. Any other read or parse error leaves the file untouched:
/// a bad document must not be rewritten with security switches forced off.
fn read_for_merge(path: &std::path::Path) -> Result<Settings, String> {
    match std::fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(format!("host settings unreadable: {}", e.kind())),
        // serde_json::Error's Display quotes the offending value; the journal and set-* must not.
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| match (e.line(), e.column()) {
            (0, 0) => "host_settings.json is not valid".to_owned(),
            (line, column) => format!("host_settings.json is not valid (line {line}, column {column})"),
        }),
    }
}

fn merge_write(patch: impl FnOnce(&mut Settings)) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = settings_path();
    if let Some(dir) = path.parent() { std::fs::create_dir_all(dir).map_err(|e| e.kind().to_string())?; }
    let _lock = SettingsLock::acquire(&path)?;
    let mut disk = read_for_merge(&path)?;
    patch(&mut disk);
    let json = serde_json::to_vec_pretty(&disk).map_err(|e| e.to_string())?;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).map_err(|e| e.kind().to_string())?;
    file.write_all(&json).map_err(|e| e.kind().to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.kind().to_string())?;
    Ok(())
}

/// Handle for one desktop connection (system actions are host-wide).
#[derive(Default)]
pub struct PeerActions { started: AtomicBool }

/// Power actions pass --no-ask-password: without polkit permission (e.g. no active local session) systemctl
/// would otherwise wait for an authentication dialog on the controlled screen; fail and log instead.
fn quiet(program: &str, args: &[&str]) -> bool {
    Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .status().map(|s| s.success()).unwrap_or(false)
}

fn detached(program: &str, args: &[&str]) -> bool {
    Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .spawn().map(|mut child| { std::thread::spawn(move || child.wait()); true }).unwrap_or(false)
}

/// Current mute state of the default output (None = no audio control available).
pub fn host_muted() -> Option<bool> {
    if let Ok(out) = Command::new("wpctl").args(["get-volume", "@DEFAULT_AUDIO_SINK@"]).output() {
        if out.status.success() { return Some(String::from_utf8_lossy(&out.stdout).contains("MUTED")); }
    }
    let out = Command::new("pactl").args(["get-sink-mute", "@DEFAULT_SINK@"]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).contains("yes"))
}

pub fn set_host_muted(muted: bool) -> bool {
    let flag = if muted { "1" } else { "0" };
    quiet("wpctl", &["set-mute", "@DEFAULT_AUDIO_SINK@", flag]) || quiet("pactl", &["set-sink-mute", "@DEFAULT_SINK@", flag])
}

pub fn lock_session() -> bool { quiet("loginctl", &["lock-session"]) }

/// The on/off value of a toggle action: SimpleAction.value (field 5), else args JSON/text; default on.
pub fn toggle_value(value: Option<u64>, args: &[u8]) -> bool {
    if let Some(v) = value { return v != 0; }
    let text = String::from_utf8_lossy(args);
    let text = text.trim();
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(text) {
        let scalar = |v: &serde_json::Value| v.as_bool().or_else(|| v.as_i64().map(|n| n != 0))
            .or_else(|| v.as_str().map(|s| matches!(s, "1" | "true" | "on")));
        if let Some(b) = scalar(&json) { return b; }
        if let Some(b) = json.as_object().and_then(|o| o.values().find_map(scalar)) { return b; }
    }
    !matches!(text, "0" | "false" | "off")
}

/// Shape of action args for diagnostics: JSON keys and scalar types, never values.
pub fn args_shape(args: &[u8]) -> String {
    match serde_json::from_slice::<serde_json::Value>(args) {
        Ok(serde_json::Value::Object(o)) => format!("{:?}", o.iter().map(|(k, v)| (k.chars().take(32).collect::<String>(), match v {
            serde_json::Value::Bool(_) => "bool", serde_json::Value::Number(_) => "number",
            serde_json::Value::String(_) => "string", _ => "other" })).collect::<Vec<_>>()),
        Ok(_) => "json scalar".into(),
        Err(_) => format!("{} bytes non-json", args.len()),
    }
}

fn mute_for_session(state: &mut HostState) {
    if state.restore.is_none() {
        state.restore = host_muted();
        set_host_muted(true);
    }
    crate::virtual_audio::enable();
}
fn restore_mute(state: &mut HostState) {
    crate::virtual_audio::disable();
    if let Some(previous) = state.restore.take() { set_host_muted(previous); }
}

impl PeerActions {
    /// Runs one system action; returns true when the settings report should be resent.
    /// Blocking (spawns system tools); call from spawn_blocking.
    pub fn handle(&self, action: u64, value: Option<u64>, args: &[u8], show_desktop: impl FnOnce()) -> bool {
        match action {
            SHOW_DESKTOP => { show_desktop(); log::info!("System action show_desktop"); return false }
            LOCK_SYSTEM => { log::info!("System action lock_system: ok={}", lock_session()); return false }
            SHOW_TASKMGR => {
                let ok = detached("gnome-system-monitor", &[]) || detached("plasma-systemmonitor", &[]) || detached("xfce4-taskmanager", &[]);
                log::info!("System action show_taskmgr: ok={}", ok); return false
            }
            REBOOT_SYSTEM => { log::warn!("System action reboot_system requested by viewer"); log::info!("reboot ok={}", quiet("systemctl", &["--no-ask-password", "reboot"])); return false }
            SHUTDOWN_SYSTEM => { log::warn!("System action shutdown_system requested by viewer"); log::info!("poweroff ok={}", quiet("systemctl", &["--no-ask-password", "poweroff"])); return false }
            _ => {}
        }
        let mut state = state().lock().unwrap();
        let before = state.settings.clone();
        match action {
            // 静音运行 on: the viewer sends MUTE_SYSTEM + MUTE_TEMPORARY (observed), off: MUTE_RESTORE.
            MUTE_SYSTEM | MUTE_TEMPORARY => {
                let on = toggle_value(value, args);
                state.settings.keep_mute = on;
                if on { mute_for_session(&mut state); } else { restore_mute(&mut state); }
                log::info!("System action mute (action {}): on={} virtual_sink={} default_muted={:?}", action, on, crate::virtual_audio::active(), host_muted());
            }
            // Keep the host muted after the session (远程结束后，恢复被控端声音设置 unchecked).
            MUTE_PERMANENT => {
                let on = toggle_value(value, args);
                state.settings.mute_permanent = on;
                if on { state.settings.keep_mute = true; mute_for_session(&mut state); }
                log::info!("System action mute_permanent: on={} virtual_sink={} default_muted={:?}", on, crate::virtual_audio::active(), host_muted());
            }
            MUTE_RESTORE => {
                state.settings.keep_mute = false;
                state.settings.mute_permanent = false;
                restore_mute(&mut state);
                log::info!("System action mute_restore: virtual_sink={} default_muted={:?}", crate::virtual_audio::active(), host_muted());
            }
            SET_AUTO_LOCK => {
                // Observed: enabling 远程结束后自动锁屏 sends value 2 and disabling value 1.
                let on = match value { Some(v) => v == 2, None => toggle_value(None, args) };
                state.settings.auto_lock = on;
                log::info!("System action set_auto_lock: on={}", on);
            }
            _ => return false,
        }
        if state.settings != before { if let Err(e) = save(&state.settings) { log::warn!("Host settings not saved: {e}"); } }
        true
    }

    /// Desktop connection started: re-apply 静音运行 for the first remote session.
    pub fn session_started(&self) {
        if self.started.swap(true, Ordering::Relaxed) { return; }
        let mut state = state().lock().unwrap();
        state.desktop_sessions += 1;
        if state.desktop_sessions == 1 && state.settings.keep_mute {
            mute_for_session(&mut state);
            log::info!("Host muted for remote session (静音运行)");
        }
    }

    /// Desktop connection ended. After the last one: undo a session-only mute and lock if the
    /// viewer asked for it.
    pub fn session_ended(&self) {
        if !self.started.swap(false, Ordering::Relaxed) { return; }
        let mut state = state().lock().unwrap();
        state.desktop_sessions = state.desktop_sessions.saturating_sub(1);
        if state.desktop_sessions > 0 { return; }
        crate::virtual_audio::disable();
        if !state.settings.mute_permanent { restore_mute(&mut state); }
        if state.settings.auto_lock {
            log::info!("Auto lock after remote session: ok={}", lock_session());
        }
    }

    /// Message 13 SystemMetrics: permissions, file-transfer folder and the current settings.
    pub fn metrics_payload(&self, sound: bool, desktop: crate::system_metrics::Permission) -> Vec<u8> {
        use crate::system_metrics::payload_with_settings;
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        let current = state().lock().unwrap().settings.clone();
        payload_with_settings(desktop, desktop, desktop, &home, &crate::system_metrics::Settings {
            sound, keep_mute: current.keep_mute, mute_permanent: current.mute_permanent, auto_lock: current.auto_lock,
        })
    }
}

/// Session RPCs answered with success: 15 send_video_track_req -> 11 (the single desktop track
/// 0 is always sent), 21 virtual_audio_driver_policy_req -> 17 (no virtual driver needed: the
/// sink monitor is captured directly). Responses carry error_code 0 (default).
pub fn session_rpc_response(rpc: &[crate::protocol::WireField]) -> Option<Vec<u8>> {
    use crate::protocol::{decode_wire, WireValue};
    let body = |n: u32| rpc.iter().find_map(|f| match (f.number, &f.value) { (x, WireValue::Bytes(b)) if x == n => Some(*b), _ => None });
    let (request, response_field) = [(15u32, 11u8), (21, 17)].into_iter().find_map(|(req, resp)| body(req).map(|b| (b, resp)))?;
    let value = decode_wire(request).ok().and_then(|f| f.iter().find_map(|f| match (f.number, &f.value) { (1, WireValue::Varint(v)) => Some(*v), _ => None }));
    log::info!("Session RPC answered: response_field={} value={:?}", response_field, value);
    let header = body(1).unwrap_or(&[]);
    let mut out = vec![10, header.len() as u8];
    out.extend_from_slice(header);
    out.extend_from_slice(&[response_field << 3 | 2, 0]);
    Some(out)
}

/// Plugin RPCs (魔兽世界排队提醒工具): no plugins exist on Linux. Query answers "disabled"
/// (QueryPluginSettingResponse field 12); enable/install/notification requests answer error_code 1.
pub fn plugin_response(rpc: &[crate::protocol::WireField]) -> Option<Vec<u8>> {
    use crate::protocol::{decode_wire, WireValue};
    fn varint(mut v: u64, out: &mut Vec<u8>) { while v >= 128 { out.push((v as u8 & 127) | 128); v >>= 7; } out.push(v as u8); }
    fn bytes(field: u64, v: &[u8], out: &mut Vec<u8>) { varint(field << 3 | 2, out); varint(v.len() as u64, out); out.extend_from_slice(v); }
    let body = |n: u32| rpc.iter().find_map(|f| match (f.number, &f.value) { (x, WireValue::Bytes(b)) if x == n => Some(*b), _ => None });
    // (request field, response field, plugin_id field in the response)
    let (request, response_field, id_field): (&[u8], u64, u64) = [(16u32, 12u64, 3u64), (17, 13, 3), (18, 14, 2), (19, 15, 2), (20, 16, 2)]
        .into_iter().find_map(|(req, resp, id)| body(req).map(|b| (b, resp, id)))?;
    let plugin_id = decode_wire(request).ok()?.iter().find_map(|f| match (f.number, &f.value) { (1, WireValue::Bytes(b)) => Some(b.to_vec()), _ => None }).unwrap_or_default();
    let mut payload = Vec::new();
    if response_field != 12 { payload.extend_from_slice(&[8, 1]); } // error_code 1: unsupported
    bytes(id_field, &plugin_id, &mut payload); // query: error_code 0, enable false (defaults)
    let mut out = Vec::new();
    bytes(1, body(1).unwrap_or(&[]), &mut out);
    bytes(response_field, &payload, &mut out);
    log::info!("Plugin RPC answered: response_field={} (no Linux plugins)", response_field);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn toggle_values_from_value_field_json_and_text() {
        assert!(!toggle_value(Some(0), b"1"));
        assert!(toggle_value(Some(1), b""));
        assert!(!toggle_value(None, br#"{"enable":false}"#));
        assert!(toggle_value(None, br#"{"value":1}"#));
        assert!(!toggle_value(None, b"0"));
        assert!(toggle_value(None, b""));
    }
    #[test]
    fn plugin_query_answers_disabled_with_same_request_and_plugin_id() {
        use crate::protocol::{decode_wire, WireValue};
        // RpcRequest { 1 header{1:9}, 16 query{1:"wow"} }
        let request = [10, 2, 8, 9, 130, 1, 5, 10, 3, b'w', b'o', b'w'];
        let response = plugin_response(&decode_wire(&request).unwrap()).unwrap();
        let fields = decode_wire(&response).unwrap();
        assert_eq!(fields[0].value, WireValue::Bytes(&[8, 9]));
        assert_eq!(fields[1].number, 12);
        assert_eq!(fields[1].value, WireValue::Bytes(&[26, 3, b'w', b'o', b'w']));
        assert!(plugin_response(&decode_wire(&[10, 2, 8, 9]).unwrap()).is_none());
    }
    #[test]
    fn merge_save_keeps_security_fields_and_mode() {
        let _guard = crate::config::test_env_lock().lock().unwrap();
        let dir = std::env::temp_dir().join(format!("uu-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.json");
        std::env::set_var("UU_CONFIG", &cfg);
        let path = settings_path();
        std::fs::write(&path, br#"{"auto_lock":true,"keep_mute":true,"mute_permanent":false}"#).unwrap();
        let saved = merge_save(|s| s.video_encoder = Some("vaapi".into())).unwrap();
        assert!(saved.auto_lock && saved.keep_mute && !saved.mute_permanent);
        assert_eq!(saved.video_encoder.as_deref(), Some("vaapi"));
        let again = merge_save(|s| s.video_codec = Some("h264".into())).unwrap();
        assert_eq!(again.video_encoder.as_deref(), Some("vaapi"));
        assert_eq!(again.video_codec.as_deref(), Some("h264"));
        assert!(again.auto_lock);
        let mode = { use std::os::unix::fs::PermissionsExt; std::fs::metadata(&path).unwrap().permissions().mode() & 0o777 };
        assert_eq!(mode, 0o600);
        // A mute/lock save must not drop video_* written after the in-memory copy was taken.
        let mut memory = load_settings();
        std::fs::write(&path, br#"{"auto_lock":true,"keep_mute":true,"mute_permanent":false,"video_encoder":"nvenc","video_codec":"h265"}"#).unwrap();
        memory.keep_mute = false;
        save(&memory).unwrap();
        let disk: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(disk["video_encoder"], "nvenc");
        assert_eq!(disk["video_codec"], "h265");
        assert_eq!(disk["keep_mute"], false);
        assert_eq!(disk["auto_lock"], true);
        // A non-string video field must not fail the whole document (security switches survive).
        std::fs::write(&path, br#"{"auto_lock":true,"keep_mute":false,"mute_permanent":true,"video_encoder":1,"video_codec":{"x":1}}"#).unwrap();
        let loaded = load_settings();
        assert!(loaded.auto_lock && !loaded.keep_mute && loaded.mute_permanent);
        assert!(loaded.video_encoder.is_none() && loaded.video_codec.is_none());
        merge_save(|s| s.video_encoder = Some("cpu".into())).unwrap();
        let disk: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(disk["auto_lock"], true);
        assert_eq!(disk["mute_permanent"], true);
        assert_eq!(disk["video_encoder"], "cpu");
        let _ = std::fs::remove_dir_all(&dir);
        std::env::remove_var("UU_CONFIG");
    }
    #[test]
    fn merge_write_refuses_malformed_or_unreadable_settings() {
        let _guard = crate::config::test_env_lock().lock().unwrap();
        let dir = std::env::temp_dir().join(format!("uu-settings-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("UU_CONFIG", dir.join("config.json"));
        let path = settings_path();
        let malformed = br#"{"auto_lock":true, "keep_mute":"#;
        std::fs::write(&path, malformed).unwrap();
        let err = merge_write(|s| s.video_encoder = Some("cpu".into())).unwrap_err();
        assert!(err.contains("not valid"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), malformed);
        let bad_type = br#"{"auto_lock":"yes","keep_mute":false,"mute_permanent":false}"#;
        std::fs::write(&path, bad_type).unwrap();
        let err = merge_write(|s| s.video_codec = Some("h265".into())).unwrap_err();
        assert!(err.contains("not valid"), "{err}");
        assert!(err.contains("line ") && err.contains("column "), "{err}");
        assert!(!err.contains("yes"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), bad_type);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let err = merge_write(|s| s.video_encoder = Some("cpu".into())).unwrap_err();
        assert!(err.contains("unreadable"), "{err}");
        assert!(path.is_dir());
        let _ = std::fs::remove_dir_all(&dir);
        std::env::remove_var("UU_CONFIG");
    }
    #[test]
    fn args_shape_never_contains_values() {
        let shape = args_shape(br#"{"enable":true,"secret":"abc"}"#);
        assert!(shape.contains("enable") && !shape.contains("abc"));
    }
}
