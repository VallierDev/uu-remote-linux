//! Hyprland/wlroots native backend. No XWayland fallback and no input logging.
#[cfg(all(target_os="linux", feature="wayland"))]
crate::engine_ffi! { crate::engine_plugin::Engine::Wayland;
    fn uu_wl_select_output(name: *const libc::c_char, width: u32, height: u32) -> i32;
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
        crate::engine_plugin::load(crate::engine_plugin::Engine::Wayland).map_err(|reason| format!("Wayland engine unavailable: {reason}"))?;
        let (mut w,mut h)=(0,0);
        let result=unsafe { uu_wl_init(&mut w,&mut h) };
        if result != 0 || w <= 0 || h <= 0 {
            return Err(format!("Wayland initialization failed ({result}); requires screencopy, virtual keyboard and virtual pointer. Check UU_WAYLAND_OUTPUT and session environment."));
        }
        // The compositor registry and hyprctl need not enumerate outputs in the
        // same order. Bind the initial engine to the inventory's selected output.
        static BINDING: std::sync::OnceLock<Result<(),String>>=std::sync::OnceLock::new();
        if !outputs().is_empty() {
            BINDING.get_or_init(||select(0)).as_ref().map_err(Clone::clone)?;
            return Ok(input_size((w as u32,h as u32)));
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
    std::thread::sleep(std::time::Duration::from_millis(100));
    println!("Typed {typed} of {} characters in {} calls; check that they match.",
        sequence.chars().filter(|character|*character!='|').count(),sequence.split('|').count());
    Ok(())
}

// Opt-in single-track monitor switching: Hyprland outputs (UU_WAYLAND_MULTISCREEN) or X11 RandR
// monitors (UU_X11_MULTISCREEN). The existing engines own capture/pointer binding; X11 crops the
// root to the selected monitor. No compositor layout or user credentials are modified.
#[derive(Clone, Debug)]
pub struct Output { pub name:String, pub width:u32, pub height:u32, pub fps:i32, pub x:i32, pub y:i32 }
struct Inventory { outputs:Vec<Output>, x11:bool, portal:bool }
static OUTPUTS: std::sync::OnceLock<Inventory> = std::sync::OnceLock::new();
#[derive(Default)]
struct Selection { id:usize, generation:u64, presented:Option<u64>, failed:bool }
impl Selection {
    fn begin(&mut self,id:usize) {
        self.id=id; self.generation+=1; self.presented=None; self.failed=false;
    }
    fn sent(&mut self,id:usize,generation:u64) {
        if (self.id,self.generation)==(id,generation) {self.presented=Some(generation);}
    }
    fn ready(&self)->bool {self.presented==Some(self.generation)}
}
static SELECTION:std::sync::Mutex<Selection>=std::sync::Mutex::new(Selection {id:0,generation:0,presented:None,failed:false});
fn inventory() -> &'static Inventory {
    OUTPUTS.get_or_init(|| {
        let backend=crate::desktop_backend::detect().ok();
        if backend==Some(crate::desktop_backend::DesktopBackend::X11) && !crate::desktop_backend::headless()
            && std::env::var("UU_X11_MULTISCREEN").ok().as_deref()==Some("1") {
            let outputs=x11_outputs();
            return Inventory {x11:!outputs.is_empty(),outputs,portal:false};
        }
        if backend==Some(crate::desktop_backend::DesktopBackend::Portal) && !crate::desktop_backend::headless() {
            let outputs=portal_outputs();
            return Inventory {portal:!outputs.is_empty(),outputs,x11:false};
        }
        if !cfg!(all(target_os="linux",feature="wayland"))
            || std::env::var("UU_WAYLAND_MULTISCREEN").ok().as_deref()!=Some("1")
            || backend!=Some(crate::desktop_backend::DesktopBackend::Wayland) {return Inventory {outputs:Vec::new(),x11:false,portal:false};}
        let niri=std::env::var_os("NIRI_SOCKET").is_some();
        let output=if niri {std::process::Command::new("niri").args(["msg","--json","outputs"]).output()}
            else {std::process::Command::new("hyprctl").args(["monitors","-j"]).output()};
        let result=output.map_err(|_|"monitor command unavailable").and_then(|output| {
            if !output.status.success() {return Err("monitor command failed");}
            let preferred=std::env::var("UU_WAYLAND_OUTPUT").unwrap_or_default();
            if niri {parse_niri_outputs(&output.stdout,&preferred)} else {parse_outputs(&output.stdout,&preferred)}
        });
        let outputs=match result {Ok(outputs)=>outputs,Err(reason)=>{log::warn!("Wayland multi-screen disabled: {}",reason);Vec::new()}};
        Inventory {outputs,x11:false,portal:false}
    })
}
pub fn outputs() -> &'static [Output] {&inventory().outputs}
/// GNOME portal grants one monitor per consent (multi-select is refused), so each monitor
/// gets its own saved consent and a switch opens that monitor's session. Read once.
fn portal_outputs()->Vec<Output> {
    let monitors=crate::portal::monitors();
    if monitors.len()<2 {return Vec::new();}
    let fps=crate::video::display_fps() as i32;
    let mut outputs:Vec<Output>=monitors.iter().map(|m|Output {name:m.connector.clone(),width:m.width,height:m.height,fps,x:m.x,y:m.y}).collect();
    // Screen 0 is the one opened first: the monitor whose consent is already saved, else the primary.
    let dir=crate::portal::permissions_directory();
    let granted=|name:&str|dir.as_deref().is_some_and(|d|d.join(crate::portal::monitor_key(name)).exists());
    let primary=monitors.iter().find(|m|m.primary).map(|m|m.connector.clone()).unwrap_or_default();
    outputs.sort_by_key(|o|(!granted(&o.name),o.name!=primary,o.x,o.y));
    outputs.truncate(16);
    log::info!("Portal multi-screen: {} monitors",outputs.len());
    outputs
}
/// Connector of the selected screen when the portal advertises several monitors.
pub fn portal_monitor()->Option<String> {
    if !inventory().portal {return None;}
    source(selected()).map(|o|o.name.clone())
}
/// Monitor at a portal stream's logical position (Mutter logical layout).
pub fn portal_monitor_at(x:i32,y:i32)->Option<String> {
    if !inventory().portal {return None;}
    outputs().iter().find(|o|(o.x,o.y)==(x,y)).map(|o|o.name.clone())
}
/// X11 monitors are advertised: the stream captures the selected monitor's region of the root.
pub fn x11_multiscreen()->bool {inventory().x11}
/// Read once, like the Hyprland inventory: a changed monitor layout needs a host restart.
fn x11_outputs()->Vec<Output> {
    let mut command=std::process::Command::new("xrandr");
    if let Some(display)=crate::capture::x11_display().or_else(||std::env::var("DISPLAY").ok()).filter(|d|!d.is_empty()) {
        command.arg("--display").arg(display);
    }
    let result=command.arg("--listmonitors").output().map_err(|_|"xrandr unavailable").and_then(|output| {
        if !output.status.success() {return Err("xrandr failed");}
        parse_x11_monitors(&String::from_utf8_lossy(&output.stdout),&std::env::var("UU_X11_OUTPUT").unwrap_or_default(),crate::video::display_fps() as i32)
    });
    match result {
        Ok(outputs) if outputs.len()>=2=>{log::info!("X11 multi-screen: {} monitors",outputs.len());outputs},
        Ok(_)=>{log::warn!("X11 multi-screen disabled: fewer than two monitors");Vec::new()},
        Err(reason)=>{log::warn!("X11 multi-screen disabled: {}",reason);Vec::new()},
    }
}
/// `xrandr --listmonitors`: ` 0: +*HDMI-0 1920/527x1080/296+1920+0  HDMI-0` (`*` = primary).
/// The preferred monitor (UU_X11_OUTPUT, else the primary) is id 0; the rest go left to right.
fn parse_x11_monitors(text:&str,preferred:&str,fps:i32)->Result<Vec<Output>,&'static str> {
    fn geometry(text:&str)->Option<(u32,u32,i32,i32)> {
        let (width,rest)=text.split_once('x')?;
        let split=rest.find('+')?;
        let (height,offsets)=rest.split_at(split);
        let mut offsets=offsets[1..].split('+');
        let size=|v:&str|v.split('/').next()?.parse::<u32>().ok();
        let (x,y)=(offsets.next()?.parse::<i32>().ok()?,offsets.next()?.parse::<i32>().ok()?);
        if offsets.next().is_some() || x<0 || y<0 {return None;}
        Some((size(width)?,size(height)?,x,y))
    }
    let mut seen=std::collections::HashSet::new();
    let mut primary=String::new();
    let mut outputs=Vec::new();
    for line in text.lines().skip(1) {
        let mut fields=line.split_whitespace();
        let (Some(_),Some(tag),Some(rect))=(fields.next(),fields.next(),fields.next()) else {continue};
        let name=tag.trim_start_matches(['+','*']);
        let Some((width,height,x,y))=geometry(rect) else {continue};
        if name.is_empty() || name.len()>127 || !seen.insert(name.to_owned())
            || width<2 || height<2 || width>16384 || height>16384 || u64::from(width)*u64::from(height)*4>256*1024*1024 {continue;}
        if tag.contains('*') {primary=name.to_owned();}
        outputs.push(Output {name:name.into(),width,height,fps,x,y});
    }
    let first=if preferred.is_empty() {primary} else {preferred.to_owned()};
    outputs.sort_by_key(|m| (m.name!=first,m.x,m.y));
    outputs.truncate(16);
    if outputs.is_empty() || (!preferred.is_empty() && outputs[0].name!=preferred) {return Err("No capturable preferred output");}
    Ok(outputs)
}
fn parse_niri_outputs(json:&[u8],preferred:&str)->Result<Vec<Output>,&'static str> {
    let inventory=serde_json::from_slice::<serde_json::Map<String,serde_json::Value>>(json).map_err(|_|"Invalid niri inventory")?;
    let monitors:Vec<_>=inventory.values().filter_map(|m| {
        let logical=m["logical"].as_object()?;
        let mode=m["modes"].as_array()?.get(usize::try_from(m["current_mode"].as_u64()?).ok()?)?;
        let transform=logical.get("transform")?.as_str()?;
        if !matches!(transform,"Normal"|"90"|"180"|"270") {return None;}
        let (w,h)=(mode["width"].as_u64()?,mode["height"].as_u64()?);
        let (w,h)=if matches!(transform,"90"|"270") {(h,w)} else {(w,h)};
        Some(serde_json::json!({"name":m["name"],"width":w,"height":h,"transform":0,
            "refreshRate":mode["refresh_rate"].as_f64()?/1000.0}))
    }).collect();
    parse_outputs(&serde_json::to_vec(&monitors).map_err(|_|"Invalid niri inventory")?,preferred)
}
fn parse_outputs(json:&[u8],preferred:&str)->Result<Vec<Output>,&'static str> {
    let monitors=serde_json::from_slice::<Vec<serde_json::Value>>(json).map_err(|_|"Invalid monitor inventory")?;
    let mut seen=std::collections::HashSet::new();
    let mut outputs:Vec<Output>=monitors.into_iter().filter_map(|m| {
        if m["disabled"].as_bool()==Some(true) || m["transform"].as_u64()!=Some(0) {return None;}
        let name=m["name"].as_str()?;
        let width=u32::try_from(m["width"].as_u64()?).ok()?;
        let height=u32::try_from(m["height"].as_u64()?).ok()?;
        if name.is_empty() || name.len()>127 || name.contains('\0') || !seen.insert(name.to_owned())
            || width<2 || height<2 || width>16384 || height>16384 || u64::from(width)*u64::from(height)*4>256*1024*1024 {return None;}
        Some(Output {name:name.into(),width,height,fps:m["refreshRate"].as_f64()?.round().clamp(1.0,240.0) as i32,x:0,y:0})
    }).collect();
    outputs.sort_by_key(|m| (m.name!=preferred,m.name.clone()));
    outputs.truncate(16);
    if outputs.is_empty() || (!preferred.is_empty() && outputs[0].name!=preferred) {return Err("No capturable preferred output");}
    Ok(outputs)
}
pub fn selection()->(usize,u64) {let s=SELECTION.lock().unwrap();(s.id,s.generation)}
pub fn selected()->usize {selection().0}
pub fn source(id:usize)->Option<&'static Output> {outputs().get(id)}
pub fn input_size(fallback:(u32,u32))->(u32,u32) {source(selected()).map_or(fallback,|s|(s.width,s.height))}
/// Region the encoder captures for this display: the Wayland output, or the selected X11 monitor.
pub fn capture_source(display:&str,portal:bool,id:usize)->Option<&'static Output> {
    if display.starts_with("wayland:") || (!portal && !display.starts_with("kms:") && x11_multiscreen()) {source(id)} else {None}
}
/// Offset of the selected X11 monitor in the root: absolute input is relative to that monitor.
pub fn x11_placement()->Option<(i32,i32)> {
    if !x11_multiscreen() {return None;}
    source(selected()).map(|s|(s.x,s.y))
}
/// Pixel size of one advertised output. Unknown ids keep `fallback` (connection size).
pub fn source_size(id:usize,fallback:(u32,u32))->(u32,u32) {
    #[cfg(test)]
    if let Some(outputs)=*test_outputs().lock().unwrap_or_else(|e|e.into_inner()) {
        return outputs.get(id).map_or(fallback,|s|(s.width,s.height));
    }
    source(id).map_or(fallback,|s|(s.width,s.height))
}
#[cfg(test)]
fn test_outputs()->&'static std::sync::Mutex<Option<&'static [Output]>> {
    static SLOT:std::sync::OnceLock<std::sync::Mutex<Option<&'static [Output]>>>=std::sync::OnceLock::new();
    SLOT.get_or_init(||std::sync::Mutex::new(None))
}
/// Native size written into CaptureSettingSync. Wayland uses the output the encoder is reading
/// (`source_id`), which can differ from the size captured when the connection opened; so does an
/// X11 monitor with UU_X11_MULTISCREEN. Otherwise X11, portal and KMS keep the connection size
/// (X11 mode changes go through `x11_resized`).
pub fn sync_native(display:&str,source_id:usize,connection:(u32,u32))->(u32,u32) {
    if display.starts_with("wayland:") || (!display.starts_with("kms:") && x11_multiscreen()) {source_size(source_id,connection)} else {connection}
}
pub fn input_ready()->bool {outputs().is_empty() || SELECTION.lock().unwrap().ready()}
pub fn switch_failed()->bool {SELECTION.lock().unwrap().failed}
pub fn presented(id:usize,generation:u64) {SELECTION.lock().unwrap().sent(id,generation);}
pub fn failed(id:usize,generation:u64) {
    let mut state=SELECTION.lock().unwrap();
    if (state.id,state.generation)==(id,generation) {state.failed=true;}
}
pub fn select(id:usize)->Result<(),String> {
    let output=source(id).ok_or("Unknown screen")?;
    if inventory().portal {
        let mut state=SELECTION.lock().unwrap();
        crate::portal::switch_active(&output.name)?;
        state.begin(id);
        log::info!("Portal screen selected: id={} source={}x{}",id,output.width,output.height);
        return Ok(());
    }
    if x11_multiscreen() {
        SELECTION.lock().unwrap().begin(id);
        log::info!("X11 screen selected: id={} source={}x{}+{}+{}",id,output.width,output.height,output.x,output.y);
        return Ok(());
    }
    #[cfg(all(target_os="linux",feature="wayland"))]
    {
        let name=std::ffi::CString::new(output.name.as_str()).map_err(|_|"Invalid output name")?;
        let mut state=SELECTION.lock().unwrap();
        if unsafe {uu_wl_select_output(name.as_ptr(),output.width,output.height)}!=0 {return Err("Wayland output switch failed".into());}
        state.begin(id);
        log::info!("Wayland screen selected: id={} source={}x{}",id,output.width,output.height);
        Ok(())
    }
    #[cfg(not(all(target_os="linux",feature="wayland")))]
    {let _=output;Err("Wayland output switching unavailable".into())}
}

/// CaptureSettingRequest.screen_id is field 4 (rpc_main.proto); official desktop
/// activation also uses SimpleAction 8 {"screen_id":id}. Reference: openuuyc main
/// a87b8e7, stream_control/wire.rs and publisher/displays.rs (Windows S456070).
/// https://github.com/djkcyl/openuuyc/tree/a87b8e772c238e542215ab039f1b1d2ca4a9ef46/src/features/stream_control
/// Official phone acceptance on Hyprland: field 4 selected each advertised id;
/// current-source settings used the signed value -1.
pub fn requested_output(data:&[u8]) -> Option<usize> {
    use crate::protocol::{decode_wire,WireValue};
    let outer=decode_wire(data).ok()?;
    fn bytes<'a>(fields:&[crate::protocol::WireField<'a>],n:u32)->Option<&'a [u8]> {fields.iter().find_map(|f|match f.value {WireValue::Bytes(b) if f.number==n=>Some(b),_=>None})}
    if let Some(rpc)=bytes(&outer,21) {
        let rpc=decode_wire(rpc).ok()?;
        let request=decode_wire(bytes(&rpc,2)?).ok()?;
        let id=request.iter().find_map(|f|match f.value {WireValue::Varint(v) if f.number==4=>Some(v as i64),_=>None}).unwrap_or(0);
        return usize::try_from(id).ok();
    }
    let action=decode_wire(bytes(&outer,3)?).ok()?;
    let kind=action.iter().find_map(|f|match f.value {WireValue::Varint(v) if f.number==1=>Some(v),_=>None})?;
    if kind!=8 {return None;}
    let args:serde_json::Value=serde_json::from_slice(bytes(&action,2)?).ok()?;
    usize::try_from(args["screen_id"].as_i64()?).ok()
}

#[cfg(test)]
mod monitor_switch_tests {
    use super::*;
    #[test]
    fn capture_setting_screen_zero_and_signed_current_target() {
        // Message.rpc_request(21), RpcRequest.capture_setting(2), screen_id(4).
        let request=|payload:&[u8]| {let mut rpc=vec![18,payload.len() as u8];rpc.extend(payload);crate::initialization::envelope(1,2,21,&rpc)};
        assert_eq!(requested_output(&request(&[32,1])),Some(1));
        assert_eq!(requested_output(&request(&[])),Some(0));
        assert_eq!(requested_output(&request(&[32,255,255,255,255,255,255,255,255,255,1])),None);
        assert_eq!(requested_output(b"not protobuf"),None);
    }
    #[test]
    fn only_explicit_desktop_restart_selects_an_output() {
        let request=|action:u8,args:&str| {let mut sa=vec![8,action,18,args.len() as u8];sa.extend(args.bytes());crate::initialization::envelope(1,2,3,&sa)};
        assert_eq!(requested_output(&request(8,r#"{"screen_id":1}"#)),Some(1));
        assert_eq!(requested_output(&request(8,r#"{"screen_id":-1}"#)),None);
        assert_eq!(requested_output(&request(7,r#"{"screen_id":1}"#)),None);
    }
}

#[cfg(test)]
mod selection_regressions {
    use super::*;
    #[test]
    fn old_frames_cannot_unlock_a_return_to_the_same_screen() {
        let mut s=Selection::default();
        s.begin(0);let old=s.generation;s.sent(0,old);assert!(s.ready());
        s.begin(1);assert!(!s.ready());s.sent(0,old);assert!(!s.ready());
        s.begin(0);let current=s.generation;
        s.sent(0,old);assert!(!s.ready());
        s.sent(1,current);assert!(!s.ready());
        s.sent(0,current);assert!(s.ready());
    }
    #[test]
    #[ignore = "requires an unlocked Wayland session and UU_WAYLAND_MULTISCREEN=1"]
    fn live_monitor_switch_captures_each_output() {
        assert!(outputs().len()>1);
        init().unwrap();
        for (id,output) in outputs().iter().enumerate() {
            select(id).unwrap();
            let mut frame=vec![0;(output.width as usize)*(output.height as usize)*4];
            capture(&mut frame).unwrap();
            assert_eq!(selected(),id);
        }
        select(0).unwrap();
    }
    #[test]
    fn niri_inventory_uses_active_mode_and_rotated_pixel_size() {
        let json=br#"{"one":{"name":"DP-1","modes":[{"width":2240,"height":1400,"refresh_rate":60000}],"current_mode":0,"logical":{"transform":"90"}},
          "off":{"name":"off","modes":[],"current_mode":null,"logical":null}}"#;
        let outputs=parse_niri_outputs(json,"").unwrap();
        assert_eq!((outputs.len(),outputs[0].width,outputs[0].height,outputs[0].fps),(1,1400,2240,60));
        assert!(parse_niri_outputs(json,"off").is_err());
        assert!(parse_niri_outputs(b"[]","").is_err());
    }
    #[test]
    fn mixed_scale_inventory_preserves_pixel_sizes_and_preferred_output() {
        let json=br#"[{"name":"eDP-1","width":1920,"height":1080,"scale":1.3333334,"transform":0,"refreshRate":144},
          {"name":"HDMI-A-1","width":2560,"height":1440,"scale":1.25,"transform":0,"refreshRate":75}]"#;
        let outputs=parse_outputs(json,"HDMI-A-1").unwrap();
        assert_eq!(outputs.iter().map(|m|(m.name.as_str(),m.width,m.height)).collect::<Vec<_>>(),
            vec![("HDMI-A-1",2560,1440),("eDP-1",1920,1080)]);
        assert!(parse_outputs(json,"missing-output").is_err());
    }
    #[test]
    fn unusable_outputs_are_not_advertised() {
        let json=br#"[{"name":"rotated","width":1920,"height":1080,"transform":1,"refreshRate":60},
          {"name":"too-large","width":16000,"height":16000,"transform":0,"refreshRate":60},
          {"name":"off","width":1920,"height":1080,"disabled":true,"transform":0,"refreshRate":60},
          {"name":"normal","width":1920,"height":1080,"transform":0,"refreshRate":60}]"#;
        let outputs=parse_outputs(json,"").unwrap();
        assert_eq!(outputs.len(),1);assert_eq!(outputs[0].name,"normal");
        assert!(parse_outputs(b"not JSON","").is_err());
    }
    #[test]
    fn x11_monitors_keep_their_root_offsets() {
        // #98: GNOME/X11, two 1920x1080 monitors side by side, primary on the right.
        let text="Monitors: 2\n 0: +*HDMI-0 1920/527x1080/296+1920+0  HDMI-0\n 1: +HDMI-1 1920/521x1080/293+0+0  HDMI-1\n";
        let outputs=parse_x11_monitors(text,"",60).unwrap();
        assert_eq!(outputs.iter().map(|m|(m.name.as_str(),m.width,m.height,m.x,m.y)).collect::<Vec<_>>(),
            vec![("HDMI-0",1920,1080,1920,0),("HDMI-1",1920,1080,0,0)]);
        let outputs=parse_x11_monitors(text,"HDMI-1",60).unwrap();
        assert_eq!((outputs[0].name.as_str(),outputs[1].name.as_str()),("HDMI-1","HDMI-0"));
        assert!(parse_x11_monitors(text,"DP-9",60).is_err());
        // xrandr --setmonitor virtual monitors (output "none"); no primary: left to right.
        let virtual_monitors="Monitors: 3\n 0: +A 1280/338x1024/270+1280+0  none\n 1: +B 1280/338x1024/270+0+0  none\n 2: C 0/0x0/0+0+0  none\n";
        let outputs=parse_x11_monitors(virtual_monitors,"",30).unwrap();
        assert_eq!(outputs.iter().map(|m|(m.name.as_str(),m.x)).collect::<Vec<_>>(),vec![("B",0),("A",1280)]);
        assert!(parse_x11_monitors("Monitors: 1\n 0: +*X 1920/1x1080/1-10+0  X\n","",60).is_err());
        assert!(parse_x11_monitors("garbage","",60).is_err());
    }
    #[test]
    fn sync_native_follows_the_wayland_source_id() {
        assert_eq!(sync_native(":0",1,(1920,1080)),(1920,1080));
        assert_eq!(sync_native("kms:/dev/dri/card0",1,(1920,1080)),(1920,1080));
        let inventory:&'static [Output]=Box::leak(vec![
            Output {name:"eDP-1".into(),width:1920,height:1080,fps:60,x:0,y:0},
            Output {name:"HDMI-A-1".into(),width:2560,height:1440,fps:75,x:0,y:0},
        ].into_boxed_slice());
        *test_outputs().lock().unwrap()=Some(inventory);
        // Connection opened on the first output; the encoder is reading the second.
        assert_eq!(sync_native("wayland:wayland-0",1,(1920,1080)),(2560,1440));
        assert_eq!(source_size(0,(800,600)),(1920,1080));
        assert_eq!(sync_native("wayland:wayland-0",9,(800,600)),(800,600));
        assert_eq!(sync_native(":0",1,(1920,1080)),(1920,1080));
        *test_outputs().lock().unwrap()=None;
    }
}
