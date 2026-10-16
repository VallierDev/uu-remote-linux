//! GNOME/KDE portal session. Capture and input must share this Arc; do not
//! recreate it in an encoder subprocess (that would request consent again).
use std::sync::Arc;
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct FrameVersion(usize, u64);
#[cfg(unix)]
#[path = "portal_permissions.rs"]
mod permissions;

#[cfg(all(target_os = "linux", feature = "portal"))]
mod native {
    use std::{ffi::c_void, ptr::NonNull, sync::Mutex};
    extern "C" {
        fn uu_portal_open_persist(restore: *const libc::c_char, save: extern "C" fn(*const libc::c_char, *mut c_void), data: *mut c_void, error: *mut i32, width: *mut u32, height: *mut u32) -> *mut c_void;
        fn uu_portal_clipboard_supported(portal: *mut c_void) -> i32;
        fn uu_portal_clipboard_ready(portal: *mut c_void) -> i32;
        fn uu_portal_clipboard_set(portal: *mut c_void, text: *const u8, length: usize) -> i32;
        fn uu_portal_clipboard_poll(portal: *mut c_void, text: *mut u8, capacity: usize) -> i32;
        fn uu_portal_close(portal: *mut c_void);
        fn uu_portal_alive(portal: *mut c_void) -> i32;
        fn uu_portal_reset_frame(portal: *mut c_void);
        fn uu_portal_capture_since(portal: *mut c_void, bgra: *mut u8, length: usize, revision: *mut u64) -> i32;
        fn uu_portal_move(portal: *mut c_void, x: f64, y: f64, relative: i32) -> i32;
        fn uu_portal_button(portal: *mut c_void, button: i32, pressed: i32) -> i32;
        fn uu_portal_key(portal: *mut c_void, keysym: u32, pressed: i32) -> i32;
    }
    struct Handle(NonNull<c_void>);
    // The engine owns callback threads; public calls are serialized by Mutex.
    unsafe impl Send for Handle {}
    impl Drop for Handle { fn drop(&mut self) { unsafe { uu_portal_close(self.0.as_ptr()) } } }
    fn blank_changed(frame: &mut [u8], version: &mut Option<super::FrameVersion>) -> bool {
        if *version == Some(super::FrameVersion(0, 0)) { return false; }
        super::blank(frame); *version = Some(super::FrameVersion(0, 0)); true
    }
    pub struct Session { handle: Mutex<Handle>, locked: tokio::sync::watch::Receiver<Option<bool>>, was_locked: std::sync::atomic::AtomicBool, retry_at: Mutex<Option<std::time::Instant>>, pub width: u32, pub height: u32 }
    impl Session {
        pub fn is_alive(&self) -> bool {
            self.handle.lock().is_ok_and(|handle| unsafe { uu_portal_alive(handle.0.as_ptr()) != 0 })
        }
        pub fn open() -> Result<Self, String> {
            let (mut error, mut width, mut height) = (0, 0, 0);
            let directory = super::permissions::directory();
            let key = super::permissions::desktop_key();
            let token = directory.as_deref().and_then(|dir| super::permissions::take(dir, key))
                .and_then(|value| std::ffi::CString::new(value).ok());
            let mut cache = (directory, key);
            extern "C" fn save_token(token: *const libc::c_char, data: *mut c_void) {
                // Callback is synchronous during open; cache lives through the call.
                let result = std::panic::catch_unwind(|| unsafe {
                    let cache = &*(data as *const (Option<std::path::PathBuf>, &'static str));
                    if let (Some(dir), Ok(value)) = (cache.0.as_deref(), std::ffi::CStr::from_ptr(token).to_str()) {
                        if super::permissions::save(dir, cache.1, value).is_err() { log::warn!("Portal permission cache could not be saved"); }
                    }
                });
                if result.is_err() { log::warn!("Portal permission cache callback failed"); }
            }
            let pointer = unsafe { uu_portal_open_persist(token.as_ref().map_or(std::ptr::null(), |v| v.as_ptr()), save_token, &mut cache as *mut _ as *mut c_void, &mut error, &mut width, &mut height) };
            let handle = NonNull::new(pointer).ok_or_else(|| super::error(error))?;
            // A freshly opened session holds no lock-screen frame: only an observed lock (or a
            // closed stream) must discard the cached frame. Starting "locked" cleared the first
            // valid frame, and a static desktop sends no new one, so the viewer stayed black (#2).
            Ok(Self { handle: Mutex::new(Handle(handle)), locked: crate::session_lock::watch_locked(), was_locked: std::sync::atomic::AtomicBool::new(false), retry_at: Mutex::new(None), width, height })
        }
        pub fn capture(&self, frame: &mut [u8]) -> Result<(), String> {
            self.capture_changed(frame, &mut None).map(|_| ())
        }
        pub fn capture_changed(&self, frame: &mut [u8], version: &mut Option<super::FrameVersion>) -> Result<bool, String> {
            if frame.len() != self.width as usize * self.height as usize * 4 { return Err(super::error(11)); }
            let mut handle = self.handle.lock().map_err(|_| "Portal session lock poisoned")?;
            let state = *self.locked.borrow();
            if state != Some(false) {
                // Unknown state (watcher not reported yet): show nothing but keep the cached frame.
                if state == Some(true) {
                    self.was_locked.store(true, std::sync::atomic::Ordering::Release);
                    unsafe { uu_portal_reset_frame(handle.0.as_ptr()) };
                }
                return Ok(blank_changed(frame, version));
            }
            if self.was_locked.swap(false, std::sync::atomic::Ordering::AcqRel) {
                // First unlocked capture after a real lock: drop frames captured while locked.
                unsafe { uu_portal_reset_frame(handle.0.as_ptr()) };
                return Ok(blank_changed(frame, version));
            }
            if unsafe { uu_portal_alive(handle.0.as_ptr()) } == 0 {
                // GNOME can close the stream before the periodic lock watcher updates.
                if crate::session_lock::locked_now() != Some(false) {
                    self.was_locked.store(true, std::sync::atomic::Ordering::Release);
                    return Ok(blank_changed(frame, version));
                }
                // logind's LockedHint can lag compositor stream closure. Give it a
                // full watcher interval before attempting another permission session.
                let mut retry = self.retry_at.lock().map_err(|_| "Portal recovery lock poisoned")?;
                let deadline = retry.get_or_insert_with(|| std::time::Instant::now() + std::time::Duration::from_secs(1));
                if std::time::Instant::now() < *deadline { return Ok(blank_changed(frame, version)); }
                let replacement = match Self::open() {
                    Ok(value) => value,
                    Err(error) => {
                        log::warn!("Portal capture recovery deferred: {error}");
                        *retry = Some(std::time::Instant::now() + std::time::Duration::from_secs(5));
                        return Ok(blank_changed(frame, version));
                    }
                };
                *retry = None;
                if replacement.width != self.width || replacement.height != self.height {
                    return Err("Portal dimensions changed; reconnect required".into());
                }
                *handle = replacement.handle.into_inner().map_err(|_| "Portal session lock poisoned")?;
                log::info!("Portal capture session restored after closure");
            }
            let identity = handle.0.as_ptr() as usize;
            let mut revision = version.filter(|v| v.0 == identity).map_or(0, |v| v.1);
            let code = unsafe { uu_portal_capture_since(handle.0.as_ptr(), frame.as_mut_ptr(), frame.len(), &mut revision) };
            match code {
                0 | 12 => { *version = Some(super::FrameVersion(identity, revision)); Ok(code == 0) }
                9 | 10 => Ok(blank_changed(frame, version)),
                _ => Err(super::error(code)),
            }
        }

        pub fn clipboard_supported(&self) -> bool {
            self.handle.lock().is_ok_and(|handle| unsafe { uu_portal_clipboard_supported(handle.0.as_ptr()) != 0 })
        }
        pub fn clipboard_ready(&self) -> bool {
            self.handle.lock().is_ok_and(|handle| unsafe { uu_portal_clipboard_ready(handle.0.as_ptr()) != 0 })
        }
        pub fn clipboard_set(&self, text: &str) -> Result<(), String> {
            if text.len()>crate::clipboard::MAX_TEXT || *self.locked.borrow()!=Some(false) { return Err("Portal clipboard unavailable".into()); }
            let handle=self.handle.lock().map_err(|_| "Portal session lock poisoned")?;
            if unsafe { uu_portal_clipboard_set(handle.0.as_ptr(),text.as_ptr(),text.len()) }==0 {Ok(())} else {Err("Portal clipboard ownership failed".into())}
        }
        pub fn clipboard_poll(&self) -> Option<String> {
            if *self.locked.borrow()!=Some(false) { return None; }
            let handle=self.handle.lock().ok()?;
            let mut bytes=vec![0;crate::clipboard::MAX_TEXT];
            let length=unsafe { uu_portal_clipboard_poll(handle.0.as_ptr(),bytes.as_mut_ptr(),bytes.len()) };
            if length<0 { return None; }
            bytes.truncate(length as usize); String::from_utf8(bytes).ok()
        }
        pub fn move_pointer(&self, x: f64, y: f64, relative: bool) -> Result<(), String> {
            if *self.locked.borrow() != Some(false) { return Err("Portal input blocked while locked".into()); }
            let handle = self.handle.lock().map_err(|_| "Portal session lock poisoned")?;
            super::check(unsafe { uu_portal_move(handle.0.as_ptr(), x, y, i32::from(relative)) })
        }
        pub fn button(&self, button: i32, pressed: bool) -> Result<(), String> {
            if pressed && *self.locked.borrow() != Some(false) { return Err("Portal input blocked while locked".into()); }
            let handle = self.handle.lock().map_err(|_| "Portal session lock poisoned")?;
            super::check(unsafe { uu_portal_button(handle.0.as_ptr(), button, i32::from(pressed)) })
        }
        pub fn key(&self, symbol: u64, pressed: bool) -> Result<(), String> {
            if pressed && *self.locked.borrow() != Some(false) { return Err("Portal input blocked while locked".into()); }
            let symbol = u32::try_from(symbol).map_err(|_| "Invalid Portal keysym")?;
            let handle = self.handle.lock().map_err(|_| "Portal session lock poisoned")?;
            super::check(unsafe { uu_portal_key(handle.0.as_ptr(), symbol, i32::from(pressed)) })
        }
    }
}

fn blank(frame: &mut [u8]) { for pixel in frame.chunks_exact_mut(4) { pixel.copy_from_slice(&[0, 0, 0, 255]); } }

fn error(code: i32) -> String {
    let reason = match code {
        1 => "session bus unavailable", 2 => "RemoteDesktop/ScreenCast portal unavailable",
        3 => "request failed", 4 => "permission cancelled or denied", 5 => "permission request timed out",
        6 => "single monitor and keyboard/pointer permission required", 7 => "PipeWire connection failed",
        8 => "unsupported frame format or size", 9 => "session closed or stream lost",
        10 => "frame unavailable", 11 => "invalid input or frame buffer", _ => "operation failed",
    };
    format!("Portal {reason} (code {code})")
}
fn check(code: i32) -> Result<(), String> { if code == 0 { Ok(()) } else { Err(error(code)) } }

#[cfg(all(target_os = "linux", feature = "portal"))]
pub use native::Session as PortalSession;
#[cfg(not(all(target_os = "linux", feature = "portal")))]
pub struct PortalSession { pub width: u32, pub height: u32 }
#[cfg(not(all(target_os = "linux", feature = "portal")))]
impl PortalSession {
    pub fn clipboard_supported(&self) -> bool { false }
    pub fn clipboard_ready(&self) -> bool { false }
    pub fn clipboard_set(&self, _: &str) -> Result<(), String> { Err(error(2)) }
    pub fn clipboard_poll(&self) -> Option<String> { None }
    pub fn is_alive(&self) -> bool { false }
    pub fn open() -> Result<Self, String> { Err("Portal backend requires Linux and --features portal".into()) }
    pub fn capture(&self, _: &mut [u8]) -> Result<(), String> { Err(error(2)) }
    pub fn capture_changed(&self, _: &mut [u8], _: &mut Option<FrameVersion>) -> Result<bool, String> { Err(error(2)) }
    pub fn move_pointer(&self, _: f64, _: f64, _: bool) -> Result<(), String> { Err(error(2)) }
    pub fn button(&self, _: i32, _: bool) -> Result<(), String> { Err(error(2)) }
    pub fn key(&self, _: u64, _: bool) -> Result<(), String> { Err(error(2)) }
}

/// Forward frames in-process into encoder stdin. Cancellation drops the Arc;
/// the caller keeps one owning Arc for all subsequent desktop reconnects.
pub fn write_frames(session: Arc<PortalSession>, mut output: impl std::io::Write, cancelled: &std::sync::atomic::AtomicBool) -> Result<(), String> {
    let length = (session.width as usize).checked_mul(session.height as usize).and_then(|n| n.checked_mul(4)).ok_or("Portal frame too large")?;
    let mut frame = vec![0; length];
    let interval = std::time::Duration::from_nanos(1_000_000_000 / crate::video::FPS);
    while !cancelled.load(std::sync::atomic::Ordering::Acquire) {
        let started = std::time::Instant::now();
        session.capture(&mut frame)?;
        output.write_all(&frame).map_err(|_| "Portal encoder input closed")?;
        if let Some(left) = interval.checked_sub(started.elapsed()) { std::thread::sleep(left); }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn locked_frames_hide_old_content() { let mut frame = [42; 8]; super::blank(&mut frame); assert_eq!(frame, [0,0,0,255,0,0,0,255]); }
    #[test]
    fn errors_never_include_remote_messages_or_identifiers() {
        assert!(super::check(4).unwrap_err().contains("cancelled or denied"));
        assert!(super::check(9).unwrap_err().contains("closed or stream lost"));
    }
}
