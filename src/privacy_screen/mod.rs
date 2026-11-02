//! Host privacy screen (防窥): the viewer's RPC 7 hides the local screen and blocks local input
//! while it controls the desktop. This module is the desktop-independent controller: it counts
//! desktop sessions, switches modes, rolls back failures, keeps the recovery record and the guard,
//! and serves the protocol. Desktop work lives behind `backend::Backend` (only `kde` so far).
mod backend;
mod cover;
mod guard;
mod kde;
mod mode;
mod protocol;
mod record;
mod tool;

pub use mode::Mode;
use std::sync::{atomic::{AtomicBool, Ordering}, Mutex};

struct Active { mode: Mode, record: Option<record::Record> }
struct State { sessions: usize, active: Option<Active> }
static STATE: Mutex<State> = Mutex::new(State { sessions: 0, active: None });
/// Lock-free view of `STATE.active` for capture/input paths.
static ACTIVE: AtomicBool = AtomicBool::new(false);
static PICTURE: AtomicBool = AtomicBool::new(false);
static POINTER: Mutex<Option<[f64; 4]>> = Mutex::new(None);

pub fn supported() -> bool { backend::detect().is_some() }
pub fn active() -> bool { ACTIVE.load(Ordering::Relaxed) }
/// The viewer's privacy_type now in effect.
pub fn current_type() -> u8 {
    if !active() { mode::TYPE_OFF } else if PICTURE.load(Ordering::Relaxed) { Mode::Picture.wire_type() } else { Mode::ScreenOff.wire_type() }
}
/// Message 15 SystemStateChange { 5 PrivateScreenState } for the state now in effect.
pub fn state_message(seq: u64, timestamp: u64) -> Vec<u8> {
    crate::initialization::envelope(seq, timestamp, 15, &protocol::state_change(current_type()))
}
/// See `backend::Entered::pointer_region`.
pub fn pointer_region() -> Option<[f64; 4]> { *POINTER.lock().ok()? }

fn publish(active: Option<&Active>) {
    ACTIVE.store(active.is_some(), Ordering::Relaxed);
    PICTURE.store(active.is_some_and(|a| a.mode == Mode::Picture), Ordering::Relaxed);
}
fn leave(state: &mut State) -> Result<(), String> {
    let Some(active) = state.active.as_ref() else { return Ok(()) };
    cover::hide();
    if let Ok(mut p) = POINTER.lock() { *p = None; }
    if let Some(record) = &active.record {
        let backend = backend::named(&record.backend).ok_or("unknown privacy backend in the recovery record")?;
        backend.leave(&record.data)?;
    }
    record::remove()?;
    state.active = None;
    publish(None);
    guard::stop();
    log::info!("Privacy screen restored");
    Ok(())
}
fn enter(state: &mut State, mode: Mode) -> Result<(), String> {
    record::ownership()?;
    if state.active.as_ref().is_some_and(|a| a.mode == mode) { return Ok(()); }
    // Switching mode: start over from the restored desktop.
    leave(state)?;
    let backend = backend::detect().ok_or("privacy screen is not supported on this desktop")?;
    if let Some(reason) = backend.unavailable(mode) { return Err(reason); }
    // A pending recovery must never be overwritten by a new snapshot.
    if record::pending() { return Err("previous output recovery is pending".into()); }
    guard::start()?;
    // Published before the backend runs: capture keep-awake paths must not undo its changes.
    state.active = Some(Active { mode, record: None });
    publish(state.active.as_ref());
    let mut journal = record::Journal::new(backend.name(), mode);
    let result = backend.enter(mode, &mut journal);
    if let Some(active) = state.active.as_mut() { active.record = journal.into_record(); }
    let entered = result?;
    *POINTER.lock().map_err(|_| "pointer state poisoned")? = entered.pointer_region;
    log::info!("Privacy screen enabled: backend={} mode={mode:?}", backend.name());
    Ok(())
}
fn save_preference(mode: Mode) {
    // Session lifecycle takes the settings lock before STATE; never take it while holding STATE.
    if let Err(e) = crate::system_actions::merge_save(|s| s.privacy_type = Some(mode.wire_type())) { log::warn!("Privacy mode not saved: {e}"); }
}
/// RPC 7: `mode` is None for an unknown privacy_type.
fn set(enable: bool, mode: Option<Mode>) -> Result<(), String> {
    let mut state = STATE.lock().map_err(|_| "privacy state poisoned")?;
    if !enable { return leave(&mut state); }
    let mode = mode.ok_or("unsupported privacy type")?;
    if state.sessions == 0 { return Err("no active desktop session".into()); }
    let result = enter(&mut state, mode);
    // A failed rollback keeps the record and the guard: the next request, exit or start retries.
    if result.is_err() { if let Err(e) = leave(&mut state) { log::warn!("Privacy rollback failed: {e}"); } }
    drop(state);
    if result.is_ok() { save_preference(mode); }
    result
}
/// Why RPC 7 cannot be served at all; checked before answering.
fn refuse(enable: bool, mode: Option<Mode>) -> Option<String> {
    if !enable { return None; }
    let Some(mode) = mode else { return Some("unsupported privacy type".into()) };
    if STATE.lock().map_or(true, |s| s.sessions == 0) { return Some("no active desktop session".into()); }
    let Some(backend) = backend::detect() else { return Some("privacy screen is not supported on this desktop".into()) };
    backend.unavailable(mode)
}
/// A switch the RPC accepted; run it with `Accepted::apply` after sending the response.
pub struct Accepted { enable: bool, mode: Option<Mode> }
/// RPC 7 answer, sent before the switch: the Mac viewer gives up on the RPC after a few seconds
/// (error 9998 + n in its RPC failure handler) and a switch takes longer. A later failure is
/// reported as ReportError -7, which the viewer handles by resetting its privacy menu.
pub fn rpc_accept(rpc: &[crate::protocol::WireField]) -> Option<(Vec<u8>, Option<Accepted>)> {
    let (header, request) = protocol::parse(rpc)?;
    if let Some(e) = refuse(request.enable, request.mode) {
        log::warn!("Privacy request refused: enable={} mode={:?}: {e}", request.enable, request.mode);
        return Some((protocol::response(&header, &request, true, current_type()), None));
    }
    let expected = if request.enable { request.mode.map_or(mode::TYPE_OFF, Mode::wire_type) } else { mode::TYPE_OFF };
    Some((protocol::response(&header, &request, false, expected), Some(Accepted { enable: request.enable, mode: request.mode })))
}
/// Mac 4.43.0 handleForReportError: -7 resets privacy to off; -9 reports an RDP session change,
/// not privacy success. Success is carried by RpcResponse 3 and SystemMetrics.setting 7.
pub const REPORT_ERROR: i32 = -7;
impl Accepted {
    pub fn apply(self) -> Result<(), String> {
        let result = set(self.enable, self.mode);
        if let Err(e) = &result { log::warn!("Privacy request failed: enable={} mode={:?}: {e}", self.enable, self.mode); }
        result
    }
}
pub fn session_started() { if let Ok(mut s) = STATE.lock() { s.sessions += 1; } }
pub fn session_ended() {
    if let Ok(mut s) = STATE.lock() {
        s.sessions = s.sessions.saturating_sub(1);
        if s.sessions == 0 { if let Err(e) = leave(&mut s) { log::warn!("Privacy restore failed: {e}"); } }
    }
}
pub fn shutdown() {
    if let Ok(mut s) = STATE.lock() {
        s.sessions = 0;
        if let Err(e) = leave(&mut s) { log::warn!("Privacy restore failed: {e}"); }
    }
}
/// Undo a record left by a crash (host start, guard). Failure keeps the record for the next try.
pub fn recover() {
    if let Err(e) = record::ownership() { log::warn!("Privacy control unavailable: {e}"); return; }
    let Some(saved) = record::load() else { return };
    if let Ok(mut state) = STATE.lock() {
        state.active = Some(Active { mode: saved.mode, record: Some(saved) });
        if let Err(e) = leave(&mut state) { log::warn!("Privacy recovery failed: {e}"); }
    }
}
/// Local diagnostic: needs an existing portal session (registered by the caller).
pub fn probe(mode: Mode) -> Result<(), String> {
    recover();
    let result = {
        let mut state = STATE.lock().map_err(|_| "privacy state poisoned")?;
        state.sessions = 1;
        enter(&mut state, mode)
    };
    if result.is_ok() { save_preference(mode); }
    if result.is_ok() { std::thread::sleep(std::time::Duration::from_secs(5)); }
    let mut state = STATE.lock().map_err(|_| "privacy state poisoned")?;
    let restored = leave(&mut state);
    if restored.is_err() { log::warn!("Privacy probe restore failed; the next start retries"); }
    result.and(restored)
}
pub use protocol::screensaver_response;
pub fn guard_main() -> i32 { guard::main() }
pub fn cover_main(args: &[String]) -> i32 { cover::main(args) }

#[cfg(test)]
mod tests {
    #[test]
    fn enabling_without_a_backend_is_refused_and_reports_off() {
        let bytes = [10, 2, 8, 42, 58, 4, 8, 1, 16, 3];
        let (response, accepted) = super::rpc_accept(&crate::protocol::decode_wire(&bytes).unwrap()).unwrap();
        assert_eq!(response, [10, 2, 8, 42, 26, 6, 8, 1, 16, 3, 24, 1]);
        assert!(accepted.is_none());
    }
}
