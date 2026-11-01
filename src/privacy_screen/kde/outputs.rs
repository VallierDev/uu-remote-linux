//! KWin output layout through libkscreen's `kscreen-doctor`.
use super::super::tool::{run, wait_until};

#[derive(Debug, PartialEq)]
pub struct Output { pub name: String, pub rect: [f64; 4] }

fn json() -> Result<String, String> { run("kscreen-doctor", &["-j".into()]) }
/// Enabled, connected outputs that KWin did not create for a screencast.
fn physical_in(text: &str) -> Result<Vec<String>, String> {
    let json: serde_json::Value = serde_json::from_str(text).map_err(|_| "invalid output configuration")?;
    let outputs = json["outputs"].as_array().ok_or("missing outputs")?;
    let names: Vec<String> = outputs.iter()
        .filter(|o| o["enabled"] == true && o["connected"] == true)
        .filter_map(|o| o["name"].as_str())
        .filter(|name| !name.starts_with("Virtual") && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
        .map(str::to_owned).collect();
    if names.is_empty() { return Err("no active physical outputs".into()); }
    Ok(names)
}
pub fn physical() -> Result<Vec<String>, String> { physical_in(&json()?) }
/// Enabled outputs in KWin's logical coordinates (mode size / scale, rotated).
fn layout_in(text: &str) -> Result<Vec<Output>, String> {
    let json: serde_json::Value = serde_json::from_str(text).map_err(|_| "invalid output configuration")?;
    let outputs = json["outputs"].as_array().ok_or("missing outputs")?;
    // KWin's screencast virtual output has no "enabled" field (seen live, Plasma 6).
    Ok(outputs.iter().filter(|o| o["enabled"] != false).filter_map(|o| {
        let mode = o["modes"].as_array()?.iter().find(|m| m["id"] == o["currentModeId"])?;
        let (mut w, mut h) = (mode["size"]["width"].as_f64()?, mode["size"]["height"].as_f64()?);
        if matches!(o["rotation"].as_u64(), Some(2 | 8)) { std::mem::swap(&mut w, &mut h); }
        let scale = o["scale"].as_f64().filter(|s| *s > 0.0).unwrap_or(1.0);
        Some(Output { name: o["name"].as_str()?.into(), rect: [o["pos"]["x"].as_f64()?, o["pos"]["y"].as_f64()?, w / scale, h / scale] })
    }).collect())
}
pub fn layout() -> Result<Vec<Output>, String> { layout_in(&json()?) }
/// `name` as fractions (x, y, w, h) of the layout's bounding box.
pub fn region(layout: &[Output], name: &str) -> Option<[f64; 4]> {
    let target = layout.iter().find(|o| o.name == name)?.rect;
    let (x0, y0) = layout.iter().fold((f64::MAX, f64::MAX), |(x, y), o| (x.min(o.rect[0]), y.min(o.rect[1])));
    let (x1, y1) = layout.iter().fold((f64::MIN, f64::MIN), |(x, y), o| (x.max(o.rect[0] + o.rect[2]), y.max(o.rect[1] + o.rect[3])));
    let (w, h) = (x1 - x0, y1 - y0);
    (w > 0.0 && h > 0.0).then(|| [(target[0] - x0) / w, (target[1] - y0) / h, target[2] / w, target[3] / h])
}
/// Enables or disables `outputs` and waits until KWin has applied it.
pub fn set(outputs: &[String], enable: bool) -> Result<(), String> {
    let action = if enable { "enable" } else { "disable" };
    run("kscreen-doctor", &outputs.iter().map(|o| format!("output.{o}.{action}")).collect::<Vec<_>>())?;
    let applied = wait_until(|| {
        let now = physical().unwrap_or_default();
        outputs.iter().all(|o| now.contains(o) == enable)
    });
    if applied { Ok(()) } else { Err(format!("outputs not {action}d")) }
}
pub fn set_primary(name: &str) -> Result<(), String> { run("kscreen-doctor", &[format!("output.{name}.priority.1")]).map(|_| ()) }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshot_keeps_enabled_physical_outputs_only() {
        let names = physical_in(r#"{"outputs":[{"name":"eDP-1","enabled":true,"connected":true},
            {"name":"DP-1","enabled":false,"connected":true},{"name":"Virtual-uu","enabled":true,"connected":true},
            {"name":"HDMI-A-1","enabled":true,"connected":false}]}"#).unwrap();
        assert_eq!(names, ["eDP-1"]);
        assert!(physical_in(r#"{"outputs":[{"name":"Virtual-x","enabled":true,"connected":true}]}"#).is_err());
    }
    #[test]
    fn pointer_region_is_the_virtual_output_in_the_layout() {
        let all = layout_in(r#"{"outputs":[
            {"name":"eDP-1","enabled":true,"currentModeId":"2","modes":[{"id":"2","size":{"width":3072,"height":1920}}],"pos":{"x":0,"y":0},"scale":1.5,"rotation":1},
            {"name":"Virtual-1","currentModeId":"1","modes":[{"id":"1","size":{"width":1920,"height":1080}}],"pos":{"x":2048,"y":0},"scale":1,"rotation":1},
            {"name":"DP-1","enabled":false,"currentModeId":"1","modes":[{"id":"1","size":{"width":1,"height":1}}],"pos":{"x":0,"y":0}}]}"#).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].rect, [0.0, 0.0, 2048.0, 1280.0]);
        assert_eq!(region(&all, "Virtual-1").unwrap(), [2048.0 / 3968.0, 0.0, 1920.0 / 3968.0, 1080.0 / 1280.0]);
    }
}
