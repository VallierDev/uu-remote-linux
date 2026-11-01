//! Picture mode's cover: a fullscreen keep-above window per physical monitor (GTK, in its own
//! process so it cannot disturb the host's UI thread), killed on restore and with the host.
use std::{process::{Child, Command, Stdio}, sync::Mutex};

static COVER: Mutex<Option<Child>> = Mutex::new(None);

pub fn supported() -> bool { cfg!(uu_gtk) }
/// `rects` are the monitors to cover in the desktop's logical coordinates (x, y, w, h).
pub fn show(image: Option<&std::path::Path>, rects: &[[f64; 4]]) -> Result<(), String> {
    hide();
    let exe = std::env::current_exe().map_err(|e| e.kind().to_string())?;
    let mut command = Command::new(exe);
    command.arg("privacy-wallpaper").arg(image.map(|p| p.as_os_str().to_owned()).unwrap_or_default());
    for rect in rects { command.arg(rect.iter().map(|v| (v.round() as i64).to_string()).collect::<Vec<_>>().join(",")); }
    // Keep the pipe writer in Child for the host's lifetime, independently of its spawning thread.
    let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().map_err(|e| format!("cover window: {}", e.kind()))?;
    // A spawned child is not a shown cover: GTK init or monitor matching can still fail (exit 1/2).
    let stdout = child.stdout.take().ok_or("cover window: no stdout")?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let mut line = String::new();
        let _ = tx.send(std::io::BufReader::new(stdout).read_line(&mut line).is_ok() && line.trim() == "ready");
    });
    if rx.recv_timeout(std::time::Duration::from_secs(5)) != Ok(true) {
        let _ = child.kill();
        let status = child.wait().ok().and_then(|s| s.code());
        return Err(format!("cover window not shown (exit {status:?})"));
    }
    *COVER.lock().map_err(|_| "cover state poisoned")? = Some(child);
    Ok(())
}
pub fn hide() {
    if let Some(mut child) = COVER.lock().ok().and_then(|mut c| c.take()) { let _ = child.kill(); let _ = child.wait(); }
}
/// `privacy-wallpaper <image> <x,y,w,h>...`: the cover window process.
pub fn main(args: &[String]) -> i32 {
    let rects: Vec<i32> = args.iter().skip(1).flat_map(|r| r.split(',').map(|v| v.parse().unwrap_or(-1))).collect();
    #[cfg(uu_gtk)] {
        extern "C" { fn uu_privacy_wallpaper_run(image: *const libc::c_char, rects: *const i32, count: i32) -> i32; }
        let image = std::ffi::CString::new(args.first().cloned().unwrap_or_default()).unwrap_or_default();
        unsafe { uu_privacy_wallpaper_run(image.as_ptr(), rects.as_ptr(), (rects.len() / 4) as i32) }
    }
    #[cfg(not(uu_gtk))] { let _ = rects; 1 }
}
