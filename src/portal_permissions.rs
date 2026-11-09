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

/// An unclassified grant is kept separately until its monitor is known.
pub fn pending_key(key: &str) -> String { format!("{key}.pending") }

pub fn exists(dir: &Path, key: &str) -> bool {
    dir.join(key).exists() || dir.join(pending_key(key)).exists()
}

pub struct Rotation {
    directory: Option<PathBuf>,
    pending: String,
    saved: Option<String>,
    token: Option<String>,
}

impl Rotation {
    pub fn new(directory: Option<PathBuf>, requested_key: &str) -> Self {
        Self { directory, pending: pending_key(requested_key), saved: None, token: None }
    }

    pub fn received(&self) -> bool { self.token.is_some() }

    /// A fresh request has not consumed the old grant; keep it until the new session opens.
    pub fn remember(&mut self, token: &str) { self.token = Some(token.to_owned()); }

    /// Persist before PipeWire starts; `known_key` comes from the actual granted source.
    pub fn record(&mut self, token: &str, known_key: Option<&str>) -> std::io::Result<()> {
        self.remember(token);
        let Some(dir) = self.directory.as_deref() else { return Ok(()); };
        let destination = known_key.unwrap_or(&self.pending);
        save(dir, destination, token)?;
        self.saved = Some(destination.to_owned());
        if let Some(key) = known_key { let _ = fs::remove_file(dir.join(pending_key(key))); }
        Ok(())
    }

    /// Move, rather than copy, so a single-use token never has two cache entries.
    pub fn finish(&mut self, destination: &str, replace: bool) -> std::io::Result<()> {
        let (Some(dir), Some(token)) = (self.directory.as_deref(), self.token.as_deref()) else { return Ok(()); };
        if self.saved.as_deref() == Some(destination) { return Ok(()); }
        if !replace && dir.join(destination).exists() { return Ok(()); }
        if let Some(source) = self.saved.as_deref() {
            fs::rename(dir.join(source), dir.join(destination))?;
            fs::File::open(dir)?.sync_all()?;
        } else {
            save(dir, destination, token)?;
        }
        self.saved = Some(destination.to_owned());
        let _ = fs::remove_file(dir.join(pending_key(destination)));
        Ok(())
    }
}

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

    #[test]
    fn rotated_grant_survives_failure_and_is_moved_after_success() {
        let dir = std::env::temp_dir().join(format!("uu-rotation-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        save(&dir, "gnome@other", "other-grant").unwrap();
        let mut rotation = Rotation::new(Some(dir.clone()), "gnome@wanted");
        rotation.record("new-grant", None).unwrap();
        // Simulate capture failure: no finish callback runs.
        assert!(exists(&dir, "gnome@wanted"));
        assert_eq!(take(&dir, &pending_key("gnome@wanted")).as_deref(), Some("new-grant"));
        assert!(!exists(&dir, "gnome@wanted"));
        assert_eq!(take(&dir, "gnome@other").as_deref(), Some("other-grant"));

        let mut rotation = Rotation::new(Some(dir.clone()), "gnome@wanted");
        rotation.record("newer-grant", None).unwrap();
        rotation.finish("gnome@actual", true).unwrap();
        assert!(!dir.join(pending_key("gnome@wanted")).exists());
        assert_eq!(take(&dir, "gnome@actual").as_deref(), Some("newer-grant"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn known_grants_replace_only_their_monitor_and_unknown_grants_do_not_overwrite() {
        let dir = std::env::temp_dir().join(format!("uu-rotation-known-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        save(&dir, "gnome", "existing-grant").unwrap();
        save(&dir, "gnome@other", "other-grant").unwrap();
        let mut unknown = Rotation::new(Some(dir.clone()), "gnome@wanted");
        unknown.record("unknown-grant", None).unwrap();
        unknown.finish("gnome", false).unwrap();
        assert_eq!(take(&dir, "gnome").as_deref(), Some("existing-grant"));
        assert_eq!(take(&dir, &pending_key("gnome@wanted")).as_deref(), Some("unknown-grant"));
        let mut known = Rotation::new(Some(dir.clone()), "gnome@wanted");
        known.record("known-grant", Some("gnome@actual")).unwrap();
        assert_eq!(take(&dir, "gnome@actual").as_deref(), Some("known-grant"));
        assert_eq!(take(&dir, "gnome@other").as_deref(), Some("other-grant"));
        let mut no_token = Rotation::new(Some(dir.clone()), "gnome");
        no_token.finish("gnome", true).unwrap();
        assert!(!no_token.received());
        assert!(!dir.join("gnome").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fresh_request_keeps_existing_grant_until_open_succeeds() {
        let dir = std::env::temp_dir().join(format!("uu-rotation-fresh-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        save(&dir, "gnome", "existing-grant").unwrap();
        let mut fresh = Rotation::new(Some(dir.clone()), "gnome");
        fresh.remember("fresh-grant");
        assert_eq!(fs::read_to_string(dir.join("gnome")).unwrap(), "existing-grant");
        assert!(!dir.join(pending_key("gnome")).exists());
        fresh.finish("gnome", true).unwrap();
        assert_eq!(take(&dir, "gnome").as_deref(), Some("fresh-grant"));
        fs::remove_dir_all(dir).unwrap();
    }
}
