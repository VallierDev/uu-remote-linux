//! Native Linux X11 Input Injection Engine via XTest

#[cfg(target_os = "linux")]
extern "C" {
    fn x11_mouse_move(x: libc::c_int, y: libc::c_int) -> libc::c_int;
    fn x11_mouse_relative(dx: libc::c_int, dy: libc::c_int) -> libc::c_int;
    fn x11_mouse_button(button: libc::c_int, is_press: libc::c_int) -> libc::c_int;
    fn x11_pointer_state(x:*mut libc::c_int,y:*mut libc::c_int,mask:*mut libc::c_uint)->libc::c_int;
    fn x11_key_symbol(symbol: libc::c_ulong, is_press: libc::c_int) -> libc::c_int;
    fn x11_key_event(keycode: libc::c_int, is_press: libc::c_int) -> libc::c_int;
    fn x11_release_all() -> libc::c_int;
    fn x11_terminal_shortcut_state() -> libc::c_int;
    fn x11_type_utf8(text: *const libc::c_char) -> libc::c_int;
}
#[cfg(not(target_os = "linux"))]
unsafe fn x11_terminal_shortcut_state() -> libc::c_int { 0 }
#[cfg(not(target_os = "linux"))]
unsafe fn x11_type_utf8(_: *const libc::c_char) -> libc::c_int { 0 }

#[cfg(not(target_os = "linux"))]
unsafe fn x11_mouse_move(_: libc::c_int, _: libc::c_int) -> libc::c_int { 0 }
#[cfg(not(target_os = "linux"))]
unsafe fn x11_mouse_button(_: libc::c_int, _: libc::c_int) -> libc::c_int { 0 }
#[cfg(not(target_os = "linux"))]
unsafe fn x11_key_event(_: libc::c_int, _: libc::c_int) -> libc::c_int { 0 }

#[cfg(not(target_os = "linux"))]
unsafe fn x11_key_symbol(_: libc::c_ulong, _: libc::c_int) -> libc::c_int { 0 }

#[cfg(not(target_os = "linux"))]
unsafe fn x11_mouse_relative(_: libc::c_int, _: libc::c_int) -> libc::c_int {0}

#[cfg(not(target_os="linux"))]
unsafe fn x11_release_all()->i32 {0}

#[derive(Default)]
struct PendingPointer {
    absolute: Option<(i32,i32)>,
    relative: (i64,i64),
    scroll: (i64,i64),
}
#[cfg(not(target_os = "linux"))]
unsafe fn x11_pointer_state(_: *mut libc::c_int,_:*mut libc::c_int,_:*mut libc::c_uint)->libc::c_int {-1}
#[derive(Default)]
struct HeldInput { buttons: std::collections::BTreeSet<i32>, keys: std::collections::BTreeSet<u64> }
/// Held modifier VKs (bit 0 Ctrl, 1 Shift, 2 Alt, 3 Win) and keys that got a synthetic Shift.
#[derive(Default)]
struct Shortcuts { modifiers: u8, shifted: std::collections::BTreeSet<i32> }
/// Mobile viewers in touch mode send a finger as a left-button drag at absolute positions (observed:
/// Android sends press, a stream of mouse_move_absolute, release), which on a desktop selects text
/// instead of scrolling. By default (`Gesture`) the host decides per press: moving past `SLOP` before
/// `HOLD` scrolls smoothly (content follows the finger, also in terminals), holding still selects
/// like a mouse drag, and a short press is a click. `UU_TOUCH_INPUT=touch` replays drags as one finger
/// on the portal touchscreen instead; `=0` keeps mouse buttons. `at` is the last absolute position;
/// relative motion (the viewers' trackpad mode) clears it and keeps mouse buttons.
#[derive(Default)]
struct TouchDrag { mode: TouchMode, at: Option<(i32,i32)>, down: bool, finger: Finger }
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum TouchMode { #[default] Mouse, Gesture, Touch }
/// A mobile left press in gesture mode: not decided yet, scrolling, or selecting with button 1 down.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Finger { #[default] Up, Pending { start: (i32,i32), at: std::time::Instant }, Scroll { last: (i32,i32) }, Select }
/// Pixels a finger may wander before a press becomes a scroll, and how long it holds still to select.
const SLOP: i32 = 12;
const HOLD: std::time::Duration = std::time::Duration::from_millis(450);
/// What an absolute position does to the finger: the next state and a scroll from→to or a pointer move.
#[derive(Debug, PartialEq)]
enum FingerStep { Scroll((i32,i32),(i32,i32)), Move(i32,i32), Stay }
fn finger_motion(finger: Finger, to: (i32,i32)) -> (Finger, FingerStep) {
    match finger {
        Finger::Pending { start, .. } if (to.0-start.0).abs().max((to.1-start.1).abs()) > SLOP => (Finger::Scroll { last: to }, FingerStep::Scroll(start, to)),
        Finger::Pending { .. } => (finger, FingerStep::Stay),
        Finger::Scroll { last } => (Finger::Scroll { last: to }, FingerStep::Scroll(last, to)),
        Finger::Select | Finger::Up => (finger, FingerStep::Move(to.0, to.1)),
    }
}
/// A pending press held still for `HOLD`: where the selection starts.
fn finger_held(finger: Finger, now: std::time::Instant) -> Option<(i32,i32)> {
    match finger { Finger::Pending { start, at } if now.duration_since(at) >= HOLD => Some(start), _ => None }
}
/// Viewer platform from the connect header `controller_platform` (website enum: 1 windows, 2 android,
/// 3 ios, 4 macos, 5 tv).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Controller { #[default] Other, Mac, Mobile }
impl Controller {
    pub fn from_platform(platform: Option<&serde_json::Value>) -> Self {
        let Some(v) = platform else { return Self::Other };
        let text = v.as_str().map(str::to_ascii_lowercase);
        match v.as_i64().or_else(|| text.as_deref().and_then(|s| s.parse().ok())) {
            Some(4) => Self::Mac,
            Some(2 | 3) => Self::Mobile,
            _ if text.is_some_and(|s| s.contains("mac") || s.contains("darwin")) => Self::Mac,
            _ => Self::Other,
        }
    }
}
/// Mac viewer with 「使用 macOS 快捷键」 off sends Cmd as VK_LWIN (verified: Cmd 0x5B, Option 0xA4,
/// Ctrl 0xA2). Cmd is held back until we know whether it starts a Mac shortcut.
#[derive(Default)]
struct CmdState { held: bool, pending: bool, passthrough: bool, combos: std::collections::BTreeMap<i32, bool> }

pub struct InputInjector { mac_controller: std::sync::atomic::AtomicBool, cmd: std::sync::Mutex<CmdState>, serial:std::sync::Mutex<()>, owner:std::sync::Mutex<DesktopOwner>, shortcuts: std::sync::Mutex<Shortcuts>, uinput: Option<crate::uinput::Devices>, screen: std::sync::Mutex<(u32,u32)>, portal_failures: std::sync::atomic::AtomicUsize, portal_outcomes: [std::sync::atomic::AtomicUsize; 3], portal:std::sync::Mutex<Option<std::sync::Arc<crate::portal::PortalSession>>>, backend: crate::desktop_backend::DesktopBackend, pending: std::sync::Mutex<PendingPointer>, held: std::sync::Mutex<HeldInput>, touch: std::sync::Mutex<TouchDrag> }

impl InputInjector {
    pub fn new() -> Self {
        let backend = crate::desktop_backend::detect().unwrap_or(crate::desktop_backend::DesktopBackend::X11);
        let uinput = if crate::uinput::wanted(backend) {
            match crate::uinput::Devices::open() {
                Ok(devices) => { log::info!("Input backend: uinput (kernel virtual keyboard/pointer)"); Some(devices) }
                Err(e) => { log::warn!("uinput unavailable ({}); using the desktop input path. See README for the udev rule.", e.kind()); None }
            }
        } else { None };
        Self { mac_controller: std::sync::atomic::AtomicBool::new(false), cmd: std::sync::Mutex::new(CmdState::default()), serial:std::sync::Mutex::new(()), owner:std::sync::Mutex::new(DesktopOwner::default()), shortcuts: std::sync::Mutex::new(Shortcuts::default()), uinput, screen: std::sync::Mutex::new((0,0)), portal_failures: std::sync::atomic::AtomicUsize::new(0), portal_outcomes: std::array::from_fn(|_| std::sync::atomic::AtomicUsize::new(0)), portal:std::sync::Mutex::new(None), backend:crate::desktop_backend::detect().unwrap_or(crate::desktop_backend::DesktopBackend::X11), pending: std::sync::Mutex::new(PendingPointer::default()), held:std::sync::Mutex::new(HeldInput::default()), touch:std::sync::Mutex::new(TouchDrag::default()) }
    }

    pub fn bind_portal(&self,session:Option<std::sync::Arc<crate::portal::PortalSession>>) {
        log::info!("Desktop portal input binding updated: bound={}", session.is_some());
        *self.portal.lock().unwrap()=session;
    }
    pub fn mouse_move(&self, x: i32, y: i32) {
        let _serial=self.serial.lock().unwrap();
        self.inject_move(x,y,false);
    }

    pub fn mouse_absolute_normalized(&self, x: f64, y: f64, size: (u32,u32)) {
        *self.screen.lock().unwrap() = size;
        if let Some(position) = crate::input_mapping::normalized_pointer(x,y,size.0,size.1) {
            let mut pending = self.pending.lock().unwrap();
            pending.absolute = Some(position);
            pending.relative = (0,0);
        }
    }
    pub fn mouse_relative(&self, dx: i32, dy: i32) {
        if dx.unsigned_abs() <= 65535 && dy.unsigned_abs() <= 65535 {
            let mut pending = self.pending.lock().unwrap();
            pending.relative.0 = (pending.relative.0 + i64::from(dx)).clamp(-65535,65535);
            pending.relative.1 = (pending.relative.1 + i64::from(dy)).clamp(-65535,65535);
        }
    }
    pub fn mouse_scroll(&self, dx: i32, dy: i32) {
        // Official Mac normal wheel=120; precise trackpads use smaller values.
        let mut pending = self.pending.lock().unwrap();
        pending.scroll.0 = (pending.scroll.0 + i64::from(dx)).clamp(-12000,12000);
        pending.scroll.1 = (pending.scroll.1 + i64::from(dy)).clamp(-12000,12000);
    }
    pub fn start_pointer_worker(self: &std::sync::Arc<Self>) -> tokio::task::JoinHandle<()> {
        let input = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(4));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop { tick.tick().await; input.flush_pointer(); }
        })
    }
    pub fn discard_pointer(&self) { *self.pending.lock().unwrap() = PendingPointer::default(); }
    pub fn flush_pointer(&self) {
        let _serial=self.serial.lock().unwrap();
        self.flush_pointer_locked();
    }
    fn flush_pointer_locked(&self) {
        // Hold the queue lock through injection so an older timer tick cannot
        // overwrite a newer position flushed before a button transition.
        let mut pending = self.pending.lock().unwrap();
        let motion = std::mem::take(&mut *pending);
        pending.scroll = (motion.scroll.0 % 120, motion.scroll.1 % 120);
        let mut touch = self.touch.lock().unwrap();
        if let Some((x,y)) = motion.absolute {
            if touch.down { self.inject_touch(1,x,y); } else {
                let (finger, step) = finger_motion(touch.finger, (x,y));
                touch.finger = finger;
                match step {
                    FingerStep::Scroll(from,to) => { self.inject_scroll(from,to,false); },
                    FingerStep::Move(x,y) => { self.inject_move(x,y,false); },
                    FingerStep::Stay => {},
                }
            }
            touch.at = Some((x,y));
        }
        // Held still: select from where the finger went down, like a mouse drag.
        if let Some((x,y)) = finger_held(touch.finger, std::time::Instant::now()) {
            self.inject_move(x,y,false);
            touch.finger = if self.inject_button(1,true) == 0 { Finger::Select } else { Finger::Up };
        }
        drop(touch);
        if motion.relative != (0,0) {
            self.touch.lock().unwrap().at = None;
            self.inject_move(motion.relative.0 as i32,motion.relative.1 as i32,true);
        }
        for (value, positive, negative) in [(motion.scroll.1,4,5),(motion.scroll.0,7,6)] {
            let count = value / 120;
            let button = if count > 0 { positive } else { negative };
            for _ in 0..count.unsigned_abs().min(100) {
                self.inject_button(button,true); self.inject_button(button,false);
            }
        }
    }
    pub fn wire_mouse_button(&self, button: i32, is_press: bool) {
        if let Some(mapped) = crate::input_mapping::mouse_button_to_x11(button) { self.mouse_button(mapped,is_press); }
        else {
            static UNKNOWN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            if UNKNOWN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 16 {
                log::warn!("Unsupported desktop wire mouse button: {}", button);
            }
        }
    }
    pub fn mouse_button(&self, button: i32, is_press: bool) {
        let _serial=self.serial.lock().unwrap();
        self.flush_pointer_locked();
        if button == 1 && self.touch_button(is_press) { return; }
        let result = self.inject_button(button,is_press);
        if result == 0 {
            let mut held = self.held.lock().unwrap();
            if is_press { held.buttons.insert(button); } else { held.buttons.remove(&button); }
        } else if self.backend != crate::desktop_backend::DesktopBackend::Portal { log::warn!("Desktop mouse injection failed: {}",result); }
        static BUTTON_AUDIT:std::sync::atomic::AtomicUsize=std::sync::atomic::AtomicUsize::new(0);
        if self.backend==crate::desktop_backend::DesktopBackend::X11 && BUTTON_AUDIT.fetch_add(1,std::sync::atomic::Ordering::Relaxed)<24 {
            let (mut x,mut y,mut mask)=(0,0,0);
            let query=unsafe {x11_pointer_state(&mut x,&mut y,&mut mask)};
            log::info!("Pointer button audit: button={} down={} result={} query={} x={} y={} mask={}",button,is_press,result,query,x,y,mask);
        }
    }

    /// Mac viewers send Cmd+V as Ctrl+V (verified: Cmd and Ctrl both arrive as VK_LCONTROL), which in
    /// a Linux terminal quotes the next key instead of pasting. In a focused terminal Ctrl+V becomes
    /// Ctrl+Shift+V. Ctrl+C is never translated: it must always interrupt, and copy is Cmd+Shift+C
    /// (= Ctrl+Shift+C). X11 only; `UU_TERMINAL_SHORTCUTS=0` disables.
    fn terminal_shift(&self, key: i32, is_press: bool) -> bool {
        let bit = match key { 0x11 | 0xa2 | 0xa3 => 1, 0x10 | 0xa0 | 0xa1 => 2, 0x12 | 0xa4 | 0xa5 => 4, 0x5b | 0x5c => 8, _ => 0 };
        let mut shortcuts = self.shortcuts.lock().unwrap();
        if bit != 0 {
            if is_press { shortcuts.modifiers |= bit } else { shortcuts.modifiers &= !bit }
            return false;
        }
        if !is_press { return shortcuts.shifted.remove(&key); }
        if key != 0x56 || shortcuts.modifiers != 1 { return false; }
        if std::env::var("UU_TERMINAL_SHORTCUTS").is_ok_and(|v| v == "0") || std::env::var_os("DISPLAY").is_none() { return false; }
        let shift = unsafe { x11_terminal_shortcut_state() } & 1 != 0;
        if shift {
            shortcuts.shifted.insert(key);
            static NOTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            if NOTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 4 {
                log::info!("Terminal shortcut: Ctrl+V sent as Ctrl+Shift+V");
            }
        }
        shift
    }

    /// The current desktop controller's platform (connect header controller_platform).
    /// Mobile drags follow `UU_TOUCH_INPUT`: unset = gesture, `touch` = portal touchscreen, `0` = mouse.
    pub fn set_controller(&self, controller: Controller) {
        self.mac_controller.store(controller == Controller::Mac, std::sync::atomic::Ordering::Relaxed);
        *self.cmd.lock().unwrap() = CmdState::default();
        let mut touch = self.touch.lock().unwrap();
        self.lift_finger(&mut touch);
        let mode = if controller != Controller::Mobile { TouchMode::Mouse } else {
            match std::env::var("UU_TOUCH_INPUT").as_deref() { Ok("0") => TouchMode::Mouse, Ok("touch") => TouchMode::Touch, _ => TouchMode::Gesture }
        };
        *touch = TouchDrag { mode, ..TouchDrag::default() };
    }
    /// Ends whatever a finger holds: the touch point, a selection's button, a scroll gesture.
    fn lift_finger(&self, touch: &mut TouchDrag) {
        if std::mem::take(&mut touch.down) { self.inject_touch(2,0,0); }
        match std::mem::take(&mut touch.finger) {
            Finger::Select => { self.inject_button(1,false); },
            Finger::Scroll { last } => { self.inject_scroll(last,last,true); },
            Finger::Pending { .. } | Finger::Up => {},
        }
    }
    /// Left button of a mobile controller: a finger down at the last absolute position, or up.
    /// False leaves the button to the mouse path (no touchscreen, trackpad mode, other viewers).
    fn touch_button(&self, pressed: bool) -> bool {
        let mut touch = self.touch.lock().unwrap();
        if touch.mode == TouchMode::Gesture { return self.gesture_button(&mut touch, pressed) }
        if !pressed {
            if !touch.down { return false; }
            touch.down = false;
            self.inject_touch(2,0,0);
            return true;
        }
        if touch.down { return true; }
        let Some((x,y)) = touch.at.filter(|_| touch.mode == TouchMode::Touch) else { return false };
        let supported = self.backend == crate::desktop_backend::DesktopBackend::Portal
            && self.portal.lock().unwrap().as_ref().is_some_and(|portal| portal.touch_supported());
        static REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !REPORTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            log::info!("Mobile controller drag input: touchscreen={supported}");
        }
        if !supported { self.offer_touch_consent(); }
        touch.down = supported && self.inject_touch(0,x,y) == 0;
        touch.down
    }
    /// Gesture mode (portal only, for its smooth scroll): a press waits for `finger_motion`/`finger_held`
    /// to decide; releasing an undecided press is a click.
    fn gesture_button(&self, touch: &mut TouchDrag, pressed: bool) -> bool {
        if !pressed {
            return match std::mem::take(&mut touch.finger) {
                Finger::Up => false,
                Finger::Pending { start, .. } => {
                    self.inject_move(start.0,start.1,false);
                    if self.inject_button(1,true) == 0 { self.inject_button(1,false); }
                    true
                },
                finger => { touch.finger = finger; self.lift_finger(touch); true },
            };
        }
        if touch.finger != Finger::Up { return true }
        let bound = self.backend == crate::desktop_backend::DesktopBackend::Portal && self.portal.lock().unwrap().is_some();
        let Some(start) = touch.at.filter(|_| bound) else { return false };
        static REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !REPORTED.swap(true, std::sync::atomic::Ordering::Relaxed) { log::info!("Mobile controller drag input: gesture (drag scrolls, hold selects)"); }
        touch.finger = Finger::Pending { start, at: std::time::Instant::now() };
        true
    }
    /// Content follows the finger: moving it up scrolls down.
    fn inject_scroll(&self, from: (i32,i32), to: (i32,i32), finish: bool) -> i32 {
        let Some(portal) = self.portal.lock().unwrap().clone() else { return -1 };
        self.portal_result("scroll", portal.scroll(f64::from(from.0 - to.0), f64::from(from.1 - to.1), finish))
    }
    /// The portal offers a touchscreen the restored consent lacks: ask the host once per run, in the
    /// background (GNOME's share dialog, visible to the phone too); drags stay mouse drags meanwhile.
    fn offer_touch_consent(&self) {
        static ASKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let Some(portal) = self.portal.lock().unwrap().clone() else { return };
        if ASKED.swap(true, std::sync::atomic::Ordering::Relaxed) { return }
        std::thread::spawn(move || match portal.upgrade_touch() {
            Ok(true) => log::info!("Touch consent upgraded: mobile drags are touch from now on"),
            Ok(false) => log::info!("Touch consent upgrade not needed or not offered by the portal"),
            Err(reason) => log::info!("Touch consent upgrade not granted: {reason}"),
        });
    }
    fn inject_touch(&self, phase: i32, x: i32, y: i32) -> i32 {
        let Some(portal) = self.portal.lock().unwrap().clone() else { return -1 };
        self.portal_result("touch", portal.touch(phase, f64::from(x), f64::from(y)))
    }

    fn terminal_focused(&self) -> bool {
        self.backend == crate::desktop_backend::DesktopBackend::X11 && std::env::var_os("DISPLAY").is_some()
            && !std::env::var("UU_TERMINAL_SHORTCUTS").is_ok_and(|v| v == "0")
            && unsafe { x11_terminal_shortcut_state() } & 1 != 0
    }

    /// Mac habits for a Mac viewer with 「使用 macOS 快捷键」 off: Cmd+C/V/A/F/X/Z (the keys that option
    /// lists) become Ctrl+key, and in a focused terminal Cmd+C/V become Ctrl+Shift+C/V (copy/paste)
    /// while Ctrl+C still interrupts. Other Cmd combos, and Cmd alone, stay Super. Returns true when handled.
    fn mac_cmd(&self, key: i32, is_press: bool) -> bool {
        const SUPER_L: u64 = 0xffeb; const CONTROL_L: u64 = 0xffe3; const SHIFT_L: u64 = 0xffe1;
        let mut st = self.cmd.lock().unwrap();
        if matches!(key, 0x5b | 0x5c) {
            if is_press { st.held = true; st.pending = true; st.passthrough = false; }
            else {
                if st.passthrough { self.inject_key(SUPER_L, false); }
                else if st.pending { self.inject_key(SUPER_L, true); self.inject_key(SUPER_L, false); }
                st.held = false; st.pending = false; st.passthrough = false;
            }
            return true;
        }
        if !is_press {
            let Some(shifted) = st.combos.remove(&key) else { return false };
            self.key_symbol_event(key, false);
            if shifted { self.inject_key(SHIFT_L, false); }
            self.inject_key(CONTROL_L, false);
            return true;
        }
        if !st.held { return false; }
        // Shift/Option/Ctrl pressed with Cmd (e.g. Cmd+Shift+Z) must not decide the combo yet.
        if matches!(key, 0x10..=0x12 | 0xa0..=0xa5) { return false; }
        if st.combos.contains_key(&key) { self.key_symbol_event(key, true); return true; } // autorepeat
        if matches!(key, 0x41 | 0x43 | 0x46 | 0x56 | 0x58 | 0x5a) && !st.passthrough {
            st.pending = false;
            let shifted = matches!(key, 0x43 | 0x56) && self.terminal_focused();
            self.inject_key(CONTROL_L, true);
            if shifted { self.inject_key(SHIFT_L, true); }
            self.key_symbol_event(key, true);
            st.combos.insert(key, shifted);
            static NOTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            if NOTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 4 { log::info!("Mac shortcut: Cmd combo sent as Ctrl{}", if shifted { "+Shift (terminal)" } else { "" }); }
            return true;
        }
        if st.pending { self.inject_key(SUPER_L, true); st.pending = false; st.passthrough = true; }
        false
    }

    pub fn windows_key_event(&self, key: i32, is_press: bool) {
        let _serial=self.serial.lock().unwrap();
        if self.mac_controller.load(std::sync::atomic::Ordering::Relaxed) && self.mac_cmd(key, is_press) { return; }
        if self.terminal_shift(key, is_press) {
            const SHIFT_L: u64 = 0xffe1;
            if is_press {
                self.inject_key(SHIFT_L, true);
                self.key_symbol_event(key, true);
            } else {
                self.key_symbol_event(key, false);
                self.inject_key(SHIFT_L, false);
            }
            return;
        }
        self.key_symbol_event(key, is_press);
    }

    fn key_symbol_event(&self, key: i32, is_press: bool) {
        if let Some(symbol) = crate::input_mapping::windows_vk_to_keysym(key) {
            let result = self.inject_key(symbol,is_press);
            static AUDIT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            if AUDIT.fetch_add(1,std::sync::atomic::Ordering::Relaxed) < 8 {
                log::info!("Desktop keyboard injection outcome: result={result}");
            }
            // KP_0..KP_9 type digits only with NumLock on (#46); record the server's NumLock state, not the key.
            static NUMPAD: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            if (0x60..=0x6f).contains(&key) && is_press && self.backend==crate::desktop_backend::DesktopBackend::X11
                && NUMPAD.fetch_add(1,std::sync::atomic::Ordering::Relaxed) < 4 {
                let (mut x,mut y,mut mask)=(0,0,0u32);
                let state=unsafe { x11_pointer_state(&mut x,&mut y,&mut mask) };
                log::info!("Desktop numpad key: result={result} numlock={}", state==0 && mask & 0x10 != 0);
            }
            if result == 0 {
                let mut held=self.held.lock().unwrap();
                if is_press {held.keys.insert(symbol);} else {held.keys.remove(&symbol);}
            }
        }
    }

    pub fn release_held(&self) {
        let _serial=self.serial.lock().unwrap();
        self.discard_pointer();
        { let mut touch=self.touch.lock().unwrap(); self.lift_finger(&mut touch); }
        let mut held=self.held.lock().unwrap();
        if let Some(uinput)=&self.uinput {
            Self::uinput_result(uinput.release_all());
        } else if self.backend==crate::desktop_backend::DesktopBackend::X11 {
            unsafe {x11_release_all();}
        } else if self.backend==crate::desktop_backend::DesktopBackend::Wayland && self.portal.lock().unwrap().is_none() {
            crate::wayland::release_all();
        } else if self.backend==crate::desktop_backend::DesktopBackend::Portal && self.portal.lock().unwrap().is_none() {
            // Nothing is bound yet (e.g. the first locked connection): no input can be held.
        } else {
            // Portal/Wayland cannot query global state. Sweep every key/button
            // that this host can emit through the currently bound session.
            let keys:std::collections::BTreeSet<_>=(0..=255).filter_map(crate::input_mapping::windows_vk_to_keysym)
                .chain(held.keys.iter().copied()).collect();
            // A dead Portal bus can time out per call. Stop on the first
            // failure rather than blocking teardown for hundreds of calls;
            // the next connected session retries the complete sweep.
            let buttons_ok=[1,2,3,8,9].into_iter().all(|button|self.inject_button(button,false)==0);
            let keys_ok=buttons_ok && keys.into_iter().all(|key|self.inject_key(key,false)==0);
            if !keys_ok {log::warn!("Desktop input reset incomplete: backend unavailable");}
        }
        *held=HeldInput::default();
        *self.shortcuts.lock().unwrap()=Shortcuts::default();
        *self.cmd.lock().unwrap()=CmdState::default();
    }
    // Per-injector budgets cover missing bindings, locked sessions, stale handles
    // and D-Bus errors. Never include keys, coordinates, text or portal tokens.
    fn portal_result(&self, operation: &str, result: Result<(), String>) -> i32 {
        use std::sync::atomic::Ordering::Relaxed;
        let slot = match operation { "motion" => 0, "button" => 1, _ => 2 };
        if self.portal_outcomes[slot].fetch_add(1, Relaxed) < 16 {
            log::info!("Portal input outcome: operation={} success={}", operation, result.is_ok());
        }
        match result {
            Ok(()) => 0,
            Err(reason) => {
                let index = self.portal_failures.fetch_add(1, Relaxed);
                if index < 16 {
                    log::warn!("Portal input injection failed: operation={} reason={} failure={}/16{}",
                        operation, reason, index + 1, if index == 15 { " (further failures suppressed)" } else { "" });
                }
                -1
            }
        }
    }
    fn uinput_result(result: std::io::Result<()>) -> i32 {
        match result {
            Ok(()) => 0,
            Err(e) => {
                static FAILURES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
                if FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 { log::warn!("uinput injection failed: {}", e.kind()); }
                -1
            }
        }
    }
    fn inject_move(&self,x:i32,y:i32,relative:bool)->i32 {
        // uinput ABS axes span the compositor's whole desktop. A captured output
        // uses its own pixel space; bind absolute motion to that Wayland output,
        // including when uinput remains in use for keyboard/buttons/relative input.
        // Only a compositor without wlr-virtual-pointer (-2) falls through to the uinput
        // absolute pointer; an out-of-range point must not land on another output.
        if !relative && self.backend==crate::desktop_backend::DesktopBackend::Wayland {
            let result=crate::wayland::move_pointer(x,y,false);
            if result!=-2 { return result; }
        }
        // Portal (GNOME/KDE): absolute motion is addressed to the shared stream, i.e. the captured
        // monitor; a uinput tablet would stretch the point across every monitor of the desktop.
        if !relative && self.backend==crate::desktop_backend::DesktopBackend::Portal {
            if let Some(portal)=self.portal.lock().unwrap().as_ref() { return self.portal_result("motion", portal.move_pointer(f64::from(x),f64::from(y),false)); }
        }
        // X11 multi-screen: the point is inside the selected monitor; XTest and uinput span the root.
        let placement=if relative || self.backend!=crate::desktop_backend::DesktopBackend::X11 {None} else {crate::wayland::x11_placement()};
        let (x,y)=placement.map_or((x,y),|(ox,oy)|(x+ox,y+oy));
        if let Some(uinput)=&self.uinput {
            if relative { return Self::uinput_result(uinput.move_relative(x,y)); }
            let (w,h)=placement.and_then(|_|crate::capture::x11_last_size()).unwrap_or(*self.screen.lock().unwrap());
            if w > 1 && h > 1 {
                let scale = |v: i32, extent: u32| (i64::from(v) * i64::from(crate::uinput::ABS_MAX) / i64::from(extent - 1)) as i32;
                return Self::uinput_result(uinput.move_absolute(scale(x,w),scale(y,h)));
            }
        }
        if self.backend==crate::desktop_backend::DesktopBackend::Kms { return -1; }
        if let Some(portal)=self.portal.lock().unwrap().as_ref() { return self.portal_result("motion", portal.move_pointer(f64::from(x),f64::from(y),relative)); }
        if self.backend==crate::desktop_backend::DesktopBackend::Portal {return self.portal_result("motion", Err("Portal input session is not bound".into()));}
        if self.backend==crate::desktop_backend::DesktopBackend::Wayland { crate::wayland::move_pointer(x,y,relative) }
        else if relative { unsafe { x11_mouse_relative(x,y) } } else { unsafe { x11_mouse_move(x,y) } }
    }
    fn inject_button(&self,button:i32,pressed:bool)->i32 {
        // Portal: clicks follow absolute motion on the same session, so a tap cannot land
        // before the compositor has moved the pointer there (uinput is a separate queue).
        if self.backend==crate::desktop_backend::DesktopBackend::Portal {
            if let Some(portal)=self.portal.lock().unwrap().as_ref() { return self.portal_result("button", portal.button(button,pressed)); }
        }
        if let Some(uinput)=&self.uinput {
            if let Some((v,h))=crate::uinput::wheel_step(button) { return if pressed { Self::uinput_result(uinput.wheel(v,h)) } else { 0 }; }
            if let Some(code)=crate::uinput::button_code(button) { return Self::uinput_result(uinput.button(code,pressed)); }
        }
        if self.backend==crate::desktop_backend::DesktopBackend::Kms { return -1; }
        if let Some(portal)=self.portal.lock().unwrap().as_ref() { return self.portal_result("button", portal.button(button,pressed)); }
        if self.backend==crate::desktop_backend::DesktopBackend::Portal {return self.portal_result("button", Err("Portal input session is not bound".into()));}
        if self.backend==crate::desktop_backend::DesktopBackend::Wayland { crate::wayland::button(button,pressed) }
        else { unsafe { x11_mouse_button(button,i32::from(pressed)) } }
    }
    fn inject_key(&self,symbol:u64,pressed:bool)->i32 {
        if let Some(uinput)=&self.uinput {
            if let Some(code)=crate::uinput::keysym_to_evdev(symbol) { return Self::uinput_result(uinput.key(code,pressed)); }
        }
        if self.backend==crate::desktop_backend::DesktopBackend::Kms { return -1; }
        if let Some(portal)=self.portal.lock().unwrap().as_ref() { return self.portal_result("keyboard", portal.key(symbol,pressed)); }
        if self.backend==crate::desktop_backend::DesktopBackend::Portal {return self.portal_result("keyboard", Err("Portal input session is not bound".into()));}
        if self.backend==crate::desktop_backend::DesktopBackend::Wayland { crate::wayland::key(symbol,pressed) }
        else { unsafe { x11_key_symbol(symbol as libc::c_ulong,i32::from(pressed)) } }
    }
    /// Set clipboard content for text injection on Wayland / Portal.
    /// Tries Portal RemoteDesktop.SetSelection first, then wl-copy as fallback.
    fn set_clipboard_text(&self, text: &str) -> bool {
        if text.is_empty() || text.len() > crate::clipboard::MAX_TEXT {
            return false;
        }
        // In unit tests, never invoke external clipboard binaries (wl-copy)
        // or alter the system clipboard to prevent side effects on developer desktops.
        #[cfg(test)]
        {
            let _ = text;
            false
        }
        #[cfg(not(test))]
        {
            // 1. Try Portal clipboard if session is bound and active
            if let Some(portal) = self.portal.lock().unwrap().as_ref() {
                if portal.clipboard_set(text).is_ok() {
                    return true;
                }
            }
            // 2. Try wl-copy in Wayland session
            if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                if let Ok(mut child) = std::process::Command::new("wl-copy")
                    .args(["--type", "text/plain;charset=utf-8"])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                {
                    if let Some(mut stdin) = child.stdin.take() {
                        use std::io::Write;
                        let _ = stdin.write_all(text.as_bytes());
                    }
                    if let Ok(status) = child.wait() {
                        if status.success() {
                            return true;
                        }
                    }
                }
            }
            false
        }
    }

    /// Injects text by copying it to the clipboard and simulating a paste shortcut.
    /// Uses Shift+Insert by default (broadest Linux compatibility across GUI apps and terminals),
    /// or Ctrl+V if configured via UU_WAYLAND_PASTE_KEY=ctrl_v.
    fn paste_text_via_clipboard(&self, text: &str) -> bool {
        if !self.set_clipboard_text(text) {
            return false;
        }

        // Give the clipboard daemon / compositor a brief moment to register the selection.
        std::thread::sleep(std::time::Duration::from_millis(25));

        let use_ctrl_v = std::env::var("UU_WAYLAND_PASTE_KEY")
            .is_ok_and(|v| v.eq_ignore_ascii_case("ctrl_v") || v.eq_ignore_ascii_case("ctrl+v"));

        if use_ctrl_v {
            // Control_L (0xffe3) + 'v' (0x76)
            self.inject_key(0xffe3, true);
            std::thread::sleep(std::time::Duration::from_millis(15));
            self.inject_key(0x76, true);
            std::thread::sleep(std::time::Duration::from_millis(20));
            self.inject_key(0x76, false);
            std::thread::sleep(std::time::Duration::from_millis(15));
            self.inject_key(0xffe3, false);
        } else {
            // Shift_L (0xffe1) + Insert (0xff63)
            self.inject_key(0xffe1, true);
            std::thread::sleep(std::time::Duration::from_millis(15));
            self.inject_key(0xff63, true);
            std::thread::sleep(std::time::Duration::from_millis(20));
            self.inject_key(0xff63, false);
            std::thread::sleep(std::time::Duration::from_millis(15));
            self.inject_key(0xffe1, false);
        }

        // Brief delay allowing the target application to consume the paste event
        std::thread::sleep(std::time::Duration::from_millis(25));
        true
    }

    /// Mobile input-method text (`text_input`). X11 types it directly; Wayland re-maps a spare
    /// keycode in the virtual keyboard's keymap. Portal and other Wayland environments inject
    /// via clipboard paste (Shift+Insert). Set UU_WAYLAND_TEXT=0 to disable Wayland text injection.
    pub fn text_input(&self, text: &str) {
        let _serial=self.serial.lock().unwrap();
        let Ok(c_text)=std::ffi::CString::new(text.replace('\0',"")) else { return };
        if self.backend==crate::desktop_backend::DesktopBackend::X11 {
            let typed=unsafe { x11_type_utf8(c_text.as_ptr()) };
            log::info!("Desktop text input: chars={} result={}", text.chars().count(), typed);
        } else if self.backend==crate::desktop_backend::DesktopBackend::Wayland {
            // Escape hatch: disable Wayland text typing/pasting entirely without rebuilding.
            if std::env::var("UU_WAYLAND_TEXT").is_ok_and(|v|v=="0") { return; }
            let typed=crate::wayland::type_utf8(text);
            if typed>=0 {
                log::info!("Desktop text input: chars={} result={}", text.chars().count(), typed);
                return;
            }
            // Fallback to clipboard paste if keymap has no spare keycodes or native typing failed
            if self.paste_text_via_clipboard(text) {
                log::info!("Desktop text input (Wayland clipboard paste fallback): chars={} result=ok", text.chars().count());
                return;
            }
            static WARNED:std::sync::atomic::AtomicBool=std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true,std::sync::atomic::Ordering::Relaxed) {
                log::warn!("Mobile text input unavailable on the Wayland backend (native and clipboard fallback failed)");
            }
        } else if self.backend==crate::desktop_backend::DesktopBackend::Portal {
            if std::env::var("UU_WAYLAND_TEXT").is_ok_and(|v|v=="0") { return; }
            // Shift+Insert pastes the PRIMARY selection in VTE terminals, not the clipboard just set:
            // type what the active layout has as keys and paste only the rest.
            let typed=self.type_portal_keysyms(text);
            if typed==text.chars().count() {
                log::info!("Desktop text input (Portal keysyms): chars={typed}");
                return;
            }
            let rest:String=text.chars().skip(typed).collect();
            if self.paste_text_via_clipboard(&rest) {
                log::info!("Desktop text input (Portal clipboard paste): chars={} typed={typed} result=ok", text.chars().count());
                return;
            }
            static WARNED:std::sync::atomic::AtomicBool=std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true,std::sync::atomic::Ordering::Relaxed) {
                log::warn!("Portal clipboard text paste failed: ensure clipboard permission or wl-copy is available");
            }
        } else {
            static WARNED:std::sync::atomic::AtomicBool=std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true,std::sync::atomic::Ordering::Relaxed) {
                log::warn!("Mobile text input is not supported on the {:?} backend yet", self.backend);
            }
        }
    }
    /// Types `text` as portal keysyms when it is all printable ASCII (or newline/tab) and GNOME's
    /// active input source is a Latin xkb layout; returns how many characters were typed.
    fn type_portal_keysyms(&self, text: &str) -> usize {
        let Some(symbols)=ascii_keysyms(text) else { return 0 };
        if !gnome_layout_types_ascii() { return 0; }
        let Some(portal)=self.portal.lock().unwrap().clone() else { return 0 };
        symbols.iter().take_while(|&&symbol| portal.type_keysym(symbol).is_ok()).count()
    }
    pub fn key_event(&self, keycode: i32, is_press: bool) {
        let _serial=self.serial.lock().unwrap();
        if self.backend!=crate::desktop_backend::DesktopBackend::X11 { return; }
        unsafe { x11_key_event(keycode, if is_press { 1 } else { 0 }) };
    }
}

#[cfg(test)]
mod motion_tests {
    use super::*;
    #[test]
    fn coalesces_absolute_history_to_latest_position() {
        let input = InputInjector::new();
        input.mouse_absolute_normalized(0.1,0.1,(1920,1080));
        input.mouse_absolute_normalized(0.9,0.9,(1920,1080));
        assert_eq!(input.pending.lock().unwrap().absolute,Some((1727,971)));
    }
    #[test]
    fn preserves_relative_distance_after_latest_absolute() {
        let input = InputInjector::new();
        input.mouse_relative(10,20);
        input.mouse_absolute_normalized(0.5,0.5,(1920,1080));
        input.mouse_relative(3,-2); input.mouse_relative(4,5);
        let state = input.pending.lock().unwrap();
        assert_eq!(state.absolute,Some((960,540)));
        assert_eq!(state.relative,(7,3));
    }
    #[test]
    fn button_transition_flushes_motion_and_teardown_discards_pending() {
        let input = InputInjector::new();
        input.mouse_absolute_normalized(0.5,0.5,(1920,1080));
        input.mouse_button(1,true);
        assert!(input.pending.lock().unwrap().absolute.is_none());
        input.mouse_relative(1,2); input.discard_pointer();
        assert_eq!(input.pending.lock().unwrap().relative,(0,0));
    }
}

#[cfg(test)]
mod shortcut_tests {
    use super::*;
    #[test]
    fn modifiers_are_tracked_and_non_terminal_keys_pass_through() {
        let input = InputInjector::new();
        assert!(!input.terminal_shift(0x11, true));
        assert_eq!(input.shortcuts.lock().unwrap().modifiers, 1);
        // No X display in tests: Ctrl+V is not translated.
        assert!(!input.terminal_shift(0x56, true));
        assert!(!input.terminal_shift(0x56, false));
        assert!(!input.terminal_shift(0x11, false));
        assert_eq!(input.shortcuts.lock().unwrap().modifiers, 0);
        // A translated key keeps its synthetic Shift until its own release.
        input.shortcuts.lock().unwrap().shifted.insert(0x56);
        assert!(input.terminal_shift(0x56, false));
        assert!(!input.terminal_shift(0x56, false));
        // Ctrl+C is never translated.
        input.terminal_shift(0x11, true);
        assert!(!input.terminal_shift(0x43, true));
    }
}

#[cfg(test)]
mod scroll_tests {
    use super::*;
    #[test]
    fn precise_scroll_accumulates_sub_notch_remainder() {
        let input = InputInjector::new();
        input.mouse_scroll(0, 2); input.flush_pointer();
        assert_eq!(input.pending.lock().unwrap().scroll,(0,2));
        input.mouse_scroll(0,118); input.flush_pointer();
        assert_eq!(input.pending.lock().unwrap().scroll,(0,0));
        input.mouse_scroll(-125,-125); input.flush_pointer();
        assert_eq!(input.pending.lock().unwrap().scroll,(-5,-5));
    }
}

/// Only the newest desktop lease may inject or release input. Tool peers never
/// obtain a lease; delayed callbacks from an old desktop cannot clear a new one.
#[derive(Default)]
struct DesktopOwner { next:u64, current:u64, active:bool, stopped:bool }
impl DesktopOwner {
    fn replace(&mut self)->u64 {self.next+=1;self.current=self.next;self.active=false;self.current}
    fn owns(&self,id:u64)->bool {id!=0 && self.current==id}
}
pub struct DesktopInputSession {input:std::sync::Arc<InputInjector>,id:u64}
impl InputInjector {
    pub fn shutdown(&self) {
        let mut owner=self.owner.lock().unwrap();
        owner.current=0;owner.active=false;owner.stopped=true;
        self.release_held();
    }
    pub fn desktop_session(self:&std::sync::Arc<Self>)->std::sync::Arc<DesktopInputSession> {
        let mut owner=self.owner.lock().unwrap();
        let id=if owner.stopped {0} else {self.release_held();owner.replace()};
        std::sync::Arc::new(DesktopInputSession {input:self.clone(),id})
    }
}
impl DesktopInputSession {
    pub fn is_current(&self) -> bool {
        let owner = self.input.owner.lock().unwrap();
        owner.owns(self.id)
    }
    /// Call only inside with_current or with_input, so a superseded peer cannot change bindings.
    pub fn bind_portal(&self, portal: Option<std::sync::Arc<crate::portal::PortalSession>>) {
        self.input.bind_portal(portal);
    }
    pub fn release_input(&self) { self.input.release_held(); }
    pub fn connected(&self) {
        let mut owner=self.input.owner.lock().unwrap();
        if owner.owns(self.id) && !owner.active {self.input.release_held();owner.active=true;}
    }
    pub fn disconnected(&self) {
        let mut owner=self.input.owner.lock().unwrap();
        if owner.owns(self.id) && owner.active {owner.active=false;self.input.release_held();}
    }
    /// Lifecycle updates survive a transient disconnect, but serialize with replacement and shutdown.
    pub fn with_current(&self, action:impl FnOnce()) {
        let owner=self.input.owner.lock().unwrap();
        if owner.owns(self.id) {action();}
    }
    pub fn with_input(&self, action:impl FnOnce()) {
        let owner=self.input.owner.lock().unwrap();
        if owner.owns(self.id) && owner.active {action();}
    }
}
impl Drop for DesktopInputSession {fn drop(&mut self) {self.disconnected();}}

#[cfg(test)]
impl InputInjector {
    pub fn without_desktop() -> Self {
        Self { mac_controller:std::sync::atomic::AtomicBool::new(false),
            cmd:std::sync::Mutex::new(CmdState::default()), serial:std::sync::Mutex::new(()),
            owner:std::sync::Mutex::new(DesktopOwner::default()), shortcuts:std::sync::Mutex::new(Shortcuts::default()),
            uinput:None, screen:std::sync::Mutex::new((0,0)), portal_failures:std::sync::atomic::AtomicUsize::new(0),
            portal_outcomes:std::array::from_fn(|_|std::sync::atomic::AtomicUsize::new(0)),
            portal:std::sync::Mutex::new(None), backend:crate::desktop_backend::DesktopBackend::Portal,
            pending:std::sync::Mutex::new(PendingPointer::default()), held:std::sync::Mutex::new(HeldInput::default()),
            touch:std::sync::Mutex::new(TouchDrag::default()) }
    }
}

/// Keysyms for mobile IME text: printable ASCII is its own keysym, newline is Return, tab is Tab.
fn ascii_keysyms(text: &str) -> Option<Vec<u32>> {
    text.chars().map(|c| match c { '\n' => Some(0xff0d), '\t' => Some(0xff09), ' '..='~' => Some(c as u32), _ => None }).collect()
}
/// Whether an `org.gnome.desktop.input-sources` source list starts with an xkb layout that has
/// printable ASCII in its first group. Mutter drops keysyms the active layout lacks, and an IBus
/// engine (pinyin, ...) would compose the keys instead.
fn first_source_types_ascii(sources: &str) -> bool {
    let Some(start)=sources.find('(') else { return false };
    let Some(end)=sources[start..].find(')') else { return false };
    let fields:Vec<&str>=sources[start+1..start+end].split(',').map(|f| f.trim().trim_matches('\'')).collect();
    let [kind, layout]=fields[..] else { return false };
    let (base, variant)=layout.split_once('+').unwrap_or((layout, ""));
    const NON_LATIN: &[&str]=&["af","am","ara","bd","bg","bt","by","eg","et","ge","gr","il","in","iq","ir","kg","kh","kz","la","lk","mk","mm","mn","mv","np","ru","sy","th","tj","ua"];
    kind=="xkb" && !NON_LATIN.contains(&base) && !variant.contains("cyrillic") && (base!="rs" || variant.contains("latin"))
}
/// GNOME's active input source is the first of `mru-sources` (else of `sources`). Cached for 2 s.
fn gnome_layout_types_ascii() -> bool {
    #[cfg(test)]
    { false }
    #[cfg(not(test))]
    {
        static CACHE: std::sync::Mutex<Option<(std::time::Instant,bool)>> = std::sync::Mutex::new(None);
        let mut cache=CACHE.lock().unwrap();
        if let Some((at,value))=*cache { if at.elapsed()<std::time::Duration::from_secs(2) { return value; } }
        let read=|key: &str| std::process::Command::new("gsettings").args(["get","org.gnome.desktop.input-sources",key])
            .stderr(std::process::Stdio::null()).output().ok().filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).filter(|s| s.contains('('));
        let value=read("mru-sources").or_else(|| read("sources")).is_some_and(|s| first_source_types_ascii(&s));
        *cache=Some((std::time::Instant::now(),value));
        value
    }
}

#[cfg(test)]
mod text_keysym_tests {
    use super::*;
    #[test]
    fn ascii_text_becomes_keysyms_and_other_text_does_not() {
        assert_eq!(ascii_keysyms("Hi!\n\t~"), Some(vec![0x48,0x69,0x21,0xff0d,0xff09,0x7e]));
        assert_eq!(ascii_keysyms("čaj"), None);
        assert_eq!(ascii_keysyms("测试"), None);
    }
    #[test]
    fn only_latin_xkb_layouts_type_ascii() {
        assert!(first_source_types_ascii("[('xkb', 'us'), ('ibus', 'libpinyin'), ('xkb', 'rs+latin')]"));
        assert!(first_source_types_ascii("[('xkb', 'rs+latin')]"));
        assert!(first_source_types_ascii("[('xkb', 'de+nodeadkeys')]"));
        assert!(!first_source_types_ascii("[('ibus', 'libpinyin'), ('xkb', 'us')]"));
        assert!(!first_source_types_ascii("[('xkb', 'rs')]"));
        assert!(!first_source_types_ascii("[('xkb', 'ru')]"));
        assert!(first_source_types_ascii("[('xkb', 'ba+unicodeus')]"));
        assert!(!first_source_types_ascii("[('xkb', 'me+cyrillic')]"));
        assert!(!first_source_types_ascii("@a(ss) []"));
    }
}

#[cfg(test)]
mod touch_tests {
    use super::*;
    #[test]
    fn controller_from_platform_numbers_and_mac_strings() {
        use serde_json::json;
        assert_eq!(Controller::from_platform(Some(&json!(2))), Controller::Mobile);
        assert_eq!(Controller::from_platform(Some(&json!("3"))), Controller::Mobile);
        assert_eq!(Controller::from_platform(Some(&json!(4))), Controller::Mac);
        assert_eq!(Controller::from_platform(Some(&json!("MacOS"))), Controller::Mac);
        assert_eq!(Controller::from_platform(Some(&json!(1))), Controller::Other);
        assert_eq!(Controller::from_platform(None), Controller::Other);
    }
    #[test]
    fn a_press_scrolls_past_the_slop_and_selects_when_held() {
        let t0=std::time::Instant::now();
        let pending=Finger::Pending{start:(100,100),at:t0};
        // Small wander stays undecided; past SLOP the content follows the finger from the start.
        assert_eq!(finger_motion(pending,(105,110)),(pending,FingerStep::Stay));
        assert_eq!(finger_motion(pending,(100,100+SLOP+1)),(Finger::Scroll{last:(100,113)},FingerStep::Scroll((100,100),(100,113))));
        assert_eq!(finger_motion(Finger::Scroll{last:(100,113)},(100,90)),(Finger::Scroll{last:(100,90)},FingerStep::Scroll((100,113),(100,90))));
        assert_eq!(finger_motion(Finger::Select,(7,8)),(Finger::Select,FingerStep::Move(7,8)));
        assert_eq!(finger_motion(Finger::Up,(7,8)),(Finger::Up,FingerStep::Move(7,8)));
        assert_eq!(finger_held(pending,t0+HOLD-std::time::Duration::from_millis(1)),None);
        assert_eq!(finger_held(pending,t0+HOLD),Some((100,100)));
        assert_eq!(finger_held(Finger::Scroll{last:(1,1)},t0+HOLD*4),None);
    }
    #[test]
    fn mobile_drag_without_touchscreen_stays_a_mouse_button() {
        let input=InputInjector::without_desktop();
        input.set_controller(Controller::Mobile);
        input.mouse_absolute_normalized(0.5,0.5,(100,100)); input.flush_pointer();
        assert!(input.touch.lock().unwrap().at.is_some());
        assert!(!input.touch_button(true) && !input.touch.lock().unwrap().down);
        assert!(!input.touch_button(false));
        input.mouse_relative(1,1); input.flush_pointer();
        assert!(input.touch.lock().unwrap().at.is_none());
        input.set_controller(Controller::Other);
        assert_eq!(input.touch.lock().unwrap().mode,TouchMode::Mouse);
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::sync::{Arc,atomic::{AtomicUsize,Ordering}};
    #[test]
    fn current_owner_can_publish_while_disconnected_but_cannot_inject() {
        let input=Arc::new(InputInjector::without_desktop());
        let session=input.desktop_session();
        session.connected();session.disconnected();
        let publications=AtomicUsize::new(0);
        session.with_current(||{publications.fetch_add(1,Ordering::Relaxed);});
        session.with_input(||panic!("disconnected owner must not inject"));
        assert_eq!(publications.load(Ordering::Relaxed),1);
        session.connected();
        session.with_input(||{publications.fetch_add(1,Ordering::Relaxed);});
        let next=input.desktop_session();
        session.with_current(||panic!("replaced owner must not publish"));
        next.with_current(||{publications.fetch_add(1,Ordering::Relaxed);});
        input.shutdown();
        next.with_current(||panic!("shutdown must reject lifecycle updates"));
        assert_eq!(publications.load(Ordering::Relaxed),3);
    }
    #[test]
    fn current_owner_gate_holds_ownership_through_publication() {
        let input=Arc::new(InputInjector::without_desktop());
        let session=input.desktop_session();
        let (started,ready)=std::sync::mpsc::channel();
        let (finish,resume)=std::sync::mpsc::channel();
        let worker=session.clone();
        let publication=std::thread::spawn(move ||worker.with_current(|| {
            started.send(()).unwrap();resume.recv().unwrap();
        }));
        ready.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert!(input.owner.try_lock().is_err());
        let replacing=input.clone();
        let replacement=std::thread::spawn(move ||replacing.desktop_session());
        finish.send(()).unwrap();
        publication.join().unwrap();
        let next=replacement.join().unwrap();
        assert!(next.is_current());
        session.with_current(||panic!("late publication after replacement"));
    }
    #[test]
    fn cleanup_discards_motion_shortcuts_and_untracked_input_ledger() {
        let input=Arc::new(InputInjector::new());
        input.held.lock().unwrap().keys.insert(0xffe1);
        input.held.lock().unwrap().buttons.insert(1);
        input.shortcuts.lock().unwrap().modifiers=2;
        input.shortcuts.lock().unwrap().shifted.insert(0x56);
        input.mouse_relative(32,0);
        input.mouse_scroll(0,1);
        input.release_held();
        assert!(input.held.lock().unwrap().keys.is_empty());
        assert!(input.held.lock().unwrap().buttons.is_empty());
        assert_eq!(input.pending.lock().unwrap().relative,(0,0));
        assert_eq!(input.pending.lock().unwrap().scroll,(0,0));
        assert_eq!(input.shortcuts.lock().unwrap().modifiers,0);
        assert!(input.shortcuts.lock().unwrap().shifted.is_empty());
        input.release_held(); // idempotent even with an empty ledger
    }
    #[test]
    fn disconnected_or_replaced_desktop_cannot_inject_or_clear_new_input() {
        let input=Arc::new(InputInjector::new());
        let count=AtomicUsize::new(0);
        let old=input.desktop_session();
        old.with_input(||{count.fetch_add(1,Ordering::Relaxed);});
        assert_eq!(count.load(Ordering::Relaxed),0);
        old.connected();
        old.with_input(||{count.fetch_add(1,Ordering::Relaxed);});
        old.disconnected();
        old.with_input(||{count.fetch_add(1,Ordering::Relaxed);});
        assert_eq!(count.load(Ordering::Relaxed),1);
        input.held.lock().unwrap().buttons.insert(2);
        old.disconnected(); // repeated close states must not release later local activity
        assert!(input.held.lock().unwrap().buttons.contains(&2));
        old.connected(); // same peer can reconnect
        assert!(input.held.lock().unwrap().buttons.is_empty());
        let new=input.desktop_session();
        new.connected();
        input.held.lock().unwrap().buttons.insert(1);
        old.disconnected();old.connected();drop(old);
        assert!(input.held.lock().unwrap().buttons.contains(&1));
        new.with_input(||{count.fetch_add(1,Ordering::Relaxed);});
        assert_eq!(count.load(Ordering::Relaxed),2);
        drop(new);
        assert!(input.held.lock().unwrap().buttons.is_empty());
        let last=input.desktop_session();last.connected();
        input.shutdown();last.connected();
        last.with_input(||panic!("shutdown cannot be undone by a stale callback"));
        let later=input.desktop_session();later.connected();
        later.with_input(||panic!("shutdown must reject newly arriving offers too"));
    }
    #[test]
    fn input_gate_serializes_disconnect_against_inflight_event() {
        let input=Arc::new(InputInjector::new());
        let session=input.desktop_session();session.connected();
        let (started,ready)=std::sync::mpsc::channel();
        let (finish,resume)=std::sync::mpsc::channel();
        let worker=session.clone();
        let event=std::thread::spawn(move ||worker.with_input(|| {
            started.send(()).unwrap();resume.recv().unwrap();
            worker.input.held.lock().unwrap().buttons.insert(1);
        }));
        ready.recv().unwrap();
        let ending=session.clone();
        let teardown=std::thread::spawn(move ||ending.disconnected());
        finish.send(()).unwrap();event.join().unwrap();teardown.join().unwrap();
        assert!(input.held.lock().unwrap().buttons.is_empty());
        session.with_input(||panic!("late event after disconnect"));
    }
}

#[cfg(all(test,target_os="linux",feature="wayland"))]
mod output_pointer_tests {
    use super::*;
    #[test]
    #[ignore="requires unlocked Hyprland, UU_WAYLAND_MULTISCREEN=1 and accessible uinput; moves pointer only"]
    fn absolute_pointer_stays_on_capture_output_with_uinput_enabled() {
        fn cursor()->serde_json::Value {
            let output=std::process::Command::new("hyprctl").args(["cursorpos","-j"]).output().unwrap();
            assert!(output.status.success());serde_json::from_slice(&output.stdout).unwrap()
        }
        struct Restore(serde_json::Value);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _=std::process::Command::new("hyprctl").args(["dispatch","movecursor",&self.0["x"].to_string(),&self.0["y"].to_string()]).output();
            }
        }
        let _restore=Restore(cursor());
        crate::wayland::init().unwrap();
        let input=InputInjector::new();
        assert!(input.uinput.is_some(),"test must exercise the uinput/native hybrid path");
        // libinput discovers the newly created virtual devices asynchronously.
        std::thread::sleep(std::time::Duration::from_secs(1));
        let output=std::process::Command::new("hyprctl").args(["monitors","-j"]).output().unwrap();
        let monitors:Vec<serde_json::Value>=serde_json::from_slice(&output.stdout).unwrap();
        let outputs=crate::wayland::outputs();assert!(outputs.len()>=2,"needs at least two outputs");
        for (id,output) in outputs.iter().enumerate().chain(outputs.iter().enumerate().rev()) {
            crate::wayland::select(id).unwrap();
            let monitor=monitors.iter().find(|m|m["name"].as_str()==Some(output.name.as_str())).unwrap();
            let scale=monitor["scale"].as_f64().unwrap();
            for (x,y) in [(0.1,0.1),(0.5,0.5),(0.9,0.9),(0.1,0.9),(0.9,0.1)] {
                let size=(output.width,output.height);
                let pixels=crate::input_mapping::normalized_pointer(x,y,size.0,size.1).unwrap();
                assert_eq!(input.backend,crate::desktop_backend::DesktopBackend::Wayland);
                input.mouse_absolute_normalized(x,y,size);input.flush_pointer();
                let expected=(monitor["x"].as_f64().unwrap()+f64::from(pixels.0)/scale,
                    monitor["y"].as_f64().unwrap()+f64::from(pixels.1)/scale);
                // Motion is asynchronous; observe its first arrival instead of a
                // later position potentially changed by another input device.
                let deadline=std::time::Instant::now()+std::time::Duration::from_millis(250);
                loop {
                    let actual=cursor();
                    let matched=(actual["x"].as_f64().unwrap()-expected.0).abs()<=2.0
                        && (actual["y"].as_f64().unwrap()-expected.1).abs()<=2.0;
                    if matched {break;}
                    assert!(std::time::Instant::now()<deadline,"pointer offset: output={} normalized=({},{}) actual={} expected={:?}",output.name,x,y,actual,expected);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }
    }
}

#[cfg(all(test,target_os="linux"))]
mod x11_monitor_pointer_tests {
    use super::*;
    #[test]
    #[ignore="requires an X11 display with two or more RandR monitors and UU_X11_MULTISCREEN=1; moves pointer only"]
    fn absolute_pointer_lands_on_the_selected_x11_monitor() {
        let _capture=crate::capture::ScreenCapture::new().unwrap();
        let input=InputInjector::new();
        assert_eq!(input.backend,crate::desktop_backend::DesktopBackend::X11);
        let outputs=crate::wayland::outputs();assert!(crate::wayland::x11_multiscreen() && outputs.len()>=2,"needs two X11 monitors");
        for (id,output) in outputs.iter().enumerate().chain(outputs.iter().enumerate().rev()) {
            crate::wayland::select(id).unwrap();
            for (x,y) in [(0.0,0.0),(0.5,0.5),(0.999,0.999),(0.1,0.9)] {
                let size=(output.width,output.height);
                let pixels=crate::input_mapping::normalized_pointer(x,y,size.0,size.1).unwrap();
                input.mouse_absolute_normalized(x,y,size);input.flush_pointer();
                let (mut ax,mut ay,mut mask)=(0,0,0);
                assert_eq!(unsafe {x11_pointer_state(&mut ax,&mut ay,&mut mask)},0);
                assert_eq!((ax,ay),(output.x+pixels.0,output.y+pixels.1),"monitor={} normalized=({x},{y})",output.name);
            }
        }
        crate::wayland::select(0).unwrap();
    }
}

#[cfg(test)]
mod clipboard_paste_text_tests {
    use super::*;

    #[test]
    fn rejects_empty_and_oversized_text() {
        let input = InputInjector::new();
        assert!(!input.set_clipboard_text(""));
        let oversized = "a".repeat(crate::clipboard::MAX_TEXT + 1);
        assert!(!input.set_clipboard_text(&oversized));
    }

    #[test]
    fn text_input_handles_portal_backend_safely_without_side_effects() {
        let mut input = InputInjector::new();
        input.backend = crate::desktop_backend::DesktopBackend::Portal;
        // In unit tests, set_clipboard_text avoids external commands so this
        // will not mutate the host's actual clipboard.
        input.text_input("测试输入法文字");
    }

    #[test]
    fn text_input_respects_disable_env() {
        let mut input = InputInjector::new();
        input.backend = crate::desktop_backend::DesktopBackend::Portal;
        std::env::set_var("UU_WAYLAND_TEXT", "0");
        input.text_input("测试输入法文字");
        std::env::remove_var("UU_WAYLAND_TEXT");
    }
}
