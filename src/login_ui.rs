//! Optional GTK3 window for `uuremote-server login` (c_engine/login_window.c). Built only when
//! gtk+-3.0 is available (cfg uu_gtk); otherwise, and with `--no-gui` or no display, every call
//! is a no-op and the terminal QR remains the only UI.

#[cfg(any(uu_gtk, target_env = "ohos"))]
extern "C" {
    fn uu_login_ui_start(name: *const libc::c_char, os: *const libc::c_char, cpu: *const libc::c_char, gpu: *const libc::c_char) -> libc::c_int;
    fn uu_login_ui_set_qr(rgba: *const u8, edge: libc::c_int);
    fn uu_login_ui_set_status(text: *const libc::c_char, kind: libc::c_int);
    fn uu_login_ui_set_done(text: *const libc::c_char);
    fn uu_login_ui_closed() -> libc::c_int;
    fn uu_login_ui_close();
    fn uu_login_ui_offer_refresh(visible: libc::c_int);
    fn uu_login_ui_take_refresh() -> libc::c_int;
}

#[derive(Clone, Copy)]
pub enum Tone { Info = 0, Ok = 1, Error = 2 }

pub struct LoginUi { shown: bool, attached: bool }

#[cfg(any(uu_gtk, target_env = "ohos"))]
fn cstr(s: &str) -> std::ffi::CString { std::ffi::CString::new(s.replace('\0', "")).unwrap_or_default() }

impl LoginUi {
    pub fn none() -> Self { LoginUi { shown: false, attached: false } }

    /// Opens the window when built with GTK and a display is present; falls back silently otherwise.
    #[allow(unused_variables)]
    pub fn open(name: &str, os: &str, cpu: &str, gpu: &str) -> Self {
        #[cfg(any(uu_gtk, target_env = "ohos"))]
        if std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some() {
            let (n, o, c, g) = (cstr(name), cstr(os), cstr(cpu), cstr(gpu));
            if unsafe { uu_login_ui_start(n.as_ptr(), o.as_ptr(), c.as_ptr(), g.as_ptr()) } == 0 {
                return LoginUi { shown: true, attached: false };
            }
            eprintln!("Login window unavailable; using the terminal QR code");
        }
        LoginUi::none()
    }

    /// Account card of the host's own main window (already open): the QR stays in the window only
    /// (a service's stdout is the journal and the QR URL is a login ticket), and dropping this
    /// handle never closes the shared window.
    pub fn attached() -> Self {
        #[cfg(any(uu_gtk, target_env = "ohos"))]
        { unsafe { uu_login_ui_start(std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null()) }; LoginUi { shown: true, attached: true } }
        #[cfg(not(any(uu_gtk, target_env = "ohos")))]
        LoginUi { shown: false, attached: true }
    }
    pub fn quiet(&self) -> bool { self.attached }
    pub fn shown(&self) -> bool { self.shown }

    #[allow(unused_variables)]
    pub fn qr(&self, rgba: &[u8], edge: usize) {
        #[cfg(any(uu_gtk, target_env = "ohos"))]
        if self.shown && rgba.len() == edge * edge * 4 { unsafe { uu_login_ui_set_qr(rgba.as_ptr(), edge as libc::c_int) } }
    }

    #[allow(unused_variables)]
    /// Show or hide the 刷新二维码 button (a failed QR waits for the user, like the official client).
    pub fn offer_refresh(&self, visible: bool) {
        #[cfg(any(uu_gtk, target_env = "ohos"))]
        if self.shown { unsafe { uu_login_ui_offer_refresh(libc::c_int::from(visible)) } }
        #[cfg(not(any(uu_gtk, target_env = "ohos")))]
        let _ = visible;
    }
    /// The user clicked 刷新二维码 since the last call.
    pub fn take_refresh(&self) -> bool {
        #[cfg(any(uu_gtk, target_env = "ohos"))]
        if self.shown { return unsafe { uu_login_ui_take_refresh() } != 0; }
        false
    }
    pub fn status(&self, text: &str, tone: Tone) {
        #[cfg(any(uu_gtk, target_env = "ohos"))]
        if self.shown { let t = cstr(text); unsafe { uu_login_ui_set_status(t.as_ptr(), tone as libc::c_int) } }
    }

    #[allow(unused_variables)]
    pub fn done(&self, text: &str) {
        #[cfg(any(uu_gtk, target_env = "ohos"))]
        if self.shown { let t = cstr(text); unsafe { uu_login_ui_set_done(t.as_ptr()) } }
    }

    pub fn closed(&self) -> bool {
        #[cfg(any(uu_gtk, target_env = "ohos"))]
        if self.shown { return unsafe { uu_login_ui_closed() } != 0; }
        false
    }

    /// Resolves when the user closes the window; never resolves without a window.
    pub async fn wait_closed(&self) {
        if !self.shown { return std::future::pending().await; }
        while !self.closed() { tokio::time::sleep(std::time::Duration::from_millis(200)).await; }
    }
}

impl Drop for LoginUi {
    fn drop(&mut self) {
        #[cfg(any(uu_gtk, target_env = "ohos"))]
        if self.shown && !self.attached { unsafe { uu_login_ui_close() } }
    }
}
