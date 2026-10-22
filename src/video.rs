use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{mpsc, Semaphore, OwnedSemaphorePermit};
use webrtc::media::Sample;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::RTCPeerConnection;
use crate::capture_setting::{VideoCodec, VideoSettings};

/// Default frame rate; the viewer's 帧率 setting overrides it per connection.
pub const FPS: u64 = 60;

/// Refresh rate reported to the viewer (display_info / capability / codec negotiation), like the
/// official hosts report the monitor's mode (official Mac capability: 120). `UU_DISPLAY_FPS` overrides;
/// Hyprland from `hyprctl -j monitors`, X11 from the current `xrandr` mode, otherwise 60.
pub fn display_fps() -> u32 {
    static CACHE: std::sync::Mutex<Option<(std::time::Instant, u32)>> = std::sync::Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, fps)) = *cache { if at.elapsed() < Duration::from_secs(30) { return fps; } }
    let fps = std::env::var("UU_DISPLAY_FPS").ok().and_then(|v| v.trim().parse::<f64>().ok())
        .or_else(detect_refresh).map_or(FPS as u32, |hz| hz.round().clamp(24.0, 360.0) as u32);
    *cache = Some((std::time::Instant::now(), fps));
    fps
}
fn detect_refresh() -> Option<f64> {
    if cfg!(test) { return None; }
    // Bounded: a display tool must never stall connection setup (e.g. an X server started on demand).
    let run = |program: &str, args: &[&str]| -> Option<String> {
        use std::io::Read;
        let mut child = std::process::Command::new(program).args(args).stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn().ok()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let status = loop {
            if let Some(status) = child.try_wait().ok()? { break status; }
            if std::time::Instant::now() >= deadline { let _ = child.kill(); let _ = child.wait(); return None; }
            std::thread::sleep(Duration::from_millis(10));
        };
        let mut text = String::new();
        child.stdout.take()?.read_to_string(&mut text).ok()?;
        status.success().then_some(text)
    };
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        if let Some(hz) = run("hyprctl", &["-j", "monitors"]).as_deref().and_then(hyprland_refresh) { return Some(hz); }
    }
    if std::env::var_os("DISPLAY").is_some() { return run("xrandr", &["--current"]).as_deref().and_then(xrandr_refresh); }
    None
}
/// The focused monitor (else the first); the portal/Hyprland capture serves one output.
fn hyprland_refresh(json: &str) -> Option<f64> {
    let monitors: Vec<serde_json::Value> = serde_json::from_str(json).ok()?;
    let monitor = monitors.iter().find(|m| m["focused"] == true).or(monitors.first())?;
    monitor["refreshRate"].as_f64().filter(|hz| *hz > 0.0)
}
/// The current mode (`60.00*+`) of the output marked `primary`, else of the first connected output.
fn xrandr_refresh(text: &str) -> Option<f64> {
    let current = |lines: &mut dyn Iterator<Item = &str>| lines.take_while(|l| l.starts_with(' '))
        .flat_map(str::split_whitespace).find(|t| t.contains('*'))
        .and_then(|t| t.trim_end_matches(['*', '+']).parse::<f64>().ok());
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if line.contains(" connected primary") { return current(&mut lines); }
    }
    let mut lines = text.lines().skip_while(|l| !l.contains(" connected")).skip(1);
    current(&mut lines)
}
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
            VideoCodec::H265 => hevc_nal_kind(bytes, header) == Some(kind),
        };
        if matches { result.push(i); }
        i += prefix;
    }
    result
}

/// Validated HEVC NAL unit type from the two-byte header (`None` when it is incomplete or the
/// forbidden bit is set or temporal_id_plus1 is zero).
fn hevc_nal_kind(bytes: &[u8], header: usize) -> Option<u8> {
    (header + 1 < bytes.len() && bytes[header] & 128 == 0 && bytes[header + 1] & 7 != 0)
        .then(|| (bytes[header] >> 1) & 63)
}

/// Access-unit starts for H.265, independent of where the encoder puts its AUDs. VAAPI writes an
/// AUD only at IRAPs, so splitting on AUD alone groups a whole GOP into one "frame" (and that
/// frame then fails the IRAP gate, so nothing is sent). RFC 7798 §3.1 puts the boundary at the
/// first NAL after a VCL NAL that is either non-VCL (AUD/VPS/SPS/PPS/SEI) or starts a picture
/// (first_slice_segment_in_pic_flag), which also keeps multi-slice pictures in one unit.
fn hevc_au_offsets(bytes: &[u8]) -> Vec<usize> {
    let mut result = Vec::new();
    let mut previous_vcl = false;
    let mut first = true;
    let mut i = 0;
    while i + 3 < bytes.len() {
        let prefix = if bytes[i..].starts_with(&[0, 0, 0, 1]) { 4 }
            else if bytes[i..].starts_with(&[0, 0, 1]) { 3 } else { i += 1; continue };
        let header = i + prefix;
        if let Some(kind) = hevc_nal_kind(bytes, header) {
            let vcl = kind < 32;
            let starts_picture = vcl && bytes.get(header + 2).is_some_and(|byte| byte & 0x80 != 0);
            // RFC 7798 / H.265 7.4.2.4.4: only these non-VCL types may open the next access unit.
            // Suffix SEI (40), filler (38) and end of sequence/bitstream (36, 37) stay with the
            // picture they follow.
            let opens_unit = matches!(kind, 32..=35 | 39 | 41..=44 | 48..=55);
            if first || (previous_vcl && (opens_unit || starts_picture)) { result.push(i); }
            // Trailing NALs (suffix SEI, filler, end markers) do not end "after a VCL NAL".
            if vcl || opens_unit { previous_vcl = vcl; }
            first = false;
        }
        i += prefix;
    }
    result
}

fn au_offsets_for(bytes: &[u8], codec: VideoCodec) -> Vec<usize> {
    match codec {
        VideoCodec::H264 => nal_offsets_for(bytes, 9, codec),
        VideoCodec::H265 => hevc_au_offsets(bytes),
    }
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
fn aud_offsets(bytes: &[u8]) -> Vec<usize> { au_offsets_for(bytes, VideoCodec::H264) }
fn is_independent_frame(bytes: &[u8]) -> bool { is_independent_frame_for(bytes, VideoCodec::H264) }

#[derive(Debug)]
struct CompleteFrame {
    source_id: usize,
    source_generation: u64,
    codec: VideoCodec,
    sequence: u64,
    completed_at: Instant,
    data: Vec<u8>,
    _permit: Option<OwnedSemaphorePermit>,
}

impl CompleteFrame {
    fn matches(&self,settings:&VideoSettings)->bool {
        self.codec==settings.codec && self.source_id==settings.source_id && self.source_generation==settings.source_generation
    }
}

const RTP_CLOCK: u64 = 90_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Encoder { Cpu, Nvenc, Vaapi }
impl Encoder {
    pub(crate) fn requested() -> Self { crate::video_select::selection().encoder }
    /// Unset/auto tries NVENC only with an NVIDIA driver loaded: `ffmpeg -encoders` lists h264_nvenc
    /// on any machine, so without this every stream start first ran a doomed NVENC process (#10).
    #[cfg(test)]
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
pub(crate) fn nvidia_present() -> bool {
    std::path::Path::new("/proc/driver/nvidia/version").exists() || std::path::Path::new("/dev/nvidia0").exists()
}
/// 真彩 (4:4:4): NVENC (in-process or FFmpeg) and libx264; h264_vaapi is 4:2:0 only.
/// Auto that probed VAAPI still reports true: the 4:4:4 request uses the next capable candidate.
pub fn full_chroma_available() -> bool {
    let selection = crate::video_select::selection();
    if crate::video_select::encoder_full_chroma(selection.encoder) { return true; }
    crate::video_select::encoder_for_chroma(selection, crate::capture_setting::CHROMA_444, crate::video_select::cached_probe().as_ref())
        != selection.encoder
        || selection.encoder_source == crate::video_select::Source::Auto
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
    let mut options: Vec<String> = ["-c:v", Encoder::Vaapi.name_for(settings.codec), "-profile:v", profile_for(settings.codec, true),
        "-g", gop, "-bf", "0", "-rc_mode", "VBR", "-b:v", &settings.max_kbps().map_or("16M".into(), |k| format!("{}k",k*3/4)),
        "-maxrate", &maxrate, "-bufsize", &buffer, "-qmin", &settings.cq().to_string(), "-async_depth", "1"]
        .into_iter().map(str::to_owned).collect();
    // h264_vaapi writes an AUD per picture and the reader splits access units on it. hevc_vaapi
    // does not: with `-aud 1` it writes an AUD only at IRAPs and drops every other picture from
    // the bitstream (a 90-frame clip encoded to 3 IDRs and no P-frames), so HEVC leaves AUD off
    // and hevc_au_offsets() splits pictures by their own boundaries. Parameter sets are still
    // repeated in-band ahead of every IDR.
    if settings.codec == VideoCodec::H264 { options.extend(["-aud".into(), "1".into()]); }
    options
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
    let mut options = ["-c:v", "libx265", "-preset", "superfast", "-tune", "zerolatency", "-profile:v", profile,
        "-pix_fmt", pix_fmt, "-g", gop, "-bf", "0", "-threads", "4",
        "-maxrate", &maxrate, "-bufsize", &buffer, "-x265-params", &x265_params(gop)]
        .into_iter().map(str::to_owned).collect::<Vec<_>>();
    // Same as libx264: capped CRF starves repeated keyframes, so bitrate presets use ABR.
    match settings.max_kbps() {
        Some(k) => options.extend(["-b:v".into(), format!("{}k", k * 3 / 4)]),
        None => options.extend(["-crf".into(), settings.cq().to_string()]),
    }
    options
}

/// NVENC converts packed RGB on the GPU (BT.601 limited range, tagged bt470bg like the swscale default;
/// compared on the 3090), so 4:2:0 without scaling skips the CPU BGRA→YUV pass (#16: ~2 cores at 4K).
/// When the desktop is composited on the NVIDIA card itself, the packed-RGB path is slower than CPU
/// conversion (#16: RTX 5070 at 4K, 82.6 vs 98 fps), so it stays off unless `UU_NVENC_RGB=1`.
/// `input` is the source pixel format; `UU_NVENC_RGB=0` always restores CPU conversion.
fn nvenc_rgb_input(settings: &VideoSettings, encoder: Encoder, input: &'static str) -> Option<&'static str> {
    let wanted = match std::env::var("UU_NVENC_RGB").as_deref() {
        Ok("0") => false,
        Ok("1") => true,
        _ => !nvidia_drives_display(std::path::Path::new("/sys/class/drm")),
    };
    (encoder == Encoder::Nvenc && settings.chroma != crate::capture_setting::CHROMA_444 && settings.output.is_none() && wanted)
        .then_some(input)
}

/// A connected DRM connector (`cardN-HDMI-A-1/status`) on a card bound to the nvidia driver.
/// Only visible with nvidia-drm modeset, which Wayland compositors require.
fn nvidia_drives_display(drm: &std::path::Path) -> bool {
    std::fs::read_dir(drm).into_iter().flatten().flatten().any(|entry| {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((card, _)) = name.split_once('-') else { return false; };
        card.starts_with("card")
            && std::fs::read_to_string(entry.path().join("status")).is_ok_and(|s| s.trim() == "connected")
            && std::fs::read_link(drm.join(card).join("device/driver")).is_ok_and(|p| p.file_name().is_some_and(|n| n == "nvidia"))
    })
}

fn encoder_options(settings: &VideoSettings, encoder: Encoder, gop: &str, rgb: Option<&'static str>) -> Vec<String> {
    let (maxrate, bufsize) = rate_args(settings, encoder);
    let cq = settings.cq().to_string();
    let target = settings.max_kbps().map_or("16M".to_string(), |k| format!("{}k", k * 3 / 4));
    if encoder == Encoder::Vaapi { return vaapi_options(settings, gop); }
    let (profile, pix_fmt) = chroma_args(settings, encoder);
    if encoder == Encoder::Nvenc {
        return ["-c:v", encoder.name_for(settings.codec), "-preset", "p4", "-tune", "ull",
            "-profile:v", profile, "-pix_fmt", rgb.unwrap_or(pix_fmt), "-g", gop, "-bf", "0",
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

/// Filter chain for a capture of `source` size (None when the input size is not known yet, e.g.
/// the fixed-input probe). H.264/HEVC 4:2:0 encoders — and NVENC's surface allocation — require
/// even frame dimensions, but the captured size is not always even: the portal backend reports the
/// compositor's logical desktop size, so two stacked monitors at 150% scale capture 2560x2507 and
/// every encoder failed to open ("height not divisible by 2 (2560x2507)" from libx264,
/// "InitializeEncoder failed: out of memory (10)" from h264_nvenc), leaving the viewer stuck on the
/// 正在传输画面 spinner. When the native capture size is odd, pad the odd row/column; the host
/// display mode and every reported capture setting stay untouched.
fn scale_options(settings: &VideoSettings, encoder: Encoder, source: Option<(u32,u32)>) -> Vec<String> {
    const EVEN_PAD: &str = "pad=ceil(iw/2)*2:ceil(ih/2)*2:0:0:black";
    let odd_capture = settings.output.is_none() && source.is_some_and(|(w,h)| w % 2 != 0 || h % 2 != 0);
    if encoder == Encoder::Vaapi {
        let filtered = if odd_capture { format!("{EVEN_PAD},{}", vaapi_filter(None)) } else { vaapi_filter(settings.output) };
        return vec!["-vf".into(), filtered];
    }
    if odd_capture { return vec!["-vf".into(), EVEN_PAD.into()]; }
    settings.output.map_or_else(Vec::new, |(w,h)| vec!["-vf".into(), format!("scale={w}:{h}:flags=bilinear")])
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct FfmpegSpec {
    program: PathBuf,
    ld_library_path: Option<OsString>,
    path: Option<OsString>,
}

impl FfmpegSpec {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        if let Some(path) = &self.ld_library_path { command.env("LD_LIBRARY_PATH", path); }
        if let Some(path) = &self.path { command.env("PATH", path); }
        command
    }
}

fn ffmpeg_spec() -> FfmpegSpec {
    match crate::config::ffmpeg_root() {
        Some(root) => FfmpegSpec {
            program: root.join("bin/ffmpeg"),
            ld_library_path: Some(root.join("lib/x86_64-linux-gnu").into_os_string()),
            path: None,
        },
        None => FfmpegSpec {
            program: PathBuf::from("ffmpeg"),
            ld_library_path: None,
            path: std::env::var_os("PATH"),
        },
    }
}

pub(crate) fn ffmpeg_command() -> Command { ffmpeg_spec().command() }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FfmpegTimingMode { Modern, Legacy }

fn help_has_option(help: &str, option: &str) -> bool {
    help.lines().filter_map(|line| line.split_whitespace().next()).any(|token| {
        token == option || token.starts_with(&format!("{option}:") ) || token.starts_with(&format!("{option}["))
    })
}

fn timing_mode_from_help(help: &str) -> FfmpegTimingMode {
    if help_has_option(help, "-fps_mode") && help_has_option(help, "-enc_time_base") {
        FfmpegTimingMode::Modern
    } else {
        FfmpegTimingMode::Legacy
    }
}

fn timing_args(mode: FfmpegTimingMode, fps: u32) -> Vec<String> {
    match mode {
        FfmpegTimingMode::Modern => vec!["-fps_mode".into(), "vfr".into(), "-enc_time_base".into(), format!("1/{fps}")],
        FfmpegTimingMode::Legacy => vec!["-vsync".into(), "0".into()],
    }
}

/// Legacy is chosen only from a completed help dump that visibly lacks the
/// modern options. A failed probe (spawn error, non-zero exit, timeout, or no
/// parseable help text) must keep the modern args: `-vsync` is deprecated
/// since FFmpeg 5.1 and may not be accepted by newer builds, so falling back
/// to it on a failed probe would kill the encoder there — worse than guessing.
fn timing_mode_from_probe(success: bool, stdout: &[u8], stderr: &[u8]) -> FfmpegTimingMode {
    let mut help = Vec::with_capacity(stdout.len() + stderr.len());
    help.extend_from_slice(stdout);
    help.extend_from_slice(stderr);
    if !success || help.is_empty() { return FfmpegTimingMode::Modern; }
    timing_mode_from_help(&String::from_utf8_lossy(&help))
}

async fn probe_timing_mode(spec: &FfmpegSpec) -> FfmpegTimingMode {
    let mut command = spec.command();
    command.args(["-hide_banner", "-nostdin", "-h", "full"])
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    match tokio::time::timeout(Duration::from_secs(3), command.output()).await {
        Ok(Ok(output)) => timing_mode_from_probe(output.status.success(), &output.stdout, &output.stderr),
        _ => timing_mode_from_probe(false, &[], &[]),
    }
}

fn timing_cache() -> &'static Mutex<HashMap<FfmpegSpec, Arc<tokio::sync::OnceCell<FfmpegTimingMode>>>> {
    static CACHE: OnceLock<Mutex<HashMap<FfmpegSpec, Arc<tokio::sync::OnceCell<FfmpegTimingMode>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn timing_cell(
    cache: &Mutex<HashMap<FfmpegSpec, Arc<tokio::sync::OnceCell<FfmpegTimingMode>>>>,
    spec: &FfmpegSpec,
) -> Arc<tokio::sync::OnceCell<FfmpegTimingMode>> {
    let mut cache = cache.lock().expect("FFmpeg timing cache poisoned");
    cache.entry(spec.clone()).or_insert_with(|| Arc::new(tokio::sync::OnceCell::const_new())).clone()
}

async fn ffmpeg_timing_mode(spec: &FfmpegSpec) -> FfmpegTimingMode {
    let cell = timing_cell(timing_cache(), spec);
    *cell.get_or_init(|| probe_timing_mode(spec)).await
}

/// HEVC for the encoder this process will actually open. Codec auto must not cache the
/// pre-probe legacy guess: that answer decides the H.265 m-line for the rest of the process.
pub async fn hevc_available(size: (u32, u32)) -> bool {
    hevc_for(hevc_encoder(size).await).await
}

/// Resolution for the one cached auto probe. Zero or missing geometry falls back to 1920×1080,
/// the same nominal size headless tool peers advertise. Odd sides are rounded up: 4:2:0 rejects them.
pub(crate) fn probe_size(size: (u32, u32)) -> (u32, u32) {
    let (w, h) = if size.0 == 0 || size.1 == 0 { (1920, 1080) } else { size };
    (w + w % 2, h + h % 2)
}

/// Which encoder's HEVC result to cache. Auto uses the probe's choice; an explicit encoder does not.
pub(crate) fn hevc_encoder_from(source: crate::video_select::Source, explicit: Encoder, probed: Option<Encoder>) -> Encoder {
    if source == crate::video_select::Source::Auto { probed.unwrap_or(explicit) } else { explicit }
}

/// Encoder used to decide HEVC. Auto waits for the cached single-flight probe first.
/// `size` is the desktop this session will encode (F3: one IDR at the current resolution).
pub(crate) async fn hevc_encoder(size: (u32, u32)) -> Encoder {
    let selection = crate::video_select::selection();
    if selection.encoder_source == crate::video_select::Source::Auto && crate::video_select::cached_probe().is_none() {
        ensure_probed(probe_size(size)).await;
    }
    let fresh = crate::video_select::selection();
    let probed = crate::video_select::cached_probe().map(|report| report.chosen);
    hevc_encoder_from(fresh.encoder_source, fresh.encoder, probed)
}

/// One synthetic 4:2:0 IDR per encoder, cached. Hardware falls back to CPU once.
pub(crate) async fn hevc_for(encoder: Encoder) -> bool {
    static CACHE: std::sync::OnceLock<Mutex<HashMap<Encoder, Arc<tokio::sync::OnceCell<bool>>>>> = std::sync::OnceLock::new();
    let cells = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let cell = {
        let mut guard = cells.lock().expect("HEVC probe cache poisoned");
        guard.entry(encoder).or_insert_with(|| Arc::new(tokio::sync::OnceCell::const_new())).clone()
    };
    *cell.get_or_init(|| async move {
        let mut candidates = vec![encoder];
        if encoder != Encoder::Cpu { candidates.push(Encoder::Cpu); }
        for candidate in candidates {
            if probe_hevc_frame(candidate).await {
                log::info!("HEVC encoder probe: {} ok", candidate.name_for(VideoCodec::H265));
                return true;
            }
        }
        log::info!("HEVC encoder probe: unavailable");
        false
    }).await
}

async fn probe_hevc_frame(encoder: Encoder) -> bool {
    let settings = VideoSettings { codec: VideoCodec::H265, ..Default::default() };
    let mut command = ffmpeg_command();
    command.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    if encoder == Encoder::Vaapi { command.args(["-vaapi_device", &vaapi_device()]); }
    command.args(["-f", "lavfi", "-i", "color=black:size=256x256:rate=1", "-frames:v", "1", "-an"]);
    if encoder == Encoder::Vaapi { command.args(["-vf", &vaapi_filter(None)]); }
    command.args(encoder_options(&settings, encoder, "30", None));
    command.args(["-f", "hevc", "pipe:1"]).kill_on_drop(true).stderr(std::process::Stdio::null());
    match tokio::time::timeout(Duration::from_secs(5), command.output()).await {
        Ok(Ok(output)) => output.status.success() && is_independent_frame_for(&output.stdout, VideoCodec::H265),
        _ => false,
    }
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

/// Encoder switch: join the old task, drop what it already queued, then set IDR for the next open.
async fn stop_then_drain_then_idr(
    task: &mut tokio::task::JoinHandle<()>,
    receiver: &mut mpsc::Receiver<u8>,
    idr: &AtomicBool,
    release: &tokio::sync::Notify,
) -> Vec<&'static str> {
    let mut order = Vec::new();
    release.notify_one();
    task.abort();
    let _ = (&mut *task).await;
    order.push("stopped");
    while receiver.try_recv().is_ok() {}
    order.push("drained");
    idr.store(true, std::sync::atomic::Ordering::Release);
    order.push("idr");
    order
}

#[cfg(feature = "inproc-av")]
struct StopOnDrop(Arc<AtomicBool>);
#[cfg(feature = "inproc-av")]
impl Drop for StopOnDrop { fn drop(&mut self) { self.0.store(true, std::sync::atomic::Ordering::Relaxed); } }

async fn probe_encoder_frame(encoder: Encoder, size: (u32, u32), codec: VideoCodec) -> bool {
    #[cfg(feature = "inproc-av")]
    if inproc_probe_path(encoder) {
        return probe_inproc_frame(encoder, size, codec).await;
    }
    // CPU has no device. It still encodes one real frame on the FFmpeg path and counts only an IDR.
    probe_ffmpeg_frame(encoder, size, codec).await
}

/// inproc-av on X11 encodes the probe with the same engine a desktop stream uses.
#[cfg(feature = "inproc-av")]
fn inproc_probe_path(encoder: Encoder) -> bool {
    crate::desktop_backend::detect().ok() == Some(crate::desktop_backend::DesktopBackend::X11)
        && native_inproc(":0", false, encoder)
}

#[cfg(feature = "inproc-av")]
async fn probe_inproc_frame(encoder: Encoder, size: (u32, u32), codec: VideoCodec) -> bool {
    let settings = VideoSettings { codec, fps: 30, ..VideoSettings::default() };
    let (w, h) = (size.0.max(16), size.1.max(16));
    let max_kbps = settings.max_kbps().unwrap_or(30_000) as i64;
    let config = crate::inproc_encoder::Config {
        display: std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into()),
        input: (w, h), output: Some((w, h)), fps: 30, gop: 30, cq: settings.cq() as u32,
        target_bps: 16_000_000, max_bps: max_kbps * 1000,
        buffer_bits: i64::from(buffer_for(&settings, encoder)) * 1000,
        codec, chroma: settings.chroma, vaapi: encoder == Encoder::Vaapi,
    };
    let (tx, mut rx) = mpsc::channel(1);
    let stop = Arc::new(AtomicBool::new(false));
    let idr = Arc::new(AtomicBool::new(true));
    let budget = Arc::new(Semaphore::new(1));
    let job = tokio::task::spawn_blocking({
        let stop = stop.clone();
        move || crate::inproc_encoder::run(config, tx, stop, idr, budget)
    });
    // 3 s covers vaapi_create and the first IDR. On timeout, signal stop. run() checks stop
    // before vaapi_create / nvenc_create. The thread is not aborted (no stream_slot). Join is
    // capped so a stuck create cannot hold the SDP answer; the candidate is then a failure.
    let frame = tokio::time::timeout(PROBE_FRAME_BUDGET, rx.recv()).await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    match tokio::time::timeout(PROBE_JOIN_BUDGET, job).await {
        Ok(_) => matches!(frame, Ok(Some(unit)) if is_independent_frame_for(&unit.data, codec)),
        Err(_) => {
            log::warn!("In-process {} probe still running after {}s; treating it as failed",
                encoder.name_for(codec), (PROBE_FRAME_BUDGET + PROBE_JOIN_BUDGET).as_secs());
            false
        }
    }
}

/// First-frame wait, then how long a timed-out inproc probe may keep the answer path blocked.
const PROBE_FRAME_BUDGET: Duration = Duration::from_secs(3);
const PROBE_JOIN_BUDGET: Duration = Duration::from_secs(3);

/// Join budget after stop. A thread that is still inside create counts as a failed candidate.
pub(crate) fn probe_join_outcome(exited: bool) -> bool { exited }

async fn probe_ffmpeg_frame(encoder: Encoder, size: (u32, u32), codec: VideoCodec) -> bool {
    let settings = VideoSettings { codec, ..VideoSettings::default() };
    let mut command = ffmpeg_command();
    command.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    if encoder == Encoder::Vaapi { command.args(["-vaapi_device", &vaapi_device()]); }
    command.args(["-f", "lavfi", "-i", &format!("color=c=black:s={}x{}:r=1", size.0.max(16), size.1.max(16)), "-frames:v", "1", "-an"]);
    if encoder == Encoder::Vaapi { command.args(["-vf", &vaapi_filter(None)]); }
    command.args(encoder_options(&settings, encoder, "30", None));
    let container = if codec == VideoCodec::H265 { "hevc" } else { "h264" };
    command.args(["-f", container, "pipe:1"]).kill_on_drop(true).stderr(std::process::Stdio::null());
    match tokio::time::timeout(Duration::from_secs(3), command.output()).await {
        Ok(Ok(output)) => output.status.success() && is_independent_frame_for(&output.stdout, codec),
        _ => false,
    }
}

fn probe_flight() -> &'static tokio::sync::Mutex<()> {
    static FLIGHT: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    FLIGHT.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// SIGUSR2 compares `running` (what this stream last opened or fell back to) with the request.
/// The watch is updated when an encoder opens and is only read here, so a codec reload cannot
/// consume the report and make the next set-encoder look like a no-op.
pub(crate) fn encoder_after_reload(running: Encoder, requested: Encoder) -> (Encoder, bool) {
    (requested, running != requested)
}

/// Latest encoder a stream actually opened. The sender stays with `read_encoder`; the stream
/// only borrows the current value. `try_recv` would drop a report that a later reload needs.
pub(crate) fn watch_running(rx: &tokio::sync::watch::Receiver<Encoder>) -> Encoder {
    *rx.borrow()
}

/// Codec-preference change versus the preference last applied on this stream.
pub(crate) fn codec_pref_changed(applied: crate::video_select::CodecPref, now: crate::video_select::CodecPref) -> bool {
    applied != now
}

/// First encoder request of the process. SIGUSR2 does not clear this cache.
pub async fn ensure_probed(size: (u32, u32)) -> crate::video_select::ProbeReport {
    if let Some(cached) = crate::video_select::cached_probe() { return cached; }
    let _flight = probe_flight().lock().await;
    if let Some(cached) = crate::video_select::cached_probe() { return cached; }
    let order = crate::video_select::candidate_order(display_is_nvidia(), nvidia_present());
    let mut ok = Vec::with_capacity(order.len());
    for encoder in &order {
        ok.push(probe_encoder_frame(*encoder, size, VideoCodec::H264).await);
    }
    let chosen = crate::video_select::choose_from_probe(&order, &ok);
    let report = crate::video_select::ProbeReport { order, ok, chosen };
    let parts: Vec<String> = report.order.iter().zip(report.ok.iter()).map(|(e, good)| {
        format!("{}={}", format!("{e:?}").to_ascii_lowercase(), if *good { "ok" } else { "fail" })
    }).collect();
    log::info!("Encoder probe: {}", parts.join(" "));
    crate::video_select::store_probe(report.clone());
    report
}

/// NVIDIA driving the display: OpenGL vendor first. glxinfo missing, failing or hanging falls back
/// to a connected DRM connector on an nvidia-bound card (#89). `/dev/nvidia0` is not this check.
fn display_is_nvidia() -> bool {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() && std::env::var_os("DISPLAY").is_none() {
        return nvidia_drives_display(std::path::Path::new("/sys/class/drm"));
    }
    match glx_vendor_nvidia() {
        Some(nvidia) => nvidia,
        None => nvidia_drives_display(std::path::Path::new("/sys/class/drm")),
    }
}

fn glx_vendor_nvidia() -> Option<bool> {
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
    let mut command = std::process::Command::new("timeout");
    command.args(["2", "glxinfo", "-B"]).env("DISPLAY", &display);
    let output = command.output().ok()?;
    if !output.status.success() && output.stdout.is_empty() { return None; }
    Some(String::from_utf8_lossy(&output.stdout).lines().any(|l| l.contains("OpenGL vendor") && l.to_ascii_lowercase().contains("nvidia")))
}

async fn read_encoder(
    size: (u32, u32), display: String, portal:Option<Arc<crate::portal::PortalSession>>,
    latest: mpsc::Sender<Arc<CompleteFrame>>,
    settings: VideoSettings, sequence: Arc<std::sync::atomic::AtomicU64>, idr: Arc<AtomicBool>, budget: Arc<Semaphore>,
    opened: Option<tokio::sync::watch::Sender<Encoder>>,
) -> Result<(), String> {
    // The sequence is shared so a CPU fallback or settings restart continues it (skip statistics stay correct).
    // Explicit cpu/vaapi/nvenc keeps the v0.2.23 path and does not probe the other devices.
    if crate::video_select::selection().encoder_source == crate::video_select::Source::Auto
        && crate::video_select::cached_probe().is_none() { ensure_probed(probe_size(size)).await; }
    let probed = crate::video_select::cached_probe();
    let fresh = crate::video_select::selection();
    let encoder = crate::video_select::encoder_for_chroma(fresh, settings.chroma, probed.as_ref());
    if encoder == Encoder::Cpu && fresh.encoder != Encoder::Cpu {
        log::warn!("Encoder {} (source={}) cannot do the requested chroma; using cpu",
            format!("{:?}", fresh.encoder).to_ascii_lowercase(), crate::video_select::encoder_source_label(fresh.encoder_source));
    }
    log::info!("Video selection: encoder={} source={} codec_pref={} source={}",
        format!("{encoder:?}").to_ascii_lowercase(), crate::video_select::encoder_source_label(fresh.encoder_source),
        match fresh.codec_pref { crate::video_select::CodecPref::Auto => "auto", crate::video_select::CodecPref::H264 => "h264", crate::video_select::CodecPref::H265 => "h265" },
        crate::video_select::source_label(fresh.codec_source));
    // Publish the request before open. A runtime fallback overwrites it with CPU, so a SIGUSR2
    // during the fallback compares against the encoder that is actually taking over.
    if let Some(tx) = &opened { let _ = tx.send(encoder); }
    if encoder == Encoder::Cpu { return read_encoder_with(size,display,portal,latest,Encoder::Cpu,true,sequence,settings,idr,budget).await; }
    #[cfg_attr(not(feature = "inproc-av"), allow(unused_mut))]
    let mut result = read_encoder_with(size,display.clone(),portal.clone(),latest.clone(),encoder,true,sequence.clone(),settings,idr.clone(),budget.clone()).await;
    // A failed native NVENC session (old driver, unsupported mode) still leaves FFmpeg's NVENC process pipeline.
    #[cfg(feature = "inproc-av")]
    if let Err(error) = &result {
        if !latest.is_closed() && native_inproc(&display, portal.is_some(), encoder) && x11_resized(&display, portal.is_some(), size).is_none() {
            log::warn!("In-process {} stream unavailable; switching to the FFmpeg process pipeline: {}",encoder.name_for(settings.codec),error);
            result = read_encoder_with(size,display.clone(),portal.clone(),latest.clone(),encoder,false,sequence.clone(),settings,idr.clone(),budget.clone()).await;
        }
    }
    match result {
        Ok(())=>Ok(()),
        Err(error) if !latest.is_closed() && x11_resized(&display, portal.is_some(), size).is_none()=>{
            log::warn!("{} stream unavailable (requested {} source={}); switching scoped encoder to CPU: {}",
                encoder.name_for(settings.codec), format!("{encoder:?}").to_ascii_lowercase(),
                crate::video_select::encoder_source_label(crate::video_select::selection().encoder_source), error);
            if let Some(tx) = &opened { let _ = tx.send(Encoder::Cpu); }
            read_encoder_with(size,display,portal,latest,Encoder::Cpu,true,sequence,settings,idr,budget).await
        },
        Err(error)=>Err(error),
    }
}

/// A resized X11 root fails every encoder alike (x11grab BadMatch on the old size);
/// returns the new size so the stream restarts capture instead of walking the fallback chain.
fn x11_resized(display: &str, portal: bool, size: (u32, u32)) -> Option<(u32, u32)> {
    if portal || display.starts_with("wayland:") || display.starts_with("kms:") { return None; }
    crate::capture::x11_size().filter(|current| *current != size)
}

/// The native in-process NVENC path serves X11 only (not Wayland/KMS capture or a portal session).
#[cfg(feature = "inproc-av")]
fn native_inproc(display: &str, portal: bool, encoder: Encoder) -> bool {
    !display.starts_with("wayland:") && !display.starts_with("kms:") && !portal && crate::inproc_encoder::selected(encoder)
}

/// Capture and encode inside this process; each packet is one access unit.
#[cfg(feature = "inproc-av")]
async fn read_inprocess(
    config: crate::inproc_encoder::Config, latest: mpsc::Sender<Arc<CompleteFrame>>,
    sequence: Arc<std::sync::atomic::AtomicU64>, idr: Arc<AtomicBool>, budget: Arc<Semaphore>,
) -> Result<(), String> {
    let codec = config.codec;
    let (units_tx, mut units) = mpsc::channel::<crate::inproc_encoder::EncodedUnit>(4);
    let stop = Arc::new(AtomicBool::new(false));
    // If this future is dropped (reader task aborted), the blocking encoder stops at its next packet.
    let _stop_on_drop = StopOnDrop(stop.clone());
    // A restart aborts the previous reader without waiting for its blocking encoder; wait for it here.
    let slot = crate::inproc_encoder::stream_slot(&idr).await;
    let job = tokio::task::spawn_blocking({
        let stop = stop.clone();
        move || { let _slot = slot; crate::inproc_encoder::run(config, units_tx, stop, idr, budget) }
    });
    let mut gate = DependencyGate::default();
    while let Some(unit) = units.recv().await {
        let sequence = sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        gate.submit(&latest, Arc::new(CompleteFrame { source_generation: 0, source_id: 0, codec, sequence, completed_at: unit.completed_at, data: unit.data, _permit: Some(unit.permit) }));
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
    if encoder == Encoder::Cpu { max_kbps.clamp(500, 8000) }
    else if encoder == Encoder::Vaapi {
        let requested = std::env::var("UU_VAAPI_BUFFER_MS").ok();
        vaapi_buffer_kbits(max_kbps, requested.as_deref()).unwrap_or((max_kbps / 8).clamp(100, 500))
    }
    else { (max_kbps / 8).clamp(100, 500) }
}

/// Opt-in VAAPI VBV buffer: `码率上限 × 毫秒 / 1000` for `UU_VAAPI_BUFFER_MS` in 10-1000 ms.
/// The 100-500 kbit default is sized for NVENC; VAAPI quantizes a whole desktop keyframe when
/// the buffer is that small, which reads as a blur that sharpens over each GOP (#42).
/// Unset, out of range or unparsable values keep the default buffer.
fn vaapi_buffer_kbits(max_kbps: u32, value: Option<&str>) -> Option<u32> {
    let ms = value?.trim().parse::<u32>().ok().filter(|ms| (10..=1000).contains(ms))?;
    Some((max_kbps.saturating_mul(ms) / 1000).max(1))
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
    size: (u32,u32),display:String,portal:Option<Arc<crate::portal::PortalSession>>,latest:mpsc::Sender<Arc<CompleteFrame>>,encoder:Encoder,
    #[cfg_attr(not(feature = "inproc-av"), allow(unused_variables))] allow_inproc: bool,sequence:Arc<std::sync::atomic::AtomicU64>,
    settings: VideoSettings, #[cfg_attr(not(feature = "inproc-av"), allow(unused_variables))] idr: Arc<AtomicBool>, budget: Arc<Semaphore>,
)->Result<(),String>{
    let selected_output=if display.starts_with("wayland:") {crate::wayland::source(settings.source_id)} else {None};
    let size=selected_output.map_or(size,|s|(s.width,s.height));
    let kms=display.starts_with("kms:");
    let wayland=kms || display.starts_with("wayland:");
    let (permits_tx,mut permits_rx)=mpsc::channel::<OwnedSemaphorePermit>(4);
    let kms_capture=if kms {
        let capture=tokio::task::spawn_blocking(crate::kms::Capture::open).await.map_err(|_|"KMS worker failed")??;
        if (capture.width,capture.height)!=size { return Err("KMS mode changed; restart host to refresh desktop geometry".into()); }
        Some(Arc::new(capture))
    } else { None };
    #[cfg(feature = "inproc-av")]
    if allow_inproc && native_inproc(&display, portal.is_some(), encoder) {
        let max_kbps = settings.max_kbps().unwrap_or(30_000) as i64;
        let config = crate::inproc_encoder::Config {
            display: display.clone(), input: size, output: settings.output, fps: u32::from(settings.fps),
            gop: gop_frames(settings.fps, true) as u32, cq: settings.cq() as u32,
            target_bps: settings.max_kbps().map_or(16_000, |k| i64::from(k) * 3 / 4) * 1000,
            max_bps: max_kbps * 1000, buffer_bits: i64::from(buffer_for(&settings, encoder)) * 1000, codec: settings.codec, chroma: settings.chroma,
            vaapi: encoder == Encoder::Vaapi,
        };
        log::info!("Desktop encoder: display={} source={}x{} output={:?} fps={} cq={} maxrate={}k encoder={} pipeline=in-process",
            display,size.0,size.1,settings.output,settings.fps,config.cq,max_kbps,encoder.name_for(settings.codec));
        return read_inprocess(config, latest, sequence, idr, budget).await;
    }
    let source_size = size;
    let size = format!("{}x{}", size.0, size.1);
    let fps = settings.fps.to_string();
    // Keep the IDR interval at 2 s whatever the frame rate.
    let gop = gop_frames(settings.fps, false).to_string();
    let (maxrate, _) = rate_args(&settings, encoder);
    let cq = settings.cq().to_string();
    // x11grab delivers bgr0; the Wayland/portal/KMS pipe is bgra.
    let rgb = nvenc_rgb_input(&settings, encoder, if wayland { "bgra" } else { "bgr0" });
    log::info!("Desktop encoder: display={} source={} output={:?} fps={} cq={} maxrate={} encoder={} pix_fmt={} low_latency=true",display,size,settings.output,settings.fps,cq,maxrate,encoder.name_for(settings.codec),if encoder == Encoder::Vaapi { "nv12" } else { rgb.unwrap_or(chroma_args(&settings, encoder).1) });
    // A bundled FFmpeg prefix (config ffmpeg_root / UU_FFMPEG_ROOT) or `ffmpeg` on PATH.
    let ffmpeg = ffmpeg_spec();
    let mut command = ffmpeg.command();
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
    command.args(scale_options(&settings, encoder, Some(source_size)));
    command.args(encoder_options(&settings, encoder, &gop, rgb));
    // x11grab paces against an absolute schedule; after a capture stall it grabs
    // back-to-back to "catch up" (measured 69 frames in 200ms). vfr on a 1/FPS
    // timebase drops those same-slot frames instead of encoding a burst. The
    // legacy -vsync 0 path (FFmpeg 4.x) passes catch-up frames straight through
    // and encodes the whole burst; a known degradation on old FFmpeg.
    // Native Wayland: the capture process writes straight into the encoder's stdin;
    // relaying 33 MB 4K frames through this process cost a full extra copy per frame.
    let mut native_capture=None;
    if wayland && kms_capture.is_none() && portal.is_none() {
        // Linux keeps the running executable inode accessible after a scoped
        // development/release replacement; current_exe may name a deleted path.
        #[cfg(target_os="linux")]
        let executable=std::path::PathBuf::from("/proc/self/exe");
        #[cfg(not(target_os="linux"))]
        let executable=std::env::current_exe().map_err(|e|format!("Capture executable: {e}"))?;
        let mut capture_command=wayland_capture_command(&executable,settings.fps);
        if let Some(output)=selected_output {capture_command.env("UU_WAYLAND_OUTPUT",&output.name);}
        let mut capture=capture_command
            .env("UU_WAYLAND_DEMAND","1")
            .kill_on_drop(true).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::inherit())
            .spawn().map_err(|e|format!("Start Wayland capture: {e}"))?;
        let source:std::process::Stdio=capture.stdout.take().ok_or("Capture stdout missing")?
            .try_into().map_err(|e|format!("Capture pipe: {e}"))?;
        command.stdin(source);
        native_capture=Some(capture);
    } else if wayland { command.stdin(std::process::Stdio::piped()); }
    let timing = timing_args(ffmpeg_timing_mode(&ffmpeg).await, settings.fps);
    let mut output_args = timing;
    output_args.extend(["-f".into(), if settings.codec == VideoCodec::H265 { "hevc".into() } else { "h264".into() }, "pipe:1".into()]);
    let mut child=command.args(output_args)
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
            let mut bytes=vec![0;capture.width as usize*capture.height as usize*4];
            loop {
                tick.tick().await;
                let permit=budget.clone().acquire_owned().await.map_err(|_|"Encoder stopped")?;
                permits_tx.send(permit).await.map_err(|_|"Encoder stopped")?;
                let capture=capture.clone();
                bytes=tokio::task::spawn_blocking(move || {
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
            let mut frame=vec![0;(session.width as usize)*(session.height as usize)*4];
            let mut version=None;
            loop {
                tick.tick().await;
                let permit=budget.clone().acquire_owned().await.map_err(|_|"Encoder stopped")?;
                permits_tx.send(permit).await.map_err(|_|"Encoder stopped")?;
                let session=session.clone();
                (frame,version)=tokio::task::spawn_blocking(move || {
                    session.capture_changed(&mut frame,&mut version)?;
                    Ok::<_,String>((frame,version))
                }).await.map_err(|e|format!("Portal capture worker: {e}"))??;
                sink.write_all(&frame).await.map_err(|e|format!("Portal encoder pipe: {e}"))?;
            }
        })))
    } else if let Some(mut capture)=native_capture {
        let mut demand=capture.stdin.take().ok_or("Capture stdin missing")?;
        Some(EncoderTask(tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            tokio::select! {
                result=async {
                    let mut tick=tokio::time::interval(Duration::from_nanos(1_000_000_000/u64::from(settings.fps)));
                    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    loop {
                        let permit=budget.clone().acquire_owned().await.map_err(|_|"Encoder stopped")?;
                        tick.tick().await;
                        permits_tx.send(permit).await.map_err(|_|"Encoder stopped")?;
                        demand.write_all(&[1]).await.map_err(|_|"Capture demand pipe closed")?;
                    }
                } => result,
                status=capture.wait() => {
                    let status=status.map_err(|e|format!("Wayland capture: {e}"))?;
                    // Nothing awaits this task; log so the cause survives the generic encoder EOF.
                    log::warn!("Wayland capture exited: {status}");
                    Err(format!("Wayland capture exited: {status}"))
                },
            }
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
            let aud = au_offsets_for(&pending, settings.codec);
            if aud.len() < 2 { break; }
            let next = aud[1];
            if next > MAX_ACCESS_UNIT { return Err("Encoder access unit exceeds limit".into()); }
            let frame: Vec<u8> = pending.drain(..next).collect();
            let sequence = sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            let permit=if wayland {Some(permits_rx.recv().await.ok_or("Capture producer stopped")?)} else {None};
            gate.submit(&latest, Arc::new(CompleteFrame { source_generation: settings.source_generation, source_id: settings.source_id,
                codec: settings.codec, sequence, completed_at: Instant::now(), data: frame, _permit: permit,
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

pub(crate) fn transport_closed(pc: &RTCPeerConnection) -> bool {
    matches!(pc.connection_state(), RTCPeerConnectionState::Closed | RTCPeerConnectionState::Failed)
        || pc.sctp().state() == webrtc::sctp_transport::sctp_transport_state::RTCSctpTransportState::Closed
}

fn probe_settings() -> Result<VideoSettings,String> {
    let fps=crate::wayland::capture_fps(std::env::var("UU_PROBE_FPS").ok().as_deref())?;
    // UU_PROBE_QUALITY selects a viewer preset (1 fast 1M, 2 general 2M, 3 HD 8M, 4 bluray 20M,
    // 5 auto); bitrate presets exercise the capped/ABR encoder path instead of CRF.
    let quality=match std::env::var("UU_PROBE_QUALITY").ok().as_deref() {
        None=>crate::capture_setting::QUALITY_AUTO,
        Some(v)=>v.parse::<u64>().ok().filter(|q|(1..=5).contains(q)).ok_or("UU_PROBE_QUALITY must be 1-5")?,
    };
    let mut settings=VideoSettings {fps,quality,codec:probe_codec(),..VideoSettings::default()};
    // UU_PROBE_CHROMA=444 requests 真彩 like the viewer does (only where full_chroma_available()).
    if std::env::var("UU_PROBE_CHROMA").as_deref()==Ok("444") && full_chroma_available() { settings.chroma=crate::capture_setting::CHROMA_444; }
    Ok(settings)
}

/// The probe follows `UU_VIDEO_CODEC` so the HEVC path can be measured at all (H.264 otherwise),
/// and its first access unit is then checked against that codec instead of always H.264.
fn probe_codec() -> VideoCodec { crate::capture_setting::preferred_codec(true, true) }

fn probe_input_command(spec:&FfmpegSpec,input:&std::ffi::OsStr,loop_input:bool,settings:&VideoSettings,
    encoder:Encoder,wanted:u64,timing:FfmpegTimingMode)->Command {
    let mut command=spec.command();
    command.args(["-hide_banner","-loglevel","error","-nostdin"]);
    if encoder==Encoder::Vaapi { command.args(["-vaapi_device",&vaapi_device()]); }
    if loop_input { command.args(["-stream_loop","-1"]); }
    // Override source timestamps so clips and image sequences use the same nominal probe rate.
    command.args(["-r",&settings.fps.to_string(),"-i"]).arg(input).args(["-map","0:v:0","-an"]);
    command.args(scale_options(settings,encoder,None));
    command.args(encoder_options(settings,encoder,&gop_frames(settings.fps,false).to_string(),None));
    command.args(timing_args(timing,settings.fps));
    command.args(["-frames:v",&wanted.to_string(),"-f",if settings.codec==VideoCodec::H265 {"hevc"} else {"h264"},"pipe:1"]);
    command
}

async fn probe_input_size(spec:&FfmpegSpec,input:&std::ffi::OsStr)->Result<(u32,u32),String> {
    // Read only the PNG header; never log decoded pixels, input paths or FFmpeg diagnostics.
    let mut child=spec.command().args(["-hide_banner","-loglevel","error","-nostdin","-i"]).arg(input)
        .args(["-map","0:v:0","-frames:v","1","-c:v","png","-f","image2pipe","pipe:1"])
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null())
        .kill_on_drop(true).spawn().map_err(|e|format!("Start fixed-input probe: {e}"))?;
    let mut header=[0u8;24];
    let result=tokio::time::timeout(Duration::from_secs(15),child.stdout.as_mut().unwrap().read_exact(&mut header)).await;
    let _=child.kill().await;
    if !matches!(result,Ok(Ok(_))) || &header[..8]!=b"\x89PNG\r\n\x1a\n" {
        return Err("Cannot open/decode UU_PROBE_INPUT video input".into());
    }
    Ok((u32::from_be_bytes(header[16..20].try_into().unwrap()),u32::from_be_bytes(header[20..24].try_into().unwrap())))
}

/// Fixed input bypasses desktop initialization and always uses the FFmpeg process encoder.
pub async fn probe_input(input:OsString,path:Option<String>)->Result<(),String> {
    use std::io::Write;
    let settings=probe_settings()?;
    let wanted:u64=std::env::var("UU_PROBE_FRAMES").ok().and_then(|v|v.parse().ok()).unwrap_or(5);
    if wanted==0 { return Err("Fixed-input UU_PROBE_FRAMES must be positive".into()); }
    let spec=ffmpeg_spec();
    let size=probe_input_size(&spec,&input).await?;
    let encoder=Encoder::requested();
    let (maxrate,_)=rate_args(&settings,encoder);
    let output=settings.output.unwrap_or(size);
    println!("Fixed input probe: source={}x{} output={}x{} fps={} cq={} maxrate={} encoder={}",
        size.0,size.1,output.0,output.1,settings.fps,settings.cq(),maxrate,encoder.name_for(settings.codec));
    let mut file=match path {
        Some(path)=>Some(std::fs::OpenOptions::new().write(true).create_new(true).open(path).map_err(|e|format!("Video probe file: {e}"))?),
        None=>None,
    };
    let mut command=probe_input_command(&spec,&input,std::env::var("UU_PROBE_LOOP").as_deref()==Ok("1"),
        &settings,encoder,wanted,ffmpeg_timing_mode(&spec).await);
    let mut child=command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null()).kill_on_drop(true).spawn().map_err(|e|format!("Start fixed-input encoder: {e}"))?;
    let mut stdout=child.stdout.take().ok_or("Encoder stdout missing")?;
    let (mut count,mut bytes)=(0u64,0usize);
    tokio::time::timeout(Duration::from_secs(15+wanted/30),async {
        let mut pending=Vec::new();
        let mut chunk=[0u8;16384];
        loop {
            let n=stdout.read(&mut chunk).await.map_err(|e|format!("Encoder read: {e}"))?;
            pending.extend_from_slice(&chunk[..n]);
            loop {
                let aud=au_offsets_for(&pending,settings.codec);
                let end=if aud.len()>1 { aud[1] } else if n==0 && !pending.is_empty() { pending.len() } else { break; };
                if end>MAX_ACCESS_UNIT { return Err("Encoder access unit exceeds limit".into()); }
                if count==0 && !is_independent_frame_for(&pending[..end], settings.codec) { return Err("Video probe did not begin with IDR".into()); }
                if let Some(file)=file.as_mut() { file.write_all(&pending[..end]).map_err(|e|e.to_string())?; }
                bytes+=end; count+=1;
                pending.drain(..end);
            }
            if pending.len()>MAX_ACCESS_UNIT { return Err("Encoder access unit exceeds limit".into()); }
            if n==0 { break; }
        }
        let status=child.wait().await.map_err(|e|format!("Fixed-input encoder: {e}"))?;
        if !status.success() { return Err(format!("Fixed-input encoder failed ({status}); check input and encoder availability")); }
        if count!=wanted { return Err(format!("Fixed input ended after {count} frames; requested {wanted} (use UU_PROBE_LOOP=1 to repeat)")); }
        Ok::<(),String>(())
    }).await.map_err(|_|"Video probe timed out".to_owned())??;
    println!("Encoded capture probe: frames={count} h264_bytes={bytes} first_frame_idr=true");
    Ok(())
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
    let settings=probe_settings()?;
    let _reader=EncoderTask(tokio::spawn(read_encoder(size,display,portal,tx,settings,sequence,Arc::new(AtomicBool::new(false)),Arc::new(Semaphore::new(4)),None)));
    let mut count=0;
    let mut bytes=0;
    // UU_PROBE_FRAMES lengthens the sample for baseline measurements; default is 5 frames.
    let wanted:u64=std::env::var("UU_PROBE_FRAMES").ok().and_then(|v|v.parse().ok()).unwrap_or(5);
    tokio::time::timeout(Duration::from_secs(15+wanted/30),async {
        while count<wanted {
            let frame=rx.recv().await.ok_or("Video probe encoder ended")?;
            if count==0 && !is_independent_frame_for(&frame.data, settings.codec) { return Err("Video probe did not begin with IDR".to_owned()); }
            if let Some(file)=file.as_mut() { file.write_all(&frame.data).map_err(|e|e.to_string())?; }
            bytes+=frame.data.len(); count+=1;
        }
        Ok::<(),String>(())
    }).await.map_err(|_|"Video probe timed out".to_owned())??;
    println!("Encoded capture probe: frames={count} h264_bytes={bytes} first_frame_idr=true");
    Ok(())
}

pub async fn stream_desktop(pc: Arc<RTCPeerConnection>, track: Arc<crate::video_track::VideoTrack>, mut size: (u32, u32), display: String, portal:Option<Arc<crate::portal::PortalSession>>,
    mut settings: tokio::sync::watch::Receiver<VideoSettings>, applied: Arc<tokio::sync::watch::Sender<VideoSettings>>, idr: Arc<AtomicBool>) -> Result<(), String> {
    let _desktop_mode = crate::desktop_mode::StreamGuard::new();
    loop {
        if transport_closed(&pc) { return Ok(()); }
        if pc.connection_state() == RTCPeerConnectionState::Connected
            && pc.sctp().state() == webrtc::sctp_transport::sctp_transport_state::RTCSctpTransportState::Connected { break; }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Portal power/idle policy belongs to its CONTROL-scoped supervisor,
    // including when video has no frames or the user locks manually.
    if portal.is_none() { crate::capture::keep_display_awake()?; }
    let portal_stream = portal.is_some();
    let (latest, mut receiver) = mpsc::channel(4);
    let budget=Arc::new(Semaphore::new(4));
    let sequence = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut current = *settings.borrow_and_update();
    if !track.supports(current.codec) && track.supports(VideoCodec::H265) {
        current.codec = VideoCodec::H265; // H265-only remote video offer.
        applied.send_replace(current);
        settings.borrow_and_update();
    }
    // One value per stream: read_encoder replaces it when the encoder opens and when it falls back.
    // SIGUSR2 only reads it. A oneshot/mpsc try_recv dropped the report on a codec-only reload.
    let (opened_tx, mut opened_rx) = tokio::sync::watch::channel(Encoder::Cpu);
    let mut reader = EncoderTask(tokio::spawn(read_encoder(size, display.clone(), portal.clone(), latest.clone(), current, sequence.clone(), idr.clone(), budget.clone(), Some(opened_tx.clone()))));
    let mut seen_reload = crate::video_select::reload_generation().load(std::sync::atomic::Ordering::Acquire);
    let mut switch_encoder = false;
    let mut applied_codec_pref = crate::video_select::selection().codec_pref;
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
    // sent/skipped are cumulative; window_* and max_* cover the interval since the last stats line.
    let (mut window_frames, mut window_skipped) = (0u64, 0u64);
    let mut send_interrupted=false;
    'stream: loop {
        let reload = crate::video_select::reload_generation().load(std::sync::atomic::Ordering::Acquire);
        if reload != seen_reload {
            seen_reload = reload;
            let before_pref = applied_codec_pref;
            let _ = crate::system_actions::reload_settings();
            let after = crate::video_select::selection();
            let requested = crate::video_select::encoder_for_chroma(after, current.chroma, crate::video_select::cached_probe().as_ref());
            let running = watch_running(&opened_rx);
            let (encoder, switch) = encoder_after_reload(running, requested);
            if switch {
                let chroma = crate::video_select::chroma_after_switch(encoder, current.chroma);
                log::info!("Encoder switch: {} -> {} reason=setting chroma={}",
                    format!("{running:?}").to_ascii_lowercase(), format!("{encoder:?}").to_ascii_lowercase(),
                    if chroma == crate::capture_setting::CHROMA_444 { "444" } else { "420" });
                idr.store(true, std::sync::atomic::Ordering::Release);
                switch_encoder = true;
                applied.send_modify(|value| value.chroma = chroma);
            } else if codec_pref_changed(before_pref, after.codec_pref) {
                log::info!("Codec preference changed; takes effect on the next desktop stream");
            }
            applied_codec_pref = after.codec_pref;
        }
        let next = *settings.borrow_and_update();
        if next != current || send_interrupted || switch_encoder {
            let discard = next.codec != current.codec || next.source_generation != current.source_generation || next.source_id != current.source_id || send_interrupted;
            let restart_same = switch_encoder && !discard;
            current = next;
            timeline.fps = u64::from(next.fps);
            switch_encoder = false;
            // A viewer resolution choice may have changed the X11 display mode (desktop_mode).
            if let Some(resized) = x11_resized(&display, portal_stream, size) { size = resized; }
            if restart_same {
                // Same order as discard: the old encoder must exit before the queue is drained
                // and the shared IDR flag is set. Otherwise it can clear that flag and enqueue
                // one more frame ahead of the new IDR. In-process uses the same flag.
                reader.0.abort();
                let _ = (&mut reader.0).await;
                while receiver.try_recv().is_ok() {}
                idr.store(true, std::sync::atomic::Ordering::Release);
                reader.0 = tokio::spawn(read_encoder(size, display.clone(), portal.clone(), latest.clone(), next, sequence.clone(), idr.clone(), budget.clone(), Some(opened_tx.clone())));
            } else if discard {
                reader.0.abort();
                let _ = (&mut reader.0).await;
                while receiver.try_recv().is_ok() {}
                reader.0 = tokio::spawn(read_encoder(size, display.clone(), portal.clone(), latest.clone(), next, sequence.clone(), idr.clone(), budget.clone(), Some(opened_tx.clone())));
            } else {
                reader.restart(read_encoder(size, display.clone(), portal.clone(), latest.clone(), next, sequence.clone(), idr.clone(), budget.clone(), Some(opened_tx.clone()))).await;
            }
            send_interrupted=false;
        }
        tokio::select! {
            result = &mut reader.0 => {
                let result = result.map_err(|e| format!("Encoder reader task: {e}"))?;
                if let Some(resized) = result.as_ref().err().and_then(|_| x11_resized(&display, portal_stream, size)) {
                    log::info!("Desktop geometry changed during the stream ({}x{} -> {}x{}); restarting capture", size.0, size.1, resized.0, resized.1);
                    size = resized;
                    while receiver.try_recv().is_ok() {}
                    reader.0 = tokio::spawn(read_encoder(size, display.clone(), portal.clone(), latest.clone(), current, sequence.clone(), idr.clone(), budget.clone(), Some(opened_tx.clone())));
                    continue;
                }
                if let (Err(error), VideoCodec::H265, true) = (&result, current.codec, track.supports(VideoCodec::H264)) {
                    log::warn!("HEVC stream failed ({error}); falling back to negotiated H264");
                    applied.send_modify(|value| value.codec = VideoCodec::H264);
                    current = *settings.borrow_and_update();
                    while receiver.try_recv().is_ok() {}
                    reader.0 = tokio::spawn(read_encoder(size, display.clone(), portal.clone(), latest.clone(), current, sequence.clone(), idr.clone(), budget.clone(), Some(opened_tx.clone())));
                    continue;
                }
                if result.is_err() && !crate::wayland::outputs().is_empty() && !crate::wayland::input_ready() {
                    // Leave the stream task alive so the request worker can roll back
                    // a failed capture/encoder and restart the previous source.
                    crate::wayland::failed(current.source_id,current.source_generation);
                    loop {
                        tokio::select! {
                            changed=settings.changed()=>{if changed.is_err(){return result;}break;},
                            _=health.tick()=>{if transport_closed(&pc){return Ok(());}},
                        }
                    }
                    send_interrupted=true;
                    continue;
                }
                return result;
            },
            _ = health.tick() => { if transport_closed(&pc) { return Ok(()); } },
            changed = settings.changed(), if settings_open => {
                if changed.is_err() { settings_open = false; }
                continue;
            },
            frame = receiver.recv() => {
                // A closed queue means the reader ended; its JoinHandle carries the reason.
                let Some(frame) = frame else { return (&mut reader.0).await.map_err(|e| format!("Encoder reader task: {e}"))?; };
                if !frame.matches(&current) { continue; }
                let duration = timeline.duration_after(frame.completed_at);
                if let Some(last) = previous { skipped += frame.sequence.saturating_sub(last + 1); }
                max_age_ms = max_age_ms.max(frame.completed_at.elapsed().as_millis());
                // Continue monitoring close even if the network send stalls.
                let sample = Sample {
                    data: frame.data.clone().into(), duration, ..Default::default()
                };
                let extensions = [crate::rtp_metadata::capture_index(0), crate::rtp_metadata::new_frame()];
                let sent_before=track.revision();
                let send = track.write_sample_with_extensions(frame.codec, &sample, &extensions);
                let send_started=Instant::now();
                tokio::pin!(send);
                loop {
                    tokio::select! {
                        result = &mut send => { result.map_err(|e| format!("Video send: {e}"))?; break; },
                        _ = health.tick() => { if transport_closed(&pc) { return Ok(()); } },
                        changed = settings.changed(), if settings_open => {
                            if changed.is_err() { settings_open=false; }
                            else if *settings.borrow()!=current { send_interrupted=true; continue 'stream; }
                        },
                    }
                }
                if !crate::wayland::outputs().is_empty() && track.revision()>sent_before {
                    crate::wayland::presented(current.source_id,current.source_generation);
                }
                previous = Some(frame.sequence);
                max_send_ms=max_send_ms.max(send_started.elapsed().as_millis());
                frames += 1;
                if stats.elapsed() >= Duration::from_secs(30) {
                    if !portal_stream { crate::capture::keep_display_awake()?; }
                    log::info!("Desktop latest-frame stats: sent={} skipped={} latest_seq={} window_s={} window_sent={} window_skipped={} max_slot_age_ms={} max_send_ms={}",
                        frames, skipped, frame.sequence, stats.elapsed().as_secs(), frames - window_frames, skipped - window_skipped, max_age_ms, max_send_ms);
                    (window_frames, window_skipped) = (frames, skipped);
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
    fn reload_compares_the_encoder_the_stream_opened() {
        // Probe picked VAAPI; the pre-probe legacy guess was NVENC. set-encoder nvenc must switch.
        let (next, switch) = super::encoder_after_reload(Encoder::Vaapi, Encoder::Nvenc);
        assert_eq!(next, Encoder::Nvenc);
        assert!(switch);
        let (same, switch) = super::encoder_after_reload(Encoder::Nvenc, Encoder::Nvenc);
        assert_eq!(same, Encoder::Nvenc);
        assert!(!switch);
        // Runtime fallback to CPU is the new baseline: set-encoder nvenc switches back.
        let (next, switch) = super::encoder_after_reload(Encoder::Cpu, Encoder::Nvenc);
        assert!(switch);
        assert_eq!(next, Encoder::Nvenc);
    }

    #[test]
    fn reload_reads_the_running_encoder_without_consuming_it() {
        // set-codec then set-encoder: both reloads see the encoder this stream opened.
        let (tx, rx) = tokio::sync::watch::channel(Encoder::Cpu);
        tx.send(Encoder::Vaapi).unwrap();
        let (_, switch) = super::encoder_after_reload(super::watch_running(&rx), Encoder::Vaapi);
        assert!(!switch, "codec reload must not look like an encoder change");
        let (_, switch) = super::encoder_after_reload(super::watch_running(&rx), Encoder::Nvenc);
        assert!(switch, "the open report is still there for set-encoder");
        // Fallback overwrites the request; the next reload compares against CPU.
        tx.send(Encoder::Nvenc).unwrap();
        tx.send(Encoder::Cpu).unwrap();
        assert_eq!(super::watch_running(&rx), Encoder::Cpu);
        let (_, switch) = super::encoder_after_reload(super::watch_running(&rx), Encoder::Nvenc);
        assert!(switch);
    }

    #[tokio::test]
    async fn restart_same_stops_before_drain_and_idr() {
        // Old producer keeps the IDR flag and one queued frame until it is joined.
        let idr = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let stop = Arc::new(tokio::sync::Notify::new());
        let mut old = {
            let idr = idr.clone();
            let stop = stop.clone();
            tokio::spawn(async move {
                stop.notified().await;
                idr.store(false, std::sync::atomic::Ordering::Release);
                tx.try_send(1u8).ok();
            })
        };
        let order = super::stop_then_drain_then_idr(&mut old, &mut rx, &idr, &stop).await;
        assert_eq!(order, vec!["stopped", "drained", "idr"]);
        assert!(rx.try_recv().is_err());
        assert!(idr.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn probe_size_uses_the_desktop_and_falls_back_when_unknown() {
        assert_eq!(super::probe_size((3840, 2160)), (3840, 2160));
        // 4:2:0 rejects odd sides (portal logical size can be odd).
        assert_eq!(super::probe_size((2560, 2507)), (2560, 2508));
        // Capture init failed (0×0): same nominal size as a headless tool peer.
        assert_eq!(super::probe_size((0, 0)), (1920, 1080));
        assert_eq!(super::probe_size((1920, 0)), (1920, 1080));
    }

    #[test]
    #[test]
    fn timed_out_inproc_probe_fails_when_the_thread_does_not_exit() {
        assert!(super::probe_join_outcome(true));
        assert!(!super::probe_join_outcome(false));
    }

    #[test]
    fn hevc_auto_uses_the_probed_encoder() {
        use crate::video_select::Source;
        // Legacy guess is CPU; the probe later picks VAAPI. The m-line must follow VAAPI.
        assert_eq!(super::hevc_encoder_from(Source::Auto, Encoder::Cpu, Some(Encoder::Vaapi)), Encoder::Vaapi);
        assert_eq!(super::hevc_encoder_from(Source::Auto, Encoder::Cpu, None), Encoder::Cpu);
        // Explicit cpu stays cpu even when a probe exists.
        assert_eq!(super::hevc_encoder_from(Source::Setting, Encoder::Cpu, Some(Encoder::Vaapi)), Encoder::Cpu);
        assert_eq!(super::hevc_encoder_from(Source::Env, Encoder::Nvenc, None), Encoder::Nvenc);
    }

    #[test]
    fn codec_notice_compares_against_the_applied_preference() {
        use crate::video_select::CodecPref;
        assert!(super::codec_pref_changed(CodecPref::Auto, CodecPref::H265));
        assert!(!super::codec_pref_changed(CodecPref::H265, CodecPref::H265));
    }

    #[test]
    fn nvidia_display_is_detected_from_connected_connectors() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("uu-drm-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let card = |n: &str, driver: &str, connector: &str, status: &str| {
            std::fs::create_dir_all(root.join(format!("drivers/{driver}"))).unwrap();
            std::fs::create_dir_all(root.join(format!("drm/{n}/device"))).unwrap();
            let _ = symlink(root.join(format!("drivers/{driver}")), root.join(format!("drm/{n}/device/driver")));
            std::fs::create_dir_all(root.join(format!("drm/{n}-{connector}"))).unwrap();
            std::fs::write(root.join(format!("drm/{n}-{connector}/status")), format!("{status}\n")).unwrap();
        };
        let drm = root.join("drm");
        card("card0", "i915", "HDMI-A-1", "connected");
        card("card1", "nvidia", "DP-1", "disconnected");
        assert!(!super::nvidia_drives_display(&drm), "iGPU drives the display");
        card("card1", "nvidia", "DP-2", "connected");
        assert!(super::nvidia_drives_display(&drm), "NVIDIA drives a display");
        assert!(!super::nvidia_drives_display(&root.join("missing")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn native_frame_dimensions_are_padded_to_even() {
        // A scaled compositor can report an odd logical desktop (e.g. two stacked 150% monitors →
        // 2560x2507); H.264/HEVC 4:2:0 encoders reject odd dimensions, so a native-size capture
        // with a known odd size gets a pad filter. Even sizes, explicit output scaling and the
        // fixed-input probe (size unknown) keep their existing filter chains.
        let native = VideoSettings { output: None, ..Default::default() };
        let pad = "pad=ceil(iw/2)*2:ceil(ih/2)*2:0:0:black";
        for encoder in [Encoder::Cpu, Encoder::Nvenc] {
            assert_eq!(scale_options(&native, encoder, Some((2560, 2507))), vec!["-vf".to_string(), pad.to_string()]);
            assert!(scale_options(&native, encoder, Some((2560, 2508))).is_empty());
            assert!(scale_options(&native, encoder, None).is_empty());
        }
        assert_eq!(scale_options(&native, Encoder::Vaapi, Some((2560, 2507))),
            vec!["-vf".to_string(), format!("{pad},format=nv12,hwupload")]);
        assert_eq!(scale_options(&native, Encoder::Vaapi, Some((2560, 2508))),
            vec!["-vf".to_string(), "format=nv12,hwupload".to_string()]);
        let scaled = VideoSettings { output: Some((1280, 720)), ..Default::default() };
        assert_eq!(scale_options(&scaled, Encoder::Cpu, Some((2560, 2507))),
            vec!["-vf".to_string(), "scale=1280:720:flags=bilinear".to_string()]);
    }

    #[test]
    fn fixed_input_arguments_share_production_encoding_options() {
        let spec=FfmpegSpec {program:"ffmpeg".into(),ld_library_path:None,path:None};
        for encoder in [Encoder::Cpu,Encoder::Nvenc,Encoder::Vaapi] {
            for quality in 1..=5 {
                for chroma in [crate::capture_setting::CHROMA_420,crate::capture_setting::CHROMA_444] {
                    let settings=VideoSettings {fps:30,quality,chroma,output:Some((640,360)),..Default::default()};
                    let command=probe_input_command(&spec,std::ffi::OsStr::new("frames/%05d.png"),false,
                        &settings,encoder,17,FfmpegTimingMode::Modern);
                    let args:Vec<_>=command.as_std().get_args().map(|a|a.to_str().unwrap().to_owned()).collect();
                    let expected=encoder_options(&settings,encoder,&gop_frames(30,false).to_string(),None);
                    assert!(args.windows(expected.len()).any(|w|w==expected));
                    let scale=scale_options(&settings,encoder,None);
                    assert!(args.windows(scale.len()).any(|w|w==scale));
                    assert!(args.windows(4).any(|w|w==["-r","30","-i","frames/%05d.png"]));
                    assert!(args.windows(2).any(|w|w==["-frames:v","17"]));
                    assert!(args.windows(4).any(|w|w==["-fps_mode","vfr","-enc_time_base","1/30"]));
                    assert!(!args.iter().any(|a|["x11grab","rawvideo","-stream_loop"].contains(&a.as_str())));
                }
            }
        }
    }

    #[test]
    fn fixed_input_loop_is_an_input_option_and_keeps_paths_literal() {
        let spec=FfmpegSpec {program:"ffmpeg".into(),ld_library_path:None,path:None};
        for input in ["clip.y4m","a clip.mp4","frames/%05d.png"] {
            let command=probe_input_command(&spec,std::ffi::OsStr::new(input),true,&VideoSettings::default(),
                Encoder::Cpu,5,FfmpegTimingMode::Legacy);
            let args:Vec<_>=command.as_std().get_args().map(|a|a.to_str().unwrap()).collect();
            assert!(args.windows(2).any(|w|w==["-i",input]));
            assert!(args.windows(2).any(|w|w==["-stream_loop","-1"]));
            assert!(args.iter().position(|a|*a=="-stream_loop")<args.iter().position(|a|*a=="-i"));
            assert!(args.windows(2).any(|w|w==["-vsync","0"]));
            assert!(!args.contains(&"-vf"));
        }
    }

    #[test]
    fn display_refresh_parsers() {
        let xrandr = "Screen 0: minimum 8 x 8, current 5760 x 2160\nHDMI-1 connected 1920x1080+3840+0 (normal) 0mm x 0mm\n   1920x1080     60.00*+  50.00\nDP-1 connected primary 3840x2160+0+0 (normal) 600mm x 340mm\n   3840x2160     60.00 +  143.98*\n   2560x1440    120.00\n";
        assert_eq!(xrandr_refresh(xrandr), Some(143.98));
        assert_eq!(xrandr_refresh("Screen 0:\nVGA-1 disconnected\nHDMI-1 connected 1920x1080+0+0\n   1920x1080     74.97*+  60.00\n"), Some(74.97));
        assert_eq!(xrandr_refresh("Screen 0:\nHDMI-1 connected\n   1920x1080     60.00 +\n"), None);
        assert_eq!(hyprland_refresh(r#"[{"name":"HDMI-A-1","focused":false,"refreshRate":60.0},{"name":"DP-1","focused":true,"refreshRate":143.856}]"#), Some(143.856));
        assert_eq!(hyprland_refresh("[]"), None);
    }

    #[test]
    fn native_wayland_child_uses_requested_capture_rate() {
        for fps in [5,30,60,90,144] {
            let command=super::wayland_capture_command(std::path::Path::new("uuremote-server"),fps);
            let value=fps.to_string();
            assert!(command.as_std().get_envs().any(|(k,v)|k=="UU_WAYLAND_FPS" && v==Some(std::ffi::OsStr::new(&value))));
        }
    }
    use super::*;
    #[tokio::test]
    async fn frame_budget_spans_queued_sending_and_restarted_encoders() {
        let budget=Arc::new(Semaphore::new(2));
        let (tx,mut rx)=mpsc::channel(4);
        let mut gate=DependencyGate::default();
        for sequence in 1..=2 {
            let permit=budget.clone().acquire_owned().await.unwrap();
            gate.submit(&tx,Arc::new(CompleteFrame { source_generation: 0, source_id: 0,codec:VideoCodec::H264,sequence,completed_at:Instant::now(),
                data:vec![0,0,1,5,42],_permit:Some(permit)}));
        }
        let sending=rx.recv().await.unwrap();
        assert_eq!(budget.available_permits(),0);
        let restarted=tokio::spawn(budget.clone().acquire_owned());
        tokio::task::yield_now().await;
        assert!(!restarted.is_finished());
        drop(sending);
        let permit=tokio::time::timeout(Duration::from_secs(1),restarted).await.unwrap().unwrap().unwrap();
        assert_eq!(budget.available_permits(),0);
        drop(rx);
        assert_eq!(budget.available_permits(),1);
        drop(permit);
        assert_eq!(budget.available_permits(),2);
    }
    #[test]
    fn timing_help_requires_both_modern_options() {
        let modern = "  -fps_mode mode  video sync method\n  -enc_time_base ratio  set encoder timebase";
        assert_eq!(timing_mode_from_help(modern), FfmpegTimingMode::Modern);
        assert_eq!(timing_mode_from_help("  -vsync parameter\n"), FfmpegTimingMode::Legacy);
        assert_eq!(timing_mode_from_help("  -fps_mode mode\n"), FfmpegTimingMode::Legacy);
        assert_eq!(timing_mode_from_help("  -enc_time_base ratio\n"), FfmpegTimingMode::Legacy);
        assert_eq!(timing_mode_from_help("  --not-fps_mode-test\n"), FfmpegTimingMode::Legacy);
    }

    #[test]
    fn timing_args_match_ffmpeg_generation() {
        assert_eq!(timing_args(FfmpegTimingMode::Modern, 60),
            vec!["-fps_mode", "vfr", "-enc_time_base", "1/60"]);
        assert_eq!(timing_args(FfmpegTimingMode::Legacy, 30), vec!["-vsync", "0"]);
        assert!(!timing_args(FfmpegTimingMode::Modern, 60).contains(&"-r".to_owned()));
    }

    #[test]
    fn timing_probe_failure_keeps_modern_args() {
        let modern = b"  -fps_mode mode\n  -enc_time_base ratio\n";
        // Legacy is only confirmed from a completed help dump that lacks the options.
        assert_eq!(timing_mode_from_probe(true, b"", b"  -fps_mode mode\n"), FfmpegTimingMode::Legacy);
        // Any probe failure keeps the modern args (-vsync is deprecated on new FFmpeg).
        assert_eq!(timing_mode_from_probe(false, modern, &[]), FfmpegTimingMode::Modern);
        assert_eq!(timing_mode_from_probe(false, &[], &[]), FfmpegTimingMode::Modern);
        assert_eq!(timing_mode_from_probe(true, &[], &[]), FfmpegTimingMode::Modern);
        assert_eq!(timing_mode_from_probe(true, modern, &[]), FfmpegTimingMode::Modern);
    }

    #[tokio::test]
    async fn timing_cache_initializer_runs_once_for_concurrent_requests() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let cell = Arc::new(tokio::sync::OnceCell::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let first = cell.clone();
        let first_calls = calls.clone();
        let second = cell.clone();
        let second_calls = calls.clone();
        let (first_mode, second_mode) = tokio::join!(
            async move { *first.get_or_init(|| async {
                first_calls.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
                FfmpegTimingMode::Legacy
            }).await },
            async move { *second.get_or_init(|| async {
                second_calls.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
                FfmpegTimingMode::Modern
            }).await },
        );
        assert_eq!(first_mode, second_mode);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn ffmpeg_specs_separate_executable_environments() {
        let path = FfmpegSpec { program: PathBuf::from("ffmpeg"), ld_library_path: None,
            path: Some(OsString::from("/one")) };
        let other_path = FfmpegSpec { program: PathBuf::from("ffmpeg"), ld_library_path: None,
            path: Some(OsString::from("/two")) };
        let bundled = FfmpegSpec { program: PathBuf::from("/opt/ffmpeg/bin/ffmpeg"),
            ld_library_path: Some(OsString::from("/opt/ffmpeg/lib/x86_64-linux-gnu")), path: None };
        assert_ne!(path, other_path);
        assert_ne!(path, bundled);
    }

    #[test]
    fn x264_buffer_fits_keyframes() {
        assert_eq!(buffer_kbits(2000, Encoder::Nvenc), 250);
        assert_eq!(buffer_kbits(2000, Encoder::Vaapi), 250);
        assert_eq!(buffer_kbits(2000, Encoder::Cpu), 2000);
        assert_eq!(buffer_kbits(300, Encoder::Cpu), 500);
        assert_eq!(buffer_kbits(30_000, Encoder::Cpu), 8000);
    }

    #[test]
    fn vaapi_buffer_env_is_opt_in_and_bounded() {
        assert_eq!(vaapi_buffer_kbits(30_000, None), None);
        assert_eq!(vaapi_buffer_kbits(30_000, Some("250")), Some(7500));
        assert_eq!(vaapi_buffer_kbits(30_000, Some(" 10 ")), Some(300));
        assert_eq!(vaapi_buffer_kbits(30_000, Some("1000")), Some(30_000));
        // Out of the documented range, empty or unparsable keeps the default buffer.
        for value in ["", "9", "1001", "0", "abc", "-250", "250ms"] {
            assert_eq!(vaapi_buffer_kbits(30_000, Some(value)), None, "{value}");
        }
        // A tiny cap must not produce a zero-sized buffer.
        assert_eq!(vaapi_buffer_kbits(1, Some("10")), Some(1));
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
        let frame = |sequence, idr| Arc::new(CompleteFrame { source_generation: 0, source_id: 0, _permit: None,
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
        let frame=|sequence,idr|Arc::new(CompleteFrame{source_generation:0,source_id:0,_permit:None,sequence,codec:VideoCodec::H264,completed_at:Instant::now(),data:vec![0,0,1,9,240,0,0,1,if idr{5}else{1},42]});
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
        assert!(hevc_available((1920, 1080)).await);
    }
    #[test]
    fn hevc_annex_b_boundaries_and_irap_use_two_byte_headers() {
        let bytes = [0,0,0,1,70,1,80,0,0,1,64,1,42,0,0,1,38,1,42,0,0,1,70,1,80];
        assert_eq!(au_offsets_for(&bytes, VideoCodec::H265), vec![0,19]);
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
    #[test]
    fn hevc_access_units_do_not_depend_on_aud_placement() {
        // VAAPI writes an AUD only at IRAPs: AUD VPS SPS PPS IDR, then one P per picture.
        let vaapi = [0,0,0,1,70,1,0x80, 0,0,0,1,64,1,0x42, 0,0,0,1,66,1,0x42,
            0,0,0,1,68,1,0x42, 0,0,0,1,38,1,0x80, 0,0,0,1,2,1,0x80, 0,0,0,1,2,1,0x80];
        assert_eq!(hevc_au_offsets(&vaapi), vec![0, 35, 42]);
        // x265/nvenc style: an AUD in front of every picture.
        let with_aud = [0,0,0,1,70,1,0x80, 0,0,0,1,64,1,0x42, 0,0,0,1,38,1,0x80,
            0,0,0,1,70,1,0x80, 0,0,0,1,2,1,0x80, 0,0,0,1,70,1,0x80, 0,0,0,1,2,1,0x80];
        assert_eq!(hevc_au_offsets(&with_aud), vec![0, 21, 35]);
        // A picture split over several slices stays one access unit.
        let multi_slice = [0,0,0,1,38,1,0x80, 0,0,0,1,38,1,0x00, 0,0,0,1,2,1,0x80];
        assert_eq!(hevc_au_offsets(&multi_slice), vec![0, 14]);
        // Truncated/invalid NALs must not invent boundaries. An invalid header is skipped, and
        // the first valid NAL after it still opens the first unit.
        assert_eq!(hevc_au_offsets(&[0,0,1,70]), Vec::<usize>::new());
        assert_eq!(hevc_au_offsets(&[0,0,1,128,1,0x80]), Vec::<usize>::new());
        assert_eq!(hevc_au_offsets(&[0,0,1,128,1,0x80, 0,0,1,2,1,0x80]), vec![6]);
        // A suffix SEI (40) after a picture stays in that picture's unit; the next slice opens the next one.
        assert_eq!(hevc_au_offsets(&[0,0,1,0x26,1,0x80, 0,0,1,0x50,1,0x05, 0,0,1,0x02,1,0x80]), vec![0, 12]);
    }
    #[tokio::test]
    async fn hevc_dependency_gate_recovers_only_at_irap_after_overflow() {
        let (tx, mut rx) = mpsc::channel(1);
        let mut gate = DependencyGate::default();
        let frame = |sequence, irap| Arc::new(CompleteFrame { source_generation: 0, source_id: 0, _permit: None, codec: VideoCodec::H265,
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
        // hevc_vaapi with -aud drops every non-IRAP picture, so H.265 must not ask for AUDs.
        assert!(!vaapi.iter().any(|option| option == "-aud"));
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


#[cfg(test)]
mod screen_frame_tests {
    use super::*;
    #[test]
    fn source_restart_rejects_queued_frames_even_with_same_screen_and_codec() {
        let frame=CompleteFrame {source_id:0,source_generation:7,codec:VideoCodec::H264,sequence:1,completed_at:Instant::now(),data:vec![],_permit:None};
        let mut settings=VideoSettings {source_id:0,source_generation:7,..Default::default()};
        assert!(frame.matches(&settings));
        settings.source_generation=9;assert!(!frame.matches(&settings));
        settings.source_generation=7;settings.source_id=1;assert!(!frame.matches(&settings));
        settings.source_id=0;settings.codec=VideoCodec::H265;assert!(!frame.matches(&settings));
    }
}
