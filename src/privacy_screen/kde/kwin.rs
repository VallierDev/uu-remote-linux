//! KWin scripts (org.kde.kwin.Scripting): one-shot ones, plus the new-window keeper that picture
//! mode leaves loaded for the whole privacy session.
use super::super::tool::gdbus;

const ONE_SHOT: &str = "uu-privacy";
const KEEP_WINDOWS: &str = "uu-privacy-windows";

fn call(object: &str, method: &str, args: &[&str]) -> Result<String, String> { gdbus("org.kde.KWin", object, method, args) }
/// Output names go into a JS string literal.
fn escape(output: &str) -> String { output.replace('\\', "\\\\").replace('"', "\\\"") }
/// Windows that belong on the captured output. Desktop pets and tool palettes use toolbar/utility
/// windows, which `normalWindow` misses; panels, the desktop and transient popups stay where they
/// are, so the layout of the physical outputs is unchanged.
const MOVABLE: &str = r#"function movable(w) {
    if (!w || w.desktopWindow || w.dock) return false;
    if (w.tooltip || w.popupMenu || w.dropdownMenu || w.comboBox || w.dndIcon) return false;
    return w.normalWindow || w.dialog || w.toolbar || w.utility || w.splash;
}"#;
/// Loads `js` under `name` and runs it. `keep` leaves it loaded with its signal connections alive;
/// otherwise it is unloaded after the run, as the one-shot scripts expect.
fn script(js: &str, name: &str, keep: bool) -> Result<(), String> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).ok_or("missing runtime directory")?;
    let file = dir.join(format!("uu-privacy-{name}-{}.js", std::process::id()));
    std::fs::write(&file, js).map_err(|e| e.kind().to_string())?;
    // A script left loaded by a host that died would keep running with a stale target.
    let _ = call("/Scripting", "org.kde.kwin.Scripting.unloadScript", &[name]);
    let result = call("/Scripting", "org.kde.kwin.Scripting.loadScript", &[&file.to_string_lossy(), name]).and_then(|out| {
        let id: String = out.chars().filter(char::is_ascii_digit).collect();
        if id.is_empty() { return Err("script not loaded".into()); }
        call(&format!("/Scripting/Script{id}"), "org.kde.kwin.Script.run", &[])
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    // A failed run must not leave a half-started keeper behind.
    if !keep || result.is_err() { let _ = call("/Scripting", "org.kde.kwin.Scripting.unloadScript", &[name]); }
    let _ = std::fs::remove_file(file);
    result.map(|_| ())
}
/// Sends every movable window to `output` (a name from kscreen-doctor).
pub fn move_windows_to(output: &str) -> Result<(), String> {
    let output = escape(output);
    script(&format!(r#"{movable}
const v = workspace.screens.find(o => o.name == "{output}");
if (v) for (const w of workspace.windowList()) if (movable(w)) workspace.sendClientToScreen(w, v);"#, movable = MOVABLE), ONE_SHOT, false)
}
/// Keeps windows on `output`. Picture mode turns the physical outputs back on as secondaries, and
/// KWin then places a new window on the screen it considers active, which can be one hidden under
/// the cover: the viewer would never see it. Every window is checked when it appears and whenever
/// its geometry changes, so an application that places itself on a physical output is sent back.
/// `frameGeometryChanged` is the trigger because KWin 6.8 does not emit `outputChanged` for
/// `sendClientToScreen` moves (verified live). Unloaded by `unload_keep_windows` on restore.
pub fn keep_windows_on(output: &str) -> Result<(), String> {
    let output = escape(output);
    script(&format!(r#"{movable}
function keep(w) {{
    if (!movable(w)) return;
    // The cover windows belong on the physical outputs.
    if (w.resourceClass === "uuremote-server" || (w.caption || "").indexOf("uu-remote-linux privacy") === 0) return;
    const screen = workspace.screens.find(o => o.name === "{output}");
    if (!screen || (w.output && w.output.name === "{output}")) return;
    workspace.sendClientToScreen(w, screen);
}}
function watch(w) {{
    if (!w) return;
    // One window that cannot be moved or observed must not stop the others.
    try {{
        keep(w);
        w.frameGeometryChanged.connect(function () {{ keep(w); }});
    }} catch (e) {{}}
}}
// New windows first: a throwing sweep must not leave them unhandled.
workspace.windowAdded.connect(watch);
workspace.windowList().forEach(watch);
"#, movable = MOVABLE), KEEP_WINDOWS, true)
}
/// Stops following new windows. A name that is not loaded (screen-off mode, a restarted KWin
/// session) reports false without an error.
pub fn unload_keep_windows() -> Result<(), String> {
    call("/Scripting", "org.kde.kwin.Scripting.unloadScript", &[KEEP_WINDOWS]).map(|_| ())
}
/// Hides the cover window from the task manager. GTK's skip-taskbar hint is an X11 hint and KWin
/// ignores it on Wayland, so the cover showed up as an unidentified window with the theme's
/// generic "wayland" icon. `skipTaskbar` is writable through KWin scripting. Picture mode creates
/// one cover per physical monitor with the same title, and KWin appends " <2>" and so on to
/// duplicate captions, so match the title as a prefix.
pub fn hide_cover_from_taskbar() -> Result<(), String> {
    script(r#"for (const w of workspace.windowList())
if (w.resourceClass === "uuremote-server" && w.caption.indexOf("uu-remote-linux privacy") === 0) w.skipTaskbar = true;"#, ONE_SHOT, false)
}

#[cfg(test)]
mod tests {
    #[test]
    fn escapes_output_names_for_the_script() {
        assert_eq!(super::escape("DP-3"), "DP-3");
        assert_eq!(super::escape(r#"a"b\c"#), r#"a\"b\\c"#);
    }
}
