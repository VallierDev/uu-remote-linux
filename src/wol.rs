//! Experimental Wake-on-LAN helper (`UU_WOL_HELPER=1`, off by default).
//!
//! The official Windows host (GameViewerServer: LoginManager::reportLanInfo, PushMessageHandler::wakeOnLan,
//! wake_on_lan.cpp host::send_wol) reports its LAN address to `api-ipv4…/api/v1/device/wake_on_lan/info`
//! after sign-in (logged as "mac / ip / subnet_mask", response carries `turn_on_wol`), and on a
//! `wake_on_lan` cloud push with a `target_device` {mac, ip, subnet_mask} it sends a magic packet to that
//! subnet's broadcast address, waking another PC of the account on the same LAN. The exact request and
//! push field names come from the official log strings only; nothing here is verified end to end yet.
//! Addresses are sent to NetEase only when the helper is enabled and are never logged.

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};

pub fn enabled() -> bool { std::env::var("UU_WOL_HELPER").is_ok_and(|v| v == "1") }

/// `aa:bb:cc:dd:ee:ff` / `aa-bb-…` / `aabbcc…` → 6 bytes.
pub fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let hex: String = text.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 12 || text.chars().any(|c| !(c.is_ascii_hexdigit() || matches!(c, ':' | '-' | '.'))) { return None; }
    let mut mac = [0u8; 6];
    for (i, byte) in mac.iter_mut().enumerate() { *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?; }
    (mac != [0; 6] && mac != [0xff; 6]).then_some(mac)
}

/// Six 0xFF bytes followed by the MAC sixteen times.
pub fn magic_packet(mac: [u8; 6]) -> Vec<u8> {
    let mut packet = vec![0xff; 6];
    for _ in 0..16 { packet.extend_from_slice(&mac); }
    packet
}

/// Directed broadcast of `ip`/`mask`, or None when either is missing or invalid.
pub fn subnet_broadcast(ip: &str, mask: &str) -> Option<Ipv4Addr> {
    let (ip, mask): (Ipv4Addr, Ipv4Addr) = (ip.parse().ok()?, mask.parse().ok()?);
    let mask = u32::from(mask);
    if mask == 0 || mask.leading_ones() + mask.trailing_zeros() != 32 { return None; }
    Some(Ipv4Addr::from(u32::from(ip) | !mask))
}

/// Handles a `wake_on_lan` push payload; returns how many datagrams were sent.
pub fn handle_push(data: &serde_json::Value) -> Result<usize, String> {
    let target = data.get("target_device").or_else(|| data.get("data").and_then(|d| d.get("target_device")))
        .ok_or("wake_on_lan push without target_device")?;
    let text = |key: &str| target.get(key).and_then(serde_json::Value::as_str).unwrap_or("");
    let mac = parse_mac(text("mac")).ok_or("wake_on_lan target has no valid mac")?;
    let packet = magic_packet(mac);
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("WOL socket: {}", e.kind()))?;
    socket.set_broadcast(true).map_err(|e| format!("WOL broadcast: {}", e.kind()))?;
    let mut targets = vec![Ipv4Addr::BROADCAST];
    if let Some(directed) = subnet_broadcast(text("ip"), text("subnet_mask")) { targets.insert(0, directed); }
    let mut sent = 0;
    for address in targets { for port in [9, 7] {
        if socket.send_to(&packet, SocketAddrV4::new(address, port)).is_ok() { sent += 1; }
    } }
    if sent == 0 { return Err("WOL packet could not be sent".into()); }
    Ok(sent)
}

/// This host's LAN identity on the default IPv4 route: (mac, ip, subnet mask).
#[cfg(target_os = "linux")]
pub fn lan_info() -> Option<(String, String, String)> {
    let routes = std::fs::read_to_string("/proc/net/route").ok()?;
    let iface = routes.lines().skip(1).map(|l| l.split_whitespace().collect::<Vec<_>>())
        .find(|f| f.len() > 2 && f[1] == "00000000")?.first()?.to_string();
    let mac = std::fs::read_to_string(format!("/sys/class/net/{iface}/address")).ok()?.trim().to_owned();
    parse_mac(&mac)?;
    let mut addrs: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut addrs) } != 0 { return None; }
    let mut result = None;
    let mut cursor = addrs;
    while !cursor.is_null() {
        let entry = unsafe { &*cursor };
        cursor = entry.ifa_next;
        if entry.ifa_addr.is_null() || entry.ifa_netmask.is_null() { continue; }
        let name = unsafe { std::ffi::CStr::from_ptr(entry.ifa_name) }.to_string_lossy();
        if name != iface.as_str() || i32::from(unsafe { (*entry.ifa_addr).sa_family }) != libc::AF_INET { continue; }
        let v4 = |p: *mut libc::sockaddr| Ipv4Addr::from(u32::from_be(unsafe { (*(p as *const libc::sockaddr_in)).sin_addr.s_addr }));
        result = Some((mac.clone(), v4(entry.ifa_addr).to_string(), v4(entry.ifa_netmask).to_string()));
        break;
    }
    unsafe { libc::freeifaddrs(addrs) };
    result
}
#[cfg(not(target_os = "linux"))]
pub fn lan_info() -> Option<(String, String, String)> { None }

/// Body of the helper registration request (same address as `inner_ip` and `ip`: this host has one LAN address).
fn report_body(mac: &str, ip: &str, subnet_mask: &str) -> serde_json::Value {
    serde_json::json!({ "mac": mac, "inner_ip": ip, "ip": ip, "subnet_mask": subnet_mask })
}

/// Report this host's LAN identity like the official host after sign-in.
/// The server rejects the report (HTTP 422, code 1001) unless `inner_ip` is present; the official macOS
/// client sends `inner_ip`, `ip`, `mac` and `subnet_mask`. Verified live: with `inner_ip` the helper is
/// accepted (HTTP 200) and the cloud forwards `wake_on_lan` pushes to this host.
pub async fn report(auth: &crate::auth::UUAuthClient) {
    let Some((mac, ip, subnet_mask)) = lan_info() else { log::warn!("WOL helper: no IPv4 default-route interface"); return; };
    let body = report_body(&mac, &ip, &subnet_mask);
    match auth.post_json_url("https://api-ipv4.nrd.nie.163.com", "/api/v1/device/wake_on_lan/info", &body).await {
        Ok((status, value)) => log::info!("WOL helper: LAN info reported: status={status} code={:?} turn_on_wol={:?}",
            value.get("code").and_then(serde_json::Value::as_i64),
            value.get("data").and_then(|d| d.get("turn_on_wol")).or_else(|| value.get("turn_on_wol"))),
        Err(error) => log::warn!("WOL helper: LAN info report failed: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mac_parsing_and_packet() {
        assert_eq!(parse_mac("AA:bb:cc:dd:ee:01"), Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 1]));
        assert_eq!(parse_mac("aa-bb-cc-dd-ee-01"), parse_mac("aabbccddee01"));
        assert!(parse_mac("aa:bb:cc").is_none() && parse_mac("zz:bb:cc:dd:ee:01").is_none() && parse_mac("00:00:00:00:00:00").is_none());
        let packet = magic_packet([1, 2, 3, 4, 5, 6]);
        assert_eq!(packet.len(), 102);
        assert_eq!(&packet[..6], &[0xff; 6]);
        assert_eq!(&packet[96..], &[1, 2, 3, 4, 5, 6]);
    }
    #[test]
    fn report_body_has_inner_ip() {
        let body = report_body("aa:bb:cc:dd:ee:01", "192.168.2.29", "255.255.255.0");
        assert_eq!(body["inner_ip"], "192.168.2.29");
        assert_eq!(body["ip"], "192.168.2.29");
        assert_eq!(body["mac"], "aa:bb:cc:dd:ee:01");
        assert_eq!(body["subnet_mask"], "255.255.255.0");
    }

    #[test]
    fn directed_broadcast() {
        assert_eq!(subnet_broadcast("192.168.2.29", "255.255.255.0"), Some(Ipv4Addr::new(192, 168, 2, 255)));
        assert_eq!(subnet_broadcast("10.1.2.3", "255.255.0.0"), Some(Ipv4Addr::new(10, 1, 255, 255)));
        assert_eq!(subnet_broadcast("10.1.2.3", "255.0.255.0"), None);
        assert_eq!(subnet_broadcast("", "255.255.255.0"), None);
    }
    #[test]
    fn push_without_target_is_rejected() {
        assert!(handle_push(&serde_json::json!({})).is_err());
        assert!(handle_push(&serde_json::json!({"target_device": {"mac": "nope"}})).is_err());
    }
}
