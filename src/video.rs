use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use webrtc::media::Sample;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::RTCPeerConnection;
use crate::capture_setting::{VideoCodec, VideoSettings};

/// Default frame rate; the viewer's 帧率 setting overrides it per connection.
pub const FPS: u64 = 60;
/// Keyframe interval. All-intra 60fps measured 30 Mbit/s on the real desktop (saturating the
/// viewer's Wi-Fi: 3-6 fps received). On a static desktop with NVENC CQ18 a 0.5s GOP measured
/// 7.7 Mbit/s and a 2s GOP 1.9 Mbit/s. A broken prediction chain freezes until the next IDR or
/// a PLI. Only the in-process encoder answers PLI/FIR with an IDR, so the child-process path
/// keeps the short 0.5s GOP and the longer 2s GOP is used only where an on-demand IDR exists.
const GOP_SECONDS_PROCESS: f64 = 0.5;
const GOP_SECONDS_IN_PROCESS: f64 = 2.0;

/// Keyframe interval in frames at the nominal rate. `UU_VIDEO_GOP_SECONDS` overrides the default for both paths.
fn gop_frames(fps: u32, in_process: bool) -> u64 {
    let default = if in_process { GOP_SECONDS_IN_PROCESS } else { GOP_SECONDS_PROCESS };
    let seconds = std::env::var("UU_VIDEO_GOP_SECONDS").ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(default);
    ((f64::from(fps) * seconds).round() as u64).max(1)
}
const MAX_ACCESS_UNIT: usize = 8 * 1024 * 1024;

fn nal_offsets_for(bytes: &[u8], kind: u8, codec: VideoCodec) -> Vec<usize> {
    let mut result = Vec::new();
    let mut i = 0;
    while i + 3 < bytes.len() {
        let prefix = if bytes[i..].starts_with(&[0, 0, 0, 1]) { 4 }
            else if bytes[i..].starts_with(&[0, 0, 1]) { 3 } else { i += 1; continue };
        let header = i + prefix;
        let matches = match codec {
            VideoCodec::H264 => header < bytes.len() && bytes[header] & 31 == kind,
            // HEVC has a two-byte header: forbidden_zero_bit=0, temporal_id_plus1 != 0.
            VideoCodec::H265 => header + 1 < bytes.len() && bytes[header] & 128 == 0
                && bytes[header + 1] & 7 != 0 && (bytes[header] >> 1) & 63 == kind,
        };
        if matches { result.push(i); }
        i += prefix;
    }
    result
}
fn aud_offsets_for(bytes: &[u8], codec: VideoCodec) -> Vec<usize> {
    nal_offsets_for(bytes, if codec == VideoCodec::H265 { 35 } else { 9 }, codec)
}
fn is_independent_frame_for(bytes: &[u8], codec: VideoCodec) -> bool {
    match codec {
        VideoCodec::H264 => !nal_offsets_for(bytes, 5, codec).is_empty() && nal_offsets_for(bytes, 1, codec).is_empty(),
        // BLA/IDR/CRA are IRAP (RFC 7798 §4.1); no leading pictures with our no-B-frame encoder.
        VideoCodec::H265 => (16..=21).any(|kind| !nal_offsets_for(bytes, kind, codec).is_empty())
            && !(0..16).any(|kind| !nal_offsets_for(bytes, kind, codec).is_empty()),
    }
}
#[cfg(test)]
fn aud_offsets(bytes: &[u8]) -> Vec<usize> { aud_offsets_for(bytes, VideoCodec::H264) }
fn is_independent_frame(bytes: &[u8]) -> bool { is_independent_frame_for(bytes, VideoCodec::H264) }

#[derive(Debug)]
struct CompleteFrame {
    codec: VideoCodec,
    sequence: u64,
    completed_at: Instant,
    data: Vec<u8>,
}

const RTP_CLOCK: u64 = 90_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Encoder { Cpu, Nvenc, Vaapi }
impl Encoder {
    pub(crate) fn requested() -> Self { Self::from_setting(std::env::var("UU_VIDEO_ENCODER").ok().as_deref(), nvidia_present()) }
    /// Unset/auto tries NVENC only with an NVIDIA driver loaded: `ffmpeg -encoders` lists h264_nvenc
    /// on any machine, so without this every stream start first ran a doomed NVENC process (#10).
    fn from_setting(value: Option<&str>, nvidia: bool) -> Self {
        match value { Some("cpu") => Self::Cpu, Some("vaapi") => Self::Vaapi, Some("nvenc") => Self::Nvenc,
            _ => if nvidia { Self::Nvenc } else { Self::Cpu } }
    }
    pub(crate) fn name_for(self, codec: VideoCodec) -> &'static str {
        match (self, codec) {
            (Self::Cpu, VideoCodec::H264) => "libx264", (Self::Cpu, VideoCodec::H265) => "libx265",
            (Self::Nvenc, VideoCodec::H264) => "h264_nvenc", (Self::Nvenc, VideoCodec::H265) => "hevc_nvenc",
            (Self::Vaapi, VideoCodec::H264) => "h264_vaapi", (Self::Vaapi, VideoCodec::H265) => "hevc_vaapi",
        }
    }
}
fn nvidia_present() -> bool {
    std::path::Path::new("/proc/driver/nvidia/version").exists() || std::path::Path::new("/dev/nvidia0").exists()
}
/// 真彩 (4:4:4) needs the FFmpeg process pipeline with NVENC or libx264 (h264_vaapi and the
/// in-process encoder are 4:2:0 only).
pub fn full_chroma_available() -> bool {
    #[cfg(feature = "inproc-av")]
    if crate::inproc_encoder::selected() { return false; }
    Encoder::requested() != Encoder::Vaapi
}

/// (-profile:v, -pix_fmt) for NVENC / libx264 / libx265: 4:4:4 (真彩) needs a 4:4:4 profile.
fn chroma_args(settings: &VideoSettings, encoder: Encoder) -> (&'static str, &'static str) {
    if settings.chroma != crate::capture_setting::CHROMA_444 { return (profile_for(settings.codec, false), "yuv420p"); }
    match (settings.codec, encoder) {
        (VideoCodec::H264, Encoder::Nvenc) => ("high444p", "yuv444p"),
        (VideoCodec::H264, _) => ("high444", "yuv444p"),
        (VideoCodec::H265, Encoder::Nvenc) => ("rext", "yuv444p"),
        (VideoCodec::H265, _) => ("main444-8", "yuv444p"),
    }
}
pub(crate) fn vaapi_device() -> String { std::env::var("UU_VAAPI_DEVICE").unwrap_or_else(|_| "/dev/dri/renderD128".into()) }

/// NV12 conversion must precede upload; scaling runs on software frames, before hwupload.
fn vaapi_filter(output: Option<(u32, u32)>) -> String {
    let scale = output.map_or(String::new(), |(w,h)| format!("scale={w}:{h}:flags=bilinear,"));
    format!("{scale}format=nv12,hwupload")
}
fn vaapi_options(settings: &VideoSettings, gop: &str) -> Vec<String> {
    let (maxrate, buffer) = rate_args(settings, Encoder::Vaapi);
    // QVBR is not uniformly available on Intel/AMD drivers. VBR + qmin sets a quality
    // floor while retaining the viewer's bitrate cap; unsupported drivers fall back to CPU.
    ["-c:v", Encoder::Vaapi.name_for(settings.codec), "-profile:v", profile_for(settings.codec, true), "-g", gop, "-bf", "0",
        "-rc_mode", "VBR", "-b:v", &settings.max_kbps().map_or("16M".into(), |k| format!("{}k",k*3/4)),
        "-maxrate", &maxrate, "-bufsize", &buffer, "-qmin", &settings.cq().to_string(), "-async_depth", "1", "-aud", "1"]
        .into_iter().map(str::to_owned).collect()
}

fn profile_for(codec: VideoCodec, vaapi: bool) -> &'static str {
    match codec { VideoCodec::H265 => "main", VideoCodec::H264 => if vaapi { "constrained_baseline" } else { "baseline" } }
}
pub(crate) fn x265_params(gop: &str) -> String {
    // Explicit no-B/no-lookahead avoids delayed frames; repeat parameter sets at each IRAP.
    format!("aud=1:repeat-headers=1:scenecut=0:keyint={gop}:min-keyint={gop}:open-gop=0:bframes=0:rc-lookahead=0:frame-threads=1:pools=4")
}
fn cpu_hevc_options(settings: &VideoSettings, gop: &str) -> Vec<String> {
    let (maxrate, buffer) = rate_args(settings, Encoder::Cpu);
    let (profile, pix_fmt) = chroma_args(settings, Encoder::Cpu);
    ["-c:v", "libx265", "-preset", "superfast", "-tune", "zerolatency", "-profile:v", profile,
        "-pix_fmt", pix_fmt, "-g", gop, "-bf", "0", "-threads", "4", "-crf", &settings.cq().to_string(),
        "-maxrate", &maxrate, "-bufsize", &buffer, "-x265-params", &x265_params(gop)]
        .into_iter().map(str::to_owned).collect()
}

fn encoder_options(settings: &VideoSettings, encoder: Encoder, gop: &str) -> Vec<String> {
    let (maxrate, bufsize) = rate_args(settings, encoder);
    let cq = settings.cq().to_string();
    let target = settings.max_kbps().map_or("16M".to_string(), |k| format!("{}k", k * 3 / 4));
    if encoder == Encoder::Vaapi { return vaapi_options(settings, gop); }
    let (profile, pix_fmt) = chroma_args(settings, encoder);
    if encoder == Encoder::Nvenc {
        return ["-c:v", encoder.name_for(settings.codec), "-preset", "p4", "-tune", "ull",
            "-profile:v", profile, "-pix_fmt", pix_fmt, "-g", gop, "-bf", "0",
            "-rc", "vbr", "-cq", &cq, "-b:v", &target, "-maxrate", &maxrate, "-bufsize", &bufsize,
            "-rc-lookahead", "0", "-zerolatency", "1", "-delay", "0", "-aud", "1"]
            .into_iter().map(str::to_owned).collect();
    }
    if settings.codec == VideoCodec::H265 { return cpu_hevc_options(settings, gop); }
    let params = format!("aud=1:repeat-headers=1:scenecut=0:keyint={gop}:min-keyint={gop}");
    let mut options = vec!["-c:v", "libx264", "-preset", "superfast", "-tune", "zerolatency",
        "-profile:v", profile, "-pix_fmt", pix_fmt, "-g", gop, "-threads", "4",
        "-maxrate", &maxrate, "-bufsize", &bufsize, "-x264-params", &params];
    // Capped CRF starves repeated 4K keyframes; use ABR for bitrate presets.
    if settings.max_kbps().is_some() { options.extend(["-b:v", &target]); }
    else { options.extend(["-crf", &cq]); }
    options.into_iter().map(str::to_owned).collect()
}

fn ffmpeg_command() -> Command {
    match crate::config::ffmpeg_root() {
        Some(root) => { let mut command = Command::new(root.join("bin/ffmpeg"));
            command.env("LD_LIBRARY_PATH", root.join("lib/x86_64-linux-gnu")); command }
        None => Command::new("ffmpeg"),
    }
}

/// A compiled-in encoder name does not prove that its driver can initialize.
/// Probe one synthetic 4:2:0 frame once per process before advertising HEVC.
pub async fn hevc_available() -> bool {
    static AVAILABLE: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();
    *AVAILABLE.get_or_init(|| async {
        let requested = Encoder::requested();
        let mut candidates = vec![requested];
        if requested != Encoder::Cpu { candidates.push(Encoder::Cpu); }
        for encoder in candidates {
            let settings = VideoSettings { codec: VideoCodec::H265, ..Default::default() };
            let mut command = ffmpeg_command();
            command.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
            if encoder == Encoder::Vaapi { command.args(["-vaapi_device", &vaapi_device()]); }
            command.args(["-f", "lavfi", "-i", "color=black:size=256x256:rate=1", "-frames:v", "1", "-an"]);
            if encoder == Encoder::Vaapi { command.args(["-vf", &vaapi_filter(None)]); }
            command.args(encoder_options(&settings, encoder, "30"));
            command.args(["-f", "hevc", "pipe:1"]).kill_on_drop(true).stderr(std::process::Stdio::null());
            if let Ok(Ok(output)) = tokio::time::timeout(Duration::from_secs(5), command.output()).await {
                if output.status.success() && is_independent_frame_for(&output.stdout, VideoCodec::H265) {
                    log::info!("HEVC encoder probe: {} ok", encoder.name_for(VideoCodec::H265));
                    return true;
                }
            }
        }
        log::info!("HEVC encoder probe: unavailable");
        false
    }).await
}

/// Round UP (+1ns float guard) so webrtc's floor(duration*90000) yields exactly `ticks`.
fn ticks_to_duration(ticks: u64) -> Duration {
    Duration::from_nanos((ticks * 1_000_000_000 + RTP_CLOCK - 1) / RTP_CLOCK + 1)
}

/// RTP timestamps follow when frames actually leave the encoder, not a nominal
/// 60fps count: a starved X11 capture would otherwise stamp 5fps as 60fps.
/// webrtc-rs stamps a sample with the running timestamp and only then advances it
/// by the sample duration, so each duration predicts the gap to the NEXT frame
/// (one nominal frame after this one). A stall costs one frame of timing error
/// and is corrected on the following frame instead of accumulating.
struct RtpTimeline { origin: Option<Instant>, stamp: u64, fps: u64 }
impl Default for RtpTimeline { fn default() -> Self { Self { origin: None, stamp: 0, fps: FPS } } }
impl RtpTimeline {
    fn duration_after(&mut self, completed_at: Instant) -> Duration {
        let origin = *self.origin.get_or_insert(completed_at);
        let real = ((completed_at.saturating_duration_since(origin).as_nanos() * u128::from(RTP_CLOCK) + 500_000_000) / 1_000_000_000) as u64;
        let advance = (real + RTP_CLOCK / self.fps).saturating_sub(self.stamp).max(1);
        self.stamp += advance;
        ticks_to_duration(advance)
    }
}

/// Frames travel through a short queue so P-frames are never silently replaced.
/// After an overflow, dependent frames are skipped until the next IDR, so the
/// viewer never receives a broken prediction chain and no backlog can build up.
struct DependencyGate {needs_idr:bool}
impl Default for DependencyGate {fn default()->Self{Self{needs_idr:true}}}
impl DependencyGate{
    fn submit(&mut self,tx:&mpsc::Sender<Arc<CompleteFrame>>,frame:Arc<CompleteFrame>){
        if self.needs_idr&&!is_independent_frame_for(&frame.data, frame.codec){return;}
        match tx.try_send(frame){
            Ok(())=>self.needs_idr=false,
            Err(mpsc::error::TrySendError::Full(_))=>self.needs_idr=true,
            Err(mpsc::error::TrySendError::Closed(_))=>{},
        }
    }
}

// Aborting the owner task drops Child, whose kill_on_drop kills only this encoder.
struct EncoderTask(tokio::task::JoinHandle<Result<(), String>>);
impl Drop for EncoderTask { fn drop(&mut self) { self.0.abort(); } }
impl EncoderTask {
    async fn restart(&mut self, future: impl std::future::Future<Output = Result<(), String>> + Send + 'static) {
        // abort() alone can leave the old producer running until its next yield.
        // Finish it before a new IDR can be followed by an old encoder's P-frame.
        self.0.abort();
        let _ = (&mut self.0).await;
        self.0 = tokio::spawn(future);
    }
}

#[cfg(feature = "inproc-av")]
struct StopOnDrop(Arc<AtomicBool>);
#[cfg(feature = "inproc-av")]
impl Drop for StopOnDrop { fn drop(&mut self) { self.0.store(true, std::sync::atomic::Ordering::Relaxed); } }

async fn read_encoder(
    size: (u32, u32), display: String, portal:Option<Arc<crate::portal::PortalSession>>,
    latest: mpsc::Sender<Arc<CompleteFrame>>,
    settings: VideoSettings, sequence: Arc<std::sync::atomic::AtomicU64>, idr: Arc<AtomicBool>,
) -> Result<(), String> {
    // The sequence is shared so a CPU fallback or settings restart continues it (skip statistics stay correct).
    let encoder = Encoder::requested();
    if encoder == Encoder::Cpu { return read_encoder_with(size,display,portal,latest,Encoder::Cpu,sequence,settings,idr).await; }
    match read_encoder_with(size,display.clone(),portal.clone(),latest.clone(),encoder,sequence.clone(),settings,idr.clone()).await {
        Ok(())=>Ok(()),
        Err(error) if !latest.is_closed()=>{
            log::warn!("{} stream unavailable; switching scoped encoder to CPU: {}",encoder.name_for(settings.codec),error);
            read_encoder_with(size,display,portal,latest,Encoder::Cpu,sequence,settings,idr).await
        },
        Err(error)=>Err(error),
    }
}

/// Capture and encode inside this process; each packet is one access unit.
#[cfg(feature = "inproc-av")]
async fn read_inprocess(
    config: crate::inproc_encoder::Config, latest: mpsc::Sender<Arc<CompleteFrame>>,
    sequence: Arc<std::sync::atomic::AtomicU64>, idr: Arc<AtomicBool>,
) -> Result<(), String> {
    let codec = config.codec;
    let (units_tx, mut units) = mpsc::channel::<Vec<u8>>(4);
    let stop = Arc::new(AtomicBool::new(false));
    // If this future is dropped (reader task aborted), the blocking encoder stops at its next packet.
    let _stop_on_drop = StopOnDrop(stop.clone());
    let job = tokio::task::spawn_blocking({
        let stop = stop.clone();
        move || crate::inproc_encoder::run(config, units_tx, stop, idr)
    });
    let mut gate = DependencyGate::default();
    while let Some(data) = units.recv().await {
        let sequence = sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        gate.submit(&latest, Arc::new(CompleteFrame { codec, sequence, completed_at: Instant::now(), data }));
        if latest.is_closed() { break; }
    }
    // Dropping the receiver also unblocks the encoder if it is waiting to send.
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    drop(units);
    job.await.map_err(|e| format!("Encoder task: {e}"))?
}

/// Rate-control arguments: constant quality capped at 30M (自动), or capped at the viewer's
/// chosen bitrate. The VBV buffer stays at most 500k so a cap never adds queueing latency.
/// VBV buffer in kbit. NVENC ull and VAAPI keep the small low-latency buffer; libx264 needs room for a
/// whole IDR frame, otherwise large desktops get near-grey keyframes that sharpen over the GOP (#8).
/// Half a second of bitrate still starved repeated 0.5 s-GOP keyframes at 3296x2036/2M; one second did not.
fn buffer_kbits(max_kbps: u32, encoder: Encoder) -> u32 {
    if encoder == Encoder::Cpu { max_kbps.clamp(500, 8000) } else { (max_kbps / 8).clamp(100, 500) }
}

fn buffer_for(settings: &VideoSettings, encoder: Encoder) -> u32 {
    if settings.codec == VideoCodec::H265 && encoder == Encoder::Cpu {
        settings.max_kbps().unwrap_or(30_000) // one second of HEVC bitrate, including high presets
    } else { buffer_kbits(settings.max_kbps().unwrap_or(30_000), encoder) }
}

fn rate_args(settings: &VideoSettings, encoder: Encoder) -> (String, String) {
    let max_kbps = settings.max_kbps().unwrap_or(30_000);
    (format!("{max_kbps}k"), format!("{}k", buffer_for(settings, encoder)))
}

async fn read_encoder_with(
    size: (u32,u32),display:String,portal:Option<Arc<crate::portal::PortalSession>>,latest:mpsc::Sender<Arc<CompleteFrame>>,encoder:Encoder,sequence:Arc<std::sync::atomic::AtomicU64>,
    settings: VideoSettings, #[cfg_attr(not(feature = "inproc-av"), allow(unused_variables))] idr: Arc<AtomicBool>,
)->Result<(),String>{
    let kms=display.starts_with("kms:");
    let wayland=kms || display.starts_with("wayland:");
    let kms_capture=if kms {
        let capture=tokio::task::spawn_blocking(crate::kms::Capture::open).await.map_err(|_|"KMS worker failed")??;
        if (capture.width,capture.height)!=size { return Err("KMS mode changed; restart host to refresh desktop geometry".into()); }
        Some(Arc::new(capture))
    } else { None };
    #[cfg(feature = "inproc-av")]
    if !wayland && portal.is_none() && crate::inproc_encoder::selected() {
        let max_kbps = settings.max_kbps().unwrap_or(30_000) as i64;
        let config = crate::inproc_encoder::Config {
            display: display.clone(), input: size, output: settings.output, fps: u32::from(settings.fps),
            gop: gop_frames(settings.fps, true) as u32, cq: settings.cq() as u32,
            target_bps: settings.max_kbps().map_or(16_000, |k| i64::from(k) * 3 / 4) * 1000,
            max_bps: max_kbps * 1000, buffer_bits: i64::from(buffer_for(&settings, encoder)) * 1000, encoder, codec: settings.codec,
            damage: crate::inproc_encoder::damage_enabled(),
        };
        log::info!("Desktop encoder: display={} source={}x{} output={:?} fps={} cq={} maxrate={}k encoder={} pipeline=in-process",
            display,size.0,size.1,settings.output,settings.fps,config.cq,max_kbps,encoder.name_for(settings.codec));
        return read_inprocess(config, latest, sequence, idr).await;
    }
    let size = format!("{}x{}", size.0, size.1);
    let fps = settings.fps.to_string();
    // Keep the IDR interval at 2 s whatever the frame rate.
    let gop = gop_frames(settings.fps, false).to_string();
    let (maxrate, _) = rate_args(&settings, encoder);
    let cq = settings.cq().to_string();
    log::info!("Desktop encoder: display={} source={} output={:?} fps={} cq={} maxrate={} encoder={} pix_fmt={} low_latency=true",display,size,settings.output,settings.fps,cq,maxrate,encoder.name_for(settings.codec),if encoder == Encoder::Vaapi { "nv12" } else { chroma_args(&settings, encoder).1 });
    // A bundled FFmpeg prefix (config ffmpeg_root / UU_FFMPEG_ROOT) or `ffmpeg` on PATH.
    let mut command = ffmpeg_command();
    command.args(["-hide_banner","-loglevel","error","-nostdin"]);
    if encoder == Encoder::Vaapi { command.args(["-vaapi_device", &vaapi_device()]); }
    if wayland {
        command.args(["-f","rawvideo","-pixel_format","bgra","-video_size",&size,
            "-framerate",&fps,"-i","pipe:0","-an"]);
    } else {
        // The viewer draws its local pointer; do not bake a second into video.
        command.args(["-f","x11grab","-draw_mouse","0","-framerate",&fps,
            "-video_size",&size,"-i",&display,"-an"]);
    }
    // 其他分辨率: scale the captured desktop (the host display mode is left untouched).
    if encoder == Encoder::Vaapi {
        command.args(["-vf", &vaapi_filter(settings.output)]);
    } else if let Some((width, height)) = settings.output {
        command.args(["-vf", &format!("scale={width}:{height}:flags=bilinear")]);
    }
    command.args(encoder_options(&settings, encoder, &gop));
    // x11grab paces against an absolute schedule; after a capture stall it grabs
    // back-to-back to "catch up" (measured 69 frames in 200ms). vfr on a 1/FPS
    // timebase drops those same-slot frames instead of encoding a burst.
    let time_base = format!("1/{fps}");
    if wayland { command.stdin(std::process::Stdio::piped()); }
    let mut child=command.args(["-fps_mode","vfr","-enc_time_base",&time_base,"-f",if settings.codec == VideoCodec::H265 { "hevc" } else { "h264" },"pipe:1"])
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn().map_err(|e| format!("Start scoped encoder: {e}"))?;
    // Scope capture to this encoder. Dropping either task kills its child.
    let _capture=if let Some(capture)=kms_capture {
        let mut sink=child.stdin.take().ok_or("Encoder stdin missing")?;
        Some(EncoderTask(tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let mut tick=tokio::time::interval(Duration::from_nanos(1_000_000_000/u64::from(settings.fps)));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let capture=capture.clone();
                let bytes=tokio::task::spawn_blocking(move || {
                    let mut bytes=vec![0;capture.width as usize*capture.height as usize*4];
                    capture.capture(&mut bytes)?; Ok::<_,String>(bytes)
                }).await.map_err(|_|"KMS worker failed")??;
                sink.write_all(&bytes).await.map_err(|_|"KMS encoder pipe failed")?;
            }
        })))
    } else if let Some(session)=portal {
        let mut sink=child.stdin.take().ok_or("Encoder stdin missing")?;
        Some(EncoderTask(tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let interval=Duration::from_nanos(1_000_000_000/u64::from(settings.fps));
            let mut tick=tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let session=session.clone();
                let frame=tokio::task::spawn_blocking(move || {
                    let mut frame=vec![0;(session.width as usize)*(session.height as usize)*4];
                    session.capture(&mut frame)?;
                    Ok::<_,String>(frame)
                }).await.map_err(|e|format!("Portal capture worker: {e}"))??;
                sink.write_all(&frame).await.map_err(|e|format!("Portal encoder pipe: {e}"))?;
            }
        })))
    } else if wayland {
        // Linux keeps the running executable inode accessible after a scoped
        // development/release replacement; current_exe may name a deleted path.
        #[cfg(target_os="linux")]
        let executable=std::path::PathBuf::from("/proc/self/exe");
        #[cfg(not(target_os="linux"))]
        let executable=std::env::current_exe().map_err(|e|format!("Capture executable: {e}"))?;
        let mut capture=wayland_capture_command(&executable,settings.fps)
            .kill_on_drop(true).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::inherit())
            .spawn().map_err(|e|format!("Start Wayland capture: {e}"))?;
        let mut source=capture.stdout.take().ok_or("Capture stdout missing")?;
        let mut sink=child.stdin.take().ok_or("Encoder stdin missing")?;
        Some(EncoderTask(tokio::spawn(async move {
            let result=tokio::io::copy(&mut source,&mut sink).await;
            drop(capture);
            result.map(|_|()).map_err(|e|format!("Wayland frame pipe: {e}"))
        })))
    } else { None };
    let mut stdout = child.stdout.take().ok_or("Encoder stdout missing")?;
    let mut pending = Vec::new();
    let mut chunk = [0u8; 16384];
    let mut gate = DependencyGate::default();
    loop {
        let count = stdout.read(&mut chunk).await.map_err(|e| format!("Encoder read: {e}"))?;
        if count == 0 { return Err("Encoder output ended".into()); }
        pending.extend_from_slice(&chunk[..count]);
        // A chunk can include the next AUD, so split before enforcing the bound.
        loop {
            let aud = aud_offsets_for(&pending, settings.codec);
            if aud.len() < 2 { break; }
            let next = aud[1];
            if next > MAX_ACCESS_UNIT { return Err("Encoder access unit exceeds limit".into()); }
            let frame: Vec<u8> = pending.drain(..next).collect();
            let sequence = sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            gate.submit(&latest, Arc::new(CompleteFrame {
                codec: settings.codec, sequence, completed_at: Instant::now(), data: frame,
            }));
            if latest.is_closed() { return Ok(()); }
        }
        if pending.len() > MAX_ACCESS_UNIT { return Err("Encoder access unit exceeds limit".into()); }
    }
}

fn wayland_capture_command(executable:&std::path::Path,fps:u32)->Command {
    let mut command=Command::new(executable);
    command.arg("wayland-capture").env("UU_WAYLAND_FPS",fps.to_string());
    command
}

fn transport_closed(pc: &RTCPeerConnection) -> bool {
    matches!(pc.connection_state(), RTCPeerConnectionState::Closed | RTCPeerConnectionState::Failed)
        || pc.sctp().state() == webrtc::sctp_transport::sctp_transport_state::RTCSctpTransportState::Closed
}

/// Exercise the real capture/encoder chain without UU auth or a network peer.
pub async fn probe(size:(u32,u32),display:String,portal:Option<Arc<crate::portal::PortalSession>>,path:Option<String>)->Result<(),String> {
    use std::io::Write;
    let mut file=match path {
        Some(path)=>Some(std::fs::OpenOptions::new().write(true).create_new(true).open(path).map_err(|e|format!("Video probe file: {e}"))?),
        None=>None,
    };
    let (tx,mut rx)=mpsc::channel(4);
    let sequence=Arc::new(std::sync::atomic::AtomicU64::new(0));
    let fps=crate::wayland::capture_fps(std::env::var("UU_PROBE_FPS").ok().as_deref())?;
    let settings=VideoSettings {fps,..VideoSettings::default()};
    let _reader=EncoderTask(tokio::spawn(read_encoder(size,display,portal,tx,settings,sequence,Arc::new(AtomicBool::new(false)))));
    let mut count=0;
    let mut bytes=0;
    // UU_PROBE_FRAMES lengthens the sample for baseline measurements; default is 5 frames.
    let wanted:u64=std::env::var("UU_PROBE_FRAMES").ok().and_then(|v|v.parse().ok()).unwrap_or(5);
    tokio::time::timeout(Duration::from_secs(15+wanted/30),async {
        while count<wanted {
            let frame=rx.recv().await.ok_or("Video probe encoder ended")?;
            if count==0 && !is_independent_frame(&frame.data) { return Err("Video probe did not begin with IDR".to_owned()); }
            if let Some(file)=file.as_mut() { file.write_all(&frame.data).map_err(|e|e.to_string())?; }
            bytes+=frame.data.len(); count+=1;
        }
        Ok::<(),String>(())
    }).await.map_err(|_|"Video probe timed out".to_owned())??;
    println!("Encoded capture probe: frames={count} h264_bytes={bytes} first_frame_idr=true");
    Ok(())
}

pub async fn stream_desktop(pc: Arc<RTCPeerConnection>, track: Arc<crate::video_track::VideoTrack>, size: (u32, u32), display: String, portal:Option<Arc<crate::portal::PortalSession>>,
    mut settings: tokio::sync::watch::Receiver<VideoSettings>, applied: Arc<tokio::sync::watch::Sender<VideoSettings>>, idr: Arc<AtomicBool>) -> Result<(), String> {
    loop {
        if transport_closed(&pc) { return Ok(()); }
        if pc.connection_state() == RTCPeerConnectionState::Connected
            && pc.sctp().state() == webrtc::sctp_transport::sctp_transport_state::RTCSctpTransportState::Connected { break; }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    crate::capture::keep_display_awake()?;
    let (latest, mut receiver) = mpsc::channel(4);
    let sequence = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut current = *settings.borrow_and_update();
    if !track.supports(current.codec) && track.supports(VideoCodec::H265) {
        current.codec = VideoCodec::H265; // H265-only remote video offer.
        applied.send_replace(current);
        settings.borrow_and_update();
    }
    let mut reader = EncoderTask(tokio::spawn(read_encoder(size, display.clone(), portal.clone(), latest.clone(), current, sequence.clone(), idr.clone())));
    let mut previous = None;
    let mut timeline = RtpTimeline { fps: u64::from(current.fps), ..Default::default() };
    let mut settings_open = true;
    let mut frames = 0u64;
    let mut skipped = 0u64;
    let mut max_age_ms = 0u128;
    let mut max_send_ms = 0u128;
    let mut health = tokio::time::interval(Duration::from_millis(200));
    health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut stats = Instant::now();
    loop {
        tokio::select! {
            result = &mut reader.0 => {
                let result = result.map_err(|e| format!("Encoder reader task: {e}"))?;
                if result.is_err() && current.codec == VideoCodec::H265 && track.supports(VideoCodec::H264) {
                    log::warn!("HEVC unavailable; falling back to negotiated H264");
                    current.codec = VideoCodec::H264;
                    applied.send_modify(|value| value.codec = VideoCodec::H264);
                    current = *settings.borrow_and_update();
                    while receiver.try_recv().is_ok() {}
                    reader.0 = tokio::spawn(read_encoder(size, display.clone(), portal.clone(), latest.clone(), current, sequence.clone(), idr.clone()));
                    continue;
                }
                return result;
            },
            _ = health.tick() => { if transport_closed(&pc) { return Ok(()); } },
            changed = settings.changed(), if settings_open => {
                if changed.is_err() { settings_open = false; continue; }
                // Restart the encoder with the viewer's new 帧率/画质; it begins with an IDR, and frames
                // still queued from the old encoder remain a valid chain before it.
                let next = *settings.borrow_and_update();
                let codec_changed = next.codec != current.codec;
                current = next;
                timeline.fps = u64::from(next.fps);
                if codec_changed {
                    reader.0.abort();
                    let _ = (&mut reader.0).await;
                    // Drain only after the old producer is gone, before the new IDR can enter.
                    while receiver.try_recv().is_ok() {}
                    reader.0 = tokio::spawn(read_encoder(size, display.clone(), portal.clone(), latest.clone(), next, sequence.clone(), idr.clone()));
                } else {
                    reader.restart(read_encoder(size, display.clone(), portal.clone(), latest.clone(), next, sequence.clone(), idr.clone())).await;
                }
            },
            frame = receiver.recv() => {
                // A closed queue means the reader ended; its JoinHandle carries the reason.
                let Some(frame) = frame else { return (&mut reader.0).await.map_err(|e| format!("Encoder reader task: {e}"))?; };
                if frame.codec != current.codec { continue; }
                let duration = timeline.duration_after(frame.completed_at);
                if let Some(last) = previous { skipped += frame.sequence.saturating_sub(last + 1); }
                max_age_ms = max_age_ms.max(frame.completed_at.elapsed().as_millis());
                // Continue monitoring close even if the network send stalls.
                let sample = Sample {
                    data: frame.data.clone().into(), duration, ..Default::default()
                };
                let extensions = [crate::rtp_metadata::capture_index(0), crate::rtp_metadata::new_frame()];
                let send = track.write_sample_with_extensions(frame.codec, &sample, &extensions);
                let send_started=Instant::now();
                tokio::pin!(send);
                loop {
                    tokio::select! {
                        result = &mut send => { result.map_err(|e| format!("Video send: {e}"))?; break; },
                        _ = health.tick() => { if transport_closed(&pc) { return Ok(()); } },
                    }
                }
                previous = Some(frame.sequence);
                max_send_ms=max_send_ms.max(send_started.elapsed().as_millis());
                frames += 1;
                if stats.elapsed() >= Duration::from_secs(30) {
                    crate::capture::keep_display_awake()?;
                    log::info!("Desktop latest-frame stats: sent={} skipped={} latest_seq={} max_slot_age_ms={} max_send_ms={}", frames, skipped, frame.sequence, max_age_ms,max_send_ms);
                    stats = Instant::now();
                    max_age_ms = 0;
                    max_send_ms = 0;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_wayland_child_uses_requested_capture_rate() {
        for fps in [5,30,60,90,144] {
            let command=super::wayland_capture_command(std::path::Path::new("uuremote-server"),fps);
            let value=fps.to_string();
            assert!(command.as_std().get_envs().any(|(k,v)|k=="UU_WAYLAND_FPS" && v==Some(std::ffi::OsStr::new(&value))));
        }
    }
    use super::*;
    #[test]
    fn x264_buffer_fits_keyframes() {
        assert_eq!(buffer_kbits(2000, Encoder::Nvenc), 250);
        assert_eq!(buffer_kbits(2000, Encoder::Vaapi), 250);
        assert_eq!(buffer_kbits(2000, Encoder::Cpu), 2000);
        assert_eq!(buffer_kbits(300, Encoder::Cpu), 500);
        assert_eq!(buffer_kbits(30_000, Encoder::Cpu), 8000);
    }

    #[test]
    fn vaapi_is_explicit_and_upload_follows_scale() {
        assert_eq!(Encoder::from_setting(None, true), Encoder::Nvenc);
        assert_eq!(Encoder::from_setting(None, false), Encoder::Cpu);
        assert_eq!(Encoder::from_setting(Some("nvenc"), false), Encoder::Nvenc);
        assert_eq!(Encoder::from_setting(Some("cpu"), true), Encoder::Cpu);
        assert_eq!(Encoder::from_setting(Some("vaapi"), true), Encoder::Vaapi);
        assert_eq!(vaapi_filter(None), "format=nv12,hwupload");
        assert_eq!(vaapi_filter(Some((1280,720))), "scale=1280:720:flags=bilinear,format=nv12,hwupload");
        let options = vaapi_options(&VideoSettings::default(), "30");
        for pair in [["-g","30"], ["-bf","0"], ["-async_depth","1"], ["-aud","1"]] {
            assert!(options.windows(2).any(|w| w[0] == pair[0] && w[1] == pair[1]));
        }
        assert!(!options.iter().any(|v| v == "-r" || v == "-pix_fmt"));
    }

    #[tokio::test]
    async fn encoder_restart_stops_old_producer_before_starting_new_chain() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct MarkStopped(Arc<AtomicBool>);
        impl Drop for MarkStopped {
            fn drop(&mut self) { self.0.store(true, Ordering::SeqCst); }
        }
        let stopped = Arc::new(AtomicBool::new(false));
        let old_stopped = stopped.clone();
        let (tx, mut rx) = mpsc::channel(4);
        let old_tx = tx.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let frame = |sequence, idr| Arc::new(CompleteFrame {
            sequence, codec: VideoCodec::H264, completed_at: Instant::now(),
            data: vec![0, 0, 1, 9, 240, 0, 0, 1, if idr { 5 } else { 1 }, 42],
        });
        let mut reader = EncoderTask(tokio::spawn(async move {
            let _stopped = MarkStopped(old_stopped);
            let mut gate = DependencyGate::default();
            gate.submit(&old_tx, frame(1, true));
            gate.submit(&old_tx, frame(2, false));
            started_tx.send(()).unwrap();
            std::future::pending::<Result<(), String>>().await
        }));
        started_rx.await.unwrap();
        let replacement_stopped = stopped.clone();
        reader.restart(async move {
            assert!(replacement_stopped.load(Ordering::SeqCst));
            DependencyGate::default().submit(&tx, frame(3, true));
            Ok(())
        }).await;
        // On this current-thread runtime, abort without awaiting cannot drop the old
        // future before returning here. No sleep or scheduler race is needed.
        assert!(stopped.load(Ordering::SeqCst), "old producer is still alive after restart");
        (&mut reader.0).await.unwrap().unwrap();
        for (sequence, idr) in [(1, true), (2, false), (3, true)] {
            let queued = rx.recv().await.unwrap();
            assert_eq!(queued.sequence, sequence);
            assert_eq!(is_independent_frame(&queued.data), idr);
        }
    }
    #[tokio::test]
    async fn encoder_restart_replaces_an_already_failed_producer() {
        let mut reader = EncoderTask(tokio::spawn(async { Err("encoder ended".into()) }));
        while !reader.0.is_finished() { tokio::task::yield_now().await; }
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        reader.restart(async move {
            started_tx.send(()).unwrap();
            Ok(())
        }).await;
        started_rx.await.unwrap();
        (&mut reader.0).await.unwrap().unwrap();
    }
    #[test]
    fn boundaries_accept_complete_idr_and_reject_dependent_frames() {
        let bytes = [0,0,0,1,9,240,0,0,1,7,1,0,0,1,5,42,0,0,1,9,240];
        assert_eq!(aud_offsets(&bytes), vec![0,16]);
        assert!(is_independent_frame(&bytes[..16]));
        assert!(!is_independent_frame(&[0,0,1,9,240,0,0,1,1,42]));
        assert!(!is_independent_frame(&[0,0,1,7,1,0,0,0,1]));
    }
    #[test]
    fn rtp_ticks_survive_webrtc_floor_conversion(){
        for ticks in (1..=200_000).chain([90_000*60, 90_000*3600]) {
            assert_eq!((ticks_to_duration(ticks).as_secs_f64()*90000.) as u64,ticks);
        }
    }
    fn stamps(offsets_ms:&[u64])->Vec<u64>{
        let start=Instant::now();
        let mut timeline=RtpTimeline::default();
        let mut stamp=0;
        offsets_ms.iter().map(|ms|{
            let current=stamp;
            stamp+=(timeline.duration_after(start+Duration::from_millis(*ms)).as_secs_f64()*90000.) as u64;
            current
        }).collect()
    }
    #[test]
    fn steady_60fps_keeps_exact_1500_tick_spacing_for_an_hour(){
        let start=Instant::now();
        let mut timeline=RtpTimeline::default();
        let mut stamp=0u64;
        for frame in 0..216_000u64 {
            let at=start+Duration::from_nanos(frame*1_000_000_000/FPS);
            assert_eq!(stamp,frame*1500);
            stamp+=(timeline.duration_after(at).as_secs_f64()*90000.) as u64;
        }
    }
    #[test]
    fn starved_capture_is_stamped_at_real_time_not_nominal_60fps(){
        // 5fps capture: frames 200ms apart must be ~18000 ticks apart, not 1500.
        let s=stamps(&[0,200,400,600,800]);
        assert_eq!(s[0],0);
        for pair in s.windows(2).skip(1) { assert_eq!(pair[1]-pair[0],18_000); }
    }
    #[test]
    fn stall_costs_one_frame_then_timeline_matches_real_time(){
        let s=stamps(&[0,17,517,534,551]);
        assert_eq!(s[2],17*90+1500);     // predicted before the stall was visible
        assert_eq!(s[3],517*90+1500);    // realigned: one nominal frame after real stall end
        assert_eq!(s[4],534*90+1500);    // no residual offset accumulates
        assert!(s.windows(2).all(|p|p[1]>p[0]));
    }
    #[tokio::test]
    async fn bounded_queue_never_sends_broken_prediction_chain_after_overload(){
        let(tx,mut rx)=mpsc::channel(1);
        let mut gate=DependencyGate::default();
        let frame=|sequence,idr|Arc::new(CompleteFrame{sequence,codec:VideoCodec::H264,completed_at:Instant::now(),data:vec![0,0,1,9,240,0,0,1,if idr{5}else{1},42]});
        gate.submit(&tx,frame(1,false));assert!(rx.try_recv().is_err()); // no IDR yet
        gate.submit(&tx,frame(2,true));
        gate.submit(&tx,frame(3,false)); // queue full: P3 lost
        assert_eq!(rx.recv().await.unwrap().sequence,2);
        gate.submit(&tx,frame(4,false)); // P4 references lost P3
        assert!(rx.try_recv().is_err());
        gate.submit(&tx,frame(31,true));assert_eq!(rx.recv().await.unwrap().sequence,31);
        gate.submit(&tx,frame(32,false));assert_eq!(rx.recv().await.unwrap().sequence,32);
    }
    #[tokio::test]
    #[ignore = "requires installed FFmpeg with a working HEVC encoder"]
    async fn hevc_capability_probe_initializes_real_encoder() {
        assert!(hevc_available().await);
    }
    #[test]
    fn hevc_annex_b_boundaries_and_irap_use_two_byte_headers() {
        let bytes = [0,0,0,1,70,1,80,0,0,1,64,1,42,0,0,1,38,1,42,0,0,1,70,1,80];
        assert_eq!(aud_offsets_for(&bytes, VideoCodec::H265), vec![0,19]);
        assert!(is_independent_frame_for(&bytes[..19], VideoCodec::H265));
        for kind in 16..=21 {
            assert!(is_independent_frame_for(&[0,0,1,kind << 1,1,42], VideoCodec::H265));
        }
        assert!(!is_independent_frame_for(&[0,0,1,2,1,42], VideoCodec::H265));
        assert!(!is_independent_frame_for(&[0,0,1,38], VideoCodec::H265));
        assert!(!is_independent_frame_for(&[0,0,1,38,0,42], VideoCodec::H265));
        assert!(!is_independent_frame_for(&[0,0,1,166,1,42], VideoCodec::H265));
        assert!(!is_independent_frame_for(&[0,0,1,38,1,42,0,0,1,2,1,42], VideoCodec::H265));
    }
    #[tokio::test]
    async fn hevc_dependency_gate_recovers_only_at_irap_after_overflow() {
        let (tx, mut rx) = mpsc::channel(1);
        let mut gate = DependencyGate::default();
        let frame = |sequence, irap| Arc::new(CompleteFrame { codec: VideoCodec::H265,
            sequence, completed_at: Instant::now(), data: vec![0,0,1,if irap { 38 } else { 2 },1,42] });
        gate.submit(&tx, frame(1, false));
        assert!(rx.try_recv().is_err());
        gate.submit(&tx, frame(2, true));
        gate.submit(&tx, frame(3, false));
        assert_eq!(rx.recv().await.unwrap().sequence, 2);
        gate.submit(&tx, frame(4, false));
        assert!(rx.try_recv().is_err());
        gate.submit(&tx, frame(5, true));
        assert_eq!(rx.recv().await.unwrap().sequence, 5);
    }
    #[test]
    fn hevc_options_keep_420_low_latency_and_existing_rate_policy() {
        let settings = VideoSettings { codec: VideoCodec::H265, ..Default::default() };
        let cpu = cpu_hevc_options(&settings, "30");
        for pair in [["-c:v", "libx265"], ["-profile:v", "main"], ["-pix_fmt", "yuv420p"],
            ["-bf", "0"], ["-bufsize", "30000k"]] {
            assert!(cpu.windows(2).any(|window| window == pair));
        }
        let params = x265_params("30");
        for option in ["aud=1", "repeat-headers=1", "bframes=0", "rc-lookahead=0", "keyint=30", "min-keyint=30"] {
            assert!(params.split(':').any(|value| value == option));
        }
        let vaapi = vaapi_options(&settings, "30");
        for pair in [["-c:v", "hevc_vaapi"], ["-profile:v", "main"], ["-bf", "0"], ["-bufsize", "500k"]] {
            assert!(vaapi.windows(2).any(|window| window == pair));
        }
        assert_eq!(Encoder::Nvenc.name_for(VideoCodec::H265), "hevc_nvenc");
        assert_eq!(Encoder::Cpu.name_for(VideoCodec::H264), "libx264");
    }
    #[test]
    fn nvenc_idr_with_parameter_sets_is_independent(){
        // AUD, SPS, PPS, IDR slice as h264_nvenc emits every GOP (verified on the 3090).
        let idr=[0,0,0,1,9,16,0,0,0,1,7,66,0,0,0,1,8,206,0,0,1,5,136];
        assert!(is_independent_frame(&idr));
        assert!(!is_independent_frame(&[0,0,0,1,9,48,0,0,1,1,154]));
    }
}
