//! Optional desktop engines loaded at runtime (`--features engine-plugins`).
//!
//! The Wayland (wlroots) and GNOME/KDE portal engines link libwayland/libxkbcommon and
//! GIO/PipeWire/libei. Linking them into `uuremote-server` would stop it from starting on X11 hosts
//! without those libraries, and libei is missing on older distributions. With engine-plugins they
//! are built as `libuu-wayland.so` / `libuu-portal.so`, shipped next to the binary, and opened with
//! dlopen only when the session needs them, so one release package serves every backend.
//! Without the feature the engines are linked statically as before and nothing here is used.
#![cfg_attr(not(feature = "engine-plugins"), allow(dead_code))]

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine { Wayland, Portal }

impl Engine {
    pub fn file(self) -> &'static str { match self { Self::Wayland => "libuu-wayland.so", Self::Portal => "libuu-portal.so" } }
    fn index(self) -> usize { self as usize }
}

/// Directories searched for engine libraries, in order.
fn search_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::var_os("UU_ENGINE_DIR").filter(|d| !d.is_empty()) { dirs.push(dir.into()); }
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|d| d.to_path_buf())) {
        dirs.push(exe_dir.join("../lib/uu-remote-linux"));
        dirs.push(exe_dir.clone());
    }
    if let Some(home) = std::env::var_os("HOME") { dirs.push(std::path::Path::new(&home).join(".local/lib/uu-remote-linux")); }
    dirs.extend(["/usr/local/lib/uu-remote-linux", "/usr/lib/uu-remote-linux"].map(std::path::PathBuf::from));
    dirs
}

#[cfg(all(target_os = "linux", feature = "engine-plugins"))]
mod native {
    use super::Engine;
    use std::sync::OnceLock;

    static HANDLES: [OnceLock<Result<usize, String>>; 2] = [OnceLock::new(), OnceLock::new()];

    fn dlerror() -> String {
        let message = unsafe { libc::dlerror() };
        if message.is_null() { return "unknown dlopen error".into(); }
        unsafe { std::ffi::CStr::from_ptr(message) }.to_string_lossy().into_owned()
    }

    fn open(engine: Engine) -> Result<usize, String> {
        for dir in super::search_dirs() {
            let path = dir.join(engine.file());
            if !path.is_file() { continue; }
            use std::os::unix::ffi::OsStrExt;
            let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { continue };
            let handle = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if handle.is_null() {
                // Usually a missing system library (e.g. libei.so.1) or a too old glibc.
                return Err(format!("{} could not be loaded: {}", path.display(), dlerror()));
            }
            log::info!("Desktop engine loaded: {}", path.display());
            return Ok(handle as usize);
        }
        Err(format!("{} not found (looked next to uuremote-server, in ../lib/uu-remote-linux, ~/.local/lib/uu-remote-linux, /usr/lib/uu-remote-linux; UU_ENGINE_DIR overrides)", engine.file()))
    }

    pub fn load(engine: Engine) -> Result<(), String> {
        HANDLES[engine.index()].get_or_init(|| open(engine)).as_ref().map(|_| ()).map_err(Clone::clone)
    }

    /// Address of `name` (NUL-terminated) in the engine, or 0 when the engine or symbol is missing.
    pub fn symbol(engine: Engine, name: &'static str) -> usize {
        let Ok(handle) = HANDLES[engine.index()].get_or_init(|| open(engine)) else { return 0 };
        let address = unsafe { libc::dlsym(*handle as *mut libc::c_void, name.as_ptr().cast()) };
        if address.is_null() { log::warn!("Desktop engine {} lacks {}", engine.file(), name.trim_end_matches('\0')); }
        address as usize
    }
}

/// Loads `engine`, or explains why it is unavailable. Always Ok when the engines are linked in.
pub fn load(engine: Engine) -> Result<(), String> {
    #[cfg(all(target_os = "linux", feature = "engine-plugins"))]
    { native::load(engine) }
    #[cfg(not(all(target_os = "linux", feature = "engine-plugins")))]
    { let _ = engine; Ok(()) }
}

#[cfg(all(target_os = "linux", feature = "engine-plugins"))]
pub use native::symbol;

/// Value returned by an engine call whose library could not be loaded (callers check `load` first).
pub trait Missing { fn missing() -> Self; }
impl Missing for () { fn missing() -> Self {} }
impl Missing for i32 { fn missing() -> Self { -1 } }
impl Missing for usize { fn missing() -> Self { 0 } }
impl<T> Missing for *mut T { fn missing() -> Self { std::ptr::null_mut() } }

/// Declares engine C functions: an `extern "C"` block when linked statically, or dlsym-resolved
/// wrappers with the same names and signatures with `engine-plugins`.
#[macro_export]
macro_rules! engine_ffi {
    ($engine:expr; $(fn $name:ident($($arg:ident: $ty:ty),* $(,)?) $(-> $ret:ty)?;)*) => {
        #[cfg(not(feature = "engine-plugins"))]
        extern "C" { $(fn $name($($arg: $ty),*) $(-> $ret)?;)* }
        $(
            #[cfg(feature = "engine-plugins")]
            unsafe fn $name($($arg: $ty),*) $(-> $ret)? {
                static ADDRESS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
                let address = *ADDRESS.get_or_init(|| $crate::engine_plugin::symbol($engine, concat!(stringify!($name), "\0")));
                if address == 0 { return $crate::engine_plugin::Missing::missing(); }
                let function: unsafe extern "C" fn($($ty),*) $(-> $ret)? = unsafe { std::mem::transmute(address) };
                unsafe { function($($arg),*) }
            }
        )*
    };
}

#[cfg(test)]
mod tests {
    #[test]
    fn search_prefers_override_then_package_layout() {
        let dirs = super::search_dirs();
        let exe_dir = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
        assert!(dirs.iter().position(|d| *d == exe_dir.join("../lib/uu-remote-linux")) < dirs.iter().position(|d| *d == exe_dir));
        assert_eq!(dirs.last().unwrap(), std::path::Path::new("/usr/lib/uu-remote-linux"));
    }
}
