//! GNOME/KDE portal session. Capture and input must share this Arc; do not
//! recreate it in an encoder subprocess (that would request consent again).
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortalError { pub code: i32 }

impl std::fmt::Display for PortalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", error(self.code))
    }
}

impl std::error::Error for PortalError {}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct FrameVersion(usize, u64);
#[cfg(unix)]
#[path = "portal_permissions.rs"]
mod permissions;

#[cfg(all(target_os = "linux", feature = "portal"))]
mod native {
    use std::{ffi::c_void, ptr::NonNull, sync::{Mutex, MutexGuard, TryLockError}};
    crate::engine_ffi! { crate::engine_plugin::Engine::Portal;
        fn uu_portal_open_persist(restore: *const libc::c_char, save: extern "C" fn(*const libc::c_char, *mut c_void), data: *mut c_void, error: *mut i32, width: *mut u32, height: *mut u32) -> *mut c_void;
        fn uu_portal_open_persist_cancellable(restore: *const libc::c_char, save: extern "C" fn(*const libc::c_char, *mut c_void), save_data: *mut c_void, cancelled: extern "C" fn(*mut c_void) -> i32, cancel_data: *mut c_void, error: *mut i32, width: *mut u32, height: *mut u32) -> *mut c_void;
        fn uu_portal_clipboard_supported(portal: *mut c_void) -> i32;
        fn uu_portal_clipboard_ready(portal: *mut c_void) -> i32;
        fn uu_portal_clipboard_set(portal: *mut c_void, text: *const u8, length: usize) -> i32;
        fn uu_portal_clipboard_poll(portal: *mut c_void, text: *mut u8, capacity: usize) -> i32;
        fn uu_portal_close(portal: *mut c_void);
        fn uu_portal_alive(portal: *mut c_void) -> i32;
        fn uu_portal_capture_alive(portal: *mut c_void) -> i32;
        fn uu_portal_input_dispatch(portal: *mut c_void);
        fn uu_portal_reset_frame(portal: *mut c_void);
        fn uu_portal_capture_since(portal: *mut c_void, bgra: *mut u8, length: usize, revision: *mut u64) -> i32;
        fn uu_portal_move(portal: *mut c_void, x: f64, y: f64, relative: i32) -> i32;
        fn uu_portal_button(portal: *mut c_void, button: i32, pressed: i32) -> i32;
        fn uu_portal_key(portal: *mut c_void, keysym: u32, keycode: i32, pressed: i32) -> i32;
        fn uu_portal_stream_position(portal: *mut c_void, x: *mut i32, y: *mut i32) -> i32;
        fn uu_portal_monitors(out: *mut libc::c_char, length: usize) -> i32;
        fn uu_portal_end(portal: *mut c_void);
    }
    struct Handle(NonNull<c_void>);
    fn pack(width: u32, height: u32) -> u64 { (u64::from(width) << 32) | u64::from(height) }
    // The engine owns callback threads; public calls are serialized by Mutex.
    unsafe impl Send for Handle {}
    impl Drop for Handle { fn drop(&mut self) { unsafe { uu_portal_close(self.0.as_ptr()) } } }
    fn blank_changed(frame: &mut [u8], version: &mut Option<super::FrameVersion>) -> bool {
        if *version == Some(super::FrameVersion(0, 0)) { return false; }
        super::blank(frame); *version = Some(super::FrameVersion(0, 0)); true
    }
    // Portal preparation and native teardown can wait for consent or D-Bus.
    // Only the swap may hold the mutex shared with input and clipboard.
    fn replace_handle<T>(current: &Mutex<T>, locked: MutexGuard<'_, T>, prepare: impl FnOnce() -> Result<Option<T>, String>) -> Result<bool, String> {
        drop(locked);
        let Some(replacement) = prepare()? else { return Ok(false); };
        let old = {
            let mut handle = current.lock().map_err(|_| "Portal session lock poisoned")?;
            std::mem::replace(&mut *handle, replacement)
        };
        drop(old);
        Ok(true)
    }
    #[derive(Default)]
    struct Recovery { retry_at: Option<std::time::Instant>, failed_until: Option<std::time::Instant>, denied: bool }
    impl Recovery {
        // Shared by the encoder's stream-loss recovery and the desktop supervisor: a failed open
        // backs both off, and a terminal consent/format failure stops the encoder from prompting again.
        fn failed(&mut self, now: std::time::Instant, code: i32) {
            let until = now + std::time::Duration::from_secs(5);
            self.failed_until = Some(until);
            self.retry_at = Some(until);
            if matches!(code, 4 | 5 | 6 | 8 | 14) { self.denied = true; }
        }
        fn succeeded(&mut self) { *self = Self::default(); }
        fn ready(&mut self, now: std::time::Instant, unlocked: impl FnOnce() -> bool) -> bool {
            if self.denied { return false; }
            let deadline = self.retry_at.get_or_insert(now + std::time::Duration::from_secs(1));
            if now < *deadline { return false; }
            *deadline = now + std::time::Duration::from_secs(1);
            unlocked()
        }
    }
    fn clear_pause_for_unlocked_replacement(was_locked: &std::sync::atomic::AtomicBool, state: Option<bool>) {
        if state == Some(false) {
            was_locked.store(false, std::sync::atomic::Ordering::Release);
        }
    }
    pub struct Session { handle: Mutex<Handle>, locked: tokio::sync::watch::Receiver<Option<bool>>, was_locked: std::sync::atomic::AtomicBool, recovery: Mutex<Recovery>, size: std::sync::atomic::AtomicU64, monitor: Mutex<Option<String>> }
    impl Session {
        fn take_lock_observation(&self) -> Result<bool, String> {
            // A recovery in progress must see the lock even if another capture
            // observes unlock and would otherwise reset only the old handle.
            let _recovery = match self.recovery.try_lock() {
                Ok(recovery) => recovery,
                Err(TryLockError::WouldBlock) => return Ok(false),
                Err(TryLockError::Poisoned(_)) => return Err("Portal recovery lock poisoned".into()),
            };
            Ok(self.was_locked.swap(false, std::sync::atomic::Ordering::AcqRel))
        }
        fn check_geometry(&self, replacement: &Self) -> Result<(), super::PortalError> {
            // Encoder buffers use frame pixels. Logical geometry and EIS regions
            // belong to the replacement handle and may change with scaling (#25).
            if self.size() != replacement.size() {
                Err(super::PortalError { code: 14 })
            } else {
                Ok(())
            }
        }
        pub fn is_alive(&self) -> bool {
            self.handle.lock().is_ok_and(|handle| unsafe { uu_portal_alive(handle.0.as_ptr()) != 0 })
        }
        /// Opens a session for `monitor` (a Mutter connector) with that monitor's saved consent;
        /// None means the screen currently selected, or the only monitor.
        fn open_inner(cancelled: Option<&std::sync::atomic::AtomicBool>, monitor: Option<&str>) -> Result<Self, super::PortalError> {
            extern "C" fn is_cancelled(data: *mut c_void) -> i32 {
                let cancelled = unsafe { &*(data as *const std::sync::atomic::AtomicBool) };
                i32::from(cancelled.load(std::sync::atomic::Ordering::Acquire))
            }
            if cancelled.is_some_and(|value| value.load(std::sync::atomic::Ordering::Acquire)) {
                return Err(super::PortalError { code: 13 });
            }
            // engine-plugins: libuu-portal.so (or one of its libraries) may be missing on this host.
            crate::engine_plugin::load(crate::engine_plugin::Engine::Portal).map_err(|_| super::PortalError { code: 15 })?;
            let (mut error, mut width, mut height) = (0, 0, 0);
            let directory = super::permissions::directory();
            let key = super::permissions::desktop_key();
            let wanted = monitor.map(str::to_owned).or_else(crate::wayland::portal_monitor);
            // Each monitor has its own consent; the desktop-wide key is the single-monitor one.
            let token = directory.as_deref().and_then(|dir| {
                wanted.as_deref().and_then(|name| super::permissions::take(dir, &super::permissions::monitor_key(name)))
                    .or_else(|| super::permissions::take(dir, key))
            }).and_then(|value| std::ffi::CString::new(value).ok());
            // The new token is saved once the granted monitor is known (after open).
            let mut cache: Option<String> = None;
            extern "C" fn save_token(token: *const libc::c_char, data: *mut c_void) {
                // Callback is synchronous during open; cache lives through the call.
                let result = std::panic::catch_unwind(|| unsafe {
                    let cache = &mut *(data as *mut Option<String>);
                    *cache = std::ffi::CStr::from_ptr(token).to_str().ok().map(str::to_owned);
                });
                if result.is_err() { log::warn!("Portal permission cache callback failed"); }
            }
            let restore = token.as_ref().map_or(std::ptr::null(), |value| value.as_ptr());
            let pointer = unsafe {
                if let Some(cancelled) = cancelled {
                    uu_portal_open_persist_cancellable(
                        restore, save_token, &mut cache as *mut _ as *mut c_void,
                        is_cancelled, cancelled as *const _ as *mut c_void,
                        &mut error, &mut width, &mut height,
                    )
                } else {
                    uu_portal_open_persist(restore, save_token, &mut cache as *mut _ as *mut c_void, &mut error, &mut width, &mut height)
                }
            };
            let handle = NonNull::new(pointer).ok_or(super::PortalError { code: error })?;
            let (mut x, mut y) = (0, 0);
            let granted = (unsafe { uu_portal_stream_position(handle.as_ptr(), &mut x, &mut y) } == 0)
                .then(|| crate::wayland::portal_monitor_at(x, y)).flatten();
            if let (Some(dir), Some(token)) = (directory.as_deref(), cache.take()) {
                let saved = match &granted {
                    Some(name) => super::permissions::monitor_key(name),
                    None => key.to_owned(),
                };
                if super::permissions::save(dir, &saved, &token).is_err() { log::warn!("Portal permission cache could not be saved"); }
            }
            if wanted.is_some() && granted != wanted { log::warn!("Portal granted a different monitor than requested"); }
            // A freshly opened session holds no lock-screen frame: only an observed lock (or a
            // closed stream) must discard the cached frame. Starting "locked" cleared the first
            // valid frame, and a static desktop sends no new one, so the viewer stayed black (#2).
            Ok(Self { handle: Mutex::new(Handle(handle)), locked: crate::session_lock::watch_locked(), was_locked: std::sync::atomic::AtomicBool::new(false), recovery: Mutex::new(Recovery::default()), size: pack(width, height).into(), monitor: Mutex::new(granted) })
        }
        /// Frame pixels of the monitor the stream currently follows.
        pub fn size(&self) -> (u32, u32) {
            let packed = self.size.load(std::sync::atomic::Ordering::Acquire);
            ((packed >> 32) as u32, packed as u32)
        }
        /// End the compositor session (screen-share indicator off); the next desktop
        /// connection reopens it with the saved consent.
        pub fn end(&self) {
            if let Ok(handle) = self.handle.lock() { unsafe { uu_portal_end(handle.0.as_ptr()) }; }
        }
        /// Mutter connector of the monitor this session captures, when several are advertised.
        pub fn monitor(&self) -> Option<String> { self.monitor.lock().ok().and_then(|m| m.clone()) }
        /// Capture another monitor: open its own session (silently with saved consent; the first
        /// time GNOME asks on the host) and swap it in, so every holder of this Arc follows.
        pub fn switch_to(&self, connector: &str) -> Result<(), String> {
            if self.monitor().as_deref() == Some(connector) { return Ok(()); }
            let replacement = Self::open_inner(None, Some(connector)).map_err(|error| error.to_string())?;
            if replacement.monitor().as_deref() != Some(connector) {
                return Err("Another monitor was shared; choose this monitor in the host dialog".into());
            }
            let size = replacement.size();
            let next = replacement.handle.into_inner().map_err(|_| "Portal session lock poisoned")?;
            let previous = {
                let mut handle = self.handle.lock().map_err(|_| "Portal session lock poisoned")?;
                self.size.store(pack(size.0, size.1), std::sync::atomic::Ordering::Release);
                std::mem::replace(&mut *handle, next)
            };
            if let Ok(mut monitor) = self.monitor.lock() { *monitor = Some(connector.to_owned()); }
            drop(previous);
            Ok(())
        }
        pub fn open() -> Result<Self, String> {
            Self::open_inner(None, None).map_err(|error| error.to_string())
        }
        pub fn open_cancellable(cancelled: &std::sync::atomic::AtomicBool) -> Result<Self, super::PortalError> {
            Self::open_inner(Some(cancelled), None)
        }
        pub fn recover(&self, cancelled: &std::sync::atomic::AtomicBool) -> Result<bool, super::PortalError> {
            use std::sync::atomic::Ordering;
            if cancelled.load(Ordering::Acquire) { return Err(super::PortalError { code: 13 }); }
            // Serialize with the encoder's stream-loss recovery: two openers would ask for consent twice.
            let mut recovery = loop {
                match self.recovery.try_lock() {
                    Ok(recovery) => break recovery,
                    Err(TryLockError::WouldBlock) => {
                        if cancelled.load(Ordering::Acquire) { return Err(super::PortalError { code: 13 }); }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    Err(TryLockError::Poisoned(_)) => return Err(super::PortalError { code: 3 }),
                }
            };
            if recovery.failed_until.is_some_and(|until| std::time::Instant::now() < until) {
                return Err(super::PortalError { code: 3 });
            }
            let alive = {
                let handle = self.handle.lock().map_err(|_| super::PortalError { code: 3 })?;
                unsafe { uu_portal_alive(handle.0.as_ptr()) != 0 && uu_portal_capture_alive(handle.0.as_ptr()) != 0 }
            };
            if alive { return Ok(false); }

            let result = super::open_then_install(cancelled, || Self::open_cancellable(cancelled), |replacement| {
                self.check_geometry(&replacement)?;
                let replacement_handle = replacement.handle.into_inner().map_err(|_| super::PortalError { code: 3 })?;
                let mut handle = self.handle.lock().map_err(|_| super::PortalError { code: 3 })?;
                if cancelled.load(Ordering::Acquire) { return Err(super::PortalError { code: 13 }); }
                *handle = replacement_handle;
                clear_pause_for_unlocked_replacement(&self.was_locked, *self.locked.borrow());
                log::info!("Portal capture session restored after closure");
                Ok(())
            });
            match result {
                Ok(()) => { recovery.succeeded(); Ok(true) }
                Err(error) => {
                    if error.code != 13 { recovery.failed(std::time::Instant::now(), error.code); }
                    Err(error)
                }
            }
        }
        pub fn pause(&self) {
            if let Ok(handle) = self.handle.lock() {
                unsafe { uu_portal_reset_frame(handle.0.as_ptr()) };
                self.was_locked.store(true, std::sync::atomic::Ordering::Release);
            } else {
                self.was_locked.store(true, std::sync::atomic::Ordering::Release);
            }
        }
        pub fn capture(&self, frame: &mut [u8]) -> Result<(), String> {
            self.capture_changed(frame, &mut None).map(|_| ())
        }
        pub fn capture_changed(&self, frame: &mut [u8], version: &mut Option<super::FrameVersion>) -> Result<bool, String> {
            let (width, height) = self.size();
            // A buffer sized for the previous monitor belongs to an encoder that a switch is replacing.
            if frame.len() != width as usize * height as usize * 4 { return Ok(blank_changed(frame, version)); }
            let handle = self.handle.lock().map_err(|_| "Portal session lock poisoned")?;
            // Drain EIS even on a static desktop so pause/disconnect and sync
            // events are processed without waiting for remote input.
            unsafe { uu_portal_input_dispatch(handle.0.as_ptr()) };
            let state = *self.locked.borrow();
            if state != Some(false) {
                // Unknown state (watcher not reported yet): show nothing but keep the cached frame.
                if state == Some(true) {
                    unsafe { uu_portal_reset_frame(handle.0.as_ptr()) };
                    self.was_locked.store(true, std::sync::atomic::Ordering::Release);
                }
                return Ok(blank_changed(frame, version));
            }
            if self.was_locked.load(std::sync::atomic::Ordering::Acquire) {
                // First unlocked capture after a real lock: drop frames captured while locked.
                if self.take_lock_observation()? { unsafe { uu_portal_reset_frame(handle.0.as_ptr()) }; }
                return Ok(blank_changed(frame, version));
            }
            if unsafe { uu_portal_capture_alive(handle.0.as_ptr()) } == 0 {
                // Serialize recovery without making another capture worker hold
                // the input mutex while waiting for this worker's Portal calls.
                let mut recovery = match self.recovery.try_lock() {
                    Ok(retry) => retry,
                    Err(TryLockError::WouldBlock) => return Ok(blank_changed(frame, version)),
                    Err(TryLockError::Poisoned(_)) => return Err("Portal recovery lock poisoned".into()),
                };
                if replace_handle(&self.handle, handle, || {
                    {
                        let handle = self.handle.lock().map_err(|_| "Portal session lock poisoned")?;
                        if unsafe { uu_portal_capture_alive(handle.0.as_ptr()) } != 0 {
                            recovery.retry_at = None;
                            return Ok(None);
                        }
                    }
                    // logind's LockedHint can lag compositor stream closure.
                    if !recovery.ready(std::time::Instant::now(), || {
                        if crate::session_lock::locked_now() == Some(false) { return true; }
                        self.was_locked.store(true, std::sync::atomic::Ordering::Release);
                        false
                    }) { return Ok(None); }
                    let replacement = match Self::open_inner(None, self.monitor().as_deref()) {
                        Ok(value) => value,
                        Err(error) => {
                            log::warn!("Portal capture recovery deferred: {error}");
                            recovery.failed(std::time::Instant::now(), error.code);
                            return Ok(None);
                        }
                    };
                    recovery.succeeded();
                    self.check_geometry(&replacement).map_err(|error| error.to_string())?;
                    // Consent can outlive the unlocked state checked above.
                    if *self.locked.borrow() != Some(false) || self.was_locked.load(std::sync::atomic::Ordering::Acquire) {
                        return Ok(None);
                    }
                    replacement.handle.into_inner().map(Some).map_err(|_| "Portal session lock poisoned".into())
                })? {
                    *version = None;
                    log::info!("Portal capture session restored after stream loss");
                }
                // Recheck lock state on the next capture before showing the new session.
                return Ok(blank_changed(frame, version));
            } else {
                match self.recovery.try_lock() {
                    Ok(mut recovery) => recovery.retry_at = None,
                    Err(TryLockError::WouldBlock) => (),
                    Err(TryLockError::Poisoned(_)) => return Err("Portal recovery lock poisoned".into()),
                }
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
            let keycode = crate::uinput::keysym_to_evdev(u64::from(symbol)).map_or(-1, i32::from);
            super::check(unsafe { uu_portal_key(handle.0.as_ptr(), symbol, keycode, i32::from(pressed)) })
        }
    }

    /// Mutter's monitors (GNOME); empty elsewhere.
    pub fn monitors() -> Vec<super::Monitor> {
        if crate::engine_plugin::load(crate::engine_plugin::Engine::Portal).is_err() { return Vec::new(); }
        let mut text = vec![0u8; 8192];
        if unsafe { uu_portal_monitors(text.as_mut_ptr() as *mut libc::c_char, text.len()) } <= 0 { return Vec::new(); }
        let end = text.iter().position(|&b| b == 0).unwrap_or(text.len());
        super::parse_monitors(&String::from_utf8_lossy(&text[..end]))
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        use std::{mem::ManuallyDrop, sync::Arc};

        struct Fixture(ManuallyDrop<Session>);
        impl Fixture {
            fn new(physical: (u32, u32)) -> Self {
                let (_, locked) = tokio::sync::watch::channel(None);
                Self(ManuallyDrop::new(Session {
                    handle: Mutex::new(Handle(NonNull::dangling())),
                    locked,
                    was_locked: std::sync::atomic::AtomicBool::new(false),
                    recovery: Mutex::new(Recovery::default()),
                    size: pack(physical.0, physical.1).into(),
                    monitor: Mutex::new(None),
                }))
            }

            fn session(&self) -> &Session { &self.0 }
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                let Session { handle, locked, was_locked, recovery, .. } =
                    unsafe { ManuallyDrop::take(&mut self.0) };
                let handle = handle.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner);
                std::mem::forget(handle);
                drop((locked, was_locked, recovery));
            }
        }

        #[test]
        fn recovery_requires_only_matching_frame_dimensions() {
            let original = Fixture::new((3840, 2160));
            let same = Fixture::new((3840, 2160));
            let width_changed = Fixture::new((2560, 2160));
            let height_changed = Fixture::new((3840, 1440));

            assert!(original.session().check_geometry(same.session()).is_ok());
            assert!(original.session().check_geometry(width_changed.session()).is_err());
            assert!(original.session().check_geometry(height_changed.session()).is_err());
        }

        struct TestHandle {
            generation: u8,
            key_held: bool,
            button_held: bool,
            retirement: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
        }
        impl Drop for TestHandle {
            fn drop(&mut self) {
                if let Some((entered, resume)) = self.retirement.take() {
                    let _ = entered.send(());
                    let _ = resume.recv_timeout(std::time::Duration::from_secs(10));
                }
            }
        }
        fn release_input(current: Arc<Mutex<TestHandle>>) -> (std::thread::JoinHandle<()>, std::sync::mpsc::Receiver<(u8, bool, bool)>) {
            let (released, received) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                let mut handle = current.lock().unwrap();
                handle.key_held = false;
                handle.button_held = false;
                released.send((handle.generation, handle.key_held, handle.button_held)).unwrap();
            });
            (worker, received)
        }

        #[test]
        fn recovery_does_not_block_input_releases_during_open_or_close() {
            use std::{sync::mpsc::channel, time::Duration};
            let timeout = Duration::from_secs(3);
            let (opening, opened) = channel();
            let (resume_open, wait_open) = channel();
            let (closing, closed) = channel();
            let (resume_close, wait_close) = channel();
            let current = Arc::new(Mutex::new(TestHandle {
                generation: 1, key_held: true, button_held: true, retirement: Some((closing, wait_close)),
            }));
            let retry = Arc::new(Mutex::new(None::<std::time::Instant>));
            let recovery = {
                let current = current.clone();
                let retry = retry.clone();
                std::thread::spawn(move || {
                    let handle = current.lock().unwrap();
                    let _owner = retry.try_lock().unwrap();
                    replace_handle(&current, handle, || {
                        opening.send(()).unwrap();
                        wait_open.recv_timeout(Duration::from_secs(10)).unwrap();
                        Ok(Some(TestHandle {
                            generation: 2, key_held: true, button_held: true, retirement: None,
                        }))
                    })
                })
            };
            let opening_result = opened.recv_timeout(timeout);
            let (first_worker, first_release) = release_input(current.clone());
            let first_result = first_release.recv_timeout(timeout);
            // A concurrent capture must not wait on recovery while retaining input's mutex.
            let (contended, contention) = channel();
            let contender = {
                let current = current.clone();
                let retry = retry.clone();
                std::thread::spawn(move || {
                    let _handle = current.lock().unwrap();
                    contended.send(matches!(retry.try_lock(), Err(TryLockError::WouldBlock))).unwrap();
                })
            };
            let contention_result = contention.recv_timeout(timeout);
            let _ = resume_open.send(());
            let closing_result = closed.recv_timeout(timeout);
            let (second_worker, second_release) = release_input(current.clone());
            let second_result = second_release.recv_timeout(timeout);
            // Always release blocked work before asserting, including on regression.
            let _ = resume_close.send(());
            let recovered = recovery.join().unwrap();
            first_worker.join().unwrap();
            second_worker.join().unwrap();
            contender.join().unwrap();
            assert!(opening_result.is_ok() && closing_result.is_ok());
            assert_eq!(first_result.unwrap(), (1, false, false));
            assert_eq!(second_result.unwrap(), (2, false, false));
            assert!(contention_result.unwrap());
            assert!(recovered.unwrap());
        }

        #[test]
        fn failed_or_deferred_recovery_keeps_the_current_handle_unlocked() {
            let current = Mutex::new(1);
            for failure in [false, true] {
                let handle = current.lock().unwrap();
                let result = replace_handle(&current, handle, || {
                    assert_eq!(*current.try_lock().unwrap(), 1);
                    if failure { Err("open failed".into()) } else { Ok(None) }
                });
                if failure { assert_eq!(result.unwrap_err(), "open failed"); }
                else { assert!(!result.unwrap()); }
                assert_eq!(*current.try_lock().unwrap(), 1);
            }
        }

        #[test]
        fn terminal_failure_stops_encoder_recovery_until_the_supervisor_succeeds() {
            let now = std::time::Instant::now();
            let later = now + std::time::Duration::from_secs(60);
            let mut recovery = Recovery::default();
            recovery.failed(now, 3);
            assert!(!recovery.denied && recovery.failed_until.is_some());
            recovery.failed(now, 4);
            assert!(recovery.denied);
            assert!(!recovery.ready(later, || panic!("denied consent must not be probed again")));
            recovery.succeeded();
            assert!(!recovery.denied && recovery.failed_until.is_none() && recovery.retry_at.is_none());
        }

        #[test]
        fn recovery_backoff_skips_lock_probes_until_the_deadline() {
            use std::{cell::Cell, time::{Duration, Instant}};
            let now = Instant::now();
            let probes = Cell::new(0);
            let unlocked = || { probes.set(probes.get() + 1); true };
            let mut recovery = Recovery::default();
            // Initial one-second grace period, polled at up to 144 fps.
            for millis in 0..1000 {
                assert!(!recovery.ready(now + Duration::from_millis(millis), unlocked));
            }
            assert_eq!(probes.get(), 0);
            assert!(recovery.ready(now + Duration::from_secs(1), unlocked));
            assert_eq!(probes.get(), 1);
            // A failed open schedules the existing five-second retry delay.
            recovery.retry_at = Some(now + Duration::from_secs(6));
            for millis in 1000..6000 {
                assert!(!recovery.ready(now + Duration::from_millis(millis), unlocked));
            }
            assert_eq!(probes.get(), 1);
            // A locked/unknown probe denies opening and is itself throttled.
            assert!(!recovery.ready(now + Duration::from_secs(6), || { probes.set(2); false }));
            assert!(!recovery.ready(now + Duration::from_millis(6999), || panic!("early lock probe")));
            assert!(recovery.ready(now + Duration::from_secs(7), unlocked));
            assert_eq!(probes.get(), 3);
        }

        #[test]
        fn lock_observation_survives_unlock_while_recovery_is_opening() {
            use std::{sync::{atomic::Ordering, mpsc::channel}, time::Duration};
            let mut fixture = Fixture::new((2, 2));
            let (locked, watch) = tokio::sync::watch::channel(Some(false));
            fixture.0.locked = watch;
            let session = fixture.session();
            std::thread::scope(|threads| {
                let (opening, opened) = channel();
                let (resume, wait) = channel();
                let recovery = threads.spawn(move || {
                    let _owner = session.recovery.lock().unwrap();
                    opening.send(()).unwrap();
                    wait.recv_timeout(Duration::from_secs(10)).unwrap();
                });
                let ready = opened.recv_timeout(Duration::from_secs(3));
                locked.send_replace(Some(true));
                session.was_locked.store(true, Ordering::Release);
                locked.send_replace(Some(false));
                let (reset, result) = channel();
                let capture = threads.spawn(move || {
                    let _handle = session.handle.lock().unwrap();
                    assert_eq!(*session.locked.borrow(), Some(false));
                    reset.send(session.take_lock_observation()).unwrap();
                });
                let consumed = result.recv_timeout(Duration::from_secs(3));
                let still_pending = session.was_locked.load(Ordering::Acquire);
                let _ = resume.send(());
                recovery.join().unwrap();
                capture.join().unwrap();
                assert!(ready.is_ok());
                assert!(!consumed.unwrap().unwrap());
                assert!(still_pending);
            });
            assert!(session.take_lock_observation().unwrap());
            assert!(!session.was_locked.load(std::sync::atomic::Ordering::Acquire));
        }

        #[test]
        fn cancelled_recovery_does_not_touch_the_handle() {
            let session = Fixture::new((1920, 1080));
            let cancelled = std::sync::atomic::AtomicBool::new(true);
            assert_eq!(session.session().recover(&cancelled).unwrap_err().code, 13);
        }

        #[test]
        fn unlocked_replacement_keeps_its_first_static_frame() {
            let was_locked = std::sync::atomic::AtomicBool::new(true);
            clear_pause_for_unlocked_replacement(&was_locked, Some(false));
            assert!(!was_locked.load(std::sync::atomic::Ordering::Acquire));
        }
    }
}

fn blank(frame: &mut [u8]) { for pixel in frame.chunks_exact_mut(4) { pixel.copy_from_slice(&[0, 0, 0, 255]); } }

fn open_then_install<T>(
    cancelled: &std::sync::atomic::AtomicBool,
    open: impl FnOnce() -> Result<T, PortalError>,
    install: impl FnOnce(T) -> Result<(), PortalError>,
) -> Result<(), PortalError> {
    use std::sync::atomic::Ordering;
    if cancelled.load(Ordering::Acquire) { return Err(PortalError { code: 13 }); }
    let replacement = open()?;
    if cancelled.load(Ordering::Acquire) { return Err(PortalError { code: 13 }); }
    install(replacement)
}

fn error(code: i32) -> String {
    let reason = match code {
        1 => "session bus unavailable", 2 => "RemoteDesktop/ScreenCast portal unavailable",
        3 => "request failed", 4 => "permission cancelled or denied", 5 => "permission request timed out",
        6 => "single monitor and keyboard/pointer permission required", 7 => "PipeWire connection failed",
        8 => "unsupported frame format or size", 9 => "session closed or stream lost",
        10 => "frame unavailable", 11 => "invalid input or frame buffer",
        13 => "operation cancelled", 14 => "dimensions changed; reconnect required",
        15 => return match crate::engine_plugin::load(crate::engine_plugin::Engine::Portal) {
            Err(reason) if cfg!(feature = "portal") => format!("GNOME/KDE Wayland (portal) engine unavailable: {reason}"),
            _ => NOT_BUILT.into(),
        },
        _ => "operation failed",
    };
    format!("Portal {reason} (code {code})")
}
fn check(code: i32) -> Result<(), String> { if code == 0 { Ok(()) } else { Err(error(code)) } }

/// GNOME/KDE Wayland selected the portal backend, but this binary was built without it (the release
/// package is X11 only): say how to get a picture instead of an opaque "portal unavailable".
pub const NOT_BUILT: &str = "this build has no GNOME/KDE Wayland (portal) support: choose the Xorg session \
    (\"GNOME on Xorg\" / \"Plasma (X11)\") at the login screen, or build with --features wayland,portal";
#[cfg(all(target_os = "linux", feature = "portal"))]
pub use native::Session as PortalSession;
#[cfg(not(all(target_os = "linux", feature = "portal")))]
pub struct PortalSession { pub width: u32, pub height: u32 }
#[cfg(not(all(target_os = "linux", feature = "portal")))]
impl PortalSession {
    pub fn size(&self) -> (u32, u32) { (self.width, self.height) }
    pub fn monitor(&self) -> Option<String> { None }
    pub fn end(&self) {}
    pub fn switch_to(&self, _: &str) -> Result<(), String> { Err(error(15)) }
}
#[cfg(not(all(target_os = "linux", feature = "portal")))]
impl PortalSession {
    pub fn clipboard_supported(&self) -> bool { false }
    pub fn clipboard_ready(&self) -> bool { false }
    pub fn clipboard_set(&self, _: &str) -> Result<(), String> { Err(error(15)) }
    pub fn clipboard_poll(&self) -> Option<String> { None }
    pub fn is_alive(&self) -> bool { false }
    pub fn open() -> Result<Self, String> { Err("Portal backend requires Linux and --features portal".into()) }
    pub fn open_cancellable(cancelled: &std::sync::atomic::AtomicBool) -> Result<Self, PortalError> {
        if cancelled.load(std::sync::atomic::Ordering::Acquire) { Err(PortalError { code: 13 }) }
        else { Err(PortalError { code: 15 }) }
    }
    pub fn recover(&self, cancelled: &std::sync::atomic::AtomicBool) -> Result<bool, PortalError> {
        if cancelled.load(std::sync::atomic::Ordering::Acquire) { Err(PortalError { code: 13 }) }
        else { Err(PortalError { code: 15 }) }
    }
    pub fn pause(&self) {}
    pub fn capture(&self, _: &mut [u8]) -> Result<(), String> { Err(error(15)) }
    pub fn capture_changed(&self, _: &mut [u8], _: &mut Option<FrameVersion>) -> Result<bool, String> { Err(error(15)) }
    pub fn move_pointer(&self, _: f64, _: f64, _: bool) -> Result<(), String> { Err(error(15)) }
    pub fn button(&self, _: i32, _: bool) -> Result<(), String> { Err(error(15)) }
    pub fn key(&self, _: u64, _: bool) -> Result<(), String> { Err(error(15)) }
}

/// The portal session that monitor switches act on: one per host, reused by reconnects.
static ACTIVE: std::sync::Mutex<Option<std::sync::Weak<PortalSession>>> = std::sync::Mutex::new(None);
pub fn permissions_directory() -> Option<std::path::PathBuf> { permissions::directory() }
pub fn monitor_key(connector: &str) -> String { permissions::monitor_key(connector) }
pub fn register(session: &Arc<PortalSession>) {
    *ACTIVE.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::downgrade(session));
}
pub fn switch_active(connector: &str) -> Result<(), String> {
    let session = ACTIVE.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref().and_then(std::sync::Weak::upgrade)
        .ok_or("Portal desktop unavailable")?;
    session.switch_to(connector)
}
pub struct Monitor { pub connector: String, pub width: u32, pub height: u32, pub x: i32, pub y: i32, pub primary: bool }
#[cfg(all(target_os = "linux", feature = "portal"))]
pub use native::monitors;
#[cfg(not(all(target_os = "linux", feature = "portal")))]
pub fn monitors() -> Vec<Monitor> { Vec::new() }
/// `connector\twidth\theight\tx\ty\tprimary` lines from the engine.
fn parse_monitors(text: &str) -> Vec<Monitor> {
    text.lines().filter_map(|line| {
        let mut f = line.split('\t');
        let connector = f.next()?.to_owned();
        let (width, height) = (f.next()?.parse().ok()?, f.next()?.parse().ok()?);
        let (x, y, primary) = (f.next()?.parse().ok()?, f.next()?.parse().ok()?, f.next()? == "1");
        (!connector.is_empty() && connector.len() <= 64 && connector.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
            .then_some(Monitor { connector, width, height, x, y, primary })
    }).collect()
}

/// Forward frames in-process into encoder stdin. Cancellation drops the Arc;
/// the caller keeps one owning Arc for all subsequent desktop reconnects.
pub fn write_frames(session: Arc<PortalSession>, mut output: impl std::io::Write, cancelled: &std::sync::atomic::AtomicBool) -> Result<(), String> {
    let (width, height) = session.size();
    let length = (width as usize).checked_mul(height as usize).and_then(|n| n.checked_mul(4)).ok_or("Portal frame too large")?;
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
    fn monitor_lines_parse_and_reject_unsafe_connectors() {
        let m = super::parse_monitors("eDP-1\t2880\t1800\t0\t0\t1\nHDMI-1\t2560\t1440\t1920\t0\t0\n../x\t1\t1\t0\t0\t0\n");
        assert_eq!(m.len(), 2);
        assert!(m[0].primary && m[0].connector == "eDP-1" && (m[1].width, m[1].x) == (2560, 1920));
    }
    #[test]
    fn locked_frames_hide_old_content() { let mut frame = [42; 8]; super::blank(&mut frame); assert_eq!(frame, [0,0,0,255,0,0,0,255]); }
    #[test]
    fn errors_never_include_remote_messages_or_identifiers() {
        assert!(super::check(4).unwrap_err().contains("cancelled or denied"));
        assert!(super::check(9).unwrap_err().contains("closed or stream lost"));
        // Portal builds name the missing runtime engine; other builds say how to get one.
        let unavailable = super::check(15).unwrap_err();
        assert!(unavailable.contains("--features wayland,portal") || (cfg!(feature = "portal") && unavailable.contains("engine unavailable")));
    }
    #[test]
    fn portal_error_display_is_fixed_and_sanitized() {
        assert_eq!(super::PortalError { code: 13 }.to_string(), "Portal operation cancelled (code 13)");
        assert_eq!(super::PortalError { code: 14 }.to_string(), "Portal dimensions changed; reconnect required (code 14)");
        assert_eq!(super::PortalError { code: 123 }.to_string(), "Portal operation failed (code 123)");
    }
    #[test]
    fn opening_with_pre_cancelled_flag_returns_cancellation_code() {
        let cancelled = std::sync::atomic::AtomicBool::new(true);
        let code = match super::PortalSession::open_cancellable(&cancelled) {
            Err(error) => error.code,
            Ok(_) => panic!("pre-cancelled portal open unexpectedly succeeded"),
        };
        assert_eq!(code, 13);
    }
    #[test]
    fn replacement_opens_before_installing_under_the_handle_lock() {
        let handle = std::sync::Mutex::new(7);
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        super::open_then_install(&cancelled, || {
            assert_eq!(*handle.try_lock().unwrap(), 7);
            Ok(11)
        }, |replacement| {
            *handle.lock().unwrap() = replacement;
            Ok(())
        }).unwrap();
        assert_eq!(*handle.lock().unwrap(), 11);
    }
}
