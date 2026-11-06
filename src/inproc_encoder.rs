//! In-process X11 capture (XDamage + XShm) and NVENC encoding through the NVIDIA driver.
//! With UU_VIDEO_ENCODER=vaapi, H.264 / HEVC go through VAAPI instead: a DRI3 DMA-BUF of the root window is
//! imported as a VA surface and converted/scaled by VPP, so the CPU never touches the pixels (XShm upload
//! when DRI3 is unavailable).
//! Portal sessions can instead upload their existing BGRA frames, sharing the same GPU VPP and encoder.
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
    #[cfg(all(target_os = "linux", feature = "portal"))]
    fn nvenc_encode_gpu(engine: *mut NvencEngineOpaque,
        copy: unsafe extern "C" fn(*mut libc::c_void, u64, u32) -> i32,
        opaque: *mut libc::c_void, force_idr: i32, data: *mut *const u8,
        length: *mut u32, source_complete: *mut i32) -> i32;
    #[cfg(all(target_os = "linux", feature = "portal"))]
    fn nvenc_check_device(engine: *mut NvencEngineOpaque, expected_pci: *const libc::c_char) -> i32;
    #[cfg(all(target_os = "linux", feature = "portal"))]
    fn nvenc_encode_egl(engine: *mut NvencEngineOpaque, image: *mut libc::c_void,
        expected_pci: *const libc::c_char, force_idr: i32, data: *mut *const u8,
        length: *mut u32, source_complete: *mut i32) -> i32;
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

#[cfg(all(target_os = "linux", feature = "portal"))]
impl SafeNvencEngine {
    /// Caller has completed the producer acquire wait and holds the exclusive
    /// lease. Device/modifier provenance must be independently validated. Unknown
    /// retains image resources and requires retaining the backend and lease too.
    unsafe fn encode_dmabuf(&mut self, device: &crate::portal_gpu::Device,
        frame: &crate::portal::DmaFrame, force_idr: bool, cancelled: &dyn Fn() -> bool,
    ) -> crate::portal::GpuFrameResult {
        use crate::portal::GpuFrameResult::{Complete, Unknown};
        if cancelled() { return Complete(Err(-libc::ECANCELED)); }
        let image = match unsafe { device.import(frame) } {
            Ok(image) => image,
            Err(code) => return Unknown(code),
        };
        if cancelled() {
            return match unsafe { image.destroy_completed() } {
                Ok(()) => Complete(Err(-libc::ECANCELED)),
                Err(code) => Unknown(code),
            };
        }
        let encoded = unsafe { self.encode_image(&image, force_idr) };
        match encoded {
            Unknown(code) => Unknown(code), // Image Drop retains fd and display.
            Complete(packet) => match unsafe { image.destroy_completed() } {
                Err(code) => Unknown(code),
                Ok(()) if cancelled() => Complete(Err(-libc::ECANCELED)),
                Ok(()) => Complete(packet),
            },
        }
    }
    /// Acquire must already be complete. Keep Image and its Portal lease retained
    /// on Unknown; successful encode does not itself destroy the EGL image.
    unsafe fn encode_image(&mut self, image: &crate::portal_gpu::Image, force_idr: bool) -> crate::portal::GpuFrameResult {
        unsafe { self.encode_egl(image.handle(), image.pci(), force_idr) }
    }
    /// Borrowed EGL image and its backing lease must remain alive through this
    /// call and be retained on Unknown. Caller proves acquire completion and
    /// obtains expected_pci independently from the DMA-BUF producer device.
    unsafe fn encode_egl(&mut self, image: *mut libc::c_void,
        expected_pci: &std::ffi::CStr, force_idr: bool,
    ) -> crate::portal::GpuFrameResult {
        let mut data = std::ptr::null(); let mut length = 0; let mut complete = 0;
        let result = unsafe { nvenc_encode_egl(self.0, image, expected_pci.as_ptr(),
            i32::from(force_idr), &mut data, &mut length, &mut complete) };
        if result != 0 || complete != 1 || data.is_null() || length == 0 {
            return crate::portal::GpuFrameResult::Unknown(if result != 0 { result } else { -libc::EIO });
        }
        crate::portal::GpuFrameResult::Complete(Ok(unsafe {
            std::slice::from_raw_parts(data, length as usize).to_vec()
        }))
    }
    /// The caller establishes device/import compatibility and acquire completion.
    /// copy runs in the encoder context and must retain imports and any GPU guards
    /// on errors/panic. The closure must not own GPU guards that drop on return.
    /// All access must be submitted on this context; no reentry.
    /// Any native failure is Unknown, even if the source copy completed: the
    /// encoder can still own mapped or locked resources. Retire this worker.
    unsafe fn encode_gpu<F>(&mut self, force_idr: bool, mut copy: F) -> crate::portal::GpuFrameResult
    where F: FnMut(u64, u32) -> Result<(), i32> {
        unsafe extern "C" fn invoke<F: FnMut(u64, u32) -> Result<(), i32>>(
            opaque: *mut libc::c_void, dst: u64, pitch: u32,
        ) -> i32 {
            let copy = unsafe { &mut *opaque.cast::<F>() };
            // Rust unwinding must never cross the driver/C boundary.
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| copy(dst, pitch))) {
                Ok(Ok(())) => 0,
                Ok(Err(code)) if code != 0 => code,
                _ => -libc::EIO,
            }
        }
        let mut data = std::ptr::null(); let mut length = 0; let mut source_complete = 0;
        let result = unsafe { nvenc_encode_gpu(self.0, invoke::<F>, (&mut copy as *mut F).cast(),
            i32::from(force_idr), &mut data, &mut length, &mut source_complete) };
        if result != 0 || source_complete != 1 || data.is_null() || length == 0 {
            return crate::portal::GpuFrameResult::Unknown(if result != 0 { result } else { -libc::EIO });
        }
        // Own the compressed bytes before the next encode reuses C staging.
        crate::portal::GpuFrameResult::Complete(Ok(unsafe {
            std::slice::from_raw_parts(data, length as usize).to_vec()
        }))
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
    pub portal: Option<Arc<crate::portal::PortalSession>>,
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

// Unknown GPU completion retains native owners for the process lifetime. A
// later settings restart/reconnection must not silently start Portal capture.
#[cfg(all(target_os = "linux", feature = "portal"))]
struct PortalRetryGate(std::sync::atomic::AtomicI32);
#[cfg(all(target_os = "linux", feature = "portal"))]
impl PortalRetryGate {
    const fn new()->Self {Self(std::sync::atomic::AtomicI32::new(0))}
    fn block(&self,code:i32) {
        let code=if code==0 {-libc::EIO} else {code};
        let _=self.0.compare_exchange(0,code,Ordering::AcqRel,Ordering::Acquire);
    }
    fn check(&self)->Result<(),crate::video::EncoderError> {
        let code=self.0.load(Ordering::Acquire);
        if code==0 {Ok(())} else {
            Err(crate::video::EncoderError::Terminal(format!("Portal GPU state retained; retry disabled: code={code}")))
        }
    }
}
#[cfg(all(target_os = "linux", feature = "portal"))]
static PORTAL_RETRY_GATE:PortalRetryGate=PortalRetryGate::new();
#[cfg(all(target_os = "linux", feature = "portal"))]
pub(crate) fn check_portal_retry()->Result<(),crate::video::EncoderError> {PORTAL_RETRY_GATE.check()}
#[cfg(all(target_os = "linux", feature = "portal"))]
pub(crate) fn block_portal_retry_after_worker_failure() {PORTAL_RETRY_GATE.block(-libc::EIO);}


#[cfg(all(target_os = "linux", feature = "portal"))]
struct PortalWorkerUnwindGuard<'a>(&'a PortalRetryGate);
#[cfg(all(target_os = "linux", feature = "portal"))]
impl Drop for PortalWorkerUnwindGuard<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {self.0.block(-libc::EIO);}
    }
}

// Consume only the settlement coordinator's result. Err owners must never be
// destroyed or converted to an ordinary retryable encoder error.
#[cfg(all(target_os = "linux", feature = "portal"))]
fn portal_recovery_result<O,B>(result:Result<Option<Result<bool,crate::portal::PortalError>>,
    crate::portal::WorkerFrame<O,B>>,gate:&PortalRetryGate)->Result<bool,crate::video::EncoderError> {
    use crate::portal::WorkerFrame;
    match result {
        Err(retained)=>{
            let code=match &retained {
                WorkerFrame::Quarantined(code)|WorkerFrame::NeedsTeardown {code,..}=>*code,
                WorkerFrame::Complete {..}=>-libc::EIO, // invalid coordinator result: fail closed
            };
            gate.block(code);
            std::mem::forget(retained);
            gate.check()?;
            unreachable!()
        }
        Ok(result)=>{
            gate.check()?; // successful concurrent work cannot clear quarantine
            match result {
                None|Some(Ok(false))=>Ok(false),
                Some(Ok(true))=>Ok(true),
                Some(Err(error))=>Err(crate::video::EncoderError::Terminal(
                    format!("Portal SHM replacement unavailable: code={}",error.code))),
            }
        }
    }
}

pub struct EncodedUnit {
    pub validity: Option<crate::portal::FrameValidity>,
    pub data: Vec<u8>,
    pub permit: OwnedSemaphorePermit,
    pub completed_at: Instant,
}

fn acquire(
    budget: &Arc<Semaphore>,
    units: &mpsc::Sender<EncodedUnit>,
    cancelled: impl Fn() -> bool,
) -> Option<OwnedSemaphorePermit> {
    tokio::runtime::Handle::current().block_on(async {
        let pending = budget.clone().acquire_owned();
        tokio::pin!(pending);
        loop {
            if cancelled() || units.is_closed() { return None; }
            tokio::select! {
                permit = &mut pending => {
                    let permit = permit.ok()?;
                    if cancelled() || units.is_closed() { return None; }
                    return Some(permit);
                }
                _ = units.closed() => return None,
                _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
        }
    })
}

// Called on the owning blocking encoder thread, after source work/leases have
// completed. Reserve capacity before the final validity check. The callback can
// include a Portal source epoch; this is not atomic with later revocation.
fn send_encoded_unit(units: &mpsc::Sender<EncodedUnit>, unit: EncodedUnit,
    cancelled: impl Fn() -> bool) -> bool {
    tokio::runtime::Handle::current().block_on(async {
        // Retain the pending reservation across polls to preserve queue fairness.
        let pending = units.reserve();
        tokio::pin!(pending);
        loop {
            if cancelled() || unit.validity.as_ref().is_some_and(|v| v.cancelled()) { return false; }
            tokio::select! {
                reserved = &mut pending => {
                    let Ok(reserved) = reserved else { return false; };
                    if cancelled() || unit.validity.as_ref().is_some_and(|v| v.cancelled()) { return false; }
                    reserved.send(unit);
                    return true;
                }
                _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
        }
    })
}

fn random_keepalive(seed: &mut u32) -> Duration {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 17;
    *seed ^= *seed << 5;
    Duration::from_micros(40_000 + u64::from(*seed % 26_700))
}

/// Explicit operator admission of the reviewed producer contract. Never infer
/// this profile from buffer flags, GPU vendor or compositor version alone.
#[cfg(all(target_os = "linux", feature = "portal"))]
#[derive(Debug,PartialEq,Eq)]
struct PortalNvencAdmission {modifier:u64,device:std::path::PathBuf}
#[cfg(all(target_os = "linux", feature = "portal"))]
impl PortalNvencAdmission {
    fn parse(profile:Option<&str>,modifier:Option<&str>,device:Option<&str>)->Result<Option<Self>,String> {
        let Some(profile)=profile.filter(|s| !s.is_empty()) else {return Ok(None);};
        if profile!="mutter-release-point-v1" {return Err("Unsupported Portal NVENC producer profile".into());}
        let modifier=modifier.and_then(|s| u64::from_str_radix(s.strip_prefix("0x").unwrap_or(s),16).ok())
            .filter(|m| *m!=0x00ffffffffffffff).ok_or("Explicit Portal NVENC modifier required")?;
        let device=device.map(std::path::PathBuf::from).filter(|p|p.is_absolute())
            .ok_or("Absolute Portal NVENC render device required")?;
        Ok(Some(Self {modifier,device}))
    }
    fn configured()->Result<Option<Self>,String> {
        Self::parse(std::env::var("UU_PORTAL_NVENC_PROFILE").ok().as_deref(),
            std::env::var("UU_PORTAL_NVENC_MODIFIER").ok().as_deref(),
            std::env::var("UU_PORTAL_NVENC_DEVICE").ok().as_deref())
    }
}
pub(crate) fn portal_nvenc_selected()->bool {
    #[cfg(all(target_os = "linux", feature = "portal"))]
    {PortalNvencAdmission::configured().is_ok_and(|admission| admission.is_some())}
    #[cfg(not(all(target_os = "linux", feature = "portal")))]
    {false}
}

#[cfg(all(target_os = "linux", feature = "portal"))]
fn run_portal_nvenc(config:Config,units:mpsc::Sender<EncodedUnit>,stop:Arc<AtomicBool>,
    idr:Arc<AtomicBool>,budget:Arc<Semaphore>)->Result<(),crate::video::EncoderError> {
    use crate::video::EncoderError;
    use std::os::fd::AsFd;
    let admission=PortalNvencAdmission::configured().map_err(EncoderError::Terminal)?
        .ok_or_else(|| EncoderError::Terminal("Portal NVENC admission is not configured".into()))?;
    let session=config.portal.as_ref().ok_or("Portal session missing")?;
    if stop.load(Ordering::Acquire) || units.is_closed() {return Ok(());}
    // Reserve the exact source before initialization, including its recovery
    // path. A competing capture is not permission to try a CPU reader instead.
    let source=session.reserve_explicit(crate::portal::ExplicitProtocol::ReleasePoint)
        .map_err(|code| EncoderError::Terminal(format!("Portal capture reservation unavailable: code={code}")))?;
    let initialized=(|| {
        let render=std::fs::OpenOptions::new().read(true).write(true).open(&admission.device)
            .map_err(|e| -e.raw_os_error().unwrap_or(libc::EIO))?;
        PortalNvencFrameWorker::open(&config,render.as_fd(),crate::portal::ExplicitProtocol::ReleasePoint)
    })();
    let worker=match initialized {
        Ok(worker)=>Some(worker),
        Err(code)=>{log::warn!("Portal NVENC initialization failed: code={code}"); None}
    };
    let ready=if let Some(worker)=worker {
        if stop.load(Ordering::Acquire) || units.is_closed() {return Ok(());}
        // Explicit opt-in asserts the reviewed producer's release-before-reuse
        // contract; device identity is checked by worker initialization.
        let status=unsafe {session.configure_explicit(&source,admission.modifier)};
        if status==0 {
            log::info!("Portal NVIDIA DMA-BUF worker started with explicit release-point synchronization");
            unsafe {worker.run_portal_stream_recover(&config,session,&source,&units,&stop,&idr,&budget)}?
        } else {
            drop(worker);
            log::warn!("Portal explicit negotiation failed: code={status}");
            portal_recovery_result::<(),()>(Ok(Some(unsafe {session.fallback_explicit_to_shm(&source,&stop)})),&PORTAL_RETRY_GATE)?
        }
    } else {
        if stop.load(Ordering::Acquire) || units.is_closed() {return Ok(());}
        portal_recovery_result::<(),()>(Ok(Some(unsafe {session.fallback_explicit_to_shm(&source,&stop)})),&PORTAL_RETRY_GATE)?
    };
    drop(source); // release reservation before the video layer starts SHM capture
    if ready {Err(EncoderError::Retryable("Portal NVIDIA path retired; fresh SHM session installed".into()))}
    else {Ok(())}
}

pub fn run(
    config: Config,
    units: mpsc::Sender<EncodedUnit>,
    stop: Arc<AtomicBool>,
    idr: Arc<AtomicBool>,
    budget: Arc<Semaphore>,
) -> Result<(), crate::video::EncoderError> {
    #[cfg(all(target_os = "linux", feature = "portal"))]
    if config.portal.is_some() {check_portal_retry()?;}
    #[cfg(all(target_os = "linux", feature = "portal"))]
    let _unwind=config.portal.as_ref().map(|_| PortalWorkerUnwindGuard(&PORTAL_RETRY_GATE));
    #[cfg(all(target_os = "linux", feature = "portal"))]
    if config.portal.is_some() && !config.vaapi {return run_portal_nvenc(config,units,stop,idr,budget);}
    if config.vaapi {
        run_vaapi(config, units, stop, idr, budget).map_err(Into::into)
    } else {
        run_nvenc(config, units, stop, idr, budget).map_err(Into::into)
    }
}

pub fn black_frame(codec: crate::capture_setting::VideoCodec) -> Result<bytes::Bytes, String> {
    let device = CString::new(crate::video::vaapi_device()).map_err(|_| "Invalid VAAPI device")?;
    let config = VaapiConfigFFI {
        width: 1280, height: 720, cap_width: 1280, cap_height: 720, fps: 1, gop: 1,
        target_bps: 1_000_000, max_bps: 2_000_000, buffer_bits: 500_000,
        is_hevc: libc::c_int::from(codec == crate::capture_setting::VideoCodec::H265),
        device: device.as_ptr(),
    };
    let engine = SafeVaapiEngine(unsafe { vaapi_create(&config) });
    if engine.0.is_null() { return Err("Locked video VAAPI initialization failed".into()); }
    let pixels = vec![0; 1280 * 720 * 4];
    if unsafe { vaapi_upload_host_buffer(engine.0, pixels.as_ptr(), 1280 * 4) } != 0 {
        return Err("Locked video VAAPI upload failed".into());
    }
    let mut data = std::ptr::null();
    let mut length = 0;
    if unsafe { vaapi_encode_frame(engine.0, 1, &mut data, &mut length) } != 0 || data.is_null() || length == 0 {
        return Err("Locked video VAAPI encode failed".into());
    }
    Ok(bytes::Bytes::copy_from_slice(unsafe { std::slice::from_raw_parts(data, length as usize) }))
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
    if let Some(portal) = config.portal.as_ref() {
        return run_portal_vaapi(&config, portal, &engine, units, stop, idr, budget);
    }

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

        let Some(permit) = acquire(&budget, &units, || stop.load(Ordering::Acquire)) else {
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

            if !send_encoded_unit(&units, EncodedUnit {
                validity: None, data, permit: send_permit, completed_at: Instant::now(),
            }, || stop.load(Ordering::Acquire))
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
fn vaapi_packet(engine: &SafeVaapiEngine, idr: &AtomicBool) -> Result<Vec<u8>, String> {
    let mut data = std::ptr::null();
    let mut length = 0;
    let force_idr = libc::c_int::from(idr.swap(false, Ordering::AcqRel));
    let result = unsafe { vaapi_encode_frame(engine.0, force_idr, &mut data, &mut length) };
    if result != 0 { return Err(format!("VAAPI encode failed: {result}")); }
    if data.is_null() || length == 0 { return Ok(Vec::new()); }
    Ok(unsafe { std::slice::from_raw_parts(data, length as usize) }.to_vec())
}

fn portal_upload_if_needed(
    changed: bool, due: bool, uploaded: &mut bool, upload: impl FnOnce() -> Result<(), String>,
) -> Result<bool, String> {
    if !changed && *uploaded && !due { return Ok(false); }
    if changed || !*uploaded {
        upload()?;
        *uploaded = true;
    }
    Ok(true)
}

trait PortalVaapiIo {
    fn size(&self) -> (u32, u32);
    fn capture(&mut self, frame: &mut [u8]) -> Result<bool, String>;
    fn upload(&mut self, frame: &[u8], pitch: u32) -> Result<(), String>;
    fn encode(&mut self) -> Result<Vec<u8>, String>;
}

struct PortalVaapiNative<'a> {
    portal: &'a crate::portal::PortalSession,
    engine: &'a SafeVaapiEngine,
    idr: &'a AtomicBool,
    version: Option<crate::portal::FrameVersion>,
}
impl PortalVaapiIo for PortalVaapiNative<'_> {
    fn size(&self) -> (u32, u32) { self.portal.size() }
    fn capture(&mut self, frame: &mut [u8]) -> Result<bool, String> { self.portal.capture_changed(frame, &mut self.version) }
    fn upload(&mut self, frame: &[u8], pitch: u32) -> Result<(), String> {
        let result = unsafe { vaapi_upload_host_buffer(self.engine.0, frame.as_ptr(), pitch) };
        if result != 0 { return Err(format!("Portal VAAPI upload failed: {result}")); }
        Ok(())
    }
    fn encode(&mut self) -> Result<Vec<u8>, String> { vaapi_packet(self.engine, self.idr) }
}

fn portal_vaapi_step(io: &mut impl PortalVaapiIo, size: (u32, u32), closed: bool, due: bool,
    frame: &mut Vec<u8>, uploaded: &mut bool) -> Result<Option<Vec<u8>>, String> {
    if closed { return Ok(None); }
    if io.size() != size { return Err("Portal size changed; restart encoder".into()); }
    frame.resize(size.0 as usize * size.1 as usize * 4, 0);
    let changed = io.capture(frame)?;
    if !portal_upload_if_needed(changed, due, uploaded, || io.upload(frame, size.0 * 4))? { return Ok(None); }
    Ok(Some(io.encode()?))
}

fn run_portal_vaapi(
    config: &Config, portal: &crate::portal::PortalSession, engine: &SafeVaapiEngine,
    units: mpsc::Sender<EncodedUnit>, stop: Arc<AtomicBool>, idr: Arc<AtomicBool>, budget: Arc<Semaphore>,
) -> Result<(), String> {
    let interval = Duration::from_nanos(1_000_000_000 / u64::from(config.fps));
    let mut next_poll = Instant::now();
    let mut last_send = Instant::now();
    let mut seed = 1;
    let mut keepalive = random_keepalive(&mut seed);
    let mut io = PortalVaapiNative { portal, engine, idr: &idr, version: None };
    let mut cpu_frame = Vec::new();
    let mut uploaded = false;
    // Fail closed: even failed VA submissions may leave GPU reads in flight. Never
    // import a producer-owned Portal frame; host upload keeps fallback independent.
    log::info!("Portal VAAPI DMA-BUF disabled for safety; pipeline=host-upload");
    loop {
        if stop.load(Ordering::Relaxed) || units.is_closed() { return Ok(()); }
        let wait_dur = next_poll.saturating_duration_since(Instant::now());
        if wait_dur > Duration::ZERO {
            match portal.wait_frame(wait_dur) {
                Ok(true) | Ok(false) => (),
                Err(_) => {
                    // Frame wait failed or stream closed/stalled: back off to next_poll
                    // (preventing 100% core busy-spin) and allow existing recovery in with_dmabuf
                    // and capture_changed to handle lock/scaling without prematurely aborting the encoder.
                    std::thread::sleep(wait_dur);
                }
            }
        }
        let now = Instant::now();
        if now < next_poll { continue; }
        next_poll = (next_poll + interval).max(now);
        if portal.size() != config.input { return Err("Portal size changed; restart encoder".into()); }
        let Some(permit) = acquire(&budget, &units, || stop.load(Ordering::Acquire)) else { return Ok(()); };
        if stop.load(Ordering::Relaxed) { return Ok(()); }
        let due = last_send.elapsed() >= keepalive || idr.load(Ordering::Acquire);
        let Some(data) = portal_vaapi_step(&mut io, config.input, stop.load(Ordering::Relaxed) || units.is_closed(), due,
            &mut cpu_frame, &mut uploaded)? else { continue; };
        if data.is_empty() { continue; }
        if !send_encoded_unit(&units, EncodedUnit { validity: None, data, permit, completed_at: Instant::now() },
            || stop.load(Ordering::Acquire)) { return Ok(()); }
        last_send = Instant::now();
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

        let Some(permit) = acquire(&budget, &units, || stop.load(Ordering::Acquire)) else {
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

            if !send_encoded_unit(&units, EncodedUnit {
                validity: None, data, permit: send_permit, completed_at: Instant::now(),
            }, || stop.load(Ordering::Acquire))
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
    #[cfg(all(target_os = "linux", feature = "portal"))]
    #[test]
    fn portal_admission_requires_explicit_complete_profile() {
        assert_eq!(PortalNvencAdmission::parse(None,Some("0"),Some("/dev/dri/renderD128")),Ok(None));
        assert_eq!(PortalNvencAdmission::parse(Some(""),None,None),Ok(None));
        let profile=Some("mutter-release-point-v1");
        assert!(PortalNvencAdmission::parse(Some("auto"),Some("0"),Some("/dev/dri/renderD128")).is_err());
        for modifier in [None,Some(""),Some("bad-value"),Some("0x00ffffffffffffff"),Some("10000000000000000")] {
            assert!(PortalNvencAdmission::parse(profile,modifier,Some("/dev/dri/renderD128")).is_err());
        }
        for device in [None,Some(""),Some("renderD128")] {
            assert!(PortalNvencAdmission::parse(profile,Some("0"),device).is_err());
        }
        let parsed=PortalNvencAdmission::parse(profile,Some("0x0"),Some("/dev/dri/renderD128")).unwrap().unwrap();
        assert_eq!(parsed.modifier,0);
        assert_eq!(parsed.device,std::path::PathBuf::from("/dev/dri/renderD128"));
    }

    #[cfg(all(target_os = "linux", feature = "portal"))]
    #[test]
    fn portal_quarantine_blocks_retry_and_preserves_owners() {
        use crate::portal::WorkerFrame;
        let gate=PortalRetryGate::new();
        assert!(gate.check().is_ok());
        for (result,expected) in [(None,false),(Some(Ok(false)),false),(Some(Ok(true)),true)] {
            assert_eq!(portal_recovery_result::<(),()>(Ok(result),&gate),Ok(expected));
        }
        assert!(matches!(portal_recovery_result::<(),()>(Ok(Some(Err(crate::portal::PortalError {code:13}))),&gate),
            Err(crate::video::EncoderError::Terminal(_))));
        assert!(gate.check().is_ok(),"SHM denial is terminal for this task, not GPU quarantine");
        assert!(matches!(portal_recovery_result::<(),()>(Err(WorkerFrame::Quarantined(-libc::EIO)),&gate),
            Err(crate::video::EncoderError::Terminal(_))));
        for result in [None,Some(Ok(true)),Some(Ok(false))] {
            assert!(portal_recovery_result::<(),()>(Ok(result),&gate).is_err());
        }
        gate.block(-libc::EBUSY);
        assert_eq!(gate.0.load(Ordering::Acquire),-libc::EIO,"first quarantine reason persists");

        struct Backend(Arc<AtomicBool>);
        impl Drop for Backend {fn drop(&mut self) {self.0.store(true,Ordering::Release);}}
        let dropped=Arc::new(AtomicBool::new(false));
        let cleanup_gate=PortalRetryGate::new();
        let lease=crate::portal::synthetic_lease(crate::portal::AsyncFrame::default(),0);
        let result=portal_recovery_result(Err(WorkerFrame::NeedsTeardown {
            backend:Backend(dropped.clone()),lease,code:-libc::EBUSY}),&cleanup_gate);
        assert!(matches!(result,Err(crate::video::EncoderError::Terminal(_))));
        assert!(!dropped.load(Ordering::Acquire),"failed cleanup owners must be retained");
        assert!(cleanup_gate.check().is_err());
        // Intentionally retained tiny synthetic owners model process-lifetime quarantine.
        let zero_gate=PortalRetryGate::new(); zero_gate.block(0);
        assert!(zero_gate.check().is_err(),"zero error must not reopen retry");
        let unwind_gate=PortalRetryGate::new();
        {let _guard=PortalWorkerUnwindGuard(&unwind_gate);}
        assert!(unwind_gate.check().is_ok());
        assert!(std::panic::catch_unwind(|| {
            let _guard=PortalWorkerUnwindGuard(&unwind_gate);
            panic!("synthetic detached worker failure");
        }).is_err());
        assert!(unwind_gate.check().is_err(),"worker must latch even without a waiting reader");
    }

    #[cfg(all(target_os = "linux", feature = "portal"))]
    #[test]
    fn stream_recovery_waits_for_cleanup_and_respects_stop() {
        use crate::portal::WorkerFrame;
        use std::cell::Cell;
        let dropped=Cell::new(false);
        struct Backend<'a>(&'a Cell<bool>);
        impl Drop for Backend<'_> {fn drop(&mut self) {self.0.set(true);}}
        for (packet,stop,expected) in [(Ok(()),false,false),(Err(-libc::EIO),false,true),
            (Err(-libc::EIO),true,false),(Err(-libc::ECANCELED),false,false)] {
            dropped.set(false);
            let outcome:WorkerFrame<(),_,()>=WorkerFrame::Complete {backend:Backend(&dropped),packet};
            let result=settle_portal_stream(outcome,|| stop,|_,_| panic!("no lease"),|| {
                assert!(dropped.get()); 7
            });
            assert!(matches!(result,Ok(value) if value==expected.then_some(7)));
            assert!(dropped.get());
        }
        let outcome:WorkerFrame<(),(),()>=WorkerFrame::Quarantined(-libc::EIO);
        assert!(matches!(settle_portal_stream(outcome,|| false,|_,_| panic!("unknown cleanup"),
            || panic!("unknown recovery")),Err(WorkerFrame::Quarantined(code)) if code == -libc::EIO));
        for (fail_cleanup,stop_during_cleanup) in [(false,false),(false,true),(true,false)] {
            let lease=crate::portal::synthetic_lease(crate::portal::AsyncFrame::default(),0);
            let stop=Cell::new(false);
            let result=settle_portal_stream(WorkerFrame::NeedsTeardown {backend:(),lease,code:-libc::EIO},
                || stop.get(),|backend,mut lease| {
                    if fail_cleanup {return Err(WorkerFrame::NeedsTeardown {backend,lease,code:-libc::EBUSY});}
                    unsafe {lease.abandon_completed().unwrap();}
                    stop.set(stop_during_cleanup); // stop racing cleanup must suppress a new prompt
                    Ok(())
                },|| {assert!(!fail_cleanup && !stop.get()); 7});
            match result {
                Ok(Some(7))=>assert!(!fail_cleanup && !stop_during_cleanup),
                Ok(None)=>assert!(!fail_cleanup && stop_during_cleanup),
                Err(WorkerFrame::NeedsTeardown {mut lease,code,..})=>{
                    assert!(fail_cleanup); assert_eq!(code,-libc::EBUSY);
                    unsafe {lease.abandon_completed().unwrap();}
                }
                _=>panic!("unexpected cleanup result"),
            }
        }
    }

    #[cfg(all(target_os = "linux", feature = "portal"))]
    #[test]
    fn portal_output_preserves_errors_and_returns_unused_budget() {
        use crate::portal::WorkerFrame;
        for cancelled in [false, true] {
            let budget=Arc::new(Semaphore::new(1));
            let input: WorkerFrame<(), ()>=WorkerFrame::Complete { backend:(),
                packet: if cancelled { Ok(vec![1]) } else { Err(-libc::EIO) } };
            let result=portal_worker_output(input,budget.clone().try_acquire_owned().unwrap(),
                crate::portal::FrameValidity::new(move || cancelled));
            match result {
                WorkerFrame::Complete { packet:Err(code),.. } =>
                    assert_eq!(code,if cancelled {-libc::ECANCELED} else {-libc::EIO}),
                _ => panic!("failed or revoked packet must not become output"),
            }
            assert_eq!(budget.available_permits(),1);
        }
        let budget=Arc::new(Semaphore::new(1));
        let lease=crate::portal::synthetic_lease(crate::portal::AsyncFrame::default(),0);
        let result=portal_worker_output(WorkerFrame::NeedsTeardown {backend:42,lease,code:-libc::ESTALE},
            budget.clone().try_acquire_owned().unwrap(),crate::portal::FrameValidity::new(|| false));
        assert_eq!(budget.available_permits(),1);
        match result {
            WorkerFrame::NeedsTeardown { backend,mut lease,code } => {
                assert_eq!(backend,42); assert_eq!(code,-libc::ESTALE);
                // Synthetic-only owner, no real producer or GPU work.
                unsafe { lease.abandon_completed().unwrap(); }
            }
            _ => panic!("teardown ownership was lost"),
        }
        let input: WorkerFrame<(), ()>=WorkerFrame::Quarantined(-libc::EIO);
        let result=portal_worker_output(input,budget.clone().try_acquire_owned().unwrap(),
            crate::portal::FrameValidity::new(|| false));
        assert!(matches!(result,WorkerFrame::Quarantined(code) if code == -libc::EIO));
        assert_eq!(budget.available_permits(),1);
    }

    fn test_unit(budget: &Arc<Semaphore>) -> EncodedUnit {
        EncodedUnit { validity: None, data: vec![1], completed_at: Instant::now(),
            permit: budget.clone().try_acquire_owned().unwrap() }
    }

    #[cfg(all(target_os = "linux", feature = "portal"))]
    #[tokio::test]
    async fn worker_publication_retains_completed_status_on_send_failure() {
        use crate::portal::WorkerFrame;
        for mode in 0..5 {
            let budget=Arc::new(Semaphore::new(1));
            let mut unit=test_unit(&budget);
            unit.validity=Some(crate::portal::FrameValidity::new(move || mode==2));
            let (tx,mut rx)=mpsc::channel(1);
            if mode==3 { rx.close(); }
            let input:WorkerFrame<(),usize,EncodedUnit>=WorkerFrame::Complete {backend:7,
                packet:if mode==4 {drop(unit); Err(-libc::EIO)} else {Ok(unit)} };
            let result=tokio::task::spawn_blocking(move || {
                portal_worker_publish(input,&tx,&Arc::new(AtomicBool::new(mode==1)))
            }).await.unwrap();
            match result {
                WorkerFrame::Complete {backend,packet}=>{
                    assert_eq!(backend,7);
                    assert_eq!(packet,if mode==0 {Ok(())} else {
                        Err(if mode==4 {-libc::EIO} else {-libc::ECANCELED})
                    });
                }
                _=>panic!("publication failure must never recreate a native lease"),
            }
            if mode==0 { drop(rx.recv().await.unwrap()); }
            assert!(rx.recv().await.is_none());
            assert_eq!(budget.available_permits(),1);
        }
    }

    #[tokio::test]
    async fn output_cancellation_releases_capacity_before_and_after_reservation() {
        for after_reservation in [false, true] {
            let budget = Arc::new(Semaphore::new(1));
            let unit = test_unit(&budget);
            let (tx, mut rx) = mpsc::channel(1);
            let result = tokio::task::spawn_blocking(move || {
                let polls = std::cell::Cell::new(0);
                send_encoded_unit(&tx, unit, || {
                    let n = polls.get(); polls.set(n + 1);
                    !after_reservation || n > 0
                })
            }).await.unwrap();
            assert!(!result);
            assert!(rx.recv().await.is_none());
            assert_eq!(budget.available_permits(), 1);
        }
    }

    #[cfg(all(target_os = "linux", feature = "portal"))]
    #[tokio::test]
    async fn queued_worker_frames_keep_their_original_task_and_source_guards() {
        use crate::portal::WorkerFrame;
        let budget=Arc::new(Semaphore::new(2));
        let source=Arc::new(AtomicBool::new(false));
        let old_stop=Arc::new(AtomicBool::new(false));
        let new_stop=Arc::new(AtomicBool::new(false));
        let (tx,mut rx)=mpsc::channel(2);
        for stop in [&old_stop,&new_stop] {
            let mut unit=test_unit(&budget);
            unit.validity=Some(crate::portal::FrameValidity::new({
                let source=source.clone(); move || source.load(Ordering::Acquire)
            }));
            let input:WorkerFrame<(),usize,EncodedUnit>=WorkerFrame::Complete {backend:7,packet:Ok(unit)};
            let tx=tx.clone(); let stop=stop.clone();
            let result=tokio::task::spawn_blocking(move || portal_worker_publish(input,&tx,&stop)).await.unwrap();
            assert!(matches!(result,WorkerFrame::Complete {backend:7,packet:Ok(())}));
        }
        assert_eq!(budget.available_permits(),0);
        // Both packets were queued while live. Restart only the old task before
        // the reader receives either packet; the shared Portal stays live.
        old_stop.store(true,Ordering::Release);
        let old=rx.recv().await.unwrap(); let new=rx.recv().await.unwrap();
        let old_guard=old.validity.as_ref().unwrap().clone();
        let new_guard=new.validity.as_ref().unwrap().clone();
        assert!(old_guard.cancelled()); assert!(!new_guard.cancelled());
        assert!(!source.load(Ordering::Acquire));
        drop(old); drop(new);
        assert_eq!(budget.available_permits(),2);
        drop(old_stop); drop(new_stop); // Guard owns the original stop token.
        assert!(old_guard.cancelled()); assert!(!new_guard.cancelled());
        source.store(true,Ordering::Release);
        assert!(new_guard.cancelled());
    }

    #[tokio::test]
    async fn output_cancellation_wakes_with_full_queue_and_open_receiver() {
        let budget = Arc::new(Semaphore::new(2));
        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(test_unit(&budget)).ok().unwrap();
        let unit = test_unit(&budget);
        let stop = Arc::new(AtomicBool::new(false));
        let (polled, ready) = tokio::sync::oneshot::channel();
        let worker = tokio::task::spawn_blocking({
            let stop = stop.clone();
            move || {
                let polled = std::cell::RefCell::new(Some(polled));
                send_encoded_unit(&tx, unit, || {
                    if let Some(polled) = polled.borrow_mut().take() { let _ = polled.send(()); }
                    stop.load(Ordering::Acquire)
                })
            }
        });
        ready.await.unwrap();
        stop.store(true, Ordering::Release);
        assert!(!tokio::time::timeout(Duration::from_secs(1), worker).await.unwrap().unwrap());
        assert_eq!(budget.available_permits(), 1);
        drop(rx.recv().await.unwrap());
        assert!(rx.recv().await.is_none());
        assert_eq!(budget.available_permits(), 2);
    }

    #[tokio::test]
    async fn source_revocation_cancels_output_without_stopping_encoder() {
        let budget=Arc::new(Semaphore::new(2));
        let (tx,mut rx)=mpsc::channel(1);
        tx.try_send(test_unit(&budget)).ok().unwrap();
        let revoked=Arc::new(AtomicBool::new(false));
        let (polled,ready)=tokio::sync::oneshot::channel();
        let polled=std::sync::Mutex::new(Some(polled));
        let mut unit=test_unit(&budget);
        unit.validity=Some(crate::portal::FrameValidity::new({
            let revoked=revoked.clone();
            move || {
                if let Some(polled)=polled.lock().unwrap().take() { let _=polled.send(()); }
                revoked.load(Ordering::Acquire)
            }
        }));
        let worker=tokio::task::spawn_blocking(move || send_encoded_unit(&tx,unit,|| false));
        ready.await.unwrap();
        revoked.store(true,Ordering::Release);
        assert!(!tokio::time::timeout(Duration::from_secs(1),worker).await.unwrap().unwrap());
        assert_eq!(budget.available_permits(),1);
        drop(rx.recv().await.unwrap());
        assert!(rx.recv().await.is_none());
        assert_eq!(budget.available_permits(),2);
    }

    #[tokio::test]
    async fn output_delivers_live_packet_and_reclaims_closed_receiver_packet() {
        for closed in [false, true] {
            let budget = Arc::new(Semaphore::new(1));
            let unit = test_unit(&budget);
            let (tx, mut rx) = mpsc::channel(1);
            if closed { rx.close(); }
            assert_eq!(tokio::task::spawn_blocking(move || send_encoded_unit(&tx, unit, || false))
                .await.unwrap(), !closed);
            if !closed {
                let unit = rx.recv().await.unwrap();
                assert_eq!(unit.data, vec![1]);
                assert_eq!(budget.available_permits(), 0);
                drop(unit);
            }
            assert!(rx.recv().await.is_none());
            assert_eq!(budget.available_permits(), 1);
        }
    }

    #[test]
    #[ignore = "requires a VAAPI render node and FFmpeg for independent decode verification"]
    fn vaapi_locked_frames_decode_as_black_in_both_codecs() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        for codec in [crate::capture_setting::VideoCodec::H264, crate::capture_setting::VideoCodec::H265] {
            let frame = black_frame(codec).unwrap();
            let mut decoder = Command::new("ffmpeg").args(["-hide_banner", "-loglevel", "error", "-i", "pipe:0",
                "-frames:v", "1", "-vf", "scale=1:1", "-pix_fmt", "rgb24", "-f", "rawvideo", "pipe:1"])
                .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
            decoder.stdin.take().unwrap().write_all(&frame).unwrap();
            let decoded = decoder.wait_with_output().unwrap();
            assert!(decoded.status.success(), "decode failed for {codec:?}");
            assert_eq!(decoded.stdout.len(), 3);
            assert!(decoded.stdout.iter().all(|&value| value <= 3), "frame is not black for {codec:?}");
        }
    }

    #[test]
    fn portal_cpu_pipeline_injected_io() {
        struct Mock { size: (u32, u32), capture_error: bool, encode_error: bool, calls: Vec<&'static str>, pixels: Vec<u8> }
        impl PortalVaapiIo for Mock {
            fn size(&self) -> (u32, u32) { self.size }
            fn capture(&mut self, frame: &mut [u8]) -> Result<bool, String> {
                self.calls.push("capture");
                if self.capture_error { return Err("capture".into()); }
                frame.fill(0); Ok(true)
            }
            fn upload(&mut self, frame: &[u8], pitch: u32) -> Result<(), String> {
                self.calls.push("upload"); assert_eq!(pitch, 8); self.pixels = frame.to_vec(); Ok(())
            }
            fn encode(&mut self) -> Result<Vec<u8>, String> {
                self.calls.push("encode");
                if self.encode_error { Err("encode".into()) } else { Ok(vec![1]) }
            }
        }
        let mut io = Mock { size: (2, 2), capture_error: false, encode_error: false, calls: vec![], pixels: vec![] };
        let (mut frame, mut uploaded) = (vec![255; 16], false);
        assert_eq!(portal_vaapi_step(&mut io, (2, 2), false, false, &mut frame, &mut uploaded).unwrap(), Some(vec![1]));
        assert_eq!(io.pixels, vec![0; 16]);
        assert_eq!(io.calls, ["capture", "upload", "encode"]); // No DMA operation exists in the interface.
        io.calls.clear();
        assert_eq!(portal_vaapi_step(&mut io, (2, 2), true, false, &mut frame, &mut uploaded).unwrap(), None);
        assert!(io.calls.is_empty());
        io.size = (3, 2);
        assert!(portal_vaapi_step(&mut io, (2, 2), false, false, &mut frame, &mut uploaded).is_err());
        assert!(io.calls.is_empty());
        io.size = (2, 2); io.capture_error = true;
        assert_eq!(portal_vaapi_step(&mut io, (2, 2), false, false, &mut frame, &mut uploaded).unwrap_err(), "capture");
        assert_eq!(io.calls, ["capture"]);
        io.calls.clear(); io.capture_error = false; io.encode_error = true;
        assert_eq!(portal_vaapi_step(&mut io, (2, 2), false, false, &mut frame, &mut uploaded).unwrap_err(), "encode");
        assert_eq!(io.calls, ["capture", "upload", "encode"]);
    }

    #[test]
    fn portal_vaapi_step_propagates_injected_failures() {
        // This drives the production CPU step with injected IO, not a real Portal or VAAPI.
        struct Mock { fail: &'static str, calls: Vec<&'static str> }
        impl PortalVaapiIo for Mock {
            fn size(&self) -> (u32, u32) { (1, 1) }
            fn capture(&mut self, frame: &mut [u8]) -> Result<bool, String> {
                self.calls.push("capture");
                if self.fail == "capture" { return Err("capture".into()); }
                frame.fill(7); Ok(true)
            }
            fn upload(&mut self, frame: &[u8], _: u32) -> Result<(), String> {
                self.calls.push("upload"); assert_eq!(frame, [7; 4]);
                if self.fail == "upload" { Err("upload".into()) } else { Ok(()) }
            }
            fn encode(&mut self) -> Result<Vec<u8>, String> {
                self.calls.push("encode");
                if self.fail == "encode" { Err("encode".into()) } else { Ok(vec![7]) }
            }
        }
        for failure in ["capture", "upload", "encode"] {
            let mut io = Mock { fail: failure, calls: Vec::new() };
            let (mut frame, mut uploaded) = (vec![99; 4], false);
            let result = portal_vaapi_step(&mut io, (1, 1), false, true, &mut frame, &mut uploaded);
            assert_eq!(result.unwrap_err(), failure);
            assert_eq!(uploaded, failure == "encode");
            assert_eq!(io.calls, match failure {
                "capture" => vec!["capture"],
                "upload" => vec!["capture", "upload"],
                _ => vec!["capture", "upload", "encode"],
            });
        }
    }

    #[test]
    fn portal_host_upload_handles_first_frame_damage_keepalive_and_failure() {
        let mut uploaded = false;
        let mut calls = 0;
        assert!(portal_upload_if_needed(false, false, &mut uploaded, || { calls += 1; Ok(()) }).unwrap());
        assert!(uploaded);
        assert!(!portal_upload_if_needed(false, false, &mut uploaded, || panic!("unchanged")).unwrap());
        assert!(portal_upload_if_needed(false, true, &mut uploaded, || panic!("keepalive")).unwrap());
        assert!(portal_upload_if_needed(true, false, &mut uploaded, || { calls += 1; Ok(()) }).unwrap());
        assert_eq!(calls, 2);
        let mut fresh = false;
        assert!(portal_upload_if_needed(true, false, &mut fresh, || Err("upload failed".into())).is_err());
        assert!(!fresh);
    }

    #[tokio::test]
    async fn budget_wait_cancels_with_no_capacity_and_open_receiver() {
        let budget = Arc::new(Semaphore::new(0));
        let (tx, _rx) = mpsc::channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let (polled, ready) = tokio::sync::oneshot::channel();
        let worker = tokio::task::spawn_blocking({
            let stop = stop.clone(); let budget = budget.clone();
            move || {
                let polled = std::cell::RefCell::new(Some(polled));
                acquire(&budget, &tx, || {
                    if let Some(polled) = polled.borrow_mut().take() { let _ = polled.send(()); }
                    stop.load(Ordering::Acquire)
                })
            }
        });
        ready.await.unwrap();
        stop.store(true, Ordering::Release);
        assert!(tokio::time::timeout(Duration::from_secs(1), worker).await.unwrap().unwrap().is_none());
        budget.add_permits(1);
        assert_eq!(budget.available_permits(), 1);
    }

    #[tokio::test]
    async fn budget_cancel_after_grant_returns_permit() {
        let budget = Arc::new(Semaphore::new(1));
        let (tx, _rx) = mpsc::channel(1);
        let result = tokio::task::spawn_blocking({
            let budget = budget.clone();
            move || {
                let polls = std::cell::Cell::new(0);
                acquire(&budget, &tx, || {
                    let n = polls.get(); polls.set(n + 1); n > 0
                })
            }
        }).await.unwrap();
        assert!(result.is_none());
        assert_eq!(budget.available_permits(), 1);
    }

    #[tokio::test]
    async fn budget_live_grant_and_closed_budget() {
        for closed in [false, true] {
            let budget = Arc::new(Semaphore::new(1));
            if closed { budget.close(); }
            let (tx, _rx) = mpsc::channel(1);
            let result = tokio::task::spawn_blocking({
                let budget = budget.clone();
                move || acquire(&budget, &tx, || false)
            }).await.unwrap();
            assert_eq!(result.is_some(), !closed);
            drop(result);
            assert_eq!(budget.available_permits(), 1);
        }
    }

    #[tokio::test]
    async fn closing_encoder_output_wakes_a_blocked_capture() {
        let budget = Arc::new(Semaphore::new(0));
        let (tx, rx) = mpsc::channel(1);
        let worker = tokio::task::spawn_blocking(move || acquire(&budget, &tx, || false));
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

#[cfg(all(test, target_os = "linux", feature = "portal"))]
mod portal_hardware_tests {
    // Run on an unlocked desktop with saved Portal consent and VAAPI selected.
    // FFmpeg is used only for independent decode verification; desktop contents
    // are neither saved nor printed. These tests do not lock the real desktop
    // or replace system lock-screen acceptance in an isolated VM.
    use super::*;
    use crate::capture_setting::VideoCodec;

    struct Runner {
        stop: Arc<AtomicBool>,
        idr: Arc<AtomicBool>,
        rx: mpsc::Receiver<EncodedUnit>,
        job: Option<tokio::task::JoinHandle<Result<(), String>>>,
    }
    impl Drop for Runner {
        fn drop(&mut self) { self.stop.store(true, Ordering::Release); self.rx.close(); }
    }
    impl Runner {
        fn start(portal: Arc<crate::portal::PortalSession>, codec: VideoCodec) -> Self {
            let config = Config {
                input: portal.size(), portal: Some(portal), display: "wayland:portal".into(),
                output: None, fps: 60, gop: 120, cq: 18, target_bps: 16_000_000,
                max_bps: 30_000_000, buffer_bits: 500_000, codec,
                chroma: crate::capture_setting::CHROMA_420, vaapi: true,
            };
            let (tx, rx) = mpsc::channel(4);
            let stop = Arc::new(AtomicBool::new(false));
            let idr = Arc::new(AtomicBool::new(false));
            let worker_stop = stop.clone();
            let worker_idr = idr.clone();
            let job = tokio::task::spawn_blocking(move || run(config, tx, worker_stop, worker_idr, Arc::new(Semaphore::new(4))).map_err(Into::into));
            Self { stop, idr, rx, job: Some(job) }
        }
        async fn frame(&mut self) -> Vec<u8> {
            tokio::time::timeout(Duration::from_secs(10), self.rx.recv()).await.unwrap()
                .expect("native capture stopped").data
        }
        async fn hevc_idr(&mut self) -> Vec<u8> {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let frame = self.frame().await;
                    let nals = crate::hevc::payload(usize::MAX, &frame).unwrap();
                    if nals.iter().any(|nal| (16..=21).contains(&((nal[0] >> 1) & 63))) { return frame; }
                }
            }).await.expect("requested HEVC IDR did not arrive")
        }
        async fn finish(mut self) {
            self.stop.store(true, Ordering::Release);
            self.rx.close();
            tokio::time::timeout(Duration::from_secs(2), self.job.take().unwrap()).await
                .expect("native encoder did not stop").unwrap().unwrap();
        }
    }
    fn decoded_is_black(data: &[u8], codec: VideoCodec) -> bool {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let format = if codec == VideoCodec::H265 { "hevc" } else { "h264" };
        let mut decoder = Command::new("ffmpeg").args(["-hide_banner", "-loglevel", "error", "-f", format,
            "-i", "pipe:0", "-frames:v", "1", "-pix_fmt", "rgb24", "-f", "rawvideo", "pipe:1"])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        // Concurrent writing avoids filling stdin while the decoder is blocked on raw RGB output.
        let mut input = decoder.stdin.take().unwrap();
        let encoded = data.to_vec();
        let writer = std::thread::spawn(move || input.write_all(&encoded));
        let decoded = decoder.wait_with_output().unwrap();
        writer.join().unwrap().unwrap();
        assert!(decoded.status.success() && !decoded.stdout.is_empty(), "native frame decode failed");
        decoded.stdout.iter().all(|&value| value <= 8)
    }
    fn resources() -> (usize, u64) {
        let fds = std::fs::read_dir("/proc/self/fd").unwrap().count();
        let rss = std::fs::read_to_string("/proc/self/status").unwrap().lines()
            .find(|line| line.starts_with("VmRSS:")).unwrap().split_whitespace().nth(1).unwrap().parse().unwrap();
        (fds, rss)
    }
    async fn reopen(portal: Arc<crate::portal::PortalSession>) {
        tokio::task::spawn_blocking(move || {
            portal.end();
            assert!(portal.recover(&AtomicBool::new(false)).unwrap());
        }).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires an unlocked Portal desktop and saved consent"]
    async fn portal_snapshot_after_dma_initialization() {
        let _ = env_logger::builder().is_test(true).try_init();
        tokio::task::spawn_blocking(|| {
            let mut portal = crate::portal::PortalSession::open().unwrap();
            let (locked, watch) = tokio::sync::watch::channel(Some(false));
            portal.test_lock_watch(watch);
            let (width, height) = portal.size();
            let mut pixels = vec![0; width as usize * height as usize * 4];
            for state in [Some(true), None, Some(false)] {
                locked.send(state).unwrap();
                portal.pause();
                portal.capture(&mut pixels).unwrap();
                assert!(pixels.chunks_exact(4).any(|p| p[..3].iter().any(|&v| v > 8)),
                    "healthy snapshot was blanked for lock state {state:?}");
                println!("snapshot_lock_state={state:?} valid_frame=true");
            }
        }).await.unwrap();
    }

    // RUST_LOG=info cargo test --release --locked --features wayland,portal,inproc-av portal_five_minute_endurance_with_full_decode -- --ignored --nocapture
    // Run for five minutes, independently decode every output frame, require equal
    // encoded/decoded counts and no decode errors, and report CPU, RSS and FDs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "five-minute real Portal/VAAPI run; FFmpeg is used only as an independent decoder"]
    async fn portal_five_minute_endurance_with_full_decode() {
        use std::process::Stdio;
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let _ = env_logger::builder().is_test(true).try_init();
        let session = tokio::task::spawn_blocking(crate::portal::PortalSession::open).await.unwrap().unwrap();
        let mut runner = Runner::start(Arc::new(session), VideoCodec::H265);
        let mut decoder = tokio::process::Command::new("ffmpeg").args([
            "-hide_banner", "-loglevel", "error", "-nostats", "-threads", "2", "-f", "hevc", "-i", "pipe:0",
            "-progress", "pipe:1", "-fps_mode", "passthrough", "-f", "null", "/dev/null",
        ]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).spawn().unwrap();
        let mut input = decoder.stdin.take().unwrap();
        let mut errors = decoder.stderr.take().unwrap();
        let error_reader = tokio::spawn(async move {
            let mut total = 0usize;
            let mut buffer = [0; 2048];
            loop {
                let count = errors.read(&mut buffer).await.unwrap();
                if count == 0 { return total; }
                total += count;
            }
        });
        let output = decoder.stdout.take().unwrap();
        let progress_reader = tokio::spawn(async move {
            let mut lines = BufReader::new(output).lines();
            let mut frames = 0u64;
            while let Some(line) = lines.next_line().await.unwrap() {
                if let Some(count) = line.strip_prefix("frame=") { frames = count.trim().parse().unwrap(); }
            }
            frames
        });
        let ticks = || {
            let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
            let fields: Vec<_> = stat.split_once(") ").unwrap().1.split_whitespace().collect();
            fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
        };
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
        let started = Instant::now();
        let mut last_report = started;
        let mut last_ticks = ticks();
        let mut frames = 0u64;
        while started.elapsed() < Duration::from_secs(300) {
            let data = runner.frame().await;
            tokio::time::timeout(Duration::from_secs(5), input.write_all(&data)).await
                .expect("verification decoder stopped consuming").unwrap();
            frames += 1;
            if last_report.elapsed() >= Duration::from_secs(30) {
                let now_ticks = ticks();
                let cpu = (now_ticks - last_ticks) as f64 / hz / last_report.elapsed().as_secs_f64() * 100.0;
                let (fds, rss_kb) = resources();
                println!("endurance elapsed_s={} encoded_frames={} cpu_percent={cpu:.2} fds={fds} rss_kb={rss_kb}",
                    started.elapsed().as_secs(), frames);
                last_ticks = now_ticks; last_report = Instant::now();
            }
        }
        runner.finish().await;
        drop(input);
        assert!(tokio::time::timeout(Duration::from_secs(15), decoder.wait()).await.unwrap().unwrap().success());
        assert_eq!(error_reader.await.unwrap(), 0, "verification decoder reported errors");
        let decoded = progress_reader.await.unwrap();
        assert_eq!(decoded, frames, "decoder lost encoded frames");
        println!("portal_endurance_passed duration_s={:.2} encoded_frames={} decoded_frames={} decode_errors=0",
            started.elapsed().as_secs_f64(), frames, decoded);
    }

    // RUST_LOG=info cargo test --release --locked --features wayland,portal,inproc-av portal_restart_resources_and_injected_lock_recovery -- --ignored --nocapture
    // Recreate the Portal session 20 times, check resource growth, then inject
    // locked/unknown/unlocked observations to check healthy-frame passthrough.
    // Actually close/restore the source to check placeholder black and recovery.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires an unlocked Portal desktop, saved consent, VAAPI and FFmpeg decode verification"]
    async fn portal_restart_resources_and_injected_lock_recovery() {
        let _ = env_logger::builder().is_test(true).try_init();
        let mut session = tokio::task::spawn_blocking(crate::portal::PortalSession::open).await.unwrap().unwrap();
        // Inject only the lock observation; actual session closure/recovery and GPU encoding remain real.
        // This does not lock the user's desktop or bypass the isolated-VM acceptance guard.
        let (locked, watch) = tokio::sync::watch::channel(Some(false));
        session.test_lock_watch(watch);
        let portal = Arc::new(session);
        let mut snapshots = Vec::new();
        for cycle in 0..20 {
            let codec = if cycle % 2 == 0 { VideoCodec::H264 } else { VideoCodec::H265 };
            let mut runner = Runner::start(portal.clone(), codec);
            let frame = runner.frame().await;
            assert!(!decoded_is_black(&frame, codec), "restarted stream stayed black");
            runner.finish().await;
            reopen(portal.clone()).await;
            let (fds, rss_kb) = resources();
            snapshots.push((fds, rss_kb));
            println!("restart_cycle={} fds={} rss_kb={}", cycle + 1, fds, rss_kb);
        }
        let warm = snapshots[3];
        let last = *snapshots.last().unwrap();
        assert!(last.0 <= warm.0 + 2, "file descriptors grew across reconnects");
        assert!(last.1 <= warm.1 + 32 * 1024, "resident memory grew across reconnects");

        let mut runner = Runner::start(portal.clone(), VideoCodec::H265);
        assert!(!decoded_is_black(&runner.hevc_idr().await, VideoCodec::H265));
        for state in [Some(true), None, Some(false)] {
            locked.send(state).unwrap();
            portal.pause();
            runner.idr.store(true, Ordering::Release);
            assert!(!decoded_is_black(&runner.hevc_idr().await, VideoCodec::H265),
                "healthy DMA source was blanked for lock state {state:?}");
            println!("portal_lock_state={state:?} valid_frame=true");
        }
        for cycle in 0..3 {
            locked.send(Some(true)).unwrap();
            portal.end();
            runner.idr.store(true, Ordering::Release);
            assert!(decoded_is_black(&runner.hevc_idr().await, VideoCodec::H265), "closed Portal source did not produce placeholder frame");
            locked.send(Some(false)).unwrap();
            let session = portal.clone();
            tokio::task::spawn_blocking(move || { session.recover(&AtomicBool::new(false)).unwrap(); }).await.unwrap();
            runner.idr.store(true, Ordering::Release);
            assert!(!decoded_is_black(&runner.hevc_idr().await, VideoCodec::H265), "desktop did not recover after unlock observation");
            println!("injected_lock_cycle={} source_closed=true placeholder_black=true same_arc_recovered=true", cycle + 1);
        }
        runner.finish().await;
        println!("portal_restart_resources_passed cycles=20 fd_growth={} rss_growth_kb={} injected_lock_cycles=3",
            last.0 as i64 - warm.0 as i64, last.1 as i64 - warm.1 as i64);
    }
}

#[cfg(all(test, target_os = "linux", feature = "portal"))]
mod native_gpu_entry_tests {
    use super::*;
    use std::io::Write;
    // Explicit synthetic hardware test only. No Portal session or desktop access.
    #[test]
    #[ignore = "requires NVIDIA driver and ffmpeg; creates only synthetic GPU pixels"]
    fn rust_native_gpu_encode_and_software_decode() {
        unsafe {
            let lib = libc::dlopen(c"libcuda.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            assert!(!lib.is_null());
            let fill = libc::dlsym(lib, c"cuMemsetD8_v2".as_ptr());
            assert!(!fill.is_null());
            let fill: unsafe extern "C" fn(u64, u8, usize) -> i32 = std::mem::transmute(fill);
            let cfg = NvencConfigFFI { width:256, height:256, fps:30, gop:30, cq:23,
                target_bps:2_000_000, max_bps:2_000_000, buffer_bits:2_000_000, is_hevc:0, chroma_444:0 };
            let mut engine = SafeNvencEngine(nvenc_create(&cfg)); assert!(!engine.0.is_null());
            let mut encoded = Vec::new();
            for i in 0..8 {
                let result = engine.encode_gpu(i == 0, |dst, pitch| {
                    let status = fill(dst, if i % 2 == 0 { 0 } else { 255 }, pitch as usize * 256);
                    if status == 0 { Ok(()) } else { Err(status) }
                });
                match result {
                    crate::portal::GpuFrameResult::Complete(Ok(bytes)) => encoded.extend(bytes),
                    _ => panic!("native GPU encoding failed"),
                }
            }
            drop(engine);
            // Driver libraries intentionally remain loaded for driver threads.
            let mut child = std::process::Command::new("ffmpeg")
                .args(["-v","error","-f","h264","-i","pipe:0","-f","rawvideo","-pix_fmt","gray","pipe:1"])
                .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped()).spawn().unwrap();
            child.stdin.take().unwrap().write_all(&encoded).unwrap();
            let output = child.wait_with_output().unwrap(); assert!(output.status.success());
            let n = 256 * 256; assert_eq!(output.stdout.len(), n * 8);
            for (i, pixels) in output.stdout.chunks_exact(n).enumerate() {
                let mean = pixels.iter().map(|v| f64::from(*v)).sum::<f64>() / n as f64;
                assert!((mean - if i % 2 == 0 { 0.0 } else { 255.0 }).abs() <= 5.0);
            }
            println!("PASS: Rust GPU callback -> native NVENC -> software decode, 8 alternating synthetic frames");
        }
    }
}

#[cfg(all(test, target_os = "linux", feature = "portal"))]
mod rust_dmabuf_tests {
    pub(super) fn config()->super::Config {
        super::Config { portal:None,display:String::new(),input:(256,256),output:None,
            fps:30,gop:30,cq:23,target_bps:2_000_000,max_bps:2_000_000,buffer_bits:2_000_000,
            codec:crate::capture_setting::VideoCodec::H264,chroma:crate::capture_setting::CHROMA_420,vaapi:false }
    }
    #[test]
    fn portal_worker_configuration_rejects_unimplemented_transforms() {
        let mut cfg=config();
        cfg.output=Some((128,128)); assert!(matches!(super::portal_nvenc_config(&cfg),Err(code) if code == -libc::ENOTSUP));
        let non_drm=std::fs::File::open("/dev/null").unwrap();
        assert_eq!(PortalNvencFrameWorker::open(&cfg,non_drm.as_fd(),
            crate::portal::ExplicitProtocol::ReleasePoint).err(),Some(-libc::ENOTSUP));
        cfg.output=None; cfg.input=(255,256); assert!(matches!(super::portal_nvenc_config(&cfg),Err(code) if code == -libc::EINVAL));
        cfg.input=(256,0); assert!(super::portal_nvenc_config(&cfg).is_err());
        cfg.input=(256,256); cfg.vaapi=true; assert!(super::portal_nvenc_config(&cfg).is_err());
        cfg.vaapi=false; cfg.fps=0; assert!(super::portal_nvenc_config(&cfg).is_err());
        cfg.fps=30; cfg.chroma=999; assert!(super::portal_nvenc_config(&cfg).is_err());
        cfg.chroma=crate::capture_setting::CHROMA_444; cfg.codec=crate::capture_setting::VideoCodec::H265;
        cfg.output=Some(cfg.input);
        let ffi=super::portal_nvenc_config(&cfg).unwrap();
        assert_eq!((ffi.width,ffi.height,ffi.is_hevc,ffi.chroma_444),(256,256,1,1));
        assert_eq!((ffi.fps,ffi.gop,ffi.cq,ffi.target_bps,ffi.max_bps,ffi.buffer_bits),
            (cfg.fps,cfg.gop,cfg.cq,cfg.target_bps,cfg.max_bps,cfg.buffer_bits));
    }
    use super::*;
    use std::{ffi::c_void, os::fd::{AsFd, AsRawFd, FromRawFd}, io::Write};
    extern "C" {
        fn drmSyncobjCreate(fd:i32,flags:u32,handle:*mut u32)->i32;
        fn drmSyncobjHandleToFD(fd:i32,handle:u32,out:*mut i32)->i32;
        fn drmSyncobjTimelineSignal(fd:i32,handles:*const u32,points:*const u64,count:u32)->i32;
        fn drmSyncobjQuery(fd:i32,handles:*const u32,points:*mut u64,count:u32)->i32;
        fn drmSyncobjDestroy(fd:i32,handle:u32)->i32;
        fn gbm_create_device(fd:i32) -> *mut c_void;
        fn gbm_device_destroy(device:*mut c_void);
        fn gbm_bo_create(device:*mut c_void,w:u32,h:u32,format:u32,flags:u32) -> *mut c_void;
        fn gbm_bo_get_fd(bo:*mut c_void) -> i32;
        fn gbm_bo_get_stride(bo:*mut c_void) -> u32;
        fn gbm_bo_get_modifier(bo:*mut c_void) -> u64;
        fn gbm_bo_destroy(bo:*mut c_void);
    }
    #[test]
    #[ignore = "requires NVIDIA render node; self-created unsignaled syncobj, no desktop capture"]
    fn rust_worker_cancel_pending_acquire() {
        use std::sync::atomic::Ordering::SeqCst;
        unsafe {
          for task_stop in [false,true] {
            let path=std::env::var_os("UU_TEST_RENDER_NODE").unwrap_or_else(|| "/dev/dri/renderD128".into());
            let fd=std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
            let worker=PortalNvencFrameWorker::open(&config(),fd.as_fd(),
                crate::portal::ExplicitProtocol::ReleasePoint).unwrap();
            let mut handle=0; let mut raw=-1;
            assert_eq!(drmSyncobjCreate(fd.as_raw_fd(),0,&mut handle),0);
            assert_eq!(drmSyncobjHandleToFD(fd.as_raw_fd(),handle,&mut raw),0);
            let sync=std::os::fd::OwnedFd::from_raw_fd(raw);
            // Deliberately invalid image: reaching image import must fail rather
            // than silently passing this cancellation check.
            let snapshot=crate::portal::AsyncFrame { pixels:crate::portal::DmaFrame {fd:-1,width:256,height:256,..Default::default()},
                acquire_fd:sync.as_raw_fd(),release_fd:sync.as_raw_fd(),acquire_point:1,release_point:2,epoch:0 };
            let cancel=Arc::new(crate::portal::SyntheticCancellation::default());
            let lease=crate::portal::synthetic_lease_cancellable(snapshot,0,cancel.clone());
            let stopped=Arc::new(AtomicBool::new(false));
            let trigger_stop=stopped.clone();
            let trigger=std::thread::spawn(move || {
                let deadline=Instant::now()+Duration::from_secs(3);
                // Dispatcher polls once; the wait polls before and after its
                // first kernel wait slice. Never cancel only by elapsed sleep.
                while cancel.polls.load(SeqCst)<3 {
                    assert!(Instant::now()<deadline,"worker never entered acquire wait");
                    std::thread::sleep(Duration::from_millis(1));
                }
                let sent=Instant::now();
                if task_stop { trigger_stop.store(true,SeqCst); }
                else { cancel.cancelled.store(true,SeqCst); }
                sent
            });
            let outcome=worker.run_cancelable(lease,true,|| stopped.load(SeqCst));
            let returned=Instant::now(); let sent=trigger.join().unwrap();
            assert!(returned.duration_since(sent)<Duration::from_millis(500));
            match outcome {
                crate::portal::WorkerFrame::NeedsTeardown { backend,lease,code } => {
                    assert_eq!(code,-libc::ECANCELED); assert!(backend.timeline.is_some());
                    assert_eq!(lease.cancelled(),!task_stop);
                    // This test exclusively owns the synthetic producer; no
                    // signal submissions or producer buffer reuse can exist.
                    let recovery=backend.retire_unsubmitted_then(lease,|| {
                        // A new context can acquire the process guard only after
                        // old local imports and the worker were retired.
                        let fresh=crate::portal::AsyncFrame { acquire_fd:sync.as_raw_fd(),
                            release_fd:sync.as_raw_fd(),acquire_point:1,release_point:2,..Default::default() };
                        let mut timeline=crate::portal_timeline::Timeline::new_protocol(fd.as_fd(),&fresh,
                            crate::portal::ExplicitProtocol::ReleasePoint).unwrap();
                        timeline.discard_unsubmitted().unwrap();
                        Err::<bool,i32>(-libc::EACCES) // injected fresh-session refusal
                    });
                    assert!(matches!(recovery,Ok(Err(code)) if code == -libc::EACCES));
                }
                _=>panic!("pending acquire must retain teardown ownership"),
            }
            let mut point=0;
            assert_eq!(drmSyncobjQuery(fd.as_raw_fd(),&handle,&mut point,1),0); assert_eq!(point,0);
            let worker=PortalNvencFrameWorker::open(&config(),fd.as_fd(),
                crate::portal::ExplicitProtocol::ReleasePoint).unwrap();
            let changed=crate::portal::AsyncFrame { pixels:crate::portal::DmaFrame {
                fd:-1,width:512,height:256,..Default::default() },
                acquire_fd:-1,release_fd:-1,..Default::default() };
            match worker.run(crate::portal::synthetic_lease(changed,0),true) {
                crate::portal::WorkerFrame::NeedsTeardown { backend,lease,code }=>{
                    assert_eq!(code,-libc::ESTALE); assert!(backend.timeline.is_none());
                    assert!(matches!(backend.retire_unsubmitted_then(lease,|| Ok::<bool,i32>(false)),Ok(Ok(false))));
                }
                _=>panic!("changed dimensions must fail before timeline import"),
            }
            // Exercise the recovery gate while the timeline records active work.
            // This synthetic case submits no GPU reads; complete below is valid.
            let mut worker=PortalNvencFrameWorker::open(&config(),fd.as_fd(),
                crate::portal::ExplicitProtocol::ReleasePoint).unwrap();
            let acquire=1u64;
            assert_eq!(drmSyncobjTimelineSignal(fd.as_raw_fd(),&handle,&acquire,1),0);
            let snapshot=crate::portal::AsyncFrame { acquire_fd:sync.as_raw_fd(),
                release_fd:sync.as_raw_fd(),acquire_point:1,release_point:2,..Default::default() };
            let mut timeline=crate::portal_timeline::Timeline::new_protocol(fd.as_fd(),&snapshot,
                crate::portal::ExplicitProtocol::ReleasePoint).unwrap();
            timeline.wait(Duration::from_secs(1),&|| false).unwrap();
            timeline.begin().unwrap();
            worker.timeline=Some(timeline); worker.source_started=true;
            let lease=crate::portal::synthetic_lease(snapshot,0);
            let (worker,lease)=match worker.retire_completed_then::<_, ()>(lease,
                || panic!("active work must not enter completed recovery")) {
                Err(crate::portal::WorkerFrame::NeedsTeardown {backend,lease,code})=>{
                    assert_eq!(code,-libc::EBUSY); (backend,lease)
                }
                _=>panic!("active work must retain completed-recovery owners"),
            };
            match worker.retire_before_release_then::<_, ()>(lease,|| panic!("active work must not open recovery")) {
                Err(crate::portal::WorkerFrame::NeedsTeardown {mut backend,lease,code}) => {
                    assert_eq!(code,-libc::EBUSY);
                    backend.timeline.as_mut().unwrap().complete().unwrap();
                    assert!(matches!(backend.retire_before_release_then(lease,|| false),Ok(false)));
                }
                _=>panic!("active work must retain owners"),
            }
            assert_eq!(drmSyncobjQuery(fd.as_raw_fd(),&handle,&mut point,1),0); assert_eq!(point,1);
            drop(sync); assert_eq!(drmSyncobjDestroy(fd.as_raw_fd(),handle),0);
            println!("PASS: changed dimensions rejected before import; worker cancellation after unsignaled acquire wait; no image import, packet or release signal; synthetic retirement");
          }
        }
    }
    #[test]
    #[ignore = "requires NVIDIA render node; worker lifecycle only, no desktop or source GPU access"]
    fn rust_worker_stream_idle_exits_without_leases() {
        let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let _entered=runtime.enter();
        let path=std::env::var_os("UU_TEST_RENDER_NODE").unwrap_or_else(|| "/dev/dri/renderD128".into());
        let fd=std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
        for mode in 0..5 {
            let worker=PortalNvencFrameWorker::open(&config(),fd.as_fd(),
                crate::portal::ExplicitProtocol::ReleasePoint).unwrap();
            let stop=Arc::new(AtomicBool::new(mode==0));
            let idr=AtomicBool::new(true);
            let budget=Arc::new(Semaphore::new(1));
            let (tx,mut rx)=mpsc::channel(1);
            if mode==1 {rx.close();}
            let mut acquisitions=0;
            let outcome=unsafe {worker.run_stream::<()>(if mode==4 {0} else {30},&tx,&stop,&idr,&budget,|| {
                acquisitions+=1;
                if mode==2 {stop.store(true,Ordering::Release); Err(-libc::EAGAIN)}
                else {Err(-libc::EIO)}
            })};
            match outcome {
                crate::portal::WorkerFrame::Complete {backend,packet}=>{
                    assert_eq!(packet,match mode {3=>Err(-libc::EIO),4=>Err(-libc::EINVAL),_=>Ok(())});
                    assert!(!backend.source_started && backend.timeline.is_none());
                    drop(backend);
                }
                _=>panic!("idle stream exit must not fabricate a lease"),
            }
            assert_eq!(acquisitions,usize::from(mode==2 || mode==3));
            assert_eq!(stop.load(Ordering::Acquire),mode==0 || mode==2);
            assert!(idr.load(Ordering::Acquire)); // No frame consumed the pending IDR.
            assert_eq!(budget.available_permits(),1);
            assert!(rx.try_recv().is_err());
        }
    }
    #[test]
    #[ignore = "requires NVIDIA render node and ffmpeg; self-created GBM pixels only"]
    fn rust_dmabuf_import_encode_destroy_decode() {
        unsafe {
            let path=std::env::var_os("UU_TEST_RENDER_NODE").unwrap_or_else(|| "/dev/dri/renderD128".into());
            let fd=std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
            let gbm=gbm_create_device(fd.as_raw_fd()); assert!(!gbm.is_null());
            // Self-created BO only. No content assertion: this driver cannot CPU-map it.
            let bo=gbm_bo_create(gbm,256,256,0x34325241,4); assert!(!bo.is_null());
            let pixel_fd=gbm_bo_get_fd(bo); assert!(pixel_fd>=0);
            let pixels=std::fs::File::from_raw_fd(pixel_fd);
            let frame=crate::portal::DmaFrame { fd:pixel_fd,fourcc:0x34325241,width:256,height:256,
                stride:gbm_bo_get_stride(bo),offset:0,size:u32::try_from(pixels.metadata().unwrap().len()).unwrap(),
                modifier:gbm_bo_get_modifier(bo) };
            let mut worker=PortalNvencFrameWorker::open(&config(),fd.as_fd(),
                crate::portal::ExplicitProtocol::ReleasePoint).unwrap();
            // A rejected preflight must leave the idle encoder usable. The
            // subsequent eight-frame encode exercises that same instance.
            let different=if worker.device.pci().to_bytes()==b"ffff:ff:1f.7" {
                c"0000:00:00.0"
            } else { c"ffff:ff:1f.7" };
            assert_eq!(nvenc_check_device(worker.encoder.0,different.as_ptr()),-libc::EXDEV);
            assert_eq!(nvenc_check_device(worker.encoder.0,c"invalid".as_ptr()),-libc::EINVAL);
            assert_eq!(nvenc_check_device(worker.encoder.0,worker.device.pci().as_ptr()),0);
            let mut handle=0; let mut sync_fd=-1;
            assert_eq!(drmSyncobjCreate(fd.as_raw_fd(),0,&mut handle),0);
            assert_eq!(drmSyncobjHandleToFD(fd.as_raw_fd(),handle,&mut sync_fd),0);
            let sync=std::os::fd::OwnedFd::from_raw_fd(sync_fd);
            let mut encoded=Vec::new();
            for i in 0..8 {
                let acquire=2*i+1; let release=acquire+1;
                assert_eq!(drmSyncobjTimelineSignal(fd.as_raw_fd(),&handle,&acquire,1),0);
                let snapshot=crate::portal::AsyncFrame {
                    pixels:crate::portal::DmaFrame { fd:frame.fd,fourcc:frame.fourcc,width:frame.width,height:frame.height,
                        stride:frame.stride,offset:frame.offset,size:frame.size,modifier:frame.modifier },
                    acquire_fd:sync.as_raw_fd(),release_fd:sync.as_raw_fd(),acquire_point:acquire,release_point:release,epoch:0 };
                let lease=crate::portal::synthetic_lease(snapshot,0);
                let budget=Arc::new(Semaphore::new(1));
                let permit=budget.clone().try_acquire_owned().unwrap();
                let output=portal_worker_output(worker.run(lease,i==0),permit,
                    crate::portal::FrameValidity::new(|| false));
                match output {
                    crate::portal::WorkerFrame::Complete { backend,packet:Ok(unit) } => {
                        worker=backend;
                        assert!(unit.validity.is_some() && !unit.validity.as_ref().unwrap().cancelled());
                        assert_eq!(budget.available_permits(),0);
                        encoded.extend_from_slice(&unit.data);
                        drop(unit); assert_eq!(budget.available_permits(),1);
                    }
                    _ => panic!("Rust leased DMA-BUF worker failed"),
                }
                let mut point=0;
                assert_eq!(drmSyncobjQuery(fd.as_raw_fd(),&handle,&mut point,1),0);
                assert_eq!(point,release); assert!(worker.timeline.is_none());
            }
            // Revoke the next lease before dispatch, after eight successful
            // releases on this same worker. Previous-frame GPU history must not
            // prevent disposal of this unsubmitted lease. Invalid fds ensure
            // that cancellation never enters timeline/image import.
            let cancelled=Arc::new(crate::portal::SyntheticCancellation::default());
            cancelled.cancelled.store(true,std::sync::atomic::Ordering::SeqCst);
            let snapshot=crate::portal::AsyncFrame { acquire_fd:-1,release_fd:-1,..Default::default() };
            let lease=crate::portal::synthetic_lease_cancellable(snapshot,0,cancelled);
            match worker.run(lease,false) {
                crate::portal::WorkerFrame::NeedsTeardown {backend,lease,code} => {
                    assert_eq!(code,-libc::ECANCELED);
                    assert!(backend.timeline.is_none());
                    assert!(backend.retire_unsubmitted(lease).is_ok(),
                        "prior completed frame must not block cancelled next-frame cleanup");
                }
                _=>panic!("cancelled next frame must retain its unsubmitted lease"),
            }
            let mut point=0;
            assert_eq!(drmSyncobjQuery(fd.as_raw_fd(),&handle,&mut point,1),0);
            assert_eq!(point,16);
            let mut worker=PortalNvencFrameWorker::open(&config(),fd.as_fd(),
                crate::portal::ExplicitProtocol::ReleasePoint).unwrap();
            // Pre-admission cancellation must neither import nor encode a frame.
            assert!(matches!(worker.encoder.encode_dmabuf(&worker.device,&frame,false,&|| true),
                crate::portal::GpuFrameResult::Complete(Err(code)) if code == -libc::ECANCELED));
            for cancel_at in [2,3] {
                let polls=std::cell::Cell::new(0);
                let result=worker.encoder.encode_dmabuf(&worker.device,&frame,true,&|| {
                    polls.set(polls.get()+1); polls.get()>=cancel_at
                });
                assert!(matches!(result,crate::portal::GpuFrameResult::Complete(Err(code)) if code == -libc::ECANCELED));
                assert_eq!(polls.get(),cancel_at);
            }
            // Cancellation must leave a known-idle encoder reusable.
            assert!(matches!(worker.encoder.encode_dmabuf(&worker.device,&frame,true,&|| false),
                crate::portal::GpuFrameResult::Complete(Ok(_))));
            // Inject incompatible release metadata after a known-completed encode.
            // All GPU work completed, but metadata validation rejects release
            // before any signal promise. Local cleanup must not signal or queue.
            let acquire=17u64;
            assert_eq!(drmSyncobjTimelineSignal(fd.as_raw_fd(),&handle,&acquire,1),0);
            let snapshot=crate::portal::AsyncFrame {
                pixels:frame, acquire_fd:sync.as_raw_fd(),release_fd:sync.as_raw_fd(),
                acquire_point:17,release_point:18,epoch:0 };
            let faults=Arc::new(crate::portal::SyntheticCancellation::default());
            faults.abandon_fail_once.store(true,std::sync::atomic::Ordering::SeqCst);
            let lease=crate::portal::synthetic_lease_cancellable(snapshot,1,faults); // worker expects flags=0
            match worker.run(lease,true) {
                crate::portal::WorkerFrame::NeedsTeardown { backend,lease,code } => {
                    assert_eq!(code,-libc::EINVAL);
                    assert!(backend.timeline.is_some());
                    match backend.retire_unsubmitted_then::<_, ()>(lease,|| panic!("must not open fallback after GPU submission")) {
                        Err(crate::portal::WorkerFrame::NeedsTeardown {backend,lease,code}) => {
                            assert_eq!(code,-libc::EBUSY);
                            let (backend,lease)=match backend.retire_before_release_then::<_, ()>(lease,
                                || panic!("failed abandonment must not open recovery")) {
                                Err(crate::portal::WorkerFrame::NeedsTeardown {backend,lease,code}) => {
                                    assert_eq!(code,-libc::EIO);
                                    assert!(backend.timeline.is_none());
                                    assert!(lease.cancelled());
                                    (backend,lease)
                                }
                                _=>panic!("abandon failure must retain worker and lease"),
                            };
                            let recovery=backend.retire_before_release_then(lease,|| {
                                // Recovery runs only after local ownership and
                                // the exclusive timeline guard have been freed.
                                let probe=crate::portal::AsyncFrame { acquire_fd:sync.as_raw_fd(),
                                    release_fd:sync.as_raw_fd(),acquire_point:17,release_point:18,..Default::default() };
                                let mut fresh=crate::portal_timeline::Timeline::new_protocol(
                                    fd.as_fd(),&probe,crate::portal::ExplicitProtocol::ReleasePoint).unwrap();
                                fresh.discard_unsubmitted().unwrap();
                                Err::<bool,i32>(-libc::EACCES)
                            });
                            assert!(matches!(recovery,Ok(Err(code)) if code == -libc::EACCES));
                        }
                        _ => panic!("post-submit retirement must still require proof"),
                    }
                }
                _ => panic!("expected known-completed teardown"),
            }
            let mut point=0;
            assert_eq!(drmSyncobjQuery(fd.as_raw_fd(),&handle,&mut point,1),0);
            assert_eq!(point,17); // Retirement must not signal the promised release.
            // Successful retirement releases the exclusive context guard.
            let probe=crate::portal::AsyncFrame { acquire_fd:sync.as_raw_fd(),release_fd:sync.as_raw_fd(),
                acquire_point:17,release_point:18,..Default::default() };
            let mut timeline=crate::portal_timeline::Timeline::new(fd.as_fd(),&probe).unwrap();
            timeline.retire_proven_session().unwrap(); drop(timeline);
            // Release succeeds, but the synthetic transport fails buffer return.
            // A missing timeline must not be treated as pre-submit disposal:
            // no packet, no retry of release, and no automatic SHM recovery.
            let worker=PortalNvencFrameWorker::open(&config(),fd.as_fd(),
                crate::portal::ExplicitProtocol::ReleasePoint).unwrap();
            let acquire=19u64;
            assert_eq!(drmSyncobjTimelineSignal(fd.as_raw_fd(),&handle,&acquire,1),0);
            let snapshot=crate::portal::AsyncFrame {
                pixels:crate::portal::DmaFrame { fd:pixel_fd,fourcc:0x34325241,width:256,height:256,
                    stride:gbm_bo_get_stride(bo),offset:0,size:u32::try_from(pixels.metadata().unwrap().len()).unwrap(),
                    modifier:gbm_bo_get_modifier(bo) },
                acquire_fd:sync.as_raw_fd(),release_fd:sync.as_raw_fd(),acquire_point:19,release_point:20,epoch:0 };
            let faults=Arc::new(crate::portal::SyntheticCancellation::default());
            faults.finish_fail_after_release_once.store(true,std::sync::atomic::Ordering::SeqCst);
            faults.abandon_fail_once.store(true,std::sync::atomic::Ordering::SeqCst);
            let lease=crate::portal::synthetic_lease_cancellable(snapshot,0,faults.clone());
            let budget=Arc::new(Semaphore::new(1));
            let permit=budget.clone().try_acquire_owned().unwrap();
            match portal_worker_output(worker.run(lease,true),permit,crate::portal::FrameValidity::new(|| false)) {
                crate::portal::WorkerFrame::NeedsTeardown {backend,lease,code} => {
                    assert_eq!(code,-libc::EIO);
                    assert_eq!(budget.available_permits(),1);
                    assert!(backend.timeline.is_none() && backend.source_started);
                    assert!(!backend.before_release_retired);
                    match backend.retire_before_release_then::<_, ()>(lease,
                        || panic!("post-release transport failure must not open SHM recovery")) {
                        Err(crate::portal::WorkerFrame::NeedsTeardown {backend,lease,code}) => {
                            assert_eq!(code,-libc::EBUSY);
                            assert!(backend.source_completed);
                            let (backend,lease)=match backend.retire_completed_then::<_, ()>(lease,
                                || panic!("failed lease disposal must not start recovery")) {
                                Err(crate::portal::WorkerFrame::NeedsTeardown {backend,lease,code})=>{
                                    assert_eq!(code,-libc::EIO); assert!(backend.retiring);
                                    (backend,lease)
                                }
                                _=>panic!("lease disposal failure must retain owners"),
                            };
                            // Local disposal is allowed by actual GPU completion,
                            // not by an assertion of producer termination.
                            assert!(matches!(backend.retire_completed_then(lease,|| {
                                Err::<bool,i32>(-libc::EACCES)
                            }),Ok(Err(code)) if code == -libc::EACCES));
                        }
                        _=>panic!("post-release failure must preserve ownership"),
                    }
                }
                _=>panic!("failed buffer return must not publish an encoded unit"),
            }
            assert_eq!(faults.release_calls.load(std::sync::atomic::Ordering::SeqCst),1);
            assert_eq!(drmSyncobjQuery(fd.as_raw_fd(),&handle,&mut point,1),0);
            assert_eq!(point,20);
            // Exercise the continuous driver, not just the per-frame entry:
            // publish one real GPU frame, then cached IDR and timed keepalive
            // while the producer supplies no further frames, then a source error.
            let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
            let entered=runtime.enter();
            let worker=PortalNvencFrameWorker::open(&config(),fd.as_fd(),
                crate::portal::ExplicitProtocol::ReleasePoint).unwrap();
            let acquire_point=21u64;
            assert_eq!(drmSyncobjTimelineSignal(fd.as_raw_fd(),&handle,&acquire_point,1),0);
            let mut snapshot=Some(crate::portal::AsyncFrame {
                pixels:crate::portal::DmaFrame { fd:pixel_fd,fourcc:0x34325241,width:256,height:256,
                    stride:gbm_bo_get_stride(bo),offset:0,size:u32::try_from(pixels.metadata().unwrap().len()).unwrap(),
                    modifier:gbm_bo_get_modifier(bo) },
                acquire_fd:sync.as_raw_fd(),release_fd:sync.as_raw_fd(),acquire_point:21,release_point:22,epoch:0 });
            let (tx,mut rx)=mpsc::channel::<EncodedUnit>(1);
            let budget=Arc::new(Semaphore::new(1));
            let stop=Arc::new(AtomicBool::new(false));
            let idr=Arc::new(AtomicBool::new(true));
            let count=Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let received_count=count.clone();
            let request_idr=idr.clone();
            let received=runtime.spawn(async move {
                let mut bytes=Vec::new(); let mut guard=None;
                for index in 0..3 {
                    let unit=rx.recv().await.expect("stream/cache frame missing");
                    assert!(!unit.data.is_empty());
                    bytes.extend_from_slice(&unit.data);
                    guard=unit.validity.clone();
                    if index==0 {request_idr.store(true,Ordering::Release);}
                    drop(unit); // release budget before the next acquisition
                    received_count.fetch_add(1,Ordering::Release);
                }
                let end=rx.recv().await;
                assert!(end.is_none());
                (guard.unwrap(),bytes)
            });
            let deadline=Instant::now()+Duration::from_secs(3);
            let outcome=worker.run_stream(30,&tx,&stop,&idr,&budget,|| {
                if Instant::now()>=deadline {return Err(-libc::ETIMEDOUT);}
                if let Some(frame)=snapshot.take() {
                    Ok((crate::portal::synthetic_lease(frame,0),crate::portal::FrameValidity::new(|| false)))
                } else if count.load(Ordering::Acquire)<3 {Err(-libc::EAGAIN)} else {Err(-libc::EIO)}
            });
            match &outcome {
                crate::portal::WorkerFrame::Complete {backend,packet}=>{
                    assert_eq!(*packet,Err(-libc::EIO));
                    assert!(backend.timeline.is_none() && !backend.source_started);
                }
                _=>panic!("source acquisition error after release must own no lease"),
            }
            assert!(matches!(settle_portal_stream(outcome,|| stop.load(Ordering::Acquire),
                |_,_| panic!("finished stream must have no retained lease"),|| {
                    assert_eq!(budget.available_permits(),1);
                    assert!(!stop.load(Ordering::Acquire));
                    7 // replacement callback only; no real Portal in hardware regression
                }),Ok(Some(7))));
            drop(tx);
            let (guard,cached)=runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(2),received).await.unwrap().unwrap()
            });
            assert!(guard.cancelled());
            // A recoverable source error retires old packets without pretending
            // the user stopped the task (which would suppress SHM recovery).
            assert!(!stop.load(Ordering::Acquire));
            assert!(!idr.load(Ordering::Acquire));
            assert_eq!(count.load(Ordering::Acquire),3);
            encoded.extend_from_slice(&cached);
            assert_eq!(budget.available_permits(),1);
            assert_eq!(drmSyncobjQuery(fd.as_raw_fd(),&handle,&mut point,1),0);
            assert_eq!(point,22);
            drop(entered); drop(runtime);
            drop(sync); assert_eq!(drmSyncobjDestroy(fd.as_raw_fd(),handle),0); drop(pixels); gbm_bo_destroy(bo); gbm_device_destroy(gbm);
            let mut child=std::process::Command::new("ffmpeg")
                .args(["-v","error","-f","h264","-i","pipe:0","-f","rawvideo","-pix_fmt","gray","pipe:1"])
                .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped()).spawn().unwrap();
            child.stdin.take().unwrap().write_all(&encoded).unwrap();
            let output=child.wait_with_output().unwrap(); assert!(output.status.success());
            assert_eq!(output.stdout.len(),11*256*256);
            println!("PASS: 11 decoded synthetic frames including cached IDR and timed keepalive; cached frames use no new lease or release; cancellation and local recovery checks passed");
        }
    }
}

/// Controlled multi-frame acceptance on the explicitly reviewed Mutter
/// release-point producer. Caller establishes that producer contract and holds
/// the exclusive native lease; this is not automatic production admission.
#[cfg(all(test, target_os = "linux", feature = "portal"))]
pub(crate) unsafe fn test_portal_worker_encode(
    source: &crate::portal::ExplicitSession, frames:usize,
    mut acquire: impl FnMut()->(crate::portal::AsyncLease<Arc<crate::portal::NativeHandle>>,OwnedSemaphorePermit),
) -> Vec<crate::portal::FrameValidity> {
    use std::os::fd::AsFd;
    assert!((1..=120).contains(&frames));
    let path=std::env::var_os("UU_TEST_RENDER_NODE").unwrap_or_else(|| "/dev/dri/renderD128".into());
    let fd=std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
    let mut worker:Option<PortalNvencFrameWorker>=None;
    let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let _entered=runtime.enter();
    let (units,mut receiver)=mpsc::channel(1);
    let stop=Arc::new(AtomicBool::new(false));
    let mut guards=Vec::new();
    let mut encoded=Vec::new();
    for index in 0..frames {
        let (lease,permit)=acquire();
        assert!(lease.belongs_to(source));
        if worker.is_none() {
            let mut cfg=rust_dmabuf_tests::config();
            cfg.input=(lease.frame().pixels.width,lease.frame().pixels.height);
            worker=Some(PortalNvencFrameWorker::open(&cfg,fd.as_fd(),crate::portal::ExplicitProtocol::ReleasePoint).unwrap());
        }
        match unsafe { worker.take().unwrap().run_portal_to_queue(lease,permit,index==0,&units,&stop) } {
            crate::portal::WorkerFrame::Complete {backend,packet}=>{
                assert!(backend.timeline.is_none() && !backend.source_started);
                worker=Some(backend);
                packet.expect("real worker publication unavailable");
            }
            crate::portal::WorkerFrame::NeedsTeardown {backend,lease,code}=>{
                if let Err(retained)=unsafe {backend.retire_completed_then(lease,|| ())} {
                    std::mem::forget(retained);
                }
                panic!("real worker encode failed: code={code}");
            }
            crate::portal::WorkerFrame::Quarantined(code)=>panic!("real worker completion unknown: code={code}"),
        };
        let unit=runtime.block_on(receiver.recv()).expect("published worker frame missing");
        let guard=unit.validity.as_ref().unwrap().clone();
        assert!(!guard.cancelled() && !unit.data.is_empty());
        // A bounded diagnostic stream, held only in memory. Drop the frame unit
        // every iteration so the one-slot production budget can be reacquired.
        assert!(encoded.len()+unit.data.len()<=64*1024*1024);
        encoded.extend_from_slice(&unit.data);
        guards.push(guard);
        drop(unit);
    }
    drop(worker);
    drop(units);
    assert!(runtime.block_on(receiver.recv()).is_none());
    assert!(guards.iter().all(|guard| !guard.cancelled()));
    test_portal_decode_frames(&encoded,frames);
    guards
}

#[cfg(all(test, target_os = "linux", feature = "portal"))]
pub(crate) fn test_portal_decode_frames(encoded:&[u8],frames:usize) {
    use std::io::Write;
    // Decode only through pipes. Neither compressed nor decoded desktop pixels
    // are persisted or printed. ffmpeg's progress reports only frame counters.
    let mut child=std::process::Command::new("ffmpeg")
        .args(["-v","error","-xerror","-f","h264","-i","pipe:0","-progress","pipe:1","-f","null","-"])
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped()).spawn().unwrap();
    let written=child.stdin.take().unwrap().write_all(encoded);
    let output=child.wait_with_output().unwrap();
    assert!(written.is_ok() && output.status.success(),"real worker bitstream decode failed");
    let progress=String::from_utf8(output.stdout).unwrap();
    assert_eq!(progress.lines().filter_map(|line| line.strip_prefix("frame="))
        .filter_map(|n| n.trim().parse::<u64>().ok()).last(),Some(frames as u64));
}

/// Real diagnostic using the same Config/run dispatch as the video task. Only
/// initial consent/lock observation and receiver transport are test adapters.
#[cfg(all(test,target_os = "linux",feature = "portal"))]
pub(crate) fn test_portal_production_run(session:Arc<crate::portal::PortalSession>,frames:usize) {
    assert!((3..=120).contains(&frames));
    let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let _entered=runtime.enter();
    let mut config=rust_dmabuf_tests::config();
    config.input=session.size(); config.portal=Some(session); config.display="wayland:portal".into();
    let budget=Arc::new(Semaphore::new(1));
    let stop=Arc::new(AtomicBool::new(false));
    let idr=Arc::new(AtomicBool::new(true));
    let (tx,mut rx)=mpsc::channel::<EncodedUnit>(1);
    let consumer_stop=stop.clone();
    let receiver=runtime.spawn(async move {
        let mut guards=Vec::new(); let mut encoded=Vec::new();
        for index in 0..frames {
            let unit=tokio::time::timeout(Duration::from_secs(10),rx.recv()).await.unwrap().unwrap();
            assert!(!unit.validity.as_ref().unwrap().cancelled());
            assert!(encoded.len()+unit.data.len()<=64*1024*1024);
            encoded.extend_from_slice(&unit.data); guards.push(unit.validity.as_ref().unwrap().clone());
            if index+1==frames {consumer_stop.store(true,Ordering::Release);}
            drop(unit);
        }
        assert!(rx.recv().await.is_none());
        (encoded,guards)
    });
    let result=run(config,tx,stop.clone(),idr,budget.clone());
    assert!(result.is_ok(),"production worker did not stop normally: {result:?}");
    let (encoded,guards)=runtime.block_on(receiver).unwrap();
    assert!(stop.load(Ordering::Acquire));
    assert_eq!(budget.available_permits(),1);
    assert!(guards.iter().all(|guard|guard.cancelled()));
    assert!(check_portal_retry().is_ok());
    test_portal_decode_frames(&encoded,frames);
}

/// Real first source frame, followed by injected EAGAIN. Exercises the actual
/// continuous loop, cached IDR/keepalive, output queue and cooperative task stop.
/// Same reviewed producer admission contract as test_portal_worker_encode.
#[cfg(all(test, target_os = "linux", feature = "portal"))]
pub(crate) unsafe fn test_portal_worker_stream(
    source:&crate::portal::ExplicitSession,input:(u32,u32),frames:usize,budget:&Arc<Semaphore>,
    live_session:Option<&crate::portal::PortalSession>,
    mut acquire:impl FnMut()->Result<crate::portal::AsyncLease<Arc<crate::portal::NativeHandle>>,i32>,
) -> Vec<crate::portal::FrameValidity> {
    use std::os::fd::AsFd;
    assert!((3..=120).contains(&frames));
    let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let _entered=runtime.enter();
    let path=std::env::var_os("UU_TEST_RENDER_NODE").unwrap_or_else(|| "/dev/dri/renderD128".into());
    let fd=std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
    let mut cfg=rust_dmabuf_tests::config(); cfg.input=input;
    let worker=PortalNvencFrameWorker::open(&cfg,fd.as_fd(),crate::portal::ExplicitProtocol::ReleasePoint).unwrap();
    let (tx,mut rx)=mpsc::channel::<EncodedUnit>(1);
    let stop=Arc::new(AtomicBool::new(false));
    let idr=Arc::new(AtomicBool::new(false));
    let consumer_stop=stop.clone(); let consumer_idr=idr.clone();
    let receiver=runtime.spawn(async move {
        let mut guards=Vec::new(); let mut encoded=Vec::new();
        for index in 0..frames {
            let unit=tokio::time::timeout(Duration::from_secs(10),rx.recv()).await.unwrap().unwrap();
            assert!(!unit.validity.as_ref().unwrap().cancelled());
            assert!(encoded.len()+unit.data.len()<=64*1024*1024);
            encoded.extend_from_slice(&unit.data);
            guards.push(unit.validity.as_ref().unwrap().clone());
            if index==0 {consumer_idr.store(true,Ordering::Release);}
            // Set stop before returning the last budget slot, preventing an
            // extra encode from racing receiver completion.
            if index+1==frames {consumer_stop.store(true,Ordering::Release);}
            drop(unit);
        }
        assert!(rx.recv().await.is_none());
        (guards,encoded)
    });
    let mut delivered=false;
    let deadline=Instant::now()+Duration::from_secs(15);
    let outcome=if let Some(session)=live_session {
        unsafe {worker.run_portal_stream(&cfg,session,source,&tx,&stop,&idr,budget)}
    } else {unsafe {worker.run_stream(cfg.fps,&tx,&stop,&idr,budget,|| {
        if Instant::now()>=deadline {return Err(-libc::ETIMEDOUT);}
        if delivered {return Err(-libc::EAGAIN);}
        let lease=acquire()?;
        assert!(lease.belongs_to(source));
        let guard=lease.publication_guard(); delivered=true;
        Ok((lease,guard))
    })}};
    drop(tx);
    match outcome {
        crate::portal::WorkerFrame::Complete {backend,packet}=>{drop(backend); assert!(packet.is_ok());}
        crate::portal::WorkerFrame::NeedsTeardown {backend,lease,code}=>{
            if let Err(retained)=unsafe{backend.retire_completed_then(lease,|| ())} {std::mem::forget(retained);}
            panic!("real cached stream failed: code={code}");
        }
        crate::portal::WorkerFrame::Quarantined(code)=>panic!("real cached stream completion unknown: code={code}"),
    }
    let (guards,encoded)=runtime.block_on(receiver).unwrap();
    assert!((delivered || live_session.is_some()) && !idr.load(Ordering::Acquire));
    assert_eq!(budget.available_permits(),1);
    assert!(guards.iter().all(|g| g.cancelled()));
    test_portal_decode_frames(&encoded,frames);
    guards
}

/// Drive the real continuous adapter to the injected post-GPU/pre-release error,
/// then use the same settlement/retirement path as the production coordinator.
#[cfg(all(test, target_os = "linux", feature = "portal"))]
pub(crate) unsafe fn test_portal_stream_recovery<R>(
    session:&crate::portal::PortalSession,source:&crate::portal::ExplicitSession,
    recover:impl FnOnce(&AtomicBool)->R) -> R {
    use std::os::fd::AsFd;
    let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let _entered=runtime.enter();
    let mut cfg=rust_dmabuf_tests::config(); cfg.input=session.size();
    let path=std::env::var_os("UU_TEST_RENDER_NODE").unwrap_or_else(|| "/dev/dri/renderD128".into());
    let fd=std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
    let mut worker=PortalNvencFrameWorker::open(&cfg,fd.as_fd(),crate::portal::ExplicitProtocol::ReleasePoint).unwrap();
    worker.fail_before_release=Some(-libc::EIO);
    let (tx,mut rx)=mpsc::channel::<EncodedUnit>(1);
    let stop=Arc::new(AtomicBool::new(false));
    let timeout_stop=stop.clone();
    let timeout=runtime.spawn(async move {
        tokio::time::sleep(Duration::from_secs(15)).await;
        timeout_stop.store(true,Ordering::Release);
    });
    let budget=Arc::new(Semaphore::new(1));
    let outcome=unsafe {worker.run_portal_stream(&cfg,session,source,&tx,&stop,&AtomicBool::new(true),&budget)};
    timeout.abort();
    let expected=matches!(&outcome,crate::portal::WorkerFrame::NeedsTeardown {backend,code,..}
        if *code == -libc::EIO && backend.source_started && backend.source_completed &&
            backend.timeline.is_some() && backend.fail_before_release.is_none());
    if !expected {
        std::mem::forget(outcome); // Unexpected completion state must retain owners.
        panic!("stream did not reach controlled completed-GPU failure");
    }
    let result=settle_portal_stream(outcome,|| stop.load(Ordering::Acquire) || tx.is_closed(),
        |backend,lease| unsafe {backend.recover_portal_completed_using(lease,source,|| ())},|| {
            assert!(!stop.load(Ordering::Acquire));
            assert_eq!(budget.available_permits(),1);
            assert!(matches!(rx.try_recv(),Err(mpsc::error::TryRecvError::Empty)));
            recover(&stop)
        });
    match result {
        Ok(Some(result))=>result,
        Ok(None)=>panic!("stream recovery unexpectedly cancelled"),
        Err(retained)=>{std::mem::forget(retained); panic!("stream cleanup retained owners");}
    }
}

/// Controlled native recovery diagnostic. With completed=false, incompatible
/// dimensions reject before import. With completed=true, caller must establish
/// the reviewed producer contract: actual encoding succeeds before an injected
/// test-only error prevents release. Never persists consent tokens.
#[cfg(all(test, target_os = "linux", feature = "portal"))]
pub(crate) unsafe fn test_portal_worker_recovery<R>(
    lease: crate::portal::AsyncLease<Arc<crate::portal::NativeHandle>>,
    source: &crate::portal::ExplicitSession, permit: OwnedSemaphorePermit,
    completed: bool, recover: impl FnOnce() -> R) -> R {
    use std::os::fd::AsFd;
    let mut cfg=rust_dmabuf_tests::config();
    if completed { cfg.input=(lease.frame().pixels.width,lease.frame().pixels.height); }
    else if (lease.frame().pixels.width,lease.frame().pixels.height)==cfg.input { cfg.input=(128,128); }
    let path=std::env::var_os("UU_TEST_RENDER_NODE").unwrap_or_else(|| "/dev/dri/renderD128".into());
    let fd=std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
    let mut worker=PortalNvencFrameWorker::open(&cfg,fd.as_fd(),crate::portal::ExplicitProtocol::ReleasePoint).unwrap();
    if completed { worker.fail_before_release=Some(-libc::EIO); }
    else { assert_ne!(worker.input,(lease.frame().pixels.width,lease.frame().pixels.height)); }
    match unsafe {worker.run_portal(lease,permit,true)} {
        crate::portal::WorkerFrame::NeedsTeardown {backend,lease,code} => {
            assert_eq!(code,if completed {-libc::EIO} else {-libc::ESTALE});
            assert_eq!(backend.source_started,completed);
            assert_eq!(backend.source_completed,completed);
            assert_eq!(backend.timeline.is_some(),completed);
            assert!(backend.fail_before_release.is_none(),"error must be injected after successful encoding");
            match unsafe {backend.recover_portal_completed_using(lease,source,recover)} {
                Ok(result)=>result,
                Err(retained)=>{
                    // A failed cleanup must not drop owners during assertion unwind.
                    std::mem::forget(retained);
                    panic!("real worker recovery cleanup failed");
                }
            }
        }
        other=>{
            std::mem::forget(other);
            panic!("real worker recovery fault must retain teardown ownership");
        }
    }
}

// The import path copies BGRA to an equally sized NVENC surface; it has no
// scaler or padding pass. Reject unsupported viewer requests before opening EGL.
#[cfg(all(target_os = "linux", feature = "portal"))]
fn portal_nvenc_config(config:&Config)->Result<NvencConfigFFI,i32> {
    if config.vaapi || config.output.is_some_and(|size| size!=config.input) {
        return Err(-libc::ENOTSUP);
    }
    let (width,height)=config.input;
    if width==0 || height==0 || width%2!=0 || height%2!=0 || config.fps==0 || config.gop==0 || config.cq>51 {
        return Err(-libc::EINVAL);
    }
    if !matches!(config.chroma, crate::capture_setting::CHROMA_420 | crate::capture_setting::CHROMA_444) {
        return Err(-libc::ENOTSUP);
    }
    Ok(NvencConfigFFI { width,height,fps:config.fps,gop:config.gop,cq:config.cq,
        target_bps:config.target_bps,max_bps:config.max_bps,buffer_bits:config.buffer_bits,
        is_hevc:i32::from(config.codec==crate::capture_setting::VideoCodec::H265),
        chroma_444:i32::from(config.chroma==crate::capture_setting::CHROMA_444) })
}

/// Experimental serialized frame worker. No automatic selection or compositor
/// admission. Construct on the owning thread only after the timeline contract is
/// independently established; Device intentionally prevents cross-thread moves.
#[cfg(all(target_os = "linux", feature = "portal"))]
struct PortalNvencFrameWorker {
    encoder: SafeNvencEngine,
    device: crate::portal_gpu::Device,
    render: std::os::fd::OwnedFd,
    protocol: crate::portal::ExplicitProtocol,
    timeline: Option<crate::portal_timeline::Timeline>,
    input: (u32,u32),
    source_started: bool,
    source_completed: bool,
    #[cfg(test)]
    fail_before_release: Option<i32>,
    retiring: bool,
    before_release_retired: bool,
}
#[cfg(all(target_os = "linux", feature = "portal"))]
fn portal_worker_output<O, B>(result: crate::portal::WorkerFrame<O, B>,
    permit: OwnedSemaphorePermit, validity: crate::portal::FrameValidity)
    -> crate::portal::WorkerFrame<O, B, EncodedUnit> {
    use crate::portal::WorkerFrame;
    match result {
        WorkerFrame::Complete { backend, packet } => {
            let packet = packet.and_then(|data| {
                if validity.cancelled() { return Err(-libc::ECANCELED); }
                Ok(EncodedUnit { data, permit, validity: Some(validity), completed_at: Instant::now() })
            });
            WorkerFrame::Complete { backend, packet }
        }
        WorkerFrame::NeedsTeardown { backend, lease, code } =>
            WorkerFrame::NeedsTeardown { backend, lease, code },
        WorkerFrame::Quarantined(code) => WorkerFrame::Quarantined(code),
    }
}
// Sending happens only after native finish has released the lease. A cancelled
// publication is therefore Complete(Err), never a teardown request or permission
// to release/return the source buffer again.
#[cfg(all(target_os = "linux", feature = "portal"))]
fn portal_worker_publish<O,B>(result:crate::portal::WorkerFrame<O,B,EncodedUnit>,
    units:&mpsc::Sender<EncodedUnit>, stop:&Arc<AtomicBool>) -> crate::portal::WorkerFrame<O,B,()> {
    use crate::portal::WorkerFrame;
    match result {
        WorkerFrame::Complete {backend,packet}=>{
            let packet=packet.and_then(|mut unit| {
                // Keep task identity after enqueue: restarting the encoder does
                // not necessarily revoke its still-live Portal source. This
                // guard travels with the unit into CompleteFrame/network checks.
                let source=unit.validity.take();
                let task=stop.clone();
                unit.validity=Some(crate::portal::FrameValidity::new(move ||
                    task.load(Ordering::Acquire) || source.as_ref().is_some_and(|v| v.cancelled())));
                if send_encoded_unit(units,unit,|| stop.load(Ordering::Acquire)) { Ok(()) }
                else { Err(-libc::ECANCELED) }
            });
            WorkerFrame::Complete {backend,packet}
        }
        WorkerFrame::NeedsTeardown {backend,lease,code}=>WorkerFrame::NeedsTeardown {backend,lease,code},
        WorkerFrame::Quarantined(code)=>WorkerFrame::Quarantined(code),
    }
}
// Resolve a finished loop on its owning thread before considering replacement.
// None means stopped; Err retains cleanup owners or the quarantine verdict.
#[cfg(all(target_os = "linux", feature = "portal"))]
fn settle_portal_stream<O,B,R>(outcome:crate::portal::WorkerFrame<O,B,()>,
    cancelled:impl Fn()->bool,
    retire:impl FnOnce(B,crate::portal::AsyncLease<O>)->Result<(),crate::portal::WorkerFrame<O,B>>,
    recover:impl FnOnce()->R) -> Result<Option<R>,crate::portal::WorkerFrame<O,B>> {
    use crate::portal::WorkerFrame;
    let failure=match outcome {
        WorkerFrame::Complete {backend,packet}=>{drop(backend); packet.err()}
        WorkerFrame::NeedsTeardown {backend,lease,code}=>{
            // Even a stopped task must dispose known-idle resources. Failure
            // retains the exact owners and must never open another session.
            retire(backend,lease)?;
            Some(code)
        }
        WorkerFrame::Quarantined(code)=>return Err(WorkerFrame::Quarantined(code)),
    };
    if failure.is_none() || failure==Some(-libc::ECANCELED) || cancelled() {return Ok(None);}
    Ok(Some(recover()))
}

#[cfg(all(target_os = "linux", feature = "portal"))]
impl PortalNvencFrameWorker {
    /// Owning-thread loop for an already admitted source. Does not negotiate or
    /// replace sessions. Complete owns no live lease; NeedsTeardown retains the
    /// exact failed lease; Quarantined must never trigger fallback/restart.
    /// Requires an entered Tokio runtime. After the first completed frame,
    /// keepalive/IDR may reuse only the encoder-owned CUDA surface.
    unsafe fn run_stream<O>(mut self, fps:u32, units:&mpsc::Sender<EncodedUnit>,
        stop:&Arc<AtomicBool>, idr:&AtomicBool, budget:&Arc<Semaphore>,
        mut next_frame:impl FnMut()->Result<(crate::portal::AsyncLease<O>,crate::portal::FrameValidity),i32>)
        -> crate::portal::WorkerFrame<O,Self,()> {
        use crate::portal::WorkerFrame;
        struct InvalidateOnExit(Arc<AtomicBool>);
        impl Drop for InvalidateOnExit {
            fn drop(&mut self) { self.0.store(true,Ordering::Release); }
        }
        // Retiring this loop invalidates its queued packets, but must not forge
        // caller cancellation: recovery after a source error still needs to run.
        let retired=Arc::new(AtomicBool::new(false));
        let _invalidate=InvalidateOnExit(retired.clone());
        if fps==0 { return WorkerFrame::Complete {backend:self,packet:Err(-libc::EINVAL)}; }
        let interval=Duration::from_nanos((1_000_000_000/u64::from(fps)).max(1));
        let mut next=Instant::now();
        let mut first=true;
        let mut seed=1;
        let mut keepalive_due=Instant::now();
        let mut source:Option<crate::portal::FrameValidity>=None;
        loop {
            let cancelled=|| stop.load(Ordering::Acquire) || units.is_closed() ||
                source.as_ref().is_some_and(|guard| guard.cancelled());
            if cancelled() { return WorkerFrame::Complete {backend:self,packet:Ok(())}; }
            let delay=next.saturating_duration_since(Instant::now());
            if !delay.is_zero() {
                std::thread::sleep(delay.min(Duration::from_millis(10)));
                continue;
            }
            let Some(permit)=acquire(budget,units,cancelled) else {
                return WorkerFrame::Complete {backend:self,packet:Ok(())};
            };
            let (frame,validity)=match next_frame() {
                Ok((lease,validity))=>{
                    source=Some(validity.clone());
                    let force=first | idr.swap(false,Ordering::AcqRel);
                    let frame=unsafe {self.run_cancelable(lease,force,
                        || stop.load(Ordering::Acquire) || units.is_closed() || validity.cancelled())};
                    (frame,validity)
                }
                Err(code) if code == -libc::EAGAIN && !first &&
                    (idr.load(Ordering::Acquire) || Instant::now()>=keepalive_due)=>{
                    let validity=source.as_ref().unwrap().clone();
                    if stop.load(Ordering::Acquire) || units.is_closed() || validity.cancelled() {
                        return WorkerFrame::Complete {backend:self,packet:Err(-libc::ECANCELED)};
                    }
                    // The previous source copy, encode, import destruction and
                    // lease return succeeded. No Portal fd/image/lease is used
                    // by this no-op copy: nvenc_encode_gpu encodes its own cu_dptr.
                    let force=idr.swap(false,Ordering::AcqRel);
                    let packet=match unsafe {self.encoder.encode_gpu(force,|_,_| Ok(()))} {
                        crate::portal::GpuFrameResult::Complete(packet)=>packet,
                        crate::portal::GpuFrameResult::Unknown(code)=>{
                            std::mem::forget(self); // native context can still be mapped/pushed
                            return WorkerFrame::Quarantined(code);
                        }
                    };
                    (WorkerFrame::Complete {backend:self,packet},validity)
                }
                Err(code) if code == -libc::EAGAIN=>{
                    drop(permit);
                    next=Instant::now()+Duration::from_millis(5);
                    continue;
                }
                Err(code)=>return WorkerFrame::Complete {backend:self,packet:Err(code)},
            };
            let generation=retired.clone();
            let validity=crate::portal::FrameValidity::new(move ||
                generation.load(Ordering::Acquire) || validity.cancelled());
            match portal_worker_publish(portal_worker_output(frame,permit,validity),units,stop) {
                WorkerFrame::Complete {backend,packet:Ok(())}=>{
                    self=backend; first=false;
                    next=(next+interval).max(Instant::now());
                    keepalive_due=Instant::now()+random_keepalive(&mut seed);
                }
                other=>return other,
            }
        }
    }

    /// Same admission contract as run_stream. Identity-checked acquisition must
    /// use this original explicit session, never the session's replacement.
    unsafe fn run_portal_stream(self,config:&Config,session:&crate::portal::PortalSession,
        source:&crate::portal::ExplicitSession,units:&mpsc::Sender<EncodedUnit>,
        stop:&Arc<AtomicBool>,idr:&AtomicBool,budget:&Arc<Semaphore>)
        -> crate::portal::WorkerFrame<Arc<crate::portal::NativeHandle>,Self,()> {
        unsafe {self.run_stream(config.fps,units,stop,idr,budget,|| {
            let lease=session.acquire_explicit(source)?;
            let validity=lease.publication_guard();
            Ok((lease,validity))
        })}
    }

    /// Serialized coordinator for an admitted source. A failed loop is fully
    /// retired before fresh SHM negotiation. Unknown completion/cleanup failure
    /// retains state and permanently disables Portal retry in this process.
    /// Only Ok(true) permits SHM fallback; Ok(false) means stopped/superseded.
    unsafe fn run_portal_stream_recover(self,config:&Config,session:&crate::portal::PortalSession,
        source:&crate::portal::ExplicitSession,units:&mpsc::Sender<EncodedUnit>,
        stop:&Arc<AtomicBool>,idr:&AtomicBool,budget:&Arc<Semaphore>)
        -> Result<bool,crate::video::EncoderError> {
        check_portal_retry()?;
        let outcome=unsafe {self.run_portal_stream(config,session,source,units,stop,idr,budget)};
        let result=settle_portal_stream(outcome,|| stop.load(Ordering::Acquire) || units.is_closed(),
            |backend,lease| unsafe {backend.recover_portal_completed_using(lease,source,|| ())},
            || unsafe {session.fallback_explicit_to_shm(source,stop)});
        portal_recovery_result(result,&PORTAL_RETRY_GATE)
    }

    /// Owning-thread frame entry through the existing cancellable output queue.
    /// Requires an entered Tokio runtime and the same source admission as run.
    /// A Complete error after publication cancellation owns no native lease.
    unsafe fn run_portal_to_queue(self,
        lease:crate::portal::AsyncLease<Arc<crate::portal::NativeHandle>>,
        permit:OwnedSemaphorePermit, force_idr:bool,
        units:&mpsc::Sender<EncodedUnit>, stop:&Arc<AtomicBool>)
        -> crate::portal::WorkerFrame<Arc<crate::portal::NativeHandle>,Self,()> {
        let validity=lease.publication_guard();
        let result=unsafe {self.run_cancelable(lease,force_idr,
            || stop.load(Ordering::Acquire) || units.is_closed())};
        portal_worker_publish(portal_worker_output(result,permit,validity),units,stop)
    }
    /// Owning-thread recovery for this worker's NeedsTeardown pair only. Caller
    /// must serialize this source pipeline (no other workers/source accesses).
    /// Accepts unsubmitted or known-completed GPU work only. Past release attempts
    /// are never retried. Never use for Quarantined or as proof of producer termination.
    /// Outer error retains cleanup owners; inner error is a fresh-session failure
    /// after the worker has been destroyed. Ok(false) means source superseded.
    unsafe fn recover_portal_completed(self,
        lease: crate::portal::AsyncLease<Arc<crate::portal::NativeHandle>>,
        session: &crate::portal::PortalSession, source: &crate::portal::ExplicitSession,
        cancelled: &AtomicBool)
        -> Result<Result<bool, crate::portal::PortalError>,
            crate::portal::WorkerFrame<Arc<crate::portal::NativeHandle>, Self>> {
        unsafe { self.recover_portal_completed_using(lease, source, || {
            // No worker or lease survives successful retirement. The existing
            // recovery method checks current identity, cancellation, close result,
            // lock state and geometry before installing the new SHM session.
            session.fallback_explicit_to_shm(source, cancelled)
        }) }
    }

    // Shared identity/retirement ordering; tests substitute only the ephemeral
    // session opener, leaving the real SHM recovery transaction intact.
    unsafe fn recover_portal_completed_using<R>(self,
        lease: crate::portal::AsyncLease<Arc<crate::portal::NativeHandle>>,
        source: &crate::portal::ExplicitSession, recover: impl FnOnce() -> R)
        -> Result<R, crate::portal::WorkerFrame<Arc<crate::portal::NativeHandle>, Self>> {
        if !lease.belongs_to(source) {
            return Err(crate::portal::WorkerFrame::NeedsTeardown {backend:self,lease,code:-libc::EINVAL});
        }
        unsafe { self.retire_completed_then(lease, recover) }
    }

    unsafe fn retire_unsubmitted_then<O, R>(self, lease: crate::portal::AsyncLease<O>,
        recover: impl FnOnce() -> R) -> Result<R, crate::portal::WorkerFrame<O, Self>> {
        unsafe { self.retire_unsubmitted(lease)?; }
        Ok(recover())
    }

    unsafe fn retire_before_release_then<O, R>(self, lease: crate::portal::AsyncLease<O>,
        recover: impl FnOnce() -> R) -> Result<R, crate::portal::WorkerFrame<O, Self>> {
        unsafe { self.retire_before_release(lease)?; }
        Ok(recover())
    }

    /// Owning-thread entry for an admitted real Portal lease. Validity comes from
    /// that lease, never from the current/replacement session. Same producer and
    /// completion preconditions as run; the budget must be acquired before lease.
    unsafe fn run_portal(self,
        lease: crate::portal::AsyncLease<Arc<crate::portal::NativeHandle>>,
        permit: OwnedSemaphorePermit, force_idr: bool)
        -> crate::portal::WorkerFrame<Arc<crate::portal::NativeHandle>, Self, EncodedUnit> {
        let validity = lease.publication_guard();
        portal_worker_output(unsafe { self.run(lease, force_idr) }, permit, validity)
    }
    /// Initializes only worker-owned resources; does not open a Portal session or
    /// admit GPU access. The unsafe run method still requires producer proof.
    fn open(config:&Config, render:std::os::fd::BorrowedFd<'_>,
        protocol:crate::portal::ExplicitProtocol)->Result<Self,i32> {
        let ffi=portal_nvenc_config(config)?;
        let render=render.try_clone_to_owned().map_err(|e| -e.raw_os_error().unwrap_or(libc::EIO))?;
        use std::os::fd::AsFd;
        let device=crate::portal_gpu::Device::open(render.as_fd())?;
        let encoder=SafeNvencEngine(unsafe { nvenc_create(&ffi) });
        if encoder.0.is_null() { return Err(-libc::EIO); }
        let matched=unsafe { nvenc_check_device(encoder.0,device.pci().as_ptr()) };
        if matched!=0 { return Err(matched); }
        Ok(Self { encoder,device,render,protocol,timeline:None,input:config.input,
            source_started:false,source_completed:false,retiring:false,before_release_retired:false,
            #[cfg(test)] fail_before_release:None })
    }
    /// Only a NeedsTeardown result from this worker, on its owning thread.
    /// No source work may have been submitted outside run. Ends the local consumer
    /// session and discards its references without signaling or requeueing. This
    /// does not assert producer termination or permit post-submit retirement.
    unsafe fn retire_unsubmitted<O>(mut self, mut lease: crate::portal::AsyncLease<O>)
        -> Result<(), crate::portal::WorkerFrame<O, Self>> {
        let result = (|| {
            if self.source_started { return Err(-libc::EBUSY); }
            if let Some(timeline)=self.timeline.as_mut() {
                unsafe { timeline.discard_unsubmitted()?; }
                self.timeline=None;
            }
            unsafe { lease.abandon_completed() }
        })();
        match result {
            Ok(()) => { drop(lease); drop(self); Ok(()) }
            Err(code) => Err(crate::portal::WorkerFrame::NeedsTeardown {backend:self,lease,code}),
        }
    }
    /// Only the known-idle NeedsTeardown result from this worker. No source access
    /// may occur outside run or after this call. Local disposal never signals or
    /// queues the lease, and is refused if release may already have been promised.
    unsafe fn retire_before_release<O>(mut self, mut lease: crate::portal::AsyncLease<O>)
        -> Result<(), crate::portal::WorkerFrame<O, Self>> {
        let result = (|| {
            match self.timeline.as_mut() {
                Some(timeline) => {
                    unsafe { timeline.discard_before_release()?; }
                    self.timeline=None;
                    // Preserve this proof if ending/abandoning the lease fails.
                    // A missing timeline alone cannot distinguish local disposal
                    // from a previous successful release.
                    self.before_release_retired=true;
                }
                None if self.source_started && !self.before_release_retired => return Err(-libc::EBUSY),
                None => {},
            }
            unsafe { lease.abandon_completed() }
        })();
        match result {
            Ok(()) => { drop(lease); drop(self); Ok(()) }
            Err(code) => Err(crate::portal::WorkerFrame::NeedsTeardown {backend:self,lease,code}),
        }
    }
    /// Known-idle NeedsTeardown only; permanently end this consumer's source use.
    /// Drops local references even after a signal/queue failure, without retrying
    /// release, requeueing or claiming that the producer has terminated.
    unsafe fn retire_completed_then<O,R>(mut self, mut lease: crate::portal::AsyncLease<O>,
        recover: impl FnOnce()->R) -> Result<R,crate::portal::WorkerFrame<O,Self>> {
        let result=(|| {
            if self.source_started && !self.source_completed { return Err(-libc::EBUSY); }
            self.retiring=true;
            if let Some(timeline)=self.timeline.as_mut() {
                unsafe { timeline.discard_completed()?; }
                self.timeline=None;
            }
            unsafe { lease.abandon_completed() }
        })();
        match result {
            Ok(())=>{ drop(lease); drop(self); Ok(recover()) }
            Err(code)=>Err(crate::portal::WorkerFrame::NeedsTeardown {backend:self,lease,code}),
        }
    }
    /// Only consume a NeedsTeardown pair from this worker, on its owning thread.
    /// Caller independently proves irrevocable termination of the ENTIRE source
    /// session, no buffer reuse and completion of ALL GPU access. A Portal Close
    /// reply, cancellation, timeout or stream removal is not that proof.
    /// Never use for Quarantined/unknown completion. Success retires the encoder;
    /// failure preserves both owners and never retries release/queue.
    unsafe fn retire_proven_session<O>(mut self, mut lease: crate::portal::AsyncLease<O>)
        -> Result<(), crate::portal::WorkerFrame<O, Self>> {
        let result = (|| {
            if let Some(timeline) = self.timeline.as_mut() {
                unsafe { timeline.retire_proven_session()?; }
                self.timeline = None;
            }
            unsafe { lease.abandon_completed() }
        })();
        match result {
            Ok(()) => { drop(lease); drop(self); Ok(()) }
            Err(code) => Err(crate::portal::WorkerFrame::NeedsTeardown { backend:self, lease, code }),
        }
    }
    /// All authority/lifetime requirements of Timeline::new_protocol apply.
    /// protocol must match the lease's original Portal session. On NeedsTeardown,
    /// do not reuse: local retirement handles unsubmitted or independently
    /// known-completed work without repeating a release attempt. It never
    /// asserts producer termination. Quarantined
    /// resources must remain retained indefinitely.
    unsafe fn run<O>(self, lease: crate::portal::AsyncLease<O>, force_idr: bool)
        -> crate::portal::WorkerFrame<O, Self> {
        unsafe { self.run_cancelable(lease,force_idr,|| false) }
    }
    /// Same admission contract as run; task stop also interrupts acquire polling.
    unsafe fn run_cancelable<O>(self,lease:crate::portal::AsyncLease<O>,force_idr:bool,
        stopped:impl Fn()->bool) -> crate::portal::WorkerFrame<O,Self> {
        use crate::portal::GpuFrameResult::{Complete, Unknown};
        use std::os::fd::AsFd;
        unsafe extern "C" fn release(data:*mut libc::c_void, metadata:*mut libc::c_void)->i32 {
            let worker=unsafe { &mut *data.cast::<PortalNvencFrameWorker>() };
            let Some(timeline)=worker.timeline.as_mut() else { return -libc::EINVAL };
            if let Err(code)=unsafe { timeline.release(metadata) } { return code; }
            // A destroy error occurs after signaling, so it must never retry release.
            if let Err(code)=timeline.destroy() { return code; }
            worker.timeline=None;
            0
        }
        let result=unsafe { crate::portal::run_explicit_frame_cancelable(lease,self,|worker,frame,cancelled| {
            // Retirement is terminal for this worker, even if lease cleanup
            // still needs a retry. Never admit another frame on that proof.
            if worker.before_release_retired || worker.retiring { return Complete(Err(-libc::ECANCELED)); }
            if worker.timeline.is_some() { return Unknown(-libc::EBUSY); }
            worker.source_started=false;
            worker.source_completed=false;
            if (frame.pixels.width,frame.pixels.height)!=worker.input {
                return Complete(Err(-libc::ESTALE));
            }
            worker.timeline=match crate::portal_timeline::Timeline::new_protocol(worker.render.as_fd(),frame,worker.protocol) {
                Ok(timeline)=>Some(timeline),
                Err(error)=>{
                    worker.timeline=error.retained;
                    // Import failures have never submitted GPU work. Keep partial
                    // native ownership available for proof-dependent retirement.
                    return Complete(Err(error.code));
                },
            };
            let timeline=worker.timeline.as_mut().unwrap();
            if let Err(code)=timeline.wait(Duration::from_secs(2),cancelled) { return Complete(Err(code)); }
            if cancelled() { return Complete(Err(-libc::ECANCELED)); }
            if let Err(code)=timeline.begin() { return Complete(Err(code)); }
            worker.source_started=true;
            let encoded=worker.encoder.encode_dmabuf(&worker.device,&frame.pixels,force_idr,cancelled);
            match encoded {
                Unknown(code)=>Unknown(code),
                Complete(packet)=>match timeline.complete() {
                    Ok(())=>{
                        worker.source_completed=true;
                        #[cfg(test)]
                        if packet.is_ok() {
                            if let Some(code)=worker.fail_before_release.take() { return Complete(Err(code)); }
                        }
                        Complete(packet)
                    },
                    Err(code)=>Unknown(code),
                },
            }
        },release,stopped) };
        match result {
            crate::portal::WorkerFrame::Complete {mut backend,packet} => {
                // Only successful native finish disposes this frame's lease.
                // Clear history here, not inside release: the latter can still
                // be followed by a queue/finish error retaining the old lease.
                // Next-frame cancellation may bypass the encode closure entirely.
                backend.source_started=false;
                backend.source_completed=false;
                crate::portal::WorkerFrame::Complete {backend,packet}
            }
            other=>other,
        }
    }
}
