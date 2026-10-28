//! Runtime configuration. Nothing machine- or account-specific is compiled in.
//! File: $UU_CONFIG, else $XDG_CONFIG_HOME/uuremote/config.json, else ~/.config/uuremote/config.json.
//! Environment variables override file values. Keep the file private (chmod 600).

use std::path::PathBuf;
use std::sync::OnceLock;

/// Tests that point UU_CONFIG at a temp file must take this, or they race.
#[cfg(test)]
pub(crate) fn test_env_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    &LOCK
}

pub fn path() -> PathBuf {
    if let Some(p) = std::env::var_os("UU_CONFIG") { return PathBuf::from(p); }
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("uuremote/config.json")
}

/// Re-reads the config file (used after `login` rewrites it while the server runs).
pub fn read_fresh() -> Result<Option<serde_json::Value>, String> {
    let p = path();
    match std::fs::read(&p) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| format!("invalid config {}: {e}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read config {}: {e}", p.display())),
    }
}

/// Parsed config file, or None when absent. A malformed file is an error, not silently ignored.
pub fn file() -> Result<Option<&'static serde_json::Value>, String> {
    static FILE: OnceLock<Result<Option<serde_json::Value>, String>> = OnceLock::new();
    FILE.get_or_init(|| {
        let p = path();
        match std::fs::read(&p) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| format!("invalid config {}: {e}", p.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("cannot read config {}: {e}", p.display())),
        }
    }).as_ref().map(|v| v.as_ref()).map_err(Clone::clone)
}

/// Optional bundled FFmpeg prefix: UU_FFMPEG_ROOT or config "ffmpeg_root".
pub fn ffmpeg_root() -> Option<PathBuf> {
    std::env::var_os("UU_FFMPEG_ROOT").filter(|v| !v.is_empty()).map(PathBuf::from)
        .or_else(|| file().ok().flatten().and_then(|f| f.get("ffmpeg_root")).and_then(|v| v.as_str()).map(PathBuf::from))
}
