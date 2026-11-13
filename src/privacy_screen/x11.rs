//! Experimental X11 screen-off mode through `xrandr` output gamma.
//!
//! Linux has no capture-excluded overlay, so the CRTC gamma ramps are zeroed instead
//! (`--brightness 0`): scanout turns black while the framebuffer the capture reads is unchanged, so
//! the remote picture keeps working. Step numbers and limits in docs/privacy-screen.md.
//! Every output that carries a CRTC is recorded with its previous gamma/brightness in the recovery
//! journal before anything changes, so a crash (or the guard) can put the screen back.
use super::{
    backend::{Backend, Entered},
    record::Journal,
    tool, Mode,
};

pub const NAME: &str = "x11";

#[derive(serde::Serialize, serde::Deserialize)]
struct Data {
    display: String,
    outputs: Vec<Output>,
}

/// One output's restore parameters, as `xrandr --verbose` reported them.
#[derive(serde::Serialize, serde::Deserialize)]
struct Output {
    name: String,
    gamma: String,
    brightness: String,
}

pub struct X11;

pub fn detected() -> bool {
    crate::desktop_backend::detect().ok() == Some(crate::desktop_backend::DesktopBackend::X11)
        // A unit that only inherits DISPLAY is still a Wayland session, where xrandr cannot dim.
        && !crate::desktop_backend::headless()
        && !crate::desktop_backend::wayland_session_running()
        && tool::available("xrandr")
        && tool::available("timeout")
}

fn xrandr(display: &str, args: &[String]) -> Result<String, String> {
    let mut all = vec!["--display".to_owned(), display.to_owned()];
    all.extend_from_slice(args);
    tool::run("xrandr", &all)
}

/// Connected outputs that drive a CRTC: `name connected [primary] WxH+X+Y (...)`. An output that is
/// connected but has no mode reports no geometry and is skipped, as is a disconnected one.
fn connected_output(line: &str) -> Option<String> {
    let mut fields = line.split_whitespace();
    let name = fields.next()?;
    if fields.next()? != "connected" { return None; }
    fields.any(|field| field.contains('x') && field.contains('+')).then(|| name.to_owned())
}

/// `xrandr --verbose`: the unindented header starts each output, the indented `Gamma:` and
/// `Brightness:` lines below it belong to that output.
fn parse_outputs(verbose: &str) -> Vec<Output> {
    let mut outputs: Vec<Output> = Vec::new();
    for line in verbose.lines() {
        if !line.starts_with([' ', '\t']) {
            if let Some(name) = connected_output(line) {
                outputs.push(Output { name, gamma: "1.0:1.0:1.0".into(), brightness: "1.0".into() });
            }
            continue;
        }
        let Some(output) = outputs.last_mut() else { continue };
        let line = line.trim();
        if let Some(value) = line.strip_prefix("Gamma:") { output.gamma = value.trim().to_owned(); }
        if let Some(value) = line.strip_prefix("Brightness:") { output.brightness = value.trim().to_owned(); }
    }
    // A brightness of 0 or an unreadable value is a screen a previous crash left black: restore 1.0
    // instead of copying it forward.
    for output in &mut outputs {
        if !output.brightness.parse::<f64>().is_ok_and(|value| value > 0.0) { output.brightness = "1.0".into(); }
    }
    outputs
}

fn black_args(outputs: &[Output]) -> Vec<String> {
    outputs.iter().flat_map(|o| ["--output".to_owned(), o.name.clone(), "--brightness".to_owned(), "0".to_owned()]).collect()
}

fn restore_args(outputs: &[Output]) -> Vec<String> {
    outputs.iter().flat_map(|o| ["--output".to_owned(), o.name.clone(), "--gamma".to_owned(), o.gamma.clone(),
        "--brightness".to_owned(), o.brightness.clone()]).collect()
}

fn darken(display: &str) -> Result<Data, String> {
    let outputs = parse_outputs(&xrandr(display, &["--verbose".to_owned()])?);
    if outputs.is_empty() { return Err("xrandr reports no output driven by a CRTC".into()); }
    Ok(Data { display: display.to_owned(), outputs })
}

impl Backend for X11 {
    fn name(&self) -> &'static str {
        NAME
    }

    fn unavailable(&self, mode: Mode) -> Option<String> {
        if mode == Mode::Picture {
            return Some("X11 privacy picture mode is not supported".into());
        }
        match std::env::var("DISPLAY") {
            Err(_) => Some("X11 privacy screen needs DISPLAY".into()),
            Ok(display) => xrandr(&display, &["--query".to_owned()])
                .err()
                .map(|e| format!("X11 output control unavailable: {e}")),
        }
    }

    fn enter(&self, mode: Mode, journal: &mut Journal) -> Result<Entered, String> {
        if mode != Mode::ScreenOff {
            return Err("X11 privacy picture mode is not supported".into());
        }
        let data = darken(&std::env::var("DISPLAY").map_err(|_| "X11 privacy screen needs DISPLAY")?)?;
        journal.save(&data)?;
        if let Err(e) = xrandr(&data.display, &black_args(&data.outputs)) {
            let _ = xrandr(&data.display, &restore_args(&data.outputs));
            return Err(e);
        }
        log::info!("Privacy X11: {} output(s) darkened (gamma and brightness 0)", data.outputs.len());
        Ok(Entered::default())
    }

    fn leave(&self, data: &serde_json::Value) -> Result<(), String> {
        let data: Data = serde_json::from_value(data.clone()).map_err(|_| "invalid X11 recovery data")?;
        xrandr(&data.display, &restore_args(&data.outputs))?;
        log::info!("Privacy X11 restored: {} output(s)", data.outputs.len());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{black_args, parse_outputs, restore_args};

    const VERBOSE: &str = "Screen 0: minimum 320 x 200, current 3840 x 1080, maximum 8192 x 8192
eDP-1 connected primary 1920x1080+0+0 (0x45) normal (normal left inverted right x axis y axis) 309mm x 174mm
\tIdentifier: 0x42
\tTimestamp: 12345
\tGamma: 1.0:0.9:1.0
\tBrightness: 0.85
HDMI-1 disconnected (normal left inverted right x axis y axis)
DP-1 connected 1920x1080+1920+0 (0x47) normal (normal left inverted right x axis y axis) 527mm x 296mm
\tGamma: 1.0:1.0:1.0
\tBrightness: 1.0
DP-2 connected (normal left inverted right x axis y axis)
";

    #[test]
    fn keeps_only_connected_outputs_with_a_crtc_and_their_gamma() {
        let outputs = parse_outputs(VERBOSE);
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].name, "eDP-1");
        assert_eq!(outputs[0].gamma, "1.0:0.9:1.0");
        assert_eq!(outputs[0].brightness, "0.85");
        // Missing Gamma/Brightness lines fall back to the identity, not to garbage.
        assert_eq!(outputs[1].name, "DP-1");
        assert_eq!(outputs[1].gamma, "1.0:1.0:1.0");
        assert_eq!(outputs[1].brightness, "1.0");
    }

    #[test]
    fn a_screen_a_crash_left_black_is_restored_to_full_brightness() {
        let outputs = parse_outputs("eDP-1 connected primary 1920x1080+0+0 (0x45) normal\n\tGamma: 1.0:0.0:1.0\n\tBrightness: 0\n");
        assert_eq!(outputs[0].brightness, "1.0");
        assert_eq!(restore_args(&outputs), ["--output", "eDP-1", "--gamma", "1.0:0.0:1.0", "--brightness", "1.0"]);
        assert_eq!(black_args(&outputs), ["--output", "eDP-1", "--brightness", "0"]);
    }
}
