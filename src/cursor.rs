//! Pointer shape mirroring. The video no longer contains the pointer (x11grab
//! -draw_mouse 0), so the viewer draws its local pointer; this tells it which shape.
//! Wire layout recovered from the official Mac host sendCursorShape (UURemoteServer
//! arm64 0x1000065ac..0x100006d3c): Message.which=15 SystemStateChange, field 2
//! CursorShape { 1 pos_x, 2 pos_y = hotspot, 3 width, 4 height, 5 byte_value = PNG
//! (representationUsingType 4), 6 cursor_type = 0 with image, 7/8 coordinate x/y
//! scale = video pixels per screen point (double), 9 screen_id }.

use std::sync::{Arc, OnceLock};
use tokio::sync::watch;

#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
extern "C" {
    fn x11_cursor_open(display_name: *const libc::c_char) -> libc::c_int;
    fn x11_cursor_wait(timeout_ms: libc::c_int) -> libc::c_int;
    fn x11_cursor_poll(pixels: *mut u32, max_pixels: libc::c_int, width: *mut libc::c_int, height: *mut libc::c_int,
        hot_x: *mut libc::c_int, hot_y: *mut libc::c_int, force: libc::c_int) -> libc::c_int;
}

#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
const MAX_EDGE: usize = 256;
/// Upper bound between checks; cursor-change events wake the monitor immediately.
#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
const WAIT_MS: libc::c_int = 250;

#[derive(Debug, PartialEq)]
pub struct CursorImage { pub width: u32, pub height: u32, pub hot_x: i32, pub hot_y: i32, pub png: Vec<u8> }

/// XFixes delivers premultiplied ARGB32; PNG needs straight-alpha RGBA.
#[cfg_attr(not(all(target_os = "linux", not(target_env = "ohos"))), allow(dead_code))]
fn argb_premultiplied_to_rgba(pixels: &[u32]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(pixels.len() * 4);
    for &pixel in pixels {
        let a = pixel >> 24;
        let straight = |shift: u32| -> u8 {
            let c = (pixel >> shift) & 0xff;
            if a == 0 { 0 } else { ((c * 255 + a / 2) / a).min(255) as u8 }
        };
        rgba.extend_from_slice(&[straight(16), straight(8), straight(0), a as u8]);
    }
    rgba
}

/// Minimal 8-bit RGBA PNG (filter 0) using the existing flate2 dependency.
#[cfg_attr(not(all(target_os = "linux", not(target_env = "ohos"))), allow(dead_code))]
pub(crate) fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    use std::io::Write;
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut crc = flate2::Crc::new();
        crc.update(kind);
        crc.update(data);
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&crc.sum().to_be_bytes());
    }
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit, RGBA, deflate, filter 0, no interlace
    let mut scanlines = Vec::with_capacity(rgba.len() + height as usize);
    for row in rgba.chunks(width as usize * 4) {
        scanlines.push(0);
        scanlines.extend_from_slice(row);
    }
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&scanlines).expect("in-memory zlib");
    let compressed = encoder.finish().expect("in-memory zlib");
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut png, b"IHDR", &header);
    chunk(&mut png, b"IDAT", &compressed);
    chunk(&mut png, b"IEND", &[]);
    png
}

fn varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 128 { out.push((value as u8 & 127) | 128); value >>= 7; }
    out.push(value as u8);
}

/// Message.which=15 SystemStateChange { 2: CursorShape } for an unscaled source screen 0.
pub fn encode_cursor_shape(seq: u64, timestamp: u64, cursor: &CursorImage) -> Vec<u8> {
    let mut shape = Vec::new();
    for (field, value) in [(1, cursor.hot_x), (2, cursor.hot_y), (3, cursor.width as i32), (4, cursor.height as i32)] {
        if value != 0 { varint(field << 3, &mut shape); varint(value as i64 as u64, &mut shape); }
    }
    varint(5 << 3 | 2, &mut shape);
    varint(cursor.png.len() as u64, &mut shape);
    shape.extend_from_slice(&cursor.png);
    // cursor_type 0 (image present) and screen_id 0 are protobuf defaults and omitted.
    for field in [7u64, 8] {
        varint(field << 3 | 1, &mut shape);
        shape.extend_from_slice(&1.0f64.to_le_bytes());
    }
    let mut change = Vec::new();
    varint(2 << 3 | 2, &mut change);
    varint(shape.len() as u64, &mut change);
    change.extend_from_slice(&shape);
    crate::initialization::envelope(seq, timestamp, 15, &change)
}

/// One process-wide monitor thread; every session subscribes to the latest shape.
pub fn monitor(display: &str) -> watch::Receiver<Option<Arc<CursorImage>>> {
    if display.starts_with("wayland:") {
        // XFixes cannot observe native Wayland cursors; no false XWayland monitor.
        let (_,receiver)=watch::channel(None); return receiver;
    }
    static LATEST: OnceLock<watch::Sender<Option<Arc<CursorImage>>>> = OnceLock::new();
    LATEST.get_or_init(|| {
        let (tx, _) = watch::channel(None);
        start_thread(display.to_owned(), tx.clone());
        tx
    }).subscribe()
}

#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
fn start_thread(display: String, tx: watch::Sender<Option<Arc<CursorImage>>>) {
    let spawned = std::thread::Builder::new().name("cursor-shape".into()).spawn(move || {
        let Ok(name) = std::ffi::CString::new(display) else { return };
        let opened = unsafe { x11_cursor_open(name.as_ptr()) };
        if opened != 0 { log::warn!("Cursor shape monitor unavailable: {}", opened); return; }
        let mut pixels = vec![0u32; MAX_EDGE * MAX_EDGE];
        let mut force = 1;
        loop {
            let (mut w, mut h, mut x, mut y) = (0, 0, 0, 0);
            let result = unsafe { x11_cursor_poll(pixels.as_mut_ptr(), pixels.len() as i32, &mut w, &mut h, &mut x, &mut y, force) };
            if result == 1 {
                let (width, height) = (w as u32, h as u32);
                let rgba = argb_premultiplied_to_rgba(&pixels[..(width * height) as usize]);
                tx.send_replace(Some(Arc::new(CursorImage { width, height, hot_x: x, hot_y: y, png: encode_png(width, height, &rgba) })));
                force = 0;
            } else if result < 0 {
                log::warn!("Cursor shape poll failed: {}", result);
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            unsafe { x11_cursor_wait(WAIT_MS) };
        }
    });
    if let Err(e) = spawned { log::warn!("Cursor shape thread failed: {}", e); }
}

#[cfg(not(all(target_os = "linux", not(target_env = "ohos"))))]
fn start_thread(_: String, _: watch::Sender<Option<Arc<CursorImage>>>) {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unpremultiplies_alpha_for_png() {
        // Opaque white, half-transparent premultiplied red (0x80,0x80,0,0), fully transparent.
        assert_eq!(argb_premultiplied_to_rgba(&[0xffffffff, 0x80800000, 0x00000000]),
            vec![255,255,255,255, 255,0,0,128, 0,0,0,0]);
    }
    #[test]
    fn png_has_valid_structure_and_decodes_back() {
        let rgba: Vec<u8> = (0..2*3*4).map(|i| i as u8 * 10).collect();
        let png = encode_png(2, 3, &rgba);
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 2);
        assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 3);
        assert!(png.ends_with(&[0,0,0,0, b'I',b'E',b'N',b'D', 0xae,0x42,0x60,0x82]));
        // IDAT inflates to filtered scanlines: each row = filter byte 0 + 8 pixels bytes.
        let idat_len = u32::from_be_bytes(png[33..37].try_into().unwrap()) as usize;
        assert_eq!(&png[37..41], b"IDAT");
        let mut raw = Vec::new();
        std::io::Read::read_to_end(&mut flate2::read::ZlibDecoder::new(&png[41..41+idat_len]), &mut raw).unwrap();
        assert_eq!(raw.len(), 3 * (1 + 8));
        assert_eq!(&raw[1..9], &rgba[0..8]);
        assert_eq!(raw[9], 0);
    }
    #[test]
    fn cursor_shape_matches_official_field_layout() {
        let cursor = CursorImage { width: 24, height: 24, hot_x: 4, hot_y: 2, png: vec![1, 2, 3] };
        let message = encode_cursor_shape(7, 9, &cursor);
        // Message: seq 7, timestamp 9, field 15 (tag 0x7a) SystemStateChange.
        assert_eq!(&message[..4], &[8, 7, 16, 9]);
        assert_eq!(message[4], 0x7a);
        let change = &message[6..];
        assert_eq!(change[0], 0x12); // SystemStateChange.field2 cursor_shape
        let shape = &change[2..];
        assert_eq!(&shape[..8], &[8, 4, 16, 2, 24, 24, 32, 24]); // hotspot, width, height
        assert_eq!(&shape[8..13], &[0x2a, 3, 1, 2, 3]); // byte_value PNG
        assert_eq!(shape[13], 0x39); // field 7 double
        assert_eq!(&shape[14..22], &1.0f64.to_le_bytes());
        assert_eq!(shape[22], 0x41); // field 8 double
        assert_eq!(shape.len(), 31);
    }
}
