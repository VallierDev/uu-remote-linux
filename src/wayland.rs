//! Hyprland/wlroots native backend. No XWayland fallback and no input logging.
#[cfg(all(target_os="linux", feature="wayland"))]
extern "C" {
    fn uu_wl_init(out_w: *mut i32, out_h: *mut i32) -> i32;
    fn uu_wl_capture(buffer: *mut u8, length: usize) -> i32;
    fn uu_wl_move(x: i32, y: i32, relative: i32) -> i32;
    fn uu_wl_button(button: i32, pressed: i32) -> i32;
    fn uu_wl_key(symbol: u64, pressed: i32) -> i32;
    fn uu_wl_wake() -> i32;
    fn uu_wl_release_all() -> i32;
    fn uu_wl_type_utf8(text: *const libc::c_char) -> i32;
    fn uu_wl_text_ready() -> i32;
}
/// How many characters one keymap upload can carry; 0 means this keymap has no spare keycode.
pub fn text_ready()->i32 {
    #[cfg(all(target_os="linux",feature="wayland"))]
    { return unsafe { uu_wl_text_ready() }; }
    #[cfg(not(all(target_os="linux",feature="wayland")))]
    -1
}
/// Mobile input-method text (#13/#46). Returns the number of characters typed, or a negative code
/// when this keymap has no spare keycode to carry them. Never logs the text.
pub fn type_utf8(text:&str)->i32 {
    #[cfg(all(target_os="linux",feature="wayland"))]
    {
        let Ok(c_text)=std::ffi::CString::new(text.replace('\0',"")) else { return -1 };
        return unsafe { uu_wl_type_utf8(c_text.as_ptr()) };
    }
    #[cfg(not(all(target_os="linux",feature="wayland")))]
    { let _=text; -1 }
}
pub fn release_all()->i32 {
    #[cfg(all(target_os="linux",feature="wayland"))]
    {return unsafe {uu_wl_release_all()};}
    #[cfg(not(all(target_os="linux",feature="wayland")))]
    -1
}
pub fn init() -> Result<(u32,u32),String> {
    #[cfg(all(target_os="linux", feature="wayland"))]
    {
        let (mut w,mut h)=(0,0);
        let result=unsafe { uu_wl_init(&mut w,&mut h) };
        if result != 0 || w <= 0 || h <= 0 {
            return Err(format!("Wayland initialization failed ({result}); requires screencopy, virtual keyboard and virtual pointer. Check UU_WAYLAND_OUTPUT and session environment."));
        }
        return Ok((w as u32,h as u32));
    }
    #[cfg(not(all(target_os="linux", feature="wayland")))]
    Err("this build was compiled without the wayland feature; rebuild with --features wayland".into())
}
pub fn capture(buffer:&mut [u8]) -> Result<(),String> {
    #[cfg(all(target_os="linux", feature="wayland"))]
    { let result=unsafe { uu_wl_capture(buffer.as_mut_ptr(),buffer.len()) }; if result==0 { return Ok(()) } return Err(format!("Wayland capture failed ({result})")); }
    #[cfg(not(all(target_os="linux", feature="wayland")))]
    { let _=buffer; Err("Wayland capture unavailable".into()) }
}
pub fn wake()->Result<(),String> {
    #[cfg(all(target_os="linux", feature="wayland"))]
    { let result=unsafe { uu_wl_wake() }; if result==0 { return Ok(()); } return Err("Wayland output power control unavailable".into()); }
    #[cfg(not(all(target_os="linux", feature="wayland")))]
    Err("Wayland output power control unavailable".into())
}
pub fn move_pointer(x:i32,y:i32,relative:bool)->i32 {
    #[cfg(all(target_os="linux", feature="wayland"))]
    { return unsafe { uu_wl_move(x,y,i32::from(relative)) }; }
    #[cfg(not(all(target_os="linux", feature="wayland")))]
    { let _=(x,y,relative); -1 }
}
pub fn button(button:i32,pressed:bool)->i32 {
    #[cfg(all(target_os="linux", feature="wayland"))]
    { return unsafe { uu_wl_button(button,i32::from(pressed)) }; }
    #[cfg(not(all(target_os="linux", feature="wayland")))]
    { let _=(button,pressed); -1 }
}
pub fn key(symbol:u64,pressed:bool)->i32 {
    #[cfg(all(target_os="linux", feature="wayland"))]
    { return unsafe { uu_wl_key(symbol,i32::from(pressed)) }; }
    #[cfg(not(all(target_os="linux", feature="wayland")))]
    { let _=(symbol,pressed); -1 }
}

/// A separate process owns capture; encoder cancellation closes its pipe.
/// Capture and the pipe write run on separate threads with two frame buffers, so a
/// slow encoder read no longer delays the next screencopy request.
pub fn capture_stdout() -> Result<(),String> {
    use std::io::{Read,Write};
    let (w,h)=init()?;
    let size=(w as usize)*(h as usize)*4;
    let fps=capture_fps(std::env::var("UU_WAYLAND_FPS").ok().as_deref())?;
    let interval=std::time::Duration::from_nanos(1_000_000_000/u64::from(fps));
    // A larger pipe absorbs more of each frame per write; failure keeps the default.
    #[cfg(target_os="linux")]
    let _=unsafe { libc::fcntl(1, libc::F_SETPIPE_SZ, 1<<20) };
    let (full_tx,full_rx)=std::sync::mpsc::sync_channel::<Vec<u8>>(1);
    let (free_tx,free_rx)=std::sync::mpsc::sync_channel::<Vec<u8>>(2);
    for _ in 0..2 { free_tx.send(vec![0u8;size]).map_err(|_|"Capture buffers")?; }
    let writer=std::thread::Builder::new().name("uu-wl-write".into()).spawn(move || -> Result<(),String> {
        let mut stdout=std::io::stdout().lock();
        for frame in full_rx {
            stdout.write_all(&frame).map_err(|e|format!("Capture pipe: {e}"))?;
            stdout.flush().map_err(|e|format!("Capture pipe: {e}"))?;
            if free_tx.send(frame).is_err() { break; }
        }
        Ok(())
    }).map_err(|e|format!("Capture writer: {e}"))?;
    let demand=std::env::var("UU_WAYLAND_DEMAND").is_ok_and(|v|v=="1");
    let mut stdin=std::io::stdin().lock();
    let mut delivered=std::time::Instant::now();
    loop {
        if demand { stdin.read_exact(&mut [0]).map_err(|_|"Capture demand pipe closed")?; }
        // Time from buffer availability: a slow encoder read must not shorten the next interval.
        let Ok(mut frame)=free_rx.recv() else { break };
        let start=std::time::Instant::now();
        // The engine prefetches the next frame on delivery; after a backpressure stall that
        // prefetch is as old as the stall, so drop it and capture the current screen.
        if demand && start.duration_since(delivered)>interval*2 { capture(&mut frame)?; }
        capture(&mut frame)?;
        delivered=std::time::Instant::now();
        if full_tx.send(frame).is_err() { break; }
        if !demand {
            if let Some(left)=interval.checked_sub(start.elapsed()) { std::thread::sleep(left); }
        }
    }
    drop(full_tx);
    writer.join().map_err(|_|"Capture writer panicked")?
}

pub(crate) fn capture_fps(value:Option<&str>)->Result<u32,String> {
    match value {
        None=>Ok(crate::video::FPS as u32),
        Some(value)=>value.parse::<u32>().ok().filter(|fps|(5..=144).contains(fps))
            .ok_or_else(||"UU_WAYLAND_FPS must be between 5 and 144".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn capture_rate_accepts_viewer_range_and_rejects_invalid_values() {
        assert_eq!(super::capture_fps(None),Ok(60));
        for fps in [5,15,30,60,90,144] {assert_eq!(super::capture_fps(Some(&fps.to_string())),Ok(fps));}
        for value in ["0","4","145","-1","NaN",""] {assert!(super::capture_fps(Some(value)).is_err());}
    }
}

/// Use only in an isolated acceptance desktop; emits fixed test events, no logs.
pub fn input_probe()->Result<(),String> {
    let (w,h)=init()?;
    wake()?;
    if move_pointer((w/4) as i32,(h/4) as i32,false)!=0 { return Err("Pointer probe failed".into()); }
    std::thread::sleep(std::time::Duration::from_millis(50));
    if move_pointer((w/2) as i32,(h/2) as i32,false)!=0 { return Err("Pointer probe failed".into()); }
    std::thread::sleep(std::time::Duration::from_millis(100));
    if button(1,true)!=0 || button(1,false)!=0 { return Err("Button probe failed".into()); }
    std::thread::sleep(std::time::Duration::from_millis(100));
    if move_pointer(32,0,true)!=0 { return Err("Relative pointer probe failed".into()); }
    std::thread::sleep(std::time::Duration::from_millis(100));
    for symbol in b"uuwayland" {
        if key(u64::from(*symbol),true)!=0 || key(u64::from(*symbol),false)!=0 {
            return Err("Keyboard probe failed".into());
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(250));
    println!("Wayland test events sent; verify the native GTK acceptance report.");
    Ok(())
}

/// Local diagnostic for the mobile-text path: report the keymap's capacity for it, then type a
/// sample into the focused window. Only the built-in sample is echoed; UU_PROBE_TEXT stays private.
pub fn text_probe()->Result<(),String> {
    init()?;
    let ready=text_ready();
    if ready<=0 { return Err("this keymap has no spare keycode: mobile text input cannot be carried".into()); }
    const SAMPLE:&str="uu remote abc 你好世界";
    let (sample,builtin)=match std::env::var("UU_PROBE_TEXT") {
        Ok(value)=>(value,false), Err(_)=>(SAMPLE.to_owned(),true),
    };
    println!("Wayland text probe: spare keycodes={ready} characters={} bytes={}",sample.chars().count(),sample.len());
    if builtin { println!("Sample: {sample}"); }
    // UU_PROBE_SEQUENCE types each |-separated field as its own call - one text_input message each,
    // the way the phone's input method sends them - with UU_PROBE_STEP_MS between the calls. One
    // process means one virtual keyboard, exactly as the host has.
    let step=std::env::var("UU_PROBE_STEP_MS").ok().and_then(|v|v.parse::<u64>().ok()).unwrap_or(0);
    let sequence=std::env::var("UU_PROBE_SEQUENCE").unwrap_or_else(|_|sample.clone());
    let mut typed=0;
    for (index,message) in sequence.split('|').enumerate() {
        if index>0 && step>0 { std::thread::sleep(std::time::Duration::from_millis(step)); }
        let one=type_utf8(message);
        if one<0 { return Err(format!("Wayland text typing failed ({one})")); }
        typed+=one;
    }
    if typed<0 { return Err(format!("Wayland text typing failed ({typed})")); }
    println!("Typed {typed} of {} characters in {} calls; check that they match.",
        sequence.chars().filter(|character|*character!='|').count(),sequence.split('|').count());
    Ok(())
}
