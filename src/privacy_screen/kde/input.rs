//! Local input devices through KWin's InputDevice D-Bus interface (sysName `eventN`). KWin saves
//! the enabled flag, which is why the controller runs the guard.
use super::super::tool::{gdbus, variant};

fn call(object: &str, method: &str, args: &[&str]) -> Result<String, String> { gdbus("org.kde.KWin", object, method, args) }
/// Keyboards, pointers and touch to block: everything except our own uinput devices, the power
/// button and switches (lid). Software devices fed by local hardware (gesture daemons, ydotoold)
/// are blocked too.
pub fn local() -> Result<Vec<String>, String> {
    let list = call("/org/kde/KWin/InputDevice", "org.freedesktop.DBus.Properties.Get", &["org.kde.KWin.InputDeviceManager", "devicesSysNames"])?;
    let mut blocked = Vec::new();
    for sys in list.split('\'').skip(1).step_by(2).filter(|n| n.strip_prefix("event").is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))) {
        let Ok(text) = call(&format!("/org/kde/KWin/InputDevice/{sys}"), "org.freedesktop.DBus.Properties.GetAll", &["org.kde.KWin.InputDevice"]) else { continue };
        let yes = |key| variant(&text, key) == Some("true");
        let name = variant(&text, "name").unwrap_or_default();
        if (yes("keyboard") || yes("pointer") || yes("touch")) && !yes("switchDevice") && yes("enabled")
            && !name.starts_with("uu-remote-linux") && name != "Power Button" { blocked.push(sys.to_owned()); }
    }
    Ok(blocked)
}
pub fn set(devices: &[String], enabled: bool) -> Result<(), String> {
    let value = if enabled { "<true>" } else { "<false>" };
    let mut failure = None;
    for sys in devices {
        if let Err(e) = call(&format!("/org/kde/KWin/InputDevice/{sys}"), "org.freedesktop.DBus.Properties.Set", &["org.kde.KWin.InputDevice", "enabled", value]) {
            failure = Some(e);
        }
    }
    failure.map_or(Ok(()), Err)
}
