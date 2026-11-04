//! Local input devices through KWin's InputDevice D-Bus interface (sysName `eventN`). KWin saves
//! the enabled flag, which is why the controller runs the guard.
//!
//! `eventN` comes from udev enumeration and can change between boots, while a recovery record may
//! be replayed after a reboot. Records therefore store the name/product/vendor KWin reports next
//! to the sysName, and a restore matches the current device by that identity.
use super::super::tool::{gdbus, variant};
use serde::{Deserialize, Serialize};

fn call(object: &str, method: &str, args: &[&str]) -> Result<String, String> { gdbus("org.kde.KWin", object, method, args) }

/// One blocked device. `Legacy` is a record written before identities were stored.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Device {
    Named { sys: String, name: String, product: u32, vendor: u32 },
    Legacy(String),
}

impl Device {
    pub fn sys(&self) -> &str {
        match self { Self::Named { sys, .. } | Self::Legacy(sys) => sys }
    }
    fn name(&self) -> &str {
        match self { Self::Named { name, .. } => name, Self::Legacy(sys) => sys }
    }
    fn identity(&self) -> Option<(&str, u32, u32)> {
        match self { Self::Named { name, product, vendor, .. } => Some((name, *product, *vendor)), Self::Legacy(_) => None }
    }
}

/// `'key': <uint32 16500>` carries a type prefix; strings and booleans do not.
fn number(text: &str, key: &str) -> Option<u32> { variant(text, key)?.split_whitespace().last()?.parse().ok() }

#[derive(Clone, Copy, Default)]
struct Flags { keyboard: bool, pointer: bool, touch: bool, switch: bool, enabled: bool }

/// Every device KWin lists, blocked or not: restore must find a device that is still disabled.
fn enumerate() -> Result<Vec<(Device, Flags)>, String> {
    let list = call("/org/kde/KWin/InputDevice", "org.freedesktop.DBus.Properties.Get", &["org.kde.KWin.InputDeviceManager", "devicesSysNames"])?;
    let mut devices = Vec::new();
    for sys in list.split('\'').skip(1).step_by(2).filter(|n| n.strip_prefix("event").is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))) {
        let Ok(text) = call(&format!("/org/kde/KWin/InputDevice/{sys}"), "org.freedesktop.DBus.Properties.GetAll", &["org.kde.KWin.InputDevice"]) else { continue };
        let yes = |key| variant(&text, key) == Some("true");
        devices.push((Device::Named {
            sys: sys.to_owned(),
            name: variant(&text, "name").unwrap_or_default().to_owned(),
            product: number(&text, "product").unwrap_or(0),
            vendor: number(&text, "vendor").unwrap_or(0),
        }, Flags { keyboard: yes("keyboard"), pointer: yes("pointer"), touch: yes("touch"), switch: yes("switchDevice"), enabled: yes("enabled") }));
    }
    Ok(devices)
}

/// Keyboards, pointers and touch to block: everything except our own uinput devices, the power
/// button and switches (lid). Software devices fed by local hardware (gesture daemons, ydotoold)
/// are blocked too.
pub fn local() -> Result<Vec<Device>, String> {
    Ok(enumerate()?.into_iter().filter(|(d, f)| {
        (f.keyboard || f.pointer || f.touch) && !f.switch && f.enabled
            && !d.name().starts_with("uu-remote-linux") && d.name() != "Power Button"
    }).map(|(d, _)| d).collect())
}

/// The current device matching `wanted`: same name/product/vendor, or the exact sysName for a
/// legacy record written before identities were stored.
fn current<'a>(devices: &'a [Device], wanted: &Device) -> Option<&'a Device> {
    match wanted.identity() {
        Some(identity) => devices.iter().find(|d| d.identity() == Some(identity)),
        None => devices.iter().find(|d| d.sys() == wanted.sys()),
    }
}

fn set_sys<'a>(devices: impl Iterator<Item = &'a str>, enabled: bool) -> Result<(), String> {
    let value = if enabled { "<true>" } else { "<false>" };
    let mut failure = None;
    for sys in devices {
        if let Err(e) = call(&format!("/org/kde/KWin/InputDevice/{sys}"), "org.freedesktop.DBus.Properties.Set", &["org.kde.KWin.InputDevice", "enabled", value]) {
            failure = Some(e);
        }
    }
    failure.map_or(Ok(()), Err)
}

pub fn set(devices: &[Device], enabled: bool) -> Result<(), String> {
    set_sys(devices.iter().map(Device::sys), enabled)
}

/// Re-enables the recorded devices. A device that no longer exists is skipped: its `eventN` may
/// belong to a different device after a reboot, and failing the whole restore would keep the
/// recovery record forever. A device that exists but cannot be enabled still fails the restore,
/// so the record and the guard stay for the next attempt.
pub fn restore(devices: &[Device]) -> Result<(), String> {
    if devices.is_empty() { return Ok(()); }
    let now: Vec<Device> = enumerate()?.into_iter().map(|(d, _)| d).collect();
    let mut failure = None;
    for wanted in devices {
        let Some(found) = current(&now, wanted) else {
            log::warn!("Privacy restore: {} ({}) is gone; skipping", wanted.sys(), wanted.name());
            continue;
        };
        if let Err(e) = set_sys(std::iter::once(found.sys()), true) { failure = Some(e); }
    }
    failure.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(sys: &str, name: &str, product: u32, vendor: u32) -> Device {
        Device::Named { sys: sys.into(), name: name.into(), product, vendor }
    }

    #[test]
    fn parses_numbered_variants() {
        let text = "({'name': <'Logitech G304'>, 'product': <uint32 16500>, 'vendor': <uint32 1133>},)";
        assert_eq!(number(text, "product"), Some(16500));
        assert_eq!(number(text, "vendor"), Some(1133));
        assert_eq!(number(text, "missing"), None);
    }

    #[test]
    fn reads_legacy_and_named_records() {
        let legacy: Vec<Device> = serde_json::from_str(r#"["event5","event6"]"#).unwrap();
        assert!(matches!(&legacy[0], Device::Legacy(sys) if sys == "event5"));
        let named: Device = serde_json::from_str(r#"{"sys":"event12","name":"Logitech G304","product":16500,"vendor":1133}"#).unwrap();
        assert_eq!(named.identity(), Some(("Logitech G304", 16500, 1133)));
    }

    #[test]
    fn matches_a_renumbered_device_by_identity() {
        let mouse = named("event12", "Logitech G304", 16500, 1133);
        let keyboard = named("event3", "AT Translated Set 2 keyboard", 1, 1);
        let recorded = [named("event14", "Logitech G304", 16500, 1133)];
        let now = [keyboard, mouse];
        assert_eq!(super::current(&now, &recorded[0]).map(Device::sys), Some("event12"));
    }

    #[test]
    fn a_missing_device_matches_nothing() {
        let recorded = [named("event14", "Logitech G304", 16500, 1133)];
        let now = [named("event3", "AT Translated Set 2 keyboard", 1, 1)];
        assert!(super::current(&now, &recorded[0]).is_none());
    }

    #[test]
    fn a_legacy_record_matches_the_exact_sysname() {
        let recorded = Device::Legacy("event5".into());
        let now = [named("event5", "AT Translated Set 2 keyboard", 1, 1)];
        assert_eq!(super::current(&now, &recorded).map(Device::sys), Some("event5"));
    }
}
