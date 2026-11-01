//! Recovery record beside the config (`privacy-power.json`, 0600) and the process lock.
use super::Mode;

/// What a backend changed, so the guard or the next start can undo it after a crash.
/// `data` belongs to the backend named in `backend`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Record { pub backend: String, pub mode: Mode, pub data: serde_json::Value }

pub fn path() -> std::path::PathBuf { crate::config::path().with_file_name("privacy-power.json") }
pub fn pending() -> bool { path().exists() }

/// One process controls the outputs of a config directory (host, probe, guard). Only success is
/// kept: a lock still held by a dying host must be retried (the guard polls this).
pub fn ownership() -> Result<(), String> {
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    static LOCK: std::sync::Mutex<Option<std::fs::File>> = std::sync::Mutex::new(None);
    let mut held = LOCK.lock().map_err(|_| "privacy lock state poisoned")?;
    if held.is_some() { return Ok(()); }
    let path = path().with_extension("lock");
    std::fs::create_dir_all(path.parent().ok_or("missing config directory")?).map_err(|e| e.kind().to_string())?;
    let file = std::fs::OpenOptions::new().read(true).write(true).create(true).mode(0o600)
        .open(path).map_err(|e| e.kind().to_string())?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("output control is owned by another process".into());
    }
    *held = Some(file);
    Ok(())
}

pub fn load() -> Option<Record> { serde_json::from_slice(&std::fs::read(path()).ok()?).ok() }
fn store(record: &Record) -> Result<(), String> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let path = path();
    std::fs::create_dir_all(path.parent().ok_or("missing config directory")?).map_err(|e| e.kind().to_string())?;
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600)
        .open(&tmp).map_err(|e| e.kind().to_string())?;
    file.write_all(&serde_json::to_vec(record).map_err(|_| "invalid saved state")?).map_err(|e| e.kind().to_string())?;
    file.sync_all().map_err(|e| e.kind().to_string())?;
    std::fs::rename(tmp, path).map_err(|e| e.kind().to_string())
}
pub fn remove() -> Result<(), String> {
    std::fs::remove_file(path()).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) })
        .map_err(|e| format!("remove recovery record: {}", e.kind()))
}

/// Handed to `Backend::enter`: save what undoes a step before making it.
pub struct Journal { record: Record, saved: bool }
impl Journal {
    pub fn new(backend: &str, mode: Mode) -> Self {
        Self { record: Record { backend: backend.into(), mode, data: serde_json::Value::Null }, saved: false }
    }
    pub fn save<T: serde::Serialize>(&mut self, data: &T) -> Result<(), String> {
        self.record.data = serde_json::to_value(data).map_err(|_| "invalid saved state")?;
        store(&self.record)?;
        self.saved = true;
        Ok(())
    }
    /// The last saved record; None when the backend changed nothing.
    pub fn into_record(self) -> Option<Record> { self.saved.then_some(self.record) }
}
