//! UI language: Chinese (the official clients' wording) or English.
//! `UU_LANG=zh|en`, else the choice saved from the settings window (`language` next to config.json),
//! else the locale (`LC_ALL` / `LC_MESSAGES` / `LANG` starting with `zh` → Chinese, anything else → English).
use std::sync::OnceLock;

static ENGLISH: OnceLock<bool> = OnceLock::new();

fn parse(value: &str) -> Option<bool> {
    let value = value.trim().to_ascii_lowercase();
    if value.starts_with("zh") { Some(false) } else if value.starts_with("en") { Some(true) } else { None }
}

fn saved_path() -> std::path::PathBuf {
    crate::config::path().parent().map_or_else(|| "language".into(), |dir| dir.join("language"))
}

fn detect() -> bool {
    if let Some(english) = std::env::var("UU_LANG").ok().as_deref().and_then(parse) { return english; }
    if let Some(english) = std::fs::read_to_string(saved_path()).ok().as_deref().and_then(parse) { return english; }
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"].iter()
        .find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty()));
    !locale.is_some_and(|v| v.to_ascii_lowercase().starts_with("zh"))
}

pub fn english() -> bool { *ENGLISH.get_or_init(detect) }

/// Pick the text for the current language.
pub fn s(zh: &'static str, en: &'static str) -> &'static str { if english() { en } else { zh } }

/// Saves the settings-window choice; it applies on the next start.
pub fn save(english: bool) -> std::io::Result<()> {
    let path = saved_path();
    if let Some(dir) = path.parent() { std::fs::create_dir_all(dir)?; }
    std::fs::write(path, if english { "en\n" } else { "zh\n" })
}

/// `tr!("连接失败：{e}", "Connection failed: {e}")` formats the string for the current language.
#[macro_export]
macro_rules! tr {
    ($zh:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        if $crate::i18n::english() { format!($en $(, $arg)*) } else { format!($zh $(, $arg)*) }
    };
}

// The GTK windows (c_engine/i18n.h) ask the same question so both halves always agree.
#[no_mangle] pub extern "C" fn uu_i18n_english() -> std::os::raw::c_int { english().into() }
#[no_mangle] pub extern "C" fn uu_i18n_save(english: std::os::raw::c_int) -> std::os::raw::c_int {
    save(english != 0).is_ok().into()
}

#[cfg(test)] mod tests {
    #[test] fn language_values_are_parsed(){
        assert_eq!(super::parse("zh_CN.UTF-8"),Some(false));
        assert_eq!(super::parse("EN\n"),Some(true));
        assert_eq!(super::parse("sr_RS"),None);
    }
}
