//! One file for every `UU_*` environment knob, injected into the process environment at startup
//! (the readers across the tree keep using `std::env`): a systemd unit `Environment=` line is lost
//! on reinstall and cumbersome for a dozen knobs.
//! `env.conf` sits next to the config file: one `KEY=VALUE` per line, a leading `#` comments the
//! whole line (values may contain `#`), the first `=` splits. Only `UU_[A-Z0-9_]+` keys are
//! accepted; `UU_CONFIG` is refused (it would desync this file from the config it points at); a
//! key already set in the real environment wins (unit/systemd config stays the override layer);
//! credential keys belong to config.json / the real environment and are refused here, so a
//! shareable tuning file cannot carry an account token. Logs and warnings name keys only, never values.

fn path() -> std::path::PathBuf { crate::config::path().with_file_name("env.conf") }

/// Account secrets never belong in this file.
fn credential(key: &str) -> bool { matches!(key, "UU_USER_ID" | "UU_TOKEN" | "UU_CSRF" | "UU_SIGN_KEY" | "UU_SECRET" | "UU_SYSTEM_ID") }

/// Rejected although it looks like a knob: env.conf is found through the default config path,
/// so re-pointing `UU_CONFIG` from inside it would make later `config::path()` reads (and the
/// guest-assist real-environment check) look at a different config.json than this file lives next to.
fn reserved(key: &str) -> bool { key == "UU_CONFIG" }

fn valid_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    bytes.len() > 3 && bytes.starts_with(b"UU_") && bytes[3..].iter().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_')
}

/// Loads `env.conf` into the process environment; returns per-line rejections as key-only text.
pub fn load() -> Vec<String> {
    let p = path();
    let text = match std::fs::read_to_string(&p) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => return vec![format!("cannot read {}: {e}", p.display())],
    };
    apply(&text)
}

fn apply(text: &str) -> Vec<String> {
    let mut applied = 0;
    let mut problems = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        // Only a leading '#' comments a line: values (paths, parameters) may contain '#'.
        if line.is_empty() || line.starts_with('#') { continue; }
        let (key, value) = match line.split_once('=') {
            Some(pair) => (pair.0.trim(), pair.1.trim()),
            None => { problems.push(format!("line {}: missing KEY=VALUE", number + 1)); continue; }
        };
        if !valid_key(key) { problems.push(format!("line {}: {key:?} is not a UU_* setting name", number + 1)); continue; }
        if reserved(key) { problems.push(format!("line {}: {key} is refused here, it would point config.json somewhere else than this file; set it in the real environment", number + 1)); continue; }
        if credential(key) { problems.push(format!("line {}: {key} is a credential, put it in config.json instead", number + 1)); continue; }
        if std::env::var_os(key).is_none() { std::env::set_var(key, value); applied += 1; }
    }
    // /proc/<pid>/environ only shows the exec-time environment, so this count (logged by main) is the proof of injection.
    if applied > 0 || !problems.is_empty() { eprintln!("env.conf: applied {applied} setting(s), {} refused", problems.len()); }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_conf_injects_keys_and_refuses_the_rest() {
        let _guard = crate::config::test_env_lock().lock().unwrap();
        std::env::remove_var("UU_DESKTOP_BACKEND");
        std::env::remove_var("UU_CONFIG");
        std::env::remove_var("UU_ENV_CONF_PROBE");
        std::env::set_var("UU_ENV_CONF_PROBE", "keep");
        let problems = apply("# full-line comment\nPATH=/evil\nUU_CONFIG=/elsewhere/config.json\nUU_TOKEN=secret\nUU_SYSTEM_ID=42\nno_equals\nUU_DESKTOP_BACKEND=none # part of the value\nUU_DESKTOP_BACKEND=x11");
        assert_eq!(std::env::var("UU_DESKTOP_BACKEND").unwrap(), "none # part of the value", "a '#' inside a value survives; the first line wins, the duplicate falls through as already set");
        assert_eq!(std::env::var("UU_ENV_CONF_PROBE").unwrap(), "keep");
        assert_eq!(problems.len(), 5, "bad key, reserved UU_CONFIG, two credentials, missing =");
        assert!(std::env::var_os("UU_TOKEN").is_none() && std::env::var_os("UU_SYSTEM_ID").is_none() && std::env::var_os("UU_CONFIG").is_none() && !std::env::var("PATH").unwrap().starts_with("/evil"));
        std::env::remove_var("UU_DESKTOP_BACKEND");
        std::env::remove_var("UU_ENV_CONF_PROBE");
    }

    #[test]
    fn key_shape_and_credential_rules() {
        assert!(valid_key("UU_FT_BLOCK") && valid_key("UU_A1_B"));
        for bad in ["UU_", "UU_a", "PATH", "UU_A-B", "U_X", ""] {
            assert!(!valid_key(bad), "{bad:?}");
        }
        assert!(credential("UU_CSRF") && credential("UU_SYSTEM_ID") && !credential("UU_DESKTOP_BACKEND"));
        assert!(reserved("UU_CONFIG") && !reserved("UU_DESKTOP_BACKEND"));
    }
}
