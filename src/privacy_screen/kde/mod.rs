//! KDE Plasma Wayland: capture and input move to a KWin virtual output through the portal
//! (ScreenCast source type VIRTUAL), then the physical outputs are disabled with kscreen-doctor,
//! so remote input cannot wake them (KWin treats injected input as user activity and undoes DPMS).
//! Same order as KRdp's virtual-monitor "replace" policy and Sunshine's KDE setup: snapshot the
//! physical outputs before the virtual one exists (KWin applies stored configs on creation),
//! re-enable them before the virtual output goes away, and never disable the virtual output
//! itself (KWin bug 527747 persists that and breaks later virtual shares).
//! Picture mode then turns the physical outputs back on as secondary ones and covers them.
mod input;
mod kwin;
mod outputs;

use super::{backend::{Backend, Entered}, record::Journal, tool, Mode};

pub const NAME: &str = "kde";
pub struct Kde;

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Data { outputs: Vec<String>, #[serde(default)] devices: Vec<input::Device> }

impl Kde {
    pub fn detected() -> bool {
        let kde = std::env::var_os("WAYLAND_DISPLAY").is_some()
            && std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().split(':').any(|s| s.eq_ignore_ascii_case("KDE"));
        cfg!(feature = "portal") && kde && tool::available("timeout") && tool::available("kscreen-doctor")
            && crate::desktop_backend::detect().ok() == Some(crate::desktop_backend::DesktopBackend::Portal)
    }
}
fn block_input() -> bool { std::env::var("UU_PRIVACY_BLOCK_INPUT").as_deref() != Ok("0") }
/// Plasma's desktop wallpaper image, if it is a plain file.
fn wallpaper() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
    let config = std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from).unwrap_or(home.join(".config"));
    let text = std::fs::read_to_string(config.join("plasma-org.kde.plasma.desktop-appletsrc")).ok()?;
    text.lines().find_map(|l| l.strip_prefix("Image=")).map(|v| std::path::PathBuf::from(v.trim().trim_start_matches("file://")))
        .filter(|p| p.is_file())
}
/// Windows moved to the virtual output when the physical outputs went away. Turn those back on as
/// secondary outputs, keep the windows on the virtual one, and cover the physical ones.
fn cover(data: &Data) -> Result<[f64; 4], String> {
    let virtual_name = outputs::layout()?.into_iter().map(|o| o.name).find(|n| !data.outputs.contains(n)).ok_or("virtual output not found")?;
    outputs::set(&data.outputs, true).map_err(|e| format!("enable outputs: {e}"))?;
    if let Err(e) = outputs::set_primary(&virtual_name) { log::warn!("Privacy virtual output priority: {e}"); }
    kwin::move_windows_to(&virtual_name).map_err(|e| format!("move windows: {e}"))?;
    let layout = outputs::layout()?;
    let physical: Vec<[f64; 4]> = layout.iter().filter(|o| data.outputs.contains(&o.name)).map(|o| o.rect).collect();
    if physical.len() != data.outputs.len() { return Err("physical outputs did not come back".into()); }
    let region = outputs::region(&layout, &virtual_name).ok_or("virtual output geometry")?;
    super::cover::show(wallpaper().as_deref(), &physical)?;
    // The cover is a plain Wayland toplevel: GTK's skip-taskbar hint is X11-only, so without this
    // the task manager shows it as an unidentified window with the theme's generic "wayland" icon.
    // KWin lists a freshly mapped window asynchronously, so one more pass catches a late cover.
    for pass in 0..2 {
        if let Err(e) = kwin::hide_cover_from_taskbar() { log::warn!("Privacy cover taskbar: {e}"); break; }
        if pass == 0 { std::thread::sleep(std::time::Duration::from_millis(200)); }
    }
    Ok(region)
}

impl Backend for Kde {
    fn name(&self) -> &'static str { NAME }
    fn unavailable(&self, mode: Mode) -> Option<String> {
        if !tool::available("gdbus") { return Some("privacy screen needs gdbus".into()); }
        // Checked before any output or device changes: on the virtual share KDE drops portal input,
        // and local devices get blocked, so without uinput neither side could operate.
        if !crate::input::uinput_ready() { return Some("privacy screen needs uinput input (UU_INPUT_BACKEND=native or /dev/uinput unavailable)".into()); }
        (mode == Mode::Picture && !super::cover::supported()).then(|| "picture mode needs GTK".into())
    }
    fn enter(&self, mode: Mode, journal: &mut Journal) -> Result<Entered, String> {
        let mut data = Data { outputs: outputs::physical()?, devices: Vec::new() };
        journal.save(&data)?;
        crate::portal::set_virtual_active(true).map_err(|e| format!("virtual output: {e}"))?;
        outputs::set(&data.outputs, false).map_err(|e| format!("disable outputs: {e}"))?;
        let mut entered = Entered::default();
        if mode == Mode::Picture { entered.pointer_region = Some(cover(&data)?); }
        if block_input() {
            data.devices = input::local().map_err(|e| format!("input devices: {e}"))?;
            journal.save(&data)?;
            input::set(&data.devices, false).map_err(|e| format!("block input: {e}"))?;
        }
        log::info!("Privacy KDE: outputs={} input_devices={}", data.outputs.len(), data.devices.len());
        Ok(entered)
    }
    fn leave(&self, data: &serde_json::Value) -> Result<(), String> {
        let data: Data = serde_json::from_value(data.clone()).map_err(|_| "invalid KDE recovery data")?;
        // KWin persists the device flag: a failure is returned (after restoring the outputs) so the
        // controller keeps the recovery record and the guard for the next attempt. A device that
        // no longer exists is skipped, so a record replayed after a reboot cannot wedge recovery.
        let devices = input::restore(&data.devices).map_err(|e| format!("enable input devices: {e}"));
        // Physical outputs first: switching back opens a monitor share, which needs them enabled.
        // The wait also matters: a share opened before KWin applies the mode got the virtual
        // output's geometry (seen live: 1920x1080 instead of the panel's 3072x1920).
        if let Err(e) = outputs::set(&data.outputs, true) {
            if !outputs::physical().is_ok_and(|now| data.outputs.iter().all(|o| now.contains(o))) { return Err(format!("enable outputs: {e}")); }
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        crate::portal::set_virtual_active(false).map_err(|e| format!("restore capture source: {e}"))?;
        devices?;
        log::info!("Privacy KDE restored: outputs={} input_devices={}", data.outputs.len(), data.devices.len());
        Ok(())
    }
}
