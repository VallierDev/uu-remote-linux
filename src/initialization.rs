//! Official CONTROL initialization protobuf payloads recovered from the Mac app.
//! See analysis/protocol-fix-20261013/control/screen-state-field-maps.json.
//! The caller supplies measured Linux geometry, track binding and permissions.
//! No hardware capabilities or successful actions are inferred here.

fn varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 128 {
        out.push((value as u8 & 127) | 128);
        value >>= 7;
    }
    out.push(value as u8);
}
fn integer(number: u32, value: i32, out: &mut Vec<u8>) {
    if value != 0 {
        varint(u64::from(number) << 3, out);
        varint(value as i64 as u64, out);
    }
}
fn boolean(number: u32, value: bool, out: &mut Vec<u8>) {
    if value { integer(number, 1, out); }
}
fn bytes(number: u32, value: &[u8], out: &mut Vec<u8>) {
    varint((u64::from(number) << 3) | 2, out);
    varint(value.len() as u64, out);
    out.extend_from_slice(value);
}
fn text(number: u32, value: &str, out: &mut Vec<u8>) {
    if !value.is_empty() { bytes(number, value.as_bytes(), out); }
}
pub fn envelope(seq: u64, timestamp: u64, which: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    if seq != 0 { out.push(8); varint(seq, &mut out); }
    if timestamp != 0 { out.push(16); varint(timestamp, &mut out); }
    bytes(which, payload, &mut out);
    out
}

#[derive(Debug, Clone, Copy)]
pub struct WinRect {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
    pub pixel_width: i32,
    pub pixel_height: i32,
}
impl WinRect {
    fn encode(self) -> Result<Vec<u8>, String> {
        if self.width <= 0 || self.height <= 0 || self.pixel_width <= 0 || self.pixel_height <= 0 {
            return Err("Screen geometry must have positive logical and pixel dimensions".into());
        }
        let mut out = Vec::new();
        for (field, value) in [(1,self.left),(2,self.top),(3,self.width),(4,self.height),(5,self.pixel_width),(6,self.pixel_height)] {
            integer(field, value, &mut out);
        }
        Ok(out)
    }
}

pub struct Screen<'a> {
    pub id: i32,
    pub fps: i32,
    pub resolution: WinRect,
    pub primary: bool,
    /// Pixel/logical scale of the advertised coordinate space (1.0 for capture-setting v4).
    pub dpr: f64,
    pub display_name: &'a str,
    pub video_track_index: i32,
    /// Official enum: 0 physical, 1 virtual, 2 super-screen.
    pub screen_type: i32,
    /// Official enum: 0 unknown, 1 built-in, 2 not built-in.
    pub builtin_screen_type: i32,
    /// Further selectable (scaled) stream sizes besides `resolution`, for 其他分辨率.
    pub scaled: &'a [(i32, i32)],
    /// Size currently streamed when it differs from `resolution`.
    pub current: Option<(i32, i32)>,
}
impl Screen<'_> {
    fn encode(&self) -> Result<Vec<u8>, String> {
        if self.id < 0 || self.fps <= 0 || self.video_track_index < 0 {
            return Err("Screen id/track must be nonnegative and fps positive".into());
        }
        if !self.dpr.is_finite() || self.dpr <= 0.0 {
            return Err("Screen dpr must be positive and finite".into());
        }
        if !(0..=2).contains(&self.screen_type) || !(0..=2).contains(&self.builtin_screen_type) {
            return Err("Unknown Screen enum value".into());
        }
        let rect = self.resolution.encode()?;
        let mut out = Vec::new();
        integer(1, self.id, &mut out);
        integer(2, self.fps, &mut out);
        bytes(3, &rect, &mut out); // the measured native resolution first
        let sized = |(width, height): (i32, i32)| WinRect { width, height, pixel_width: width, pixel_height: height, ..self.resolution }.encode();
        for size in self.scaled { bytes(3, &sized(*size)?, &mut out); }
        let current = match self.current { Some(size) => sized(size)?, None => rect.clone() };
        bytes(4, &current, &mut out); // current_resolution
        integer(5, self.screen_type, &mut out);
        bytes(6, &rect, &mut out); // init_resolution
        boolean(7, self.primary, &mut out);
        out.push(65); // field 8 Double: fixed64, not varint
        out.extend_from_slice(&self.dpr.to_le_bytes());
        // Field 9 dpi_scale omitted: no unmeasured monitor-DPI claim.
        text(10, self.display_name, &mut out);
        // Field 11 resolution_type omitted: default UNKNOWN is truthful.
        integer(12, self.video_track_index, &mut out);
        integer(13, self.builtin_screen_type, &mut out);
        Ok(out)
    }
}

/// Message.which field 7 ScreenSources, including the selected screen id.
pub fn encode_screen_sources(seq: u64, timestamp: u64, screens: &[Screen<'_>], current_id: i32) -> Result<Vec<u8>, String> {
    if screens.is_empty() || !screens.iter().any(|s| s.id == current_id) {
        return Err("Selected screen must exist in the measured inventory".into());
    }
    let mut ids = std::collections::HashSet::new();
    let mut tracks = std::collections::HashSet::new();
    let mut payload = Vec::new();
    for screen in screens {
        if !ids.insert(screen.id) || !tracks.insert(screen.video_track_index) {
            return Err("Screen ids and video track indices must be unique".into());
        }
        bytes(1, &screen.encode()?, &mut payload);
    }
    integer(2, current_id, &mut payload);
    Ok(envelope(seq, timestamp, 7, &payload))
}

/// Message.which field 8 CaptureChange. Enum: desktop=0, window=1,
/// MuMu=2, hook=3, none=99. Caller confirms the active capture before emitting.
pub fn encode_capture_change(seq: u64, timestamp: u64, capture_type: i32, type_value: i32, description: &str) -> Result<Vec<u8>, String> {
    if !matches!(capture_type, 0 | 1 | 2 | 3 | 99) {
        return Err("Unknown CaptureChange type".into());
    }
    let mut payload = Vec::new();
    integer(1, capture_type, &mut payload);
    integer(2, type_value, &mut payload);
    text(3, description, &mut payload);
    Ok(envelope(seq, timestamp, 8, &payload))
}

/// Minimal initialization for the caller's measured single Linux framebuffer.
/// Current deployment binding is source_id=0, video_track_index=0. Dimensions
/// describe the source desktop, not the downscaled H264 bitstream. No monitor
/// DPI, audio, input permission, or optional feature versions are advertised.
/// Invalid dimensions/fps produce no messages, never invented fallback values.
pub fn initial_messages(width: u32, height: u32, fps: u32) -> Vec<(u32, Vec<u8>)> {
    let (Ok(width), Ok(height), Ok(fps)) = (i32::try_from(width), i32::try_from(height), i32::try_from(fps)) else { return Vec::new(); };
    let screen = Screen {
        id: 0, fps,
        resolution: WinRect { left: 0, top: 0, width, height, pixel_width: width, pixel_height: height },
        primary: true, dpr: 1.0, display_name: "", video_track_index: 0,
        screen_type: 0, builtin_screen_type: 0, scaled: &[], current: None,
    };
    let Ok(encoded_screen) = screen.encode() else { return Vec::new(); };
    let mut inventory = Vec::new();
    bytes(1, &encoded_screen, &mut inventory);
    // current_screen_id=0 is an ordinary scalar default. CaptureChange's
    // desktop=0/type_value=0 defaults are still represented by the which tag.
    vec![(7, inventory), (8, Vec::new())]
}

#[derive(Debug, Clone, Copy)]
pub struct Permissions {
    pub assistant: bool,
    pub video_capture_allowed: bool,
    pub capture_content_visible: bool,
    pub audio_capture_allowed: bool,
    pub floating_ball: bool,
}
/// Message.field15 SystemStateChange.field3 PermissionState.
pub fn encode_permissions(seq: u64, timestamp: u64, permissions: Permissions) -> Vec<u8> {
    let mut state = Vec::new();
    for (n, v) in [(1,permissions.assistant),(2,permissions.video_capture_allowed),(3,permissions.capture_content_visible),(4,permissions.audio_capture_allowed),(5,permissions.floating_ball)] {
        boolean(n, v, &mut state);
    }
    let mut change = Vec::new();
    bytes(3, &state, &mut change);
    envelope(seq, timestamp, 15, &change)
}

/// QuerySystemState StateId=1 expects this secure desktop state notification.
/// Locked/SAS/support flags must come from the current Linux session's evidence.
pub fn encode_secure_desktop(seq: u64, timestamp: u64, locked: bool, sas_shown: bool, supports_auto_logon: bool) -> Vec<u8> {
    let mut state = Vec::new();
    boolean(1, locked, &mut state);
    boolean(2, sas_shown, &mut state);
    boolean(5, supports_auto_logon, &mut state);
    let mut change = Vec::new();
    bytes(1, &state, &mut change);
    envelope(seq, timestamp, 15, &change)
}

/// Message.field12 ReportError { 2: error_code, 3: error_msg } (action left default, as the
/// Mac host does for a failed AUTO_UNLOCK_SYSTEM).
pub fn encode_report_error(seq: u64, timestamp: u64, error_code: i32, error_msg: &str) -> Vec<u8> {
    let mut report = Vec::new();
    integer(2, error_code, &mut report);
    text(3, error_msg, &mut report);
    envelope(seq, timestamp, 12, &report)
}

#[cfg(test)]
#[path = "protocol.rs"]
mod wire_test_support;

#[cfg(test)]
mod tests {
    use super::*;
    use super::wire_test_support::{decode_wire, WireValue};
    fn nested(data: &[u8], field: u32) -> Vec<u8> {
        decode_wire(data).unwrap().into_iter().find_map(|f| {
            if f.number == field { if let WireValue::Bytes(b) = f.value { return Some(b.to_vec()); } }
            None
        }).expect("expected nested field")
    }
    fn number(data: &[u8], field: u32) -> Option<u64> {
        decode_wire(data).unwrap().into_iter().find_map(|f| {
            if f.number == field { if let WireValue::Varint(v) = f.value { return Some(v); } }
            None
        })
    }
    fn screen() -> Screen<'static> {
        Screen { id: 1, fps: 10, resolution: WinRect { left: -1920, top: 0, width: 1920, height: 1080, pixel_width: 1920, pixel_height: 1080 }, primary: true, dpr: 1.0, display_name: "test display", video_track_index: 0, screen_type: 0, builtin_screen_type: 0, scaled: &[], current: None }
    }
    #[test]
    fn screen_inventory_uses_correct_nested_rect_and_fixed64_scale() {
        let msg = encode_screen_sources(7, 9, &[screen()], 1).unwrap();
        let inventory = nested(&msg, 7);
        assert_eq!(number(&inventory, 2), Some(1));
        let display = nested(&inventory, 1);
        let rect = nested(&display, 4);
        assert_eq!(number(&rect, 1), Some((-1920i64) as u64));
        assert_eq!(number(&rect, 3), Some(1920));
        assert_eq!(number(&rect, 4), Some(1080));
        assert_eq!(number(&rect, 5), Some(1920));
        assert_eq!(number(&rect, 6), Some(1080));
        assert_eq!(number(&display, 12).unwrap_or(0), 0);
        assert!(decode_wire(&display).unwrap().iter().any(|f| f.number == 8 && f.value == WireValue::Fixed64(1.0f64.to_le_bytes())));
        assert_eq!(nested(&display, 3), rect);
        assert_eq!(nested(&display, 6), rect);
    }
    #[test]
    fn minimal_messages_preserve_actual_source_size_and_binding_zero() {
        let m = initial_messages(1920, 1080, 10);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].0, 7);
        assert_eq!(m[1], (8, Vec::<u8>::new()));
        let display = nested(&m[0].1, 1);
        assert_eq!(number(&display, 1).unwrap_or(0), 0);
        assert_eq!(number(&display, 12).unwrap_or(0), 0);
        let rect = nested(&display, 4);
        assert_eq!(number(&rect, 3), Some(1920));
        assert_eq!(number(&rect, 4), Some(1080));
        assert!(initial_messages(0, 1080, 10).is_empty());
        assert!(initial_messages(u32::MAX, 1080, 10).is_empty());
    }

    #[test]
    fn rejects_missing_or_invalid_measured_inventory() {
        assert!(encode_screen_sources(1, 1, &[], 1).is_err());
        assert!(encode_screen_sources(1, 1, &[screen()], 2).is_err());
        let mut s = screen(); s.resolution.width = 0;
        assert!(encode_screen_sources(1, 1, &[s], 1).is_err());
        let mut s = screen(); s.dpr = f64::NAN;
        assert!(encode_screen_sources(1, 1, &[s], 1).is_err());
        assert!(encode_screen_sources(1, 1, &[screen(), screen()], 1).is_err());
    }
    #[test]
    fn permission_state_does_not_claim_audio_or_extra_features() {
        let m = encode_permissions(1, 2, Permissions { assistant: false, video_capture_allowed: true, capture_content_visible: true, audio_capture_allowed: false, floating_ball: false });
        let p = nested(&nested(&m, 15), 3);
        assert_eq!(number(&p, 2), Some(1));
        assert_eq!(number(&p, 3), Some(1));
        assert_eq!(number(&p, 4), None);
        assert_eq!(number(&p, 5), None);
    }
    #[test]
    fn default_secure_state_is_explicitly_present() {
        let report = encode_report_error(3, 4, -5, "linux auto unlock failed");
        let error = nested(&report, 12);
        assert_eq!(number(&error, 2), Some((-5i64) as u64)); // int32 negative: 10-byte varint
        assert_eq!(number(&error, 1), None);
        let state = encode_secure_desktop(1, 2, false, false, false);
        assert_eq!(nested(&nested(&state, 15), 1), Vec::<u8>::new());
    }
    #[test]
    fn capture_change_preserves_official_desktop_enum_default() {
        let c = encode_capture_change(1, 2, 0, 1, "test capture").unwrap();
        let p = nested(&c, 8);
        assert_eq!(number(&p, 1).unwrap_or(0), 0);
        assert_eq!(number(&p, 2), Some(1));
        assert!(encode_capture_change(1, 2, 4, 1, "").is_err());
    }
}
