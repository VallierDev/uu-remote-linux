//! `uuremote-server doctor`: one report with what an issue needs (session, backend, plugins,
//! input permission, encoders, config and service state), safe to paste publicly. It prints only
//! booleans, names and versions: no account ids, tokens, addresses, MACs or file contents, and the
//! home directory is shown as `~`.

fn redact(text: &str) -> String {
    let mut out = text.to_owned();
    if let Some(home) = std::env::var_os("HOME").and_then(|h| h.into_string().ok()).filter(|h| h.len() > 1) { out = out.replace(&home, "~"); }
    if let Ok(user) = std::env::var("USER") { if user.len() > 2 { out = out.replace(&user, "<user>"); } }
    out
}

fn env_state(name: &str) -> String {
    match std::env::var(name) { Ok(v) if v.is_empty() => "set (empty)".into(), Ok(_) => "set".into(), Err(_) => "unset".into() }
}

fn line(key: &str, value: impl std::fmt::Display) { println!("{key:<22} {}", redact(&value.to_string())); }

fn format_probe(probe: &crate::video_select::ProbeReport) -> String {
    let body = probe.order.iter().zip(probe.outcomes.iter())
        .map(|(e, o)| format!("{}={}", format!("{e:?}").to_ascii_lowercase(), o.label()))
        .collect::<Vec<_>>().join(" ");
    if probe.expires_at.is_some() { format!("{body} (failure result, retried after 10 min)") } else { body }
}

fn glibc() -> String {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        extern "C" { fn gnu_get_libc_version() -> *const libc::c_char; }
        return unsafe { std::ffi::CStr::from_ptr(gnu_get_libc_version()) }.to_string_lossy().into_owned();
    }
    #[allow(unreachable_code)]
    "n/a".into()
}

fn writable(path: &str) -> &'static str {
    let Ok(c_path) = std::ffi::CString::new(path) else { return "invalid" };
    if !std::path::Path::new(path).exists() { "missing" }
    else if unsafe { libc::access(c_path.as_ptr(), libc::W_OK) } == 0 { "writable" } else { "no permission" }
}

async fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = tokio::time::timeout(std::time::Duration::from_secs(5),
        tokio::process::Command::new(program).args(args).kill_on_drop(true).output()).await.ok()?.ok()?;
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Whether the report must say that this session has no screen to capture: neither DISPLAY nor
/// WAYLAND_DISPLAY is set, and the backend is the X11 one that needs a display to open.
fn missing_display(backend: crate::desktop_backend::DesktopBackend, headless: bool,
                   display: Option<&std::ffi::OsStr>, wayland: Option<&std::ffi::OsStr>) -> bool {
    !headless && backend == crate::desktop_backend::DesktopBackend::X11
        && display.is_none() && wayland.is_none()
}

pub async fn run() -> Result<(), String> {
    use crate::desktop_backend as backend;
    use crate::engine_plugin::{load, Engine};
    // Info logs (e.g. plugin paths under the home directory) would bypass the redaction below.
    log::set_max_level(log::LevelFilter::Warn);
    println!("uu-remote-linux doctor (safe to paste: no ids, tokens, addresses or file contents)");
    line("version", format!("v{} {}", env!("CARGO_PKG_VERSION"), std::env::consts::ARCH));
    line("glibc", glibc());
    let os = std::fs::read_to_string("/etc/os-release").ok()
        .and_then(|t| t.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_owned())));
    line("os", os.unwrap_or_else(|| "unknown".into()));
    let features: Vec<&str> = [("wayland", cfg!(feature = "wayland")), ("portal", cfg!(feature = "portal")),
        ("engine-plugins", cfg!(feature = "engine-plugins")), ("inproc-av", cfg!(feature = "inproc-av"))]
        .into_iter().filter(|(_, on)| *on).map(|(name, _)| name).collect();
    line("build features", if features.is_empty() { "default (X11 only)".to_owned() } else { features.join(", ") });

    println!("-- session");
    line("XDG_SESSION_TYPE", std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "unset".into()));
    line("XDG_CURRENT_DESKTOP", std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "unset".into()));
    line("DISPLAY", env_state("DISPLAY"));
    line("WAYLAND_DISPLAY", env_state("WAYLAND_DISPLAY"));
    line("wayland running", backend::wayland_session_running());
    line("UU_DESKTOP_BACKEND", std::env::var("UU_DESKTOP_BACKEND").unwrap_or_else(|_| "auto".into()));
    let selected = backend::detect();
    line("selected backend", match &selected { Ok(b) if backend::headless() => format!("none (headless, {b:?} unused)"), Ok(b) => format!("{b:?}"), Err(e) => format!("error: {e}") });
    if selected.as_ref().is_ok_and(|b| *b == backend::DesktopBackend::X11) && backend::wayland_session_running() && !backend::headless() {
        println!("  ! X11 backend inside a Wayland session: the picture will be black");
    }
    // A session without any display cannot capture a screen; say so instead of naming a backend.
    if let Ok(selected) = &selected {
        if missing_display(*selected, backend::headless(), std::env::var_os("DISPLAY").as_deref(), std::env::var_os("WAYLAND_DISPLAY").as_deref()) {
            println!("  ! DISPLAY and WAYLAND_DISPLAY are unset: the X11 engine will only try :0 and :1; without an X server there, no screen can be captured");
            println!("    terminal, file transfer and port mapping still work; UU_DESKTOP_BACKEND=none marks the host as headless");
        }
    }
    if cfg!(feature = "engine-plugins") {
        for engine in [Engine::Wayland, Engine::Portal] {
            line(engine.file(), match load(engine) { Ok(()) => "loaded".to_owned(), Err(e) => e });
        }
    }

    println!("-- input");
    line("/dev/uinput", writable("/dev/uinput"));
    line("UU_INPUT_BACKEND", std::env::var("UU_INPUT_BACKEND").unwrap_or_else(|_| "auto".into()));

    println!("-- video");
    let selection = crate::video_select::selection();
    let env_enc = std::env::var("UU_VIDEO_ENCODER").unwrap_or_else(|_| "unset".into());
    let codec = match selection.codec_pref { crate::video_select::CodecPref::Auto => "auto", crate::video_select::CodecPref::H264 => "h264", crate::video_select::CodecPref::H265 => "h265" };
    let probe_text = if selection.encoder_source == crate::video_select::Source::Auto {
        format_probe(&crate::video::run_auto_probe().await)
    } else {
        crate::video_select::cached_probe().as_ref().map(format_probe).unwrap_or_else(|| "not yet run".to_owned())
    };
    let selection = if selection.encoder_source == crate::video_select::Source::Auto { crate::video_select::selection() } else { selection };
    line("encoder probe", probe_text);
    line("encoder", format!("{:?} (source={}; UU_VIDEO_ENCODER={env_enc})", selection.encoder, crate::video_select::encoder_source_label(selection.encoder_source)));
    line("codec", format!("{codec} (source={})", crate::video_select::source_label(selection.codec_source)));
    // Both codecs offered: hardware prefers H.265, CPU stays H.264 unless the preference forces one.
    let resolved = crate::capture_setting::pick_codec(true, true, crate::video_select::codec_force(selection.codec_pref), crate::video_select::hardware(selection.encoder));
    line("resolved codec", format!("{resolved:?}"));
    match crate::video::ffmpeg_encoders().await {
        Some(found) => line("ffmpeg encoders", if found.is_empty() { "none of libx264/libx265/nvenc/vaapi".to_owned() } else { found.join(", ") }),
        None => line("ffmpeg encoders", "ffmpeg not runnable (install ffmpeg or set ffmpeg_root)"),
    }
    let mut version = crate::video::ffmpeg_command();
    version.arg("-version").kill_on_drop(true);
    if let Ok(Ok(output)) = tokio::time::timeout(std::time::Duration::from_secs(5), version.output()).await {
        line("ffmpeg version", String::from_utf8_lossy(&output.stdout).lines().next().unwrap_or("").split_whitespace().nth(2).unwrap_or("unknown"));
    }
    line("nvidia driver", std::path::Path::new("/proc/driver/nvidia/version").exists());
    line("render node", writable(&std::env::var("UU_VAAPI_DEVICE").unwrap_or_else(|_| "/dev/dri/renderD128".into())));

    println!("-- audio");
    for tool in ["pactl", "pw-cli"] {
        line(tool, command_output("sh", &["-c", &format!("command -v {tool} >/dev/null && echo found || echo missing")]).await.unwrap_or_else(|| "unknown".into()));
    }

    println!("-- account and service");
    let path = crate::config::path();
    line("config", path.display());
    match std::fs::metadata(&path) {
        Ok(meta) => {
            use std::os::unix::fs::PermissionsExt;
            line("config mode", format!("{:o}", meta.permissions().mode() & 0o777));
            // Presence only: never read values out of the file.
            let fields: Option<serde_json::Value> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok());
            let has = |key: &str| fields.as_ref().and_then(|v| v.get(key)).is_some_and(|v| !v.is_null() && v.as_str() != Some(""));
            line("signed in", has("token") && has("user_id"));
            line("device registered", has("device_id"));
        }
        Err(_) => line("config mode", "missing (run `uuremote-server login`)"),
    }
    line("service", command_output("systemctl", &["--user", "is-active", "uuremote-native.service"]).await.unwrap_or_else(|| "systemctl unavailable".into()));
    let running = host_processes();
    line("uuremote-server procs", format!("{} (including this one; more than 2 means duplicates)", running.map_or_else(|| "unknown".into(), |n| n.to_string())));
    if running.is_some_and(|n| n > 2) {
        println!("  ! another host instance is running (check `pgrep -af uuremote`); stop the extra one, e.g. an old copy started by hand");
        println!("    instances sharing one identity, on this or another machine, kick each other out of the room: never copy config.json between machines");
    }
    Ok(())
}

/// Hosts re-exec through /proc/self/exe (comm "exe"), so match the executable's file name, not the process name.
/// Helpers a host spawns (Wayland capture, login window) are its children and are not separate instances.
fn host_processes() -> Option<usize> {
    let hosts: Vec<(u32, u32)> = std::fs::read_dir("/proc").ok()?.flatten()
        .filter_map(|entry| Some((entry.file_name().to_str()?.parse::<u32>().ok()?, entry.path())))
        .filter(|(_, dir)| std::fs::read_link(dir.join("exe")).is_ok_and(|exe| is_host_executable(&exe)))
        .filter_map(|(pid, dir)| Some((pid, parent_pid(&std::fs::read_to_string(dir.join("stat")).ok()?)?)))
        .collect();
    Some(top_level(&hosts))
}

/// Field 4 of /proc/<pid>/stat; the command name in field 2 may contain spaces and parentheses.
fn parent_pid(stat: &str) -> Option<u32> {
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

fn top_level(hosts: &[(u32, u32)]) -> usize {
    hosts.iter().filter(|(_, parent)| !hosts.iter().any(|(pid, _)| pid == parent)).count()
}

fn is_host_executable(exe: &std::path::Path) -> bool {
    exe.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.trim_end_matches(" (deleted)") == "uuremote-server")
}

#[cfg(test)]
mod tests {
    #[test]
    fn redaction_hides_home_and_user() {
        let home = std::env::var("HOME").unwrap_or_default();
        if home.len() > 1 { assert_eq!(super::redact(&format!("{home}/.config/x")), "~/.config/x"); }
        assert_eq!(super::env_state("UU_DOCTOR_TEST_SURELY_UNSET"), "unset");
    }
    #[test]
    fn a_session_without_any_display_is_reported() {
        use crate::desktop_backend::DesktopBackend;
        let display = std::ffi::OsStr::new(":0");
        let wayland = std::ffi::OsStr::new("wayland-1");
        // No DISPLAY and no WAYLAND_DISPLAY: nothing can be captured.
        assert!(super::missing_display(DesktopBackend::X11, false, None, None));
        // One of them present: the backend has a screen to open.
        assert!(!super::missing_display(DesktopBackend::X11, false, Some(display), None));
        assert!(!super::missing_display(DesktopBackend::X11, false, None, Some(wayland)));
        // Another backend, or a headless host: not this warning.
        assert!(!super::missing_display(DesktopBackend::Wayland, false, None, None));
        assert!(!super::missing_display(DesktopBackend::X11, true, None, None));
    }

    #[test]
    fn host_processes_match_the_executable_even_after_reexec() {
        use std::path::Path;
        for exe in ["/usr/bin/uuremote-server", "/home/u/.local/bin/uuremote-server", "/usr/bin/uuremote-server (deleted)"] {
            assert!(super::is_host_executable(Path::new(exe)));
        }
        for exe in ["/usr/bin/uuremote-viewer", "/usr/bin/bash", "/usr/bin/uuremote-server-old"] {
            assert!(!super::is_host_executable(Path::new(exe)));
        }
    }

    #[test]
    fn host_helpers_are_not_counted_as_instances() {
        assert_eq!(super::parent_pid("8943 (exe) S 1699 8943 1699 0 -1"), Some(1699));
        assert_eq!(super::parent_pid("12 (a) b (c) R 7 12"), Some(7));
        // service 10 with a login window 11, a stale copy 20, doctor 30
        assert_eq!(super::top_level(&[(10, 1), (11, 10), (20, 5), (30, 9)]), 3);
        assert_eq!(super::top_level(&[(10, 1), (11, 10), (30, 9)]), 2);
    }
}
