//! Controller Opus -> a dedicated virtual microphone. Never record or log audio payloads.
//! Opt in with UU_MICROPHONE=1. The normal desktop-output/monitor route is not changed.
use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock, atomic::{AtomicBool, Ordering}};
use std::time::Duration;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;
use webrtc::{peer_connection::{RTCPeerConnection, peer_connection_state::RTCPeerConnectionState},
    rtp::packet::Packet, rtp_transceiver::rtp_codec::RTPCodecType, track::track_remote::TrackRemote};

mod pulse;

pub const SOURCE: &str = "uu-remote-microphone";
const SINK: &str = "uu-remote-mic-input";
const MAX_PACKETS: u16 = 12;
static AVAILABLE: AtomicBool = AtomicBool::new(false);
static ROUTE: OnceLock<Mutex<Option<Route>>> = OnceLock::new();
struct Workers { stopped: watch::Sender<bool>, active: watch::Sender<usize> }
fn workers() -> &'static Workers {
    static STATE: OnceLock<Workers> = OnceLock::new();
    STATE.get_or_init(|| Workers { stopped: watch::channel(false).0, active: watch::channel(0).0 })
}
struct Worker;
impl Worker {
    fn new() -> Self { workers().active.send_modify(|count| *count += 1); Self }
}
impl Drop for Worker {
    fn drop(&mut self) { workers().active.send_modify(|count| *count -= 1); }
}
struct Module { id: u32, kind: &'static str, args: Vec<String> }
struct Route { source: Module, sink: Module, _lock: std::fs::File }

pub fn requested() -> bool { std::env::var("UU_MICROPHONE").is_ok_and(|v| v == "1") }
pub fn available() -> bool { AVAILABLE.load(Ordering::Acquire) }
pub fn policy_request(rpc: &[crate::protocol::WireField]) -> Option<u64> {
    use crate::protocol::{decode_wire, WireValue};
    let body = rpc.iter().find_map(|f| match (f.number, &f.value) { (21, WireValue::Bytes(b)) => Some(*b), _ => None })?;
    let fields = decode_wire(body).ok()?;
    Some(fields.iter().find_map(|f| match (f.number, &f.value) { (1, WireValue::Varint(v)) => Some(*v), _ => None }).unwrap_or(0))
}
fn pactl(args: &[&str]) -> Result<String, String> {
    let out = Command::new("pactl").args(args).env("LC_ALL", "C")
        .stdin(Stdio::null()).stderr(Stdio::null()).output().map_err(|_| "pactl unavailable")?;
    if !out.status.success() { return Err("PulseAudio virtual microphone operation failed".into()); }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}
impl Module {
    fn matching_id(line: &str, kind: &str, args: &[String]) -> Option<u32> {
        let mut fields = line.split('\t');
        let id = fields.next()?.parse().ok()?;
        if fields.next()? != kind { return None; }
        let actual: Vec<_> = fields.next()?.split_whitespace().collect();
        (actual.len() == args.len() && args.iter().all(|arg| actual.contains(&arg.as_str()))).then_some(id)
    }
    fn open(kind: &'static str, args: Vec<String>) -> Result<Self, String> {
        let modules = pactl(&["list", "short", "modules"])?;
        let key = &args[0];
        let named: Vec<_> = modules.lines().filter(|line| line.split('\t').nth(2)
            .is_some_and(|a| a.split_whitespace().any(|a| a == key))).collect();
        let id = if named.is_empty() {
            let mut command = vec!["load-module", kind];
            command.extend(args.iter().map(String::as_str));
            pactl(&command)?.parse().map_err(|_| "Virtual microphone module returned no id")?
        } else if named.len() == 1 {
            Self::matching_id(named[0], kind, &args).ok_or("Virtual microphone name already belongs to another module")?
        } else {
            return Err("Virtual microphone module ownership is ambiguous".into());
        };
        Ok(Self { id, kind, args })
    }
    fn unload(&self) {
        // Audio-server restarts may reuse numeric ids; never unload an unrelated replacement.
        if pactl(&["list", "short", "modules"]).is_ok_and(|list| list.lines()
            .any(|line| Self::matching_id(line, self.kind, &self.args) == Some(self.id))) {
            let _ = pactl(&["unload-module", &self.id.to_string()]);
        }
    }
}
fn route_lock_path(source: &str) -> Result<std::path::PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    // Fixed per-user endpoints need a lock shared even by launchers with different UU_CONFIG,
    // XDG_RUNTIME_DIR or TMPDIR. Only the lock inode persists; flock releases on process exit.
    let dir = std::path::PathBuf::from(format!("/tmp/uu-remote-linux-{}", unsafe { libc::geteuid() }));
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)
        .map_err(|_| "Microphone runtime directory unavailable")?;
    let metadata = std::fs::symlink_metadata(&dir).map_err(|_| "Microphone runtime directory unavailable")?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        return Err("Microphone runtime directory is not private".into());
    }
    Ok(dir.join(format!("{source}.lock")))
}
fn route_lock(source: &str) -> Result<std::fs::File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(route_lock_path(source)?)
        .map_err(|_| "Microphone ownership lock unavailable")?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("Another host owns the virtual microphone".into());
    }
    Ok(lock)
}
fn native_pulse(info: &str) -> Result<bool, String> {
    let field = |key: &str| info.lines().find_map(|line| line.strip_prefix(key).map(str::trim));
    let name = field("Server Name:").ok_or("Microphone audio server unavailable")?;
    if name.to_ascii_lowercase().contains("pipewire") { return Ok(false); }
    if !name.eq_ignore_ascii_case("pulseaudio") { return Err("Unsupported microphone audio server".into()); }
    let major = field("Server Version:").and_then(|v| v.split('.').next()).and_then(|v| v.parse::<u32>().ok());
    if !major.is_some_and(|v| v >= 13) { return Err("Remote microphone requires PulseAudio 13 or newer".into()); }
    Ok(true)
}
fn sink_args(name: &str) -> Result<Vec<String>, String> {
    let description = crate::i18n::s("UU-远程麦克风输入", "UU-Remote-Microphone-Input");
    let mut args = vec![format!("sink_name={name}"), format!("sink_properties=device.description={description}"),
        "rate=48000".into(), "channels=2".into()];
    // PulseAudio 13+: norewinds starts the null sink with a 50 ms block instead of 2 s of
    // idle silence. PipeWire's compatible module does not accept this Pulse-only argument.
    if native_pulse(&pactl(&["info"])?)? { args.push("norewinds=1".into()); }
    Ok(args)
}
impl Route {
    fn new(sink_name: &str, source_name: &str) -> Result<Self, String> {
        let lock = route_lock(source_name)?;
        let sink = Module::open("module-null-sink", sink_args(sink_name)?)?;
        let description = crate::i18n::s("UU-远程麦克风", "UU-Remote-Microphone");
        let source = Module::open("module-remap-source", vec![format!("source_name={source_name}"),
            format!("master={sink_name}.monitor"), format!("source_properties=device.description={description}"),
            "rate=48000".into(), "channels=2".into()]);
        match source {
            Ok(source) => Ok(Route { source, sink, _lock: lock }),
            Err(e) => { sink.unload(); Err(e) }
        }
    }
}
impl Drop for Route {
    fn drop(&mut self) { self.source.unload(); self.sink.unload(); }
}
pub struct Cleanup;
impl Drop for Cleanup {
    fn drop(&mut self) {
        AVAILABLE.store(false, Ordering::Release);
        workers().stopped.send_replace(true);
        // On an unexpected unwind leave recovery to the next run rather than move a live stream
        // off its disappearing sink. Normal shutdown waits for every playback worker below.
        if *workers().active.borrow() == 0 { release_route(); }
    }
}
pub fn init() {
    if !requested() { return; }
    let mut state = ROUTE.get_or_init(|| Mutex::new(None)).lock().unwrap();
    if state.is_some() { return; }
    let result = (|| {
        let _ = Decoder::new()?;
        pulse::available()?;
        Route::new(SINK, SOURCE)
    })();
    match result {
        Ok(route) => {
            *state = Some(route);
            workers().stopped.send_replace(false);
            AVAILABLE.store(true, Ordering::Release);
            log::info!("Remote microphone virtual input ready (48 kHz stereo, pinned Pulse stream)");
        }
        Err(error) => log::warn!("Remote microphone unavailable: {error}"),
    }
}
pub async fn shutdown() {
    AVAILABLE.store(false, Ordering::Release);
    workers().stopped.send_replace(true);
    let mut active = workers().active.subscribe();
    let _ = active.wait_for(|count| *count == 0).await;
    release_route();
}
fn release_route() {
    if let Some(state) = ROUTE.get() {
        state.lock().unwrap().take();
    }
}

#[cfg(target_os = "linux")]
struct Opus {
    _handle: usize,
    create: unsafe extern "C" fn(i32, i32, *mut i32) -> *mut libc::c_void,
    destroy: unsafe extern "C" fn(*mut libc::c_void),
    decode: unsafe extern "C" fn(*mut libc::c_void, *const u8, i32, *mut i16, i32, i32) -> i32,
    samples: unsafe extern "C" fn(*const u8, i32, i32) -> i32,
}
#[cfg(target_os = "linux")]
fn opus() -> Result<&'static Opus, String> {
    static LIB: OnceLock<Option<Opus>> = OnceLock::new();
    LIB.get_or_init(|| unsafe {
        let handle = libc::dlopen(c"libopus.so.0".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if handle.is_null() { return None; }
        let names = [c"opus_decoder_create", c"opus_decoder_destroy", c"opus_decode", c"opus_packet_get_nb_samples"];
        let functions: Vec<_> = names.iter().map(|name| libc::dlsym(handle, name.as_ptr())).collect();
        if functions.iter().any(|f| f.is_null()) { libc::dlclose(handle); return None; }
        // The library stays loaded for the process lifetime; signatures match the public Opus API.
        Some(Opus { _handle: handle as usize, create: std::mem::transmute(functions[0]),
            destroy: std::mem::transmute(functions[1]), decode: std::mem::transmute(functions[2]),
            samples: std::mem::transmute(functions[3]) })
    }).as_ref().ok_or_else(|| "libopus runtime unavailable".into())
}
struct Decoder(*mut libc::c_void);
#[derive(Debug, PartialEq, Eq)]
enum Frame { Opus(Vec<u8>), Conceal(u32) }
// One task owns the decoder and all calls; no decoder state is shared concurrently.
unsafe impl Send for Decoder {}
impl Decoder {
    fn new() -> Result<Self, String> {
        #[cfg(target_os = "linux")] {
            let mut error = 0;
            let decoder = unsafe { (opus()?.create)(48000, 2, &mut error) };
            if error == 0 && !decoder.is_null() { return Ok(Self(decoder)); }
        }
        Err("Opus microphone decoder unavailable".into())
    }
    fn decode(&mut self, frame: &Frame) -> Result<(Vec<u8>, u32), String> {
        #[cfg(target_os = "linux")] {
            let (packet, size) = match frame {
                Frame::Opus(data) if valid_packet(data) => (Some(data.as_slice()), 5760),
                Frame::Conceal(samples) if (120..=5760).contains(samples) && samples % 120 == 0 => (None, *samples as i32),
                _ => return Err("Invalid microphone Opus frame".into()),
            };
            let data = packet.unwrap_or(&[]);
            let mut pcm = vec![0i16; 5760 * 2];
            let samples = unsafe { (opus()?.decode)(self.0, if packet.is_some() { data.as_ptr() } else { std::ptr::null() },
                data.len() as i32, pcm.as_mut_ptr(), size, 0) };
            if samples <= 0 { return Err("Microphone Opus decode failed".into()); }
            let bytes = pcm[..samples as usize * 2].iter().flat_map(|s| s.to_le_bytes()).collect();
            return Ok((bytes, samples as u32));
        }
        #[allow(unreachable_code)] { let _ = frame; Err("Microphone decoding requires Linux".into()) }
    }
}
impl Drop for Decoder {
    fn drop(&mut self) { #[cfg(target_os = "linux")] if let Ok(lib) = opus() { unsafe { (lib.destroy)(self.0); } } }
}
fn packet_samples(data: &[u8]) -> Option<u32> {
    if data.is_empty() || data.len() > 8192 { return None; }
    #[cfg(target_os = "linux")] {
        let samples = unsafe { (opus().ok()?.samples)(data.as_ptr(), data.len() as i32, 48000) };
        return (120..=5760).contains(&samples).then_some(samples as u32);
    }
    #[allow(unreachable_code)] None
}
fn valid_packet(data: &[u8]) -> bool { packet_samples(data).is_some() }

/// Fixed memory and bounded reordering across sequence wrap; stale backlog is discarded.
#[derive(Default)]
struct Jitter { next: Option<u16>, timestamp: Option<u32>, last_samples: Option<u32>, packets: HashMap<u16, QueuedPacket> }
struct QueuedPacket { timestamp: u32, samples: u32, payload: Vec<u8> }
impl Jitter {
    fn push(&mut self, packet: Packet) {
        let Some(samples) = packet_samples(&packet.payload) else { return; };
        let seq = packet.header.sequence_number;
        let next = *self.next.get_or_insert(seq);
        let ahead = seq.wrapping_sub(next);
        if ahead >= 0x8000 { return; }
        if ahead >= MAX_PACKETS { self.packets.clear(); self.next = Some(seq); self.timestamp = None; }
        self.packets.entry(seq).or_insert_with(|| QueuedPacket {
            timestamp: packet.header.timestamp, samples, payload: packet.payload.to_vec(),
        });
    }
    /// A future packet's RTP clock bounds the missing audio, including variable packet durations.
    fn pop(&mut self) -> Option<Frame> {
        if self.packets.is_empty() { return None; }
        let seq = self.next?;
        self.next = Some(seq.wrapping_add(1));
        if let Some(packet) = self.packets.remove(&seq) {
            self.timestamp = Some(packet.timestamp.wrapping_add(packet.samples));
            self.last_samples = Some(packet.samples);
            return Some(Frame::Opus(packet.payload));
        }
        let inferred = self.timestamp.and_then(|timestamp| {
            let (&future, packet) = self.packets.iter().min_by_key(|(n, _)| n.wrapping_sub(seq))?;
            let missing = u32::from(future.wrapping_sub(seq));
            let gap = packet.timestamp.wrapping_sub(timestamp);
            // Ignore timestamp discontinuities/DTX; do not manufacture seconds of concealment.
            (missing > 0 && (missing * 120..=missing * 5760).contains(&gap) && gap % 120 == 0)
                .then(|| gap / missing / 120 * 120)
        });
        let samples = inferred.or(self.last_samples).unwrap_or(960);
        self.timestamp = self.timestamp.map(|timestamp| timestamp.wrapping_add(samples));
        Some(Frame::Conceal(samples))
    }
}

type Writer = std::pin::Pin<Box<dyn AsyncWrite + Send>>;
struct Playback { decoder: Decoder, writer: Writer, output: Option<pulse::Output> }
impl Playback {
    fn new() -> Result<Self, String> {
        Self::at(SINK)
    }
    fn at(sink: &str) -> Result<Self, String> {
        let decoder = Decoder::new()?;
        let (writer, output) = pulse::start(sink)?;
        Ok(Self { decoder, writer: Box::pin(writer), output: Some(output) })
    }
    async fn write(&mut self, frame: &Frame) -> Result<u32, String> {
        let (bytes, samples) = self.decoder.decode(frame)?;
        match tokio::time::timeout(Duration::from_millis(200), self.writer.write_all(&bytes)).await {
            Ok(Ok(())) => {},
            Ok(Err(_)) => {
                if let Some(output) = self.output.as_mut() {
                    if let Err(error) = output.stop().await { return Err(error); }
                }
                return Err("Microphone playback ended".into());
            },
            Err(_) => return Err("Microphone playback stalled".into()),
        }
        Ok(samples)
    }
    async fn stop(&mut self) {
        if let Some(output) = self.output.as_mut() { let _ = output.stop().await; }
    }
}

type Factory = Arc<dyn Fn() -> Result<Playback, String> + Send + Sync>;
#[derive(Clone)]
pub struct Session { open: watch::Sender<bool>, owner: watch::Sender<u64>, serial: Arc<Mutex<()>>, factory: Factory }
impl Session {
    pub fn new() -> Self {
        // Do not request or accept capture until the controller explicitly enables it.
        Self { open: watch::channel(false).0, owner: watch::channel(0).0, serial: Arc::new(Mutex::new(())), factory: Arc::new(Playback::new) }
    }
    /// Official SimpleAction map: 23 microphone_opened, 24 microphone_closed.
    pub fn action(&self, action: u64) -> bool {
        self.action_with(action, || {})
    }
    /// Commit the gate and enqueue its notifications as one ordered transition across channels.
    /// The callback only queues messages; actual network sends must happen outside this lock.
    pub fn action_with(&self, action: u64, enqueue: impl FnOnce()) -> bool {
        match action {
            23 | 24 => {
                let _serial = self.serial.lock().unwrap();
                self.open.send_replace(action == 23);
                enqueue();
                log::info!("Remote microphone switch: open={}", action == 23);
                true
            },
            _ => false,
        }
    }
    fn claim_track(&self) -> (u64, watch::Receiver<u64>) {
        let owner = self.owner.subscribe();
        let mut generation = 0;
        self.owner.send_modify(|id| { *id = id.wrapping_add(1); generation = *id; });
        (generation, owner)
    }
    pub fn attach(&self, pc: &Arc<RTCPeerConnection>, active: watch::Receiver<bool>) {
        let weak = Arc::downgrade(pc);
        let session = self.clone();
        pc.on_track(Box::new(move |track, _, _| {
            let (session, active, weak) = (session.clone(), active.clone(), weak.clone());
            Box::pin(async move {
                if track.kind() != RTPCodecType::Audio { return; }
                if !track.codec().capability.mime_type.eq_ignore_ascii_case("audio/opus") {
                    log::warn!("Incoming microphone track uses an unsupported codec"); return;
                }
                log::info!("Incoming microphone Opus track received");
                if let Some(pc) = weak.upgrade() {
                    let (generation, owner) = session.claim_track();
                    let worker = Worker::new();
                    tokio::spawn(session.receive(track, pc, active, generation, owner, worker));
                }
            })
        }));
    }
    async fn receive(self, track: Arc<TrackRemote>, pc: Arc<RTCPeerConnection>, mut active: watch::Receiver<bool>,
        generation: u64, mut owner: watch::Receiver<u64>, _worker: Worker) {
        let mut stopped = workers().stopped.subscribe();
        let mut open = self.open.subscribe();
        let mut jitter = Jitter::default();
        let mut player: Option<Playback> = None;
        let mut next = tokio::time::Instant::now();
        let mut health = tokio::time::interval(Duration::from_millis(100));
        let mut played = 0u64;
        let mut lost = 0u64;
        // TrackRemote reads are not cancel safe. Keep the same future across timer and gate ticks.
        let mut pending = Box::pin(track.read_rtp());
        loop {
            // A worker can first run after its replacement has already subscribed. Check the id,
            // not only changed(): both receivers may otherwise have marked the latest id as seen.
            if *stopped.borrow() || *owner.borrow_and_update() != generation { break; }
            let allowed = *open.borrow() && *active.borrow() && pc.connection_state() == RTCPeerConnectionState::Connected;
            if !allowed {
                jitter = Jitter::default();
                if let Some(mut output) = player.take() { output.stop().await; log::info!("Remote microphone playback paused"); }
            }
            tokio::select! {
                biased;
                _ = stopped.changed() => break,
                _ = owner.changed() => break,
                change = open.changed() => {
                    if change.is_err() { break; }
                    jitter = Jitter::default();
                    if let Some(mut output) = player.take() { output.stop().await; }
                },
                change = active.changed() => if change.is_err() { break; },
                _ = health.tick() => {
                    if matches!(pc.connection_state(), RTCPeerConnectionState::Closed | RTCPeerConnectionState::Failed) { break; }
                },
                _ = tokio::time::sleep_until(next), if player.is_some() && allowed => {
                    if let Some(packet) = jitter.pop() {
                        if matches!(packet, Frame::Conceal(_)) { lost += 1; }
                        match player.as_mut().unwrap().write(&packet).await {
                            Ok(samples) => {
                                played += 1;
                                next = (next + Duration::from_secs_f64(f64::from(samples) / 48000.)).max(tokio::time::Instant::now());
                                if played == 1 || played % 250 == 0 { log::info!("Remote microphone playback: frames={played} concealed={lost}"); }
                            },
                            Err(error) => { log::warn!("{error}"); break; }
                        }
                    } else {
                        next = tokio::time::Instant::now() + Duration::from_millis(10);
                    }
                },
                received = &mut pending => {
                    let Ok((packet, _)) = received else { break; };
                    pending.set(track.read_rtp());
                    if !allowed || !valid_packet(&packet.payload) { continue; }
                    if player.is_none() {
                        match (self.factory)() {
                            Ok(output) => { player = Some(output); next = tokio::time::Instant::now() + Duration::from_millis(40); },
                            Err(error) => { log::warn!("{error}"); break; }
                        }
                    }
                    jitter.push(packet);
                },
            }
        }
        if let Some(mut output) = player { output.stop().await; }
        log::info!("Remote microphone track ended: frames={played} concealed={lost}");
    }
}

#[cfg(test)]
mod tests;
