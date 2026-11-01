//! External desktop tools shared by the backends.
use std::process::{Command, Stdio};

// timeout bounds desktop tools even when their compositor/D-Bus service stops responding.
pub fn run(tool: &str, args: &[String]) -> Result<String, String> {
    let out = Command::new("timeout").args(["--kill-after=1", "5", tool]).args(args)
        .env("LC_ALL", "C").stdin(Stdio::null()).stderr(Stdio::null()).output()
        .map_err(|e| format!("{tool} unavailable: {}", e.kind()))?;
    if !out.status.success() { return Err(format!("{tool} failed: {}", out.status)); }
    String::from_utf8(out.stdout).map_err(|_| "invalid display tool output".into())
}
pub fn available(tool: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|p|
        p.join(tool).metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)))
}
/// One session-bus method call through gdbus; returns its GVariant text.
pub fn gdbus(destination: &str, object: &str, method: &str, args: &[&str]) -> Result<String, String> {
    let mut all: Vec<String> = ["call", "--session", "-d", destination, "-o", object, "-m", method].map(String::from).into();
    all.extend(args.iter().map(|a| a.to_string()));
    run("gdbus", &all)
}
/// `'key': <value>` from gdbus's text for an a{sv} (strings unquoted).
pub fn variant<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let start = text.find(&format!("'{key}': <"))? + key.len() + 5;
    let rest = &text[start..];
    if let Some(quoted) = rest.strip_prefix('\'') { return quoted.split('\'').next(); }
    rest.split('>').next()
}
/// Polls `done` for up to three seconds: desktop tools return before the compositor applies a change.
pub fn wait_until(mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..30 {
        if done() { return true; }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_properties_from_gdbus_text() {
        let text = "({'alphaNumericKeyboard': <true>, 'enabled': <true>, 'name': <'AT Translated Set 2 keyboard'>, 'pointer': <false>},)";
        assert_eq!(variant(text, "name"), Some("AT Translated Set 2 keyboard"));
        assert_eq!(variant(text, "enabled"), Some("true"));
        assert_eq!(variant(text, "pointer"), Some("false"));
        assert_eq!(variant(text, "touch"), None);
    }
}
