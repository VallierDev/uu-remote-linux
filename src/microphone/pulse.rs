//! Pulse playback pinned with PA_STREAM_DONT_MOVE, supported by PulseAudio and PipeWire-pulse.
//! Load the stable public C API at runtime so opt-out builds need no Pulse development package.
use std::ffi::{CStr, CString};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, DuplexStream};
use tokio::sync::watch;

type Handle = *mut libc::c_void;
type Free = Option<unsafe extern "C" fn(Handle)>;
#[repr(C)]
struct SampleSpec { format: i32, rate: u32, channels: u8 }
#[repr(C)]
struct BufferAttr { maxlength: u32, tlength: u32, prebuf: u32, minreq: u32, fragsize: u32 }

struct Api {
    _library: usize,
    mainloop_new: unsafe extern "C" fn() -> Handle,
    mainloop_free: unsafe extern "C" fn(Handle),
    mainloop_get_api: unsafe extern "C" fn(Handle) -> Handle,
    mainloop_iterate: unsafe extern "C" fn(Handle, i32, *mut i32) -> i32,
    context_new: unsafe extern "C" fn(Handle, *const libc::c_char) -> Handle,
    context_connect: unsafe extern "C" fn(Handle, *const libc::c_char, u32, Handle) -> i32,
    context_get_state: unsafe extern "C" fn(Handle) -> i32,
    context_disconnect: unsafe extern "C" fn(Handle),
    context_unref: unsafe extern "C" fn(Handle),
    stream_new: unsafe extern "C" fn(Handle, *const libc::c_char, *const SampleSpec, Handle) -> Handle,
    stream_connect_playback: unsafe extern "C" fn(Handle, *const libc::c_char, *const BufferAttr, u32, Handle, Handle) -> i32,
    stream_get_state: unsafe extern "C" fn(Handle) -> i32,
    stream_get_device_name: unsafe extern "C" fn(Handle) -> *const libc::c_char,
    stream_writable_size: unsafe extern "C" fn(Handle) -> usize,
    stream_write: unsafe extern "C" fn(Handle, *const libc::c_void, usize, Free, i64, i32) -> i32,
    stream_disconnect: unsafe extern "C" fn(Handle) -> i32,
    stream_unref: unsafe extern "C" fn(Handle),
}
fn api() -> Result<&'static Api, String> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let library = libc::dlopen(c"libpulse.so.0".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if library.is_null() { return None; }
        macro_rules! load {
            ($name:expr, $signature:ty) => {{
                let function = libc::dlsym(library, $name.as_ptr());
                if function.is_null() { return None; }
                std::mem::transmute::<Handle, $signature>(function)
            }};
        }
        let result = (|| Some(Api {
            _library: library as usize,
            mainloop_new: load!(c"pa_mainloop_new", unsafe extern "C" fn() -> Handle),
            mainloop_free: load!(c"pa_mainloop_free", unsafe extern "C" fn(Handle)),
            mainloop_get_api: load!(c"pa_mainloop_get_api", unsafe extern "C" fn(Handle) -> Handle),
            mainloop_iterate: load!(c"pa_mainloop_iterate", unsafe extern "C" fn(Handle, i32, *mut i32) -> i32),
            context_new: load!(c"pa_context_new", unsafe extern "C" fn(Handle, *const libc::c_char) -> Handle),
            context_connect: load!(c"pa_context_connect", unsafe extern "C" fn(Handle, *const libc::c_char, u32, Handle) -> i32),
            context_get_state: load!(c"pa_context_get_state", unsafe extern "C" fn(Handle) -> i32),
            context_disconnect: load!(c"pa_context_disconnect", unsafe extern "C" fn(Handle)),
            context_unref: load!(c"pa_context_unref", unsafe extern "C" fn(Handle)),
            stream_new: load!(c"pa_stream_new", unsafe extern "C" fn(Handle, *const libc::c_char, *const SampleSpec, Handle) -> Handle),
            stream_connect_playback: load!(c"pa_stream_connect_playback", unsafe extern "C" fn(Handle, *const libc::c_char, *const BufferAttr, u32, Handle, Handle) -> i32),
            stream_get_state: load!(c"pa_stream_get_state", unsafe extern "C" fn(Handle) -> i32),
            stream_get_device_name: load!(c"pa_stream_get_device_name", unsafe extern "C" fn(Handle) -> *const libc::c_char),
            stream_writable_size: load!(c"pa_stream_writable_size", unsafe extern "C" fn(Handle) -> usize),
            stream_write: load!(c"pa_stream_write", unsafe extern "C" fn(Handle, *const libc::c_void, usize, Free, i64, i32) -> i32),
            stream_disconnect: load!(c"pa_stream_disconnect", unsafe extern "C" fn(Handle) -> i32),
            stream_unref: load!(c"pa_stream_unref", unsafe extern "C" fn(Handle)),
        }))();
        if result.is_none() { libc::dlclose(library); }
        result
    }).as_ref().ok_or_else(|| "libpulse runtime unavailable".into())
}
pub(super) fn available() -> Result<(), String> { api().map(|_| ()) }

struct Stream { api: &'static Api, mainloop: Handle, context: Handle, stream: Handle }
impl Drop for Stream {
    fn drop(&mut self) { unsafe {
        if !self.stream.is_null() { (self.api.stream_disconnect)(self.stream); (self.api.stream_unref)(self.stream); }
        if !self.context.is_null() { (self.api.context_disconnect)(self.context); (self.api.context_unref)(self.context); }
        if !self.mainloop.is_null() { (self.api.mainloop_free)(self.mainloop); }
    } }
}
impl Stream {
    fn connect(api: &'static Api, sink: &CStr, stopped: &watch::Receiver<bool>) -> Result<Self, String> { unsafe {
        let mut output = Self { api, mainloop: (api.mainloop_new)(), context: std::ptr::null_mut(), stream: std::ptr::null_mut() };
        if output.mainloop.is_null() { return Err("Microphone audio loop unavailable".into()); }
        let client = CString::new(crate::i18n::s("UU 远程麦克风", "UU Remote microphone")).unwrap();
        output.context = (api.context_new)((api.mainloop_get_api)(output.mainloop), client.as_ptr());
        // PA_CONTEXT_NOAUTOSPAWN: connecting to the configured server must not start another one.
        if output.context.is_null() || (api.context_connect)(output.context, std::ptr::null(), 1, std::ptr::null_mut()) < 0 {
            return Err("Microphone audio server unavailable".into());
        }
        let context = output.context;
        output.wait(stopped, || (api.context_get_state)(context) == 4)?; // PA_CONTEXT_READY
        let spec = SampleSpec { format: 3, rate: 48000, channels: 2 }; // PA_SAMPLE_S16LE
        let name = CString::new(crate::i18n::s("远程麦克风", "Remote-Microphone")).unwrap();
        output.stream = (api.stream_new)(context, name.as_ptr(), &spec, std::ptr::null_mut());
        let attr = BufferAttr { maxlength: u32::MAX, tlength: 5760, prebuf: u32::MAX, minreq: 1920, fragsize: u32::MAX };
        // pulse/def.h: PA_STREAM_DONT_MOVE | PA_STREAM_ADJUST_LATENCY. A disappearing target
        // must fail closed; PipeWire-only node properties cannot enforce this on PulseAudio.
        if output.stream.is_null() || (api.stream_connect_playback)(output.stream, sink.as_ptr(), &attr,
            0x0200 | 0x2000, std::ptr::null_mut(), std::ptr::null_mut()) < 0 {
            return Err("Microphone audio target unavailable".into());
        }
        let stream = output.stream;
        output.wait(stopped, || (api.stream_get_state)(stream) == 2)?; // PA_STREAM_READY
        let actual = (api.stream_get_device_name)(stream);
        if actual.is_null() || CStr::from_ptr(actual) != sink { return Err("Microphone audio target changed".into()); }
        Ok(output)
    } }
    fn pump(&mut self) -> Result<(), String> { unsafe {
        if (self.api.mainloop_iterate)(self.mainloop, 0, std::ptr::null_mut()) < 0
            || (self.api.context_get_state)(self.context) >= 5
            || (!self.stream.is_null() && (self.api.stream_get_state)(self.stream) >= 3) {
            return Err("Microphone audio connection ended".into());
        }
        Ok(())
    } }
    fn wait(&mut self, stopped: &watch::Receiver<bool>, ready: impl Fn() -> bool) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if *stopped.borrow() { return Err("Microphone playback stopped".into()); }
            self.pump()?;
            if ready() { return Ok(()); }
            if Instant::now() >= deadline { return Err("Microphone audio connection timed out".into()); }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn write(&mut self, pcm: &[u8], stopped: &watch::Receiver<bool>) -> Result<(), String> {
        if pcm.len() % 4 != 0 { return Err("Invalid microphone PCM frame".into()); }
        let deadline = Instant::now() + Duration::from_millis(200);
        let mut offset = 0;
        while offset < pcm.len() {
            if *stopped.borrow() { return Err("Microphone playback stopped".into()); }
            self.pump()?;
            let available = unsafe { (self.api.stream_writable_size)(self.stream) };
            if available == usize::MAX { return Err("Microphone audio stream ended".into()); }
            let count = available.min(pcm.len() - offset) & !3;
            if count > 0 {
                // With no free callback libpulse copies these bytes before returning.
                if unsafe { (self.api.stream_write)(self.stream, pcm[offset..].as_ptr().cast(), count, None, 0, 0) } < 0 {
                    return Err("Microphone audio write failed".into());
                }
                offset += count;
            } else {
                if Instant::now() >= deadline {
                    return Err(format!("Microphone playback stalled: remaining={} writable={available}", pcm.len() - offset));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        self.pump()
    }
}

pub(super) struct Output { stopped: watch::Sender<bool>, task: Option<tokio::task::JoinHandle<Result<(), String>>> }
impl Output {
    pub(super) async fn stop(&mut self) -> Result<(), String> {
        self.stopped.send_replace(true);
        if let Some(task) = self.task.take() {
            return task.await.map_err(|_| "Microphone audio worker aborted")?;
        }
        Ok(())
    }
}
impl Drop for Output { fn drop(&mut self) { self.stopped.send_replace(true); } }
pub(super) fn start(sink: &str) -> Result<(DuplexStream, Output), String> {
    let api = api()?;
    let sink = CString::new(sink).map_err(|_| "Invalid microphone audio target")?;
    let (writer, mut reader) = tokio::io::duplex(65536);
    let (stop, mut stopped) = watch::channel(false);
    let runtime = tokio::runtime::Handle::current();
    let task = tokio::task::spawn_blocking(move || {
        let mut output = Stream::connect(api, &sink, &stopped)?;
        let mut buffer = [0; 23040]; // maximum 120 ms stereo PCM
        loop {
            if *stopped.borrow() { return Ok(()); }
            let count = runtime.block_on(async { tokio::select! {
                biased;
                _ = stopped.changed() => Ok(0),
                result = reader.read(&mut buffer) => result,
            } }).map_err(|_| "Microphone PCM input ended")?;
            if count == 0 { return Ok(()); }
            output.write(&buffer[..count], &stopped)?;
        }
    });
    Ok((writer, Output { stopped: stop, task: Some(task) }))
}
