//! 防窥模式 (privacy screen) on X11: the physical monitors go black while the stream keeps
//! showing the desktop. Official protocol (Mac UURemoteServer / Windows GameViewerServer maps):
//! Message 21 RpcRequest field 7 PrivateScreenSettingRequest { 1 enable, 2 privacy_type, 3 setting_source }
//! -> Message 22 RpcResponse field 3 PrivateScreenSettingResponse { 1 error_code, 2 req_privacy_type,
//! 3 cur_privacy_type }; PrivateScreenType 1 NOT_SET, 2 NORMAL_PRIVACY, 3 ENHANCED_PRIVACY (wallpaper).
//! Declared through FeatureFlag 4 ff_private_screen = INIT_VERION (1, normal privacy only) and
//! ff_system_metrics = SETTING_PRIVATE_SCREEN (3); SettingMetric 4 private_screen, 7 private_screen_type.
//! The official host answers cur_privacy_type from its saved type (NORMAL/ENHANCED, else NOT_SET).
//!
//! Linux has no capture-excluded overlay window, so the CRTC gamma ramps are zeroed instead
//! (`xrandr --brightness 0`): scanout turns black, the framebuffer the capture reads is unchanged.
//! The previous gamma/brightness is saved to a file first, so a restart restores a screen a crash left black.

use std::process::Command;
use std::sync::Mutex;

pub const FF_PRIVATE_SCREEN: u8 = 1;
pub const PS_NOT_SET: u64 = 1;
pub const PS_NORMAL: u64 = 2;
const PS_ENHANCED: u64 = 3;
const ERROR_FAILED: u64 = 1;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct Output { name: String, gamma: String, brightness: String }

#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Saved { display: String, outputs: Vec<Output> }

static ACTIVE: Mutex<Option<Saved>> = Mutex::new(None);

fn saved_path() -> std::path::PathBuf { crate::config::path().with_file_name("privacy_screen.json") }

/// Only the X11 engine can black out scanout without blacking out the capture.
pub fn supported() -> bool {
    !cfg!(test) && !crate::desktop_backend::headless()
        && matches!(crate::desktop_backend::detect(), Ok(crate::desktop_backend::DesktopBackend::X11))
        && crate::capture::x11_display().is_some()
}

pub fn active() -> bool { ACTIVE.lock().map(|a| a.is_some()).unwrap_or(false) }

/// The saved type the viewer shows: NORMAL once privacy was used or chosen, else NOT_SET.
pub fn current_type() -> u64 {
    if crate::system_actions::load_settings().privacy_screen_type == PS_NORMAL { PS_NORMAL } else { PS_NOT_SET }
}

fn xrandr(display: &str, args: &[String]) -> Result<String, String> {
    let output = Command::new("xrandr").arg("--display").arg(display).args(args).output()
        .map_err(|e| format!("xrandr unavailable: {}", e.kind()))?;
    if !output.status.success() { return Err(format!("xrandr exited with {}", output.status)); }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Connected outputs that drive a CRTC (geometry WxH+X+Y on the header line), with their gamma.
fn parse_outputs(verbose: &str) -> Vec<Output> {
    let mut outputs: Vec<Output> = Vec::new();
    let mut current: Option<usize> = None;
    for line in verbose.lines() {
        if !line.starts_with([' ', '\t']) {
            current = None;
            let mut words = line.split_whitespace();
            let (Some(name), Some("connected")) = (words.next(), words.next()) else { continue };
            if words.take(2).any(|w| w.contains('x') && w.contains('+')) {
                outputs.push(Output { name: name.into(), gamma: "1.0:1.0:1.0".into(), brightness: "1.0".into() });
                current = Some(outputs.len() - 1);
            }
        } else if let Some(i) = current {
            let line = line.trim();
            if let Some(v) = line.strip_prefix("Gamma:") { outputs[i].gamma = v.trim().into(); }
            if let Some(v) = line.strip_prefix("Brightness:") { outputs[i].brightness = v.trim().into(); }
        }
    }
    // A black screen saved by a crashed run must not become the value restored later.
    for output in &mut outputs {
        if output.brightness.parse::<f64>().map_or(true, |b| !(b > 0.0)) { output.brightness = "1.0".into(); }
    }
    outputs
}

fn black_args(outputs: &[Output]) -> Vec<String> {
    outputs.iter().flat_map(|o| ["--output".into(), o.name.clone(), "--brightness".into(), "0".into()]).collect()
}

fn restore_args(outputs: &[Output]) -> Vec<String> {
    outputs.iter().flat_map(|o| ["--output".into(), o.name.clone(), "--gamma".into(), o.gamma.clone(),
        "--brightness".into(), o.brightness.clone()]).collect()
}

fn turn_on() -> Result<(), String> {
    let mut active = ACTIVE.lock().map_err(|_| "privacy state poisoned")?;
    if active.is_some() { return Ok(()); }
    let display = crate::capture::x11_display().ok_or("X11 display not open")?;
    let outputs = parse_outputs(&xrandr(&display, &["--verbose".into()])?);
    if outputs.is_empty() { return Err("no active output".into()); }
    let saved = Saved { display, outputs };
    std::fs::write(saved_path(), serde_json::to_vec(&saved).map_err(|e| e.to_string())?).map_err(|e| e.kind().to_string())?;
    if let Err(e) = xrandr(&saved.display, &black_args(&saved.outputs)) {
        let _ = xrandr(&saved.display, &restore_args(&saved.outputs));
        let _ = std::fs::remove_file(saved_path());
        return Err(e);
    }
    log::info!("Privacy screen on: outputs={}", saved.outputs.len());
    *active = Some(saved);
    Ok(())
}

/// Restores the monitors; also used on session end and shutdown. Blocking.
pub fn disable() {
    let Ok(mut active) = ACTIVE.lock() else { return };
    let Some(saved) = active.take() else { return };
    match xrandr(&saved.display, &restore_args(&saved.outputs)) {
        Ok(_) => { let _ = std::fs::remove_file(saved_path()); log::info!("Privacy screen off"); }
        Err(e) => log::warn!("Privacy screen restore failed: {e}"),
    }
}

/// At startup: a previous run that ended while private left the monitors black.
pub fn recover() {
    let Ok(bytes) = std::fs::read(saved_path()) else { return };
    match serde_json::from_slice::<Saved>(&bytes) {
        Ok(saved) => match xrandr(&saved.display, &restore_args(&saved.outputs)) {
            Ok(_) => { let _ = std::fs::remove_file(saved_path()); log::info!("Privacy screen left by a previous run restored"); }
            Err(e) => log::warn!("Privacy screen left by a previous run not restored: {e}"),
        },
        Err(_) => { let _ = std::fs::remove_file(saved_path()); }
    }
}

/// RpcRequest field 7 -> RpcResponse { 1 header, 3 PrivateScreenSettingResponse }. Blocking (xrandr).
pub fn rpc_response(rpc: &[crate::protocol::WireField]) -> Option<Vec<u8>> {
    use crate::protocol::{decode_wire, WireValue};
    let body = |n: u32| rpc.iter().find_map(|f| match (f.number, &f.value) { (x, WireValue::Bytes(b)) if x == n => Some(*b), _ => None });
    let request = decode_wire(body(7)?).ok()?;
    let varint = |n: u32| request.iter().find_map(|f| match (f.number, &f.value) { (x, WireValue::Varint(v)) if x == n => Some(*v), _ => None });
    let (enable, requested) = (varint(1).unwrap_or(0) != 0, varint(2).unwrap_or(0));
    let result = if requested == PS_ENHANCED {
        Err("enhanced privacy (wallpaper) is not supported".to_owned())
    } else if !supported() {
        Err("privacy screen needs the X11 desktop".to_owned())
    } else {
        if requested == PS_NORMAL || enable {
            if let Err(e) = crate::system_actions::merge_save(|s| s.privacy_screen_type = PS_NORMAL) { log::warn!("Privacy type not saved: {e}"); }
        }
        if enable { turn_on() } else { disable(); Ok(()) }
    };
    let error = match result {
        Ok(()) => 0,
        Err(e) => { log::warn!("Privacy screen request failed: enable={enable} type={requested}: {e}"); ERROR_FAILED }
    };
    log::info!("Privacy screen request: enable={} type={} source={:?} error={}", enable, requested, varint(3), error);
    let mut payload = Vec::new();
    if error != 0 { payload.extend_from_slice(&[8, error as u8]); }
    if requested != 0 { payload.push(16); varint_into(requested, &mut payload); }
    payload.push(24); varint_into(current_type(), &mut payload);
    let header = body(1).unwrap_or(&[]);
    let mut out = vec![10];
    varint_into(header.len() as u64, &mut out);
    out.extend_from_slice(header);
    out.push(26);
    varint_into(payload.len() as u64, &mut out);
    out.extend_from_slice(&payload);
    Some(out)
}

pub fn is_request(rpc: &[crate::protocol::WireField]) -> bool { rpc.iter().any(|f| f.number == 7) }

fn varint_into(mut v: u64, out: &mut Vec<u8>) {
    while v >= 128 { out.push((v as u8 & 127) | 128); v >>= 7; }
    out.push(v as u8);
}

#[cfg(test)]
mod tests {
    use super::*;

    const VERBOSE: &str = "Screen 0: minimum 8 x 8, current 4480 x 1440, maximum 32767 x 32767
HDMI-1 connected primary 2560x1440+0+0 (0x46) normal (normal left inverted right x axis y axis) 597mm x 336mm
\tIdentifier: 0x42
\tGamma:      1.0:1.0:1.0
\tBrightness: 1.0
  2560x1440 (0x46) 241.500MHz +HSync -VSync *current +preferred
DP-1 connected 1920x1080+2560+0 (0x50) normal (normal left inverted right x axis y axis) 527mm x 296mm
\tGamma:      1.0:0.9:0.8
\tBrightness: 0.0
DP-2 connected (normal left inverted right x axis y axis)
\tGamma:      1.0:1.0:1.0
HDMI-2 disconnected (normal left inverted right x axis y axis)
";

    #[test]
    fn active_outputs_keep_gamma_and_a_black_brightness_is_not_saved() {
        let outputs = parse_outputs(VERBOSE);
        assert_eq!(outputs, vec![
            Output { name: "HDMI-1".into(), gamma: "1.0:1.0:1.0".into(), brightness: "1.0".into() },
            Output { name: "DP-1".into(), gamma: "1.0:0.9:0.8".into(), brightness: "1.0".into() },
        ]);
        assert_eq!(black_args(&outputs[..1]), ["--output", "HDMI-1", "--brightness", "0"]);
        assert_eq!(restore_args(&outputs[1..]), ["--output", "DP-1", "--gamma", "1.0:0.9:0.8", "--brightness", "1.0"]);
    }

    #[test]
    fn settings_report_private_screen_on_and_its_type() {
        use crate::system_metrics::{payload_with_settings, Permission::Allow, Settings};
        let on = payload_with_settings(Allow, Allow, Allow, "", &Settings { private_screen: Some((true, PS_NORMAL)), ..Default::default() });
        assert!(on.ends_with(&[34, 10, 32, 1, 56, 2, 64, 2, 72, 2, 80, 2]));
        let unset = payload_with_settings(Allow, Allow, Allow, "", &Settings { private_screen: Some((false, PS_NOT_SET)), ..Default::default() });
        assert!(unset.ends_with(&[34, 8, 56, 1, 64, 2, 72, 2, 80, 2]));
    }

    #[test]
    fn unsupported_request_answers_failure_with_the_header() {
        use crate::protocol::{decode_wire, WireValue};
        // RpcRequest { 1 header{1:5}, 7 { 1 enable, 2 NORMAL, 3 source 1 } }
        let rpc = [10, 2, 8, 5, 58, 6, 8, 1, 16, 2, 24, 1];
        let fields = decode_wire(&rpc).unwrap();
        assert!(is_request(&fields));
        let out = rpc_response(&fields).unwrap();
        let outer = decode_wire(&out).unwrap();
        assert_eq!(outer[0].value, WireValue::Bytes(&[8, 5]));
        let WireValue::Bytes(payload) = outer[1].value else { panic!("payload") };
        assert_eq!(outer[1].number, 3);
        let payload = decode_wire(payload).unwrap();
        assert_eq!(payload[0].value, WireValue::Varint(ERROR_FAILED));
        assert_eq!(payload[1].value, WireValue::Varint(PS_NORMAL));
        assert_eq!(payload[2].number, 3);
        assert!(!active());
    }
}
