//! Lock-screen state and remote auto-unlock for the desktop session.
//!
//! Official protocol (UURemoteServer LockScreenManager / SimpleActionHandler, see
//! analysis/protocol-fix-20261013/control): the host reports Message 15 SystemStateChange
//! { 1: SecureDesktopState { 1 is_locked, 2 is_sas_shown, 5 is_support_auto_logon } } and the
//! viewer's saved password arrives as SimpleAction { 1: 15 AUTO_UNLOCK_SYSTEM, 2: {"key": ...} }.
//! A failed unlock is answered with Message 12 ReportError { 2: -5, 3: "... auto unlock failed" }.
//!
//! The Mac host types the password into loginwindow through Accessibility and waits for the
//! unlock. Here the password is verified with PAM and the session is unlocked through logind
//! (`loginctl unlock-session`, allowed for the session owner), which avoids remote keystrokes
//! and gives an exact wrong-password result. The password is never logged or stored.

use std::process::{Command, Stdio};
use std::path::{Path, PathBuf};

use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::watch;

mod logind;

/// Official UnlockResult cases (LockScreenManager).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnlockResult { Success, WrongPassword, NoLoggedInUser, NoPermission, Failed }

/// Official ReportError.error_code for a failed auto unlock.
pub const UNLOCK_FAILED_CODE: i32 = -5;

#[cfg(target_os = "linux")]
extern "C" {
    fn uu_pam_available() -> libc::c_int;
    fn uu_pam_check(service: *const libc::c_char, user: *const libc::c_char, password: *const libc::c_char) -> libc::c_int;
}

// Resolve once: execvp's unsuccessful PATH candidates are also recorded by auditd.
fn loginctl_path() -> Option<&'static Path> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        std::env::var_os("PATH").into_iter().flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .map(|dir| dir.join("loginctl"))
            .find(|path| path.is_file() && std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
                .map(|p| unsafe { libc::access(p.as_ptr(), libc::X_OK) == 0 }).unwrap_or(false))
            .and_then(|path| std::fs::canonicalize(path).ok())
    }).as_deref()
}

#[cfg(test)]
static COMMAND_STARTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn bounded_command(path: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    use std::io::Read;
    #[cfg(test)]
    COMMAND_STARTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let mut child = Command::new(path).args(args).stdin(Stdio::null()).stdout(Stdio::piped())
        .stderr(Stdio::null()).spawn().ok()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() { return None; }
                let mut text = String::new();
                child.stdout.take()?.take(4096).read_to_string(&mut text).ok()?;
                return Some(text.trim().to_owned());
            }
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => { let _ = child.kill(); let _ = child.wait(); return None; }
        }
    }
}

fn loginctl(args: &[&str]) -> Option<String> {
    bounded_command(loginctl_path()?, args, Duration::from_secs(3)).filter(|s| !s.is_empty())
}

/// The logind session that owns this user's graphical display.
fn display_session() -> Option<String> {
    let uid = unsafe { libc::getuid() }.to_string();
    loginctl(&["show-user", &uid, "-p", "Display", "--value"])
}

pub fn locked_now() -> Option<bool> { display_session().as_deref().and_then(session_locked) }

fn session_locked(session: &str) -> Option<bool> {
    loginctl(&["show-session", session, "-p", "LockedHint", "--value"]).and_then(|v| match v.as_str() { "yes" => Some(true), "no" => Some(false), _ => None })
}

fn user_name() -> Option<String> {
    let pw = unsafe { libc::getpwuid(libc::getuid()) };
    if pw.is_null() { return None; }
    unsafe { std::ffi::CStr::from_ptr((*pw).pw_name) }.to_str().ok().map(str::to_owned)
}

fn pam_available() -> bool {
    #[cfg(target_os = "linux")]
    { unsafe { uu_pam_available() != 0 } }
    #[cfg(not(target_os = "linux"))]
    { false }
}

/// PAM "login" service check of the user's own password: 0 ok, 1 wrong, -1 unavailable.
#[cfg(target_os = "linux")]
fn pam_check(user: &str, password: &str) -> i32 {
    let (Ok(service), Ok(user), Ok(pass)) = (std::ffi::CString::new("login"), std::ffi::CString::new(user),
        std::ffi::CString::new(password)) else { return -1 };
    let verdict = unsafe { uu_pam_check(service.as_ptr(), user.as_ptr(), pass.as_ptr()) };
    pass.into_bytes_with_nul().fill(0);
    verdict
}
#[cfg(not(target_os = "linux"))]
fn pam_check(_user: &str, _password: &str) -> i32 { -1 }

/// Auto unlock is offered when there is a graphical logind session and PAM can be loaded.
pub fn supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| pam_available() && display_session().is_some())
}

/// Current lock state (None = unknown), shared by all desktop connections.
pub fn watch_locked() -> watch::Receiver<Option<bool>> {
    static STATE: OnceLock<watch::Sender<Option<bool>>> = OnceLock::new();
    STATE.get_or_init(|| {
        let (tx, _) = watch::channel(None);
        let sender = tx.clone();
        std::thread::Builder::new().name("uu-lock-state".into()).spawn(move || {
            tokio::runtime::Builder::new_current_thread().enable_all().build()
                .expect("lock monitor runtime").block_on(logind::monitor(sender));
        }).expect("spawn lock state thread");
        tx
    }).subscribe()
}

fn publish_lock_state(sender: &watch::Sender<Option<bool>>, locked: Option<bool>) {
    sender.send_if_modified(|current| if *current != locked {
        log::info!("Desktop lock state: {}", match locked { Some(true) => "locked", Some(false) => "unlocked", None => "unknown" });
        if locked == Some(true) { log::info!("Desktop is locked; capture continues while the compositor supplies frames"); }
        *current = locked; true
    } else { false });
}

/// Verifies the session user's password and unlocks the display session. Blocking (PAM may
/// delay a failure by seconds); call from spawn_blocking. Consumes and wipes the password.
pub fn unlock(password: String) -> UnlockResult {
    let mut password = password;
    let result = unlock_with(&password);
    unsafe { password.as_bytes_mut().fill(0); }
    result
}

fn unlock_with(password: &str) -> UnlockResult {
    let Some(session) = display_session() else { return UnlockResult::NoLoggedInUser };
    if session_locked(&session) == Some(false) { return UnlockResult::Success; }
    let Some(user) = user_name() else { return UnlockResult::NoLoggedInUser };
    if password.is_empty() || password.contains('\0') { return UnlockResult::WrongPassword; }
    match pam_check(&user, password) { 0 => {}, 1 => return UnlockResult::WrongPassword, _ => return UnlockResult::Failed }
    if loginctl_path().and_then(|path| bounded_command(path, &["unlock-session", &session], Duration::from_secs(3))).is_none() {
        return UnlockResult::NoPermission;
    }
    if let Err(error) = crate::capture::keep_display_awake() {
        log::warn!("Display wake after auto unlock failed: {error}");
    }
    // Post-unlock confirmation like the official host: the lock screen must actually go away.
    for _ in 0..100 { // GNOME takes ~3 s to drop the shield after logind's Unlock
        if session_locked(&session) == Some(false) { return UnlockResult::Success; }
        std::thread::sleep(Duration::from_millis(100));
    }
    UnlockResult::Failed
}

type Channel = std::sync::Arc<webrtc::data_channel::RTCDataChannel>;
type Sequence = std::sync::Arc<std::sync::atomic::AtomicU64>;

fn now() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() }

/// Message 15 SystemStateChange { 1: SecureDesktopState } on CONTROL.
pub async fn send_state(channel: &Channel, sequence: &Sequence, locked: bool) -> bool {
    let seq = sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let message = crate::initialization::encode_secure_desktop(seq, now(), locked, false, supported());
    match channel.send(&bytes::Bytes::from(message)).await {
        Ok(_) => {
            log::info!("Secure desktop state sent: locked={} auto_unlock={}", locked, supported());
            true
        }
        Err(e) => { log::warn!("Secure desktop state send failed: {e}"); false }
    }
}

/// Reports the lock state to one desktop viewer: now, then on every change, until it closes.
pub async fn report_states(channel: Channel, sequence: Sequence, mut state: watch::Receiver<Option<bool>>) {
    use webrtc::data_channel::data_channel_state::RTCDataChannelState;
    let mut sent = None;
    loop {
        let locked = *state.borrow_and_update();
        if let Some(locked) = locked.filter(|l| sent != Some(*l)) {
            if send_state(&channel, &sequence, locked).await { sent = Some(locked); }
        }
        tokio::select! {
            changed = state.changed() => if changed.is_err() { return; },
            _ = tokio::time::sleep(Duration::from_secs(2)) => {},
        }
        if channel.ready_state() != RTCDataChannelState::Open { return; }
    }
}

/// SimpleAction AUTO_UNLOCK_SYSTEM: unlock, log only the outcome, report failure like the Mac host.
pub async fn handle_auto_unlock(password: Option<String>, channel: Channel, sequence: Sequence) {
    let result = match password {
        Some(password) => tokio::task::spawn_blocking(move || unlock(password)).await.unwrap_or(UnlockResult::Failed),
        None => UnlockResult::WrongPassword,
    };
    log::info!("Auto unlock result: {:?}", result);
    if result != UnlockResult::Success {
        let seq = sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let message = crate::initialization::encode_report_error(seq, now(), UNLOCK_FAILED_CODE, "linux auto unlock failed");
        if let Err(e) = channel.send(&bytes::Bytes::from(message)).await { log::warn!("Auto unlock failure report failed: {e}"); }
    }
}

/// `uuremote-server unlock-check`: diagnostics for auto unlock; password from stdin, never echoed.
pub fn check_from_stdin() -> Result<(), String> {
    let session = display_session();
    println!("display_session={} locked={:?} pam={} auto_unlock_supported={}", session.as_deref().unwrap_or("-"),
        session.as_deref().and_then(session_locked), pam_available(), supported());
    let mut password = String::new();
    std::io::stdin().read_line(&mut password).map_err(|e| e.to_string())?;
    let mut password = password.trim_end_matches(['\r', '\n']).to_string();
    let user = user_name().ok_or("no user name")?;
    let verdict = pam_check(&user, &password);
    unsafe { password.as_bytes_mut().fill(0); }
    println!("password={}", match verdict { 0 => "accepted", 1 => "rejected", _ => "unverifiable (PAM unavailable/error)" });
    Ok(())
}

/// The `key` (password) from SimpleAction.args JSON.
pub fn password_from_args(args: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(args).ok()?;
    value.get("key")?.as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_state_is_preserved_and_repeated_states_do_not_notify() {
        let (tx, mut rx) = watch::channel(Some(true));
        publish_lock_state(&tx, None);
        assert_eq!(*rx.borrow_and_update(), None);
        publish_lock_state(&tx, None);
        assert!(!rx.has_changed().unwrap());
        publish_lock_state(&tx, Some(false));
        assert!(rx.has_changed().unwrap());
        assert_eq!(*rx.borrow_and_update(), Some(false));
    }

    #[test]
    fn hung_fallback_is_killed_and_reaped() {
        let start = std::time::Instant::now();
        assert_eq!(bounded_command(Path::new("/bin/sleep"), &["10"], Duration::from_millis(30)), None);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn password_comes_from_the_official_key_field() {
        assert_eq!(password_from_args(br#"{"key":"p@ss word"}"#).as_deref(), Some("p@ss word"));
        assert_eq!(password_from_args(br#"{"password":"x"}"#), None);
        assert_eq!(password_from_args(b"not json"), None);
    }
}
