//! In-process X11 capture (XDamage + XShm) and NVENC encoding through the NVIDIA driver.
//! When capture and encoder sizes match, the XShm buffer is registered as CUDA pinned memory and
//! NVENC reads it by DMA, so there is no host-to-device copy (the X server still writes XShm).

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
    buffer_bits: i64,
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
    pub codec: crate::capture_setting::VideoCodec,
}

pub fn disabled() -> bool {
    std::env::var("UU_VIDEO_PIPELINE").is_ok_and(|v| v == "process")
}

pub fn damage_enabled() -> bool {
    std::env::var("UU_CAPTURE").map_or(true, |v| v != "grab")
}

pub fn selected(encoder: crate::video::Encoder) -> bool {
    encoder == crate::video::Encoder::Nvenc && damage_enabled() && !disabled() && crate::config::ffmpeg_root().is_none()
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
        buffer_bits: config.buffer_bits,
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
/// weights and pixel centers aligned.
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
}
