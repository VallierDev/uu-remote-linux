//! Official protobuf maps: RpcRequest 7 {1 enable, 2 privacy_type, 3 setting_source};
//! RpcResponse 3 {1 error_code, 2 req_privacy_type, 3 cur_privacy_type}. Types: see `Mode`.
//! Field numbers and enums verified against the Mac UURemote SwiftProtobuf name maps.
use super::Mode;

pub struct Request { pub enable: bool, pub mode: Option<Mode>, requested: u64 }

impl Request {
    /// iOS viewers encode "turn off the display" as `enable=0, privacy_type=3`: their enable bit is
    /// the picture-mode switch, and the explicit screen-off type carries the intent. iOS "off" is
    /// `enable=0, type=1` (seen live), so rewrite only this exact combination. Android 4.43.0 was
    /// verified with the field-1 semantics, so the rewrite is iOS-only.
    pub fn normalize_ios_screen_off(&mut self, ios: bool) -> bool {
        if ios && !self.enable && self.mode == Some(Mode::ScreenOff) {
            self.enable = true;
            return true;
        }
        false
    }
    /// Windows viewers send every privacy request with `enable=0` and carry the intent in
    /// `privacy_type`: `0/0` is off, `0/2` picture and `0/3` screen-off (verified live on the
    /// Windows client). Android 4.43.0 and macOS keep field 1 as the master switch.
    pub fn normalize_windows(&mut self, windows: bool) -> bool {
        if windows && !self.enable && self.mode.is_some() {
            self.enable = true;
            return true;
        }
        false
    }
}

pub fn parse(rpc: &[crate::protocol::WireField]) -> Option<(Vec<u8>, Request)> {
    use crate::protocol::{decode_wire, WireValue};
    let body = |n| rpc.iter().find_map(|f| match &f.value { WireValue::Bytes(b) if f.number == n => Some(*b), _ => None });
    let header = body(1)?.to_vec();
    let req = decode_wire(body(7)?).ok()?;
    let value = |n| req.iter().find_map(|f| match f.value { WireValue::Varint(v) if f.number == n => Some(v), _ => None }).unwrap_or(0);
    log::info!("Privacy request: enable_value={} requested_type={} source={}", value(1), value(2), value(3));
    Some((header, Request { enable: value(1) != 0, mode: Mode::from_type(value(2)), requested: value(2) }))
}

/// RpcResponse with the request's header, error_code 1 on failure, and the type now in effect.
pub fn response(header: &[u8], request: &Request, failed: bool, current: u8) -> Vec<u8> {
    let mut payload = Vec::new();
    if failed { payload.extend([8, 1]); }
    if request.requested != 0 { payload.push(16); varint(request.requested, &mut payload); }
    payload.extend([24, current]);
    let mut out = vec![10];
    varint(header.len() as u64, &mut out);
    out.extend(header);
    out.push(26);
    varint(payload.len() as u64, &mut out);
    out.extend(payload);
    out
}
/// RpcRequest 25 update_screen_saver_req → RpcResponse 21 update_screen_saver_rsp (Mac UURemote
/// name maps). The viewer sends it before enabling picture mode (notifyScreensaverUpdate) and aborts
/// with error 9998 when it times out. The picture itself is the host's: answer an empty (success) one.
pub fn screensaver_response(rpc: &[crate::protocol::WireField]) -> Option<Vec<u8>> {
    use crate::protocol::WireValue;
    if !rpc.iter().any(|f| f.number == 25) { return None; }
    let header = rpc.iter().find_map(|f| match &f.value { WireValue::Bytes(b) if f.number == 1 => Some(*b), _ => None })?;
    let mut out = vec![10];
    varint(header.len() as u64, &mut out);
    out.extend(header);
    out.extend([0xaa, 1, 0]); // field 21, length-delimited, empty
    log::info!("Privacy screensaver update answered");
    Some(out)
}
/// SystemStateChange { 5 PrivateScreenState { 1 error_code, 2 req_privacy_type, 3 cur_privacy_type } }
/// (field numbers from the Mac UURemote name map); the host pushes it when the state changes.
pub fn state_change(current: u8) -> Vec<u8> {
    let state = [16, current, 24, current];
    let mut out = vec![42, state.len() as u8];
    out.extend(state);
    out
}
fn varint(mut n: u64, out: &mut Vec<u8>) {
    while n >= 128 { out.push((n as u8 & 127) | 128); n >>= 7; }
    out.push(n as u8);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_request_and_echoes_header() {
        let bytes = [10, 2, 8, 42, 58, 4, 8, 1, 16, 3];
        let (header, request) = parse(&crate::protocol::decode_wire(&bytes).unwrap()).unwrap();
        assert!(request.enable);
        assert_eq!(request.mode, Some(Mode::ScreenOff));
        assert_eq!(response(&header, &request, true, 1), [10, 2, 8, 42, 26, 6, 8, 1, 16, 3, 24, 1]);
        assert_eq!(response(&header, &request, false, 3), [10, 2, 8, 42, 26, 4, 16, 3, 24, 3]);
        assert_eq!(state_change(2), [42, 4, 16, 2, 24, 2]);
        let saver = crate::protocol::decode_wire(&[10, 2, 8, 7, 0xca, 1, 0]).unwrap();
        assert_eq!(screensaver_response(&saver).unwrap(), [10, 2, 8, 7, 0xaa, 1, 0]);
    }
    #[test]
    fn normalizes_ios_screen_off_only() {
        let screen_off = crate::protocol::decode_wire(&[10, 2, 8, 42, 58, 4, 8, 0, 16, 3]).unwrap();
        let (_, mut request) = parse(&screen_off).unwrap();
        assert!(request.normalize_ios_screen_off(true));
        assert!(request.enable);
        assert_eq!(request.mode, Some(Mode::ScreenOff));
        let (_, mut request) = parse(&screen_off).unwrap();
        assert!(!request.normalize_ios_screen_off(false));
        assert!(!request.enable);

        // An iOS "off" (type 1) and a Mac-style enable (1,3) must stay untouched.
        let off = crate::protocol::decode_wire(&[10, 2, 8, 42, 58, 4, 8, 0, 16, 1]).unwrap();
        let (_, mut request) = parse(&off).unwrap();
        assert!(!request.normalize_ios_screen_off(true));
        assert!(!request.enable);
        let mac = crate::protocol::decode_wire(&[10, 2, 8, 42, 58, 4, 8, 1, 16, 3]).unwrap();
        let (_, mut request) = parse(&mac).unwrap();
        assert!(!request.normalize_ios_screen_off(true));
        assert!(request.enable);
    }
    #[test]
    fn normalizes_windows_encoding_only() {
        // Windows: enable is always 0, the type carries the intent.
        for (wire, mode) in [(2u8, Mode::Picture), (3u8, Mode::ScreenOff)] {
            let bytes = [10, 2, 8, 42, 58, 4, 8, 0, 16, wire];
            let (_, mut request) = parse(&crate::protocol::decode_wire(&bytes).unwrap()).unwrap();
            assert!(request.normalize_windows(true));
            assert!(request.enable);
            assert_eq!(request.mode, Some(mode));
        }
        // Off stays off: type 0 (seen when the Windows switch is turned off) and type 1.
        for wire in [0u8, 1] {
            let bytes = [10, 2, 8, 42, 58, 4, 8, 0, 16, wire];
            let (_, mut request) = parse(&crate::protocol::decode_wire(&bytes).unwrap()).unwrap();
            assert!(!request.normalize_windows(true));
            assert!(!request.enable);
        }
        // Other platforms and explicit enables are untouched.
        let picture = crate::protocol::decode_wire(&[10, 2, 8, 42, 58, 4, 8, 0, 16, 2]).unwrap();
        let (_, mut request) = parse(&picture).unwrap();
        assert!(!request.normalize_windows(false));
        assert!(!request.enable);
        let explicit = crate::protocol::decode_wire(&[10, 2, 8, 42, 58, 4, 8, 1, 16, 2]).unwrap();
        let (_, mut request) = parse(&explicit).unwrap();
        assert!(!request.normalize_windows(true));
        assert!(request.enable);
    }
}
