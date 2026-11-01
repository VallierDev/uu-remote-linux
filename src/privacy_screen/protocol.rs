//! Official protobuf maps: RpcRequest 7 {1 enable, 2 privacy_type, 3 setting_source};
//! RpcResponse 3 {1 error_code, 2 req_privacy_type, 3 cur_privacy_type}. Types: see `Mode`.
use super::Mode;

pub struct Request { pub enable: bool, pub mode: Option<Mode>, requested: u64 }

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
    }
}
