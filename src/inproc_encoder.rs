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
    abr: u32,
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

    fn x264c_create(config: *const X264cConfigFFI, error: *mut libc::c_int) -> *mut X264cEngineOpaque;
    fn x264c_convert_bgrx_i420(src: *const u8, pitch: u32, y: *mut u8, u: *mut u8, v: *mut u8,
                               width: libc::c_int, height: libc::c_int);
    fn x264c_convert_row_pair(top: *const u8, bottom: *const u8, y_top: *mut u8, y_bottom: *mut u8,
                              u: *mut u8, v: *mut u8, width: libc::c_int);
    fn x264c_encode_i420(
        engine: *mut X264cEngineOpaque,
        y: *const u8,
        u: *const u8,
        v: *const u8,
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

#[derive(Clone)]
pub struct Config {
    pub display: String,
    pub input: (u32, u32),
    pub output: Option<(u32, u32)>,
    pub fps: u32,
    pub gop: u32,
    pub cq: u32,
    /// Viewer bitrate preset: the x264 engine uses ABR + VBV; 自动 keeps CRF with the same VBV bound.
    pub abr: bool,
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
    handle: &tokio::runtime::Handle,
) -> Option<OwnedSemaphorePermit> {
    handle.block_on(async {
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

/// x264c_create reports why it failed; the message carries the reason, never a library path.
fn x264c_error(code: libc::c_int) -> &'static str {
    match code {
        1 => "libx264.so could not be dlopen'ed",
        2 => "libx264 lacks the compile-time X264_BUILD symbols (x264.h / library mismatch)",
        3 => "x264 encoder setup failed",
        _ => "unknown failure",
    }
}

/// One produced I420 frame waiting for the encoder thread.
struct QueuedFrame {
    planes: Planes,
    force_idr: libc::c_int,
    permit: OwnedSemaphorePermit,
    /// Capture start → encoder finished, for the latency statistic (monotonic Instants only).
    captured_at: Instant,
    capture_us: u32,
    convert_us: u32,
}

fn elapsed_us(since: Instant) -> u32 { u32::try_from(since.elapsed().as_micros()).unwrap_or(u32::MAX) }

/// Depth-1 hand-off to the encoder: only the newest frame is kept, so a slow encoder never
/// builds a latency queue. A replaced frame's plane buffer goes back to the pool.
struct FrameQueue {
    state: std::sync::Mutex<(Option<QueuedFrame>, bool)>,
    cv: std::sync::Condvar,
}

impl FrameQueue {
    fn new() -> Self {
        Self { state: std::sync::Mutex::new((None, false)), cv: std::sync::Condvar::new() }
    }

    fn push(&self, frame: QueuedFrame) -> Option<QueuedFrame> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let old = state.0.replace(frame);
        drop(state);
        self.cv.notify_one();
        old
    }

    fn take(&self) -> Option<QueuedFrame> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(frame) = state.0.take() {
                return Some(frame);
            }
            if state.1 {
                return None;
            }
            state = self.cv.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.1 = true;
        drop(state);
        self.cv.notify_all();
    }
}

/// Double-buffered I420 planes shared by the capture and encoder threads. x264 copies the input
/// picture during encoder_encode, so the encoder returns its buffer right after encoding.
struct PlanePool {
    state: std::sync::Mutex<(Vec<Planes>, usize)>,
    cv: std::sync::Condvar,
    max: usize,
}

impl PlanePool {
    fn new(max: usize) -> Self {
        Self { state: std::sync::Mutex::new((Vec::new(), 0)), cv: std::sync::Condvar::new(), max }
    }

    /// Waits for a free plane buffer; None when `stop` is set while waiting.
    fn take(&self, w: u32, h: u32, stop: &AtomicBool) -> Option<Planes> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(planes) = state.0.pop() {
                return Some(planes);
            }
            if state.1 < self.max {
                state.1 += 1;
                drop(state);
                return Some(Planes::new(w, h));
            }
            if stop.load(Ordering::Relaxed) {
                return None;
            }
            state = self.cv.wait_timeout(state, Duration::from_millis(20)).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    fn give(&self, planes: Planes) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.0.push(planes);
        drop(state);
        self.cv.notify_one();
    }
}

/// In-process libx264 (H.264 only): XShm -> I420 on the CPU -> x264 in a dlopen'ed libx264.
/// Capture/scale/convert and encode run on two threads with a depth-1 hand-off, so their serial
/// per-frame work overlaps instead of adding up; the keepalive/budget/IDR policy is unchanged.
fn run_x264(
    config: Config,
    units: mpsc::Sender<EncodedUnit>,
    stop: Arc<AtomicBool>,
    idr: Arc<AtomicBool>,
    budget: Arc<Semaphore>,
) -> Result<(), String> {
    let (width0, height0) = config.output.unwrap_or(config.input);
    let (width, height) = (width0 + width0 % 2, height0 + height0 % 2);
    log::info!(
        "Initializing Native In-Process x264: {}x{}, {} fps, GOP {}, CQ {}",
        width,
        height,
        config.fps,
        config.gop,
        config.cq
    );
    let handle = tokio::runtime::Handle::current();
    let queue = Arc::new(FrameQueue::new());
    let pool = Arc::new(PlanePool::new(2));
    let producer = {
        let (config, queue, pool, units, stop, idr, budget, handle) = (
            config.clone(), queue.clone(), pool.clone(), units.clone(), stop.clone(),
            idr.clone(), budget.clone(), handle.clone(),
        );
        std::thread::Builder::new().name("x264-capture".into())
            .spawn(move || produce_x264(config, queue, pool, units, stop, idr, budget, handle, width, height))
            .map_err(|e| format!("x264 capture thread: {e}"))?
    };
    let encoder = {
        let (config, queue, pool, units, stop) = (config, queue.clone(), pool.clone(), units, stop.clone());
        std::thread::Builder::new().name("x264-encode".into())
            .spawn(move || encode_x264(config, queue, pool, units, stop, width, height))
            .map_err(|e| format!("x264 encoder thread: {e}"))?
    };
    let producer = producer.join().unwrap_or_else(|_| Err("x264 capture thread panicked".into()));
    if producer.is_err() {
        stop.store(true, Ordering::Relaxed);
        queue.close();
    }
    let encoder = encoder.join().unwrap_or_else(|_| Err("x264 encoder thread panicked".into()));
    encoder.and(producer)
}

fn produce_x264(
    config: Config, queue: Arc<FrameQueue>, pool: Arc<PlanePool>, units: mpsc::Sender<EncodedUnit>,
    stop: Arc<AtomicBool>, idr: Arc<AtomicBool>, budget: Arc<Semaphore>, handle: tokio::runtime::Handle,
    width: u32, height: u32,
) -> Result<(), String> {
    let result = produce_x264_inner(&config, &queue, &pool, &units, &stop, &idr, &budget, &handle, width, height);
    queue.close();
    result
}

#[allow(clippy::too_many_arguments)]
fn produce_x264_inner(
    config: &Config, queue: &FrameQueue, pool: &PlanePool, units: &mpsc::Sender<EncodedUnit>,
    stop: &AtomicBool, idr: &AtomicBool, budget: &Arc<Semaphore>, handle: &tokio::runtime::Handle,
    width: u32, height: u32,
) -> Result<(), String> {
    let mut capture = DamageCapture::open(&config.display)
        .map_err(|e| format!("DamageCapture::open failed on {}: {e}", config.display))?;
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
    let mut frame_buf = Vec::new();
    let locked = crate::session_lock::watch_locked();
    let mut last_locked = *locked.borrow();
    // An IDR request on a frame that gets superseded before encoding is forwarded to the next one.
    let mut pending_idr = false;

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let deadline = if dirty { next_tick } else { last_send + keepalive };
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
        let Some(permit) = acquire(budget, units, handle) else {
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
        let force_idr = if idr.swap(false, Ordering::AcqRel) || pending_idr {
            pending_idr = false;
            1
        } else {
            0
        };
        // Keepalive frames re-encode the current screen contents as a tiny P-frame.
        let Some(mut planes) = pool.take(width, height, stop) else {
            return Ok(());
        };
        let captured_at = Instant::now();
        let source = capture.capture()?;
        let capture_us = elapsed_us(captured_at);
        let convert_started = Instant::now();
        if cap_width == width && cap_height == height {
            unsafe {
                x264c_convert_bgrx_i420(source.as_ptr(), bytes_per_line as u32, planes.y.as_mut_ptr(),
                                        planes.u.as_mut_ptr(), planes.v.as_mut_ptr(),
                                        width as libc::c_int, height as libc::c_int);
            }
        } else if config.output.is_some() {
            scale_convert_bgrx_i420(source, cap_width, cap_height, &mut planes.y, &mut planes.u, &mut planes.v, width, height);
        } else {
            frame_buf.resize(width as usize * height as usize * 4, 0);
            pad_argb32(source, cap_width, cap_height, &mut frame_buf, width, height);
            unsafe {
                x264c_convert_bgrx_i420(frame_buf.as_ptr(), width * 4, planes.y.as_mut_ptr(),
                                        planes.u.as_mut_ptr(), planes.v.as_mut_ptr(),
                                        width as libc::c_int, height as libc::c_int);
            }
        }
        let convert_us = elapsed_us(convert_started);

        if let Some(old) = queue.push(QueuedFrame { planes, force_idr, permit, captured_at, capture_us, convert_us }) {
            if old.force_idr != 0 {
                pending_idr = true;
            }
            pool.give(old.planes);
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

fn encode_x264(
    config: Config, queue: Arc<FrameQueue>, pool: Arc<PlanePool>, units: mpsc::Sender<EncodedUnit>,
    stop: Arc<AtomicBool>, width: u32, height: u32,
) -> Result<(), String> {
    let result = encode_x264_inner(&config, &queue, &pool, &units, &stop, width, height);
    queue.close();
    result
}

fn encode_x264_inner(
    config: &Config, queue: &FrameQueue, pool: &PlanePool, units: &mpsc::Sender<EncodedUnit>,
    stop: &AtomicBool, width: u32, height: u32,
) -> Result<(), String> {
    let ffi_cfg = X264cConfigFFI {
        width,
        height,
        fps: config.fps,
        gop: config.gop,
        cq: config.cq,
        abr: u32::from(config.abr),
        target_bps: config.target_bps,
        max_bps: config.max_bps,
        buffer_bits: config.buffer_bits,
    };
    let mut reason = 0;
    let raw_engine = unsafe { x264c_create(&ffi_cfg, &mut reason) };
    if raw_engine.is_null() {
        return Err(format!("Failed to create native x264 engine: {}", x264c_error(reason)));
    }
    let engine = SafeX264Engine(raw_engine);
    // Stage means and the end-to-end latency median over one statistic window (2 s at 60 fps).
    const STATS_FRAMES: u64 = 120;
    let (mut frames, mut capture_sum, mut convert_sum, mut encode_sum) = (0u64, 0u64, 0u64, 0u64);
    let mut latencies: Vec<u32> = Vec::with_capacity(STATS_FRAMES as usize);
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let Some(frame) = queue.take() else {
            return Ok(());
        };
        let mut out_data: *const u8 = std::ptr::null();
        let mut out_len: u32 = 0;
        let encode_started = Instant::now();
        let res = unsafe {
            x264c_encode_i420(engine.0, frame.planes.y.as_ptr(), frame.planes.u.as_ptr(),
                              frame.planes.v.as_ptr(), frame.force_idr, &mut out_data, &mut out_len)
        };
        let encode_us = elapsed_us(encode_started);
        // x264 copied the input picture during encoder_encode; hand the buffer back immediately.
        pool.give(frame.planes);
        if res != 0 {
            return Err(format!("x264c_encode_i420 failed with error {res}"));
        }
        if out_len > 0 && !out_data.is_null() {
            let data = unsafe { std::slice::from_raw_parts(out_data, out_len as usize) }.to_vec();
            let completed_at = Instant::now();
            frames += 1;
            capture_sum += u64::from(frame.capture_us);
            convert_sum += u64::from(frame.convert_us);
            encode_sum += u64::from(encode_us);
            latencies.push(elapsed_us(frame.captured_at));
            if frames % STATS_FRAMES == 0 {
                latencies.sort_unstable();
                let ms = |total: u64| total as f64 / frames as f64 / 1000.0;
                log::info!(
                    "x264 pipeline stages: frames={} capture={:.2}ms convert={:.2}ms encode={:.2}ms latency_median={:.2}ms",
                    STATS_FRAMES,
                    ms(capture_sum),
                    ms(convert_sum),
                    ms(encode_sum),
                    f64::from(latencies[latencies.len() / 2]) / 1000.0
                );
                capture_sum = 0;
                convert_sum = 0;
                encode_sum = 0;
                latencies.clear();
            }
            if units
                .blocking_send(EncodedUnit { data, permit: frame.permit, completed_at })
                .is_err()
            {
                return Ok(());
            }
        }
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
    let handle = tokio::runtime::Handle::current();

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

        let Some(permit) = acquire(&budget, &units, &handle) else {
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
    let handle = tokio::runtime::Handle::current();

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

        let Some(permit) = acquire(&budget, &units, &handle) else {
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

/// One I420 frame owned by the encoder pipeline. x264 copies the input picture during
/// encoder_encode, so a plane buffer is reusable as soon as x264c_encode_i420 returns.
struct Planes { y: Vec<u8>, u: Vec<u8>, v: Vec<u8> }
impl Planes {
    fn new(w: u32, h: u32) -> Self {
        let (cw, ch) = (w as usize / 2, h as usize / 2);
        Self { y: vec![0; w as usize * h as usize], u: vec![0; cw * ch], v: vec![0; cw * ch] }
    }
}

/// Source position in 1/256 pixel units: (d + 0.5) * src / dst - 0.5, clamped to the image.
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

/// Bilinear ARGB32 scaling (the former swscale path used SWS_BILINEAR), with 8-bit fixed-point
/// weights and pixel centers aligned. Separable two-pass form: the vertical pass writes an
/// unrounded 16-bit intermediate (<= 255*256) with a row-uniform weight, the horizontal pass
/// rounds once at the end — bit-identical to the fused scalar form. On x86-64 with SSE4.1 the
/// horizontal pass runs one pixel (4 channels) per __m128i. The in-process x264 path uses the
/// fused scale+convert instead; this stays for the NVENC copy path and as the fused path test
/// reference.
fn scale_bilinear_argb32(src: &[u8], src_w: u32, src_h: u32, dst: &mut [u8], dst_w: u32, dst_h: u32) {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return;
    }
    let (columns, rows) = (taps_for(dst_w, src_w), taps_for(dst_h, src_h));
    let src_stride = src_w as usize * 4;
    let mut mid = vec![0u16; src_stride];
    for (r, dst_row) in dst.chunks_exact_mut(dst_w as usize * 4).enumerate() {
        scaled_row(src, src_stride, &rows, r, &columns, &mut mid, dst_row);
    }
}

/// One destination BGRX row of the separable scaler: vertical lerp into a u16 row, then the
/// SSE4.1/scalar horizontal pass. Weights stay in u32 in the vertical pass: a fy of 0 makes the
/// top weight 256, which must not wrap in u16 before the single end rounding.
fn scaled_row(src: &[u8], src_stride: usize, rows: &[(usize, usize, u32)], r: usize,
              columns: &[(usize, usize, u32)], mid: &mut [u16], dst_row: &mut [u8]) {
    let (y0, y1, fy) = rows[r];
    let (top, bottom) = (&src[y0 * src_stride..], &src[y1 * src_stride..]);
    let (a, b) = (256 - fy, fy);
    if avx2_available() {
        // Safety: avx2_available() checked the CPU feature; slices are live and equally long.
        unsafe { vertical_row_avx2(top, bottom, a as u16, b as u16, mid) };
    } else {
        for (m, (t, bt)) in mid.iter_mut().zip(top.iter().zip(bottom)) {
            *m = (u32::from(*t) * a + u32::from(*bt) * b) as u16;
        }
    }
    if sse41_available() {
        // Safety: sse41_available() checked the CPU feature; rows come from live slices.
        unsafe { horizontal_row_sse41(mid, columns, dst_row) };
    } else {
        horizontal_row_scalar(mid, columns, dst_row);
    }
}

/// Scale and convert to I420 two destination rows at a time: the scaled rows live in two row
/// buffers and go straight into Y/U/V, so a scaled BGRX frame is never materialised.
fn scale_convert_bgrx_i420(src: &[u8], src_w: u32, src_h: u32, y: &mut [u8], u: &mut [u8], v: &mut [u8],
                           dst_w: u32, dst_h: u32) {
    debug_assert!(dst_w % 2 == 0 && dst_h % 2 == 0);
    let src_stride = src_w as usize * 4;
    let dst_stride = dst_w as usize * 4;
    let (columns, rows) = (taps_for(dst_w, src_w), taps_for(dst_h, src_h));
    let mut mid = vec![0u16; src_stride];
    let mut top = vec![0u8; dst_stride];
    let mut bottom = vec![0u8; dst_stride];
    let (dw, cw) = (dst_w as usize, dst_w as usize / 2);
    for pair in 0..dst_h as usize / 2 {
        scaled_row(src, src_stride, &rows, pair * 2, &columns, &mut mid, &mut top);
        scaled_row(src, src_stride, &rows, pair * 2 + 1, &columns, &mut mid, &mut bottom);
        let (y_row, c_row) = (pair * 2 * dw, pair * cw);
        unsafe {
            x264c_convert_row_pair(top.as_ptr(), bottom.as_ptr(), y[y_row..].as_mut_ptr(),
                                   y[y_row + dw..].as_mut_ptr(), u[c_row..].as_mut_ptr(),
                                   v[c_row..].as_mut_ptr(), dst_w as libc::c_int);
        }
    }
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

/// Vertical pass of the scaler: 16 pixels per AVX2 iteration (u16 arithmetic cannot overflow:
/// top*a + bottom*b <= 255*256). Scalar tail and scalar fallback keep the same integers.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn vertical_row_avx2(top: &[u8], bottom: &[u8], a: u16, b: u16, mid: &mut [u16]) {
    use std::arch::x86_64::*;
    let (va, vb) = (_mm256_set1_epi16(a as i16), _mm256_set1_epi16(b as i16));
    let mut i = 0;
    while i + 16 <= mid.len() {
        let t = _mm256_cvtepu8_epi16(_mm_loadu_si128(top.as_ptr().add(i) as *const __m128i));
        let u = _mm256_cvtepu8_epi16(_mm_loadu_si128(bottom.as_ptr().add(i) as *const __m128i));
        let m = _mm256_add_epi16(_mm256_mullo_epi16(t, va), _mm256_mullo_epi16(u, vb));
        _mm256_storeu_si256(mid.as_mut_ptr().add(i) as *mut __m256i, m);
        i += 16;
    }
    for (m, (t, u)) in mid[i..].iter_mut().zip(top[i..].iter().zip(bottom[i..].iter())) {
        *m = (u32::from(*t) * u32::from(a) + u32::from(*u) * u32::from(b)) as u16;
    }
}

fn avx2_available() -> bool {
    static AVX2: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    #[cfg(target_arch = "x86_64")]
    { *AVX2.get_or_init(|| std::arch::is_x86_feature_detected!("avx2")) }
    #[cfg(not(target_arch = "x86_64"))]
    { let _ = &AVX2; false }
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
        let worker = tokio::task::spawn_blocking(move || acquire(&budget, &tx, &tokio::runtime::Handle::current()));
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
        for (d, (x0, x1, fx)) in (0..4usize).zip(taps_for(4, 7)) {
            for (r, (y0, y1, fy)) in (0..3usize).zip(taps_for(3, 5)) {
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

#[cfg(test)]
mod convert_tests {
    use super::*;

    extern "C" {
        fn x264c_test_convert(src: *const u8, pitch: u32, y: *mut u8, u: *mut u8, v: *mut u8,
                              w: libc::c_int, h: libc::c_int, force_scalar: libc::c_int) -> libc::c_int;
    }

    fn pixels(w: u32, h: u32, seed: u32) -> Vec<u8> {
        let mut s = seed | 1;
        let mut v = vec![0u8; w as usize * h as usize * 4];
        for p in v.chunks_exact_mut(4) {
            s ^= s << 13; s ^= s >> 17; s ^= s << 5;
            p.copy_from_slice(&[s as u8, (s >> 8) as u8, (s >> 16) as u8, 0xFF]);
        }
        v
    }

    /// (Y, U, V, whether the AVX2 path ran)
    fn convert(src: &[u8], pitch: u32, w: u32, h: u32, force_scalar: bool) -> (Vec<u8>, Vec<u8>, Vec<u8>, bool) {
        let (mut y, mut u, mut v) = (
            vec![0u8; (w * h) as usize],
            vec![0u8; (w / 2 * (h / 2)) as usize],
            vec![0u8; (w / 2 * (h / 2)) as usize],
        );
        let used = unsafe {
            x264c_test_convert(src.as_ptr(), pitch, y.as_mut_ptr(), u.as_mut_ptr(), v.as_mut_ptr(),
                               w as libc::c_int, h as libc::c_int, libc::c_int::from(force_scalar))
        };
        (y, u, v, used == 1)
    }

    #[test]
    fn avx2_bgrx_to_i420_matches_scalar_byte_for_byte() {
        // Widths that are not multiples of 16/32 exercise the vector tail; the heights give an
        // odd number of row pairs, and 1920x1200 is the production size.
        for (w, h) in [(1922u32, 10u32), (2560, 4), (1366, 6), (1920, 1200)] {
            let src = pixels(w, h, 0x1234_5678 ^ w);
            let (sy, su, sv, scalar_used) = convert(&src, w * 4, w, h, true);
            assert!(!scalar_used, "force_scalar must run the scalar path");
            let (ay, au, av, avx2_used) = convert(&src, w * 4, w, h, false);
            if !avx2_used {
                eprintln!("AVX2 not available; scalar output only for {w}x{h}");
                continue;
            }
            assert!(sy == ay, "Y differs at {w}x{h}");
            assert!(su == au, "U differs at {w}x{h}");
            assert!(sv == av, "V differs at {w}x{h}");
        }
    }

    #[test]
    fn fused_scale_convert_matches_scale_then_convert_byte_for_byte() {
        // Production size, a source whose width is not a multiple of the vector width, and an
        // odd number of destination row pairs.
        for (sw, sh, dw, dh) in [(2560u32, 1600u32, 1920u32, 1200u32), (1922, 1000, 1366, 710), (1920, 1200, 1280, 800)] {
            let src = pixels(sw, sh, 0x9E37_79B9 ^ sw);
            let mut scaled = vec![0u8; (dw * dh * 4) as usize];
            scale_bilinear_argb32(&src, sw, sh, &mut scaled, dw, dh);
            let (sy, su, sv, _) = convert(&scaled, dw * 4, dw, dh, true);
            let mut planes = Planes::new(dw, dh);
            scale_convert_bgrx_i420(&src, sw, sh, &mut planes.y, &mut planes.u, &mut planes.v, dw, dh);
            assert!(sy == planes.y, "Y differs at {sw}x{sh} -> {dw}x{dh}");
            assert!(su == planes.u, "U differs at {sw}x{sh} -> {dw}x{dh}");
            assert!(sv == planes.v, "V differs at {sw}x{sh} -> {dw}x{dh}");
        }
    }
}
