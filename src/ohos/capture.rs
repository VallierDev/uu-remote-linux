//! HarmonyOS display metadata. Capture consent and hardware resources belong to the app.
use crate::desktop_backend::DesktopBackend;
use std::sync::Arc;
pub struct ScreenCapture {
    pub backend: DesktopBackend,
    pub init_error: Option<String>,
    pub portal: Option<Arc<crate::portal::PortalSession>>,
    pub display: String,
    pub width: u32,
    pub height: u32,
}
impl ScreenCapture {
    pub fn new() -> Result<Self, String> {
        let (width, height) = crate::ohos::dimensions();
        if width == 0 || height == 0 {
            return Err("Display is not initialized".into());
        }
        Ok(Self {
            backend: DesktopBackend::X11,
            init_error: None,
            portal: None,
            display: "ohos:screen".into(),
            width,
            height,
        })
    }
    pub fn unavailable(error: String) -> Self {
        Self {
            backend: DesktopBackend::X11,
            init_error: Some(error),
            portal: None,
            display: "ohos:screen".into(),
            width: 0,
            height: 0,
        }
    }
    pub fn open_portal(
        _: &std::sync::atomic::AtomicBool,
    ) -> Result<Self, crate::portal::PortalError> {
        Err(crate::portal::PortalError { code: 15 })
    }
    pub fn capture_frame(&self) -> Result<Vec<u8>, String> {
        Err("HarmonyOS capture is connected directly to the hardware encoder".into())
    }
}
pub fn x11_size() -> Option<(u32, u32)> {
    None
}
pub fn x11_last_size() -> Option<(u32, u32)> {
    None
}
pub fn x11_display() -> Option<String> {
    None
}
pub fn keep_display_awake() -> Result<(), String> {
    Ok(())
}
