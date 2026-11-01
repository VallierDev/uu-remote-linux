/// What the viewer asks the local screen to show while it controls the desktop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// A picture covers the physical screens (the official viewer's privacy/wallpaper mode).
    Picture,
    /// The physical screens are off.
    ScreenOff,
}

/// `privacy_type` as the official viewer numbers it (seen live): 1 off, 2 picture, 3 screen off.
pub const TYPE_OFF: u8 = 1;
impl Mode {
    pub fn from_type(value: u64) -> Option<Self> {
        match value { 2 => Some(Self::Picture), 3 => Some(Self::ScreenOff), _ => None }
    }
    pub fn wire_type(self) -> u8 { match self { Self::Picture => 2, Self::ScreenOff => 3 } }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wire_types_round_trip() {
        for mode in [Mode::Picture, Mode::ScreenOff] { assert_eq!(Mode::from_type(mode.wire_type().into()), Some(mode)); }
        assert_eq!(Mode::from_type(1), None);
        assert_eq!(Mode::from_type(0), None);
    }
}
