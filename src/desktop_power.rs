//! Small GNOME-only display wake and idle-inhibition bridge.

#[cfg(all(target_os = "linux", feature = "portal"))]
mod native {
    use std::{ffi::c_void, ptr::NonNull};

    crate::engine_ffi! { crate::engine_plugin::Engine::Portal;
        fn uu_desktop_power_wake() -> i32;
        fn uu_desktop_idle_inhibitor_acquire(error: *mut i32) -> *mut c_void;
        fn uu_desktop_idle_inhibitor_release(inhibitor: *mut c_void);
        fn uu_desktop_screensaver_inhibitor_acquire(error: *mut i32) -> *mut c_void;
    }

    pub struct Handle(NonNull<c_void>);

    // GDBusConnection is thread-safe; the C handle is only used by its single Drop call.
    unsafe impl Send for Handle {}

    impl Handle {
        pub fn acquire(gnome: bool) -> Result<Self, String> {
            let mut error = 0;
            let pointer = unsafe { if gnome { uu_desktop_idle_inhibitor_acquire(&mut error) } else { uu_desktop_screensaver_inhibitor_acquire(&mut error) } };
            NonNull::new(pointer).map(Self).ok_or_else(|| super::error("idle inhibition", error))
        }

        pub fn wake() -> Result<(), String> {
            super::check("display wake", unsafe { uu_desktop_power_wake() })
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { uu_desktop_idle_inhibitor_release(self.0.as_ptr()) }
        }
    }
}

/// Set GNOME's display power-save mode to on. This does not unlock the session or change
/// lock-screen policy. Session-bus setup and each method call are bounded to three seconds.
pub fn wake() -> Result<(), String> {
    if crate::privacy_screen::active() { return Ok(()); }
    if !gnome_portal_active() {
        return Ok(());
    }
    #[cfg(all(target_os = "linux", feature = "portal"))]
    {
        native::Handle::wake()
    }
    #[cfg(not(all(target_os = "linux", feature = "portal")))]
    {
        Ok(())
    }
}

/// Holds an idle-only inhibitor until dropped: GNOME SessionManager on GNOME, the freedesktop
/// ScreenSaver interface on KDE (remote input through the portal does not count as activity
/// there, so the session locked mid-session, #61).
///
/// Acquire and drop perform blocking D-Bus work. Session-bus setup and each method call are
/// bounded to three seconds. Callers on an async runtime should move both operations to a
/// blocking worker.
pub struct IdleInhibitor {
    #[cfg(all(target_os = "linux", feature = "portal"))]
    _handle: Option<native::Handle>,
}

impl IdleInhibitor {
    pub fn acquire() -> Result<Self, String> {
        let kde = portal_desktop_is("kde");
        if !gnome_portal_active() && !kde {
            return Ok(Self {
                #[cfg(all(target_os = "linux", feature = "portal"))]
                _handle: None,
            });
        }
        #[cfg(all(target_os = "linux", feature = "portal"))]
        {
            native::Handle::acquire(!kde).map(|handle| Self { _handle: Some(handle) })
        }
        #[cfg(not(all(target_os = "linux", feature = "portal")))]
        {
            Ok(Self {})
        }
    }
}

fn gnome_portal_active() -> bool { portal_desktop_is("gnome") }

fn portal_desktop_is(name: &str) -> bool {
    let matches = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().split(':')
        .any(|desktop| desktop.eq_ignore_ascii_case(name));
    matches && crate::desktop_backend::detect().is_ok_and(|backend| backend == crate::desktop_backend::DesktopBackend::Portal)
}

#[cfg(all(target_os = "linux", feature = "portal"))]
fn check(operation: &str, code: i32) -> Result<(), String> {
    if code == 0 { Ok(()) } else { Err(error(operation, code)) }
}

#[cfg(all(target_os = "linux", feature = "portal"))]
fn error(operation: &str, code: i32) -> String {
    let reason = match code {
        1 => "session D-Bus unavailable",
        2 => "GNOME D-Bus operation failed",
        _ => "GNOME D-Bus operation failed",
    };
    format!("{operation}: {reason} (code {code})")
}
