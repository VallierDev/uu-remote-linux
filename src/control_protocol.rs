//! Mac UU CONTROL JSON messages recovered from the shipped ARM64 client.
//! Wire parsing accepts UTF-8 JSON carried by binary SCTP messages as well as text.
//! Coordinates and key/button identifiers remain wire values; Linux translation
//! belongs to the input engine. Only the verified protobuf echo-response shape
//! is encoded; other protobuf messages remain opaque.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum ControlInput {
    Keyboard { key: i32, pressed: bool },
    KeyboardClick { key: i32 },
    MouseButton { button: i32, pressed: bool },
    MouseClick { button: i32 },
    MouseAbsolute { x: f64, y: f64 },
    MouseRelative { dx: i32, dy: i32 },
    MouseScroll { dx: i32, dy: i32 },
    /// Official Windows host `text_input {content}` (KeyboardMouseRunner::execTextInput): text typed by the
    /// mobile clients' own input method. Never logged.
    TextInput { text: String },
    /// Official Windows host GamepadManager (0x5dde60 dispatcher): connect/disconnect read `index`
    /// as a byte; `gpd_update` is the 0x5ddb30 state decoder. Slots 0..=3 are the host's XInput cap;
    /// a higher index is a known action this host does not implement.
    GamepadConnect { index: u8 },
    GamepadDisconnect { index: u8 },
    GamepadUpdate { index: u8, buttons: u16, lt: u8, rt: u8, lx: i16, ly: i16, rx: i16, ry: i16 },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ControlMessage {
    KeyboardHeartbeat,
    Input(ControlInput),
    Unsupported(Value),
}

fn integer(v: &Value, name: &str) -> Result<i32, String> {
    v.get(name)
        .and_then(Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
        .ok_or_else(|| format!("CONTROL field {name} must be an i32 integer"))
}

fn coordinate(v: &Value, name: &str) -> Result<f64, String> {
    v.get(name).and_then(Value::as_f64).filter(|n| n.is_finite())
        .ok_or_else(|| format!("CONTROL field {name} must be a finite number"))
}

/// Official 0x5ddb30 state decoder field widths: `index` and triggers are bytes, `buttons` u16,
/// thumb values signed 16-bit. `Ok(None)` is a known gamepad action for a slot above the XInput cap.
fn gamepad_index(v: &Value) -> Result<Option<u8>, String> {
    let index = u8::try_from(integer(v, "index")?).map_err(|_| "CONTROL field index must be a byte".to_owned())?;
    Ok((index <= 3).then_some(index))
}
fn gamepad_u8(v: &Value, name: &str) -> Result<u8, String> {
    u8::try_from(integer(v, name)?).map_err(|_| format!("CONTROL field {name} must be a byte"))
}
fn gamepad_u16(v: &Value, name: &str) -> Result<u16, String> {
    u16::try_from(integer(v, name)?).map_err(|_| format!("CONTROL field {name} must be a u16"))
}
fn gamepad_i16(v: &Value, name: &str) -> Result<i16, String> {
    i16::try_from(integer(v, name)?).map_err(|_| format!("CONTROL field {name} must be an i16"))
}

pub fn decode_control(data: &[u8]) -> Result<ControlMessage, String> {
    let v: Value = serde_json::from_slice(data)
        .map_err(|_| "CONTROL payload is not valid JSON".to_owned())?;
    if v.get("heartbeat").and_then(Value::as_str) == Some("kbd_heartbeat") {
        return Ok(ControlMessage::KeyboardHeartbeat);
    }
    let Some(action) = v.get("action").and_then(Value::as_str) else {
        return Ok(ControlMessage::Unsupported(v));
    };
    let event = match action {
        "kbd_press" => ControlInput::Keyboard { key: integer(&v, "key")?, pressed: true },
        "kbd_release" => ControlInput::Keyboard { key: integer(&v, "key")?, pressed: false },
        "kbd_click" => ControlInput::KeyboardClick { key: integer(&v, "key")? },
        "mouse_press" => ControlInput::MouseButton { button: integer(&v, "button")?, pressed: true },
        "mouse_release" => ControlInput::MouseButton { button: integer(&v, "button")?, pressed: false },
        "mouse_click" => ControlInput::MouseClick { button: integer(&v, "button")? },
        "mouse_move_absolute" => ControlInput::MouseAbsolute {
            x: coordinate(&v, "abs_x")?, y: coordinate(&v, "abs_y")?,
        },
        "mouse_move_relative" => ControlInput::MouseRelative {
            dx: integer(&v, "delta_x")?, dy: integer(&v, "delta_y")?,
        },
        "text_input" => {
            let text = v.get("content").and_then(Value::as_str).ok_or("CONTROL field content must be a string")?;
            if text.len() > 16 * 1024 { return Err("text_input content exceeds 16 KiB".into()); }
            ControlInput::TextInput { text: text.to_owned() }
        }
        "mouse_scroll" => ControlInput::MouseScroll {
            dx: integer(&v, "delta_x")?, dy: integer(&v, "delta_y")?,
        },
        "gpd_connect" => match gamepad_index(&v)? {
            Some(index) => ControlInput::GamepadConnect { index },
            None => return Ok(ControlMessage::Unsupported(v)),
        },
        "gpd_disconnect" => match gamepad_index(&v)? {
            Some(index) => ControlInput::GamepadDisconnect { index },
            None => return Ok(ControlMessage::Unsupported(v)),
        },
        "gpd_update" => {
            let Some(index) = gamepad_index(&v)? else { return Ok(ControlMessage::Unsupported(v)); };
            ControlInput::GamepadUpdate {
                index,
                buttons: gamepad_u16(&v, "buttons")?,
                lt: gamepad_u8(&v, "left_trigger")?,
                rt: gamepad_u8(&v, "right_trigger")?,
                lx: gamepad_i16(&v, "left_thumb_x")?,
                ly: gamepad_i16(&v, "left_thumb_y")?,
                rx: gamepad_i16(&v, "right_thumb_x")?,
                ry: gamepad_i16(&v, "right_thumb_y")?,
            }
        }
        _ => return Ok(ControlMessage::Unsupported(v)),
    };
    Ok(ControlMessage::Input(event))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_binary_carried_keyboard_heartbeat() {
        let bytes = br#"{"heartbeat":"kbd_heartbeat"}"#;
        assert_eq!(bytes.len(), 29);
        assert_eq!(decode_control(bytes).unwrap(), ControlMessage::KeyboardHeartbeat);
    }

    #[test]
    fn decodes_mac_absolute_and_relative_wire_keys() {
        assert_eq!(decode_control(br#"{"action":"mouse_move_absolute","abs_x":12,"abs_y":34}"#).unwrap(),
            ControlMessage::Input(ControlInput::MouseAbsolute { x: 12.0, y: 34.0 }));
        assert_eq!(decode_control(br#"{"action":"mouse_move_relative","delta_x":-4,"delta_y":5}"#).unwrap(),
            ControlMessage::Input(ControlInput::MouseRelative { dx: -4, dy: 5 }));
    }

    #[test]
    fn preserves_wire_key_and_button_identifiers() {
        assert_eq!(decode_control(br#"{"action":"kbd_release","key":65}"#).unwrap(),
            ControlMessage::Input(ControlInput::Keyboard { key: 65, pressed: false }));
        assert_eq!(decode_control(br#"{"action":"mouse_press","button":2}"#).unwrap(),
            ControlMessage::Input(ControlInput::MouseButton { button: 2, pressed: true }));
    }

    #[test]
    fn rejects_missing_or_unrepresentable_input_fields() {
        assert!(decode_control(br#"{"action":"kbd_press"}"#).is_err());
        assert!(decode_control(br#"{"action":"kbd_press","key":2147483648}"#).is_err());
        assert!(decode_control(br#"{"action":"kbd_press","key":1.5}"#).is_err());
        assert!(decode_control(&[0xff, 0x08]).is_err());
    }

    #[test]
    fn decodes_official_gamepad_wire_fields() {
        assert_eq!(decode_control(br#"{"action":"gpd_connect","index":0}"#).unwrap(),
            ControlMessage::Input(ControlInput::GamepadConnect { index: 0 }));
        assert_eq!(decode_control(br#"{"action":"gpd_disconnect","index":3}"#).unwrap(),
            ControlMessage::Input(ControlInput::GamepadDisconnect { index: 3 }));
        assert_eq!(decode_control(br#"{"action":"gpd_update","index":0,"buttons":4096,"left_trigger":128,"right_trigger":255,"left_thumb_x":-32768,"left_thumb_y":32767,"right_thumb_x":100,"right_thumb_y":-100}"#).unwrap(),
            ControlMessage::Input(ControlInput::GamepadUpdate {
                index: 0, buttons: 4096, lt: 128, rt: 255, lx: -32768, ly: 32767, rx: 100, ry: -100,
            }));
    }

    #[test]
    fn rejects_incomplete_or_unrepresentable_gamepad_updates() {
        let base = || serde_json::json!({"action":"gpd_update","index":0,"buttons":0,"left_trigger":0,
            "right_trigger":0,"left_thumb_x":0,"left_thumb_y":0,"right_thumb_x":0,"right_thumb_y":0});
        // 0x5ddb30: every state field is required.
        for field in ["index","buttons","left_trigger","right_trigger","left_thumb_x","left_thumb_y","right_thumb_x","right_thumb_y"] {
            let mut value = base();
            value.as_object_mut().unwrap().remove(field);
            assert!(decode_control(&serde_json::to_vec(&value).unwrap()).is_err(), "missing {field}");
        }
        for (field, value) in [("buttons", 65536), ("buttons", -1), ("left_trigger", 256), ("right_trigger", -1),
            ("left_thumb_x", -32769), ("left_thumb_y", 32768)] {
            let mut json = base();
            json[field] = value.into();
            assert!(decode_control(&serde_json::to_vec(&json).unwrap()).is_err(), "field {field}={value}");
        }
        let mut json = base();
        json["buttons"] = serde_json::json!(1.5);
        assert!(decode_control(&serde_json::to_vec(&json).unwrap()).is_err());
        let mut json = base();
        json["index"] = 4.into();
        assert!(matches!(decode_control(&serde_json::to_vec(&json).unwrap()).unwrap(), ControlMessage::Unsupported(_)));
        assert!(matches!(decode_control(br#"{"action":"gpd_connect","index":4}"#).unwrap(), ControlMessage::Unsupported(_)));
        assert!(matches!(decode_control(br#"{"action":"gpd_disconnect","index":4}"#).unwrap(), ControlMessage::Unsupported(_)));
        assert!(decode_control(br#"{"action":"gpd_connect","index":-1}"#).is_err());
    }

    #[test]
    fn encodes_echo_response_without_advertising_unverified_features() {
        let reply = encode_echo_response_for(1, 2, 200);
        assert_eq!(&reply[..4], &[8, 1, 16, 2]);
        assert!(reply.windows(11).any(|b| b == br#"{"seq":200}"#));
        assert!(reply.ends_with(&[34, 8, 8, 4, 16, 2, 24, 2, 48, 2]));
        assert!(reply.windows(2).any(|b| b == [8, 1]));
    }

    #[test]
    fn advertises_verified_text_rpc_clipboard_when_backend_ready() {
        use crate::protocol::{decode_wire,WireValue};
        let response = encode_echo_response_with_clipboard(1,2,200,true);
        let outer=decode_wire(&response).unwrap();
        let action=outer.iter().find(|f|f.number==3).unwrap();
        let WireValue::Bytes(action)=action.value else {panic!("action type")};
        let fields=decode_wire(action).unwrap();
        let flag=fields.iter().find(|f|f.number==4).unwrap();
        let WireValue::Bytes(flag)=flag.value else {panic!("flag type")};
        let features=decode_wire(flag).unwrap();
        let numbers:Vec<(u32,u64)>=features.iter().map(|f|match f.value {WireValue::Varint(v)=>(f.number,v),_=>panic!("flag type")}).collect();
        assert_eq!(numbers,vec![(1,4),(2,2),(3,2),(6,2),(8,3)]);
    }

    #[test]
    fn names_only_official_simple_action_values() {
        assert_eq!(simple_action_name(4), Some("capture_desktop"));
        assert_eq!(simple_action_name(22), Some("acquire_system_info"));
        assert_eq!(simple_action_name(27), Some("need_restart_userapp"));
        assert_eq!(simple_action_name(28), None);
        assert_eq!(simple_action_name(u64::MAX), None);
    }

    #[test]
    fn does_not_invent_actions_for_unknown_json() {
        assert!(matches!(decode_control(br#"{"action":"unverified"}"#).unwrap(), ControlMessage::Unsupported(_)));
        assert_eq!(decode_control("{\"action\":\"text_input\",\"content\":\"你好 a\"}".as_bytes()).unwrap(),
            ControlMessage::Input(ControlInput::TextInput { text: "你好 a".into() }));
        assert!(decode_control(br#"{"action":"text_input"}"#).is_err());
    }
}

/// Generate the verified protobuf SimpleAction echoResponse envelope.
/// The caller chooses sequence/timestamp policy; no peer feature flags are copied.
pub fn encode_echo_response(seq: u64, timestamp: u64) -> Vec<u8> {
    encode_echo_response_for(seq, timestamp, seq)
}

/// Correlate the response using the official args JSON request-sequence field.
/// An explicit empty FeatureFlag advertises no unimplemented feature versions.
pub fn encode_echo_response_for(seq: u64, timestamp: u64, request_seq: u64) -> Vec<u8> {
    encode_echo_response_with_clipboard(seq, timestamp, request_seq, false)
}

/// Advertise only the verified legacy text bridge when the native backend is ready.
/// FeatureFlag.ff_clipboard field 8: SUPPORT_TEXT_CHANGE=3; no file-transfer claim.
pub fn encode_echo_response_with_clipboard(seq: u64, timestamp: u64, request_seq: u64, clipboard: bool) -> Vec<u8> {
    fn put_varint(mut v: u64, out: &mut Vec<u8>) {
        while v >= 128 {
            out.push((v as u8 & 127) | 128);
            v >>= 7;
        }
        out.push(v as u8);
    }
    let args = format!(r#"{{"seq":{request_seq}}}"#);
    let mut action = vec![8, 1, 18];
    put_varint(args.len() as u64, &mut action);
    action.extend_from_slice(args.as_bytes());
    // FeatureFlag: 1 ff_capture_setting = CUSTOM_BITRATE (fps/quality/bitrate RPCs),
    // 2 ff_simple_action = AUTO_LOCK (mute + lock after session), 3 ff_system_metrics = SETTING_AUTO_LOCK,
    // 6 ff_file_transfer_ftp = LIST_COMPRESS (zlib dir_data), 8 ff_clipboard = SUPPORT_TEXT_CHANGE.
    let mut flags = vec![8, crate::capture_setting::FEATURE_VERSION as u8,
        16, crate::system_actions::FF_SIMPLE_ACTION, 24, crate::system_actions::FF_SYSTEM_METRICS, 48, 2];
    // Privacy screen (Mac UURemote name maps): 3 ff_system_metrics = FF_SYSTEM_METRICS_SETTING_PRIVATE_SCREEN
    // (3, after AUTO_LOCK 2; the viewer then reads SettingMetric 4/7), 4 ff_private_screen =
    // FF_PRIVATE_SCREEN_ENHANCED_PRIVACY (2): screen off besides the picture mode.
    if crate::privacy_screen::supported() { flags[5] = 3; flags.extend([32, 2]); }
    if clipboard { flags.extend_from_slice(&[64, 3]); }
    action.push(34);
    action.push(flags.len() as u8);
    action.extend_from_slice(&flags);
    let mut out = Vec::new();
    if seq != 0 {
        out.push(8);
        put_varint(seq, &mut out);
    }
    if timestamp != 0 {
        out.push(16);
        put_varint(timestamp, &mut out);
    }
    out.push(26); // Message.simpleAction
    put_varint(action.len() as u64, &mut out);
    out.extend_from_slice(&action);
    out
}

/// Official ActionType map recovered from _ProtoNameProviding's 28-entry table.
/// This describes messages; it does not authorize or execute system actions.
pub fn simple_action_name(action: u64) -> Option<&'static str> {
    const NAMES: [&str; 28] = [
        "echo_request", "echo_response", "show_desktop", "show_windows",
        "capture_desktop", "capture_window", "capture_mumu", "stop_capture",
        "restart_capture", "reboot_system", "lock_system", "key_toggle",
        "shutdown_system", "show_taskmgr", "mute_system", "auto_unlock_system",
        "mute_restore", "mute_permanent", "mute_temporary", "set_auto_lock",
        "stop_acquire_update", "quit_capture", "acquire_system_info",
        "microphone_opened", "microphone_closed", "speaker_opened",
        "speaker_closed", "need_restart_userapp",
    ];
    usize::try_from(action).ok().and_then(|n| NAMES.get(n).copied())
}
