//! Native Linux X11 Capture Engine via XShm

use std::sync::Mutex;

#[cfg(target_os = "linux")]
extern "C" {
    fn x11_init(display_name: *const libc::c_char) -> libc::c_int;
    fn x11_get_dimensions(out_w: *mut libc::c_int, out_h: *mut libc::c_int) -> libc::c_int;
    fn x11_refresh_geometry() -> libc::c_int;
    fn x11_capture_frame(
        out_buffer: *mut libc::c_uchar,
        out_w: *mut libc::c_int,
        out_h: *mut libc::c_int,
    ) -> libc::c_int;
    fn x11_close();
    fn x11_wake_display() -> libc::c_int;
    fn x11_display_name() -> *const libc::c_char;
}

#[cfg(not(target_os = "linux"))]
unsafe fn x11_init(_: *const libc::c_char) -> libc::c_int { 0 }
#[cfg(not(target_os = "linux"))]
unsafe fn x11_get_dimensions(out_w: *mut libc::c_int, out_h: *mut libc::c_int) -> libc::c_int {
    *out_w = 1920;
    *out_h = 1080;
    0
}
#[cfg(not(target_os = "linux"))]
unsafe fn x11_capture_frame(
    out_buffer: *mut libc::c_uchar,
    out_w: *mut libc::c_int,
    out_h: *mut libc::c_int,
) -> libc::c_int {
    *out_w = 1920;
    *out_h = 1080;
    std::ptr::write_bytes(out_buffer, 0, (1920 * 1080 * 4) as usize);
    0
}
#[cfg(not(target_os = "linux"))]
unsafe fn x11_close() {}
#[cfg(not(target_os = "linux"))]
unsafe fn x11_refresh_geometry() -> libc::c_int { 0 }

#[cfg(not(target_os = "linux"))]
unsafe fn x11_display_name() -> *const libc::c_char { b":0\0".as_ptr().cast() }

/// The X11 root can be resized while the host runs (RandR, xrdp reconnects); the
/// geometry read at startup then makes x11grab reject every new connection.
/// Re-reads the root size; None when the X11 engine is not in use or the query fails.
pub fn x11_size() -> Option<(u32, u32)> {
    match unsafe { x11_refresh_geometry() } {
        0 | 1 => x11_last_size(),
        -1 => None,
        code => { log::warn!("X11 geometry query failed with code {code}"); None }
    }
}

/// The root size from the last refresh, without an X round trip (pointer mapping).
/// Display the X11 engine opened, once a capture has initialized it.
pub fn x11_display() -> Option<String> {
    let name = unsafe { x11_display_name() };
    if name.is_null() { None } else { Some(unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy().into_owned()) }
}

pub fn x11_last_size() -> Option<(u32, u32)> {
    let (mut w, mut h): (libc::c_int, libc::c_int) = (0, 0);
    if unsafe { x11_get_dimensions(&mut w, &mut h) } != 0 || w <= 0 || h <= 0 { return None; }
    Some((w as u32, h as u32))
}

pub struct ScreenCapture {
    pub backend: crate::desktop_backend::DesktopBackend,
    pub init_error: Option<String>,
    pub portal: Option<std::sync::Arc<crate::portal::PortalSession>>,
    pub display: String,
    kms: Option<crate::kms::Capture>,
    pub width: u32,
    pub height: u32,
    buffer: Mutex<Vec<u8>>,
}

impl ScreenCapture {
    pub fn open_portal(cancelled: &std::sync::atomic::AtomicBool) -> Result<Self, crate::portal::PortalError> {
        let session = std::sync::Arc::new(crate::portal::PortalSession::open_cancellable(cancelled)?);
        crate::portal::register(&session);
        let (width, height) = session.size();
        Ok(Self { backend: crate::desktop_backend::DesktopBackend::Portal, init_error: None,
            display: "wayland:portal".into(), width, height,
            portal: Some(session), kms: None, buffer: Mutex::new(Vec::new()) })
    }
    pub fn new() -> Result<Self, String> {
        let backend=crate::desktop_backend::detect()?;
        if backend == crate::desktop_backend::DesktopBackend::Kms {
            let kms=crate::kms::Capture::open()?;
            let (width,height)=(kms.width,kms.height);
            return Ok(Self { backend,init_error:None,portal:None,display:"kms:linear".into(),width,height,
                buffer:Mutex::new(Vec::new()),kms:Some(kms) });
        }
        if backend == crate::desktop_backend::DesktopBackend::Portal {
            return Self::open_portal(&std::sync::atomic::AtomicBool::new(false)).map_err(|error| error.to_string());
        }
        if backend == crate::desktop_backend::DesktopBackend::Wayland {
            let (width,height)=crate::wayland::init()?;
            return Ok(Self { backend, init_error:None, portal:None, kms:None, display:"wayland:native".into(), width,height,
                buffer:Mutex::new(Vec::new()) });
        }
        let ret = unsafe { x11_init(std::ptr::null()) };
        if ret != 0 {
            return Err(format!("x11_init failed with code {}", ret));
        }

        let mut w: libc::c_int = 0;
        let mut h: libc::c_int = 0;
        unsafe { x11_get_dimensions(&mut w, &mut h) };

        if w <= 0 || h <= 0 {
            return Err(format!("Invalid screen dimensions: {}x{}", w, h));
        }

        let buffer = Mutex::new(Vec::new());

        log::info!("ScreenCapture initialized: {}x{}", w, h);
        let display_ptr = unsafe { x11_display_name() };
        if display_ptr.is_null() { return Err("Capture display name unavailable".into()); }
        let display = unsafe { std::ffi::CStr::from_ptr(display_ptr) }.to_string_lossy().into_owned();
        log::info!("Selected capture display: {}", display);
        X11_USERS.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Ok(Self {
            backend,
            init_error:None,
            portal:None,
            kms:None,
            display,
            width: w as u32,
            height: h as u32,
            buffer,
        })
    }

    /// Metadata for non-desktop tools. Never advertise a usable fake screen.
    pub fn unavailable(error:String)->Self {
        Self { backend:crate::desktop_backend::detect().unwrap_or(crate::desktop_backend::DesktopBackend::X11),
            init_error:Some(error), portal:None, kms:None, display:String::new(), width:0,height:0,buffer:Mutex::new(Vec::new()) }
    }

    /// 截取一帧 BGRA 图像
    pub fn capture_frame(&self) -> Result<Vec<u8>, String> {
        if let Some(error)=&self.init_error { return Err(error.clone()); }
        let mut buf = self.buffer.lock().unwrap();
        buf.resize(self.width as usize * self.height as usize * 4, 0);
        if let Some(kms)=&self.kms { kms.capture(&mut buf)?; return Ok(buf.clone()); }
        if let Some(session)=&self.portal { session.capture(&mut buf)?; return Ok(buf.clone()); }
        if self.backend == crate::desktop_backend::DesktopBackend::Wayland {
            crate::wayland::capture(&mut buf)?;
            return Ok(buf.clone());
        }
        let mut w: libc::c_int = self.width as libc::c_int;
        let mut h: libc::c_int = self.height as libc::c_int;

        let ret = unsafe { x11_capture_frame(buf.as_mut_ptr(), &mut w, &mut h) };
        if ret != 0 {
            if ret == -4 { return Err(format!("desktop resized to {w}x{h}; reconnect to capture the new size")); }
            return Err(format!("x11_capture_frame failed with code {}", ret));
        }

        Ok(buf.clone())
    }
}

/// The X11 engine keeps one process-wide display connection shared by every ScreenCapture.
/// Only the last one may close it: a short-lived capture (device init's screen size during guest
/// assistance, or the replaced capture after a resize) used to close it under the live stream.
static X11_USERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        if self.init_error.is_none() && self.backend==crate::desktop_backend::DesktopBackend::X11
            && X11_USERS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel) == 1 { unsafe { x11_close() }; }
    }
}

/// Keep the display visible only while a peer is streaming; never unlock it.
pub fn keep_display_awake() -> Result<(), String> {
    if crate::desktop_backend::detect()? == crate::desktop_backend::DesktopBackend::Kms { return Ok(()); }
    if crate::desktop_backend::detect()? == crate::desktop_backend::DesktopBackend::Portal {
        return crate::desktop_power::wake();
    }
    if crate::desktop_backend::detect()? == crate::desktop_backend::DesktopBackend::Wayland {
        return crate::wayland::wake();
    }
    #[cfg(target_os = "linux")]
    if unsafe { x11_wake_display() } != 0 { return Err("X11 display wake failed".into()); }
    Ok(())
}
