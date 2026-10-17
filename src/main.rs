//! UU Remote Pure Native Linux Server (Rust Edition)
//! Zero-Wine, Zero-Python, Single-Binary, 5 Native Tools Integrated

mod desktop_backend;
mod desktop_session;
mod desktop_power;
mod portal;
mod network_diagnostics;
mod session_ice;
mod wayland;
mod auth;
mod assist;
mod config;
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

fn superseded_peer_ids(sessions: &HashMap<String, WebRtcSession>, ice: &str, desktop: bool) -> Vec<String> {
    sessions.iter().filter(|(id, peer)| id.as_str() == ice || (desktop && peer.desktop))
        .map(|(id, _)| id.clone()).collect()
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
/// SIGUSR1 (`uuremote-server show`) opens the host window.
fn present_on_usr1(ui: std::sync::Arc<assist::LocalUi>) {
    // Install the handler before publishing the pid: SIGUSR1's default action terminates the process.
    let Ok(mut usr1) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1()) else { return };
    write_host_pid();
    tokio::spawn(async move { while usr1.recv().await.is_some() { ui.present(); } });
}

fn host_pid_path() -> std::path::PathBuf { config::path().with_file_name("host.pid") }
/// Lets `uuremote-server show` find this host service to open its window (SIGUSR1).
fn write_host_pid() {
    use std::os::unix::fs::OpenOptionsExt;
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(host_pid_path()) {
        let _ = write!(f, "{}", std::process::id());
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
        for _ in 0..60 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let fresh = std::fs::metadata(host_pid_path()).and_then(|m| m.modified()).ok().is_some_and(|t| Some(t) != before);
            let pid = std::fs::read_to_string(host_pid_path()).ok().and_then(|s| s.trim().parse::<i32>().ok());
            if let (true, Some(pid)) = (fresh, pid) {
                std::thread::sleep(std::time::Duration::from_millis(500)); // signal handler installed right after the pid
                if unsafe { libc::kill(pid, libc::SIGUSR1) } == 0 { return Ok(()); }
            }
        }
        return Err("the UU remote host service started but did not open its window".into());
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
    const SUBCOMMANDS: [&str; 13] = ["show", "assist", "desktop-input-probe", "virtual-audio-probe", "login", "unlock-check", "desktop-probe", "desktop-video-probe", "desktop-audio-probe", "wayland-capture",
        "wayland-input-probe", "clipboard-acceptance", "portal-acceptance"];
    if let Some(arg) = std::env::args().nth(1).filter(|a| !SUBCOMMANDS.contains(&a.as_str())) {
        let usage = format!("usage: uuremote-server [{}]\n  (no argument: run the host)", SUBCOMMANDS.join(" | "));
        if matches!(arg.as_str(), "help" | "-h" | "--help") { println!("{usage}"); return Ok(()); }
        eprintln!("unknown argument\n{usage}");
        std::process::exit(2);
    }

    if std::env::args().nth(1).as_deref() == Some("show") { return show_running_host(); }
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

    let capture = Arc::new((if desktop_backend::headless() {
        Err("headless mode (UU_DESKTOP_BACKEND=none)".to_owned())
    } else if desktop_backend::detect()?==desktop_backend::DesktopBackend::Portal {
        Err("Portal capture waits for a desktop CONTROL connection".to_owned())
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
    for (_, old) in webrtc_sessions.lock().await.drain() { let _ = old.pc.close().await; }
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
    let mut mac_by_ice: HashMap<String, bool> = HashMap::new();
    let mut next_mac = false;

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
                    for id in ended { if let Some(s) = sessions.remove(&id) { desktop_ended |= s.desktop; let _ = s.pc.close().await; } }
                    if desktop_ended && !sessions.values().any(|s| s.desktop) { if let Some(s) = assist.as_mut() { s.session_ended()?; } }
                    if let Some(s) = assist.as_mut() { s.tick()?; }
                }
                SignalingEvent::Connected => {
                    log::info!("Signaling handshake established.");
                }
                SignalingEvent::RoomInfoAck(_) => { log::info!("RoomInfo acknowledged by NetEase server"); }
                SignalingEvent::DeviceInfoChanged(val) => {
                    if assist.is_some() {
                        // Shape only (type value + key names), never field values: guest challenges carry salts/ids.
                        let keys = |v: Option<&serde_json::Value>| v.and_then(|v| v.as_object()).map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
                        log::debug!("Assist bmsg_push shape: type={:?} keys={:?} data_keys={:?}",
                            val.get("type").and_then(serde_json::Value::as_str), keys(Some(&val)), keys(val.get("data")));
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
                SignalingEvent::ConnectRequest { capture_type, ice_id, transport, controller_mac } => {
                    if mac_by_ice.len()>=32 { mac_by_ice.clear(); }
                    match ice_id.clone() { Some(id) => { mac_by_ice.insert(id, controller_mac); } None => next_mac = controller_mac }
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
                    for k in ended { if let Some(old) = sessions.remove(&k) { desktop_ended |= old.desktop; let _ = old.pc.close().await; } }
                    if desktop_ended && !sessions.values().any(|s| s.desktop) { if let Some(s) = assist.as_mut() { s.session_ended()?; } }
                    if let Some(s) = assist.as_mut() { s.tick()?; }

                    if soac.msg_type == "offer" {
                        let (capture_type,transport) = capture_types.remove(&ice).or_else(|| next_capture_type.take()).unwrap_or((1,crate::session_ice::SessionIce::default()));
                        // CaptureType: 0 unknown, 1 desktop, 2 window; 5 file transfer, 8 terminal, 9 port mapping.
                        let desktop = capture_type <= 2;
                        let controller_mac = mac_by_ice.remove(&ice).unwrap_or(next_mac);
                        if desktop { input.set_mac_controller(controller_mac); }
                        log::info!("Offer for connection: capture_type={} desktop={} active_peers={}", capture_type, desktop, sessions.len());
                        // Replace only this ICE id or a previous desktop, never tool peers.
                        for id in superseded_peer_ids(&sessions, &ice, desktop) {
                            if let Some(old) = sessions.remove(&id) { let _ = old.pc.close().await; }
                        }
                        if desktop && desktop_backend::headless() {
                            log::info!("Desktop connection ignored: headless mode (terminal, file transfer and port mapping remain available)");
                            continue;
                        }
                        let resized=desktop && capture.init_error.is_none() && capture.backend==desktop_backend::DesktopBackend::X11
                            && tokio::task::spawn_blocking(crate::capture::x11_resized).await.unwrap_or(false);
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
                            Ok(sess) => {
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
                                    if let Some(session) = sessions.remove(&ice) { let _ = session.pc.close().await; }
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
                            desktop_ended |= old.desktop;
                            if let Err(e) = old.pc.close().await { log::warn!("Close released peer failed: {}", e); }
                        }
                    }
                    if desktop_ended && !sessions.values().any(|s| s.desktop) { if let Some(s) = assist.as_mut() { s.session_ended()?; } }
                    if let Some(s) = assist.as_mut() { s.tick()?; }
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
