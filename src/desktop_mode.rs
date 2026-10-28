//! X11 desktop resolution follows the viewer's 分辨率 choice, like the official Windows host:
//! the display mode itself changes (xrandr), not just the encoded size. The mode in use before
//! the first change is restored for 原始/follow-remote and when the last desktop stream ends.
//! Disable with UU_DESKTOP_RESIZE=0 (the viewer choice then scales the encoded stream instead).

use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request { Native, Size(u32, u32) }

struct Original { display: String, output: String, mode: String }

static ORIGINAL: Mutex<Option<Original>> = Mutex::new(None);
static STREAMS: AtomicUsize = AtomicUsize::new(0);

/// Upper bound per side (H.264 NVENC/VAAPI limit) and lower bound for a usable desktop.
const MAX_SIDE: u32 = 4096;
const MIN_SIZE: (u32, u32) = (640, 480);

pub fn enabled() -> bool { std::env::var("UU_DESKTOP_RESIZE").map_or(true, |v| v != "0") }

/// Keeps the changed mode while any desktop stream runs; the last one restores it.
pub struct StreamGuard;
impl StreamGuard {
    pub fn new() -> Self { STREAMS.fetch_add(1, Ordering::SeqCst); Self }
}
impl Drop for StreamGuard {
    fn drop(&mut self) {
        if STREAMS.fetch_sub(1, Ordering::SeqCst) == 1 && ORIGINAL.lock().unwrap().is_some() {
            // xrandr blocks; never run it on the async runtime thread dropping the stream.
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_secs(2));
                if STREAMS.load(Ordering::SeqCst) == 0 { restore(); }
            });
        }
    }
}

/// Applies the viewer's resolution request to the X11 display; returns the desktop size now in
/// use, or None when nothing changed hands (the caller keeps encoder scaling as the fallback).
pub fn apply(display: &str, request: Request) -> Option<(u32, u32)> {
    match request {
        Request::Native => { restore(); crate::capture::x11_size() },
        Request::Size(w, h) => {
            let target = fit(w, h)?;
            match switch(display, target) {
                Ok(()) => crate::capture::x11_size(),
                Err(error) => { log::warn!("Desktop resolution change to {}x{} failed: {}", target.0, target.1, error); None },
            }
        },
    }
}

/// Even sides within MIN_SIZE..=MAX_SIDE, keeping the aspect ratio when scaled down.
fn fit(w: u32, h: u32) -> Option<(u32, u32)> {
    if w == 0 || h == 0 { return None; }
    let scale = (f64::from(MAX_SIDE) / f64::from(w.max(h))).min(1.0);
    let (w, h) = ((f64::from(w) * scale).round() as u32, (f64::from(h) * scale).round() as u32);
    Some((w.max(MIN_SIZE.0) & !1, h.max(MIN_SIZE.1) & !1))
}

fn switch(display: &str, target: (u32, u32)) -> Result<(), String> {
    let state = query(display)?;
    if state.current == Some(target) { return Ok(()); }
    {
        let mut original = ORIGINAL.lock().unwrap();
        if original.is_none() {
            let mode = state.current_mode.clone().ok_or("current mode unknown")?;
            log::info!("Desktop resolution: saving original mode {} on {}", mode, state.output);
            *original = Some(Original { display: display.into(), output: state.output.clone(), mode });
        }
    }
    let mode = match state.modes.iter().find(|(_, size)| *size == target) {
        Some((name, _)) => name.clone(),
        None => {
            let (name, timings) = cvt_reduced(target.0, target.1);
            // An existing mode of this name (earlier session) makes --newmode fail harmlessly.
            let mut args = vec!["--newmode", name.as_str()];
            args.extend(timings.iter().map(String::as_str));
            let _ = xrandr(display, &args);
            xrandr(display, &["--addmode", &state.output, &name])?;
            name
        },
    };
    set_mode(display, &state.output, &mode, target)?;
    log::info!("Desktop resolution changed to {}x{} ({})", target.0, target.1, mode);
    Ok(())
}

/// Restores the mode saved before the first change, if any.
pub fn restore() {
    let Some(original) = ORIGINAL.lock().unwrap().take() else { return; };
    let size = query(&original.display).ok().and_then(|state| state.modes.into_iter().find(|(name, _)| *name == original.mode).map(|(_, size)| size));
    match size.ok_or_else(|| "original mode no longer listed".to_owned())
        .and_then(|size| set_mode(&original.display, &original.output, &original.mode, size)) {
        Ok(()) => log::info!("Desktop resolution restored to {}", original.mode),
        Err(error) => log::warn!("Desktop resolution restore failed: {}", error),
    }
    let _ = crate::capture::x11_size();
}

/// GNOME/mutter may answer a mode list change by re-applying its preferred mode; set it again.
fn set_mode(display: &str, output: &str, mode: &str, size: (u32, u32)) -> Result<(), String> {
    for _ in 0..3 {
        xrandr(display, &["--output", output, "--mode", mode])?;
        std::thread::sleep(std::time::Duration::from_millis(1500));
        if query(display)?.current == Some(size) { return Ok(()); }
    }
    Err(format!("{output} did not keep {mode}"))
}

fn xrandr(display: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new("xrandr").arg("--display").arg(display).args(args).output()
        .map_err(|e| format!("xrandr unavailable: {}", e.kind()))?;
    if !output.status.success() { return Err(format!("xrandr {} exited with {}", args.first().unwrap_or(&""), output.status)); }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

struct State { output: String, current: Option<(u32, u32)>, current_mode: Option<String>, modes: Vec<(String, (u32, u32))> }

fn query(display: &str) -> Result<State, String> { parse(&xrandr(display, &["--query"])?).ok_or_else(|| "no connected output".into()) }

/// The primary connected output (else the first connected one with a mode) and its mode list.
fn parse(text: &str) -> Option<State> {
    let mut outputs: Vec<(bool, State)> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("   ") {
            let Some((_, state)) = outputs.last_mut() else { continue; };
            let mut parts = rest.split_whitespace();
            let Some(name) = parts.next() else { continue; };
            let Some(size) = mode_size(name) else { continue; };
            if parts.any(|rate| rate.contains('*')) { state.current = Some(size); state.current_mode = Some(name.into()); }
            state.modes.push((name.into(), size));
        } else if !line.starts_with(' ') {
            let mut parts = line.split_whitespace();
            let (Some(name), Some("connected")) = (parts.next(), parts.next()) else { outputs.push((false, State { output: String::new(), current: None, current_mode: None, modes: Vec::new() })); continue; };
            let primary = line.contains(" connected primary ");
            outputs.push((primary, State { output: name.into(), current: None, current_mode: None, modes: Vec::new() }));
        }
    }
    let mut connected: Vec<(bool, State)> = outputs.into_iter().filter(|(_, s)| !s.output.is_empty() && s.current.is_some()).collect();
    let index = connected.iter().position(|(primary, _)| *primary).unwrap_or(0);
    (index < connected.len()).then(|| connected.swap_remove(index).1)
}

/// "1920x1080", "1920x1080R", "1920x1080_60.00", "1920x1080i" → (1920, 1080).
fn mode_size(name: &str) -> Option<(u32, u32)> {
    let (w, rest) = name.split_once('x')?;
    let h: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some((w.parse().ok()?, h.parse().ok()?))
}

/// VESA CVT reduced-blanking (v1) timings: 60 Hz, or 30 Hz when 60 Hz exceeds an HDMI 2.0
/// pixel clock (virtual/forced connectors on integrated GPUs reject faster modes).
fn cvt_reduced(w: u32, h: u32) -> (String, Vec<String>) {
    const H_BLANK: u32 = 160; const H_SYNC: u32 = 32; const H_FRONT: u32 = 48;
    const V_FRONT: u32 = 3; const MIN_V_BACK: u32 = 6; const MIN_VBLANK_US: f64 = 460.0;
    let v_sync = match (w * 3 == h * 4, w * 9 == h * 16, w * 10 == h * 16, w * 4 == h * 5, w * 9 == h * 15) {
        (true, ..) => 4, (_, true, ..) => 5, (_, _, true, ..) => 6, (_, _, _, true, _) | (.., true) => 7, _ => 10,
    };
    let timing = |refresh: f64| {
        let h_period = (1_000_000.0 / refresh - MIN_VBLANK_US) / f64::from(h);
        let vbi = ((MIN_VBLANK_US / h_period) as u32 + 1).max(V_FRONT + v_sync + MIN_V_BACK);
        let (total_h, total_v) = (w + H_BLANK, h + vbi);
        let clock = (refresh * f64::from(total_h) * f64::from(total_v) / 1_000_000.0 / 0.25).floor() * 0.25;
        (clock, total_h, total_v)
    };
    let (mut refresh, mut t) = (60, timing(60.0));
    if t.0 > 590.0 { refresh = 30; t = timing(30.0); }
    let (clock, total_h, total_v) = t;
    let name = format!("{w}x{h}_{refresh}R");
    let timings = [format!("{clock:.2}"), w.to_string(), (w + H_FRONT).to_string(), (w + H_FRONT + H_SYNC).to_string(), total_h.to_string(),
        h.to_string(), (h + V_FRONT).to_string(), (h + V_FRONT + v_sync).to_string(), total_v.to_string(), "+hsync".into(), "-vsync".into()];
    (name, timings.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduced_blanking_matches_the_cvt_tool() {
        // `cvt -r 1920 1080`: 138.50 1920 1968 2000 2080 1080 1083 1088 1111 +hsync -vsync
        let (name, t) = cvt_reduced(1920, 1080);
        assert_eq!(name, "1920x1080_60R");
        assert_eq!(t.join(" "), "138.50 1920 1968 2000 2080 1080 1083 1088 1111 +hsync -vsync");
        // `cvt -r 2560 1600`: 268.50 2560 2608 2640 2720 1600 1603 1609 1646
        assert_eq!(cvt_reduced(2560, 1600).1.join(" "), "268.50 2560 2608 2640 2720 1600 1603 1609 1646 +hsync -vsync");
        // 4K at 60 Hz needs 533 MHz (under the cap); a larger desktop drops to 30 Hz.
        assert_eq!(cvt_reduced(3840, 2160).0, "3840x2160_60R");
        assert_eq!(cvt_reduced(4096, 2560).0, "4096x2560_30R");
    }

    #[test]
    fn requested_sizes_are_even_and_bounded() {
        assert_eq!(fit(1280, 720), Some((1280, 720)));
        assert_eq!(fit(4112, 2580), Some((4096, 2570)));
        assert_eq!(fit(301, 201), Some((640, 480)));
        assert_eq!(fit(0, 1080), None);
    }

    #[test]
    fn parses_primary_output_modes_and_current_mode() {
        let text = r"Screen 0: minimum 320 x 200, current 1920 x 1080, maximum 16384 x 16384
DP-1 disconnected (normal left inverted right x axis y axis)
HDMI-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 0mm x 0mm
   1024x768      60.00
   3840x2160R    29.98
   1920x1080R    59.93*+
HDMI-2 disconnected (normal left inverted right x axis y axis)
";
        let state = parse(text).unwrap();
        assert_eq!(state.output, "HDMI-1");
        assert_eq!(state.current, Some((1920, 1080)));
        assert_eq!(state.current_mode.as_deref(), Some("1920x1080R"));
        assert_eq!(state.modes.len(), 3);
        assert_eq!(state.modes[1], ("3840x2160R".to_owned(), (3840, 2160)));
        assert!(parse("Screen 0: minimum 320 x 200\nDP-1 disconnected (normal)\n").is_none());
    }
}
