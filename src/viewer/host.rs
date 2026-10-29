//! "Allow the same account to control this device": the Settings switch runs this computer's own
//! host (`uuremote-server`, the sibling binary) as the systemd user service, so the Linux machine
//! shows up in every client of that account. Hosting is opt-in: the viewer never enables the
//! service by itself, and the unit's enabled state is the saved choice. The host reads the shared
//! config.json, so a running host only needs a restart to go online with a new account.
use std::process::{Command, Stdio};

const UNIT: &str = "uuremote-native.service";
/// `UU_VIEWER_HOST=0` leaves the host service alone.
pub fn managed() -> bool { std::env::var("UU_VIEWER_HOST").as_deref() != Ok("0") }
fn systemctl(args: &[&str]) -> bool {
    Command::new("systemctl").arg("--user").args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .status().is_ok_and(|s| s.success())
}
pub fn active() -> bool { systemctl(&["is-active", "--quiet", UNIT]) }
fn installed() -> bool { systemctl(&["cat", UNIT]) }

/// Same unit as packaging/deb and README (graphical-session.target imports DISPLAY / WAYLAND_DISPLAY).
fn unit_text(server: &std::path::Path) -> String {
    format!("[Unit]\nDescription=UU Remote Linux host (unofficial)\nAfter=graphical-session.target\nPartOf=graphical-session.target\n\n\
[Service]\nEnvironment=RUST_LOG=info\nExecStart={}\nRestart=always\nRestartSec=5\n\n[Install]\nWantedBy=graphical-session.target\n", server.display())
}
fn install(server: &std::path::Path) -> Result<(), String> {
    let base = std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from).filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))).ok_or("No config directory")?;
    let dir = base.join("systemd/user");
    std::fs::create_dir_all(&dir).map_err(|_| "Cannot create the systemd user directory")?;
    std::fs::write(dir.join(UNIT), unit_text(server)).map_err(|_| "Cannot write the host service")?;
    if systemctl(&["daemon-reload"]) { Ok(()) } else { Err("systemctl --user daemon-reload failed".into()) }
}

/// Starts the host and keeps it enabled (installing the user unit next to the viewer's binary if
/// there is none). Only the Settings switch calls this.
pub fn enable(viewer: &std::path::Path) -> Result<(), String> {
    if !installed() {
        let server = viewer.with_file_name("uuremote-server");
        if !server.is_file() { return Err("uuremote-server is not installed next to the viewer".into()) }
        install(&server)?;
    }
    if systemctl(&["enable", "--now", UNIT]) { log::info!("Host service enabled"); Ok(()) } else { Err("Cannot start the host service".into()) }
}
/// The account changed: a running host reloads its credentials. A stopped host stays stopped.
pub fn restart() -> Result<(), String> {
    if systemctl(&["try-restart", UNIT]) { log::info!("Host service restarted"); Ok(()) } else { Err("Cannot restart the host service".into()) }
}
pub fn disable() -> Result<(), String> {
    if systemctl(&["disable", "--now", UNIT]) { log::info!("Host service disabled"); Ok(()) } else { Err("Cannot stop the host service".into()) }
}

#[cfg(test)] mod tests {
    #[test] fn unit_runs_the_given_server(){
        let text=super::unit_text(std::path::Path::new("/opt/uu/uuremote-server"));
        assert!(text.contains("ExecStart=/opt/uu/uuremote-server\n"));assert!(text.contains("WantedBy=graphical-session.target"));
    }
}
