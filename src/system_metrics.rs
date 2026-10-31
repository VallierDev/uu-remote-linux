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
/// 2 mute_permanent, 3 auto_lock; GeneralMetric 2 system_sound_support), and 防窥 when the host
/// supports it (SettingMetric 4 private_screen on, 7 private_screen_type).
#[derive(Debug, Default, Clone, Copy)]
pub struct Settings { pub sound: bool, pub keep_mute: bool, pub mute_permanent: bool, pub auto_lock: bool, pub private_screen: Option<(bool, u64)> }

pub fn payload_with_settings(capture: Permission, operation: Permission, control: Permission, location: &str, settings: &Settings) -> Vec<u8> {
    let mut setting = Vec::new();
    for (field, on) in [(1u8, settings.keep_mute), (2, settings.mute_permanent), (3, settings.auto_lock)] {
        if on { setting.push(field << 3); setting.push(1); }
    }
    if let Some((true, _)) = settings.private_screen { setting.extend_from_slice(&[4 << 3, 1]); }
    if !location.is_empty() {
        setting.push(5 << 3 | 2);
        varint(location.len() as u64, &mut setting);
        setting.extend_from_slice(location.as_bytes());
    }
    if let Some((_, kind)) = settings.private_screen { setting.push(7 << 3); varint(kind, &mut setting); }
    for (field, state) in [(8u8, capture), (9u8, operation), (10u8, control)] {
        if state != Permission::Unknown {
            setting.push(field << 3);
            setting.push(state as u8);
        }
    }
    // SystemMetrics.general: system_sound_support = SS_SUPPORT (2) when desktop audio is streamed
    // (GvPb_SystemSoundSupport: 0 unknown, 1 nonsupport, 2 support).
    let mut out = if settings.sound { vec![26, 2, 16, 2] } else { vec![26, 0] };
    // No support_touch, sound capability, win_os_ver, virtual
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
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].number, 3);
        assert_eq!(fields[0].value, WireValue::Bytes(&[]));
        let WireValue::Bytes(setting) = fields[1].value else { panic!("setting not a message") };
        assert_eq!(fields[1].number, 4);
        let s = decode_wire(setting).unwrap();
        assert_eq!(s.iter().map(|f| f.number).collect::<Vec<_>>(), vec![8, 9, 10]);
        assert!(s.iter().all(|f| f.value == WireValue::Varint(2)));
    }
    #[test]
    fn rejected_and_unknown_observations_are_not_promoted_to_allow() {
        assert_eq!(payload(Permission::Unknown, Permission::Unknown, Permission::Unknown), vec![26, 0, 34, 0]);
        assert_eq!(payload(Permission::Allow, Permission::Reject, Permission::Reject), vec![26, 0, 34, 6, 64, 2, 72, 1, 80, 1]);
    }
    #[test]
    fn emits_no_audio_touch_windows_or_auto_unlock_capability() {
        let p = payload(Permission::Allow, Permission::Allow, Permission::Allow);
        let fields = decode_wire(&p).unwrap();
        let WireValue::Bytes(general) = fields[0].value else { panic!() };
        assert!(general.is_empty());
        let WireValue::Bytes(setting) = fields[1].value else { panic!() };
        assert!(decode_wire(setting).unwrap().iter().all(|f| (8..=10).contains(&f.number)));
    }
    #[test]
    fn reports_file_transfer_save_location_as_setting_field_5() {
        let home = "/home/user/".repeat(12);
        let p = payload_with_location(Permission::Allow, Permission::Allow, Permission::Allow, &home);
        let fields = decode_wire(&p).unwrap();
        let WireValue::Bytes(setting) = fields[1].value else { panic!() };
        let s = decode_wire(setting).unwrap();
        assert_eq!(s[0].number, 5);
        assert_eq!(s[0].value, WireValue::Bytes(home.as_bytes()));
        assert_eq!(decode_wire(&remote_download_path("/home/x")).unwrap()[0].value, WireValue::Bytes(b"/home/x"));
    }
}
