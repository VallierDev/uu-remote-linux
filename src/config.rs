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

/// Reported client version (x-param-vn and User-Agent).
/// Defaults to matching modern official Windows client (4.45.0.4318).
/// Can be overridden via UU_CLIENT_VERSION or config "client_version".
pub fn client_version() -> String {
    std::env::var("UU_CLIENT_VERSION").ok().filter(|v| !v.is_empty())
        .or_else(|| file().ok().flatten().and_then(|f| f.get("client_version")).and_then(|v| v.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "4.45.0.4318".into())
}

/// Reported client version code (x-param-vc).
/// Defaults to matching modern official Windows client (4318).
/// Can be overridden via UU_VERSION_CODE or config "version_code".
pub fn version_code() -> String {
    std::env::var("UU_VERSION_CODE").ok().filter(|v| !v.is_empty())
        .or_else(|| file().ok().flatten().and_then(|f| f.get("version_code")).and_then(|v| v.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "4318".into())
}

/// Blank = unset. Errors never echo the value: it may carry userinfo.
fn parse_base_url(raw: &str, source: &str) -> Result<Option<String>, String> {
    let raw = raw.trim();
    if raw.is_empty() { return Ok(None); }
    let invalid = || format!("{source} must be an http(s) URL with a host and no query or fragment");
    let url = reqwest::Url::parse(raw).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() || url.query().is_some() || url.fragment().is_some() {
        return Err(invalid());
    }
    Ok(Some(url.as_str().trim_end_matches('/').to_owned()))
}

fn custom_base_url_from(env: Option<&std::ffi::OsStr>, cfg: Result<Option<&serde_json::Value>, String>) -> Result<Option<String>, String> {
    if let Some(env) = env {
        let env = env.to_str().ok_or("UU_BASE_URL is not valid UTF-8")?;
        if let Some(url) = parse_base_url(env, "UU_BASE_URL")? { return Ok(Some(url)); }
    }
    match cfg?.and_then(|f| f.get("base_url")) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => parse_base_url(s, "config base_url"),
        Some(_) => Err("config base_url must be a string".into()),
    }
}

/// Explicit cloud API base URL override (`UU_BASE_URL` or config `"base_url"`), without a trailing slash.
/// A set but invalid value is an error, never the default: the signed requests carry the account token,
/// which must not reach NetEase when it belongs to a self-hosted server.
pub fn custom_base_url() -> Result<Option<String>, String> {
    custom_base_url_from(std::env::var_os("UU_BASE_URL").as_deref(), file())
}

/// Cloud API base URL (`UU_BASE_URL` or config `"base_url"`), without a trailing slash.
pub fn base_url() -> Result<String, String> {
    Ok(custom_base_url()?.unwrap_or_else(|| crate::auth::DEFAULT_BASE_URL.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(env: Option<&str>, cfg: Option<&serde_json::Value>) -> Result<Option<String>, String> {
        custom_base_url_from(env.map(std::ffi::OsStr::new), Ok(cfg))
    }

    #[test]
    fn custom_base_url_resolves_precedence_and_sanitizes() {
        let cfg = serde_json::json!({ "base_url": " https://cfg.example.test/// " });
        assert_eq!(resolve(None, None), Ok(None));
        assert_eq!(resolve(None, Some(&cfg)).unwrap().as_deref(), Some("https://cfg.example.test"));
        assert_eq!(resolve(Some(" http://127.0.0.1:8080/ "), Some(&cfg)).unwrap().as_deref(), Some("http://127.0.0.1:8080"));
        assert_eq!(resolve(Some("https://remora.example.test/api/"), None).unwrap().as_deref(), Some("https://remora.example.test/api"));
        assert_eq!(resolve(Some("  "), Some(&cfg)).unwrap().as_deref(), Some("https://cfg.example.test"));
        assert_eq!(resolve(None, Some(&serde_json::json!({ "base_url": null }))), Ok(None));
        assert_eq!(resolve(None, Some(&serde_json::json!({ "base_url": "" }))), Ok(None));
    }

    #[test]
    fn invalid_custom_base_url_fails_closed() {
        let cfg = serde_json::json!({ "base_url": "https://cfg.example.test" });
        for bad in ["remora.example.test", "localhost:8080", "ftp://bad.example", "https://", " /// ", "https://a.test/?x=1", "https://a.test/#x"] {
            assert!(resolve(Some(bad), Some(&cfg)).is_err(), "env {bad:?}");
            assert!(resolve(None, Some(&serde_json::json!({ "base_url": bad }))).is_err(), "config {bad:?}");
        }
        assert!(resolve(None, Some(&serde_json::json!({ "base_url": 8080 }))).is_err());
        assert!(custom_base_url_from(None, Err("invalid config".into())).is_err());
        assert_eq!(custom_base_url_from(Some(std::ffi::OsStr::new("https://env.test")), Err("invalid config".into())), Ok(Some("https://env.test".into())));
    }
}

