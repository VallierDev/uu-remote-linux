//! Direct In-process X11 capture (XDamage + XShm) and Hardware NVENC encoding.
//! Zero-Copy Mode: Directly maps XShm buffer to GPU address space via CUDA Pinned Memory,
//! eliminating all CPU memcpy overhead.

use std::collections::VecDeque;
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
    is_hevc: libc::c_int,
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
    pub encoder: crate::video::Encoder,
    pub codec: crate::capture_setting::VideoCodec,
    /// Capture on XDamage changes (XShm) instead of continuous poll.
    pub damage: bool,
}

pub fn disabled() -> bool {
    std::env::var("UU_VIDEO_PIPELINE").is_ok_and(|v| v == "process")
}

pub fn damage_enabled() -> bool {
    std::env::var("UU_CAPTURE").map_or(true, |v| v != "grab")
}

pub fn selected(encoder: crate::video::Encoder) -> bool {
    encoder == crate::video::Encoder::Nvenc && !disabled() && crate::config::ffmpeg_root().is_none()
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
    let (width, height) = config.output.unwrap_or(config.input);
    let (width, height) = (width + width % 2, height + height % 2);
    let is_hevc = if config.codec == crate::capture_setting::VideoCodec::H265 {
        1
    } else {
        0
    };

    log::info!(
        "Initializing Native In-Process NVENC: {}x{}, {} fps, GOP {}, CQ {}, HEVC={}",
        width,
        height,
        config.fps,
        config.gop,
        config.cq,
        is_hevc == 1
    );

    let ffi_cfg = NvencConfigFFI {
        width,
        height,
        fps: config.fps,
        gop: config.gop,
        cq: config.cq,
        target_bps: config.target_bps,
        max_bps: config.max_bps,
        is_hevc,
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
            log::info!("Native NVENC Zero-Copy Pinned Host DMA activated: 0 bytes CPU memcpy!");
        } else {
            log::warn!("nvenc_register_host_buffer returned {res}, falling back to cuMemcpyHtoD");
        }
    } else {
        log::warn!(
            "Capture size ({}x{}) != encoder size ({}x{}), Zero-Copy disabled",
            cap_width,
            cap_height,
            width,
            height
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
            // Fallback mode: capture slice and scale if requested resolution differs
            let source = capture.capture()?;
            let (target_ptr, pitch) = if cap_width == width && cap_height == height {
                (source.as_ptr(), bytes_per_line as u32)
            } else {
                if scaled_buf.len() != (width * height * 4) as usize {
                    scaled_buf.resize((width * height * 4) as usize, 0);
                }
                scale_nearest_argb32(
                    source,
                    cap_width,
                    cap_height,
                    &mut scaled_buf,
                    width,
                    height,
                );
                (scaled_buf.as_ptr(), width * 4)
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

fn scale_nearest_argb32(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    dst: &mut [u8],
    dst_w: u32,
    dst_h: u32,
) {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return;
    }
    let src_stride = src_w as usize * 4;
    let dst_stride = dst_w as usize * 4;
    for dy in 0..dst_h {
        let sy = ((dy as u64 * src_h as u64) / dst_h as u64) as usize;
        let src_row = &src[sy * src_stride..];
        let dst_row = &mut dst[dy as usize * dst_stride..(dy as usize + 1) * dst_stride];
        for dx in 0..dst_w {
            let sx = ((dx as u64 * src_w as u64) / dst_w as u64) as usize;
            let src_px = &src_row[sx * 4..sx * 4 + 4];
            let dst_px = &mut dst_row[dx as usize * 4..dx as usize * 4 + 4];
            dst_px.copy_from_slice(src_px);
        }
    }
}
