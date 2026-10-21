//! Direct In-process X11 capture and Hardware NVENC / VAAPI encoding.
//! Zero-Copy Mode:
//! - NVENC: Direct CUDA Pinned Host Memory mapping (Zero-Copy)
//! - VAAPI: Direct DRM_PRIME_2 / DRI3 DMA-BUF import + Hardware VPP (Zero CPU-Copy)

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
    is_hevc: libc::c_int,
    device: *const libc::c_char,
}

#[repr(C)]
struct Dri3CaptureOpaque {
    _private: [u8; 0],
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
    pub chroma: u64,
}

pub fn disabled() -> bool {
    std::env::var("UU_VIDEO_PIPELINE").is_ok_and(|v| v == "process")
}

pub fn damage_enabled() -> bool {
    std::env::var("UU_CAPTURE").map_or(true, |v| v != "grab")
}

pub fn selected(encoder: crate::video::Encoder) -> bool {
    (encoder == crate::video::Encoder::Nvenc || encoder == crate::video::Encoder::Vaapi)
        && damage_enabled()
        && !disabled()
        && crate::config::ffmpeg_root().is_none()
}

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
    *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
    Duration::from_millis(500 + u64::from(*seed >> 24) % 1500)
}

pub fn run(
    config: Config,
    units: mpsc::Sender<EncodedUnit>,
    stop: Arc<AtomicBool>,
    idr: Arc<AtomicBool>,
    budget: Arc<Semaphore>,
) -> Result<(), String> {
    let (width, height) = config.output.unwrap_or(config.input);
    let (width, height) = (width + width % 2, height + height % 2);
    let is_hevc = if config.codec == crate::capture_setting::VideoCodec::H265 { 1 } else { 0 };

    if std::env::var("UU_VIDEO_ENCODER").map_or(false, |v| v == "vaapi") {
        run_vaapi(config, units, stop, idr, budget, is_hevc)
    } else {
        run_nvenc(config, units, stop, idr, budget, width, height, is_hevc)
    }
}

fn run_vaapi(
    config: Config,
    units: mpsc::Sender<EncodedUnit>,
    stop: Arc<AtomicBool>,
    idr: Arc<AtomicBool>,
    budget: Arc<Semaphore>,
    is_hevc: libc::c_int,
) -> Result<(), String> {
    let (out_w, out_h) = config.output.unwrap_or(config.input);
    let (out_w, out_h) = (out_w + out_w % 2, out_h + out_h % 2);
    let (cap_w, cap_h) = config.input;
    let c_device = CString::new(crate::video::vaapi_device()).unwrap_or_default();
    let ffi_cfg = VaapiConfigFFI {
        width: out_w,
        height: out_h,
        cap_width: cap_w,
        cap_height: cap_h,
        fps: config.fps,
        gop: config.gop,
        target_bps: config.target_bps,
        is_hevc,
        device: c_device.as_ptr(),
    };

    let c_display = CString::new(config.display.as_str()).unwrap_or_default();

    // Capture stays at the full input size; VPP scales to the encode size
    let raw_dri3 = unsafe { dri3_capture_create(c_display.as_ptr(), cap_w, cap_h) };
    let mut dri3_cap = if !raw_dri3.is_null() {
        Some(SafeDri3Capture(raw_dri3))
    } else {
        None
    };

    let raw_engine = unsafe { vaapi_create(&ffi_cfg) };
    if raw_engine.is_null() {
        return Err("Failed to create native VAAPI engine".into());
    }
    let engine = SafeVaapiEngine(raw_engine);

    let mut fallback_capture = None;
    let mut zero_copy = false;

    if let Some(ref dri3) = dri3_cap {
        let dma_fd = unsafe { dri3_capture_get_dma_fd(dri3.0) };
        let stride = unsafe { dri3_capture_get_stride(dri3.0) };
        if dma_fd >= 0 && stride > 0 {
            let res = unsafe { vaapi_import_dma_buffer(engine.0, dma_fd, stride) };
            if res == 0 {
                zero_copy = true;
                log::info!("Native VAAPI True Zero CPU-Copy Pipeline Activated (DRI3 DMA-BUF -> DRM_PRIME_2 -> VPP -> VCN)!");
            }
        }
    }

    if !zero_copy {
        log::warn!("DRI3 DMA-BUF not available on display {}, falling back to XShm CPU upload", config.display);
        dri3_cap = None;
        fallback_capture = Some(
            DamageCapture::open(&config.display)
                .map_err(|e| format!("DamageCapture::open failed on {}: {e}", config.display))?
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
        let wait = deadline.saturating_duration_since(Instant::now());

        if zero_copy {
            let dri3 = dri3_cap.as_ref().unwrap();
            let timeout_ms = wait.as_millis().clamp(1, 1000) as i32;
            if unsafe { dri3_capture_wait_damage(dri3.0, timeout_ms) } > 0 {
                dirty = true;
            }
        } else {
            let cap = fallback_capture.as_mut().unwrap();
            if cap.wait(wait.max(Duration::from_millis(1)))? {
                dirty = true;
            }
        }

        let now = Instant::now();
        if dirty && now < next_tick {
            continue;
        }

        let current_locked = *locked.borrow();
        let force_idr = idr.swap(false, Ordering::Relaxed) || (current_locked != last_locked);
        last_locked = current_locked;

        let permit = match acquire(&budget, &units) {
            Some(p) => p,
            None => return Ok(()),
        };

        let mut out_data: *const u8 = std::ptr::null();
        let mut out_len: u32 = 0;

        if zero_copy {
            let dri3 = dri3_cap.as_ref().unwrap();
            unsafe {
                dri3_capture_blit(dri3.0);
                let res = vaapi_encode_frame(engine.0, if force_idr { 1 } else { 0 }, &mut out_data, &mut out_len);
                if res != 0 {
                    return Err(format!("vaapi_encode_frame failed: {res}"));
                }
            }
        } else {
            let cap = fallback_capture.as_mut().unwrap();
            let (cap_w, _) = cap.size();
            let pitch = cap_w as u32 * 4;
            // Without refresh the XShm segment stays at its initial contents and every frame is black.
            cap.refresh()?;
            unsafe {
                let upload = vaapi_upload_host_buffer(engine.0, cap.shm_raw_ptr(), pitch);
                if upload != 0 {
                    return Err(format!("vaapi_upload_host_buffer failed: {upload}"));
                }
                let res = vaapi_encode_frame(engine.0, if force_idr { 1 } else { 0 }, &mut out_data, &mut out_len);
                if res != 0 {
                    return Err(format!("vaapi_encode_frame failed: {res}"));
                }
            }
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

fn run_nvenc(
    config: Config,
    units: mpsc::Sender<EncodedUnit>,
    stop: Arc<AtomicBool>,
    idr: Arc<AtomicBool>,
    budget: Arc<Semaphore>,
    width: u32,
    height: u32,
    is_hevc: libc::c_int,
) -> Result<(), String> {
    let chroma_444 = libc::c_int::from(config.chroma == crate::capture_setting::CHROMA_444);
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
    let mut scaled_buf = Vec::new();

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

        let current_locked = *locked.borrow();
        let force_idr = idr.swap(false, Ordering::Relaxed) || (current_locked != last_locked);
        last_locked = current_locked;

        let permit = match acquire(&budget, &units) {
            Some(p) => p,
            None => return Ok(()),
        };

        let mut out_data: *const u8 = std::ptr::null();
        let mut out_len: u32 = 0;

        // Zero-copy or not, NVENC reads the XShm segment: pull the damaged pixels in first
        // (#74 had this in the registered-buffer branch; the refactor dropped it).
        capture.refresh()?;

        let res = if zero_copy {
            unsafe {
                nvenc_encode_frame(
                    engine.0,
                    std::ptr::null(),
                    0,
                    if force_idr { 1 } else { 0 },
                    &mut out_data,
                    &mut out_len,
                )
            }
        } else {
            let (target_ptr, pitch) = if cap_width == width && cap_height == height {
                (capture.shm_raw_ptr() as *const u8, bytes_per_line as u32)
            } else {
                scaled_buf.resize(width as usize * height as usize * 4, 0);
                if config.output.is_some() {
                    scale_bilinear_argb32(
                        unsafe { std::slice::from_raw_parts(capture.shm_raw_ptr(), capture.shm_size()) },
                        cap_width,
                        cap_height,
                        &mut scaled_buf,
                        width,
                        height,
                    );
                } else {
                    pad_argb32(
                        unsafe { std::slice::from_raw_parts(capture.shm_raw_ptr(), capture.shm_size()) },
                        cap_width,
                        cap_height,
                        &mut scaled_buf,
                        width,
                        height,
                    );
                }
                (scaled_buf.as_ptr(), width * 4)
            };
            unsafe {
                nvenc_encode_frame(
                    engine.0,
                    target_ptr,
                    pitch,
                    if force_idr { 1 } else { 0 },
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

fn scale_bilinear_argb32(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    dst: &mut [u8],
    dst_w: u32,
    dst_h: u32,
) {
    let taps = |dst_count: u32, src_count: u32| -> Vec<(usize, usize, u32)> {
        let step = (u64::from(src_count) << 16) / u64::from(dst_count);
        (0..dst_count)
            .map(|d| {
                let center = ((u64::from(d) * step + (step >> 1)).saturating_sub(1 << 15)) as u32;
                let (pos, frac) = ((center >> 16) as usize, center & 0xffff);
                let first = pos.min(src_count as usize - 1);
                let second = (pos + 1).min(src_count as usize - 1);
                (first, second, frac >> 8)
            })
            .collect()
    };
    let (columns, rows) = (taps(dst_w, src_w), taps(dst_h, src_h));
    let src_stride = src_w as usize * 4;
    for (dst_row, &(y0, y1, fy)) in dst.chunks_exact_mut(dst_w as usize * 4).zip(&rows) {
        let (top, bottom) = (&src[y0 * src_stride..], &src[y1 * src_stride..]);
        for (pixel, &(x0, x1, fx)) in dst_row.chunks_exact_mut(4).zip(&columns) {
            for c in 0..4 {
                let lerp = |row: &[u8]| u32::from(row[x0 * 4 + c]) * (256 - fx) + u32::from(row[x1 * 4 + c]) * fx;
                pixel[c] = ((lerp(top) * (256 - fy) + lerp(bottom) * fy + 32768) >> 16) as u8;
            }
        }
    }
}

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
