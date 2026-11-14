//! Typed C boundary for the HarmonyOS app; no process exit or signal handlers.
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::watch;
static CONFIG: OnceLock<String> = OnceLock::new();
static WIDTH: AtomicU32 = AtomicU32::new(0);
static HEIGHT: AtomicU32 = AtomicU32::new(0);
static STOP: Mutex<Option<watch::Sender<bool>>> = Mutex::new(None);
static TASK: Mutex<Option<tokio::task::JoinHandle<()>>> = Mutex::new(None);
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("HarmonyOS runtime")
    })
}
extern "C" {
    fn uu_platform_log(level: i32, text: *const libc::c_char);
    fn uu_login_clear();
    fn uu_input_authorized() -> bool;
    fn uu_video_config(fps: u32, kbps: u32) -> i32;
    fn uu_video_poll(data: *mut u8, capacity: usize) -> i32;
    fn uu_video_request_idr();
    fn uu_platform_status(text: *const libc::c_char);
}
pub(crate) fn status(text: &str) {
    if let Ok(text) = std::ffi::CString::new(text) {
        unsafe {
            uu_platform_status(text.as_ptr());
        }
    }
}
pub fn dimensions() -> (u32, u32) {
    (
        WIDTH.load(Ordering::Acquire),
        HEIGHT.load(Ordering::Acquire),
    )
}
#[no_mangle]
pub unsafe extern "C" fn uu_host_init(path: *const libc::c_char, width: u32, height: u32,
    language: *const libc::c_char) -> i32 {
    if path.is_null() || language.is_null() || width == 0 || height == 0 || width > 16384 || height > 16384 {
        return -1;
    }
    let Ok(path) = std::ffi::CStr::from_ptr(path).to_str() else {
        return -1;
    };
    let Ok(language) = std::ffi::CStr::from_ptr(language).to_str() else { return -1; };
    if !matches!(language, "zh" | "en") { return -1; }
    // Install process settings once, before creating any runtime threads.
    let configured = CONFIG.get_or_init(|| {
        std::env::set_var("UU_CONFIG", path);
        std::env::set_var("UU_LANG", language);
        std::env::set_var("UU_REMOTE_ASSIST", "1");
        std::env::set_var("UU_LOGIN_WINDOW", "0");
        std::env::set_var("UU_AUDIO", "0");
        std::env::set_var("UU_DESKTOP_RESIZE", "0");
        path.to_owned()
    });
    if configured != path {
        return -1;
    }
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Info);
    WIDTH.store(width, Ordering::Release);
    HEIGHT.store(height, Ordering::Release);
    status(crate::i18n::s("准备就绪", "Ready"));
    0
}
#[no_mangle]
pub extern "C" fn uu_host_login() -> i32 {
    let mut task = TASK.lock().unwrap();
    if task.as_ref().is_some_and(|t| !t.is_finished()) {
        return -1;
    }
    *task = Some(runtime().spawn(async {
        let ui = crate::login_ui::LoginUi::attached();
        match crate::login::flow(&ui).await {
            Ok(_) => {
                unsafe {
                    uu_login_clear();
                }
                status(crate::i18n::s("已登录，可以开始共享", "Signed in, ready to share"));
            }
            Err(_) => status(crate::i18n::s("登录失败，请刷新二维码重试", "Sign-in failed. Refresh the QR code to retry.")),
        }
    }));
    0
}
#[no_mangle]
pub extern "C" fn uu_host_logged_in() -> bool {
    crate::config::read_fresh()
        .ok()
        .and_then(|f| crate::auth::Credentials::load(f.as_ref()).ok())
        .is_some()
}
#[no_mangle]
pub extern "C" fn uu_host_start() -> i32 {
    if !uu_host_logged_in() {
        return -1;
    }
    let mut task = TASK.lock().unwrap();
    if task.as_ref().is_some_and(|t| !t.is_finished()) {
        return -2;
    }
    let (stop, mut shutdown) = watch::channel(false);
    *STOP.lock().unwrap() = Some(stop.clone());
    *task = Some(runtime().spawn(async move {
        let capture = match crate::capture::ScreenCapture::new() { Ok(c) => Arc::new(c), Err(_) => { status(crate::i18n::s("屏幕初始化失败", "Display initialization failed")); return; } };
        let input = Arc::new(crate::input::InputInjector::new());
        let sessions = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
        let mut serving_shutdown = shutdown.clone();
        let credentials = match crate::config::read_fresh().and_then(|file| crate::auth::Credentials::load(file.as_ref())) {
            Ok(credentials) => credentials,
            Err(_) => { status(crate::i18n::s("登录已失效，请重新扫码", "Sign-in expired. Scan a new QR code.")); return; }
        };
        let client = Arc::new(crate::auth::UUAuthClient::new(credentials.clone()));
        let session = match crate::assist::Assist::start_account(client, credentials) {
            Ok(session) => session,
            Err(_) => { status(crate::i18n::s("远程协助初始化失败，请重新开始共享", "Assistance initialization failed. Restart sharing.")); return; }
        };
        let (mut commands, command_worker) = session.commands(stop);
        let mut assist = Some(session);
        let pointer = input.start_pointer_worker();
        status(crate::i18n::s("正在连接 UU 服务…", "Connecting to UU…"));
        let success = tokio::select! {
            r = crate::serve(capture, input.clone(), &mut serving_shutdown, &mut assist, &mut commands, sessions.clone()) => r,
            _ = shutdown.changed() => Ok(()),
        }.is_ok();
        command_worker.abort();
        let _ = command_worker.await;
        input.shutdown(); pointer.abort();
        for (_, peer) in sessions.lock().await.drain() { crate::close_peer(&peer.pc).await; }
        if let Some(session) = assist.as_mut() {
            session.allowed = false;
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), session.cancel()).await;
        }
        status(if success { crate::i18n::s("共享已停止", "Sharing stopped") }
            else { crate::i18n::s("连接失败，请停止后重试", "Connection failed. Stop sharing and retry.") });
    }));
    0
}
#[no_mangle]
pub extern "C" fn uu_host_stop() {
    if let Some(stop) = STOP.lock().unwrap().take() {
        let _ = stop.send(true);
    } else if let Some(task) = TASK.lock().unwrap().take() {
        task.abort();
    }
}
pub struct Encoder {
    buffer: Vec<u8>,
}
impl Encoder {
    pub fn open(_: (u32, u32), fps: u32, kbps: u32) -> Result<Self, String> {
        let code = unsafe { uu_video_config(fps.min(60), kbps) };
        if code != 0 {
            return Err(format!("HarmonyOS encoder setup failed: {code}"));
        }
        Ok(Self {
            buffer: vec![0; 8 * 1024 * 1024],
        })
    }
    pub fn request_idr(&self) {
        unsafe {
            uu_video_request_idr();
        }
    }
    pub fn poll(&mut self) -> Result<Option<Vec<u8>>, String> {
        let size = unsafe { uu_video_poll(self.buffer.as_mut_ptr(), self.buffer.len()) };
        if size < 0 {
            return Err(format!("HarmonyOS capture stopped: {size}"));
        }
        if size == 0 {
            return Ok(None);
        }
        Ok(Some(self.buffer[..size as usize].to_vec()))
    }
}

pub fn input_permission() -> crate::system_metrics::Permission {
    if unsafe { uu_input_authorized() } {
        crate::system_metrics::Permission::Allow
    } else {
        crate::system_metrics::Permission::Reject
    }
}

struct PlatformLogger;
static LOGGER: PlatformLogger = PlatformLogger;
impl log::Log for PlatformLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Info
    }
    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            if let Ok(text) = std::ffi::CString::new(format!("{}", record.args())) {
                unsafe {
                    uu_platform_log(record.level() as i32, text.as_ptr());
                }
            }
        }
    }
    fn flush(&self) {}
}
