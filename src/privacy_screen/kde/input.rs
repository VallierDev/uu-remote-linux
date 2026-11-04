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
/// The flag says whether a property read failed. A restore re-enables the readable devices, then
/// returns the flag as an error so its record and the guard stay for another attempt; blocking
/// skips such a device, which is all it can do.
fn enumerate() -> Result<(Vec<(Device, Flags)>, bool), String> {
    let list = call("/org/kde/KWin/InputDevice", "org.freedesktop.DBus.Properties.Get", &["org.kde.KWin.InputDeviceManager", "devicesSysNames"])?;
    let mut devices = Vec::new();
    let mut incomplete = false;
    for sys in list.split('\'').skip(1).step_by(2).filter(|n| n.strip_prefix("event").is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))) {
        let text = match call(&format!("/org/kde/KWin/InputDevice/{sys}"), "org.freedesktop.DBus.Properties.GetAll", &["org.kde.KWin.InputDevice"]) {
            Ok(text) => text,
            Err(_) => { incomplete = true; continue; }
        };
        let yes = |key| variant(&text, key) == Some("true");
        devices.push((Device::Named {
            sys: sys.to_owned(),
            name: variant(&text, "name").unwrap_or_default().to_owned(),
            product: number(&text, "product").unwrap_or(0),
            vendor: number(&text, "vendor").unwrap_or(0),
        }, Flags { keyboard: yes("keyboard"), pointer: yes("pointer"), touch: yes("touch"), switch: yes("switchDevice"), enabled: yes("enabled") }));
    }
    Ok((devices, incomplete))
}

/// Keyboards, pointers and touch to block: everything except our own uinput devices, the power
/// button and switches (lid). Software devices fed by local hardware (gesture daemons, ydotoold)
/// are blocked too.
pub fn local() -> Result<Vec<Device>, String> {
    Ok(enumerate()?.0.into_iter().filter(|(d, f)| {
        (f.keyboard || f.pointer || f.touch) && !f.switch && f.enabled
            && !d.name().starts_with("uu-remote-linux") && d.name() != "Power Button"
    }).map(|(d, _)| d).collect())
}

/// Every current device matching `wanted`: same name/product/vendor, or the exact sysName for a
/// legacy record. Several devices can share one identity (two identical keyboards, or one receiver
/// exposing several event nodes under the same name), so callers must handle every match.
fn matching<'a>(devices: &'a [Device], wanted: &Device) -> impl Iterator<Item = &'a Device> + 'a {
    let identity = wanted.identity().map(|(name, product, vendor)| (name.to_owned(), product, vendor));
    let sys = wanted.sys().to_owned();
    devices.iter().filter(move |d| match (&identity, d.identity()) {
        (Some((name, product, vendor)), Some((current, p, v))) => current == name && p == *product && v == *vendor,
        _ => d.sys() == sys,
    })
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

/// Current sysNames to re-enable and how many record entries have no current match.
fn restore_targets<'a>(now: &'a [Device], recorded: &[Device]) -> (Vec<&'a str>, usize) {
    let mut targets: Vec<&str> = Vec::new();
    let mut gone = 0;
    for wanted in recorded {
        let mut found = false;
        for current in matching(now, wanted) {
            found = true;
            if !targets.contains(&current.sys()) { targets.push(current.sys()); }
        }
        if !found { gone += 1; }
    }
    (targets, gone)
}

/// Re-enables every current device matching a record entry, deduplicated by sysName: two record
/// entries can describe the same device, and one identity can cover several devices. A recorded
/// device that no longer exists is skipped: its `eventN` may belong to a different device after a
/// reboot, and failing the whole restore would keep the recovery record forever. A device that
/// exists but cannot be enabled still fails the restore, so the record and the guard stay for the
/// next attempt.
pub fn restore(devices: &[Device]) -> Result<(), String> {
    if devices.is_empty() { return Ok(()); }
    let (found, incomplete) = enumerate()?;
    let now: Vec<Device> = found.into_iter().map(|(d, _)| d).collect();
    let (targets, gone) = restore_targets(&now, devices);
    // Counts only: device names and sysNames are not logged (AGENTS.md).
    if gone > 0 { log::warn!("Privacy restore: {gone} recorded device(s) are gone; skipping"); }
    // Re-enable the readable devices first: one unreadable device must not keep every other one
    // off. The error afterwards keeps the record and the guard for another attempt.
    set_sys(targets.into_iter(), true)?;
    if incomplete { return Err("input device properties could not be read; retrying later".into()); }
    Ok(())
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
        assert_eq!(super::restore_targets(&now, &recorded), (vec!["event12"], 0));
    }

    #[test]
    fn enables_every_device_sharing_an_identity_once() {
        let first = named("event12", "Logitech G304", 16500, 1133);
        let second = named("event15", "Logitech G304", 16500, 1133);
        let now = [first, second];
        // Two record entries for one identity, as a receiver exposing several event nodes produces.
        let recorded = [named("event14", "Logitech G304", 16500, 1133), named("event16", "Logitech G304", 16500, 1133)];
        assert_eq!(super::restore_targets(&now, &recorded), (vec!["event12", "event15"], 0));
    }

    #[test]
    fn a_missing_device_counts_as_gone() {
        let recorded = [named("event14", "Logitech G304", 16500, 1133)];
        let now = [named("event3", "AT Translated Set 2 keyboard", 1, 1)];
        assert_eq!(super::restore_targets(&now, &recorded), (Vec::new(), 1));
    }

    #[test]
    fn a_legacy_record_matches_the_exact_sysname() {
        let recorded = [Device::Legacy("event5".into())];
        let now = [named("event5", "AT Translated Set 2 keyboard", 1, 1)];
        assert_eq!(super::restore_targets(&now, &recorded), (vec!["event5"], 0));
    }
}
