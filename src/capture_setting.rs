//! Viewer quality/frame-rate settings (画质 / 帧率) for the desktop stream.
//!
//! The viewer only sends settings to a host whose echo FeatureFlag declares ff_capture_setting
//! (field 1); our echo declares FF_CAPTURE_SETTING_CUSTOM_BITRATE = 4. Requests arrive as
//! Message 21 RpcRequest { 1 RequestHeader, 2 CaptureSettingRequest | 3 FpsSettingRequest |
//! 4 VideoQualitySettingReqeust } and are answered with Message 22 RpcResponse { 1 ResponseHeader
//! (same request_id), 2 CaptureSettingResponse { 1 errors (none = applied) } }. The applied values
//! are then reported with Message 25 CaptureSettingSync. Field maps: server-field-maps.json.

use crate::protocol::{decode_wire, WireField, WireValue};
use tokio::sync::watch;

/// Official GvPb_VideoQuality values.
pub const QUALITY_FAST: u64 = 1;
pub const QUALITY_GENERAL: u64 = 2;
pub const QUALITY_HD: u64 = 3;
pub const QUALITY_BLURAY: u64 = 4;
pub const QUALITY_AUTO: u64 = 5;
pub const QUALITY_CUSTOM: u64 = 6;

/// FeatureFlag.ff_capture_setting we declare: fps, preset quality and custom bitrate.
/// Versions are cumulative (official FF_CaptureSetting enum: 3 CHROMA_SAMPLING, 4 CUSTOM_BITRATE,
/// 5 DPI_SCALE, 6 HDR), so 4 also tells the viewer we accept a chroma format (真彩).
pub const FEATURE_VERSION: u64 = 4;

/// Official GvPb_ChromaFormat values (CaptureSettingRequest field 7, CaptureSettingSync field 3).
pub const CHROMA_420: u64 = 1;
pub const CHROMA_444: u64 = 3;

const MIN_FPS: u32 = 5;
const MAX_FPS: u32 = 144;

/// Official GvPb_CodecType, CaptureSettingRequest.codec_type (field 13).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec { #[default] H264, H265 }
impl VideoCodec {
    pub fn wire(self) -> u64 { match self { Self::H264 => 1, Self::H265 => 2 } }
    pub fn mime(self) -> &'static str { match self { Self::H264 => "video/H264", Self::H265 => "video/H265" } }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoSettings {
    pub source_id: usize,
    pub source_generation: u64,
    pub fps: u32,
    pub codec: VideoCodec,
    pub quality: u64,
    /// Viewer max_custom_bitrate in bits/s (only used for QUALITY_CUSTOM).
    pub custom_bps: u32,
    /// Scaled stream size (其他分辨率); None streams the native desktop size.
    pub output: Option<(u32, u32)>,
    /// CHROMA_420 or CHROMA_444 (真彩); 4:4:4 only when the encoder pipeline supports it.
    pub chroma: u64,
}

impl Default for VideoSettings {
    fn default() -> Self { let (source_id,source_generation)=crate::wayland::selection(); Self { source_id,source_generation, codec: VideoCodec::H264, fps: 60, quality: QUALITY_AUTO, custom_bps: 0, output: None, chroma: CHROMA_420 } }
}

/// Selectable smaller stream sizes with the desktop's aspect ratio (even dimensions).
/// The host display itself is never reconfigured; the encoder scales.
pub fn scaled_sizes(native: (u32, u32)) -> Vec<(u32, u32)> {
    [5u32, 4, 3, 2].iter().map(|sixths| (native.0 * sixths / 6 & !1, native.1 * sixths / 6 & !1))
        .filter(|(w, h)| *w >= 640 && *h >= 360).collect()
}

/// Requested size clamped to the desktop: None (native) when at least as large.
fn clamp_output(native: (u32, u32), width: u64, height: u64) -> Option<(u32, u32)> {
    if width == 0 || height == 0 { return None; }
    if width >= u64::from(native.0) || height >= u64::from(native.1) { return None; }
    Some(((width as u32).max(320) & !1, (height as u32).max(180) & !1))
}

impl VideoSettings {
    /// Constant-quality level: presets must look different even on a mostly static desktop,
    /// where a bitrate cap alone never binds (measured ~2.7 Mbit/s at CQ18).
    pub fn cq(&self) -> u32 {
        match self.quality { QUALITY_BLURAY => 17, QUALITY_HD => 21, QUALITY_GENERAL => 26, QUALITY_FAST => 30, _ => 18 }
    }

    /// Encoder bitrate cap in kbit/s; None keeps the constant-quality default (自动).
    /// Preset caps follow the viewer menu labels: 高清 2M, 超清 8M, 原画 20M.
    pub fn max_kbps(&self) -> Option<u32> {
        match self.quality {
            QUALITY_FAST => Some(1_000),
            QUALITY_GENERAL => Some(2_000),
            QUALITY_HD => Some(8_000),
            QUALITY_BLURAY => Some(20_000),
            QUALITY_CUSTOM if self.custom_bps > 0 =>
                Some((self.custom_bps / 1_000).clamp(1, 100_000)),
            _ => None,
        }
    }
}

fn varint(mut v: u64, out: &mut Vec<u8>) { while v >= 128 { out.push((v as u8 & 127) | 128); v >>= 7; } out.push(v as u8); }
fn put_varint(field: u64, v: u64, out: &mut Vec<u8>) { varint(field << 3, out); varint(v, out); }
fn put_bytes(field: u64, v: &[u8], out: &mut Vec<u8>) { varint(field << 3 | 2, out); varint(v.len() as u64, out); out.extend_from_slice(v); }

fn number(fields: &[WireField], n: u32) -> Option<u64> {
    fields.iter().find_map(|f| match (f.number, &f.value) { (x, WireValue::Varint(v)) if x == n => Some(*v), _ => None })
}
fn message<'a>(fields: &[WireField<'a>], n: u32) -> Option<&'a [u8]> {
    fields.iter().find_map(|f| match (f.number, &f.value) { (x, WireValue::Bytes(b)) if x == n => Some(*b), _ => None })
}

/// Frame rate from a value that is either an fps option index (observed 1 = 30, 2 = 60;
/// 3/4 assumed 90/144 from the menu order) or already frames per second.
fn fps_from_option(value: u64) -> Option<u64> {
    match value { 0 => None, 1 => Some(30), 2 => Some(60), 3 => Some(90), 4 => Some(144), v => Some(v) }
}

/// Applies a capture-setting RPC to `settings` and returns the RpcResponse payload (Message 22),
/// or None when the RPC is not a capture/fps/quality setting.
/// max_custom_bitrate (field 8) in bit/s: the mobile viewer sends 1_000_000 for 1 Mbps (#67). Values
/// that can only be a different unit stay usable instead of collapsing to a 1 kbit/s cap: below 1000
/// they are Mbit/s, below 200_000 kbit/s (no viewer offers custom rates under 200 kbit/s).
fn custom_bitrate_bps(value: u64) -> u32 {
    let bps = if value < 1_000 { value * 1_000_000 } else if value < 200_000 { value * 1_000 } else { value };
    bps.min(u64::from(u32::MAX)) as u32
}

#[cfg(test)]
pub fn handle_rpc(rpc: &[WireField], settings: &watch::Sender<VideoSettings>, native: (u32, u32)) -> Option<Vec<u8>> {
    handle_rpc_with_codecs(rpc, settings, native, true, false)
}

/// Initial codec once the m-line is negotiated: H.265 when both sides support it (the Mac viewer
/// lists H265 first in its offer) and a hardware encoder is selected. Software libx265 is about ten
/// times slower than libx264 (measured 15 vs 155 fps at 1080p on a 4-core ARM host, #59), so CPU
/// hosts default to H.264. UU_VIDEO_CODEC=h264 / h265 overrides.
#[cfg(test)]
fn test_selection() -> crate::video_select::Selection {
    crate::video_select::resolve_pair(Some("auto"), None, Some("auto"), None, false, None)
}

#[cfg(test)]
pub fn preferred_codec(h264: bool, h265: bool) -> VideoCodec {
    preferred_codec_for(h264, h265, test_selection())
}

pub fn preferred_codec_for(h264: bool, h265: bool, selection: crate::video_select::Selection) -> VideoCodec {
    pick_codec(h264, h265, crate::video_select::codec_force(selection.codec_pref), crate::video_select::hardware(selection.encoder))
}

pub(crate) fn pick_codec(h264: bool, h265: bool, forced: Option<&str>, hardware: bool) -> VideoCodec {
    let wants_h265 = match forced { Some("h264") => false, Some("h265" | "hevc") => true, _ => hardware };
    if h265 && (wants_h265 || !h264) { VideoCodec::H265 } else { VideoCodec::H264 }
}

#[cfg(test)]
pub fn handle_rpc_with_codecs(rpc: &[WireField], settings: &watch::Sender<VideoSettings>, native: (u32, u32), h264: bool, h265: bool) -> Option<Vec<u8>> {
    handle_rpc_for(rpc, settings, native, h264, h265, test_selection())
}

pub fn handle_rpc_for(rpc: &[WireField], settings: &watch::Sender<VideoSettings>, native: (u32, u32), h264: bool, h265: bool, selection: crate::video_select::Selection) -> Option<Vec<u8>> {
    apply_rpc(rpc, settings, native, h264, h265, crate::video::full_chroma_for(selection.encoder), crate::video_select::codec_force(selection.codec_pref))
}

/// The resolution a capture/resolution RPC asks for, parsed like apply_rpc: X11 hosts apply it
/// to the display mode itself (desktop_mode), as the official Windows host changes its resolution.
pub fn requested_resolution(rpc: &[WireField]) -> Option<crate::desktop_mode::Request> {
    use crate::desktop_mode::Request;
    if let Some(request) = message(rpc, 2) {
        let fields = decode_wire(request).ok()?;
        if number(&fields, 10) == Some(3) { return Some(Request::Native); }
        let (w, h) = number(&fields, 16).zip(number(&fields, 17)).or_else(|| number(&fields, 5).zip(number(&fields, 6)))?;
        Some(Request::Size(u32::try_from(w).ok()?, u32::try_from(h).ok()?))
    } else if let Some(request) = message(rpc, 6) {
        let fields = decode_wire(request).ok()?;
        Some(Request::Size(u32::try_from(number(&fields, 1)?).ok()?, u32::try_from(number(&fields, 2)?).ok()?))
    } else { None }
}

fn apply_rpc(rpc: &[WireField], settings: &watch::Sender<VideoSettings>, native: (u32, u32), _h264: bool, h265: bool, full_chroma: bool, forced: Option<&str>) -> Option<Vec<u8>> {
    let header = message(rpc, 1).unwrap_or(&[]);
    let mut next = *settings.borrow();
    if let Some(request) = message(rpc, 2) {
        let fields = decode_wire(request).ok()?;
        // Numbers only: fps, quality, bitrate, resolution and flags carry no user content.
        log::info!("Capture setting request: {:?}", fields.iter().filter_map(|f| match f.value {
            WireValue::Varint(v) => Some((f.number, v)), _ => None }).collect::<Vec<_>>());
        // codec_type 13 (1 H264, 2 H265): like the official host (negotiateCodec), the host picks the
        // codec from both capability lists; the Mac viewer sends its current codec (1 while receiving
        // H.264), so only an explicit H.265 request switches, never one absent from the m-line.
        // A forced codec (host_settings video_codec or UU_VIDEO_CODEC) wins over that switch, same
        // rule as pick_codec at stream start: h264 stays H.264; auto/h265 still follow the request.
        if number(&fields, 13) == Some(2) && h265 && forced != Some("h264") { next.codec = VideoCodec::H265; }
        // Field 1 is the viewer's fps option index (1 = 30, 2 = 60, ...); 18 fpsCount is the frame rate.
        if let Some(fps) = number(&fields, 18).filter(|v| *v > 0).or_else(|| number(&fields, 1).and_then(fps_from_option)) { next.fps = fps as u32; }
        if let Some(quality) = number(&fields, 2).filter(|v| *v > 0) { next.quality = quality; }
        if let Some(bitrate) = number(&fields, 8).filter(|v| *v > 0) { next.custom_bps = custom_bitrate_bps(bitrate); }
        // Field 7 chorma_format (sic): 3 = 4:4:4 when 真彩 is on, 1 = 4:2:0. The Sync reports what is encoded.
        match number(&fields, 7) {
            Some(CHROMA_444) if full_chroma => next.chroma = CHROMA_444,
            Some(CHROMA_444) => { log::info!("Chroma 4:4:4 requested; this encoder pipeline only supports 4:2:0"); next.chroma = CHROMA_420; }
            Some(CHROMA_420) => next.chroma = CHROMA_420,
            _ => {}
        }
        // ChooseResolutionType 3 = follow remote (native); v4 uses pixels in both size pairs.
        if number(&fields, 10) == Some(3) { next.output = None; }
        else if let Some((w, h)) = number(&fields, 16).zip(number(&fields, 17)).or_else(|| number(&fields, 5).zip(number(&fields, 6))) {
            next.output = clamp_output(native, w, h);
        }
    } else if let Some(request) = message(rpc, 6) {
        let fields = decode_wire(request).ok()?;
        let (w, h) = (number(&fields, 1).unwrap_or(0), number(&fields, 2).unwrap_or(0));
        log::info!("Resolution setting request: {}x{}", w, h);
        next.output = clamp_output(native, w, h);
    } else if let Some(request) = message(rpc, 3) {
        let fields = decode_wire(request).ok()?;
        log::info!("Fps setting request: fps={:?}", number(&fields, 1));
        if let Some(fps) = number(&fields, 1).and_then(fps_from_option) { next.fps = fps as u32; }
    } else if let Some(request) = message(rpc, 4) {
        let fields = decode_wire(request).ok()?;
        log::info!("Video quality setting request: quality={:?}", number(&fields, 1));
        if let Some(quality) = number(&fields, 1).filter(|v| *v > 0) { next.quality = quality; }
    } else {
        return None;
    }
    next.fps = next.fps.clamp(MIN_FPS, MAX_FPS);
    settings.send_if_modified(|current| if *current != next {
        log::info!("Desktop video settings: fps={} quality={} max_kbps={:?} output={:?} chroma={}", next.fps, next.quality, next.max_kbps(), next.output, next.chroma);
        *current = next; true
    } else { false });
    let mut out = Vec::new();
    put_bytes(1, header, &mut out);
    put_bytes(2, &[], &mut out); // CaptureSettingResponse without errors: applied
    Some(out)
}

/// CaptureSettingResponse.errors (Error fields 1 code / 2 message, rpc_main.proto).
pub fn rejected_request(data:&[u8],reason:&str)->Option<Vec<u8>> {
    let outer=decode_wire(data).ok()?;
    let rpc=decode_wire(message(&outer,21)?).ok()?;
    message(&rpc,2)?;
    let mut error=Vec::new();
    put_varint(1,(-1i64) as u64,&mut error);
    put_bytes(2,reason.as_bytes(),&mut error);
    let mut errors=Vec::new();put_bytes(1,&error,&mut errors);
    let mut response=Vec::new();
    put_bytes(1,message(&rpc,1).unwrap_or(&[]),&mut response);
    put_bytes(2,&errors,&mut response);
    Some(response)
}

/// Message 7 ScreenSources in pixels for capture-setting v4: native, choices, current size.
pub fn screen_sources(seq: u64, timestamp: u64, native: (u32, u32), output: Option<(u32, u32)>, name: &str) -> Result<Vec<u8>, String> {
    use crate::initialization::{Screen, WinRect};
    let outputs=crate::wayland::outputs();
    if !outputs.is_empty() {
        let current=crate::wayland::selected();
        let choices:Vec<Vec<(i32,i32)>>=outputs.iter().map(|m|scaled_sizes((m.width,m.height)).into_iter().map(|(w,h)|(w as i32,h as i32)).collect()).collect();
        let screens:Vec<Screen<'_>>=outputs.iter().enumerate().map(|(id,m)| Screen {
            id:id as i32, fps:m.fps,
            resolution:WinRect {left:0,top:0,width:m.width as i32,height:m.height as i32,pixel_width:m.width as i32,pixel_height:m.height as i32},
            primary:id==0,dpr:1.0,display_name:&m.name,video_track_index:if id==current {0} else {-1},
            screen_type:0,builtin_screen_type:if m.name.starts_with("eDP") {1} else {2},scaled:&choices[id],
            current:if id==current {output.map(|(w,h)|(w as i32,h as i32))} else {None},
        }).collect();
        return crate::initialization::encode_screen_sources(seq,timestamp,&screens,current as i32);
    }
    let (w, h) = (native.0 as i32, native.1 as i32);
    let scaled: Vec<(i32, i32)> = scaled_sizes(native).into_iter().map(|(w, h)| (w as i32, h as i32)).collect();
    // DPI_SCALE starts at version 5; the observed v4 request uses fields 5/6 as pixels.
    // Keep every advertised rectangle in that space; Portal/EIS maps input independently.
    let resolution = WinRect { left: 0, top: 0, width: w, height: h, pixel_width: w, pixel_height: h };
    let screens = [Screen { id: 0, fps: crate::video::display_fps() as i32, resolution, primary: true, dpr: 1.0,
        display_name: name, video_track_index: 0, screen_type: 0, builtin_screen_type: 0,
        scaled: &scaled, current: output.map(|(w, h)| (w as i32, h as i32)) }];
    crate::initialization::encode_screen_sources(seq, timestamp, &screens, 0)
}

/// Message 25 CaptureSettingSync payload reporting what the host now encodes.
pub fn sync_payload(settings: &VideoSettings, native: (u32, u32)) -> Vec<u8> {
    let size = settings.output.unwrap_or(native);
    let mut out = Vec::new();
    put_varint(1, u64::from(settings.fps), &mut out);
    put_varint(2, settings.quality, &mut out);
    put_varint(3, settings.chroma, &mut out);
    if settings.custom_bps > 0 { put_varint(4, u64::from(settings.custom_bps), &mut out); }
    put_varint(5, u64::from(size.0), &mut out);
    put_varint(6, u64::from(size.1), &mut out);
    put_varint(9, u64::from(size.0), &mut out);
    put_varint(10, u64::from(size.1), &mut out);
    out
}

/// Message 18 CodecNegotiation {1 encoder_cap_list: EncoderCap {1 capture_mode (1 DESKTOP), 2 fps,
/// 3 codec_type, 4 width, 5 height}} (official Windows host descriptor). The viewer's own list is
/// ConnectOptions field 4 decoder_cap_list; it only selects a codec the host announces here.
pub fn codec_negotiation(size: (u32, u32), fps: u32, hevc: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for codec in [Some(VideoCodec::H264), hevc.then_some(VideoCodec::H265)].into_iter().flatten() {
        let mut cap = Vec::new();
        put_varint(1, 1, &mut cap);
        put_varint(2, u64::from(fps), &mut cap);
        put_varint(3, codec.wire(), &mut cap);
        put_varint(4, u64::from(size.0), &mut cap);
        put_varint(5, u64::from(size.1), &mut cap);
        put_bytes(1, &cap, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rpc(request_id: u64, field: u64, body: &[u8]) -> Vec<u8> {
        let mut header = Vec::new();
        put_varint(1, request_id, &mut header);
        let mut out = Vec::new();
        put_bytes(1, &header, &mut out);
        put_bytes(field, body, &mut out);
        out
    }

    #[test]
    fn capture_setting_request_updates_fps_and_quality_and_echoes_request_id() {
        let (tx, rx) = watch::channel(VideoSettings::default());
        let mut body = Vec::new();
        put_varint(1, 1, &mut body); // option index, not fps
        put_varint(2, QUALITY_GENERAL, &mut body);
        put_varint(18, 30, &mut body);
        let request = rpc(77, 2, &body);
        let response = handle_rpc(&decode_wire(&request).unwrap(), &tx, (1920, 1080)).unwrap();
        assert_eq!(*rx.borrow(), VideoSettings { source_id:0,source_generation:0, codec: VideoCodec::H264, fps: 30, quality: QUALITY_GENERAL, custom_bps: 0, output: None, chroma: CHROMA_420 });
        assert_eq!(rx.borrow().max_kbps(), Some(2_000));
        let fields = decode_wire(&response).unwrap();
        assert_eq!(message(&fields, 1), Some(&[8u8, 77][..]));
        assert_eq!(message(&fields, 2), Some(&[][..]));
    }

    #[test]
    fn separate_fps_and_quality_requests_and_limits() {
        let (tx, rx) = watch::channel(VideoSettings::default());
        handle_rpc(&decode_wire(&rpc(1, 3, &[8, 200, 1])).unwrap(), &tx, (1920, 1080)).unwrap();
        assert_eq!(rx.borrow().fps, MAX_FPS);
        // Observed viewer request: option 2 with fpsCount 60 must stream 60 fps, not 2.
        let observed = [8, 2, 16, 4, 40, 128, 15, 48, 184, 8, 144, 1, 60];
        handle_rpc(&decode_wire(&rpc(4, 2, &observed)).unwrap(), &tx, (1920, 1080)).unwrap();
        assert_eq!(rx.borrow().fps, 60);
        handle_rpc(&decode_wire(&rpc(2, 4, &[8, QUALITY_BLURAY as u8])).unwrap(), &tx, (1920, 1080)).unwrap();
        assert_eq!(rx.borrow().max_kbps(), Some(20_000));
        assert!(handle_rpc(&decode_wire(&rpc(3, 9, &[])).unwrap(), &tx, (1920, 1080)).is_none());
    }

    #[test]
    fn true_color_request_selects_444_only_when_the_encoder_supports_it() {
        let (tx, rx) = watch::channel(VideoSettings::default());
        // Observed Mac viewer request with 真彩 on: field 7 = 3.
        let request = rpc(5, 2, &[8, 2, 16, 4, 56, 3, 144, 1, 60]);
        apply_rpc(&decode_wire(&request).unwrap(), &tx, (1920, 1080), true, false, false, None).unwrap();
        assert_eq!(rx.borrow().chroma, CHROMA_420);
        apply_rpc(&decode_wire(&request).unwrap(), &tx, (1920, 1080), true, false, true, None).unwrap();
        assert_eq!(rx.borrow().chroma, CHROMA_444);
        assert_eq!(number(&decode_wire(&sync_payload(&rx.borrow(), (1920, 1080))).unwrap(), 3), Some(CHROMA_444));
        apply_rpc(&decode_wire(&rpc(6, 2, &[56, 1])).unwrap(), &tx, (1920, 1080), true, false, true, None).unwrap();
        assert_eq!(rx.borrow().chroma, CHROMA_420);
    }

    #[test]
    fn codec_negotiation_lists_h264_then_optional_h265() {
        assert_eq!(codec_negotiation((1920, 1080), 60, false), [10, 12, 8, 1, 16, 60, 24, 1, 32, 128, 15, 40, 184, 8]);
        let both = codec_negotiation((1920, 1080), 60, true);
        let caps: Vec<_> = decode_wire(&both).unwrap().iter().filter_map(|f| match f.value { WireValue::Bytes(b) => Some(b), _ => None })
            .map(|b| number(&decode_wire(b).unwrap(), 3)).collect();
        assert_eq!(caps, [Some(1), Some(2)]);
    }

    #[test]
    fn resolution_requests_scale_below_native_only() {
        let (tx, rx) = watch::channel(VideoSettings::default());
        handle_rpc(&decode_wire(&rpc(1, 6, &[8, 128, 10, 16, 208, 5])).unwrap(), &tx, (1920, 1080)).unwrap();
        assert_eq!(rx.borrow().output, Some((1280, 720)));
        handle_rpc(&decode_wire(&rpc(2, 6, &[8, 160, 32, 16, 160, 20])).unwrap(), &tx, (1920, 1080)).unwrap();
        assert_eq!(rx.borrow().output, None);
        assert_eq!(scaled_sizes((1920, 1080)), vec![(1600, 900), (1280, 720), (960, 540), (640, 360)]);
    }

    #[test]
    fn version_four_native_inventory_roundtrips_without_downscaling() {
        assert_eq!(FEATURE_VERSION, 4);
        let native = (3840, 2160);
        let inventory = screen_sources(1, 2, native, None, "test").unwrap();
        let envelope = decode_wire(&inventory).unwrap();
        let sources = decode_wire(message(&envelope, 7).unwrap()).unwrap();
        let display = decode_wire(message(&sources, 1).unwrap()).unwrap();
        let rect = decode_wire(message(&display, 4).unwrap()).unwrap();
        let mut body = Vec::new();
        put_varint(5, number(&rect, 3).unwrap(), &mut body);
        put_varint(6, number(&rect, 4).unwrap(), &mut body);
        put_varint(2, QUALITY_BLURAY, &mut body);
        let (tx, rx) = watch::channel(VideoSettings::default());
        handle_rpc(&decode_wire(&rpc(77, 2, &body)).unwrap(), &tx, native).unwrap();
        assert_eq!(rx.borrow().output, None, "native selection must preserve the full frame");
        let sync = sync_payload(&rx.borrow(), native);
        let fields = decode_wire(&sync).unwrap();
        assert_eq!(number(&fields, 5), Some(3840));
        assert_eq!(number(&fields, 6), Some(2160));
    }

    fn advertised_display(native: (u32, u32), output: Option<(u32, u32)>) -> Vec<u8> {
        let inventory = screen_sources(1, 2, native, output, "test").unwrap();
        let envelope = decode_wire(&inventory).unwrap();
        let sources = decode_wire(message(&envelope, 7).unwrap()).unwrap();
        message(&sources, 1).unwrap().to_vec()
    }

    #[test]
    fn version_four_resolution_choices_and_settings_roundtrip_in_pixels() {
        assert_eq!(FEATURE_VERSION, 4);
        for native in [(1920, 1080), (2560, 1440), (3840, 2160), (5120, 2880),
            (7680, 4320), (3440, 1440), (2160, 3840), (2257, 1505)] {
            let display = advertised_display(native, None);
            let fields = decode_wire(&display).unwrap();
            assert!(fields.iter().any(|f| f.number == 8 && f.value == WireValue::Fixed64(1.0f64.to_le_bytes())));
            assert_eq!(message(&fields, 6), message(&fields, 4));
            let choices: Vec<_> = fields.iter().filter_map(|f| match f.value {
                WireValue::Bytes(rect) if f.number == 3 => Some(rect), _ => None,
            }).collect();
            assert_eq!(choices.len(), 1 + scaled_sizes(native).len());
            assert_eq!(choices[0], message(&fields, 6).unwrap());
            for rect in choices {
                let rect = decode_wire(rect).unwrap();
                let (w, h) = (number(&rect, 3).unwrap(), number(&rect, 4).unwrap());
                assert_eq!(number(&rect, 5), Some(w));
                assert_eq!(number(&rect, 6), Some(h));
                let selected = (w as u32, h as u32);
                let expected = (selected != native).then_some(selected);
                // Exercise both the observed legacy capture request and the resolution-only RPC.
                for case in [2, 6] {
                    let (tx, rx) = watch::channel(VideoSettings::default());
                    let mut body = Vec::new();
                    let (wf, hf) = if case == 2 { (5, 6) } else { (1, 2) };
                    put_varint(wf, w, &mut body); put_varint(hf, h, &mut body);
                    if case == 2 {
                        put_varint(2, QUALITY_GENERAL, &mut body);
                        put_varint(18, 30, &mut body);
                    }
                    let response = handle_rpc(&decode_wire(&rpc(77, case, &body)).unwrap(), &tx, native).unwrap();
                    let response = decode_wire(&response).unwrap();
                    assert_eq!(message(&response, 1), Some(&[8u8, 77][..]));
                    assert_eq!(message(&response, 2), Some(&[][..]));
                    assert_eq!(rx.borrow().output, expected);
                    if case == 2 {
                        assert_eq!(rx.borrow().quality, QUALITY_GENERAL);
                        assert_eq!(rx.borrow().fps, 30);
                    }
                    let sync = sync_payload(&rx.borrow(), native);
                    let sync = decode_wire(&sync).unwrap();
                    for (wf, hf) in [(5, 6), (9, 10)] {
                        assert_eq!(number(&sync, wf), Some(w));
                        assert_eq!(number(&sync, hf), Some(h));
                    }
                    let updated = advertised_display(native, rx.borrow().output);
                    let updated = decode_wire(&updated).unwrap();
                    assert_eq!(message(&updated, 6), message(&fields, 6));
                    assert_eq!(message(&updated, 3), message(&fields, 3));
                    let current = decode_wire(message(&updated, 4).unwrap()).unwrap();
                    for (wf, hf) in [(3, 4), (5, 6)] {
                        assert_eq!(number(&current, wf), Some(w));
                        assert_eq!(number(&current, hf), Some(h));
                    }
                    // Echo synchronized dimensions with a quality/fps change, then restore native.
                    let mut body = Vec::new();
                    put_varint(5, number(&sync, 5).unwrap(), &mut body);
                    put_varint(6, number(&sync, 6).unwrap(), &mut body);
                    put_varint(2, QUALITY_BLURAY, &mut body);
                    put_varint(18, 60, &mut body);
                    handle_rpc(&decode_wire(&rpc(78, 2, &body)).unwrap(), &tx, native).unwrap();
                    assert_eq!(rx.borrow().output, expected);
                    assert_eq!(rx.borrow().quality, QUALITY_BLURAY);
                    assert_eq!(rx.borrow().fps, 60);
                    let mut native_request = Vec::new();
                    put_varint(5, u64::from(native.0), &mut native_request);
                    put_varint(6, u64::from(native.1), &mut native_request);
                    handle_rpc(&decode_wire(&rpc(79, 2, &native_request)).unwrap(), &tx, native).unwrap();
                    assert_eq!(rx.borrow().output, None);
                    handle_rpc(&decode_wire(&rpc(80, 2, &body)).unwrap(), &tx, native).unwrap();
                    assert_eq!(rx.borrow().output, expected);
                    handle_rpc(&decode_wire(&rpc(81, 2, &[80, 3])).unwrap(), &tx, native).unwrap();
                    assert_eq!(rx.borrow().output, None);
                }
            }
        }
    }

    #[test]
    fn explicit_pixel_size_takes_precedence_and_follow_remote_restores_native() {
        let native = (3840, 2160);
        let (tx, rx) = watch::channel(VideoSettings { output: Some((1280, 720)), ..Default::default() });
        let mut body = Vec::new();
        for (field, value) in [(5, 1920), (6, 1080), (16, 2560), (17, 1440)] {
            put_varint(field, value, &mut body);
        }
        handle_rpc(&decode_wire(&rpc(1, 2, &body)).unwrap(), &tx, native).unwrap();
        assert_eq!(rx.borrow().output, Some((2560, 1440)));
        put_varint(10, 3, &mut body);
        handle_rpc(&decode_wire(&rpc(2, 2, &body)).unwrap(), &tx, native).unwrap();
        assert_eq!(rx.borrow().output, None);
        let sync = sync_payload(&rx.borrow(), native);
        let fields = decode_wire(&sync).unwrap();
        assert_eq!(number(&fields, 5), Some(3840));
        assert_eq!(number(&fields, 6), Some(2160));
    }

    #[test]
    fn auto_quality_keeps_constant_quality_encoding() {
        assert_eq!(VideoSettings::default().max_kbps(), None);
        assert_eq!(VideoSettings { quality: QUALITY_CUSTOM, custom_bps: 0, ..Default::default() }.max_kbps(), None);
        assert_eq!(VideoSettings { quality: QUALITY_CUSTOM, custom_bps: 12_000_000, ..Default::default() }.max_kbps(), Some(12_000));
    }

    #[test]
    fn mobile_custom_bitrate_is_bits_per_second_in_request_and_sync() {
        // Mobile viewer sends field 8 = 1_000_000 for 1 Mbps (observed live, 2026-10-20).
        for (bps, kbps) in [(1_000_000, 1_000), (30_000_000, 30_000), (500_000, 500), (100_000_000, 100_000), (200_000_000, 100_000)] {
            let (tx, rx) = watch::channel(VideoSettings::default());
            let mut body = Vec::new();
            put_varint(2, QUALITY_CUSTOM, &mut body);
            put_varint(8, bps, &mut body);
            handle_rpc(&decode_wire(&rpc(7, 2, &body)).unwrap(), &tx, (1920, 1080)).unwrap();
            assert_eq!(rx.borrow().max_kbps(), Some(kbps));
            let sync = sync_payload(&rx.borrow(), (1920, 1080));
            assert_eq!(number(&decode_wire(&sync).unwrap(), 4), Some(bps));
        }
    }

    #[test]
    fn small_custom_bitrates_are_read_as_larger_units() {
        assert_eq!(custom_bitrate_bps(20), 20_000_000);
        assert_eq!(custom_bitrate_bps(8_000), 8_000_000);
        assert_eq!(custom_bitrate_bps(1_000_000), 1_000_000);
    }

    #[test]
    fn oversized_custom_bitrate_does_not_wrap_before_encoder_limit() {
        let (tx, rx) = watch::channel(VideoSettings::default());
        let mut body = Vec::new();
        put_varint(2, QUALITY_CUSTOM, &mut body);
        put_varint(8, u64::from(u32::MAX) + 1, &mut body);
        handle_rpc(&decode_wire(&rpc(8, 2, &body)).unwrap(), &tx, (1920, 1080)).unwrap();
        assert_eq!(rx.borrow().max_kbps(), Some(100_000));
    }

    #[test]
    fn sync_reports_fps_quality_and_resolution() {
        let payload = sync_payload(&VideoSettings::default(), (1920, 1080));
        let fields = decode_wire(&payload).unwrap();
        assert_eq!(number(&fields, 1), Some(60));
        assert_eq!(number(&fields, 2), Some(QUALITY_AUTO));
        assert_eq!(number(&fields, 5), Some(1920));
        assert_eq!(number(&fields, 10), Some(1080));
    }
}

#[cfg(test)]
mod codec_tests {
    use super::*;
    #[test]
    fn codec_is_opt_in_and_requires_negotiated_support() {
        let (tx, rx) = watch::channel(VideoSettings::default());
        let rpc = |value| decode_wire(if value == 2 { &[18, 2, 104, 2] } else { &[18, 2, 104, 1] }).unwrap();
        assert_eq!(rx.borrow().codec, VideoCodec::H264);
        handle_rpc_with_codecs(&rpc(2), &tx, (1920,1080), true, false).unwrap();
        assert_eq!(rx.borrow().codec, VideoCodec::H264);
        handle_rpc_with_codecs(&rpc(2), &tx, (1920,1080), true, true).unwrap();
        assert_eq!(rx.borrow().codec, VideoCodec::H265);
        handle_rpc_with_codecs(&rpc(1), &tx, (1920,1080), false, true).unwrap();
        assert_eq!(rx.borrow().codec, VideoCodec::H265); // unnegotiated H264 is never sent
        // An unrelated FPS RPC must preserve the explicit codec selection.
        handle_rpc_with_codecs(&decode_wire(&[26,2,8,1]).unwrap(), &tx, (1920,1080), true, true).unwrap();
        assert_eq!(rx.borrow().codec, VideoCodec::H265);
        // The viewer reports its current codec (1) in every request; that must not undo H.265.
        handle_rpc_with_codecs(&rpc(1), &tx, (1920,1080), true, true).unwrap();
        assert_eq!(rx.borrow().codec, VideoCodec::H265);
        assert_eq!(preferred_codec(true, false), VideoCodec::H264);
    }
    #[test]
    fn software_hosts_default_to_h264_unless_forced() {
        assert_eq!(pick_codec(true, true, None, true), VideoCodec::H265);
        assert_eq!(pick_codec(true, true, None, false), VideoCodec::H264);
        assert_eq!(pick_codec(true, true, Some("h265"), false), VideoCodec::H265);
        assert_eq!(pick_codec(true, true, Some("h264"), true), VideoCodec::H264);
        // Only one codec bound in the SDP: use it whatever the preference.
        assert_eq!(pick_codec(false, true, None, false), VideoCodec::H265);
        assert_eq!(pick_codec(true, false, Some("h265"), true), VideoCodec::H264);
    }
    #[test]
    fn forced_h264_ignores_later_h265_capture_request() {
        let (tx, rx) = watch::channel(VideoSettings::default());
        // Field 13 = 2 on a peer that negotiated both codecs.
        let request = decode_wire(&[18, 2, 104, 2]).unwrap();
        apply_rpc(&request, &tx, (1920, 1080), true, true, false, Some("h264")).unwrap();
        assert_eq!(rx.borrow().codec, VideoCodec::H264);
        apply_rpc(&request, &tx, (1920, 1080), true, true, false, None).unwrap();
        assert_eq!(rx.borrow().codec, VideoCodec::H265);
        let (tx, rx) = watch::channel(VideoSettings::default());
        apply_rpc(&request, &tx, (1920, 1080), true, true, false, Some("h265")).unwrap();
        assert_eq!(rx.borrow().codec, VideoCodec::H265);
    }
    #[test]
    fn true_color_keeps_codec_and_vaapi_reports_420() {
        // Field 7 = 3 (真彩). full_chroma false is the VAAPI 4:2:0 drop; true encodes 4:4:4.
        let request = decode_wire(&[18, 2, 56, 3]).unwrap();
        let (tx, rx) = watch::channel(VideoSettings { codec: VideoCodec::H265, ..Default::default() });
        apply_rpc(&request, &tx, (1920, 1080), true, true, true, None).unwrap();
        assert_eq!((rx.borrow().chroma, rx.borrow().codec), (CHROMA_444, VideoCodec::H265));
        // NVENC session (full chroma) that already chose H.265 stays H.265 when 真彩 is requested.
        let nvenc = crate::video_select::resolve_pair(None, None, None, None, true, None);
        assert_eq!(nvenc.encoder, crate::video::Encoder::Nvenc);
        let (tx, rx) = watch::channel(VideoSettings { codec: preferred_codec_for(true, true, nvenc), ..VideoSettings::default() });
        assert_eq!(rx.borrow().codec, VideoCodec::H265);
        handle_rpc_for(&request, &tx, (1920, 1080), true, true, nvenc).unwrap();
        assert_eq!((rx.borrow().chroma, rx.borrow().codec), (CHROMA_444, VideoCodec::H265));
        let (tx, rx) = watch::channel(VideoSettings { codec: VideoCodec::H265, ..Default::default() });
        apply_rpc(&request, &tx, (1920, 1080), true, true, false, None).unwrap();
        assert_eq!((rx.borrow().chroma, rx.borrow().codec), (CHROMA_420, VideoCodec::H265));
        assert_eq!(number(&decode_wire(&sync_payload(&rx.borrow(), (1920, 1080))).unwrap(), 3), Some(CHROMA_420));
        assert!(decode_wire(&sync_payload(&rx.borrow(), (1920, 1080))).unwrap().iter().all(|f| f.number != 13));
    }
}

#[cfg(test)]
mod screen_error_tests {
    use super::*;
    #[test]
    fn failed_switch_returns_correlated_error_not_success() {
        let header=[10,4,b't',b'e',b's',b't'];
        let mut rpc=Vec::new();put_bytes(1,&header,&mut rpc);put_bytes(2,&[32,99],&mut rpc);
        let request=crate::initialization::envelope(1,2,21,&rpc);
        let response=rejected_request(&request,"Unknown screen").unwrap();
        let fields=decode_wire(&response).unwrap();
        assert_eq!(message(&fields,1),Some(header.as_slice()));
        let result=decode_wire(message(&fields,2).unwrap()).unwrap();
        let error=decode_wire(message(&result,1).unwrap()).unwrap();
        assert_eq!(number(&error,1),Some(u64::MAX));
        assert_eq!(message(&error,2),Some(b"Unknown screen".as_slice()));
        assert!(rejected_request(b"invalid","failure").is_none());
    }
}
