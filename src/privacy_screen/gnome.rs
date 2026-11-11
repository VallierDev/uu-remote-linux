//! Experimental GNOME screen-off mode through Mutter's DisplayConfig PowerSaveMode.
use super::{
    backend::{Backend, Entered},
    record::Journal,
    tool, Mode,
};

pub const NAME: &str = "gnome";
const MUTTER: &str = "org.gnome.Mutter.DisplayConfig";
const PATH: &str = "/org/gnome/Mutter/DisplayConfig";
// Mutter DisplayConfig.PowerSaveMode: 0 = on, 3 = off.
const POWER_OFF: i32 = 3;

#[derive(serde::Serialize, serde::Deserialize)]
struct Data {
    power_save_mode: i32,
}

pub fn detected() -> bool {
    let gnome = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .any(|desktop| desktop.eq_ignore_ascii_case("GNOME"));
    cfg!(feature = "portal")
        && std::env::var_os("WAYLAND_DISPLAY").is_some()
        && gnome
        && tool::available("gdbus")
        && tool::available("timeout")
        && crate::desktop_backend::detect().ok()
            == Some(crate::desktop_backend::DesktopBackend::Portal)
}

fn power_save_mode() -> Result<i32, String> {
    let text = tool::gdbus(
        MUTTER,
        PATH,
        "org.freedesktop.DBus.Properties.Get",
        &[MUTTER, "PowerSaveMode"],
    )?;
    parse_power_save_mode(&text).ok_or("invalid Mutter PowerSaveMode response".into())
}

fn parse_power_save_mode(text: &str) -> Option<i32> {
    let value = text.trim().strip_prefix("(<")?.strip_suffix(">,)")?;
    let mode = value.parse::<i32>().ok()?;
    (0..=3).contains(&mode).then_some(mode)
}

fn set_power_save_mode(mode: i32) -> Result<(), String> {
    tool::gdbus(
        MUTTER,
        PATH,
        "org.freedesktop.DBus.Properties.Set",
        &[MUTTER, "PowerSaveMode", &format!("<int32 {mode}>")],
    )?;
    if power_save_mode()? != mode {
        return Err("Mutter did not apply the requested display power mode".into());
    }
    Ok(())
}

pub struct Gnome;

impl Backend for Gnome {
    fn name(&self) -> &'static str {
        NAME
    }

    fn unavailable(&self, mode: Mode) -> Option<String> {
        if mode == Mode::Picture {
            return Some("GNOME privacy picture mode is not supported".into());
        }
        power_save_mode()
            .err()
            .map(|e| format!("GNOME display power control unavailable: {e}"))
    }

    fn enter(&self, mode: Mode, journal: &mut Journal) -> Result<Entered, String> {
        if mode != Mode::ScreenOff {
            return Err("GNOME privacy picture mode is not supported".into());
        }
        let data = Data {
            power_save_mode: power_save_mode()?,
        };
        journal.save(&data)?;
        set_power_save_mode(POWER_OFF)?;
        log::info!("Privacy GNOME: display power mode set to off");
        Ok(Entered::default())
    }

    fn leave(&self, data: &serde_json::Value) -> Result<(), String> {
        let data: Data =
            serde_json::from_value(data.clone()).map_err(|_| "invalid GNOME recovery data")?;
        set_power_save_mode(data.power_save_mode)?;
        log::info!(
            "Privacy GNOME restored: display power mode={}",
            data.power_save_mode
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::parse_power_save_mode;

    #[test]
    fn parses_mutter_power_save_mode() {
        assert_eq!(parse_power_save_mode("(<0>,)"), Some(0));
        assert_eq!(parse_power_save_mode("(<3>,)"), Some(3));
        assert_eq!(parse_power_save_mode("(<4>,)"), None);
        assert_eq!(parse_power_save_mode("('3',)"), None);
        assert_eq!(parse_power_save_mode(""), None);
    }
}
