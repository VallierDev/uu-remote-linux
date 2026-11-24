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

/// Rumble sink of the latest desktop peer: pad reader threads push `(index, left, right)`,
/// webrtc forwards it as raw `gpd_vibration` JSON on CONTROL. Rebinding is the desktop-peer gate.
#[derive(Default)]
pub struct RumbleOut(std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<(u8, u8, u8)>>>);
impl RumbleOut {
    pub fn bind(&self, sender: tokio::sync::mpsc::UnboundedSender<(u8, u8, u8)>) { *self.0.lock().unwrap() = Some(sender); }
    pub fn send(&self, index: u8, left: u8, right: u8) {
        let sender = self.0.lock().unwrap().clone();
        let dropped = sender.is_none_or(|sender| sender.send((index, left, right)).is_err());
        if dropped {
            static DROPPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !DROPPED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                log::info!("Gamepad rumble dropped: no CONTROL peer");
            }
        }
    }
}
/// XInput rumble defaults on; `UU_GAMEPAD_RUMBLE=0` keeps a plain pad without FF slots.
pub fn rumble_enabled() -> bool { std::env::var("UU_GAMEPAD_RUMBLE").as_deref() != Ok("0") }

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
    const EV_FF: u16 = 0x15;
    const EV_UINPUT: u16 = 0x0101;
    const UI_FF_UPLOAD: u16 = 1;
    const UI_FF_ERASE: u16 = 2;
    const FF_RUMBLE: u16 = 0x50;
    const SYN_REPORT: u16 = 0;
    const REL_X: u16 = 0x00;
    const REL_Y: u16 = 0x01;
    const REL_HWHEEL: u16 = 0x06;
    const REL_WHEEL: u16 = 0x08;
    const ABS_X: u16 = 0x00;
    const ABS_Y: u16 = 0x01;
    const ABS_Z: u16 = 0x02;
    const ABS_RX: u16 = 0x03;
    const ABS_RY: u16 = 0x04;
    const ABS_RZ: u16 = 0x05;
    const ABS_HAT0X: u16 = 0x10;
    const ABS_HAT0Y: u16 = 0x11;
    pub const BTN_LEFT: u16 = 0x110;
    pub const BTN_RIGHT: u16 = 0x111;
    pub const BTN_MIDDLE: u16 = 0x112;
    pub const BTN_SIDE: u16 = 0x113;
    pub const BTN_EXTRA: u16 = 0x114;
    const BTN_SOUTH: u16 = 0x130;
    const BTN_EAST: u16 = 0x131;
    const BTN_NORTH: u16 = 0x133; // kernel BTN_X: xpad reports the physical left face button here
    const BTN_WEST: u16 = 0x134;  // kernel BTN_Y: physical top face button
    const BTN_TL: u16 = 0x136;
    const BTN_TR: u16 = 0x137;
    const BTN_SELECT: u16 = 0x13a;
    const BTN_START: u16 = 0x13b;
    const BTN_MODE: u16 = 0x13c;
    const BTN_THUMBL: u16 = 0x13d;
    const BTN_THUMBR: u16 = 0x13e;
    pub const ABS_MAX: i32 = 65535;

    const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
    const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
    const UI_SET_RELBIT: libc::c_ulong = 0x4004_5566;
    const UI_SET_ABSBIT: libc::c_ulong = 0x4004_5567;
    const UI_DEV_CREATE: libc::c_ulong = 0x5501;
    const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
    const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;
    const UI_ABS_SETUP: libc::c_ulong = 0x401c_5504;
    /// Linux _IOC: dir << 30 | size << 16 | 'U' << 8 | nr; dir 1 = write, 3 = read|write.
    const fn ioc(dir: u64, size: u64, nr: u64) -> libc::c_ulong {
        ((dir << 30) | (size << 16) | (0x55 << 8) | nr) as libc::c_ulong
    }
    const fn iow(size: u64, nr: u64) -> libc::c_ulong { ioc(1, size, nr) }
    const fn iowr(size: u64, nr: u64) -> libc::c_ulong { ioc(3, size, nr) }
    const UI_SET_FFBIT: libc::c_ulong = iow(std::mem::size_of::<libc::c_int>() as u64, 107);
    const UI_BEGIN_FF_UPLOAD: libc::c_ulong = iowr(std::mem::size_of::<UinputFfUpload>() as u64, 200);
    const UI_END_FF_UPLOAD: libc::c_ulong = iow(std::mem::size_of::<UinputFfUpload>() as u64, 201);
    const UI_BEGIN_FF_ERASE: libc::c_ulong = iowr(std::mem::size_of::<UinputFfErase>() as u64, 202);
    const UI_END_FF_ERASE: libc::c_ulong = iow(std::mem::size_of::<UinputFfErase>() as u64, 203);

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

    // Hand-mirrored linux/input.h + linux/uinput.h FF uapi (POD). Sizes are pinned by a test:
    // a layout slip here would corrupt the UI_BEGIN_FF_* ioctls.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct FfTrigger { button: u16, interval: u16 }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct FfReplay { length: u16, delay: u16 }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct FfEnvelope { attack_length: u16, attack_level: u16, fade_length: u16, fade_level: u16 }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct FfRumble { strong_magnitude: u16, weak_magnitude: u16 }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct FfPeriodic {
        waveform: u16, period: u16, magnitude: i16, offset: i16, phase: u16,
        envelope: FfEnvelope, custom_len: u32, custom_data: *mut i16,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    union FfEffectUnion {
        constant: [u16; 5],
        ramp: [u16; 6],
        periodic: FfPeriodic,
        condition: [[u16; 6]; 2],
        rumble: FfRumble,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct FfEffect { type_: u16, id: i16, direction: u16, trigger: FfTrigger, replay: FfReplay, u: FfEffectUnion }
    #[repr(C)]
    struct UinputFfUpload { request_id: u32, retval: i32, effect: FfEffect, old: FfEffect }
    #[repr(C)]
    struct UinputFfErase { request_id: u32, retval: i32, effect_id: u32 }

    /// XInput state exactly as the viewer sends it: already Y-inverted and trigger-scaled (0..=255).
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct GamepadState { pub buttons: u16, pub lt: u8, pub rt: u8, pub lx: i16, pub ly: i16, pub rx: i16, pub ry: i16 }

    struct Device(File);
    impl Drop for Device {
        fn drop(&mut self) { unsafe { libc::ioctl(self.0.as_raw_fd(), UI_DEV_DESTROY); } }
    }
    impl Device {
        fn ioctl(&self, request: libc::c_ulong, value: libc::c_ulong) -> std::io::Result<()> {
            if unsafe { libc::ioctl(self.0.as_raw_fd(), request, value) } < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
        }
        fn create(name: &str, product: u16, configure: impl FnOnce(&Device) -> std::io::Result<()>) -> std::io::Result<Self> {
            Self::create_id(name, 0x06 /* BUS_VIRTUAL */, 0x1d6b, product, 1, configure)
        }
        fn create_id(name: &str, bustype: u16, vendor: u16, product: u16, version: u16, configure: impl FnOnce(&Device) -> std::io::Result<()>) -> std::io::Result<Self> {
            Self::create_full(name, bustype, vendor, product, version, false, 0, configure)
        }
        /// Pads open read+write: FF upload/erase requests arrive as reads on the same fd.
        fn create_id_ff(name: &str, bustype: u16, vendor: u16, product: u16, version: u16, ff_effects_max: u32, configure: impl FnOnce(&Device) -> std::io::Result<()>) -> std::io::Result<Self> {
            Self::create_full(name, bustype, vendor, product, version, true, ff_effects_max, configure)
        }
        fn create_full(name: &str, bustype: u16, vendor: u16, product: u16, version: u16, readable: bool, ff_effects_max: u32, configure: impl FnOnce(&Device) -> std::io::Result<()>) -> std::io::Result<Self> {
            let mut options = OpenOptions::new();
            options.write(true).read(readable);
            let file = options.custom_flags(libc::O_NONBLOCK).open("/dev/uinput")?;
            let device = Device(file);
            configure(&device)?;
            let mut setup = UinputSetup { id: InputId { bustype, vendor, product, version }, name: [0; 80], ff_effects_max };
            setup.name[..name.len().min(79)].copy_from_slice(&name.as_bytes()[..name.len().min(79)]);
            if unsafe { libc::ioctl(device.0.as_raw_fd(), UI_DEV_SETUP, &setup) } < 0 { return Err(std::io::Error::last_os_error()); }
            device.ioctl(UI_DEV_CREATE, 0)?;
            Ok(device)
        }
        fn abs(&self, code: u16) -> std::io::Result<()> {
            self.abs_range(code, 0, ABS_MAX, 0)
        }
        fn abs_range(&self, code: u16, minimum: i32, maximum: i32, flat: i32) -> std::io::Result<()> {
            self.ioctl(UI_SET_ABSBIT, code.into())?;
            let setup = UinputAbsSetup { code, _pad: 0, absinfo: AbsInfo { value: 0, minimum, maximum, fuzz: 0, flat, resolution: 0 } };
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

    /// XInput button bit → evdev key; the four dpad bits drive ABS_HAT0X/Y instead.
    /// Face mapping follows xpad: A→SOUTH, B→EAST, X→NORTH (0x133), Y→WEST (0x134).
    const GAMEPAD_BUTTONS: [(u16, u16); 11] = [
        (0x0010, BTN_START), (0x0020, BTN_SELECT), (0x0040, BTN_THUMBL), (0x0080, BTN_THUMBR),
        (0x0100, BTN_TL), (0x0200, BTN_TR), (0x0400, BTN_MODE),
        (0x1000, BTN_SOUTH), (0x2000, BTN_EAST), (0x4000, BTN_NORTH), (0x8000, BTN_WEST),
    ];
    fn slot(index: u8) -> Option<usize> { (index < 4).then_some(usize::from(index)) }
    /// Opposing directions are not set by real controllers; keep a deterministic negative value
    /// when a viewer reports the full mask.
    fn hat(buttons: u16, negative: u16, positive: u16) -> i32 {
        if buttons & negative != 0 { -1 } else { i32::from(buttons & positive != 0) }
    }

    /// `ff_effects_max` and the EV_FF/FFBIT registration follow UU_GAMEPAD_RUMBLE.
    #[derive(Debug, PartialEq, Eq)]
    struct PadSetup { ff_effects_max: u32, ev_ff: bool }
    fn pad_setup(rumble: bool) -> PadSetup {
        PadSetup { ff_effects_max: if rumble { 16 } else { 0 }, ev_ff: rumble }
    }

    /// Force-feedback state for one pad. Only FF_RUMBLE is forwarded; the motor mapping follows
    /// the official Windows host (RVA 0x5df5d0 builds {index,left_motor,right_motor} from ViGEm
    /// XUSB feedback): left_motor = strong (low-frequency/heavy), right_motor = weak.
    #[derive(Default)]
    struct RumbleCore { effects: std::collections::BTreeMap<i32, (u16, u16)>, active: Option<i32> }
    impl RumbleCore {
        fn uploaded(&mut self, id: i32, strong: u16, weak: u16) { self.effects.insert(id, (strong, weak)); }
        fn played(&mut self, id: i32, repeats: i32) -> Option<(u8, u8)> {
            if repeats <= 0 { return self.stopped(id); }
            let &(strong, weak) = self.effects.get(&id)?;
            self.active = Some(id);
            Some(((strong >> 8) as u8, (weak >> 8) as u8))
        }
        fn stopped(&mut self, id: i32) -> Option<(u8, u8)> {
            if self.active == Some(id) { self.active = None; Some((0, 0)) } else { None }
        }
        fn erased(&mut self, id: i32) -> Option<(u8, u8)> {
            self.effects.remove(&id);
            self.stopped(id)
        }
    }
    /// FF_RUMBLE payload of an upload; other effect types are acknowledged and not stored.
    fn upload_rumble(effect: &FfEffect) -> Option<(i32, u16, u16)> {
        if effect.type_ != FF_RUMBLE { return None; }
        // SAFETY: the kernel fills `u` for the effect type it reports; FF_RUMBLE selects `rumble`.
        let rumble = unsafe { effect.u.rumble };
        Some((i32::from(effect.id), rumble.strong_magnitude, rumble.weak_magnitude))
    }

    /// XInput pads in the four ViGEm-equivalent slots. A slot device is created on the first
    /// connect or update for its index and survives disconnect (no udev/SDL hotplug storm;
    /// the official host's missing target auto-connects too, RVA 0x5dfba0).
    pub struct Gamepads {
        pads: Mutex<[Option<Device>; 4]>,
        last: Mutex<[Option<GamepadState>; 4]>,
        readers: Mutex<[Option<(std::sync::Arc<std::sync::atomic::AtomicBool>, std::thread::JoinHandle<()>)>; 4]>,
        #[cfg_attr(test, allow(dead_code))]  // read only by the non-test reader spawn
        rumble: std::sync::Arc<super::RumbleOut>,
        ff: bool,
        #[cfg(test)]
        recorder: Option<File>,
    }

    impl Drop for Gamepads {
        fn drop(&mut self) {
            // Set every stop flag before joining; readers may sit in poll(500 ms). Take the
            // handles out first so the readers lock is not held during the joins.
            let readers = std::mem::replace(&mut *self.readers.lock().unwrap(), std::array::from_fn(|_| None));
            for (stop, _) in readers.iter().flatten() { stop.store(true, std::sync::atomic::Ordering::Release); }
            for (_, join) in readers.into_iter().flatten() { let _ = join.join(); }
        }
    }

    impl Gamepads {
        pub fn new(rumble: std::sync::Arc<super::RumbleOut>) -> std::io::Result<Self> {
            drop(OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open("/dev/uinput")?);
            Ok(Self { pads: Mutex::new([const { None }; 4]), last: Mutex::new([None; 4]),
                readers: Mutex::new([const { None }; 4]), rumble, ff: super::rumble_enabled(),
                #[cfg(test)] recorder: None })
        }
        #[cfg(test)]
        pub fn recording_for_test() -> (Self, File) {
            let path = std::env::temp_dir().join(format!("uu-uinput-gamepad-test-{}-{}", std::process::id(), rand::random::<u64>()));
            let file = OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&path).unwrap();
            std::fs::remove_file(path).unwrap();
            let reader = file.try_clone().unwrap();
            (Self { pads: Mutex::new([const { None }; 4]), last: Mutex::new([None; 4]),
                readers: Mutex::new([const { None }; 4]), rumble: Default::default(), ff: false,
                recorder: Some(file) }, reader)
        }
        fn create_pad(&self) -> std::io::Result<Device> {
            #[cfg(test)]
            if let Some(recorder) = &self.recorder { return Ok(Device(recorder.try_clone()?)); }
            let setup = pad_setup(self.ff);
            // xpad identity 045e:028e: SDL2's built-in controller DB maps it and Steam/Proton expect
            // Xbox, matching the ViGEm XUSB device the official Windows host presents.
            Device::create_id_ff("Microsoft X-Box 360 pad", 0x03 /* BUS_USB */, 0x045e, 0x028e, 0x0114, setup.ff_effects_max, |d| {
                d.ioctl(UI_SET_EVBIT, EV_KEY.into())?;
                for (_, code) in GAMEPAD_BUTTONS { d.ioctl(UI_SET_KEYBIT, code.into())?; }
                d.ioctl(UI_SET_EVBIT, EV_ABS.into())?;
                for (code, minimum, maximum, flat) in [
                    (ABS_X, -32768, 32767, 128), (ABS_Y, -32768, 32767, 128),
                    (ABS_RX, -32768, 32767, 128), (ABS_RY, -32768, 32767, 128),
                    (ABS_Z, 0, 255, 0), (ABS_RZ, 0, 255, 0),
                    (ABS_HAT0X, -1, 1, 0), (ABS_HAT0Y, -1, 1, 0),
                ] { d.abs_range(code, minimum, maximum, flat)?; }
                if setup.ev_ff {
                    d.ioctl(UI_SET_EVBIT, EV_FF.into())?;
                    d.ioctl(UI_SET_FFBIT, FF_RUMBLE.into())?;
                }
                Ok(())
            })
        }
        pub fn ensure_slot(&self, index: u8) -> std::io::Result<()> {
            let Some(i) = slot(index) else { return Ok(()) };
            let mut pads = self.pads.lock().unwrap();
            if pads[i].is_none() {
                match self.create_pad() {
                    Ok(device) => {
                        #[cfg(not(test))]
                        if self.ff {
                            let spawned = device.0.try_clone().and_then(|file| {
                                let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                                let thread_stop = stop.clone();
                                let rumble = self.rumble.clone();
                                let join = std::thread::Builder::new().name(format!("uu-gamepad-{index}"))
                                    .spawn(move || reader_loop(file, index, rumble, thread_stop))?;
                                self.readers.lock().unwrap()[i] = Some((stop, join));
                                Ok(())
                            });
                            if let Err(e) = spawned { log::warn!("Gamepad rumble reader unavailable: {}", e.kind()); }
                        }
                        pads[i] = Some(device);
                        self.last.lock().unwrap()[i] = Some(GamepadState::default());
                    }
                    Err(e) => {
                        static FAILURES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
                        if FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
                            log::warn!("Gamepad device creation failed: {}", e.kind());
                        }
                        return Err(e);
                    }
                }
            }
            Ok(())
        }
        /// Diff against the last emitted state: only button/axis changes become events, one SYN frame.
        /// Values pass through verbatim — the viewer already inverted Y and scaled the triggers.
        pub fn emit_state(&self, index: u8, state: GamepadState) {
            let Some(i) = slot(index) else { return; };
            if self.ensure_slot(index).is_err() { return; }
            let mut pads = self.pads.lock().unwrap();
            let Some(device) = pads[i].as_mut() else { return; };
            let mut last = self.last.lock().unwrap();
            let previous = last[i].unwrap_or_default();
            let mut events = Vec::new();
            for (bit, code) in GAMEPAD_BUTTONS {
                let (was, now) = (previous.buttons & bit != 0, state.buttons & bit != 0);
                if was != now { events.push((EV_KEY, code, i32::from(now))); }
            }
            for (code, was, now) in [
                (ABS_X, i32::from(previous.lx), i32::from(state.lx)),
                (ABS_Y, i32::from(previous.ly), i32::from(state.ly)),
                (ABS_RX, i32::from(previous.rx), i32::from(state.rx)),
                (ABS_RY, i32::from(previous.ry), i32::from(state.ry)),
                (ABS_Z, i32::from(previous.lt), i32::from(state.lt)),
                (ABS_RZ, i32::from(previous.rt), i32::from(state.rt)),
                (ABS_HAT0X, hat(previous.buttons, 0x0004, 0x0008), hat(state.buttons, 0x0004, 0x0008)),
                (ABS_HAT0Y, hat(previous.buttons, 0x0001, 0x0002), hat(state.buttons, 0x0001, 0x0002)),
            ] {
                if was != now { events.push((EV_ABS, code, now)); }
            }
            if events.is_empty() { last[i] = Some(state); return; }
            match device.emit(&events) {
                Ok(()) => last[i] = Some(state),
                Err(e) => {
                    static FAILURES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
                    if FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
                        log::warn!("Gamepad injection failed: {}", e.kind());
                    }
                }
            }
        }
        /// Release every mapped button and zero all axes; the cached state follows.
        pub fn neutral(&self, index: u8) {
            let Some(i) = slot(index) else { return; };
            let mut pads = self.pads.lock().unwrap();
            let mut last = self.last.lock().unwrap();
            if let Some(device) = pads[i].as_mut() {
                let mut events: Vec<(u16, u16, i32)> = GAMEPAD_BUTTONS.iter().map(|&(_, code)| (EV_KEY, code, 0)).collect();
                events.extend([
                    (EV_ABS, ABS_X, 0), (EV_ABS, ABS_Y, 0), (EV_ABS, ABS_RX, 0), (EV_ABS, ABS_RY, 0),
                    (EV_ABS, ABS_Z, 0), (EV_ABS, ABS_RZ, 0), (EV_ABS, ABS_HAT0X, 0), (EV_ABS, ABS_HAT0Y, 0),
                ]);
                if let Err(e) = device.emit(&events) {
                    static FAILURES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
                    if FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
                        log::warn!("Gamepad release failed: {}", e.kind());
                    }
                }
            }
            last[i] = Some(GamepadState::default());
        }
        pub fn sweep(&self) { for index in 0..4 { self.neutral(index); } }
    }

    /// Reads FF upload/erase requests for one pad. The fd was opened read+write for this.
    #[cfg_attr(test, allow(dead_code))]
    fn reader_loop(file: File, index: u8, rumble: std::sync::Arc<super::RumbleOut>,
                   stop: std::sync::Arc<std::sync::atomic::AtomicBool>) {
        let fd = file.as_raw_fd();
        let mut core = RumbleCore::default();
        while !stop.load(std::sync::atomic::Ordering::Acquire) {
            let mut pollfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            let ready = unsafe { libc::poll(&mut pollfd, 1, 500) };
            if ready < 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) { continue; }
                break;
            }
            if ready == 0 { continue; }
            loop {
                let mut event: InputEvent = unsafe { std::mem::zeroed() };
                let read = unsafe { libc::read(fd, (&mut event as *mut InputEvent).cast(), std::mem::size_of::<InputEvent>()) };
                if read == 0 { return; }
                if read < 0 {
                    if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) { continue; }
                    break;
                }
                if read as usize != std::mem::size_of::<InputEvent>() { break; }
                handle_ff_event(fd, index, &rumble, &mut core, event);
            }
        }
    }

    #[cfg_attr(test, allow(dead_code))]
    fn handle_ff_event(fd: std::os::unix::io::RawFd, index: u8, rumble: &super::RumbleOut,
                       core: &mut RumbleCore, event: InputEvent) {
        match (event.kind, event.code) {
            (EV_UINPUT, UI_FF_UPLOAD) => {
                let mut upload: UinputFfUpload = unsafe { std::mem::zeroed() };
                upload.request_id = event.value as u32;
                if unsafe { libc::ioctl(fd, UI_BEGIN_FF_UPLOAD, &mut upload) } < 0 { return; }
                if let Some((id, strong, weak)) = upload_rumble(&upload.effect) { core.uploaded(id, strong, weak); }
                upload.retval = 0;
                unsafe { libc::ioctl(fd, UI_END_FF_UPLOAD, &mut upload) };
            }
            (EV_UINPUT, UI_FF_ERASE) => {
                let mut erase: UinputFfErase = unsafe { std::mem::zeroed() };
                erase.request_id = event.value as u32;
                if unsafe { libc::ioctl(fd, UI_BEGIN_FF_ERASE, &mut erase) } < 0 { return; }
                if let Some((left, right)) = core.erased(erase.effect_id as i32) { rumble.send(index, left, right); }
                erase.retval = 0;
                unsafe { libc::ioctl(fd, UI_END_FF_ERASE, &mut erase) };
            }
            (EV_FF, effect) => {
                let update = if event.value > 0 { core.played(i32::from(effect as i16), event.value) } else { core.stopped(i32::from(effect as i16)) };
                if let Some((left, right)) = update { rumble.send(index, left, right); }
            }
            _ => {}
        }
    }

    #[cfg(test)]
    mod gamepad_tests {
        use super::*;

        #[test]
        fn every_xinput_bit_maps_to_a_unique_evdev_event() {
            let mut seen = std::collections::BTreeSet::new();
            for &(bit, code) in &GAMEPAD_BUTTONS {
                let (gamepads, mut file) = Gamepads::recording_for_test();
                gamepads.emit_state(0, GamepadState { buttons: bit, ..Default::default() });
                assert_eq!(Devices::recorded_events_for_test(&mut file), [(EV_KEY, code, 1), (EV_SYN, SYN_REPORT, 0)], "bit {bit:#06x}");
                assert!(seen.insert((EV_KEY, code, 1)), "duplicate button mapping for bit {bit:#06x}");
            }
            for (bit, axis, value) in [(0x0001, ABS_HAT0Y, -1), (0x0002, ABS_HAT0Y, 1), (0x0004, ABS_HAT0X, -1), (0x0008, ABS_HAT0X, 1)] {
                let (gamepads, mut file) = Gamepads::recording_for_test();
                gamepads.emit_state(0, GamepadState { buttons: bit, ..Default::default() });
                assert_eq!(Devices::recorded_events_for_test(&mut file), [(EV_ABS, axis, value), (EV_SYN, SYN_REPORT, 0)], "bit {bit:#06x}");
                assert!(seen.insert((EV_ABS, axis, value)), "duplicate dpad mapping for bit {bit:#06x}");
            }
            // 11 buttons + 4 hat directions; Guide (0x0400) included like the official XUSB report.
            assert_eq!(seen.len(), 15);
        }

        #[test]
        fn combo_buttons_and_stick_values_stay_in_one_syn_frame() {
            let (gamepads, mut file) = Gamepads::recording_for_test();
            gamepads.emit_state(0, GamepadState { buttons: 0x400F, ..Default::default() });
            assert_eq!(Devices::recorded_events_for_test(&mut file), [
                (EV_KEY, BTN_NORTH, 1), (EV_ABS, ABS_HAT0X, -1), (EV_ABS, ABS_HAT0Y, -1), (EV_SYN, SYN_REPORT, 0),
            ]);
            // ly passes through verbatim: the viewer already inverted Y.
            let (gamepads, mut file) = Gamepads::recording_for_test();
            gamepads.emit_state(0, GamepadState { lx: -32768, ly: 32767, ..Default::default() });
            assert_eq!(Devices::recorded_events_for_test(&mut file), [
                (EV_ABS, ABS_X, -32768), (EV_ABS, ABS_Y, 32767), (EV_SYN, SYN_REPORT, 0),
            ]);
        }

        #[test]
        fn repeated_updates_emit_only_transitions_and_zeroed_state_releases() {
            let (gamepads, mut file) = Gamepads::recording_for_test();
            let state = |buttons, lx| GamepadState { buttons, lx, ..Default::default() };
            gamepads.emit_state(0, state(0x1000, 0));
            gamepads.emit_state(0, state(0x1000, 0));
            gamepads.emit_state(0, state(0x3000, 120));
            gamepads.emit_state(0, state(0, 0));
            gamepads.emit_state(0, state(0, 0));
            assert_eq!(Devices::recorded_events_for_test(&mut file), [
                (EV_KEY, BTN_SOUTH, 1), (EV_SYN, SYN_REPORT, 0),
                (EV_KEY, BTN_EAST, 1), (EV_ABS, ABS_X, 120), (EV_SYN, SYN_REPORT, 0),
                (EV_KEY, BTN_SOUTH, 0), (EV_KEY, BTN_EAST, 0), (EV_ABS, ABS_X, 0), (EV_SYN, SYN_REPORT, 0),
            ]);
        }

        #[test]
        fn ff_uapi_layout_and_ioctl_numbers_match_the_kernel() {
            assert_eq!(std::mem::size_of::<FfEffect>(), 48);
            assert_eq!(std::mem::size_of::<UinputFfUpload>(), 104);
            assert_eq!(std::mem::size_of::<UinputFfErase>(), 12);
            assert_eq!(std::mem::size_of::<FfPeriodic>(), 32);
            assert_eq!(UI_SET_FFBIT, 0x4004_556b);
            assert_eq!(UI_BEGIN_FF_UPLOAD, 0xC068_55C8);
            assert_eq!(UI_END_FF_UPLOAD, 0x4068_55C9);
            assert_eq!(UI_BEGIN_FF_ERASE, 0xC00C_55CA);
            assert_eq!(UI_END_FF_ERASE, 0x400C_55CB);
            assert_eq!((EV_UINPUT, UI_FF_UPLOAD, UI_FF_ERASE), (0x0101, 1, 2));
        }

        #[test]
        fn rumble_core_forwards_rumble_bytes_and_clears_only_the_active_effect() {
            let mut core = RumbleCore::default();
            core.uploaded(7, 0xFF00, 0x8000);
            assert_eq!(core.played(7, 1), Some((0xFF, 0x80)));
            assert_eq!(core.played(7, 2), Some((0xFF, 0x80)), "repeat re-emits the same bytes");
            assert_eq!(core.played(8, 1), None, "unknown id emits nothing");
            assert_eq!(core.stopped(8), None, "stopping a non-active id emits nothing");
            assert_eq!(core.stopped(7), Some((0, 0)));
            assert_eq!(core.stopped(7), None, "a second stop is ignored");
            core.uploaded(9, 0x1000, 0x0200);
            assert_eq!(core.played(9, 1), Some((0x10, 0x02)));
            assert_eq!(core.played(9, 0), Some((0, 0)), "repeats=0 on the active id is a stop");
            assert_eq!(core.played(9, 0), None);
            assert_eq!(core.erased(9), None, "erasing a non-active id emits nothing");
            assert_eq!(core.played(9, 1), None, "erased effects are gone");
            core.uploaded(11, 0x1200, 0x3400);
            assert_eq!(core.played(11, 1), Some((0x12, 0x34)));
            assert_eq!(core.erased(11), Some((0, 0)), "erasing the active effect stops it");
            assert_eq!(core.erased(11), None);
        }

        #[test]
        fn non_rumble_uploads_are_not_stored_or_played() {
            let mut effect: FfEffect = unsafe { std::mem::zeroed() };
            effect.type_ = 0x52; // FF_CONSTANT
            effect.id = 5;
            assert_eq!(upload_rumble(&effect), None);
            effect.type_ = FF_RUMBLE;
            effect.u.rumble = FfRumble { strong_magnitude: 0x1200, weak_magnitude: 0x3400 };
            assert_eq!(upload_rumble(&effect), Some((5, 0x1200, 0x3400)));
            let mut core = RumbleCore::default();
            assert_eq!(core.played(5, 1), None, "an unuploaded id has no effect");
        }

        #[test]
        fn rumble_capability_controls_ff_slots_and_registration() {
            assert_eq!(pad_setup(false), PadSetup { ff_effects_max: 0, ev_ff: false });
            assert_eq!(pad_setup(true), PadSetup { ff_effects_max: 16, ev_ff: true });
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
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct GamepadState { pub buttons: u16, pub lt: u8, pub rt: u8, pub lx: i16, pub ly: i16, pub rx: i16, pub ry: i16 }
    pub struct Gamepads;
    impl Gamepads {
        pub fn new(_: std::sync::Arc<super::RumbleOut>) -> std::io::Result<Self> { Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "uinput requires Linux")) }
        pub fn ensure_slot(&self, _: u8) -> std::io::Result<()> { Ok(()) }
        pub fn emit_state(&self, _: u8, _: GamepadState) {}
        pub fn neutral(&self, _: u8) {}
        pub fn sweep(&self) {}
    }
}

pub use imp::{Devices, GamepadState, Gamepads, ABS_MAX, BTN_EXTRA, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, BTN_SIDE};

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
