//! Verified SystemMetrics protobuf shape for ordinary Linux X11 control.
//! Official maps/types: analysis/protocol-fix-20261013/control/system-metrics-*.json.
//! This reports observations supplied by the caller; it never unlocks a session,
//! sends SAS, enables audio, advertises Windows, or changes desktop settings.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum Permission {
    Unknown = 0,
    Reject = 1,
    Allow = 2,
}

/// Payload for Message.which field 13 SystemMetrics. Use initialization::envelope
/// with fresh local sequence/current UNIX seconds. Current locked state belongs
/// to a separate SystemStateChange notification, not this capability message.
pub fn payload(capture: Permission, operation: Permission, control: Permission) -> Vec<u8> {
    payload_with_location(capture, operation, control, "")
}

/// Same, plus SettingMetric field 5 file_transfer_save_location: the host directory the
/// viewer's file-transfer window opens on (the official Mac host reports its receive folder).
pub fn payload_with_location(capture: Permission, operation: Permission, control: Permission, location: &str) -> Vec<u8> {
    payload_with_settings(capture, operation, control, location, &Settings::default())
}

/// Optional host settings the viewer shows in 声音 / 安全 (SettingMetric 1 keep_mute,
/// 2 mute_permanent, 3 auto_lock; GeneralMetric 2 system_sound_support).
#[derive(Debug, Default, Clone, Copy)]
pub struct Settings { pub sound: bool, pub keep_mute: bool, pub mute_permanent: bool, pub auto_lock: bool, pub private_screen: Option<u8> }

pub fn payload_with_settings(capture: Permission, operation: Permission, control: Permission, location: &str, settings: &Settings) -> Vec<u8> {
    let mut setting = Vec::new();
    // Mac 4.43.0 callBackSystemMetrics reads field 7 first, falling back to field 4 only when 7=0.
    // Report the effective mode (1 off, 2 picture, 3 screen off), not the saved preference.
    if let Some(kind) = settings.private_screen {
        setting.extend([4 << 3, u8::from(kind != 1), 7 << 3, kind]);
    }
    for (field, on) in [(1u8, settings.keep_mute), (2, settings.mute_permanent), (3, settings.auto_lock)] {
        if on { setting.push(field << 3); setting.push(1); }
    }
    if !location.is_empty() {
        setting.push(5 << 3 | 2);
        varint(location.len() as u64, &mut setting);
        setting.extend_from_slice(location.as_bytes());
    }
    for (field, state) in [(8u8, capture), (9u8, operation), (10u8, control)] {
        if state != Permission::Unknown {
            setting.push(field << 3);
            setting.push(state as u8);
        }
    }
    // SystemMetrics.general: system_sound_support = SS_SUPPORT (2) when desktop audio is streamed
    // (GvPb_SystemSoundSupport: 0 unknown, 1 nonsupport, 2 support).
    // Official Mac host 4.43.0 (626), arm64: SystemMetricsHandler (0x10000a96c)
    // appends GENERAL=1 (0x10000abe8) and SETTING=2 (helper 0x10000a230, write
    // 0x10000a64c) as it fills those sections; traverse (0x100423840) packs field 1.
    // We always report both sections. The Mac viewer ignores an empty list
    // (callBackSystemMetrics, 0x1003b4584). Host evidence: docs/privacy-screen.md.
    let mut out = vec![10, 2, 1, 2];
    out.extend(if settings.sound { &[26, 2, 16, 2][..] } else { &[26, 0][..] });
    // No support_touch, sound capability, win_os_ver, private-screen, virtual
    // mouse, auto-lock, file path or optional feature claims are fabricated.
    out.push(34); // SystemMetrics.setting field 4
    varint(setting.len() as u64, &mut out);
    out.extend_from_slice(&setting);
    out
}

fn varint(mut v: u64, out: &mut Vec<u8>) {
    while v >= 128 { out.push((v as u8 & 127) | 128); v >>= 7; }
    out.push(v as u8);
}

/// Message field 26 RemoteDownloadPath{1 download_path}.
pub fn remote_download_path(path: &str) -> Vec<u8> {
    let mut out = vec![10];
    varint(path.len() as u64, &mut out);
    out.extend_from_slice(path.as_bytes());
    out
}

#[cfg(test)]
#[path = "protocol.rs"]
mod wire_test_support;

#[cfg(test)]
mod tests {
    use super::*;
    use super::wire_test_support::{decode_wire, WireValue};
    #[test]
    fn measured_allow_capabilities_use_official_enum_values() {
        let p = payload(Permission::Allow, Permission::Allow, Permission::Allow);
        let fields = decode_wire(&p).unwrap();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0].number, 1);
        assert_eq!(fields[0].value, WireValue::Bytes(&[1, 2]));
        assert_eq!(fields[1].number, 3);
        assert_eq!(fields[1].value, WireValue::Bytes(&[]));
        let WireValue::Bytes(setting) = fields[2].value else { panic!("setting not a message") };
        assert_eq!(fields[2].number, 4);
        let s = decode_wire(setting).unwrap();
        assert_eq!(s.iter().map(|f| f.number).collect::<Vec<_>>(), vec![8, 9, 10]);
        assert!(s.iter().all(|f| f.value == WireValue::Varint(2)));
    }
    #[test]
    fn rejected_and_unknown_observations_are_not_promoted_to_allow() {
        assert_eq!(payload(Permission::Unknown, Permission::Unknown, Permission::Unknown), vec![10, 2, 1, 2, 26, 0, 34, 0]);
        assert_eq!(payload(Permission::Allow, Permission::Reject, Permission::Reject), vec![10, 2, 1, 2, 26, 0, 34, 6, 64, 2, 72, 1, 80, 1]);
    }
    #[test]
    fn emits_no_audio_touch_windows_or_auto_unlock_capability() {
        let p = payload(Permission::Allow, Permission::Allow, Permission::Allow);
        let fields = decode_wire(&p).unwrap();
        let WireValue::Bytes(general) = fields[1].value else { panic!() };
        assert!(general.is_empty());
        let WireValue::Bytes(setting) = fields[2].value else { panic!() };
        assert!(decode_wire(setting).unwrap().iter().all(|f| (8..=10).contains(&f.number)));
    }
    #[test]
    fn reports_file_transfer_save_location_as_setting_field_5() {
        let home = "/home/user/".repeat(12);
        let p = payload_with_location(Permission::Allow, Permission::Allow, Permission::Allow, &home);
        let fields = decode_wire(&p).unwrap();
        let WireValue::Bytes(setting) = fields[2].value else { panic!() };
        let s = decode_wire(setting).unwrap();
        assert_eq!(s[0].number, 5);
        assert_eq!(s[0].value, WireValue::Bytes(home.as_bytes()));
        assert_eq!(decode_wire(&remote_download_path("/home/x")).unwrap()[0].value, WireValue::Bytes(b"/home/x"));
    }
    #[test]
    fn privacy_modes_report_matching_flag_and_current_type() {
        for (kind, expected) in [(1, [32, 0, 56, 1]), (2, [32, 1, 56, 2]), (3, [32, 1, 56, 3])] {
            let settings = Settings { private_screen: Some(kind), ..Settings::default() };
            let p = payload_with_settings(Permission::Unknown, Permission::Unknown, Permission::Unknown, "", &settings);
            let fields = decode_wire(&p).unwrap();
            assert_eq!(fields[0].number, 1);
            assert_eq!(fields[0].value, WireValue::Bytes(&[1, 2]));
            assert_eq!(fields.iter().find(|f| f.number == 4).unwrap().value, WireValue::Bytes(&expected));
        }
    }
}
