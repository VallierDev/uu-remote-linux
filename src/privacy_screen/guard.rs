//! Desktops may persist what a backend changes (KWin saves input-device flags), so a killed host
//! could leave local input or screens off for good. This child recovers from the record when the
//! host's end of its stdin pipe closes, however the host ends; after a normal restore the record is
//! gone and it just exits.
use std::{process::{Child, Command, Stdio}, sync::Mutex};

static GUARD: Mutex<Option<Child>> = Mutex::new(None);

pub fn start() -> Result<(), String> {
    let mut guard = GUARD.lock().map_err(|_| "guard state poisoned")?;
    if guard.is_some() { return Ok(()); }
    let exe = std::env::current_exe().map_err(|e| e.kind().to_string())?;
    let child = Command::new(exe).arg("privacy-guard").stdin(Stdio::piped()).stdout(Stdio::null()).spawn()
        .map_err(|e| format!("privacy guard: {}", e.kind()))?;
    *guard = Some(child);
    Ok(())
}
pub fn stop() {
    if let Some(mut child) = GUARD.lock().ok().and_then(|mut g| g.take()) { drop(child.stdin.take()); let _ = child.wait(); }
}
/// `privacy-guard`: waits for the host to go away, then recovers.
pub fn main() -> i32 {
    use std::io::Read;
    let _ = std::io::stdin().read_to_end(&mut Vec::new());
    // A normal restore removes the record before closing the pipe.
    if !super::record::pending() { return 0; }
    // The host's flock is released with its last descriptor; give a dying host a moment.
    super::tool::wait_until(|| super::record::ownership().is_ok());
    if super::record::pending() { log::warn!("Privacy guard: host ended without restoring; recovering"); super::recover(); }
    0
}
