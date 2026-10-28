//! Single-use Portal permissions. Never log the token or reuse it after an attempt.
use std::{fs::{self, OpenOptions}, io::{Read, Write}, path::{Path, PathBuf}};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

pub fn directory() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
        .filter(|p| p.is_absolute()).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    let dir = base.join("uuremote/portal-permissions");
    // Reject symlinks in the parent chain; do not redirect credential storage.
    for parent in dir.ancestors() {
        if let Ok(meta) = fs::symlink_metadata(parent) {
            if meta.file_type().is_symlink() { return None; }
        }
    }
    fs::create_dir_all(&dir).ok()?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).ok()?;
    let meta = fs::symlink_metadata(&dir).ok()?;
    (meta.is_dir() && meta.uid() == unsafe { libc::getuid() }).then_some(dir)
}

pub fn desktop_key() -> &'static str {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_lowercase();
    if desktop.split(':').any(|v| v == "kde") { "kde" } else { "gnome" }
}

/// Consent for one monitor (Mutter connector); connectors are checked to be file-name safe.
pub fn monitor_key(connector: &str) -> String { format!("{}@{}", desktop_key(), connector) }

pub fn take(dir: &Path, key: &str) -> Option<String> {
    let path = dir.join(key);
    let mut file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(&path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.uid() != unsafe { libc::getuid() } || meta.mode() & 0o077 != 0 || meta.len() > 4096 { return None; }
    let mut token = String::new();
    file.read_to_string(&mut token).ok()?;
    // Consume before contacting the Portal: a timeout may still consume the upstream token.
    fs::remove_file(path).ok()?;
    (!token.is_empty() && !token.contains('\0')).then_some(token)
}

pub fn save(dir: &Path, key: &str, token: &str) -> std::io::Result<()> {
    if token.is_empty() || token.len() > 4096 || token.contains('\0') {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let temp = dir.join(format!(".{}.{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
        file.write_all(token.as_bytes())?; file.sync_all()?;
        fs::rename(&temp, dir.join(key))?;
        std::fs::File::open(dir)?.sync_all()
    })();
    if result.is_err() { let _ = fs::remove_file(temp); }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn permissions_are_private_single_use_and_reject_symlinks() {
        let dir = std::env::temp_dir().join(format!("uu-portal-cache-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        save(&dir, "kde", "test-token").unwrap();
        assert_eq!(fs::metadata(dir.join("kde")).unwrap().mode() & 0o777, 0o600);
        assert_eq!(take(&dir, "kde").as_deref(), Some("test-token"));
        assert!(take(&dir, "kde").is_none());
        save(&dir, "gnome", "second-token").unwrap();
        std::os::unix::fs::symlink(dir.join("gnome"), dir.join("kde")).unwrap();
        assert!(take(&dir, "kde").is_none());
        assert!(save(&dir, "gnome", &"x".repeat(4097)).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
