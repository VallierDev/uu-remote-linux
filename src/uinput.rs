//! Kernel-level virtual keyboard and pointer through /dev/uinput (as Sunshine does).
//!
//! Works the same under X11, every Wayland compositor and the greeter/lock screen, needs no
//! portal permission and sends physical keycodes, so the compositor's keysym → key lookup
//! (which can pick an AltGr level for `minus` and drop `_`, issue #3) is not involved.
//! Access: a udev rule `KERNEL=="uinput", SUBSYSTEM=="misc", OPTIONS+="static_node=uinput",
//! TAG+="uaccess"` grants the logged-in seat user access (see README).
//!
//! Devices: one keyboard + relative mouse, one absolute pointer (ABS_X/ABS_Y 0..=65535 with
//! buttons and wheels, like a QEMU tablet). Mixing relative and absolute axes in one device
//! confuses libinput.

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct TextKey { pub code: u16, pub shift: u16 }

/// Resolve an ASCII prefix against the host's current XKB layout and locked modifiers.
/// X11/XWayland is queried read-only; the returned events are emitted through uinput.
pub fn text_plan(text: &str) -> Vec<TextKey> {
    #[cfg(target_os = "linux")]
    {
        extern "C" {
            fn uu_uinput_text_plan(text: *const u8, length: usize, plan: *mut TextKey, capacity: usize) -> usize;
        }
        let capacity = text.bytes().take_while(u8::is_ascii).count();
        if capacity == 0 { return Vec::new(); }
        let mut plan = vec![TextKey::default(); capacity];
        let count = unsafe { uu_uinput_text_plan(text.as_ptr(), text.len(), plan.as_mut_ptr(), plan.len()) };
        plan.truncate(count);
        plan
    }
    #[cfg(not(target_os = "linux"))]
    { let _ = text; Vec::new() }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::fs::{File, OpenOptions};
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    use std::sync::Mutex;

    const EV_SYN: u16 = 0x00;
    const EV_KEY: u16 = 0x01;
    const EV_REL: u16 = 0x02;
    const EV_ABS: u16 = 0x03;
    const SYN_REPORT: u16 = 0;
    const REL_X: u16 = 0x00;
    const REL_Y: u16 = 0x01;
    const REL_HWHEEL: u16 = 0x06;
    const REL_WHEEL: u16 = 0x08;
    const ABS_X: u16 = 0x00;
    const ABS_Y: u16 = 0x01;
    pub const BTN_LEFT: u16 = 0x110;
    pub const BTN_RIGHT: u16 = 0x111;
    pub const BTN_MIDDLE: u16 = 0x112;
    pub const BTN_SIDE: u16 = 0x113;
    pub const BTN_EXTRA: u16 = 0x114;
    pub const ABS_MAX: i32 = 65535;

    const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
    const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
    const UI_SET_RELBIT: libc::c_ulong = 0x4004_5566;
    const UI_SET_ABSBIT: libc::c_ulong = 0x4004_5567;
    const UI_DEV_CREATE: libc::c_ulong = 0x5501;
    const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
    const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;
    const UI_ABS_SETUP: libc::c_ulong = 0x401c_5504;

    #[repr(C)]
    struct InputId { bustype: u16, vendor: u16, product: u16, version: u16 }
    #[repr(C)]
    struct UinputSetup { id: InputId, name: [u8; 80], ff_effects_max: u32 }
    #[repr(C)]
    struct AbsInfo { value: i32, minimum: i32, maximum: i32, fuzz: i32, flat: i32, resolution: i32 }
    #[repr(C)]
    struct UinputAbsSetup { code: u16, _pad: u16, absinfo: AbsInfo }
    #[repr(C)]
    struct InputEvent { time: libc::timeval, kind: u16, code: u16, value: i32 }

    struct Device(File);
    impl Drop for Device {
        fn drop(&mut self) { unsafe { libc::ioctl(self.0.as_raw_fd(), UI_DEV_DESTROY); } }
    }
    impl Device {
        fn ioctl(&self, request: libc::c_ulong, value: libc::c_ulong) -> std::io::Result<()> {
            if unsafe { libc::ioctl(self.0.as_raw_fd(), request, value) } < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
        }
        fn create(name: &str, product: u16, configure: impl FnOnce(&Device) -> std::io::Result<()>) -> std::io::Result<Self> {
            let file = OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open("/dev/uinput")?;
            let device = Device(file);
            configure(&device)?;
            let mut setup = UinputSetup { id: InputId { bustype: 0x06 /* BUS_VIRTUAL */, vendor: 0x1d6b, product, version: 1 }, name: [0; 80], ff_effects_max: 0 };
            setup.name[..name.len().min(79)].copy_from_slice(&name.as_bytes()[..name.len().min(79)]);
            if unsafe { libc::ioctl(device.0.as_raw_fd(), UI_DEV_SETUP, &setup) } < 0 { return Err(std::io::Error::last_os_error()); }
            device.ioctl(UI_DEV_CREATE, 0)?;
            Ok(device)
        }
        fn abs(&self, code: u16) -> std::io::Result<()> {
            self.ioctl(UI_SET_ABSBIT, code.into())?;
            let setup = UinputAbsSetup { code, _pad: 0, absinfo: AbsInfo { value: 0, minimum: 0, maximum: ABS_MAX, fuzz: 0, flat: 0, resolution: 0 } };
            if unsafe { libc::ioctl(self.0.as_raw_fd(), UI_ABS_SETUP, &setup) } < 0 { return Err(std::io::Error::last_os_error()); }
            Ok(())
        }
        fn emit(&mut self, events: &[(u16, u16, i32)]) -> std::io::Result<()> {
            let zero = libc::timeval { tv_sec: 0, tv_usec: 0 };
            let mut buffer = Vec::with_capacity((events.len() + 1) * std::mem::size_of::<InputEvent>());
            for &(kind, code, value) in events.iter().chain(std::iter::once(&(EV_SYN, SYN_REPORT, 0))) {
                let event = InputEvent { time: zero, kind, code, value };
                let bytes = unsafe { std::slice::from_raw_parts((&event as *const InputEvent).cast::<u8>(), std::mem::size_of::<InputEvent>()) };
                buffer.extend_from_slice(bytes);
            }
            self.0.write_all(&buffer)
        }
    }

    pub struct Devices { keyboard: Mutex<Device>, pointer: Mutex<Device> }

    impl Devices {
        #[cfg(test)]
        pub fn recording_for_test() -> (Self, File, File) {
            let recording = || {
                let path = std::env::temp_dir().join(format!("uu-uinput-test-{}-{}", std::process::id(), rand::random::<u64>()));
                let file = OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&path).unwrap();
                std::fs::remove_file(path).unwrap();
                let reader = file.try_clone().unwrap();
                (Mutex::new(Device(file)), reader)
            };
            let (keyboard, keys) = recording();
            let (pointer, mouse) = recording();
            (Self { keyboard, pointer }, keys, mouse)
        }
        #[cfg(test)]
        pub fn recorded_events_for_test(file: &mut File) -> Vec<(u16, u16, i32)> {
            use std::io::{Read, Seek, SeekFrom};
            file.seek(SeekFrom::Start(0)).unwrap();
            let mut data = Vec::new();
            file.read_to_end(&mut data).unwrap();
            assert_eq!(data.len() % std::mem::size_of::<InputEvent>(), 0);
            data.chunks_exact(std::mem::size_of::<InputEvent>()).map(|chunk| {
                let event = unsafe { std::ptr::read_unaligned(chunk.as_ptr().cast::<InputEvent>()) };
                (event.kind, event.code, event.value)
            }).collect()
        }
        pub fn open() -> std::io::Result<Self> {
            let keyboard = Device::create("uu-remote-linux keyboard", 1, |d| {
                d.ioctl(UI_SET_EVBIT, EV_KEY.into())?;
                for code in 1..=248u16 { d.ioctl(UI_SET_KEYBIT, code.into())?; }
                // Relative motion (games / pointer lock) lives with the keyboard device.
                d.ioctl(UI_SET_EVBIT, EV_REL.into())?;
                d.ioctl(UI_SET_RELBIT, REL_X.into())?;
                d.ioctl(UI_SET_RELBIT, REL_Y.into())?;
                for button in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE] { d.ioctl(UI_SET_KEYBIT, button.into())?; }
                Ok(())
            })?;
            let pointer = Device::create("uu-remote-linux pointer", 2, |d| {
                d.ioctl(UI_SET_EVBIT, EV_KEY.into())?;
                for button in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] { d.ioctl(UI_SET_KEYBIT, button.into())?; }
                d.ioctl(UI_SET_EVBIT, EV_REL.into())?;
                d.ioctl(UI_SET_RELBIT, REL_WHEEL.into())?;
                d.ioctl(UI_SET_RELBIT, REL_HWHEEL.into())?;
                d.ioctl(UI_SET_EVBIT, EV_ABS.into())?;
                d.abs(ABS_X)?;
                d.abs(ABS_Y)
            })?;
            Ok(Self { keyboard: Mutex::new(keyboard), pointer: Mutex::new(pointer) })
        }
        pub fn key(&self, code: u16, pressed: bool) -> std::io::Result<()> {
            self.keyboard.lock().unwrap().emit(&[(EV_KEY, code, i32::from(pressed))])
        }
        pub fn release_all(&self) -> std::io::Result<()> {
            // Release every supported code on our own devices, even if the
            // userspace ledger missed a press. Never touch physical devices.
            let keys:Vec<_>=(1..=248).map(|code|(EV_KEY,code,0))
                .chain([BTN_LEFT,BTN_RIGHT,BTN_MIDDLE].into_iter().map(|code|(EV_KEY,code,0))).collect();
            let buttons:Vec<_>=[BTN_LEFT,BTN_RIGHT,BTN_MIDDLE,BTN_SIDE,BTN_EXTRA].into_iter().map(|code|(EV_KEY,code,0)).collect();
            let keyboard=self.keyboard.lock().unwrap().emit(&keys);
            let pointer=self.pointer.lock().unwrap().emit(&buttons);
            keyboard.and(pointer)
        }
        pub fn move_absolute(&self, x: i32, y: i32) -> std::io::Result<()> {
            self.pointer.lock().unwrap().emit(&[(EV_ABS, ABS_X, x.clamp(0, ABS_MAX)), (EV_ABS, ABS_Y, y.clamp(0, ABS_MAX))])
        }
        pub fn move_relative(&self, dx: i32, dy: i32) -> std::io::Result<()> {
            self.keyboard.lock().unwrap().emit(&[(EV_REL, REL_X, dx), (EV_REL, REL_Y, dy)])
        }
        pub fn button(&self, code: u16, pressed: bool) -> std::io::Result<()> {
            self.pointer.lock().unwrap().emit(&[(EV_KEY, code, i32::from(pressed))])
        }
        pub fn wheel(&self, vertical: i32, horizontal: i32) -> std::io::Result<()> {
            let mut events = Vec::new();
            if vertical != 0 { events.push((EV_REL, REL_WHEEL, vertical)); }
            if horizontal != 0 { events.push((EV_REL, REL_HWHEEL, horizontal)); }
            if events.is_empty() { return Ok(()); }
            self.pointer.lock().unwrap().emit(&events)
        }
    }

    #[cfg(test)]
    mod reset_acceptance {
        use super::*;
        fn snapshot(device:&Device)->[u8;96] {
            let mut name=[0u8;80];
            let request=0x8000_0000u64 | (80<<16) | (b'U' as u64)<<8 | 44;
            assert!(unsafe {libc::ioctl(device.0.as_raw_fd(),request as libc::c_ulong,name.as_mut_ptr())}>=0);
            let end=name.iter().position(|v|*v==0).unwrap();
            let directory=std::path::Path::new("/sys/devices/virtual/input").join(std::str::from_utf8(&name[..end]).unwrap());
            let event=std::fs::read_dir(directory).unwrap().filter_map(Result::ok)
                .find(|e|e.file_name().to_string_lossy().starts_with("event")).unwrap();
            let file=File::open(std::path::Path::new("/dev/input").join(event.file_name())).unwrap();
            let mut state=[0u8;96];
            let request=0x8000_0000u64 | (96<<16) | (b'E' as u64)<<8 | 0x18;
            assert!(unsafe {libc::ioctl(file.as_raw_fd(),request as libc::c_ulong,state.as_mut_ptr())}>=0);
            state
        }
        #[test]
        #[ignore="requires isolated QEMU VM uinput and evdev permissions"]
        fn kernel_devices_release_even_without_an_inputinjector_ledger() {
            assert_eq!(std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap().trim(),"uu-hyprland-test");
            assert_eq!(std::process::Command::new("systemd-detect-virt").output().unwrap().stdout,b"qemu\n");
            let devices=Devices::open().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(500));
            devices.key(42,true).unwrap();devices.key(30,true).unwrap();
            devices.button(BTN_LEFT,true).unwrap();
            assert!(snapshot(&devices.keyboard.lock().unwrap()).iter().any(|v|*v!=0));
            assert!(snapshot(&devices.pointer.lock().unwrap()).iter().any(|v|*v!=0));
            devices.release_all().unwrap();
            assert!(snapshot(&devices.keyboard.lock().unwrap()).iter().all(|v|*v==0));
            assert!(snapshot(&devices.pointer.lock().unwrap()).iter().all(|v|*v==0));
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    pub const BTN_LEFT: u16 = 0x110;
    pub const BTN_RIGHT: u16 = 0x111;
    pub const BTN_MIDDLE: u16 = 0x112;
    pub const BTN_SIDE: u16 = 0x113;
    pub const BTN_EXTRA: u16 = 0x114;
    pub const ABS_MAX: i32 = 65535;
    pub struct Devices;
    impl Devices {
        pub fn open() -> std::io::Result<Self> { Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "uinput requires Linux")) }
        pub fn release_all(&self) -> std::io::Result<()> { Ok(()) }
        pub fn key(&self, _: u16, _: bool) -> std::io::Result<()> { Ok(()) }
        pub fn move_absolute(&self, _: i32, _: i32) -> std::io::Result<()> { Ok(()) }
        pub fn move_relative(&self, _: i32, _: i32) -> std::io::Result<()> { Ok(()) }
        pub fn button(&self, _: u16, _: bool) -> std::io::Result<()> { Ok(()) }
        pub fn wheel(&self, _: i32, _: i32) -> std::io::Result<()> { Ok(()) }
    }
}

pub use imp::{Devices, ABS_MAX, BTN_EXTRA, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, BTN_SIDE};

/// X11 button ordinal (1 left, 2 middle, 3 right, 8/9 side) → evdev button code.
pub fn button_code(x11_button: i32) -> Option<u16> {
    match x11_button { 1 => Some(BTN_LEFT), 2 => Some(BTN_MIDDLE), 3 => Some(BTN_RIGHT), 8 => Some(BTN_SIDE), 9 => Some(BTN_EXTRA), _ => None }
}

/// X11 wheel "buttons" 4/5 (vertical) and 6/7 (horizontal) → (REL_WHEEL, REL_HWHEEL) steps.
pub fn wheel_step(x11_button: i32) -> Option<(i32, i32)> {
    match x11_button { 4 => Some((1, 0)), 5 => Some((-1, 0)), 6 => Some((0, -1)), 7 => Some((0, 1)), _ => None }
}

/// Base keysym (as produced by input_mapping::windows_vk_to_keysym) → physical evdev keycode
/// at its US-layout position. The compositor then applies the user's own layout, exactly as
/// for a hardware keyboard.
pub fn keysym_to_evdev(keysym: u64) -> Option<u16> {
    const LETTERS: [u16; 26] = [30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44];
    const KEYPAD: [u16; 10] = [82, 79, 80, 81, 75, 76, 77, 71, 72, 73];
    let code = match keysym {
        0x61..=0x7a => LETTERS[(keysym - 0x61) as usize],
        0x31..=0x39 => (keysym - 0x31) as u16 + 2, // 1..9
        0x30 => 11,
        0xffb0..=0xffb9 => KEYPAD[(keysym - 0xffb0) as usize],
        0xffbe..=0xffc7 => (keysym - 0xffbe) as u16 + 59, // F1..F10
        0xffc8 => 87, 0xffc9 => 88,                         // F11, F12
        0xffca..=0xffd5 => (keysym - 0xffca) as u16 + 183,  // F13..F24
        0xff0b => 76, // Clear: keypad 5 without NumLock, as on PC keyboards
        0xff08 => 14, 0xff09 => 15, 0xff0d => 28, 0xff1b => 1, 0x20 => 57,
        0xff13 => 119, 0xffe5 => 58, 0xff7f => 69, 0xff14 => 70, 0xff61 => 99,
        0xffe1 => 42, 0xffe2 => 54, 0xffe3 => 29, 0xffe4 => 97, 0xffe9 => 56, 0xffea => 100,
        0xffeb => 125, 0xffec => 126, 0xff67 => 127,
        0xff55 => 104, 0xff56 => 109, 0xff57 => 107, 0xff50 => 102,
        0xff51 => 105, 0xff52 => 103, 0xff53 => 106, 0xff54 => 108,
        0xff63 => 110, 0xffff => 111,
        0xffaa => 55, 0xffab => 78, 0xffac => 121, 0xffad => 74, 0xffae => 83, 0xffaf => 98, 0xff8d => 96,
        0x3b => 39, 0x3d => 13, 0x2c => 51, 0x2d => 12, 0x2e => 52, 0x2f => 53, 0x60 => 41,
        0x5b => 26, 0x5c => 43, 0x5d => 27, 0x27 => 40,
        0x1008ff12 => 113, 0x1008ff11 => 114, 0x1008ff13 => 115,
        0x1008ff17 => 163, 0x1008ff16 => 165, 0x1008ff15 => 166, 0x1008ff14 => 164,
        _ => return None,
    };
    Some(code)
}

/// Input backend policy: `UU_INPUT_BACKEND=uinput` forces uinput, `native` disables it,
/// `auto` (default) uses uinput for Wayland/portal sessions when /dev/uinput is accessible
/// and keeps the verified XTest path on X11.
pub fn wanted(backend: crate::desktop_backend::DesktopBackend) -> bool {
    match std::env::var("UU_INPUT_BACKEND").as_deref() {
        Ok("uinput") => true,
        Ok("native") => false,
        _ => backend != crate::desktop_backend::DesktopBackend::X11,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maps_keysyms_to_physical_us_positions() {
        assert_eq!(keysym_to_evdev(0x61), Some(30));   // a
        assert_eq!(keysym_to_evdev(0x7a), Some(44));   // z
        assert_eq!(keysym_to_evdev(0x30), Some(11));   // 0
        assert_eq!(keysym_to_evdev(0x31), Some(2));    // 1
        assert_eq!(keysym_to_evdev(0x2d), Some(12));   // minus: Shift+minus gives '_' (#3)
        assert_eq!(keysym_to_evdev(0xffe1), Some(42)); // Shift_L
        assert_eq!(keysym_to_evdev(0xffc9), Some(88)); // F12
        assert_eq!(keysym_to_evdev(0xffb5), Some(76)); // KP_5
        assert_eq!(keysym_to_evdev(0x12345), None);
    }
    #[test]
    fn every_mapped_windows_vk_has_a_physical_key() {
        let missing: Vec<i32> = (0..=0xff).filter(|vk| {
            crate::input_mapping::windows_vk_to_keysym(*vk).is_some_and(|ks| keysym_to_evdev(ks).is_none())
        }).collect();
        assert!(missing.is_empty(), "VKs without evdev mapping: {missing:x?}");
    }
    #[test]
    fn buttons_and_wheel() {
        assert_eq!(button_code(1), Some(BTN_LEFT));
        assert_eq!(button_code(3), Some(BTN_RIGHT));
        assert_eq!(wheel_step(4), Some((1, 0)));
        assert_eq!(wheel_step(7), Some((0, 1)));
        assert_eq!(button_code(4), None);
    }
}
