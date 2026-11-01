//! One implementation per desktop. The controller (mod.rs) owns sessions, mode switching, the
//! recovery record and the guard; a backend only changes and restores its desktop.
use super::{record::Journal, Mode};

/// What the controller keeps while privacy is on.
#[derive(Default)]
pub struct Entered {
    /// Picture mode keeps physical outputs on, so uinput's absolute axes span them too: the
    /// captured (virtual) output as fractions (x, y, w, h) of the layout's bounding box.
    pub pointer_region: Option<[f64; 4]>,
}

pub trait Backend: Send + Sync {
    /// Stored in the recovery record to pick the same backend for recovery.
    fn name(&self) -> &'static str;
    /// Why `mode` cannot be served on this desktop.
    fn unavailable(&self, mode: Mode) -> Option<String>;
    /// Hide the local screen and block local input. Save what undoes each step through `journal`
    /// before making it; on an error the controller calls `leave` with the last saved data.
    fn enter(&self, mode: Mode, journal: &mut Journal) -> Result<Entered, String>;
    /// Undo `data` from `enter`. Also used for crash recovery, where no desktop stream exists.
    fn leave(&self, data: &serde_json::Value) -> Result<(), String>;
}

/// The backend for this session, if the desktop has one.
pub fn detect() -> Option<&'static dyn Backend> {
    if cfg!(test) { return None; }
    if super::kde::Kde::detected() { return Some(&super::kde::Kde); }
    None
}
pub fn named(name: &str) -> Option<&'static dyn Backend> {
    match name { super::kde::NAME => Some(&super::kde::Kde), _ => None }
}
