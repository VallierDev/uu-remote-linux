//! Damage-driven X11 capture for the in-process encoder: XDamage reports when the root window changed,
//! XShm copies it out as 32-bit BGRX. Pure Rust over x11rb; only the SysV shared-memory calls use libc.
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::damage::{self, ConnectionExt as _, ReportLevel};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xproto::Window;
use x11rb::rust_connection::RustConnection;
use x11rb::NONE;

/// X11 ZPixmap image format.
const Z_PIXMAP: u8 = 2;

pub struct DamageCapture {
    conn: RustConnection,
    root: Window,
    damage: damage::Damage,
    damage_event: u8,
    width: u16,
    height: u16,
    seg: shm::Seg,
    shm_ptr: *mut u8,
    len: usize,
}

fn text(error: impl std::fmt::Display) -> String { error.to_string() }

impl DamageCapture {
    /// Connects to `display` and sets up XDamage and XShm. Errors name the missing extension or failed step.
    pub fn open(display: &str) -> Result<Self, String> {
        let (conn, screen_num) = RustConnection::connect(Some(display)).map_err(text)?;
        let damage_info = conn.extension_information(damage::X11_EXTENSION_NAME).map_err(text)?
            .ok_or("XDamage extension is missing")?;
        if conn.extension_information(shm::X11_EXTENSION_NAME).map_err(text)?.is_none() {
            return Err("XShm extension is missing".into());
        }
        conn.damage_query_version(1, 1).map_err(text)?.reply().map_err(text)?;

        let setup = conn.setup();
        let screen = setup.roots.get(screen_num).ok_or("X screen not found")?;
        let (root, width, height, depth) = (screen.root, screen.width_in_pixels, screen.height_in_pixels, screen.root_depth);
        // The frame layout assumes packed 32-bit pixels; anything else would shear the picture.
        let format = setup.pixmap_formats.iter().find(|f| f.depth == depth).ok_or("no pixmap format for the root depth")?;
        if format.bits_per_pixel != 32 { return Err(format!("root depth {depth} is not 32 bits per pixel")); }
        let len = width as usize * height as usize * 4;

        // NonEmpty: one event each time the root goes from clean to damaged; re-armed in `wait`.
        let damage = conn.generate_id().map_err(text)?;
        conn.damage_create(damage, root, ReportLevel::NON_EMPTY).map_err(text)?.check().map_err(text)?;

        // Nothing below can fail without also releasing the local mapping, so every error path is covered here.
        let seg = conn.generate_id().map_err(text)?;
        let shmid = unsafe { libc::shmget(libc::IPC_PRIVATE, len, libc::IPC_CREAT | 0o600) };
        if shmid < 0 { return Err("shmget failed".into()); }
        let shm_ptr = unsafe { libc::shmat(shmid, std::ptr::null(), 0) } as *mut u8;
        if shm_ptr as isize == -1 {
            unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
            return Err("shmat failed".into());
        }
        let attach = conn.shm_attach(seg, shmid as u32, false).map_err(text).and_then(|c| c.check().map_err(text));
        // Marked for removal either way; the kernel frees it after the last detach.
        unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
        if let Err(error) = attach {
            unsafe { libc::shmdt(shm_ptr as *const libc::c_void) };
            return Err(error);
        }
        Ok(Self { conn, root, damage, damage_event: damage_info.first_event, width, height, seg, shm_ptr, len })
    }

    pub fn size(&self) -> (u32, u32) { (self.width as u32, self.height as u32) }

    pub fn shm_raw_ptr(&self) -> *mut u8 { self.shm_ptr }

    pub fn shm_size(&self) -> usize { self.len }

    pub fn refresh(&mut self) -> Result<(), String> {
        self.conn.shm_get_image(self.root, 0, 0, self.width, self.height, !0, Z_PIXMAP, self.seg, 0)
            .map_err(text)?.reply().map_err(text)?;
        Ok(())
    }

    /// Waits up to `timeout` for the screen to change. Returns `true` on change, `false` on timeout.
    pub fn wait(&mut self, timeout: Duration) -> Result<bool, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let mut changed = false;
            while let Some(event) = self.conn.poll_for_raw_event().map_err(text)? {
                // XDamageNotify is the extension's first event; the wire code keeps the high bit for synthetic events.
                if event[0] & 0x7f == self.damage_event { changed = true; }
            }
            if changed {
                self.conn.damage_subtract(self.damage, NONE, NONE).map_err(text)?;
                self.conn.flush().map_err(text)?;
                return Ok(true);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() { return Ok(false); }
            let mut pfd = libc::pollfd { fd: self.conn.stream().as_raw_fd(), events: libc::POLLIN, revents: 0 };
            match unsafe { libc::poll(&mut pfd, 1, remaining.as_millis().max(1) as i32) } {
                ready if ready < 0 => return Err("poll on the X connection failed".into()),
                // Readable but not yet a whole event: poll_for_raw_event would return None and poll would
                // fire again at once, spinning a core. Block for the event instead.
                ready if ready > 0 => {
                    let event = self.conn.wait_for_raw_event().map_err(text)?;
                    if event[0] & 0x7f == self.damage_event { changed = true; }
                    if changed {
                        self.conn.damage_subtract(self.damage, NONE, NONE).map_err(text)?;
                        self.conn.flush().map_err(text)?;
                        return Ok(true);
                    }
                }
                _ => {}
            }
        }
    }

    /// Copies the root into the shared segment and returns it as rows of `width * 4` bytes.
    /// The slice is valid until the next call.
    pub fn capture(&mut self) -> Result<&[u8], String> {
        self.conn.shm_get_image(self.root, 0, 0, self.width, self.height, !0, Z_PIXMAP, self.seg, 0)
            .map_err(text)?.reply().map_err(text)?;
        Ok(unsafe { std::slice::from_raw_parts(self.shm_ptr, self.len) })
    }
}

impl Drop for DamageCapture {
    fn drop(&mut self) {
        let _ = self.conn.damage_destroy(self.damage);
        let _ = self.conn.shm_detach(self.seg);
        let _ = self.conn.flush();
        unsafe { libc::shmdt(self.shm_ptr as *const libc::c_void) };
    }
}
