//! In-process X11 capture (XDamage + XShm) and NVENC encoding through the NVIDIA driver.
//! With UU_VIDEO_ENCODER=vaapi, H.264 / HEVC go through VAAPI instead: a DRI3 DMA-BUF of the root window is
//! imported as a VA surface and converted/scaled by VPP, so the CPU never touches the pixels (XShm upload
//! when DRI3 is unavailable).
//! When capture and encoder sizes match, the XShm buffer is registered as CUDA pinned memory and
//! NVENC reads it by DMA, so there is no host-to-device copy (the X server still writes XShm).

use std::collections::VecDeque;
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};

use crate::damage_capture::DamageCapture;

#[repr(C)]
struct NvencEngineOpaque {
    _private: [u8; 0],
}

#[repr(C)]
struct NvencConfigFFI {
    width: u32,
    height: u32,
    fps: u32,
    gop: u32,
    cq: u32,
    target_bps: i64,
    max_bps: i64,
    buffer_bits: i64,
    is_hevc: libc::c_int,
    chroma_444: libc::c_int,
}

#[repr(C)]
struct VaapiEngineOpaque {
    _private: [u8; 0],
}

#[repr(C)]
struct VaapiConfigFFI {
    width: u32,
    height: u32,
    cap_width: u32,
    cap_height: u32,
    fps: u32,
    gop: u32,
    target_bps: i64,
    max_bps: i64,
    buffer_bits: i64,
    is_hevc: libc::c_int,
    device: *const libc::c_char,
}

#[repr(C)]
struct Dri3CaptureOpaque {
    _private: [u8; 0],
}

#[repr(C)]
struct X264cEngineOpaque {
    _private: [u8; 0],
}

#[repr(C)]
struct X264cConfigFFI {
    width: u32,
    height: u32,
    fps: u32,
    gop: u32,
    cq: u32,
    target_bps: i64,
    max_bps: i64,
    buffer_bits: i64,
}

extern "C" {
    fn nvenc_create(config: *const NvencConfigFFI) -> *mut NvencEngineOpaque;
    fn nvenc_register_host_buffer(
        engine: *mut NvencEngineOpaque,
        host_ptr: *mut libc::c_void,
        size: usize,
    ) -> libc::c_int;
    fn nvenc_encode_frame(
        engine: *mut NvencEngineOpaque,
        input_argb: *const u8,
        pitch: u32,
        force_idr: libc::c_int,
        out_data: *mut *const u8,
        out_len: *mut u32,
    ) -> libc::c_int;
    fn nvenc_destroy(engine: *mut NvencEngineOpaque);

    fn vaapi_create(config: *const VaapiConfigFFI) -> *mut VaapiEngineOpaque;
    fn vaapi_import_dma_buffer(engine: *mut VaapiEngineOpaque, dma_fd: libc::c_int, stride: u32) -> libc::c_int;
    fn vaapi_upload_host_buffer(engine: *mut VaapiEngineOpaque, host_ptr: *const u8, pitch: u32) -> libc::c_int;
    fn vaapi_encode_frame(
        engine: *mut VaapiEngineOpaque,
        force_idr: libc::c_int,
        out_data: *mut *const u8,
        out_len: *mut u32,
    ) -> libc::c_int;
    fn vaapi_destroy(engine: *mut VaapiEngineOpaque);

    fn dri3_capture_create(display_name: *const libc::c_char, width: u32, height: u32) -> *mut Dri3CaptureOpaque;
    fn dri3_capture_blit(cap: *mut Dri3CaptureOpaque) -> libc::c_int;
    fn dri3_capture_wait_damage(cap: *mut Dri3CaptureOpaque, timeout_ms: libc::c_int) -> libc::c_int;
    fn dri3_capture_get_dma_fd(cap: *mut Dri3CaptureOpaque) -> libc::c_int;
    fn dri3_capture_get_stride(cap: *mut Dri3CaptureOpaque) -> u32;
    fn dri3_capture_destroy(cap: *mut Dri3CaptureOpaque);

    fn x264c_create(config: *const X264cConfigFFI) -> *mut X264cEngineOpaque;
    fn x264c_encode_frame(
        engine: *mut X264cEngineOpaque,
        input_bgrx: *const u8,
        pitch: u32,
        force_idr: libc::c_int,
        out_data: *mut *const u8,
        out_len: *mut u32,
    ) -> libc::c_int;
    fn x264c_destroy(engine: *mut X264cEngineOpaque);
}

struct SafeNvencEngine(*mut NvencEngineOpaque);

unsafe impl Send for SafeNvencEngine {}

impl Drop for SafeNvencEngine {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { nvenc_destroy(self.0) };
            self.0 = std::ptr::null_mut();
        }
    }
}

struct SafeVaapiEngine(*mut VaapiEngineOpaque);
unsafe impl Send for SafeVaapiEngine {}
impl Drop for SafeVaapiEngine {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { vaapi_destroy(self.0) };
            self.0 = std::ptr::null_mut();
        }
    }
}

struct SafeDri3Capture(*mut Dri3CaptureOpaque);
unsafe impl Send for SafeDri3Capture {}
impl Drop for SafeDri3Capture {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { dri3_capture_destroy(self.0) };
            self.0 = std::ptr::null_mut();
        }
    }
}

struct SafeX264Engine(*mut X264cEngineOpaque);
unsafe impl Send for SafeX264Engine {}
impl Drop for SafeX264Engine {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { x264c_destroy(self.0) };
            self.0 = std::ptr::null_mut();
        }
    }
}

pub struct Config {
    pub display: String,
    pub input: (u32, u32),
    pub output: Option<(u32, u32)>,
    pub fps: u32,
    pub gop: u32,
    pub cq: u32,
    pub target_bps: i64,
    pub max_bps: i64,
    pub buffer_bits: i64,
    pub codec: crate::capture_setting::VideoCodec,
    /// ChromaFormat (capture_setting::CHROMA_444 = 4:4:4).
    pub chroma: u64,
    /// UU_VIDEO_ENCODER=vaapi: the VAAPI engine (H.264 / HEVC Main) instead of NVENC.
    pub vaapi: bool,
    /// Encoder::Cpu with H.264 on X11: the in-process libx264 engine (no FFmpeg child).
    pub cpu: bool,
}

pub fn disabled() -> bool {
    std::env::var("UU_VIDEO_PIPELINE").is_ok_and(|v| v == "process")
}

pub fn damage_enabled() -> bool {
    std::env::var("UU_CAPTURE").map_or(true, |v| v != "grab")
}

pub fn selected(encoder: crate::video::Encoder) -> bool {
    matches!(encoder, crate::video::Encoder::Nvenc | crate::video::Encoder::Vaapi) && damage_enabled() && !disabled() && crate::config::ffmpeg_root().is_none()
}

/// One encoder thread per stream. Aborting the reader only asks the blocking thread to stop at its
/// next wake-up, so a restart waits here until the previous NVENC session and XShm are released.
/// The key is the stream's IDR flag, which survives encoder restarts.
pub async fn stream_slot(stream: &Arc<AtomicBool>) -> tokio::sync::OwnedMutexGuard<()> {
    type Slots = Vec<(std::sync::Weak<AtomicBool>, Arc<tokio::sync::Mutex<()>>)>;
    static SLOTS: std::sync::Mutex<Slots> = std::sync::Mutex::new(Vec::new());
    let slot = {
        let mut slots = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        slots.retain(|(key, _)| key.strong_count() > 0);
        match slots.iter().find(|(key, _)| std::ptr::eq(key.as_ptr(), Arc::as_ptr(stream))) {
            Some((_, slot)) => slot.clone(),
            None => {
                let slot = Arc::new(tokio::sync::Mutex::new(()));
                slots.push((Arc::downgrade(stream), slot.clone()));
                slot
            }
        }
    };
    slot.lock_owned().await
}

pub struct EncodedUnit {
    pub data: Vec<u8>,
    pub permit: OwnedSemaphorePermit,
    pub completed_at: Instant,
}

fn acquire(
    budget: &Arc<Semaphore>,
    units: &mpsc::Sender<EncodedUnit>,
) -> Option<OwnedSemaphorePermit> {
    tokio::runtime::Handle::current().block_on(async {
        tokio::select! {
            permit = budget.clone().acquire_owned() => permit.ok(),
            _ = units.closed() => None,
        }
    })
}

fn random_keepalive(seed: &mut u32) -> Duration {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 17;
    *seed ^= *seed << 5;
    Duration::from_micros(40_000 + u64::from(*seed % 26_700))
}

pub fn run(
    config: Config,
    units: mpsc::Sender<EncodedUnit>,
    stop: Arc<AtomicBool>,
    idr: Arc<AtomicBool>,
    budget: Arc<Semaphore>,
) -> Result<(), String> {
    if config.vaapi {
        run_vaapi(config, units, stop, idr, budget)
    } else if config.cpu {
        run_x264(config, units, stop, idr, budget)
    } else {
        run_nvenc(config, units, stop, idr, budget)
    }
}

/// In-process libx264 (H.264 only): XShm -> I420 on the CPU -> x264 in a dlopen'ed libx264.
/// The same capture / keepalive / budget loop as the NVENC copy path, minus device upload.
fn run_x264(
    config: Config,
    units: mpsc::Sender<EncodedUnit>,
    stop: Arc<AtomicBool>,
    idr: Arc<AtomicBool>,
    budget: Arc<Semaphore>,
) -> Result<(), String> {
    let (width, height) = config.output.unwrap_or(config.input);
    let (width, height) = (width + width % 2, height + height % 2);
    log::info!(
        "Initializing Native In-Process x264: {}x{}, {} fps, GOP {}, CQ {}",
        width,
        height,
        config.fps,
        config.gop,
        config.cq
    );

    let ffi_cfg = X264cConfigFFI {
        width,
        height,
        fps: config.fps,
        gop: config.gop,
        cq: config.cq,
        target_bps: config.target_bps,
        max_bps: config.max_bps,
        buffer_bits: config.buffer_bits,
    };

    let mut capture = DamageCapture::open(&config.display)
        .map_err(|e| format!("DamageCapture::open failed on {}: {e}", config.display))?;

    let raw_engine = unsafe { x264c_create(&ffi_cfg) };
    if raw_engine.is_null() {
        return Err("Failed to create native x264 engine (libx264 unavailable or wrong X264_BUILD)".into());
    }
    let engine = SafeX264Engine(raw_engine);
    let (cap_width, cap_height) = capture.size();
    if cap_width != width || cap_height != height {
        log::info!(
            "Capture size ({}x{}) != encoder size ({}x{}): {} on the CPU before x264",
            cap_width,
            cap_height,
            width,
            height,
            if config.output.is_some() { "bilinear scaling" } else { "padding" }
        );
    }

    let bytes_per_line = cap_width as usize * 4;
    let frame_interval = Duration::from_nanos(1_000_000_000 / u64::from(config.fps));

    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |d| d.subsec_nanos())
        | 1;
    let mut keepalive = random_keepalive(&mut seed);
    let mut dirty = true;
    let mut next_tick = Instant::now();
    let mut last_send = Instant::now();
    let mut permits = VecDeque::with_capacity(4);
    let mut frame_buf = Vec::new();

    let locked = crate::session_lock::watch_locked();
    let mut last_locked = *locked.borrow();

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        let deadline = if dirty {
            next_tick
        } else {
            last_send + keepalive
        };
        let wait = deadline.saturating_duration_since(Instant::now());

        if capture.wait(wait.max(Duration::from_millis(1)))? {
            dirty = true;
        }

        let now = Instant::now();
        if dirty && now < next_tick {
            continue;
        }
        if !dirty && now < last_send + keepalive {
            continue;
        }

        let Some(permit) = acquire(&budget, &units) else {
            return Ok(());
        };
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        dirty |= capture.wait(Duration::ZERO)?;
        let state = *locked.borrow();
        if state != last_locked {
            dirty = true;
            last_locked = state;
        }

        let force_idr = if idr.swap(false, Ordering::AcqRel) {
            1
        } else {
            0
        };

        // Keepalive frames re-encode the current screen contents as a tiny P-frame.
        let source = capture.capture()?;
        let (target_ptr, pitch) = if cap_width == width && cap_height == height {
            (source.as_ptr(), bytes_per_line as u32)
        } else {
            frame_buf.resize(width as usize * height as usize * 4, 0);
            if config.output.is_some() {
                scale_bilinear_argb32(source, cap_width, cap_height, &mut frame_buf, width, height);
            } else {
                pad_argb32(source, cap_width, cap_height, &mut frame_buf, width, height);
            }
            (frame_buf.as_ptr(), width * 4)
        };

        let mut out_data: *const u8 = std::ptr::null();
        let mut out_len: u32 = 0;
        let res = unsafe { x264c_encode_frame(engine.0, target_ptr, pitch, force_idr, &mut out_data, &mut out_len) };
        if res != 0 {
            return Err(format!("x264c_encode_frame failed with error {res}"));
        }

        if out_len > 0 && !out_data.is_null() {
            let data = unsafe { std::slice::from_raw_parts(out_data, out_len as usize) }.to_vec();
            permits.push_back(permit);
            let send_permit = permits.pop_front().unwrap();

            if units
                .blocking_send(EncodedUnit {
                    data,
                    permit: send_permit,
                    completed_at: Instant::now(),
                })
                .is_err()
            {
                return Ok(());
            }
        }

        let now = Instant::now();
        if dirty {
            let next = next_tick + frame_interval;
            next_tick = if next > now {
                next
            } else {
                now + frame_interval
            };
            dirty = false;
        }
        last_send = now;
        keepalive = random_keepalive(&mut seed);
    }
}

fn run_vaapi(
    config: Config,
    units: mpsc::Sender<EncodedUnit>,
    stop: Arc<AtomicBool>,
    idr: Arc<AtomicBool>,
    budget: Arc<Semaphore>,
) -> Result<(), String> {
    let (width, height) = config.output.unwrap_or(config.input);
    let (width, height) = (width + width % 2, height + height % 2);
    let (cap_width, cap_height) = config.input;
    log::info!(
        "Initializing Native In-Process VAAPI: {}x{} -> {}x{}, {} fps, GOP {}, HEVC={}",
        cap_width,
        cap_height,
        width,
        height,
        config.fps,
        config.gop,
        config.codec == crate::capture_setting::VideoCodec::H265
    );
    let device = CString::new(crate::video::vaapi_device()).map_err(|_| "UU_VAAPI_DEVICE contains NUL".to_string())?;
    let ffi_cfg = VaapiConfigFFI {
        width,
        height,
        cap_width,
        cap_height,
        fps: config.fps,
        gop: config.gop,
        target_bps: config.target_bps,
        max_bps: config.max_bps,
        buffer_bits: config.buffer_bits,
        is_hevc: libc::c_int::from(config.codec == crate::capture_setting::VideoCodec::H265),
        device: device.as_ptr(),
    };
    let raw_engine = unsafe { vaapi_create(&ffi_cfg) };
    if raw_engine.is_null() {
        return Err("Failed to create native VAAPI engine".into());
    }
    let engine = SafeVaapiEngine(raw_engine);

    // Capture stays at the full input size; VPP scales (or pads) to the encoder size.
    let display = CString::new(config.display.as_str()).map_err(|_| "DISPLAY contains NUL".to_string())?;
    let dri3 = Some(SafeDri3Capture(unsafe { dri3_capture_create(display.as_ptr(), cap_width, cap_height) }))
        .filter(|c| !c.0.is_null())
        .filter(|c| unsafe {
            let (fd, stride) = (dri3_capture_get_dma_fd(c.0), dri3_capture_get_stride(c.0));
            fd >= 0 && stride > 0 && vaapi_import_dma_buffer(engine.0, fd, stride) == 0
        });
    let mut capture = None;
    if dri3.is_some() {
        log::info!("Native VAAPI imports a DRI3 DMA-BUF of the root window (GPU blit, VPP to NV12)");
    } else {
        log::warn!("DRI3 DMA-BUF unavailable on {}, uploading XShm frames to VAAPI", config.display);
        capture = Some(
            DamageCapture::open(&config.display)
                .map_err(|e| format!("DamageCapture::open failed on {}: {e}", config.display))?,
        );
    }

    let frame_interval = Duration::from_nanos(1_000_000_000 / u64::from(config.fps));
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |d| d.subsec_nanos())
        | 1;
    let mut keepalive = random_keepalive(&mut seed);
    let mut dirty = true;
    let mut next_tick = Instant::now();
    let mut last_send = Instant::now();
    let mut permits = VecDeque::with_capacity(4);

    let locked = crate::session_lock::watch_locked();
    let mut last_locked = *locked.borrow();

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        let deadline = if dirty {
            next_tick
        } else {
            last_send + keepalive
        };
        let timeout = deadline.saturating_duration_since(Instant::now());

        if vaapi_wait(&dri3, &mut capture, timeout.max(Duration::from_millis(1)))? {
            dirty = true;
        }

        let now = Instant::now();
        if dirty && now < next_tick {
            continue;
        }
        if !dirty && now < last_send + keepalive {
            continue;
        }

        let Some(permit) = acquire(&budget, &units) else {
            return Ok(());
        };
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        dirty |= vaapi_wait(&dri3, &mut capture, Duration::ZERO)?;
        let state = *locked.borrow();
        if state != last_locked {
            dirty = true;
            last_locked = state;
        }

        let force_idr = if idr.swap(false, Ordering::AcqRel) {
            1
        } else {
            0
        };

        // Keepalive frames re-encode the last surface; only damage needs a new copy of the screen.
        if dirty {
            match (dri3.as_ref(), capture.as_mut()) {
                (Some(c), _) => {
                    if unsafe { dri3_capture_blit(c.0) } != 0 {
                        return Err("DRI3 blit of the root window failed".into());
                    }
                }
                (None, Some(c)) => {
                    let pitch = c.size().0 * 4;
                    let res = unsafe { vaapi_upload_host_buffer(engine.0, c.capture()?.as_ptr(), pitch) };
                    if res != 0 {
                        return Err(format!("vaapi_upload_host_buffer failed with error {res}"));
                    }
                }
                (None, None) => unreachable!(),
            }
        }

        let mut out_data: *const u8 = std::ptr::null();
        let mut out_len: u32 = 0;
        let res = unsafe { vaapi_encode_frame(engine.0, force_idr, &mut out_data, &mut out_len) };
        if res != 0 {
            return Err(format!("vaapi_encode_frame failed with error {res}"));
        }

        if out_len > 0 && !out_data.is_null() {
            let data = unsafe { std::slice::from_raw_parts(out_data, out_len as usize) }.to_vec();
            permits.push_back(permit);
            let send_permit = permits.pop_front().unwrap();

            if units
                .blocking_send(EncodedUnit {
                    data,
                    permit: send_permit,
                    completed_at: Instant::now(),
                })
                .is_err()
            {
                return Ok(());
            }
        }

        let now = Instant::now();
        if dirty {
            let next = next_tick + frame_interval;
            next_tick = if next > now {
                next
            } else {
                now + frame_interval
            };
            dirty = false;
        }
        last_send = now;
        keepalive = random_keepalive(&mut seed);
    }
}
fn vaapi_wait(dri3: &Option<SafeDri3Capture>, capture: &mut Option<DamageCapture>, timeout: Duration) -> Result<bool, String> {
    match (dri3, capture) {
        (Some(c), _) => match unsafe { dri3_capture_wait_damage(c.0, timeout.as_millis().min(1000) as i32) } {
            r if r < 0 => Err("XDamage wait on the DRI3 connection failed".into()),
            r => Ok(r > 0),
        },
        (None, Some(c)) => c.wait(timeout),
        (None, None) => unreachable!(),
    }
}

fn run_nvenc(
    config: Config,
    units: mpsc::Sender<EncodedUnit>,
    stop: Arc<AtomicBool>,
    idr: Arc<AtomicBool>,
    budget: Arc<Semaphore>,
) -> Result<(), String> {
    let (width, height) = config.output.unwrap_or(config.input);
    let (width, height) = (width + width % 2, height + height % 2);
    let is_hevc = if config.codec == crate::capture_setting::VideoCodec::H265 {
        1
    } else {
        0
    };

    let chroma_444 = libc::c_int::from(config.chroma == crate::capture_setting::CHROMA_444);
    log::info!(
        "Initializing Native In-Process NVENC: {}x{}, {} fps, GOP {}, CQ {}, HEVC={}, 4:4:4={}",
        width,
        height,
        config.fps,
        config.gop,
        config.cq,
        is_hevc == 1,
        chroma_444 == 1
    );

    let ffi_cfg = NvencConfigFFI {
        width,
        height,
        fps: config.fps,
        gop: config.gop,
        cq: config.cq,
        target_bps: config.target_bps,
        max_bps: config.max_bps,
        buffer_bits: config.buffer_bits,
        is_hevc,
        chroma_444,
    };

    let mut capture = DamageCapture::open(&config.display)
        .map_err(|e| format!("DamageCapture::open failed on {}: {e}", config.display))?;

    let raw_engine = unsafe { nvenc_create(&ffi_cfg) };
    if raw_engine.is_null() {
        return Err("Failed to create native NVENC engine".into());
    }
    let engine = SafeNvencEngine(raw_engine);
    let (cap_width, cap_height) = capture.size();

    // Check if capture buffer can be Zero-Copy registered directly
    let mut zero_copy = false;
    if cap_width == width && cap_height == height {
        let shm_ptr = capture.shm_raw_ptr() as *mut libc::c_void;
        let shm_len = capture.shm_size();
        let res = unsafe { nvenc_register_host_buffer(engine.0, shm_ptr, shm_len) };
        if res == 0 {
            zero_copy = true;
            log::info!("Native NVENC reads the registered XShm buffer directly (no host-to-device copy)");
        } else {
            log::warn!("nvenc_register_host_buffer returned {res}, copying frames with cuMemcpyHtoD");
        }
    } else {
        log::info!(
            "Capture size ({}x{}) != encoder size ({}x{}): {} on the CPU, then cuMemcpyHtoD",
            cap_width,
            cap_height,
            width,
            height,
            if config.output.is_some() { "bilinear scaling" } else { "padding" }
        );
    }

    let bytes_per_line = cap_width as usize * 4;
    let frame_interval = Duration::from_nanos(1_000_000_000 / u64::from(config.fps));

    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |d| d.subsec_nanos())
        | 1;
    let mut keepalive = random_keepalive(&mut seed);
    let mut dirty = true;
    let mut next_tick = Instant::now();
    let mut last_send = Instant::now();
    let mut permits = VecDeque::with_capacity(4);
    let mut frame_buf = Vec::new();

    let locked = crate::session_lock::watch_locked();
    let mut last_locked = *locked.borrow();

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        let deadline = if dirty {
            next_tick
        } else {
            last_send + keepalive
        };
        let wait = deadline.saturating_duration_since(Instant::now());

        if capture.wait(wait.max(Duration::from_millis(1)))? {
            dirty = true;
        }

        let now = Instant::now();
        if dirty && now < next_tick {
            continue;
        }
        if !dirty && now < last_send + keepalive {
            continue;
        }

        let Some(permit) = acquire(&budget, &units) else {
            return Ok(());
        };
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        dirty |= capture.wait(Duration::ZERO)?;
        let state = *locked.borrow();
        if state != last_locked {
            dirty = true;
            last_locked = state;
        }

        let force_idr = if idr.swap(false, Ordering::AcqRel) {
            1
        } else {
            0
        };

        let mut out_data: *const u8 = std::ptr::null();
        let mut out_len: u32 = 0;

        let res = if zero_copy {
            // In Zero-Copy mode: refresh XShm, then NVENC DMA-reads directly from shm_ptr
            if dirty {
                capture.refresh()?;
            }
            unsafe {
                nvenc_encode_frame(
                    engine.0,
                    std::ptr::null(), // NULL means use zero-copy registered buffer
                    bytes_per_line as u32,
                    force_idr,
                    &mut out_data,
                    &mut out_len,
                )
            }
        } else {
            // Copy path: 其他分辨率 is scaled; an odd native size is padded with black to the even encoder size
            let source = capture.capture()?;
            let (target_ptr, pitch) = if cap_width == width && cap_height == height {
                (source.as_ptr(), bytes_per_line as u32)
            } else {
                frame_buf.resize(width as usize * height as usize * 4, 0);
                if config.output.is_some() {
                    scale_bilinear_argb32(source, cap_width, cap_height, &mut frame_buf, width, height);
                } else {
                    pad_argb32(source, cap_width, cap_height, &mut frame_buf, width, height);
                }
                (frame_buf.as_ptr(), width * 4)
            };
            unsafe {
                nvenc_encode_frame(
                    engine.0,
                    target_ptr,
                    pitch,
                    force_idr,
                    &mut out_data,
                    &mut out_len,
                )
            }
        };

        if res != 0 {
            return Err(format!("nvenc_encode_frame failed with error {res}"));
        }

        if out_len > 0 && !out_data.is_null() {
            let data = unsafe { std::slice::from_raw_parts(out_data, out_len as usize) }.to_vec();
            permits.push_back(permit);
            let send_permit = permits.pop_front().unwrap();

            if units
                .blocking_send(EncodedUnit {
                    data,
                    permit: send_permit,
                    completed_at: Instant::now(),
                })
                .is_err()
            {
                return Ok(());
            }
        }

        let now = Instant::now();
        if dirty {
            let next = next_tick + frame_interval;
            next_tick = if next > now {
                next
            } else {
                now + frame_interval
            };
            dirty = false;
        }
        last_send = now;
        keepalive = random_keepalive(&mut seed);
    }
}

/// Bilinear ARGB32 scaling (the former swscale path used SWS_BILINEAR), with 8-bit fixed-point
/// weights and pixel centers aligned. Separable two-pass form: the vertical pass writes an
/// unrounded 16-bit intermediate (<= 255*256) with a row-uniform weight, the horizontal pass
/// rounds once at the end — bit-identical to the fused scalar form. On x86-64 with SSE4.1 the
/// horizontal pass runs one pixel (4 channels) per __m128i; mid is a thread-local buffer so a
/// 4K frame does not allocate megabytes per call.
fn scale_bilinear_argb32(src: &[u8], src_w: u32, src_h: u32, dst: &mut [u8], dst_w: u32, dst_h: u32) {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return;
    }
    // Source position in 1/256 pixel units: (d + 0.5) * src / dst - 0.5, clamped to the image.
    let taps = |dst_len: u32, src_len: u32| -> Vec<(usize, usize, u32)> {
        (0..dst_len)
            .map(|d| {
                let pos = ((2 * u64::from(d) + 1) * u64::from(src_len) * 128 / u64::from(dst_len)).saturating_sub(128);
                let first = ((pos >> 8) as usize).min(src_len as usize - 1);
                let second = (first + 1).min(src_len as usize - 1);
                (first, second, (pos & 255) as u32)
            })
            .collect()
    };
    let (columns, rows) = (taps(dst_w, src_w), taps(dst_h, src_h));
    let src_stride = src_w as usize * 4;
    // Vertical pass. Weights stay in u32: a fy of 0 makes the top weight 256, which must not
    // wrap in u16 before the single end rounding.
    thread_local! {
        static MID: std::cell::RefCell<Vec<u16>> = std::cell::RefCell::new(Vec::new());
    }
    MID.with(|mid| {
        let mut mid = mid.borrow_mut();
        mid.resize(dst_h as usize * src_stride, 0);
        for (d, &(y0, y1, fy)) in mid.chunks_mut(src_stride).zip(&rows) {
            let top = &src[y0 * src_stride..];
            let bottom = &src[y1 * src_stride..];
            let (a, b) = (256 - fy, fy);
            for (m, (t, bt)) in d.iter_mut().zip(top.iter().zip(bottom)).take(src_stride) {
                *m = (u32::from(*t) * a + u32::from(*bt) * b) as u16;
            }
        }
        let sse41 = sse41_available();
        for (row, dst_row) in mid.chunks(src_stride).zip(dst.chunks_exact_mut(dst_w as usize * 4)) {
            if sse41 {
                // Safety: sse41_available() checked the CPU feature; rows come from live slices.
                unsafe { horizontal_row_sse41(row, &columns, dst_row) };
            } else {
                horizontal_row_scalar(row, &columns, dst_row);
            }
        }
    });
}

fn horizontal_row_scalar(row: &[u16], columns: &[(usize, usize, u32)], dst_row: &mut [u8]) {
    for (pixel, &(x0, x1, fx)) in dst_row.chunks_exact_mut(4).zip(columns) {
        let (a, b) = (256 - fx, fx);
        for c in 0..4 {
            let t = u32::from(row[x0 * 4 + c]) * a + u32::from(row[x1 * 4 + c]) * b;
            pixel[c] = ((t + 32768) >> 16) as u8;
        }
    }
}

fn sse41_available() -> bool {
    static SSE41: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    #[cfg(target_arch = "x86_64")]
    { *SSE41.get_or_init(|| std::arch::is_x86_feature_detected!("sse4.1")) }
    #[cfg(not(target_arch = "x86_64"))]
    { let _ = &SSE41; false }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.1")]
unsafe fn horizontal_row_sse41(row: &[u16], columns: &[(usize, usize, u32)], dst_row: &mut [u8]) {
    use std::arch::x86_64::*;
    let zero = _mm_setzero_si128();
    let bias = _mm_set1_epi32(32768);
    for (pixel, &(x0, x1, fx)) in dst_row.chunks_exact_mut(4).zip(columns) {
        // Values fit u32 exactly like the scalar form: 255*256*256 max, no saturation bites.
        let p0 = _mm_loadl_epi64(row.as_ptr().add(x0 * 4) as *const __m128i);
        let p1 = _mm_loadl_epi64(row.as_ptr().add(x1 * 4) as *const __m128i);
        let w0 = _mm_unpacklo_epi16(p0, zero);
        let w1 = _mm_unpacklo_epi16(p1, zero);
        let wa = _mm_set1_epi32((256 - fx) as i32);
        let wb = _mm_set1_epi32(fx as i32);
        let t = _mm_add_epi32(_mm_add_epi32(_mm_mullo_epi32(w0, wa), _mm_mullo_epi32(w1, wb)), bias);
        let s = _mm_srli_epi32(t, 16);
        let packed = _mm_packus_epi16(_mm_packs_epi32(s, s), zero);
        let out = _mm_cvtsi128_si32(packed) as u32;
        pixel.copy_from_slice(&out.to_ne_bytes());
    }
}

#[cfg(not(target_arch = "x86_64"))]
unsafe fn horizontal_row_sse41(row: &[u16], columns: &[(usize, usize, u32)], dst_row: &mut [u8]) {
    horizontal_row_scalar(row, columns, dst_row)
}

/// Copies the capture into the top-left corner of the larger encoder frame; the rest stays black.
fn pad_argb32(src: &[u8], src_w: u32, src_h: u32, dst: &mut [u8], dst_w: u32, dst_h: u32) {
    let (src_stride, dst_stride) = (src_w as usize * 4, dst_w as usize * 4);
    let copy = src_stride.min(dst_stride);
    for (y, dst_row) in dst.chunks_exact_mut(dst_stride).enumerate() {
        if y < src_h.min(dst_h) as usize {
            dst_row[..copy].copy_from_slice(&src[y * src_stride..y * src_stride + copy]);
            dst_row[copy..].fill(0);
        } else {
            dst_row.fill(0);
        }
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    #[tokio::test]
    async fn closing_encoder_output_wakes_a_blocked_capture() {
        let budget = Arc::new(Semaphore::new(0));
        let (tx, rx) = mpsc::channel(1);
        let worker = tokio::task::spawn_blocking(move || acquire(&budget, &tx));
        tokio::task::yield_now().await;
        drop(rx);
        assert!(tokio::time::timeout(Duration::from_secs(1), worker).await.unwrap().unwrap().is_none());
    }

    #[tokio::test]
    async fn stream_slot_waits_for_the_previous_encoder_of_the_same_stream() {
        let (stream, other) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let held = stream_slot(&stream).await;
        // Another stream is independent.
        drop(tokio::time::timeout(Duration::from_secs(1), stream_slot(&other)).await.unwrap());
        assert!(tokio::time::timeout(Duration::from_millis(50), stream_slot(&stream)).await.is_err());
        drop(held);
        drop(tokio::time::timeout(Duration::from_secs(1), stream_slot(&stream)).await.unwrap());
    }
}

#[cfg(test)]
mod frame_tests {
    use super::*;
    fn image(w: u32, h: u32, pixel: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
        let pixel = &pixel;
        (0..h).flat_map(|y| (0..w).flat_map(move |x| pixel(x, y))).collect()
    }

    #[test]
    fn bilinear_scaling_keeps_flat_areas_and_averages_edges() {
        let flat = image(8, 6, |_, _| [10, 20, 30, 255]);
        let mut out = vec![0; 4 * 3 * 4];
        scale_bilinear_argb32(&flat, 8, 6, &mut out, 4, 3);
        assert!(out.chunks(4).all(|p| p == [10, 20, 30, 255]));
        // A 2:1 downscale of alternating black/white columns lands between the two, not on one of them.
        let stripes = image(4, 2, |x, _| if x % 2 == 0 { [0; 4] } else { [255; 4] });
        let mut out = vec![0; 2 * 1 * 4];
        scale_bilinear_argb32(&stripes, 4, 2, &mut out, 2, 1);
        assert!(out.iter().all(|&v| v == 128), "{out:?}");
        // Upscaling interpolates and never reads past the last row/column.
        let ramp = image(2, 1, |x, _| [(x * 200) as u8; 4]);
        let mut out = vec![0; 4 * 2 * 4];
        scale_bilinear_argb32(&ramp, 2, 1, &mut out, 4, 2);
        let row: Vec<u8> = out[..16].chunks(4).map(|p| p[0]).collect();
        assert_eq!(row, vec![0, 50, 150, 200]);
    }

    #[test]
    fn odd_capture_is_padded_with_black_not_stretched() {
        let src = image(3, 3, |x, y| [(x + 3 * y + 1) as u8; 4]);
        let mut out = vec![9; 4 * 4 * 4];
        pad_argb32(&src, 3, 3, &mut out, 4, 4);
        for y in 0..4usize {
            for x in 0..4usize {
                let expected = if x < 3 && y < 3 { (x + 3 * y + 1) as u8 } else { 0 };
                assert_eq!(out[(y * 4 + x) * 4], expected, "({x},{y})");
            }
        }
    }

    #[test]
    fn vertical_weight_256_does_not_wrap() {
        // 3 -> 1 rows lands on fy = 0, i.e. a top weight of 256; computing that weight in
        // u16 wraps to zero and would emit a black frame. The middle row is the tap target.
        let src = image(5, 3, |_, y| if y == 1 { [200; 4] } else { [100; 4] });
        let mut out = vec![0; 5 * 4];
        scale_bilinear_argb32(&src, 5, 3, &mut out, 5, 1);
        assert!(out.chunks(4).all(|p| p == [200; 4]), "{out:?}");
    }

    #[test]
    fn separable_scaling_matches_the_fused_form() {
        // The two-pass form must round exactly once like the fused expression it replaced.
        let src = image(7, 5, |x, y| [(x * 37 + y * 11) as u8; 4]);
        let mut out = vec![0; 4 * 3 * 4];
        scale_bilinear_argb32(&src, 7, 5, &mut out, 4, 3);
        for (d, &(x0, x1, fx)) in (0..4usize).zip(taps_for(4, 7)) {
            for (r, &(y0, y1, fy)) in (0..3usize).zip(taps_for(3, 5)) {
                for c in 0..4 {
                    let lerp = |row: usize| u32::from(src[row * 28 + x0 * 4 + c]) * (256 - fx) + u32::from(src[row * 28 + x1 * 4 + c]) * fx;
                    let expected = ((lerp(y0) * (256 - fy) + lerp(y1) * fy + 32768) >> 16) as u8;
                    assert_eq!(out[(r * 4 + d) * 4 + c], expected, "({d},{r},{c})");
                }
            }
        }
    }

    fn taps_for(dst_len: u32, src_len: u32) -> Vec<(usize, usize, u32)> {
        (0..dst_len)
            .map(|d| {
                let pos = ((2 * u64::from(d) + 1) * u64::from(src_len) * 128 / u64::from(dst_len)).saturating_sub(128);
                let first = ((pos >> 8) as usize).min(src_len as usize - 1);
                let second = (first + 1).min(src_len as usize - 1);
                (first, second, (pos & 255) as u32)
            })
            .collect()
    }
}
