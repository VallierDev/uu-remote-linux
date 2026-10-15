//! UU remote terminal (CT_TERMINAL) over BINARY_DATA_CHANNEL.
//! Frame layout recovered from the official Mac host codec (UURemoteServer 0x1001a7834 decode,
//! 0x1001a7480 encode): "TERM", version u8 = 1, type u8, two flag bytes, session_id u32 LE,
//! payload_len u32 LE (<= 1 MiB), payload. Type values are TerminalFrameType case index + 1.
//! Control payloads are JSON (cols/rows/shell/option, code/message, exit_code, password).
//! Sessions outlive a viewer connection (like the official tmux-backed sessions) and replay
//! a bounded scrollback on reattach. Terminal input/output bytes are never logged.

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::mpsc::UnboundedSender;

pub const OPEN: u8 = 1;
pub const STDIN: u8 = 2;
pub const RESIZE: u8 = 3;
pub const CLOSE: u8 = 4;
pub const STDOUT: u8 = 5;
pub const EXIT: u8 = 6;
pub const SIGNAL: u8 = 7;
pub const ERROR: u8 = 8;
pub const LIST_REQ: u8 = 9;
pub const LIST_RESP: u8 = 10;
pub const KILL_REQ: u8 = 11;
pub const KILL_RESP: u8 = 12;
pub const OPEN_RESP: u8 = 13;
pub const UNLOCK_REQ: u8 = 14;
pub const UNLOCK_RESP: u8 = 15;
pub const DETACH: u8 = 16;

const MAGIC: &[u8; 4] = b"TERM";
const HEADER: usize = 16;
const MAX_PAYLOAD: usize = 0x10_0000;
const SCROLLBACK: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame { pub kind: u8, pub flags: [u8; 2], pub session: u32, pub payload: Vec<u8> }

pub fn encode(kind: u8, session: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + payload.len());
    out.extend_from_slice(MAGIC);
    out.push(1);
    out.push(kind);
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&session.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

pub fn decode(data: &[u8]) -> Option<Frame> {
    if data.len() < HEADER || &data[..4] != MAGIC { return None; }
    let kind = data[5];
    if !(1..=16).contains(&kind) { return None; }
    let session = u32::from_le_bytes(data[8..12].try_into().ok()?);
    let len = u32::from_le_bytes(data[12..16].try_into().ok()?) as usize;
    if len > MAX_PAYLOAD || data.len() != HEADER + len { return None; }
    Some(Frame { kind, flags: [data[6], data[7]], session, payload: data[HEADER..].to_vec() })
}

pub fn is_frame(data: &[u8]) -> bool { data.len() >= 4 && &data[..4] == MAGIC }

struct Session {
    id: u32,
    shell: String,
    created_ms: u64,
    master: OwnedFd,
    pid: i32,
    sink: Mutex<Option<UnboundedSender<Vec<u8>>>>,
    scrollback: Mutex<VecDeque<u8>>,
}

impl Session {
    fn emit(&self, kind: u8, payload: &[u8]) -> bool {
        let mut sink = self.sink.lock().unwrap();
        if let Some(tx) = sink.as_ref() {
            if tx.send(encode(kind, self.id, payload)).is_ok() { return true; }
            *sink = None;
        }
        false
    }
    fn resize(&self, cols: u16, rows: u16) {
        let win = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &win); }
    }
    fn write(&self, data: &[u8]) {
        let mut off = 0;
        while off < data.len() {
            let n = unsafe { libc::write(self.master.as_raw_fd(), data[off..].as_ptr().cast(), data.len() - off) };
            if n <= 0 { break; }
            off += n as usize;
        }
    }
    fn signal(&self, sig: i32) { unsafe { libc::kill(-self.pid, sig); } }
}

fn registry() -> &'static Mutex<HashMap<u32, Arc<Session>>> {
    static REG: OnceLock<Mutex<HashMap<u32, Arc<Session>>>> = OnceLock::new();
    REG.get_or_init(Default::default)
}
static NEXT_ID: AtomicU32 = AtomicU32::new(1);

fn now_ms() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64 }

fn spawn(requested_id: u32, cols: u16, rows: u16, shell: Option<&str>) -> Result<Arc<Session>, String> {
    use std::os::unix::process::CommandExt;
    let win = nix::pty::Winsize { ws_row: rows.max(1), ws_col: cols.max(1), ws_xpixel: 0, ws_ypixel: 0 };
    let pty = nix::pty::openpty(Some(&win), None).map_err(|e| format!("openpty failed: {e}"))?;
    let shell = shell.filter(|s| s.starts_with('/') && std::path::Path::new(s).exists()).map(str::to_owned)
        .or_else(|| std::env::var("SHELL").ok().filter(|s| std::path::Path::new(s).exists()))
        .unwrap_or_else(|| "/bin/bash".into());
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let slave = pty.slave;
    let io = |fd: &OwnedFd| fd.try_clone().map(std::process::Stdio::from).map_err(|e| format!("pty dup failed: {e}"));
    let mut cmd = std::process::Command::new(&shell);
    cmd.arg("-l").current_dir(&home)
        .env("TERM", "xterm-256color").env("COLORTERM", "truecolor")
        .env("LANG", std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".into()))
        .stdin(io(&slave)?).stdout(io(&slave)?).stderr(io(&slave)?);
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 { return Err(std::io::Error::last_os_error()); }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 { return Err(std::io::Error::last_os_error()); }
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| format!("shell spawn failed: {e}"))?;
    drop(slave);
    // Official Mac 4.42.0 (TerminalManager.open) allocates an ID only for OPEN(0).
    let id = if requested_id != 0 { requested_id } else {
        loop {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            if id != 0 && !registry().lock().unwrap().contains_key(&id) { break id; }
        }
    };
    let session = Arc::new(Session { id, shell: shell.clone(), created_ms: now_ms(), pid: child.id() as i32,
        master: pty.master, sink: Mutex::new(None), scrollback: Mutex::new(VecDeque::new()) });
    let mut reader = std::fs::File::from(session.master.try_clone().map_err(|e| format!("pty dup failed: {e}"))?);
    let worker = session.clone();
    std::thread::Builder::new().name(format!("uu-term-{id}")).spawn(move || {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    {
                        let mut sb = worker.scrollback.lock().unwrap();
                        sb.extend(&buf[..n]);
                        let excess = sb.len().saturating_sub(SCROLLBACK);
                        sb.drain(..excess);
                    }
                    worker.emit(STDOUT, &buf[..n]);
                }
            }
        }
        let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
        log::info!("Terminal session exited: id={} exit_code={}", worker.id, code);
        worker.emit(EXIT, serde_json::json!({"exit_code": code}).to_string().as_bytes());
        registry().lock().unwrap().remove(&worker.id);
    }).map_err(|e| format!("reader thread failed: {e}"))?;
    registry().lock().unwrap().insert(id, session.clone());
    log::info!("Terminal session spawned: id={} cols={} rows={}", id, cols, rows);
    Ok(session)
}

/// Ends every terminal session (interactive shells ignore SIGTERM, which otherwise
/// makes systemd wait for its stop timeout). Called when the server shuts down.
pub fn shutdown_all() {
    let sessions: Vec<Arc<Session>> = registry().lock().unwrap().values().cloned().collect();
    for s in &sessions { s.signal(libc::SIGHUP); }
    if !sessions.is_empty() {
        std::thread::sleep(std::time::Duration::from_millis(300));
        for s in &sessions { s.signal(libc::SIGKILL); }
        log::info!("Terminal sessions closed on shutdown: count={}", sessions.len());
    }
}

fn json(payload: &[u8]) -> serde_json::Value { serde_json::from_slice(payload).unwrap_or(serde_json::Value::Null) }
fn dims(v: &serde_json::Value) -> (u16, u16) {
    let get = |k: &str, d: u64| v.get(k).and_then(|x| x.as_u64().or_else(|| x.as_f64().map(|f| f as u64))).unwrap_or(d).clamp(1, 1000) as u16;
    (get("cols", 80), get("rows", 24))
}
fn signal_number(payload: &[u8]) -> i32 {
    let text = String::from_utf8_lossy(payload);
    let v = json(payload);
    let name = v.as_str().map(str::to_owned).or_else(|| v.as_object().and_then(|o| o.values().find_map(|x| x.as_str().map(str::to_owned))))
        .unwrap_or_else(|| text.into_owned());
    if let Some(n) = v.as_object().and_then(|o| o.values().find_map(|x| x.as_i64())) { return n as i32; }
    match name.trim().trim_start_matches("SIG") {
        "TERM" => libc::SIGTERM, "KILL" => libc::SIGKILL, "HUP" => libc::SIGHUP, "QUIT" => libc::SIGQUIT,
        "ABRT" => libc::SIGABRT, "PIPE" => libc::SIGPIPE, "FPE" => libc::SIGFPE, "SEGV" => libc::SIGSEGV, "ILL" => libc::SIGILL,
        _ => libc::SIGINT,
    }
}
fn reply(code: i64, message: &str) -> Vec<u8> { serde_json::json!({"code": code, "message": message}).to_string().into_bytes() }

/// One viewer connection's terminal endpoint; `out` sends complete frames on BINARY_DATA_CHANNEL.
pub struct TerminalConn { out: UnboundedSender<Vec<u8>> }

impl TerminalConn {
    pub fn new(out: UnboundedSender<Vec<u8>>) -> Self { Self { out } }
    fn send(&self, kind: u8, session: u32, payload: &[u8]) { let _ = self.out.send(encode(kind, session, payload)); }

    pub fn handle(&self, data: &[u8]) {
        let Some(frame) = decode(data) else { log::warn!("Terminal frame rejected: bytes={}", data.len()); return; };
        if frame.kind != STDIN { log::info!("Terminal frame: type={} session={} payload_bytes={}", frame.kind, frame.session, frame.payload.len()); }
        let existing = registry().lock().unwrap().get(&frame.session).cloned();
        match frame.kind {
            OPEN => {
                let v = json(&frame.payload);
                let (cols, rows) = dims(&v);
                let session = match existing {
                    Some(s) => { s.resize(cols, rows); Ok(s) }
                    None => spawn(frame.session, cols, rows, v.get("shell").and_then(|s| s.as_str())),
                };
                match session {
                    Ok(s) => {
                        // Queue the acknowledgement before the PTY reader can emit live output.
                        let mut sink = s.sink.lock().unwrap();
                        self.send(OPEN_RESP, s.id, &reply(0, "ok"));
                        *sink = Some(self.out.clone());
                        let replay: Vec<u8> = s.scrollback.lock().unwrap().iter().copied().collect();
                        if !replay.is_empty() { self.send(STDOUT, s.id, &replay); }
                    }
                    Err(e) => { log::warn!("Terminal open failed: {}", e); self.send(OPEN_RESP, frame.session, &reply(1, &e)); }
                }
            }
            STDIN => if let Some(s) = existing { s.write(&frame.payload) },
            RESIZE => if let Some(s) = existing { let (c, r) = dims(&json(&frame.payload)); s.resize(c, r) },
            SIGNAL => if let Some(s) = existing { s.signal(signal_number(&frame.payload)) },
            CLOSE => if let Some(s) = existing { *s.sink.lock().unwrap() = None; s.signal(libc::SIGHUP); },
            DETACH => if let Some(s) = existing { *s.sink.lock().unwrap() = None },
            KILL_REQ => {
                let id = if frame.session != 0 { frame.session } else {
                    json(&frame.payload).get("session_id").and_then(|x| x.as_u64()).unwrap_or(0) as u32
                };
                let target = registry().lock().unwrap().get(&id).cloned();
                match target {
                    Some(s) => { *s.sink.lock().unwrap() = None; s.signal(libc::SIGHUP); s.signal(libc::SIGKILL); self.send(KILL_RESP, id, &reply(0, "ok")); }
                    None => self.send(KILL_RESP, id, &reply(1, "Session not found")),
                }
            }
            LIST_REQ => {
                let sessions: Vec<serde_json::Value> = registry().lock().unwrap().values().map(|s| {
                    let attached = s.sink.lock().unwrap().is_some();
                    serde_json::json!({"session_id": s.id, "name": format!("bash-{}", s.id), "shell": s.shell,
                        "state": if attached { "running" } else { "detached" }, "created_at_ms": s.created_ms})
                }).collect();
                log::info!("Terminal list sessions: count={}", sessions.len());
                // Official Mac 4.42.0 (handleListSessionsReq) sends a bare array with the request's header ID.
                self.send(LIST_RESP, frame.session, serde_json::Value::Array(sessions).to_string().as_bytes());
            }
            UNLOCK_REQ => self.send(UNLOCK_RESP, frame.session, &reply(0, "ok")),
            _ => self.send(ERROR, frame.session, &reply(1, "unsupported frame")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frame_round_trip_matches_official_layout() {
        let f = encode(OPEN_RESP, 7, b"{}");
        assert_eq!(&f[..8], &[b'T', b'E', b'R', b'M', 1, 13, 0, 0]);
        assert_eq!(&f[8..16], &[7, 0, 0, 0, 2, 0, 0, 0]);
        assert_eq!(decode(&f), Some(Frame { kind: 13, flags: [0, 0], session: 7, payload: b"{}".to_vec() }));
        assert_eq!(decode(&f[..17]), None);
        let mut bad = f.clone(); bad[5] = 17; assert_eq!(decode(&bad), None);
    }
    #[test]
    fn signal_names_map_to_posix_numbers() {
        assert_eq!(signal_number(b"SIGTERM"), libc::SIGTERM);
        assert_eq!(signal_number(br#"{"signal":"SIGKILL"}"#), libc::SIGKILL);
        assert_eq!(signal_number(b"whatever"), libc::SIGINT);
    }
    #[test]
    fn list_reply_is_an_array_and_preserves_request_id() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let conn = TerminalConn::new(tx);
        for request in [0, 7, u32::MAX] {
            conn.handle(&encode(LIST_REQ, request, &[]));
            let response = decode(&rx.try_recv().unwrap()).unwrap();
            assert_eq!(response.kind, LIST_RESP);
            assert_eq!(response.session, request);
            assert!(json(&response.payload).is_array());
        }
    }
    #[test]
    fn open_spawns_shell_and_streams_output() {
        for requested_id in [0, u32::MAX] {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let conn = TerminalConn::new(tx);
            conn.handle(&encode(OPEN, requested_id, br#"{"cols":100,"rows":30,"shell":"/bin/sh"}"#));
            let resp = decode(&rx.blocking_recv().unwrap()).unwrap();
            assert_eq!(resp.kind, OPEN_RESP);
            assert_eq!(json(&resp.payload)["code"], 0);
            assert_ne!(resp.session, 0);
            if requested_id != 0 { assert_eq!(resp.session, requested_id); }

            let (list_tx, mut list_rx) = tokio::sync::mpsc::unbounded_channel();
            TerminalConn::new(list_tx).handle(&encode(LIST_REQ, 7, &[]));
            let list = decode(&list_rx.try_recv().unwrap()).unwrap();
            let sessions: Vec<serde_json::Value> = serde_json::from_slice(&list.payload).unwrap();
            let session = sessions.iter().find(|s| s["session_id"] == resp.session).unwrap();
            assert_eq!(session["shell"], "/bin/sh");
            assert_eq!(session["state"], "running");

            conn.handle(&encode(STDIN, resp.session, b"echo uu-term-ok; exit 3\n"));
            let mut seen = Vec::new();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut exit = None;
            while std::time::Instant::now() < deadline && exit.is_none() {
                if let Ok(m) = rx.try_recv() {
                    let f = decode(&m).unwrap();
                    assert_eq!(f.session, resp.session);
                    if f.kind == STDOUT { seen.extend(f.payload) } else if f.kind == EXIT { exit = Some(json(&f.payload)["exit_code"].clone()) }
                } else { std::thread::sleep(std::time::Duration::from_millis(20)); }
            }
            assert!(String::from_utf8_lossy(&seen).contains("uu-term-ok"));
            assert_eq!(exit, Some(serde_json::json!(3)));
        }
    }
}

/// Terminal RPCs carried in Message 21 RpcRequest: 28 terminal_check_env_req (CheckEnvReq, empty)
/// -> RpcResponse 24 CheckEnvResp{1 code, 2 message}; 29 RenameSessionReq{1 session_id, 2 name}
/// -> RpcResponse 26 RenameSessionResp{1 session_id, 2 code, 3 message}. Returns the RpcResponse body.
pub fn rpc_response(rpc: &[crate::protocol::WireField]) -> Option<Vec<u8>> {
    use crate::protocol::WireValue;
    fn varint(mut v: u64, out: &mut Vec<u8>) { while v >= 128 { out.push((v as u8 & 127) | 128); v >>= 7; } out.push(v as u8); }
    fn bytes(field: u64, v: &[u8], out: &mut Vec<u8>) { varint(field << 3 | 2, out); varint(v.len() as u64, out); out.extend_from_slice(v); }
    let header = rpc.iter().find_map(|f| match (f.number, &f.value) { (1, WireValue::Bytes(b)) => Some(*b), _ => None }).unwrap_or(&[]);
    let mut out = Vec::new();
    bytes(1, header, &mut out);
    if rpc.iter().any(|f| f.number == 28) {
        let mut body = Vec::new();
        bytes(2, b"ok", &mut body); // code 0 (default) = available
        bytes(24, &body, &mut out);
        log::info!("Terminal check_env answered: available");
        return Some(out);
    }
    if let Some(WireValue::Bytes(req)) = rpc.iter().find(|f| f.number == 29).map(|f| &f.value) {
        let fields = crate::protocol::decode_wire(req).ok()?;
        let session = fields.iter().find_map(|f| match (f.number, &f.value) { (1, WireValue::Varint(v)) => Some(*v), _ => None }).unwrap_or(0);
        let mut body = Vec::new();
        if session != 0 { varint(1 << 3, &mut body); varint(session, &mut body); }
        bytes(3, b"ok", &mut body);
        bytes(26, &body, &mut out);
        log::info!("Terminal rename answered: session={}", session);
        return Some(out);
    }
    None
}
