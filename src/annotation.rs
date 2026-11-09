//! Viewer annotation (观看模式「批注」): the viewer draws on the host screen while watching.
//!
//! RpcRequest field 27 `draw_req` (gv_pb.DrawRequest) is answered with RpcResponse field 23
//! `draw_resp` (gv_pb.DrawResponse). Schema read from the official Mac 4.42.0 build
//! (UURemoteService: SwiftProtobuf name maps and Swift field metadata):
//!
//! - DrawRequest  { oneof: 1 stroke_request, 2 clear_request, 3 toggle_request }
//! - DrawResponse { oneof: 1 stroke_response, 2 clear_response, 3 toggle_response }
//! - DrawStrokeRequest { 1 uint32 stroke_id, 2 repeated DrawPoint points, 3 optional int32 screen_id,
//!   4 optional float line_width, 5 optional uint32 color }; DrawPoint { 1 float x, 2 float y }
//! - DrawClearRequest { 1 ClearType clear_type (1 all, 2 by id), 2 uint32 stroke_id, 3 optional int32 screen_id }
//! - DrawToggleRequest { 1 bool enable }
//! - Draw*Response { 1 int32 error_code }, 0 = success
//!
//! The Mac viewer sends normalized points (`annotationNormalizedPoint`) in batches that share a
//! stroke id (`flushAnnotationStrokePoints`), clears before a screen switch and toggles off on exit.
//! Without this handler the request went unanswered and the viewer showed 「批注开启失败」.
//!
//! The strokes are drawn on a click-through X11 overlay (c_engine/annotation_overlay.c), so the
//! person at the host sees them and the capture streams them back to the viewer.

use crate::protocol::{decode_wire, WireField, WireValue};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

pub const RPC_REQUEST: u32 = 27;
pub const RPC_RESPONSE: u64 = 23;

const SLOT_STROKE: u64 = 1;
const SLOT_CLEAR: u64 = 2;
const SLOT_TOGGLE: u64 = 3;

/// error_code: the overlay cannot be shown (no X display, no compositor or no ARGB visual).
const ERR_UNAVAILABLE: i32 = 1;

/// iOS system red, opaque: the viewer sends no color of its own (Mac 4.42.0).
const DEFAULT_COLOR: u32 = 0xFFFF_3B30;

#[cfg(target_os = "linux")]
extern "C" {
    fn uu_annotation_open(display: *const libc::c_char, x: libc::c_int, y: libc::c_int, width: libc::c_int,
        height: libc::c_int, error: *mut libc::c_int) -> *mut libc::c_void;
    fn uu_annotation_size(handle: *mut libc::c_void, width: *mut libc::c_int, height: *mut libc::c_int);
    fn uu_annotation_polyline(handle: *mut libc::c_void, points: *const i16, count: libc::c_int, argb: libc::c_uint,
        line_width: libc::c_int);
    fn uu_annotation_clear(handle: *mut libc::c_void);
    fn uu_annotation_flush(handle: *mut libc::c_void);
    fn uu_annotation_poll(handle: *mut libc::c_void) -> libc::c_int;
    fn uu_annotation_close(handle: *mut libc::c_void);
}

// macOS development builds: no X display, so the overlay never opens.
#[cfg(not(target_os = "linux"))]
unsafe fn uu_annotation_open(_: *const libc::c_char, _: libc::c_int, _: libc::c_int, _: libc::c_int, _: libc::c_int,
    error: *mut libc::c_int) -> *mut libc::c_void { *error = 1; std::ptr::null_mut() }
#[cfg(not(target_os = "linux"))]
unsafe fn uu_annotation_size(_: *mut libc::c_void, _: *mut libc::c_int, _: *mut libc::c_int) {}
#[cfg(not(target_os = "linux"))]
unsafe fn uu_annotation_polyline(_: *mut libc::c_void, _: *const i16, _: libc::c_int, _: libc::c_uint, _: libc::c_int) {}
#[cfg(not(target_os = "linux"))]
unsafe fn uu_annotation_clear(_: *mut libc::c_void) {}
#[cfg(not(target_os = "linux"))]
unsafe fn uu_annotation_flush(_: *mut libc::c_void) {}
#[cfg(not(target_os = "linux"))]
unsafe fn uu_annotation_poll(_: *mut libc::c_void) -> libc::c_int { 0 }
#[cfg(not(target_os = "linux"))]
unsafe fn uu_annotation_close(_: *mut libc::c_void) {}

#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Stroke { id: u32, points: Vec<(f32, f32)>, screen: Option<i32>, width: Option<f32>, color: Option<u32> },
    Clear { all: bool, id: u32 },
    Toggle(bool),
}

fn varint_field(fields: &[WireField], number: u32) -> Option<u64> {
    fields.iter().find_map(|f| match (f.number, &f.value) { (n, WireValue::Varint(v)) if n == number => Some(*v), _ => None })
}

fn float_field(fields: &[WireField], number: u32) -> Option<f32> {
    fields.iter().find_map(|f| match (f.number, &f.value) { (n, WireValue::Fixed32(b)) if n == number => Some(f32::from_le_bytes(*b)), _ => None })
}

fn bytes_field<'a>(fields: &'a [WireField<'a>], number: u32) -> Option<&'a [u8]> {
    fields.iter().find_map(|f| match (f.number, &f.value) { (n, WireValue::Bytes(b)) if n == number => Some(*b), _ => None })
}

/// Decodes a DrawRequest body (the bytes of RpcRequest field 27).
pub fn parse(body: &[u8]) -> Option<Request> {
    let fields = decode_wire(body).ok()?;
    if let Some(stroke) = bytes_field(&fields, 1) {
        let s = decode_wire(stroke).ok()?;
        let mut points = Vec::new();
        for field in &s {
            if let (2, WireValue::Bytes(point)) = (field.number, &field.value) {
                let p = decode_wire(point).ok()?;
                points.push((float_field(&p, 1).unwrap_or(0.0), float_field(&p, 2).unwrap_or(0.0)));
            }
        }
        return Some(Request::Stroke {
            id: varint_field(&s, 1).unwrap_or(0) as u32,
            points,
            screen: varint_field(&s, 3).map(|v| v as i32),
            width: float_field(&s, 4),
            color: varint_field(&s, 5).map(|v| v as u32),
        });
    }
    if let Some(clear) = bytes_field(&fields, 2) {
        let c = decode_wire(clear).ok()?;
        let kind = varint_field(&c, 1).unwrap_or(0);
        return Some(Request::Clear { all: kind != 2, id: varint_field(&c, 2).unwrap_or(0) as u32 });
    }
    if let Some(toggle) = bytes_field(&fields, 3) {
        let t = decode_wire(toggle).ok()?;
        return Some(Request::Toggle(varint_field(&t, 1).unwrap_or(0) != 0));
    }
    None
}

fn varint(mut v: u64, out: &mut Vec<u8>) {
    while v >= 128 { out.push((v as u8 & 127) | 128); v >>= 7; }
    out.push(v as u8);
}

fn bytes(field: u64, v: &[u8], out: &mut Vec<u8>) {
    varint(field << 3 | 2, out);
    varint(v.len() as u64, out);
    out.extend_from_slice(v);
}

/// RpcResponse { 1 response_header (echoed request header), 23 DrawResponse { slot: { 1 error_code } } }.
pub fn encode_response(header: &[u8], slot: u64, error_code: i32) -> Vec<u8> {
    let mut result = Vec::new();
    if error_code != 0 { varint(1 << 3, &mut result); varint(error_code as i64 as u64, &mut result); }
    let mut draw = Vec::new();
    bytes(slot, &result, &mut draw);
    let mut out = Vec::new();
    bytes(1, header, &mut out);
    bytes(RPC_RESPONSE, &draw, &mut out);
    out
}

/// (request header, DrawRequest body) of an RpcRequest 27 draw_req; None for every other RPC.
pub fn draw_request(rpc: &[WireField]) -> Option<(Vec<u8>, Vec<u8>)> {
    let body = bytes_field(rpc, RPC_REQUEST)?;
    Some((bytes_field(rpc, 1).unwrap_or(&[]).to_vec(), body.to_vec()))
}

/// The peer a request came from: the address of that peer's `PeerActions`. Its session tracker
/// holds the Arc until it has sent its last reset, so a newer peer cannot reuse the key meanwhile.
pub type Owner = usize;

pub fn owner<T>(peer: &std::sync::Arc<T>) -> Owner { std::sync::Arc::as_ptr(peer) as *const () as usize }

/// Applies a draw_req from `owner` and builds its RpcResponse. Blocking: a toggle waits for the overlay.
pub fn respond(owner: Owner, header: &[u8], body: &[u8]) -> Vec<u8> {
    let Some(request) = parse(body) else {
        log::warn!("Annotation request not understood: {}", crate::protocol::wire_shape(body, 3));
        return encode_response(header, SLOT_TOGGLE, ERR_UNAVAILABLE);
    };
    let (slot, code) = match request {
        Request::Toggle(enable) => {
            let (reply, result) = mpsc::channel();
            send(Command::Toggle { owner, enable, reply });
            // Opening the overlay is a few X round trips; never stall the channel for long.
            let code = result.recv_timeout(Duration::from_secs(2)).unwrap_or(ERR_UNAVAILABLE);
            log::info!("Annotation toggle: enable={} error_code={}", enable, code);
            (SLOT_TOGGLE, code)
        }
        Request::Clear { all, id } => {
            log::info!("Annotation clear: all={} stroke_present={}", all, id != 0);
            send(Command::Clear { owner, all, id });
            (SLOT_CLEAR, 0)
        }
        stroke @ Request::Stroke { .. } => {
            send(Command::Stroke(owner, stroke));
            (SLOT_STROKE, 0)
        }
    };
    encode_response(header, slot, code)
}

/// `owner`'s desktop session ended: remove its overlay and strokes. A delayed reset from a replaced
/// peer leaves the annotation of the viewer that took over alone.
pub fn reset(owner: Owner) {
    if let Some(worker) = WORKER.get() {
        let _ = worker.lock().unwrap().send(Command::Reset(owner));
    }
}

enum Command {
    Toggle { owner: Owner, enable: bool, reply: mpsc::Sender<i32> },
    Stroke(Owner, Request),
    Clear { owner: Owner, all: bool, id: u32 },
    Reset(Owner),
    /// Tests: (owner, overlay size, stroke count) while an overlay is shown.
    #[cfg(test)]
    Probe(mpsc::Sender<Option<(Owner, i32, i32, usize)>>),
}

static WORKER: OnceLock<Mutex<mpsc::Sender<Command>>> = OnceLock::new();

fn send(command: Command) {
    let worker = WORKER.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new().name("annotation".into()).spawn(move || run(rx)).expect("annotation thread");
        Mutex::new(tx)
    });
    let _ = worker.lock().unwrap().send(command);
}

/// A stroke keeps the viewer's points, so it can be laid out again when the overlay is rebuilt.
struct Stroke { id: u32, normalized: bool, argb: u32, line: Option<f32>, raw: Vec<(f32, f32)>, width: i32, points: Vec<i16> }

impl Stroke {
    fn new(id: u32, raw: Vec<(f32, f32)>, line: Option<f32>, color: Option<u32>) -> Self {
        let normalized = raw.iter().all(|&(x, y)| x.abs() <= 1.5 && y.abs() <= 1.5);
        Self { id, normalized, argb: argb(color), line, raw, width: 0, points: Vec::new() }
    }

    /// Overlay pixels for all points; a single point is a dot (a zero-length round-capped segment).
    fn layout(&mut self, width: i32, height: i32) {
        self.width = line_width(self.line, height);
        self.points = to_pixels(&self.raw, self.normalized, width, height);
        if self.points.len() == 2 { self.points.extend_from_within(0..2); }
    }
}

struct Overlay { handle: *mut libc::c_void, screen: Option<i32>, region: (i32, i32, i32, i32), width: i32, height: i32 }

impl Overlay {
    fn open(screen: Option<i32>) -> Result<Self, i32> {
        let region = region(screen);
        let display = crate::capture::x11_display().and_then(|d| std::ffi::CString::new(d).ok());
        let mut error = 0;
        let handle = unsafe {
            uu_annotation_open(display.as_ref().map_or(std::ptr::null(), |d| d.as_ptr()), region.0, region.1, region.2, region.3, &mut error)
        };
        if handle.is_null() { return Err(error); }
        let (mut width, mut height) = (0, 0);
        unsafe { uu_annotation_size(handle, &mut width, &mut height) };
        log::info!("Annotation overlay shown: {}x{}+{}+{}", width, height, region.0, region.1);
        Ok(Self { handle, screen, region, width, height })
    }

    fn draw(&self, stroke: &Stroke, from: usize) {
        let points = &stroke.points[from.min(stroke.points.len())..];
        if points.len() < 2 { return; }
        unsafe { uu_annotation_polyline(self.handle, points.as_ptr(), (points.len() / 2) as libc::c_int, stroke.argb, stroke.width) };
    }

    fn repaint(&self, strokes: &[Stroke]) {
        unsafe { uu_annotation_clear(self.handle) };
        for stroke in strokes { self.draw(stroke, 0); }
        self.flush();
    }

    fn flush(&self) { unsafe { uu_annotation_flush(self.handle) } }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe { uu_annotation_close(self.handle) };
        log::info!("Annotation overlay removed");
    }
}

/// The capture region a screen id maps to: an X11 multi-screen monitor, else the whole root (0 size).
fn region(screen: Option<i32>) -> (i32, i32, i32, i32) {
    if crate::wayland::x11_multiscreen() {
        let id = screen.filter(|s| *s >= 0).map_or_else(crate::wayland::selected, |s| s as usize);
        if let Some(o) = crate::wayland::source(id) { return (o.x, o.y, o.width as i32, o.height as i32); }
    }
    (0, 0, 0, 0)
}

/// Premultiplied ARGB for the 32-bit visual; a color without alpha is opaque.
pub fn argb(color: Option<u32>) -> u32 {
    let c = color.unwrap_or(DEFAULT_COLOR);
    let c = if c >> 24 == 0 { c | 0xFF00_0000 } else { c };
    let a = c >> 24;
    let mul = |shift: u32| (((c >> shift) & 0xFF) * a / 255) << shift;
    a << 24 | mul(16) | mul(8) | mul(0)
}

/// Line width in overlay pixels: values up to 1 are a fraction of the height, larger ones pixels.
pub fn line_width(width: Option<f32>, height: i32) -> i32 {
    match width {
        Some(w) if w.is_finite() && w > 0.0 => (if w <= 1.0 { w * height as f32 } else { w }).round().clamp(1.0, 64.0) as i32,
        _ => (height as f32 / 360.0).round().max(3.0) as i32,
    }
}

pub fn to_pixels(points: &[(f32, f32)], normalized: bool, width: i32, height: i32) -> Vec<i16> {
    let mut out = Vec::with_capacity(points.len() * 2);
    for &(x, y) in points {
        let (x, y) = if normalized { (x * width as f32, y * height as f32) } else { (x, y) };
        let clamp = |v: f32| if v.is_finite() { v.round().clamp(-16_000.0, 16_000.0) as i16 } else { 0 };
        out.push(clamp(x));
        out.push(clamp(y));
    }
    out
}

/// The worker's state: one overlay at a time, owned by the peer that last toggled it on or drew.
#[derive(Default)]
struct Annotation { overlay: Option<Overlay>, strokes: Vec<Stroke>, owner: Owner, unavailable: bool, logged: usize }

impl Annotation {
    fn close(&mut self) { self.overlay = None; self.strokes.clear(); }

    /// A request from another peer: the newer viewer takes over and the old strokes go.
    fn claim(&mut self, owner: Owner) {
        if self.owner != owner { self.close(); self.owner = owner; self.unavailable = false; }
    }

    fn open(&mut self, screen: Option<i32>) -> bool {
        match Overlay::open(screen) {
            Ok(o) => { self.overlay = Some(o); true }
            Err(error) => { log::warn!("Annotation overlay unavailable: error={}", error); self.unavailable = true; false }
        }
    }

    fn stroke(&mut self, id: u32, points: Vec<(f32, f32)>, screen: Option<i32>, width: Option<f32>, color: Option<u32>) {
        if self.overlay.as_ref().is_some_and(|o| o.region != region(screen)) { self.close(); }
        // A stroke without a toggle (e.g. after a host restart) shows the overlay anyway, but only
        // one attempt until the next toggle instead of one per batch.
        if self.overlay.is_none() && (self.unavailable || !self.open(screen)) { return; }
        if self.logged < 8 {
            // Counts and presence only: the drawing itself is the viewer's content.
            self.logged += 1;
            let normalized = points.iter().all(|&(x, y)| x.abs() <= 1.5 && y.abs() <= 1.5);
            log::info!("Annotation stroke batch: points={} normalized={} screen_set={} width_set={} color_set={}",
                points.len(), normalized, screen.is_some(), width.is_some(), color.is_some());
        }
        let o = self.overlay.as_ref().unwrap();
        match self.strokes.iter_mut().find(|s| s.id == id) {
            Some(stroke) => {
                let from = stroke.points.len().saturating_sub(2);
                stroke.points.extend(to_pixels(&points, stroke.normalized, o.width, o.height));
                stroke.raw.extend(points);
                o.draw(stroke, from);
            }
            None => {
                let mut stroke = Stroke::new(id, points, width, color);
                stroke.layout(o.width, o.height);
                o.draw(&stroke, 0);
                self.strokes.push(stroke);
            }
        }
        o.flush();
    }

    /// The root was resized (RandR; on X11 the viewer's resolution choice does this): rebuild the
    /// overlay at the new geometry and lay the strokes out again.
    fn follow_resize(&mut self) {
        let Some(screen) = self.overlay.take().map(|o| o.screen) else { return };
        if !self.open(screen) { self.strokes.clear(); return; }
        let o = self.overlay.as_ref().unwrap();
        for stroke in &mut self.strokes { stroke.layout(o.width, o.height); }
        o.repaint(&self.strokes);
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Toggle { owner, enable: true, reply } => {
                self.claim(owner);
                self.unavailable = false;
                let code = if self.overlay.is_some() || self.open(None) { 0 } else { ERR_UNAVAILABLE };
                let _ = reply.send(code);
            }
            Command::Toggle { owner, enable: false, reply } => {
                if owner == self.owner { self.close(); }
                let _ = reply.send(0);
            }
            Command::Reset(owner) => {
                if owner != self.owner { return; }
                if self.overlay.is_some() { log::info!("Annotation ended with the viewer session"); }
                self.close();
                self.unavailable = false;
            }
            Command::Clear { owner, all, id } => {
                if owner != self.owner { return; }
                if all { self.strokes.clear(); } else { self.strokes.retain(|s| s.id != id); }
                if let Some(o) = &self.overlay { o.repaint(&self.strokes); }
            }
            Command::Stroke(owner, Request::Stroke { id, points, screen, width, color }) => {
                self.claim(owner);
                self.stroke(id, points, screen, width, color);
            }
            Command::Stroke(..) => {}
            #[cfg(test)]
            Command::Probe(reply) => {
                let _ = reply.send(self.overlay.as_ref().map(|o| (self.owner, o.width, o.height, self.strokes.len())));
            }
        }
    }
}

fn run(rx: mpsc::Receiver<Command>) {
    let mut state = Annotation::default();
    loop {
        let wait = if state.overlay.is_some() { Duration::from_millis(50) } else { Duration::from_secs(3600) };
        match rx.recv_timeout(wait) {
            Ok(command) => state.handle(command),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        match state.overlay.as_ref().map_or(0, |o| unsafe { uu_annotation_poll(o.handle) }) {
            2 => state.follow_resize(),
            1 => if let Some(o) = &state.overlay { o.repaint(&state.strokes); },
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn float(field: u64, v: f32, out: &mut Vec<u8>) { varint(field << 3 | 5, out); out.extend_from_slice(&v.to_le_bytes()); }

    #[test]
    fn parses_toggle_stroke_and_clear() {
        let mut toggle = Vec::new();
        bytes(3, &[8, 1], &mut toggle);
        assert_eq!(parse(&toggle), Some(Request::Toggle(true)));
        let mut off = Vec::new();
        bytes(3, &[], &mut off);
        assert_eq!(parse(&off), Some(Request::Toggle(false)));

        let mut point = Vec::new();
        float(1, 0.25, &mut point);
        float(2, 0.75, &mut point);
        let mut stroke = vec![8, 7];
        bytes(2, &point, &mut stroke);
        bytes(2, &point, &mut stroke);
        stroke.extend_from_slice(&[24, 0]);
        float(4, 4.0, &mut stroke);
        stroke.push(40);
        varint(0xFF00_FF00, &mut stroke);
        let mut request = Vec::new();
        bytes(1, &stroke, &mut request);
        assert_eq!(parse(&request), Some(Request::Stroke {
            id: 7, points: vec![(0.25, 0.75), (0.25, 0.75)], screen: Some(0), width: Some(4.0), color: Some(0xFF00_FF00) }));

        let mut clear = Vec::new();
        bytes(2, &[8, 2, 16, 9], &mut clear);
        assert_eq!(parse(&clear), Some(Request::Clear { all: false, id: 9 }));
        let mut clear_all = Vec::new();
        bytes(2, &[8, 1], &mut clear_all);
        assert_eq!(parse(&clear_all), Some(Request::Clear { all: true, id: 0 }));
    }

    #[test]
    fn response_echoes_header_in_field_23() {
        let ok = encode_response(&[8, 5], SLOT_TOGGLE, 0);
        assert_eq!(ok, vec![10, 2, 8, 5, 186, 1, 2, 26, 0]);
        let failed = encode_response(&[], SLOT_STROKE, 1);
        assert_eq!(failed, vec![10, 0, 186, 1, 4, 10, 2, 8, 1]);
        let fields = decode_wire(&failed).unwrap();
        assert_eq!(fields[1].number, 23);
    }

    #[test]
    fn answers_only_draw_requests() {
        let mut other = Vec::new();
        bytes(1, &[8, 1], &mut other);
        bytes(28, &[], &mut other);
        assert!(draw_request(&decode_wire(&other).unwrap()).is_none());
        let mut draw = Vec::new();
        bytes(1, &[8, 5], &mut draw);
        bytes(27, &[26, 2, 8, 1], &mut draw);
        assert_eq!(draw_request(&decode_wire(&draw).unwrap()), Some((vec![8, 5], vec![26, 2, 8, 1])));
    }

    /// Needs a composited X display: DISPLAY=:0 cargo test -- --ignored annotation_overlay_smoke
    /// On a disposable display, UU_ANNOTATION_SMOKE_RESIZE=1024x768 also resizes the root (xrandr --fb).
    #[test]
    #[ignore]
    fn annotation_overlay_smoke() {
        let wait = Duration::from_secs(5);
        let toggle = |owner, enable| { let (reply, r) = mpsc::channel(); send(Command::Toggle { owner, enable, reply }); r.recv_timeout(wait).unwrap() };
        let probe = || { let (reply, r) = mpsc::channel(); send(Command::Probe(reply)); r.recv_timeout(wait).unwrap() };
        let stroke = |owner, id, points: &[(f32, f32)], width, color| send(Command::Stroke(owner, Request::Stroke {
            id, points: points.to_vec(), screen: None, width, color }));
        assert_eq!(toggle(1, true), 0);
        let diagonal: Vec<(f32, f32)> = (0..=20).map(|i| (0.3 + i as f32 * 0.02, 0.3 + i as f32 * 0.01)).collect();
        stroke(1, 1, &diagonal[..10], None, None);
        stroke(1, 1, &diagonal[10..], None, None);
        stroke(1, 2, &[(0.5, 0.6)], Some(30.0), Some(0x0000FF));
        std::thread::sleep(Duration::from_secs(
            std::env::var("UU_ANNOTATION_SMOKE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(3)));
        assert_eq!(probe().map(|p| (p.0, p.3)), Some((1, 2)));
        if let Ok(size) = std::env::var("UU_ANNOTATION_SMOKE_RESIZE") {
            // xrandr may report an error for an output larger than the new size yet still resize.
            let _ = std::process::Command::new("xrandr").args(["--fb", &size]).status();
            std::thread::sleep(Duration::from_millis(500));
            let (w, h) = size.split_once('x').map(|(w, h)| (w.parse().unwrap(), h.parse().unwrap())).unwrap();
            assert_eq!(probe().map(|p| (p.1, p.2, p.3)), Some((w, h, 2)), "overlay follows the {size} root");
        }
        // A newer viewer takes over; the replaced peer's delayed reset and toggle-off leave it alone.
        assert_eq!(toggle(2, true), 0);
        reset(1);
        assert_eq!(toggle(1, false), 0);
        assert_eq!(probe().map(|p| (p.0, p.3)), Some((2, 0)));
        stroke(2, 1, &diagonal, None, None);
        assert_eq!(probe().map(|p| (p.0, p.3)), Some((2, 1)));
        reset(2);
        assert_eq!(probe(), None);
    }

    #[test]
    fn strokes_are_laid_out_again_after_a_resize() {
        let mut dot = Stroke::new(1, vec![(0.5, 0.5)], None, None);
        dot.layout(100, 200);
        assert_eq!((dot.points.clone(), dot.width), (vec![50, 100, 50, 100], 3));
        dot.layout(3840, 2160);
        assert_eq!((dot.points.clone(), dot.width), (vec![1920, 1080, 1920, 1080], 6));
        let mut pixels = Stroke::new(2, vec![(10.0, 20.0), (30.0, 40.0)], Some(8.0), None);
        pixels.layout(100, 200);
        assert_eq!((pixels.normalized, pixels.points.clone(), pixels.width), (false, vec![10, 20, 30, 40], 8));
    }

    #[test]
    fn colors_widths_and_points() {
        assert_eq!(argb(None), 0xFFFF_3B30);
        assert_eq!(argb(Some(0x00FF00)), 0xFF00_FF00);
        assert_eq!(argb(Some(0x80FF_0000)), 0x8080_0000);
        assert_eq!(line_width(None, 2160), 6);
        assert_eq!(line_width(None, 720), 3);
        assert_eq!(line_width(Some(8.0), 2160), 8);
        assert_eq!(line_width(Some(0.01), 2000), 20);
        assert_eq!(to_pixels(&[(0.5, 1.0)], true, 3840, 2160), vec![1920, 2160]);
        assert_eq!(to_pixels(&[(12.4, f32::NAN)], false, 3840, 2160), vec![12, 0]);
    }
}
