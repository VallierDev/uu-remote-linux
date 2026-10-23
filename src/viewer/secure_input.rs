//! Compose CONTROL keyboard JSON for scripted typing. Secrets never appear in logs or Debug.
use serde_json::{json, Value};
use std::fmt;
use std::io::{BufRead, Read};
use std::ops::{Deref, DerefMut};
#[cfg(test)]
use std::sync::Mutex;
use std::time::Duration;

/// Serializes tests that mutate process environment.
#[cfg(test)]
pub static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Holds secret text; Debug/Display only show length.
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(value)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn chars_count(&self) -> usize {
        self.0.chars().count()
    }
    /// Drop the last `n` Unicode scalars (fill-without-submit for Windows PIN).
    pub fn omit_last_chars(&mut self, n: usize) -> Result<(), String> {
        let count = self.0.chars().count();
        if n > count {
            return Err("omit-last exceeds secret length".into());
        }
        if n == 0 {
            return Ok(());
        }
        let keep = count - n;
        let truncated: String = self.0.chars().take(keep).collect();
        zeroize_str(&mut self.0);
        self.0 = truncated;
        Ok(())
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret(len={})", self.0.len())
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "*** ({} bytes)", self.0.len())
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        zeroize_str(&mut self.0);
    }
}

fn zeroize_str(s: &mut String) {
    let bytes = unsafe { s.as_bytes_mut() };
    for b in bytes.iter_mut() {
        *b = 0;
    }
    s.clear();
}

/// One CONTROL keyboard action (Windows VK or Mac keycode in `key`).
#[derive(Clone, Debug, PartialEq)]
pub enum KeyAction {
    Press(i32),
    Release(i32),
}

impl KeyAction {
    pub fn to_json(&self) -> Value {
        match self {
            Self::Press(k) => json!({"action":"kbd_press","key":k}),
            Self::Release(k) => json!({"action":"kbd_release","key":k}),
        }
    }
    fn zeroize(&mut self) {
        match self {
            Self::Press(k) | Self::Release(k) => *k = 0,
        }
    }
}

/// Key event list that wipes key codes on drop.
pub struct ActionList(Vec<KeyAction>);

impl std::fmt::Debug for ActionList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ActionList(len={})", self.0.len())
    }
}

impl ActionList {
    pub fn new(actions: Vec<KeyAction>) -> Self {
        Self(actions)
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn as_slice(&self) -> &[KeyAction] {
        &self.0
    }
}

impl Deref for ActionList {
    type Target = [KeyAction];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for ActionList {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for ActionList {
    fn drop(&mut self) {
        for a in &mut self.0 {
            a.zeroize();
        }
        self.0.clear();
    }
}

pub fn parse_delay_ms(raw: Option<&str>) -> Result<Duration, String> {
    let Some(raw) = raw else {
        return Ok(Duration::ZERO);
    };
    let ms: u64 = raw
        .parse()
        .map_err(|_| String::from("delay must be a non-negative integer (milliseconds)"))?;
    if ms > 5_000 {
        return Err("delay must be at most 5000 ms".into());
    }
    Ok(Duration::from_millis(ms))
}

pub fn parse_omit_last(raw: Option<&str>) -> Result<usize, String> {
    let Some(raw) = raw else {
        return Ok(0);
    };
    let n: usize = raw
        .parse()
        .map_err(|_| String::from("omit-last must be a non-negative integer"))?;
    if n > 64 {
        return Err("omit-last must be at most 64".into());
    }
    Ok(n)
}

/// `UU_INPUT_ENV` names another variable that holds the secret; otherwise one stdin line.
/// After reading a named secret variable, that variable is removed from the environment.
pub fn load_secret(stdin: impl BufRead) -> Result<Secret, String> {
    if let Ok(name) = std::env::var("UU_INPUT_ENV") {
        validate_env_name(&name)?;
        let value = std::env::var(&name)
            .map_err(|_| "Environment variable named by UU_INPUT_ENV is not set".to_string())?;
        std::env::remove_var(&name);
        if value.is_empty() {
            return Err("Secret environment variable is empty".into());
        }
        if value.len() > 16 * 1024 {
            return Err("Secret exceeds 16 KiB".into());
        }
        return Ok(Secret::new(value));
    }
    let mut line = String::new();
    stdin
        .take(16 * 1024 + 1)
        .read_to_string(&mut line)
        .map_err(|_| "Cannot read secret from stdin")?;
    if line.len() > 16 * 1024 {
        return Err("Secret exceeds 16 KiB".into());
    }
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    if line.is_empty() {
        return Err("Secret stdin is empty (set UU_INPUT_ENV or pipe text)".into());
    }
    Ok(Secret::new(line))
}

fn validate_env_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 128 {
        return Err("UU_INPUT_ENV must be a non-empty variable name".into());
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err("UU_INPUT_ENV must be an alphanumeric variable name".into());
    }
    Ok(())
}

/// Expand template text into key actions. Brace tokens: `{Enter}`, `{Tab}`, `{Esc}`,
/// `{Backspace}`, `{Delete}`, `{Space}`, `{Ctrl+Alt+Del}`, arrows, `{Home}`, `{End}`, `{F1}`…`{F12}`.
/// Plain characters use a US QWERTY VK/Mac mapping (Shift for uppercase/symbols).
/// Errors never include the offending character or token text — only a 0-based char index.
pub fn expand(text: &str, mac: bool) -> Result<ActionList, String> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    let mut index = 0usize;
    while let Some(c) = chars.next() {
        if c == '{' {
            if chars.peek() == Some(&'{') {
                chars.next();
                push_char(&mut out, '{', mac, index)?;
                index += 1;
                continue;
            }
            let start = index;
            let mut name = String::new();
            let mut closed = false;
            for n in chars.by_ref() {
                if n == '}' {
                    closed = true;
                    break;
                }
                if n == '{' || name.len() > 32 {
                    zeroize_str(&mut name);
                    return Err(format!("Invalid special-key token at index {start}"));
                }
                name.push(n);
            }
            if !closed {
                zeroize_str(&mut name);
                return Err(format!("Unclosed special-key token at index {start}"));
            }
            // Advance index past '{' + name + '}'
            index += 1 + name.chars().count() + 1;
            let result = push_special(&mut out, &name, mac, start);
            zeroize_str(&mut name);
            result?;
            continue;
        }
        if c.is_control() && c != '\t' && c != '\n' && c != '\r' {
            return Err(format!("Control character at index {index}"));
        }
        match c {
            '\n' | '\r' => push_special(&mut out, "Enter", mac, index)?,
            '\t' => push_special(&mut out, "Tab", mac, index)?,
            _ => push_char(&mut out, c, mac, index)?,
        }
        index += 1;
    }
    Ok(ActionList::new(out))
}

fn push_special(
    out: &mut Vec<KeyAction>,
    name: &str,
    mac: bool,
    index: usize,
) -> Result<(), String> {
    let key = match name {
        "Enter" | "Return" => Some(vk_or_mac(0x0d, 36, mac)),
        "Tab" => Some(vk_or_mac(0x09, 48, mac)),
        "Esc" | "Escape" => Some(vk_or_mac(0x1b, 53, mac)),
        "Backspace" => Some(vk_or_mac(0x08, 51, mac)),
        "Delete" | "Del" => Some(vk_or_mac(0x2e, 117, mac)),
        "Space" => Some(vk_or_mac(0x20, 49, mac)),
        "Up" => Some(vk_or_mac(0x26, 126, mac)),
        "Down" => Some(vk_or_mac(0x28, 125, mac)),
        "Left" => Some(vk_or_mac(0x25, 123, mac)),
        "Right" => Some(vk_or_mac(0x27, 124, mac)),
        "Home" => Some(vk_or_mac(0x24, 115, mac)),
        "End" => Some(vk_or_mac(0x23, 119, mac)),
        "Ctrl+Alt+Del" | "CAD" => {
            let ctrl = vk_or_mac(0xa2, 59, mac);
            let alt = vk_or_mac(0xa4, 58, mac);
            let del = vk_or_mac(0x2e, 117, mac);
            out.push(KeyAction::Press(ctrl));
            out.push(KeyAction::Press(alt));
            out.push(KeyAction::Press(del));
            out.push(KeyAction::Release(del));
            out.push(KeyAction::Release(alt));
            out.push(KeyAction::Release(ctrl));
            return Ok(());
        }
        other if other.len() >= 2 && other.as_bytes()[0] == b'F' => {
            let n: u32 = other[1..]
                .parse()
                .map_err(|_| format!("Unknown special key at index {index}"))?;
            if !(1..=12).contains(&n) {
                return Err(format!("Unknown special key at index {index}"));
            }
            let vk = 0x70 + (n as i32 - 1);
            let mac_code = match n {
                1 => 122,
                2 => 120,
                3 => 99,
                4 => 118,
                5 => 96,
                6 => 97,
                7 => 98,
                8 => 100,
                9 => 101,
                10 => 109,
                11 => 103,
                12 => 111,
                _ => unreachable!(),
            };
            Some(vk_or_mac(vk, mac_code, mac))
        }
        _ => return Err(format!("Unknown special key at index {index}")),
    };
    let Some(key) = key else {
        return Ok(());
    };
    out.push(KeyAction::Press(key));
    out.push(KeyAction::Release(key));
    Ok(())
}

fn vk_or_mac(vk: i32, mac: i32, is_mac: bool) -> i32 {
    if is_mac {
        mac
    } else {
        vk
    }
}

fn push_char(out: &mut Vec<KeyAction>, c: char, mac: bool, index: usize) -> Result<(), String> {
    let (vk, shift) = us_qwerty_vk(c)
        .ok_or_else(|| format!("Unsupported character at index {index}"))?;
    let key = if mac {
        mac_from_vk(vk).ok_or_else(|| format!("Unsupported character at index {index}"))?
    } else {
        vk
    };
    let shift_key = vk_or_mac(0xa0, 56, mac);
    if shift {
        out.push(KeyAction::Press(shift_key));
    }
    out.push(KeyAction::Press(key));
    out.push(KeyAction::Release(key));
    if shift {
        out.push(KeyAction::Release(shift_key));
    }
    Ok(())
}

fn us_qwerty_vk(c: char) -> Option<(i32, bool)> {
    match c {
        'a'..='z' => Some((0x41 + (c as i32 - 'a' as i32), false)),
        'A'..='Z' => Some((0x41 + (c as i32 - 'A' as i32), true)),
        '0' => Some((0x30, false)),
        '1'..='9' => Some((0x30 + (c as i32 - '0' as i32), false)),
        ' ' => Some((0x20, false)),
        '-' => Some((0xbd, false)),
        '_' => Some((0xbd, true)),
        '=' => Some((0xbb, false)),
        '+' => Some((0xbb, true)),
        '[' => Some((0xdb, false)),
        '{' => Some((0xdb, true)),
        ']' => Some((0xdd, false)),
        '}' => Some((0xdd, true)),
        '\\' => Some((0xdc, false)),
        '|' => Some((0xdc, true)),
        ';' => Some((0xba, false)),
        ':' => Some((0xba, true)),
        '\'' => Some((0xde, false)),
        '"' => Some((0xde, true)),
        '`' => Some((0xc0, false)),
        '~' => Some((0xc0, true)),
        ',' => Some((0xbc, false)),
        '<' => Some((0xbc, true)),
        '.' => Some((0xbe, false)),
        '>' => Some((0xbe, true)),
        '/' => Some((0xbf, false)),
        '?' => Some((0xbf, true)),
        '!' => Some((0x31, true)),
        '@' => Some((0x32, true)),
        '#' => Some((0x33, true)),
        '$' => Some((0x34, true)),
        '%' => Some((0x35, true)),
        '^' => Some((0x36, true)),
        '&' => Some((0x37, true)),
        '*' => Some((0x38, true)),
        '(' => Some((0x39, true)),
        ')' => Some((0x30, true)),
        _ => None,
    }
}

fn mac_from_vk(vk: i32) -> Option<i32> {
    let scan = match vk {
        0x41..=0x5a => 4 + (vk - 0x41),
        0x31..=0x39 => 30 + (vk - 0x31),
        0x30 => 39,
        0x0d => 40,
        0x1b => 41,
        0x08 => 42,
        0x09 => 43,
        0x20 => 44,
        0xbd => 45,
        0xbb => 46,
        0xdb => 47,
        0xdd => 48,
        0xdc => 49,
        0xba => 51,
        0xde => 52,
        0xc0 => 53,
        0xbc => 54,
        0xbe => 55,
        0xbf => 56,
        0xa0 => 225,
        _ => return None,
    };
    super::input::mac_keycode(scan)
}

pub async fn send_actions(
    send: impl Fn(Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>,
    actions: &[KeyAction],
    delay: Duration,
) -> Result<(), String> {
    for (i, action) in actions.iter().enumerate() {
        send(action.to_json()).await?;
        if !delay.is_zero() && i + 1 != actions.len() {
            tokio::time::sleep(delay).await;
        }
    }
    Ok(())
}

/// Wait for CONTROL echo on an open desktop session, then send key actions.
pub async fn run_session(
    session: &mut super::session::Session,
    actions: &[KeyAction],
    delay: Duration,
) -> Result<(), String> {
    use std::sync::atomic::Ordering;
    let ready = async {
        loop {
            if session.peer.observed.control_open.load(Ordering::Acquire)
                && session.peer.observed.echo_received.load(Ordering::Acquire)
            {
                return Ok::<_, String>(());
            }
            tokio::select! {
                r = session.step() => r?,
                _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(30), ready)
        .await
        .map_err(|_| "CONTROL echo timed out")??;
    for (i, action) in actions.iter().enumerate() {
        session.peer.send_input(&action.to_json()).await?;
        if !delay.is_zero() && i + 1 != actions.len() {
            tokio::time::sleep(delay).await;
        }
    }
    Ok(())
}

/// Dry-run summary: lengths only, never the secret or key codes.
pub fn dry_run_report(secret: &Secret, omit_last: usize, mac: bool) -> Result<String, String> {
    let mut owned = Secret::new(secret.as_str().to_owned());
    owned.omit_last_chars(omit_last)?;
    let actions = expand(owned.as_str(), mac)?;
    Ok(format!(
        "dry-run: chars={} omit_last={} events={} mac={}",
        owned.chars_count(),
        omit_last,
        actions.len(),
        mac
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn secret_debug_and_display_never_leak() {
        let s = Secret::new("hunter2".into());
        let d = format!("{s:?}");
        let p = format!("{s}");
        assert!(d.contains("len=7"));
        assert!(!d.contains("hunter"));
        assert!(!p.contains("hunter"));
        assert!(p.contains("***"));
    }

    #[test]
    fn action_list_debug_shows_len_only() {
        let list = ActionList::new(vec![
            KeyAction::Press(0x41),
            KeyAction::Release(0x41),
            KeyAction::Press(0x42),
            KeyAction::Release(0x42),
        ]);
        let d = format!("{list:?}");
        assert_eq!(d, "ActionList(len=4)");
        assert!(!d.contains("0x41"));
        assert!(!d.contains("Press"));
        assert!(!d.contains("41"));
    }

    #[test]
    fn letters_digits_and_shift_symbols_map_to_windows_vk() {
        let actions = expand("Ab1!", false).unwrap();
        assert_eq!(
            &actions[0..4],
            &[
                KeyAction::Press(0xa0),
                KeyAction::Press(0x41),
                KeyAction::Release(0x41),
                KeyAction::Release(0xa0),
            ]
        );
        assert_eq!(
            &actions[4..6],
            &[KeyAction::Press(0x42), KeyAction::Release(0x42)]
        );
        assert_eq!(
            &actions[6..8],
            &[KeyAction::Press(0x31), KeyAction::Release(0x31)]
        );
        assert_eq!(
            &actions[8..12],
            &[
                KeyAction::Press(0xa0),
                KeyAction::Press(0x31),
                KeyAction::Release(0x31),
                KeyAction::Release(0xa0),
            ]
        );
    }

    #[test]
    fn special_keys_and_ctrl_alt_del() {
        let a = expand("{Enter}{Tab}{Esc}{Backspace}{Ctrl+Alt+Del}", false).unwrap();
        assert!(a
            .windows(2)
            .any(|w| w == [KeyAction::Press(0x0d), KeyAction::Release(0x0d)]));
        let cad: Vec<_> = a.iter().rev().take(6).rev().cloned().collect();
        assert_eq!(
            cad,
            vec![
                KeyAction::Press(0xa2),
                KeyAction::Press(0xa4),
                KeyAction::Press(0x2e),
                KeyAction::Release(0x2e),
                KeyAction::Release(0xa4),
                KeyAction::Release(0xa2),
            ]
        );
    }

    #[test]
    fn unknown_special_key_errors_without_echoing_payload() {
        let err = expand("{NotAKey}", false).unwrap_err();
        assert!(err.contains("Unknown special key at index"));
        assert!(!err.contains("NotAKey"));
        assert!(!err.contains("password"));
    }

    #[test]
    fn unsupported_char_errors_without_codepoint_or_char() {
        let err = expand("a中", false).unwrap_err();
        assert!(err.contains("Unsupported character at index 1"));
        assert!(!err.contains('中'));
        assert!(!err.contains("4E2D"));
        assert!(!err.contains("U+"));
    }

    #[test]
    fn unclosed_token_and_control_char_rejected() {
        assert!(expand("{Enter", false).is_err());
        assert!(expand("a\u{0001}b", false).is_err());
    }

    #[test]
    fn load_from_named_env_and_stdin() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("UU_INPUT_ENV", "UU_SECRET");
        std::env::set_var("UU_SECRET", "pin-value");
        let s = load_secret(Cursor::new(b"")).unwrap();
        assert_eq!(s.as_str(), "pin-value");
        assert!(!format!("{s:?}").contains("pin"));
        assert!(std::env::var("UU_SECRET").is_err());
        std::env::remove_var("UU_INPUT_ENV");
        let s = load_secret(Cursor::new(b"from-stdin\n")).unwrap();
        assert_eq!(s.as_str(), "from-stdin");
    }

    #[test]
    fn invalid_env_name_and_empty_secret() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("UU_INPUT_ENV", "bad-name!");
        assert!(load_secret(Cursor::new(b"")).is_err());
        std::env::remove_var("UU_INPUT_ENV");
        std::env::set_var("UU_INPUT_ENV", "EMPTY_SECRET_TEST");
        std::env::set_var("EMPTY_SECRET_TEST", "");
        assert!(load_secret(Cursor::new(b"")).is_err());
        std::env::remove_var("UU_INPUT_ENV");
        std::env::remove_var("EMPTY_SECRET_TEST");
        assert!(load_secret(Cursor::new(b"\n")).is_err());
    }

    #[test]
    fn delay_and_omit_bounds() {
        assert_eq!(parse_delay_ms(None).unwrap(), Duration::ZERO);
        assert_eq!(
            parse_delay_ms(Some("50")).unwrap(),
            Duration::from_millis(50)
        );
        assert!(parse_delay_ms(Some("-1")).is_err());
        assert!(parse_delay_ms(Some("5001")).is_err());
        assert_eq!(parse_omit_last(None).unwrap(), 0);
        assert_eq!(parse_omit_last(Some("2")).unwrap(), 2);
        assert!(parse_omit_last(Some("65")).is_err());
    }

    #[test]
    fn omit_last_and_dry_run() {
        let mut s = Secret::new("1234567".into());
        s.omit_last_chars(1).unwrap();
        assert_eq!(s.as_str(), "123456");
        let report = dry_run_report(&Secret::new("1234567".into()), 1, false).unwrap();
        assert!(report.contains("chars=6"));
        assert!(report.contains("omit_last=1"));
        assert!(report.contains("events="));
        assert!(report.contains("omit_last=1"));
        assert!(!report.contains("123456"));
        assert!(!report.contains("1234567"));
    }

    #[test]
    fn brace_escape_types_literal_brace() {
        let a = expand("{{a", false).unwrap();
        assert_eq!(
            &a[0..4],
            &[
                KeyAction::Press(0xa0),
                KeyAction::Press(0xdb),
                KeyAction::Release(0xdb),
                KeyAction::Release(0xa0),
            ]
        );
        assert_eq!(
            &a[4..6],
            &[KeyAction::Press(0x41), KeyAction::Release(0x41)]
        );
    }

    #[test]
    fn password_with_enter_suffix() {
        let a = expand("secret{Enter}", false).unwrap();
        assert_eq!(a.last(), Some(&KeyAction::Release(0x0d)));
        assert_eq!(a[a.len() - 2], KeyAction::Press(0x0d));
    }

    #[test]
    fn action_list_zeroizes_on_drop() {
        let mut a = KeyAction::Press(0x41);
        a.zeroize();
        assert_eq!(a, KeyAction::Press(0));
        let list = ActionList::new(vec![KeyAction::Press(7), KeyAction::Release(7)]);
        assert_eq!(list.len(), 2);
        drop(list);
    }

    #[tokio::test]
    async fn send_actions_respects_delay_and_order() {
        use std::sync::{Arc, Mutex};
        let log = Arc::new(Mutex::new(Vec::new()));
        let recorded = log.clone();
        let actions = expand("a{Enter}", false).unwrap();
        let start = std::time::Instant::now();
        send_actions(
            |v| {
                let recorded = recorded.clone();
                Box::pin(async move {
                    recorded.lock().unwrap().push(v);
                    Ok(())
                })
            },
            actions.as_slice(),
            Duration::from_millis(20),
        )
        .await
        .unwrap();
        assert!(start.elapsed() >= Duration::from_millis(50));
        let keys: Vec<_> = log
            .lock()
            .unwrap()
            .iter()
            .map(|v| {
                (
                    v["action"].as_str().unwrap().to_owned(),
                    v["key"].as_i64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            keys,
            vec![
                ("kbd_press".into(), 0x41),
                ("kbd_release".into(), 0x41),
                ("kbd_press".into(), 0x0d),
                ("kbd_release".into(), 0x0d),
            ]
        );
    }
}
