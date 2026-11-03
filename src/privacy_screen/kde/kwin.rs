//! One-shot KWin scripts (org.kde.kwin.Scripting).
use super::super::tool::gdbus;

fn call(object: &str, method: &str, args: &[&str]) -> Result<String, String> { gdbus("org.kde.KWin", object, method, args) }
fn script(js: &str) -> Result<(), String> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).ok_or("missing runtime directory")?;
    let file = dir.join(format!("uu-privacy-{}.js", std::process::id()));
    std::fs::write(&file, js).map_err(|e| e.kind().to_string())?;
    let name = "uu-privacy";
    let _ = call("/Scripting", "org.kde.kwin.Scripting.unloadScript", &[name]);
    let result = call("/Scripting", "org.kde.kwin.Scripting.loadScript", &[&file.to_string_lossy(), name]).and_then(|out| {
        let id: String = out.chars().filter(char::is_ascii_digit).collect();
        if id.is_empty() { return Err("script not loaded".into()); }
        call(&format!("/Scripting/Script{id}"), "org.kde.kwin.Script.run", &[])
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    let _ = call("/Scripting", "org.kde.kwin.Scripting.unloadScript", &[name]);
    let _ = std::fs::remove_file(file);
    result.map(|_| ())
}
/// Sends every normal window and dialog to `output` (a name from kscreen-doctor, file-name safe).
pub fn move_windows_to(output: &str) -> Result<(), String> {
    script(&format!(r#"const v = workspace.screens.find(o => o.name == "{output}");
if (v) for (const w of workspace.windowList()) if (w.normalWindow || w.dialog) workspace.sendClientToScreen(w, v);"#))
}
/// Hides the cover window from the task manager. GTK's skip-taskbar hint is an X11 hint and KWin
/// ignores it on Wayland, so the cover showed up as an unidentified window with the theme's
/// generic "wayland" icon. `skipTaskbar` is writable through KWin scripting. Picture mode creates
/// one cover per physical monitor with the same title, and KWin appends " <2>" and so on to
/// duplicate captions, so match the title as a prefix.
pub fn hide_cover_from_taskbar() -> Result<(), String> {
    script(r#"for (const w of workspace.windowList())
if (w.resourceClass === "uuremote-server" && w.caption.indexOf("uu-remote-linux privacy") === 0) w.skipTaskbar = true;"#)
}
