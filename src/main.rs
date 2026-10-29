//! UU Remote Pure Native Linux Server (Rust Edition)
//! Zero-Wine, Zero-Python, Single-Binary, 5 Native Tools Integrated

mod ws_transport;
mod engine_plugin;
mod doctor;
mod wol;
mod desktop_backend;
mod desktop_mode;
mod desktop_session;
mod desktop_power;
mod portal;
mod network_diagnostics;
mod session_ice;
mod wayland;
mod auth;
mod assist;
mod config;
mod i18n;
mod login;
mod login_ui;
mod session_lock;
mod capture;
mod kms;
mod file_trans;
mod input;
mod uinput;
mod input_mapping;
mod port_mapping;
mod power;
mod signaling;
mod terminal;
mod webrtc;
mod video;
mod video_select;
mod video_track;
mod locked_video;
#[cfg(test)]
mod test_network;
mod hevc;
#[cfg(feature = "inproc-av")]
mod damage_capture;
#[cfg(feature = "inproc-av")]
mod inproc_encoder;
mod capture_setting;
mod audio;
mod virtual_audio;
mod system_actions;
mod capability;
mod protocol;
mod control_protocol;
mod rtp_metadata;
mod initialization;
mod system_metrics;
mod clipboard;
mod clipboard_protocol;
mod clipboard_rich;
mod viewer_metrics;
mod cursor;

use auth::{Credentials, UUAuthClient};
mod clipboard_probe;
use capture::ScreenCapture;
use input::InputInjector;
use signaling::{SignalingClient, SignalingEvent, SoacMessage};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use webrtc::WebRtcSession;

/// Set before a normal return when the host must come back in the other mode (signed in / out).
static RESTART: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

struct AbortOnDrop(tokio::task::AbortHandle);
impl Drop for AbortOnDrop { fn drop(&mut self) { self.0.abort(); } }
fn superseded_peer_ids(sessions: &HashMap<String, WebRtcSession>, ice: &str, desktop: bool) -> Vec<String> {
    sessions.iter().filter(|(id, peer)| id.as_str() == ice || (desktop && peer.desktop))
        .map(|(id, _)| id.clone()).collect()
}

/// A peer whose transport is already broken can leave `PeerConnection::close`
/// waiting indefinitely; signaling must keep flowing without it.
async fn close_peer(pc: &::webrtc::peer_connection::RTCPeerConnection) {
    if tokio::time::timeout(std::time::Duration::from_secs(5), pc.close()).await.is_err() {
        log::warn!("Peer connection close timed out; continuing");
    }
}
fn released_peer_ids<T>(sessions: &HashMap<String, T>, ice: Option<&serde_json::Value>) -> Vec<String> {
    let ice = ice.map(|id| id.as_str().map(str::to_owned).unwrap_or_else(|| id.to_string()));
    sessions.keys().filter(|id| ice.as_deref().map_or(true, |ice| id.as_str() == ice)).cloned().collect()
}

#[cfg(test)]
mod peer_release_tests {
    use super::*;
    #[test]
    fn explicit_releases_leave_independent_and_replacement_peers_alive() {
        let mut peers = HashMap::from([
            ("old-desktop".to_owned(), ()), ("files".to_owned(), ()),
            ("terminal".to_owned(), ()), ("port-mapping".to_owned(), ()),
        ]);
        let targets = released_peer_ids(&peers, Some(&serde_json::json!("old-desktop")));
        assert_eq!(targets, ["old-desktop"]);
        for id in targets { peers.remove(&id); }
        peers.insert("new-desktop".to_owned(), ());
        assert!(released_peer_ids(&peers, Some(&serde_json::json!("old-desktop"))).is_empty());
        assert!(released_peer_ids(&peers, Some(&serde_json::json!("unknown"))).is_empty());
        for id in ["files", "terminal", "port-mapping", "new-desktop"] {
            assert_eq!(released_peer_ids(&peers, Some(&serde_json::json!(id))), [id]);
        }
        assert_eq!(peers.len(), 4);
    }
    #[test]
    fn release_without_an_ice_id_retains_room_wide_cleanup() {
        let peers = HashMap::from([("desktop".to_owned(), ()), ("files".to_owned(), ())]);
        let mut targets = released_peer_ids(&peers, None);
        targets.sort();
        assert_eq!(targets, ["desktop", "files"]);
    }
    #[test]
    fn explicit_non_string_ice_ids_never_become_room_wide_releases() {
        let peers = HashMap::from([("123".to_owned(), ()), ("files".to_owned(), ())]);
        for id in [serde_json::json!(999), serde_json::Value::Null, serde_json::json!(false), serde_json::json!({})] {
            assert!(released_peer_ids(&peers, Some(&id)).is_empty());
        }
        assert_eq!(released_peer_ids(&peers, Some(&serde_json::json!(123))), ["123"]);
    }
}
/// Re-executes this host; the new instance opens its window so the user sees the result.
fn restart_host() -> ! {
    use std::os::unix::process::CommandExt;
    let error = std::process::Command::new("/proc/self/exe").args(std::env::args_os().skip(1)).env("UU_SHOW_WINDOW", "1").exec();
    eprintln!("restart failed: {error}");
    std::process::exit(1);
}
/// systemd user unit running this process (from /proc/self/cgroup), e.g. "uuremote-native.service".
fn systemd_unit() -> Option<String> {
    std::env::var_os("INVOCATION_ID")?;
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    cgroup.lines().find_map(|l| l.rsplit('/').next().filter(|u| u.ends_with(".service")).map(str::to_owned))
}
fn display_present() -> bool { std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some() }

fn set_video_pref() -> Result<(), Box<dyn std::error::Error>> {
    let which = std::env::args().nth(1).unwrap_or_default();
    let stored = std::env::args().nth(2).unwrap_or_default().trim().to_ascii_lowercase();
    let stored = if stored == "hevc" { "h265".to_owned() } else { stored };
    if let Err(e) = system_actions::merge_save(|settings| {
        if which == "set-encoder" { settings.video_encoder = Some(stored.clone()); }
        else { settings.video_codec = Some(stored.clone()); }
    }) {
        // Exit 2 is reserved for bad CLI input; a settings file that cannot be merged is exit 1.
        eprintln!("{which}: {e}");
        std::process::exit(1);
    }
    let selection = video_select::selection();
    let codec = match selection.codec_pref { video_select::CodecPref::Auto => "auto", video_select::CodecPref::H264 => "h264", video_select::CodecPref::H265 => "h265" };
    println!("encoder={:?} source={} codec_pref={codec} source={}", selection.encoder, video_select::encoder_source_label(selection.encoder_source), video_select::source_label(selection.codec_source));
    // Both subcommands wake the host. Encoder changes restart live desktop streams; codec changes are logged only.
    match notify_host(libc::SIGUSR2) {
        Ok(()) => println!("notified the running host"),
        Err(_) => println!(
            "host not running, or the running host is not this binary: takes effect the next time a desktop connection starts, or after restarting the service"
        ),
    }
    if which == "set-codec" {
        println!("takes effect the next time a desktop connection starts");
    }
    Ok(())
}

/// Signal the host recorded in host.pid only when `/proc/<pid>/exe` is exactly
/// `canonicalize(current_exe())`. A filename match is not enough: an older host
/// has no SIGUSR2 handler, and the default action terminates the process.
/// A replaced binary reads back as `… (deleted)` and does not match.
fn notify_host(signal: i32) -> Result<(), ()> {
    let pid = std::fs::read_to_string(host_pid_path())
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .filter(|p| *p > 1)
        .ok_or(())?;
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).map_err(|_| ())?;
    let own = std::fs::canonicalize(std::env::current_exe().map_err(|_| ())?).map_err(|_| ())?;
    if !same_host_binary(&exe, &own) {
        return Err(());
    }
    if unsafe { libc::kill(pid, signal) } == 0 {
        Ok(())
    } else {
        Err(())
    }
}

/// `/proc/<pid>/exe` must equal the canonical path of this binary. A trailing ` (deleted)` does not match.
fn same_host_binary(proc_exe: &std::path::Path, own: &std::path::Path) -> bool {
    proc_exe == own
}

/// Explicit command words for set-encoder / set-codec. Empty and whitespace are illegal.
pub(crate) fn cli_pref_ok(which: &str, value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() { return false; }
    let value = value.to_ascii_lowercase();
    match which {
        "set-encoder" => matches!(value.as_str(), "auto" | "cpu" | "vaapi" | "nvenc"),
        "set-codec" => matches!(value.as_str(), "auto" | "h264" | "h265" | "hevc"),
        _ => false,
    }
}

const SUBCOMMANDS: [&str; 17] = ["doctor", "show", "assist", "set-encoder", "set-codec", "desktop-input-probe", "virtual-audio-probe", "login", "unlock-check", "desktop-probe", "desktop-video-probe", "desktop-audio-probe", "wayland-capture",
    "wayland-input-probe", "wayland-text-probe", "clipboard-acceptance", "portal-acceptance"];

/// Unknown arguments and illegal set-encoder / set-codec values exit 2 (never start a second host).
/// help / -h / --help exit 0 so main prints usage instead of starting the host.
pub(crate) fn reject_cli(arg: Option<&str>, argc: usize, value_ok: bool) -> Option<i32> {
    if matches!(arg, Some("help" | "-h" | "--help")) { return Some(0); }
    if arg.is_some_and(|a| !SUBCOMMANDS.contains(&a)) { return Some(2); }
    if matches!(arg, Some("set-encoder" | "set-codec")) && (argc != 3 || !value_ok) { return Some(2); }
    None
}

#[cfg(test)]
mod cli_exit_tests {
    #[test]
    fn unknown_and_illegal_subcommand_exit_2() {
        assert_eq!(super::reject_cli(Some("not-a-command"), 2, true), Some(2));
        assert_eq!(super::reject_cli(Some("set-encoder"), 3, false), Some(2));
        assert_eq!(super::reject_cli(Some("set-encoder"), 2, true), Some(2));
        assert_eq!(super::reject_cli(Some("set-codec"), 4, true), Some(2));
        assert_eq!(super::reject_cli(Some("set-encoder"), 3, true), None);
        assert_eq!(super::reject_cli(Some("set-codec"), 3, true), None);
        assert_eq!(super::reject_cli(Some("help"), 2, true), Some(0));
        assert_eq!(super::reject_cli(Some("-h"), 2, true), Some(0));
        assert_eq!(super::reject_cli(Some("--help"), 2, true), Some(0));
        assert_eq!(super::reject_cli(None, 1, true), None);
        assert!(!super::cli_pref_ok("set-encoder", ""));
        assert!(!super::cli_pref_ok("set-encoder", "   "));
        assert!(!super::cli_pref_ok("set-codec", ""));
        assert!(!super::cli_pref_ok("set-codec", "\t"));
        assert!(!super::cli_pref_ok("set-encoder", "bogus"));
        assert!(super::cli_pref_ok("set-encoder", " NVENC "));
        assert!(super::cli_pref_ok("set-codec", "hevc"));
        assert_eq!(super::reject_cli(Some("set-encoder"), 3, super::cli_pref_ok("set-encoder", "")), Some(2));
        assert_eq!(super::reject_cli(Some("set-codec"), 3, super::cli_pref_ok("set-codec", "  ")), Some(2));
    }

    /// SIGUSR1 and SIGUSR2 are installed with no assist UI. `host.pid` is written only after both exist.
    #[test]
    fn signal_handlers_install_without_assist() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let _guard = crate::config::test_env_lock().lock().unwrap();
            let dir =
                std::env::temp_dir().join(format!("uuremote-signal-test-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::env::set_var("UU_CONFIG", dir.join("config.json"));
            assert!(super::install_host_signals(None));
            let pid = std::fs::read_to_string(super::host_pid_path()).unwrap();
            assert_eq!(pid.trim(), std::process::id().to_string());
            std::env::remove_var("UU_CONFIG");
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn sigusr2_requires_the_same_canonical_binary() {
        let own = std::path::Path::new("/home/user/.local/bin/uuremote-server");
        assert!(super::same_host_binary(own, own));
        assert!(!super::same_host_binary(
            std::path::Path::new("/tmp/unpack/uuremote-server"),
            own
        ));
        assert!(!super::same_host_binary(
            std::path::Path::new("/home/user/.local/bin/uuremote-server (deleted)"),
            own
        ));
    }
}
/// SIGUSR1 (`uuremote-server show`) opens the host window.
/// SIGUSR2 (set-encoder, set-codec, or `kill -USR2`) re-reads host_settings. Encoder changes switch live desktop streams; codec changes are logged for the next connection.
/// The default action of either signal terminates the process, so both handlers are installed before `host.pid` is written.
/// SIGUSR2 is independent of remote assistance: `UU_REMOTE_ASSIST=0` and a failed assist setup still switch live encoders.
/// SIGUSR1 is installed first as a no-op so `show` cannot terminate a host that has no assist window yet.
fn install_host_signals(ui: Option<std::sync::Arc<assist::LocalUi>>) -> bool {
    static USR1: std::sync::Once = std::sync::Once::new();
    static USR2: std::sync::Once = std::sync::Once::new();
    let mut usr1_ok = true;
    USR1.call_once(|| {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1()) {
            Ok(mut usr1) => { tokio::spawn(async move { while usr1.recv().await.is_some() {} }); }
            Err(_) => usr1_ok = false,
        }
    });
    if !usr1_ok { return false; }
    let mut usr2_ok = true;
    USR2.call_once(|| {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined2()) {
            Ok(mut usr2) => { tokio::spawn(async move { while usr2.recv().await.is_some() { crate::video_select::publish_reload(); } }); }
            Err(_) => usr2_ok = false,
        }
    });
    if !usr2_ok { return false; }
    // Assist attaches later and replaces the no-op with the window. A second registration replaces the first.
    if let Some(ui) = ui {
        if let Ok(mut usr1) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1()) {
            tokio::spawn(async move { while usr1.recv().await.is_some() { ui.present(); } });
        }
    }
    write_host_pid();
    true
}

fn present_on_usr1(ui: std::sync::Arc<assist::LocalUi>) {
    let _ = install_host_signals(Some(ui));
}

fn host_pid_path() -> std::path::PathBuf { config::path().with_file_name("host.pid") }
/// Lets `uuremote-server show` find this host service to open its window (SIGUSR1).
fn write_host_pid() {
    use std::os::unix::fs::OpenOptionsExt;
    use std::io::Write;
    let path = host_pid_path();
    if let Some(dir) = path.parent() { let _ = std::fs::create_dir_all(dir); }
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path) {
        let _ = write!(f, "{}", std::process::id());
    }
}
/// Why `unit` is not (going to be) running, from `systemctl --user show`; None while it is active or starting.
fn unit_not_running(unit: &str, settled: bool) -> Option<String> {
    let output = std::process::Command::new("systemctl").args(["--user", "show", unit, "-p", "ActiveState,SubState,ConditionResult"]).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let value = |key: &str| text.lines().find_map(|l| l.strip_prefix(key).and_then(|v| v.strip_prefix('='))).unwrap_or("").to_owned();
    unit_state_problem(unit, &value("ActiveState"), &value("SubState"), &value("ConditionResult"), settled)
}
fn unit_state_problem(unit: &str, active: &str, sub: &str, condition: &str, settled: bool) -> Option<String> {
    let hint = format!("see `systemctl --user status {unit}` and `journalctl --user -u {unit}`");
    if condition == "no" { return Some(format!("{unit} was skipped: a Condition in the unit is not met (e.g. ConditionEnvironment outside the graphical session); {hint}")); }
    if active == "failed" { return Some(format!("{unit} failed to start; {hint}")); }
    if sub == "auto-restart" { return Some(format!("{unit} keeps exiting and being restarted; {hint}")); }
    if settled && active == "inactive" { return Some(format!("{unit} is not running; {hint}")); }
    None
}
#[cfg(test)]
mod show_unit_tests {
    #[test]
    fn skipped_crashing_and_failed_units_are_reported() {
        let p = |a, s, c, settled| super::unit_state_problem("u.service", a, s, c, settled);
        assert!(p("inactive", "dead", "no", false).unwrap().contains("skipped"));
        assert!(p("activating", "auto-restart", "yes", false).unwrap().contains("restarted"));
        assert!(p("failed", "failed", "yes", false).unwrap().contains("failed"));
        assert!(p("inactive", "dead", "yes", false).is_none());
        assert!(p("inactive", "dead", "yes", true).unwrap().contains("not running"));
        assert!(p("active", "running", "yes", true).is_none());
        assert!(p("activating", "start", "yes", true).is_none());
    }
}
/// `uuremote-server show` (desktop launcher): open the running host's window, or start the host in the
/// foreground with its window when none runs (e.g. no systemd unit installed yet).
fn show_running_host() -> Result<(), Box<dyn std::error::Error>> {
    let running = std::fs::read_to_string(host_pid_path()).ok().and_then(|s| s.trim().parse::<i32>().ok()).filter(|p| *p > 1)
        .filter(|pid| {
            let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok();
            let own = std::env::current_exe().ok();
            exe.is_some() && exe.as_ref().and_then(|p| p.file_name()) == own.as_ref().and_then(|p| p.file_name())
        });
    if let Some(pid) = running {
        if unsafe { libc::kill(pid, libc::SIGUSR1) } == 0 { return Ok(()); }
    }
    // Stopped from the tray but installed as a user service: start the service (not a second,
    // unmanaged host), wait for it to publish its pid, then ask it to show the window.
    let unit = std::env::var("UU_SERVICE_UNIT").unwrap_or_else(|_| "uuremote-native.service".into());
    let installed = std::process::Command::new("systemctl").args(["--user", "cat", &unit])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success());
    if installed && std::process::Command::new("systemctl").args(["--user", "start", &unit]).status().is_ok_and(|s| s.success()) {
        let before = std::fs::metadata(host_pid_path()).and_then(|m| m.modified()).ok();
        for attempt in 0..60 {
            // `systemctl start` also returns 0 when the unit is skipped by a Condition or keeps
            // crashing under Restart=; report that instead of waiting 30 s for a pid (#62).
            if let Some(reason) = unit_not_running(&unit, attempt >= 2) { return Err(reason.into()); }
            std::thread::sleep(std::time::Duration::from_millis(500));
            let fresh = std::fs::metadata(host_pid_path()).and_then(|m| m.modified()).ok().is_some_and(|t| Some(t) != before);
            let pid = std::fs::read_to_string(host_pid_path()).ok().and_then(|s| s.trim().parse::<i32>().ok());
            if let (true, Some(pid)) = (fresh, pid) {
                std::thread::sleep(std::time::Duration::from_millis(500)); // signal handler installed right after the pid
                if unsafe { libc::kill(pid, libc::SIGUSR1) } == 0 { return Ok(()); }
            }
        }
        return Err(format!("{unit} is running but did not open its window within 30 s; check `journalctl --user -u {unit}`").into());
    }
    use std::os::unix::process::CommandExt;
    Err(Box::new(std::process::Command::new("/proc/self/exe").env("UU_SHOW_WINDOW", "1").exec()))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Not signed in, with a desktop: one window offers remote assistance (guest identity) and the QR
    // login together; signing in restarts the host in account mode (assistance continues there).
    let unified = std::env::args().nth(1).is_none() && display_present()
        && std::env::var("UU_REMOTE_ASSIST").ok().as_deref() != Some("0") && std::env::var("UU_LOGIN_WINDOW").ok().as_deref() != Some("0")
        && config::read_fresh().ok().flatten().and_then(|c| c.get("token").and_then(|t| t.as_str()).map(|t| !t.is_empty())) != Some(true)
        && ["UU_USER_ID", "UU_TOKEN", "UU_CSRF"].iter().all(|k| std::env::var_os(k).map_or(true, |v| v.is_empty()));
    let assist_enabled = unified || std::env::args().nth(1).as_deref() == Some("assist")
        || (std::env::args().nth(1).is_none() && std::env::var("UU_REMOTE_ASSIST").ok().as_deref() == Some("1"));
    if assist_enabled {
        // HTTP/WS dependency trace logging can expose Authorization or room headers.
        env_logger::Builder::new().filter_level(log::LevelFilter::Off)
            .filter_module("uuremote_server", log::LevelFilter::Info).init();
    } else { env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init(); }
    // An unknown argument must never fall through to starting a second full host server.
    let arg1 = std::env::args().nth(1);
    let value_ok = match arg1.as_deref() {
        Some(which @ ("set-encoder" | "set-codec")) => cli_pref_ok(which, &std::env::args().nth(2).unwrap_or_default()),
        _ => true,
    };
    if let Some(code) = reject_cli(arg1.as_deref(), std::env::args().len(), value_ok) {
        let usage = format!("usage: uuremote-server [{}]\n  (no argument: run the host)", SUBCOMMANDS.join(" | "));
        if code == 0 { println!("{usage}"); return Ok(()); }
        eprintln!("unknown argument\n{usage}");
        std::process::exit(code);
    }

    if std::env::args().nth(1).as_deref() == Some("show") { return show_running_host(); }
    if matches!(std::env::args().nth(1).as_deref(), Some("set-encoder") | Some("set-codec")) { return set_video_pref(); }
    if assist_enabled && !unified {
        if std::env::args().len() > 2 { return Err("assist accepts no additional arguments; configure it with a private UU_CONFIG".into()); }
        assist::configure()?;
    }
    if std::env::args().nth(1).as_deref() == Some("clipboard-acceptance") {
        return tokio::task::spawn_blocking(clipboard_probe::run).await?.map_err(Into::into);
    }
    if std::env::args().nth(1).as_deref() == Some("portal-acceptance") {
        if std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim()!="uu-hyprland-test" {
            return Err("Portal input acceptance is restricted to the isolated test VM".into());
        }
        let screen=ScreenCapture::new()?;
        if screen.portal.is_none() { return Err("Portal acceptance requires the Portal backend".into()); }
        // Give the consent dialog time to release focus before fixed lab events.
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let input=InputInjector::new();
        input.bind_portal(screen.portal.clone());
        if std::env::args().nth(2).as_deref() == Some("--clipboard-targets") {
            let read = || -> Result<serde_json::Value, Box<dyn std::error::Error>> {
                Ok(serde_json::from_slice(&std::fs::read("/tmp/uuremote-clipboard-official.json")?)?)
            };
            let before = read()?;
            if before["native_wayland"] != true || before["window_active"] != true { return Err("Native clipboard fixture must be focused".into()); }
            for name in ["check", "copy"] {
                let bounds = before[format!("{name}_bounds")].as_array().ok_or("Missing test button bounds")?;
                let number = |index: usize| bounds[index].as_f64().ok_or("Invalid button bounds");
                let x = number(0)? + number(2)? / 2.0;
                let y = number(1)? + number(3)? / 2.0;
                let width = before["window_size"][0].as_f64().ok_or("Missing test window width")?;
                let height = before["window_size"][1].as_f64().ok_or("Missing test window height")?;
                input.mouse_absolute_normalized(x / width, y / height, (screen.width,screen.height));
                input.mouse_button(1,true); input.mouse_button(1,false);
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let after = read()?;
                let key = format!("{name}_activations");
                if after[&key].as_u64() != before[&key].as_u64().map(|n| n+1) {
                    return Err(format!("Portal small-target click missed: {name}").into());
                }
                let position = after["last_position"].as_array().ok_or("Missing native test pointer position")?;
                if (position[0].as_f64().ok_or("Invalid pointer x")? - x).abs() > 2.0 || (position[1].as_f64().ok_or("Invalid pointer y")? - y).abs() > 2.0 {
                    return Err("Portal small-target pointer error exceeds two logical pixels".into());
                }
                println!("Portal native small-target click passed: {name}; pointer_error<=2px");
            }
            return Ok(());
        }
        input.mouse_absolute_normalized(0.25,0.25,(screen.width,screen.height));input.flush_pointer();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        input.mouse_absolute_normalized(0.5,0.5,(screen.width,screen.height));input.flush_pointer();
        input.mouse_button(1,true);input.mouse_button(1,false);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        input.mouse_relative(32,0);input.flush_pointer();
        for key in b"UUWAYLAND" { input.windows_key_event(i32::from(*key),true);input.windows_key_event(i32::from(*key),false); }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        input.release_held();
        let report:serde_json::Value=serde_json::from_slice(&std::fs::read("/tmp/uuremote-input-acceptance.json")?)?;
        for key in ["native_wayland","window_active","motion","click","keyboard","text_input"] {
            if report.get(key).and_then(serde_json::Value::as_bool)!=Some(true) { return Err(format!("Portal native GTK acceptance failed: {key}").into()); }
        }
        println!("Portal native GTK input acceptance passed: motion/click/keyboard/text");
        if std::env::args().nth(2).as_deref() == Some("--lock-recovery") {
            let session = screen.portal.as_ref().unwrap();
            let before = report["key_presses"].as_u64().ok_or("Missing GTK key count")?;
            let run_lock = |action: &str| -> Result<(), String> {
                if std::process::Command::new("timeout").args(["5s", "loginctl", "--no-ask-password", action]).arg(
                    String::from_utf8(std::process::Command::new("loginctl").args(["show-user", &unsafe { libc::getuid() }.to_string(), "-p", "Display", "--value"]).output().map_err(|_| "Display session unavailable")?.stdout).map_err(|_| "Invalid display session")?.trim()
                ).status().map_err(|_| "loginctl unavailable")?.success() {
                    Ok(())
                } else { Err("Test VM lock command failed".into()) }
            };
            let locked = session_lock::watch_locked();
            run_lock("lock-session")?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            while *locked.borrow() != Some(true) && std::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            if *locked.borrow() != Some(true) { return Err("Test VM did not lock".into()); }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let closed = !session.is_alive();
            run_lock("unlock-session")?;
            let mut frame = vec![0; screen.width as usize * screen.height as usize * 4];
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                if *locked.borrow() == Some(false) {
                    session.recover(&std::sync::atomic::AtomicBool::new(false)).map_err(|error| error.to_string())?;
                }
                session.capture(&mut frame)?;
                if *locked.borrow() == Some(false) && session.is_alive() { break; }
                if std::time::Instant::now() >= deadline { return Err("Test VM portal did not recover".into()); }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            // Keep the original InputInjector and Arc: no rebind after recovery.
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            input.mouse_absolute_normalized(0.5,0.5,(screen.width,screen.height));
            input.mouse_button(1,true); input.mouse_button(1,false);
            for key in b"UUWAYLAND" { input.windows_key_event(i32::from(*key),true); input.windows_key_event(i32::from(*key),false); }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            input.release_held();
            let after:serde_json::Value=serde_json::from_slice(&std::fs::read("/tmp/uuremote-input-acceptance.json")?)?;
            if after["key_presses"].as_u64() != Some(before + 9) || after["key_releases"].as_u64() != Some(before + 9) {
                return Err("Original portal input binding failed after lock recovery".into());
            }
            println!("Portal same-Arc lock recovery input passed: session_closed={} balanced_new_keys=9", closed);
            return Ok(());
        }
        let path=std::env::args().nth(2);
        return video::probe((screen.width,screen.height),screen.display.clone(),screen.portal.clone(),path).await.map_err(|e|e.into());
    }
    if std::env::args().nth(1).as_deref() == Some("desktop-input-probe") {
        if std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim()!="uu-hyprland-test" {
            return Err("Input acceptance is restricted to the isolated VM".into());
        }
        let screen=ScreenCapture::new()?;
        let input=Arc::new(InputInjector::new());
        input.bind_portal(screen.portal.clone());
        let worker=input.start_pointer_worker();
        // libinput discovers newly created uinput devices asynchronously.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        input.mouse_absolute_normalized(0.25,0.25,(screen.width,screen.height));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        input.mouse_absolute_normalized(0.5,0.5,(screen.width,screen.height));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        input.mouse_button(1,true); input.mouse_button(1,false);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let measure=std::env::var("UU_INPUT_MEASURE").is_ok_and(|v|v=="1");
        let cursor=|| -> Result<serde_json::Value,Box<dyn std::error::Error>> {
            let output=std::process::Command::new("hyprctl").args(["-j","cursorpos"]).output()?;
            if !output.status.success() { return Err("Cursor measurement unavailable".into()); }
            Ok(serde_json::from_slice(&output.stdout)?)
        };
        let before=if measure {Some(cursor()?)} else {None};
        input.mouse_relative(32,0);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        if let Some(before)=before {
            println!("Relative pointer measurement: {}",serde_json::json!({"before":before,"after":cursor()?,"requested_delta":[32,0]}));
        }
        for key in b"UUWAYLAND" { input.windows_key_event(i32::from(*key),true); input.windows_key_event(i32::from(*key),false); }
        if std::env::var("UU_INPUT_SYMBOLS").is_ok_and(|v|v=="1") {
            // Fixed synthetic symbols exercise the same VK+modifier path as CONTROL.
            for (key,shift) in [(0xbd,true),(0xbb,true),(0xbd,false)] {
                if shift {input.windows_key_event(0x10,true);}
                input.windows_key_event(key,true); input.windows_key_event(key,false);
                if shift {input.windows_key_event(0x10,false);}
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        worker.abort(); input.release_held();
        println!("Desktop test events sent; verify the GTK acceptance report");
        return Ok(());
    }
    if std::env::args().nth(1).as_deref() == Some("wayland-input-probe") {
        return wayland::input_probe().map_err(|e|e.into());
    }
    if std::env::args().nth(1).as_deref() == Some("wayland-text-probe") {
        return wayland::text_probe().map_err(|e|e.into());
    }
    if std::env::args().nth(1).as_deref() == Some("doctor") {
        return doctor::run().await.map_err(|e| e.into());
    }
    if std::env::args().nth(1).as_deref() == Some("wayland-capture") {
        return wayland::capture_stdout().map_err(|e| e.into());
    }
    if std::env::args().nth(1).as_deref() == Some("desktop-probe") {
        let screen=ScreenCapture::new()?;
        capture::keep_display_awake()?;
        let frame=screen.capture_frame()?;
        if let Some(path)=std::env::args().nth(2) {
            use std::io::Write;
            let mut rgba=frame.clone();
            for pixel in rgba.chunks_exact_mut(4) { pixel.swap(0,2); pixel[3]=255; }
            let png=cursor::encode_png(screen.width,screen.height,&rgba);
            let mut file=std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
            file.write_all(&png)?;
        }
        println!("backend={:?} width={} height={} frame_bytes={}",screen.backend,screen.width,screen.height,frame.len());
        return Ok(());
    }
    if std::env::args().nth(1).as_deref() == Some("desktop-video-probe") {
        if let Some(input)=std::env::var_os("UU_PROBE_INPUT") {
            return video::probe_input(input,std::env::args().nth(2)).await.map_err(|e|e.into());
        }
        let screen=ScreenCapture::new()?;
        capture::keep_display_awake()?;
        return video::probe((screen.width,screen.height),screen.display.clone(),screen.portal.clone(),std::env::args().nth(2)).await.map_err(|e|e.into());
    }
    if std::env::args().nth(1).as_deref() == Some("desktop-audio-probe") {
        return audio::probe(std::env::args().nth(2).and_then(|v| v.parse().ok()).unwrap_or(5)).await.map_err(|e| e.into());
    }
    if std::env::args().nth(1).as_deref() == Some("unlock-check") {
        return session_lock::check_from_stdin().map_err(|e| e.into());
    }
    if std::env::args().nth(1).as_deref() == Some("virtual-audio-probe") {
        virtual_audio::init();
        if !virtual_audio::enable() { return Err("Virtual audio probe setup failed".into()); }
        println!("Virtual audio probe active");
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        virtual_audio::disable();
        if virtual_audio::active() { return Err("Virtual audio probe restore incomplete".into()); }
        println!("Virtual audio probe ended");
        return Ok(());
    }
    if std::env::args().nth(1).as_deref() == Some("login") {
        // Interactive: register this host and bind it to an account via QR scan
        // (GTK window when available; `login --no-gui` keeps it terminal-only).
        let gui = !std::env::args().skip(2).any(|a| a == "--no-gui");
        if login::run(gui).await.is_err() { std::process::exit(1); } // error already shown
        return Ok(());
    }
    virtual_audio::init();
    let _audio_cleanup=virtual_audio::Cleanup;
    log::info!("Starting uu-remote-linux host v{}...", env!("CARGO_PKG_VERSION"));
    session_ice::configure_relay_transport();
    // Before host.pid, and before assist: a missing or failed assist session must still handle SIGUSR2.
    let _ = install_host_signals(None);
    video::warm_auto_probe();
    video::check_ffmpeg_usable();

    if !desktop_backend::headless() && desktop_backend::detect()? == desktop_backend::DesktopBackend::X11 && desktop_backend::wayland_session_running() {
        // X11 capture inside a Wayland session reads XWayland's root window: all black, no error (#61).
        log::warn!("A Wayland session is running but the X11 backend is selected: the picture will be black. Run inside the Wayland session (WAYLAND_DISPLAY set, UU_DESKTOP_BACKEND=auto) or log in to the Xorg session");
    }
    let capture = Arc::new((if desktop_backend::headless() {
        Err("headless mode (UU_DESKTOP_BACKEND=none)".to_owned())
    } else if desktop_backend::detect()?==desktop_backend::DesktopBackend::Portal {
        Err(if !cfg!(feature = "portal") { portal::NOT_BUILT.to_owned() }
            else if let Err(reason) = engine_plugin::load(engine_plugin::Engine::Portal) { format!("GNOME/KDE Wayland (portal) engine unavailable: {reason}") }
            else { "Portal capture waits for a desktop CONTROL connection".to_owned() })
    } else { ScreenCapture::new() }).unwrap_or_else(|error| {
        log::warn!("Desktop unavailable; terminal and file tools remain available: {error}");
        ScreenCapture::unavailable(error)
    }));
    let input = Arc::new(InputInjector::new());
    input.bind_portal(capture.portal.clone());
    input.release_held(); // recover stale input before accepting any peer
    let _pointer_worker = input.start_pointer_worker();
    let (shutdown_tx, mut shutdown) = tokio::sync::watch::channel(false);
    let shutdown_input = input.clone();
    let signal_tx = shutdown_tx.clone();
    tokio::spawn(async move {
        #[cfg(target_os="linux")]
        {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
            tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
        }
        #[cfg(not(target_os="linux"))]
        let _ = tokio::signal::ctrl_c().await;
        if !assist_enabled { shutdown_input.shutdown(); virtual_audio::disable(); terminal::shutdown_all(); std::process::exit(0); }
        let _ = signal_tx.send(true);
    });
    let mut assist = if assist_enabled { Some(assist::Assist::start(&mut shutdown, unified).await?) } else { None };
    if let Some(session) = assist.as_ref().filter(|_| unified) {
        present_on_usr1(session.ui());
        let login_done = shutdown_tx.clone();
        tokio::spawn(async move {
            let ui = login_ui::LoginUi::attached();
            loop {
                match login::flow(&ui).await {
                    Ok(_) => { RESTART.store(true, std::sync::atomic::Ordering::SeqCst); let _ = login_done.send(true); return; }
                    Err(error) => { log::warn!("QR login failed: {error}; retrying"); tokio::time::sleep(std::time::Duration::from_secs(5)).await; }
                }
            }
        });
    }
    let webrtc_sessions: Arc<Mutex<HashMap<String, WebRtcSession>>> = Arc::new(Mutex::new(HashMap::new()));
    let (mut commands, command_task) = match assist.as_ref() {
        Some(s) => { let (rx, task) = s.commands(shutdown_tx); (rx, Some(task)) },
        None => (tokio::sync::mpsc::channel(1).1, None),
    };
    let mut immediate_shutdown = shutdown.clone();
    let result = tokio::select! {
        result = serve(capture, input.clone(), &mut shutdown, &mut assist, &mut commands, webrtc_sessions.clone()) => result,
        _ = immediate_shutdown.changed() => Ok(()),
    };
    if let Some(task) = command_task { task.abort(); }
    // One exit path for commands, signals, transport failures and UI failures.
    if let Some(session) = assist.as_mut() { session.allowed = false; }
    for (_, old) in webrtc_sessions.lock().await.drain() { close_peer(&old.pc).await; }
    virtual_audio::disable(); input.shutdown(); terminal::shutdown_all();
    // Switching identity (signed in / out) restarts at once and the new identity republishes the same
    // device share: logout already withdrew it, and after a QR login the cloud moved it to the account
    // (a guest cancel then fails). Only a real stop withdraws here.
    let switching = RESTART.load(std::sync::atomic::Ordering::SeqCst);
    let revoke = match assist.as_mut() { Some(s) if !switching => s.cancel().await, _ => Ok(()) };
    if revoke.is_err() { log::error!("Cloud assistance revocation failed; host is closed locally"); }
    result?;
    if RESTART.load(std::sync::atomic::Ordering::SeqCst) { restart_host(); }
    revoke.map_err(|_| "assist disabled locally, but cloud cancel failed; cloud revocation is unverified")?;
    Ok(())
}

async fn serve(mut capture: Arc<ScreenCapture>, input: Arc<InputInjector>, shutdown: &mut tokio::sync::watch::Receiver<bool>,
    assist: &mut Option<assist::Assist>, commands: &mut mpsc::Receiver<assist::Command>, webrtc_sessions: Arc<Mutex<HashMap<String, WebRtcSession>>>) -> Result<(), Box<dyn std::error::Error>> {
    let portal_desktop = (capture.backend == desktop_backend::DesktopBackend::Portal && !desktop_backend::headless())
        .then(|| Arc::new(desktop_session::PortalDesktop::new(capture.clone())));
    // Stop sharing the screen once no desktop controller is left. Viewers send stop_capture
    // before leaving and phones reconnect within seconds, so require two idle checks in a row.
    let idle_watch = portal_desktop.clone().map(|portal| {
        let sessions = webrtc_sessions.clone();
        tokio::spawn(async move {
            let mut idle = 0u8;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                if sessions.lock().await.values().any(|s| s.role.is_desktop()) { idle = 0; continue; }
                idle = idle.saturating_add(1);
                if idle >= 2 { portal.end_idle(); }
            }
        })
    });
    let _idle_watch = idle_watch.map(|task| AbortOnDrop(task.abort_handle()));
    let mut auth = if let Some(session) = assist.as_ref() { session.client.clone() } else {
        let creds = match config::file().and_then(Credentials::load) {
            Ok(creds) => creds,
            Err(e) => {
                log::warn!("Not logged in: {e} ({} or UU_* environment variables)", config::path().display());
                login::wait_for_credentials(None).await
            }
        };
        let client = Arc::new(UUAuthClient::new(creds.clone()));
        // Logged-in host: remote assistance (ID + code) alongside the device list, like the official
        // client. Shown in a hidden window opened with `uuremote-server show`; UU_REMOTE_ASSIST=0 disables.
        if std::env::var("UU_REMOTE_ASSIST").ok().as_deref() != Some("0") {
            match assist::Assist::start_account(client.clone(), creds) {
                Ok(session) => {
                    let (rx, _task) = session.commands(tokio::sync::watch::channel(false).0);
                    *commands = rx;
                    session.load_account_name().await;
                    present_on_usr1(session.ui());
                    if std::env::var("UU_SHOW_WINDOW").ok().as_deref() == Some("1") { session.ui().present(); }
                    std::env::remove_var("UU_SHOW_WINDOW");
                    log::info!("Remote assistance ready on the logged-in host (window: uuremote-server show)");
                    *assist = Some(session);
                }
                Err(error) => log::info!("Remote assistance unavailable on this host: {error}"),
            }
        }
        client
    };

    // One peer per viewer connection (desktop, file transfer, terminal...), keyed by ice_id.
    let mut next_capture_type: Option<(u64,crate::session_ice::SessionIce)> = None;
    let mut capture_types: HashMap<String, (u64,crate::session_ice::SessionIce)> = HashMap::new();
    let mut controller_by_ice: HashMap<String, crate::input::Controller> = HashMap::new();
    let mut next_controller = crate::input::Controller::Other;
    // Tool peers ask here to become the desktop (capture_desktop on an existing connection, #99).
    let (desktop_requests, mut desktop_requested) = mpsc::unbounded_channel::<String>();

    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        log::info!("Requesting fresh room credentials from NetEase cloud...");
        let room_result = tokio::select! {
            result = async {
                match assist.as_mut() {
                    Some(session) if !session.is_account() => {
                        let result = session.room().await.map_err(auth::RoomError::Other);
                        auth = session.client.clone(); result
                    }
                    _ => auth.create_room().await,
                }
            } => result,
            _ = shutdown.changed() => return Ok(()),
        };
        let room_data = match room_result {
            Ok(data) => {
                if let Some(session) = assist.as_mut().filter(|s| s.is_account()) {
                    if let Err(error) = session.ensure_published().await { log::warn!("Remote assistance share info failed: {error}"); }
                }
                data
            }
            Err(auth::RoomError::LoginExpired) => {
                // Like the official client: an expired token needs a new QR login, not retries.
                log::error!("UU login is invalid or expired; a new QR login is required");
                let stale = config::read_fresh().ok().flatten().and_then(|f| f.get("token").and_then(|t| t.as_str()).map(str::to_owned));
                auth = Arc::new(UUAuthClient::new(login::wait_for_credentials(stale).await));
                continue;
            }
            Err(e) => {
                log::error!("Failed to create room: {}. Retrying in 5s...", e);
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                continue;
            }
        };

        log::info!("Acquired room signaling credentials");

        let (tx_event, mut rx_event) = mpsc::channel::<SignalingEvent>(64);
        let tx_timer = tx_event.clone();
        let connected = tokio::select! {
            result = SignalingClient::connect(&room_data.signaling_server, &room_data.token, tx_event) => result,
            _ = shutdown.changed() => return Ok(()),
        };
        let signaling_client = match connected {
            Ok(cli) => {
                let renewal_peers=webrtc_sessions.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_secs(900)).await;
                    loop {
                        if tx_timer.is_closed() { return; }
                        let active=renewal_peers.lock().await.values().any(|session| {
                            use ::webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
                            session.pc.connection_state()==RTCPeerConnectionState::Connected
                        });
                        if !active {
                            log::info!("Idle room renewal timer elapsed; refreshing signaling room");
                            let _=tx_timer.send(SignalingEvent::Disconnected).await; return;
                        }
                        log::info!("Room renewal deferred while remote connection is active");
                        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                    }
                });
                Arc::new(cli)
            }
            Err(e) => {
                let _ = e;
                log::error!("Failed to connect signaling; retrying in 5s (details omitted)");
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                continue;
            }
        };

        // Guest display/control commands never use stdout, stderr or logs.
        let mut assist_tick = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            let event = tokio::select! {
                biased;
                _ = shutdown.changed() => Some(SignalingEvent::Stop),
                command = commands.recv(), if assist.is_some() => Some(SignalingEvent::AssistCommand(command.unwrap_or(assist::Command::Stop))),
                _ = assist_tick.tick(), if assist.is_some() => Some(SignalingEvent::AssistTick),
                Some(ice) = desktop_requested.recv() => Some(SignalingEvent::DesktopRequested(ice)),
                event = rx_event.recv() => event,
            };
            let Some(evt) = event else { break; };
            match evt {
                SignalingEvent::Stop | SignalingEvent::AssistCommand(assist::Command::Stop) => {
                    return Ok(());
                }
                SignalingEvent::AssistCommand(assist::Command::Allow) => { if let Some(s) = assist.as_mut() { s.allow()?; } }
                SignalingEvent::AssistCommand(assist::Command::Deny) => { if let Some(s) = assist.as_mut() { s.deny()?; } }
                SignalingEvent::AssistCommand(assist::Command::Verify(v)) => { if let Some(s) = assist.as_mut() { s.set_verify(v)?; } }
                SignalingEvent::AssistCommand(assist::Command::Refresh(r)) => { if let Some(s) = assist.as_mut() { s.set_refresh(r)?; } }
                SignalingEvent::AssistCommand(assist::Command::Custom(c)) => { if let Some(s) = assist.as_mut() { s.set_custom(c)?; } }
                SignalingEvent::AssistCommand(assist::Command::Quit) => {
                    // Tray 退出: under systemd (Restart=always) stop the unit itself, else just exit.
                    log::info!("Quit requested from the tray");
                    if let Some(unit) = systemd_unit() {
                        let _ = std::process::Command::new("systemctl").args(["--user", "stop", "--no-block", &unit]).status();
                    }
                    return Ok(());
                }
                SignalingEvent::AssistCommand(assist::Command::Logout) => {
                    if let Some(s) = assist.as_mut().filter(|s| s.is_account()) {
                        s.logout().await;
                        RESTART.store(true, std::sync::atomic::Ordering::SeqCst);
                        return Ok(());
                    }
                }
                SignalingEvent::AssistCommand(assist::Command::Reset) => { if let Some(s) = assist.as_mut() { s.reset()?; } }
                SignalingEvent::AssistTick => {
                    let mut sessions = webrtc_sessions.lock().await;
                    let ended: Vec<_> = sessions.iter().filter(|(_, s)| matches!(s.pc.connection_state(),
                        ::webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Closed |
                        ::webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Failed)).map(|(id, _)| id.clone()).collect();
                    let mut desktop_ended = false;
                    for id in ended { if let Some(s) = sessions.remove(&id) { desktop_ended |= s.role.is_desktop(); close_peer(&s.pc).await; } }
                    if desktop_ended && !sessions.values().any(|s| s.role.is_desktop()) { if let Some(s) = assist.as_mut() { s.session_ended()?; } }
                    // The controller left: stop sharing now; reconnecting reopens silently with the saved consent.
                    if !sessions.values().any(|s| s.role.is_desktop()) { if let Some(portal) = &portal_desktop { portal.end_idle(); } }
                    if let Some(s) = assist.as_mut() { s.tick()?; }
                }
                SignalingEvent::Connected => {
                    log::info!("Signaling handshake established.");
                    static WOL_REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                    if wol::enabled() && assist.as_ref().is_none_or(|s| s.is_account()) && !WOL_REPORTED.swap(true, std::sync::atomic::Ordering::AcqRel) {
                        let client = auth.clone();
                        tokio::spawn(async move { wol::report(&client).await; });
                    }
                }
                SignalingEvent::RoomInfoAck(_) => { log::info!("RoomInfo acknowledged by NetEase server"); }
                SignalingEvent::DeviceInfoChanged(val) => {
                    if assist.is_some() {
                        // Shape only (type value + key names), never field values: guest challenges carry salts/ids.
                        let keys = |v: Option<&serde_json::Value>| v.and_then(|v| v.as_object()).map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
                        log::debug!("Assist bmsg_push shape: type={:?} keys={:?} data_keys={:?}",
                            val.get("type").and_then(serde_json::Value::as_str), keys(Some(&val)), keys(val.get("data")));
                    }
                    if val.get("type").and_then(serde_json::Value::as_str) == Some("wake_on_lan") {
                        // Key names only: the target carries a MAC and LAN address.
                        let data = val.get("data").unwrap_or(&val);
                        log::info!("wake_on_lan push received: keys={:?} target_keys={:?} helper_enabled={}",
                            data.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default(),
                            data.get("target_device").and_then(|t| t.as_object()).map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default(),
                            wol::enabled());
                        if wol::enabled() {
                            match wol::handle_push(data) {
                                Ok(sent) => log::info!("WOL helper: magic packet sent ({sent} datagrams)"),
                                Err(error) => log::warn!("WOL helper: {error}"),
                            }
                        }
                        continue;
                    }
                    if val.get("type").and_then(serde_json::Value::as_str) == Some("get_control_mode") {
                        if let Some(s) = assist.as_mut() {
                            match s.control_mode(val.get("data").unwrap_or(&serde_json::Value::Null)).await {
                                Ok(()) => log::info!("Assist control mode reported"),
                                Err(error) => log::warn!("Assist control mode report failed: {error}"),
                            }
                        }
                        continue;
                    }
                    if val.get("type").and_then(serde_json::Value::as_str) == Some("remote_control") {
                        if let Some(s) = assist.as_mut() {
                            if let Err(error) = s.challenge(val.get("data").unwrap_or(&serde_json::Value::Null)).await {
                                log::warn!("Assist challenge failed: {error}");
                            }
                            if s.renewed { break; }
                        }
                        continue;
                    }
                    log::info!(
                        "Device status synchronized: status={:?}, availability={:?}",
                        val.get("data")
                            .and_then(|d| d.get("status"))
                            .and_then(|s| s.as_str()),
                        val.get("data")
                            .and_then(|d| d.get("publisher_availability_status"))
                            .and_then(|s| s.as_str())
                    );
                }
                SignalingEvent::ConnectRequest { capture_type, ice_id, transport, controller } => {
                    if controller_by_ice.len()>=32 { controller_by_ice.clear(); }
                    match ice_id.clone() { Some(id) => { controller_by_ice.insert(id, controller); } None => next_controller = controller }
                    // Pending cloud credentials live only within this room; cap
                    // requests that never receive an offer rather than retain them.
                    if capture_types.len()>=32 { capture_types.clear(); }
                    match ice_id { Some(id) => { capture_types.insert(id, (capture_type,transport)); } None => next_capture_type = Some((capture_type,transport)) }
                }
                SignalingEvent::BeControlled(val) => {
                    let _ = val;
                    log::info!("Incoming remote control session request; awaiting offer");
                }

                SignalingEvent::Soac(soac) => {
                    log::info!("SOAC signaling received: offer={} candidate={}", soac.msg_type == "offer", soac.msg_type == "candidate");
                    // Guest host: every peer is an assist peer. A logged-in host must keep serving its owner;
                    // assist peers there are already gated by the cloud (allow_control / code check).
                    if assist.as_ref().is_some_and(|s| !s.is_account() && !s.allowed) { continue; }
                    let ice = soac.ice_id.as_ref().map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())).unwrap_or_default();
                    let mut sessions = webrtc_sessions.lock().await;
                    // Drop peers that already ended.
                    let ended: Vec<String> = sessions.iter().filter(|(_, s)| matches!(s.pc.connection_state(),
                        ::webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Closed |
                        ::webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Failed)).map(|(k, _)| k.clone()).collect();
                    let mut desktop_ended = false;
                    for k in ended { if let Some(old) = sessions.remove(&k) { desktop_ended |= old.role.is_desktop(); close_peer(&old.pc).await; } }
                    if desktop_ended && !sessions.values().any(|s| s.role.is_desktop()) { if let Some(s) = assist.as_mut() { s.session_ended()?; } }
                    if let Some(s) = assist.as_mut() { s.tick()?; }

                    if soac.msg_type == "offer" {
                        let (capture_type,transport) = capture_types.remove(&ice).or_else(|| next_capture_type.take()).unwrap_or((1,crate::session_ice::SessionIce::default()));
                        // CaptureType: 0 unknown, 1 desktop, 2 window; 5 file transfer, 8 terminal, 9 port mapping.
                        let desktop = capture_type <= 2;
                        let controller = controller_by_ice.remove(&ice).unwrap_or(next_controller);
                        if desktop { input.set_controller(controller); }
                        log::info!("Offer for connection: capture_type={} desktop={} active_peers={}", capture_type, desktop, sessions.len());
                        // An expired NVENC failure is re-probed in the background. This offer keeps the cached encoder.
                        if desktop { video::warm_auto_probe(); video::check_ffmpeg_usable(); }
                        // Replace only this ICE id or a previous desktop, never tool peers.
                        for id in superseded_peer_ids(&sessions, &ice, desktop) {
                            if let Some(old) = sessions.remove(&id) { close_peer(&old.pc).await; }
                        }
                        // A tool peer promoted by capture_desktop stops its desktop but keeps its tools.
                        if desktop { for old in sessions.values() { old.demote(); } }
                        if desktop && desktop_backend::headless() {
                            log::info!("Desktop connection ignored: headless mode (terminal, file transfer and port mapping remain available)");
                            continue;
                        }
                        let resized=desktop && capture.init_error.is_none() && capture.backend==desktop_backend::DesktopBackend::X11
                            // Compare with this capture, not the engine: a live stream may already have refreshed it.
                            && tokio::task::spawn_blocking(crate::capture::x11_size).await.ok().flatten().is_some_and(|size| size!=(capture.width,capture.height));
                        if resized { log::info!("Desktop geometry changed since startup; reinitializing capture"); }
                        let session_capture=if desktop && portal_desktop.is_none() && (capture.init_error.is_some() || resized) {
                            match tokio::task::spawn_blocking(ScreenCapture::new).await {
                                Ok(Ok(screen))=>{ capture=Arc::new(screen);capture.clone() },
                                Ok(Err(error))=>{ log::warn!("Desktop connection unavailable: {error}"); continue; },
                                Err(_)=>{log::warn!("Desktop initialization worker failed");continue;},
                            }
                        } else { capture.clone() };
                        // Independent file/terminal peers must not detach the
                        // current desktop's authorized Portal input session.
                        if desktop && portal_desktop.is_none() { input.bind_portal(session_capture.portal.clone()); }
                        if desktop { if let Some(portal) = &portal_desktop { portal.begin(); } }
                        match WebRtcSession::new(session_capture, input.clone(), signaling_client.clone(), desktop, transport,
                            if desktop { portal_desktop.clone() } else { None }).await {
                            Ok(mut sess) => {
                                sess.controller = controller;
                                if !desktop {
                                    let (requests, peer) = (desktop_requests.clone(), ice.clone());
                                    sess.on_desktop_request(move || { let _ = requests.send(peer.clone()); });
                                }
                                // Register before awaiting SDP: shutdown can always close
                                // a peer whose answer may already be queued to the viewer.
                                sessions.insert(ice.clone(), sess);
                                let offer = sessions.get(&ice).expect("registered peer").handle_offer(&soac).await;
                                match offer {
                                Ok(answer_sdp) => {
                                    log::info!("Local SDP Answer generated, emitting soac response...");
                                    let resp = SoacMessage {
                                        msg_type: "answer".into(),
                                        sdp: Some(answer_sdp),
                                        gzip_sdp: None,
                                        candidate: None,
                                        ice_id: soac.ice_id.clone(),
                                        sdp_mid: None,
                                        sdp_mline_index: None,
                                        client_id: soac.client_id.clone(),
                                        app_control_id: soac.app_control_id.clone(),
                                    };
                                    if let Err(e) = signaling_client.emit_soac(resp).await { log::error!("Answer signaling send failed: {}", e); }
                                }
                                Err(_) => {
                                    log::error!("Failed to handle offer (details omitted)");
                                    if let Some(session) = sessions.remove(&ice) { close_peer(&session.pc).await; }
                                }
                            }},
                            Err(_) => log::error!("Create peer failed (details omitted)"),
                        }
                    } else if soac.msg_type == "candidate" {
                        if let Some(sess) = sessions.get(&ice) {
                            if let Err(e) = sess.add_candidate(&soac).await { let _ = e; log::error!("Remote ICE candidate failed (details omitted)"); }
                        } else {
                            log::warn!("Remote ICE candidate for unknown connection");
                        }
                    }
                }
                SignalingEvent::Released(val) | SignalingEvent::Left(val) => {
                    let ice = ["ice_id", "iceId"].iter().find_map(|k| val.get(*k).or_else(|| val.get("data").and_then(|d| d.get(*k))));
                    let keys: Vec<String> = val.as_object().map(|o| o.keys().take(16).cloned().collect()).unwrap_or_default();
                    let mut sessions = webrtc_sessions.lock().await;
                    let targets = released_peer_ids(&sessions, ice);
                    log::info!("Remote controller disconnected: keys={:?} ice_matched={} closing={}", keys, ice.is_some() && !targets.is_empty(), targets.len());
                    let mut desktop_ended = false;
                    for k in targets {
                        if let Some(old) = sessions.remove(&k) {
                            desktop_ended |= old.role.is_desktop();
                            close_peer(&old.pc).await;
                        }
                    }
                    if desktop_ended && !sessions.values().any(|s| s.role.is_desktop()) { if let Some(s) = assist.as_mut() { s.session_ended()?; } }
                    if let Some(s) = assist.as_mut() { s.tick()?; }
                }
                SignalingEvent::DesktopRequested(ice) => {
                    let mut sessions = webrtc_sessions.lock().await;
                    let Some(peer) = sessions.get(&ice).filter(|peer| !peer.role.is_desktop()) else { continue; };
                    if desktop_backend::headless() {
                        log::info!("Desktop request on a tool connection ignored: headless mode");
                        continue;
                    }
                    if portal_desktop.is_some() {
                        log::info!("Desktop request on a tool connection not supported with the Portal backend; open the desktop before port mapping");
                        continue;
                    }
                    let controller = peer.controller;
                    let current = peer.desktop_capture();
                    // Same recovery as a desktop offer: the display may have come up or been resized after this peer.
                    let resized = current.init_error.is_none() && current.backend == desktop_backend::DesktopBackend::X11
                        && tokio::task::spawn_blocking(crate::capture::x11_size).await.ok().flatten().is_some_and(|size| size != (current.width, current.height));
                    if resized { log::info!("Desktop geometry changed since this connection opened; reinitializing capture"); }
                    let screen = if current.init_error.is_some() || resized {
                        match tokio::task::spawn_blocking(ScreenCapture::new).await {
                            Ok(Ok(screen)) => { capture = Arc::new(screen); Some(capture.clone()) },
                            Ok(Err(error)) => { log::info!("Desktop request on a tool connection ignored: {error}"); continue; },
                            Err(_) => { log::warn!("Desktop initialization worker failed"); continue; },
                        }
                    } else { None };
                    video::warm_auto_probe(); video::check_ffmpeg_usable();
                    input.set_controller(controller);
                    input.bind_portal(screen.as_ref().map_or_else(|| sessions[&ice].capture.portal.clone(), |screen| screen.portal.clone()));
                    // One desktop at a time, as for a new desktop offer. Peers offered as desktops
                    // close; a promoted tool peer only stops its desktop and keeps its tools.
                    let superseded: Vec<String> = sessions.iter().filter(|(id, old)| id.as_str() != ice && old.role.is_desktop())
                        .map(|(id, _)| id.clone()).collect();
                    for id in superseded {
                        if sessions[&id].desktop { if let Some(old) = sessions.remove(&id) { close_peer(&old.pc).await; } }
                        else { sessions[&id].demote(); }
                    }
                    if let Some(peer) = sessions.get(&ice) { peer.promote(screen); }
                }
                SignalingEvent::Disconnected => {
                    capture_types.clear();
                    next_capture_type=None;
                    log::warn!("Signaling connection lost. Initiating reconnect loop...");
                    break;
                }
            }
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
    }
}
