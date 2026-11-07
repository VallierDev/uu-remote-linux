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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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
const OUTPUT_CHUNK: usize = 16 * 1024;

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
    last_active_ms: AtomicU64,
    master: OwnedFd,
    pid: i32,
    sink: Mutex<Option<UnboundedSender<Vec<u8>>>>,
    scrollback: Mutex<VecDeque<u8>>,
    size: Mutex<(u16, u16)>,
}

impl Session {
    fn touch(&self) { self.last_active_ms.fetch_max(now_ms(), Ordering::Relaxed); }
    /// Lock order sink → scrollback matches OPEN, so a reattach snapshot sees each
    /// chunk either in the replay or live, never both.
    fn record_output(&self, data: &[u8]) {
        self.touch();
        let mut sink = self.sink.lock().unwrap();
        {
            let mut sb = self.scrollback.lock().unwrap();
            sb.extend(data);
            let excess = sb.len().saturating_sub(SCROLLBACK);
            sb.drain(..excess);
        }
        if let Some(tx) = sink.as_ref() {
            if tx.send(encode(STDOUT, self.id, data)).is_err() { *sink = None; }
        }
    }
    fn emit(&self, kind: u8, payload: &[u8]) -> bool {
        let mut sink = self.sink.lock().unwrap();
        if let Some(tx) = sink.as_ref() {
            if tx.send(encode(kind, self.id, payload)).is_ok() { return true; }
            *sink = None;
        }
        false
    }
    // The size lock is held across the ioctl so a delayed restore cannot reapply a stale size.
    fn resize(&self, cols: u16, rows: u16) {
        let mut size = self.size.lock().unwrap();
        *size = (cols, rows);
        self.set_winsize(cols, rows);
    }
    fn set_winsize(&self, cols: u16, rows: u16) {
        let win = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &win); }
    }
    /// Full-screen programs repaint only on SIGWINCH, which an unchanged size never raises:
    /// shrink by one row, then restore the latest requested size after a pause.
    fn force_redraw(self: &Arc<Self>) {
        {
            let size = self.size.lock().unwrap();
            if size.1 <= 1 { return; }
            self.set_winsize(size.0, size.1 - 1);
        }
        let s = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            let size = s.size.lock().unwrap();
            s.set_winsize(size.0, size.1);
        });
    }
    fn write(&self, data: &[u8]) {
        if !data.is_empty() { self.touch(); }
        let mut off = 0;
        while off < data.len() {
            let n = unsafe { libc::write(self.master.as_raw_fd(), data[off..].as_ptr().cast(), data.len() - off) };
            if n <= 0 { break; }
            off += n as usize;
        }
    }
    fn signal(&self, sig: i32) { unsafe { libc::kill(-self.pid, sig); } }
    fn signal_foreground(&self, sig: i32) {
        self.touch();
        // Job-control shells put commands such as top in their own process group.
        let pgid = unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) };
        if pgid > 0 { unsafe { libc::kill(-pgid, sig); } } else { self.signal(sig); }
    }
    fn terminate(&self) {
        // Job-control shells give foreground/background jobs separate process groups.
        // Limit cleanup to this PTY's POSIX session, never other desktop/tool peers.
        if self.pid <= 0 { return; }
        #[cfg(target_os = "linux")]
        if let Ok(entries) = std::fs::read_dir("/proc") {
            let mut groups = std::collections::HashSet::new();
            for entry in entries.flatten() {
                let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else { continue; };
                if unsafe { libc::getsid(pid) } == self.pid {
                    let group = unsafe { libc::getpgid(pid) };
                    if group > 0 && group != self.pid { groups.insert(group); }
                }
            }
            for group in groups { unsafe { libc::kill(-group, libc::SIGKILL); } }
        }
        self.signal_foreground(libc::SIGKILL);
        self.signal(libc::SIGKILL);
    }
}

fn registry() -> &'static Mutex<HashMap<u32, Arc<Session>>> {
    static REG: OnceLock<Mutex<HashMap<u32, Arc<Session>>>> = OnceLock::new();
    REG.get_or_init(Default::default)
}
static NEXT_ID: AtomicU32 = AtomicU32::new(1);

fn changes() -> &'static tokio::sync::watch::Sender<u64> {
    static CHANGES: OnceLock<tokio::sync::watch::Sender<u64>> = OnceLock::new();
    CHANGES.get_or_init(|| tokio::sync::watch::channel(0).0)
}
fn notify_changes() { changes().send_modify(|revision| *revision = revision.wrapping_add(1)); }
pub fn subscribe_changes() -> tokio::sync::watch::Receiver<u64> { changes().subscribe() }
pub fn changed_message(sequence: u64, timestamp: u64) -> Vec<u8> {
    // Official Mac 4.42.0: Message name map 0x100a1eb98 gives field 28;
    // notifyTerminalListInvalidated (0x1001a341c) sends the empty message on BINARY (channel 3).
    crate::initialization::envelope(sequence, timestamp, 28, &[])
}

fn remove_session(session: &Arc<Session>) -> bool {
    let mut sessions = registry().lock().unwrap();
    // An old reader may finish after OPEN has reused the same requested ID.
    if sessions.get(&session.id).is_some_and(|current| Arc::ptr_eq(current, session)) {
        sessions.remove(&session.id);
        drop(sessions);
        notify_changes(); true
    } else { false }
}

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
    let created_ms = now_ms();
    let session = Arc::new(Session { id, shell: shell.clone(), created_ms, last_active_ms: AtomicU64::new(created_ms), pid: child.id() as i32,
        master: pty.master, sink: Mutex::new(None), scrollback: Mutex::new(VecDeque::new()), size: Mutex::new((cols, rows)) });
    let mut reader = std::fs::File::from(session.master.try_clone().map_err(|e| format!("pty dup failed: {e}"))?);
    registry().lock().unwrap().insert(id, session.clone());
    let worker = session.clone();
    std::thread::Builder::new().name(format!("uu-term-{id}")).spawn(move || {
        let mut buf = vec![0u8; OUTPUT_CHUNK];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => worker.record_output(&buf[..n]),
            }
        }
        let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
        log::info!("Terminal session exited: id={} exit_code={}", worker.id, code);
        worker.emit(EXIT, serde_json::json!({"exit_code": code}).to_string().as_bytes());
        remove_session(&worker);
    }).map_err(|e| {
        remove_session(&session); session.terminate(); format!("reader thread failed: {e}")
    })?;
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
        for s in &sessions { s.terminate(); }
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
pub struct TerminalConn { out: UnboundedSender<Vec<u8>>, closed: AtomicBool }

impl TerminalConn {
    pub fn new(out: UnboundedSender<Vec<u8>>) -> Self { Self { out, closed: AtomicBool::new(false) } }
    fn send(&self, kind: u8, session: u32, payload: &[u8]) { let _ = self.out.send(encode(kind, session, payload)); }
    pub fn disconnect(&self) {
        self.closed.store(true, Ordering::Release);
        let sessions: Vec<_> = registry().lock().unwrap().values().cloned().collect();
        let mut changed = false;
        for s in sessions {
            let mut sink = s.sink.lock().unwrap();
            if sink.as_ref().is_some_and(|tx| tx.same_channel(&self.out)) {
                *sink = None; s.touch(); changed = true;
            }
        }
        if changed { notify_changes(); }
    }

    pub fn handle(&self, data: &[u8]) {
        if self.closed.load(Ordering::Acquire) { return; }
        let Some(frame) = decode(data) else { log::warn!("Terminal frame rejected: bytes={}", data.len()); return; };
        if frame.kind != STDIN { log::info!("Terminal frame: type={} session={} payload_bytes={}", frame.kind, frame.session, frame.payload.len()); }
        let existing = registry().lock().unwrap().get(&frame.session).cloned();
        match frame.kind {
            OPEN => {
                let v = json(&frame.payload);
                let (cols, rows) = dims(&v);
                let reattach = existing.is_some();
                let session = match existing {
                    Some(s) => { s.resize(cols, rows); Ok(s) }
                    None => spawn(frame.session, cols, rows, v.get("shell").and_then(|s| s.as_str())),
                };
                match session {
                    Ok(s) => {
                        s.touch();
                        // Queue the acknowledgement before the PTY reader can emit live output.
                        let mut sink = s.sink.lock().unwrap();
                        self.send(OPEN_RESP, s.id, &reply(0, "ok"));
                        *sink = Some(self.out.clone());
                        let replay: Vec<u8> = s.scrollback.lock().unwrap().iter().copied().collect();
                        // A full scrollback exceeds SCTP's 64 KiB outbound message limit.
                        for chunk in replay.chunks(OUTPUT_CHUNK) { self.send(STDOUT, s.id, chunk); }
                        drop(sink);
                        // The replay tail starts mid-redraw for full-screen programs.
                        if reattach { s.force_redraw(); }
                        notify_changes();
                    }
                    Err(e) => { log::warn!("Terminal open failed: {}", e); self.send(OPEN_RESP, frame.session, &reply(1, &e)); }
                }
            }
            STDIN | RESIZE | SIGNAL | CLOSE | DETACH => if let Some(s) = existing {
                let mut sink = s.sink.lock().unwrap();
                // Reattach transfers ownership. Late input/DETACH from the old viewer
                // must not affect the new attachment or inject a second copy of input.
                if !sink.as_ref().is_some_and(|tx| tx.same_channel(&self.out)) { return; }
                match frame.kind {
                    // PTY writes may block during a large paste. Keep output draining.
                    STDIN => { drop(sink); s.write(&frame.payload); }
                    RESIZE => { let (c, r) = dims(&json(&frame.payload)); s.resize(c, r); }
                    SIGNAL => s.signal_foreground(signal_number(&frame.payload)),
                    CLOSE | DETACH => {
                        *sink = None; s.touch();
                        if frame.kind == CLOSE { s.signal(libc::SIGHUP); }
                        notify_changes();
                    }
                    _ => unreachable!(),
                }
            },
            KILL_REQ => {
                // Official Mac 4.42.0 handleKillSessionReq (0x100189ca0, 0x10018be64):
                // JSON session_id selects the target; the frame header correlates the reply.
                let Some(id) = json(&frame.payload).get("session_id").and_then(|x| x.as_u64()).and_then(|id| u32::try_from(id).ok()) else {
                    self.send(KILL_RESP, frame.session, &reply(400, "Invalid payload")); return;
                };
                let target = registry().lock().unwrap().remove(&id);
                match target {
                    Some(s) => {
                        s.terminate();
                        self.send(KILL_RESP, frame.session, &reply(0, "ok"));
                        notify_changes();
                        log::info!("Terminal session terminated: id={} request={}", id, frame.session);
                    }
                    None => self.send(KILL_RESP, frame.session, &reply(404, "Session not found")),
                }
            }
            LIST_REQ => {
                let sessions: Vec<serde_json::Value> = registry().lock().unwrap().values().map(|s| {
                    let attached = s.sink.lock().unwrap().as_ref().is_some_and(|tx| !tx.is_closed());
                    let name = std::path::Path::new(&s.shell).file_name().and_then(|n| n.to_str()).unwrap_or("shell");
                    // Official Mac 4.42.0 snapshot at 0x1001b82c8–0x1001b8524
                    // includes last_active_ms even when the session is detached.
                    serde_json::json!({"session_id": s.id, "name": format!("{}-{}", name, s.id), "shell": s.shell,
                        "state": if attached { "running" } else { "detached" }, "created_at_ms": s.created_ms,
                        "last_active_ms": s.last_active_ms.load(Ordering::Relaxed)})
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
    fn kill_uses_payload_id_preserves_request_id_and_updates_list_before_reply() {
        let victim = spawn(u32::MAX - 5, 80, 24, Some("/bin/sh")).unwrap();
        let decoy = spawn(u32::MAX - 6, 80, 24, Some("/bin/sh")).unwrap();
        struct Cleanup(Vec<Arc<Session>>);
        impl Drop for Cleanup {
            fn drop(&mut self) { for s in &self.0 { s.signal(libc::SIGHUP); s.signal(libc::SIGKILL); } }
        }
        let _cleanup = Cleanup(vec![victim.clone(), decoy.clone()]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let conn = TerminalConn::new(tx);
        let payload = serde_json::json!({"session_id": victim.id}).to_string();
        // A request ID may happen to equal another live session ID. Never kill it.
        conn.handle(&encode(KILL_REQ, decoy.id, payload.as_bytes()));
        let response = decode(&rx.try_recv().unwrap()).unwrap();
        assert_eq!((response.kind, response.session), (KILL_RESP, decoy.id));
        assert_eq!(json(&response.payload)["code"], 0);
        conn.handle(&encode(LIST_REQ, 123, &[]));
        let response = decode(&rx.try_recv().unwrap()).unwrap();
        let list = json(&response.payload);
        assert!(!list.as_array().unwrap().iter().any(|s| s["session_id"] == victim.id), "terminated session must be absent before acknowledgement");
        assert!(list.as_array().unwrap().iter().any(|s| s["session_id"] == decoy.id), "request correlation must not select a session");
        assert_eq!(unsafe { libc::kill(decoy.pid, 0) }, 0);
        conn.handle(&encode(KILL_REQ, 123, payload.as_bytes()));
        let response = decode(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(response.session, 123);
        assert_eq!(json(&response.payload)["code"], 404);
        for invalid in [b"{}".as_slice(), b"", br#"{"session_id":-1}"#, br#"{"session_id":4294967296}"#] {
            conn.handle(&encode(KILL_REQ, decoy.id, invalid));
            let response = decode(&rx.try_recv().unwrap()).unwrap();
            assert_eq!(response.session, decoy.id);
            assert_eq!(json(&response.payload)["code"], 400);
            assert!(registry().lock().unwrap().contains_key(&decoy.id));
        }
    }
    #[test]
    fn session_changed_matches_official_empty_binary_notification() {
        use crate::protocol::WireValue;
        let message = changed_message(17, 1234);
        let fields = crate::protocol::decode_wire(&message).unwrap();
        assert_eq!(fields.iter().map(|f| f.number).collect::<Vec<_>>(), vec![1, 2, 28]);
        assert_eq!(fields[2].value, WireValue::Bytes(&[]));
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn kill_ends_foreground_and_background_jobs() {
        let session = spawn(u32::MAX - 8, 80, 24, Some("/bin/sh")).unwrap();
        struct Cleanup(Arc<Session>);
        impl Drop for Cleanup { fn drop(&mut self) { self.0.terminate(); } }
        let _cleanup = Cleanup(session.clone());
        session.write(b"sleep 30 & sleep 30\n");
        let mut jobs = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            jobs = std::fs::read_to_string(format!("/proc/{}/task/{}/children", session.pid, session.pid)).unwrap_or_default()
                .split_whitespace().filter_map(|s| s.parse::<i32>().ok())
                .filter(|pid| std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|s| s.trim() == "sleep")).collect();
            if jobs.len() == 2 { break; }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(jobs.len(), 2, "foreground and background jobs must be running");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        TerminalConn::new(tx).handle(&encode(KILL_REQ, 123, serde_json::json!({"session_id":session.id}).to_string().as_bytes()));
        assert_eq!(json(&decode(&rx.try_recv().unwrap()).unwrap().payload)["code"], 0);
        jobs.push(session.pid);
        let alive = |pid| std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .is_ok_and(|s| s.rsplit_once(") ").is_some_and(|(_, fields)| !fields.starts_with('Z')));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline && jobs.iter().any(|pid| alive(*pid)) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!jobs.iter().any(|pid| alive(*pid)), "termination must end all job-control groups");
    }
    #[test]
    fn reattach_rejects_old_input_and_detach_but_preserves_intentional_repeats() {
        let pty = nix::pty::openpty(None, None).unwrap();
        let id = u32::MAX - 7;
        let mut termios = nix::sys::termios::tcgetattr(&pty.slave).unwrap();
        nix::sys::termios::cfmakeraw(&mut termios);
        nix::sys::termios::tcsetattr(&pty.slave, nix::sys::termios::SetArg::TCSANOW, &termios).unwrap();
        let session = Arc::new(Session { id, shell: "/bin/sh".into(), created_ms: now_ms(), last_active_ms: AtomicU64::new(now_ms()),
            master: pty.master, pid: 0, sink: Mutex::new(None), scrollback: Mutex::new(VecDeque::new()), size: Mutex::new((80, 24)) });
        registry().lock().unwrap().insert(id, session.clone());
        struct Cleanup(Arc<Session>);
        impl Drop for Cleanup { fn drop(&mut self) { remove_session(&self.0); } }
        let _cleanup = Cleanup(session.clone());
        let (old_tx, _old_rx) = tokio::sync::mpsc::unbounded_channel();
        let old = TerminalConn::new(old_tx);
        let (new_tx, _new_rx) = tokio::sync::mpsc::unbounded_channel();
        let new = TerminalConn::new(new_tx);
        old.handle(&encode(OPEN, id, b"{}"));
        new.handle(&encode(OPEN, id, b"{}"));
        old.handle(&encode(STDIN, id, b"stale"));
        old.handle(&encode(DETACH, id, &[]));
        old.disconnect();
        let input = b"\x1b[A";
        new.handle(&encode(STDIN, id, input));
        new.handle(&encode(STDIN, id, input));
        let mut slave = std::fs::File::from(pty.slave);
        let mut bytes = [0; 6];
        slave.read_exact(&mut bytes).unwrap();
        assert!(bytes == input.repeat(2).as_slice(), "only current input should arrive, including intentional repeats");
        assert!(session.sink.lock().unwrap().as_ref().is_some_and(|tx| tx.same_channel(&new.out)));
        new.disconnect();
        assert!(session.sink.lock().unwrap().is_none());
        new.handle(&encode(OPEN, id, b"{}"));
        assert!(session.sink.lock().unwrap().is_none(), "a closed connection must not reattach");
    }
    #[test]
    fn nonempty_list_matches_official_snapshot_and_survives_reattach() {
        // Official Mac 4.42.0 TerminalSession snapshot (0x1001b82c8–0x1001b8524):
        // all six fields exist, including the Int64 last_active_ms timestamp.
        #[derive(serde::Deserialize)]
        struct Snapshot { session_id: u32, name: String, shell: String, state: String,
            created_at_ms: i64, last_active_ms: i64 }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut conn = TerminalConn::new(tx);
        let id = u32::MAX - 3;
        conn.handle(&encode(OPEN, id, br#"{"cols":80,"rows":24,"shell":"/bin/sh"}"#));
        assert_eq!(decode(&rx.blocking_recv().unwrap()).unwrap().kind, OPEN_RESP);
        struct Cleanup(u32);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                if let Some(s) = registry().lock().unwrap().get(&self.0).cloned() { s.signal(libc::SIGHUP); }
            }
        }
        let _cleanup = Cleanup(id);
        let snapshot = |request| {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            TerminalConn::new(tx).handle(&encode(LIST_REQ, request, &[]));
            let reply = decode(&rx.try_recv().unwrap()).unwrap();
            assert_eq!((reply.kind, reply.session), (LIST_RESP, request));
            serde_json::from_slice::<Vec<Snapshot>>(&reply.payload).expect("official nonempty session schema")
                .into_iter().find(|s| s.session_id == id).unwrap()
        };
        let first = snapshot(100);
        assert_eq!(first.state, "running");
        assert_eq!(first.shell, "/bin/sh");
        assert_eq!(first.name, format!("sh-{id}"));
        assert!(first.created_at_ms > 0 && first.last_active_ms >= first.created_at_ms);
        for request in [101, 102] {
            conn.handle(&encode(DETACH, id, &[]));
            let detached = snapshot(request);
            assert_eq!(detached.state, "detached");
            std::thread::sleep(std::time::Duration::from_millis(5));
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            conn = TerminalConn::new(tx);
            conn.handle(&encode(OPEN, id, br#"{"cols":80,"rows":24}"#));
            assert_eq!(decode(&rx.blocking_recv().unwrap()).unwrap().kind, OPEN_RESP);
            let attached = snapshot(request + 100);
            assert_eq!(attached.state, "running");
            assert_eq!(attached.created_at_ms, first.created_at_ms);
            assert!(attached.last_active_ms > detached.last_active_ms);
        }
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn interrupt_reaches_foreground_job_without_exiting_shell() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let conn = TerminalConn::new(tx);
        let id = u32::MAX - 4;
        conn.handle(&encode(OPEN, id, br#"{"cols":80,"rows":24,"shell":"/bin/sh"}"#));
        assert_eq!(decode(&rx.blocking_recv().unwrap()).unwrap().kind, OPEN_RESP);
        let session = registry().lock().unwrap().get(&id).unwrap().clone();
        struct Cleanup(Arc<Session>);
        impl Drop for Cleanup {
            fn drop(&mut self) { self.0.signal_foreground(libc::SIGKILL); self.0.signal(libc::SIGHUP); }
        }
        let _cleanup = Cleanup(session.clone());
        let foreground = || unsafe { libc::tcgetpgrp(session.master.as_raw_fd()) };
        // The shell can foreground its child before exec resets inherited signals.
        let job_ready = || {
            let pgid = foreground();
            pgid > 0 && pgid != session.pid &&
                std::fs::read_to_string(format!("/proc/{pgid}/comm")).is_ok_and(|name| name.trim() == "sleep")
        };
        conn.handle(&encode(STDIN, id, b"sleep 30\n"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline && !job_ready() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(job_ready(), "job must be executing in a separate foreground group");
        conn.handle(&encode(SIGNAL, id, br#"{"signal":"SIGINT"}"#));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline && foreground() != session.pid {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(foreground(), session.pid, "interrupt must return control to the shell");
        assert_eq!(unsafe { libc::kill(session.pid, 0) }, 0, "shell must stay alive");
    }
    #[test]
    fn force_redraw_shrinks_then_restores_latest_size() {
        let pty = nix::pty::openpty(None, None).unwrap();
        let session = Arc::new(Session { id: u32::MAX - 2, shell: "/bin/sh".into(), created_ms: now_ms(),
            last_active_ms: AtomicU64::new(now_ms()),
            master: pty.master, pid: 0, sink: Mutex::new(None),
            scrollback: Mutex::new(VecDeque::new()), size: Mutex::new((80, 24)) });
        let rows = |s: &Session| {
            let mut win = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
            unsafe { libc::ioctl(s.master.as_raw_fd(), libc::TIOCGWINSZ as _, &mut win); }
            (win.ws_col, win.ws_row)
        };
        session.resize(100, 30);
        session.force_redraw();
        assert_eq!(rows(&session), (100, 29));
        session.resize(120, 40);
        std::thread::sleep(std::time::Duration::from_millis(400));
        assert_eq!(rows(&session), (120, 40), "restore must use the latest requested size");
    }
    #[test]
    fn reconnect_replays_large_history_in_bounded_frames() {
        let pty = nix::pty::openpty(None, None).unwrap();
        let id = u32::MAX - 1;
        let history: Vec<u8> = (0..SCROLLBACK).map(|n| (n % 251) as u8).collect();
        let session = Arc::new(Session { id, shell: "/bin/sh".into(), created_ms: now_ms(),
            last_active_ms: AtomicU64::new(now_ms()),
            master: pty.master, pid: 0, sink: Mutex::new(None),
            scrollback: Mutex::new(history.iter().copied().collect()), size: Mutex::new((80, 24)) });
        registry().lock().unwrap().insert(id, session.clone());
        struct Cleanup(u32);
        impl Drop for Cleanup { fn drop(&mut self) { registry().lock().unwrap().remove(&self.0); } }
        let _cleanup = Cleanup(id);

        for _ in 0..2 {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let conn = TerminalConn::new(tx);
            conn.handle(&encode(OPEN, id, br#"{"cols":80,"rows":24}"#));
            let opened = decode(&rx.try_recv().unwrap()).unwrap();
            assert_eq!(opened.kind, OPEN_RESP);
            assert_eq!(opened.session, id);
            let mut replay = Vec::new();
            while replay.len() < history.len() {
                let packet = rx.try_recv().unwrap();
                // SCTP's default outbound limit includes the TERM header.
                assert!(packet.len() <= 65536, "terminal replay exceeds the SCTP message limit");
                let frame = decode(&packet).unwrap();
                assert_eq!(frame.kind, STDOUT);
                assert_eq!(frame.session, id);
                replay.extend(frame.payload);
            }
            assert!(replay == history, "terminal replay must preserve every byte in order");
            assert!(session.emit(STDOUT, b"live output"));
            assert_eq!(decode(&rx.try_recv().unwrap()).unwrap().payload, b"live output");
            conn.handle(&encode(LIST_REQ, 7, &[]));
            assert_eq!(decode(&rx.try_recv().unwrap()).unwrap().kind, LIST_RESP);
            conn.handle(&encode(DETACH, id, &[]));
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
