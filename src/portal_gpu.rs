//! Worker-thread-owned EGL resources. No automatic Portal selection or acquire wait.
use std::{ffi::{c_void, CStr}, os::fd::{AsRawFd, BorrowedFd, OwnedFd}, ptr, rc::Rc};
extern "C" {
    fn pnegl_device_open(out: *mut *mut c_void, render_fd: i32) -> i32;
    fn pnegl_device_close_idle(out: *mut *mut c_void, idle: i32) -> i32;
    fn pnegl_device_pci(device: *const c_void) -> *const libc::c_char;
    fn pnegl_owned_image_create(device: *mut c_void, frame: *const crate::portal::DmaFrame, out: *mut *mut c_void) -> i32;
    fn pnegl_owned_image_handle(image: *const c_void) -> *mut c_void;
    fn pnegl_owned_image_destroy_completed(image: *mut *mut c_void, complete: i32) -> i32;
}
struct Inner(*mut c_void);
impl Drop for Inner {
    fn drop(&mut self) {
        // Only Image holds child references; it retains this owner on unknown work.
        // No raw display/context API is exposed by this module.
        if !self.0.is_null() { unsafe { pnegl_device_close_idle(&mut self.0, 1); } }
    }
}
pub(crate) struct Device(Rc<Inner>);
pub(crate) struct Image {
    raw: *mut c_void,
    held: Option<(Rc<Inner>, OwnedFd)>,
}
impl Device {
    pub(crate) fn pci(&self) -> &CStr {
        // A successfully opened device owns a stable validated PCI identifier.
        unsafe { CStr::from_ptr(pnegl_device_pci(self.0.0)) }
    }
    pub(crate) fn open(fd: BorrowedFd<'_>) -> Result<Self, i32> {
        let mut owner = Inner(ptr::null_mut());
        let result = unsafe { pnegl_device_open(&mut owner.0, fd.as_raw_fd()) };
        if result != 0 { return Err(result); }
        Ok(Self(Rc::new(owner)))
    }
    /// Caller validates DMA-BUF provenance, layout/device/modifier compatibility
    /// and lease authority. A duplicate fd retains storage, not producer ownership.
    /// Retain the original lease until all GPU access and image destruction finish.
    pub(crate) unsafe fn import(&self, frame: &crate::portal::DmaFrame) -> Result<Image, i32> {
        if frame.fd < 0 { return Err(-libc::EINVAL); }
        let fd = unsafe { BorrowedFd::borrow_raw(frame.fd) }.try_clone_to_owned()
            .map_err(|e| -e.raw_os_error().unwrap_or(libc::EIO))?;
        let snapshot = crate::portal::DmaFrame { fd:fd.as_raw_fd(), fourcc:frame.fourcc,
            width:frame.width, height:frame.height, stride:frame.stride,
            offset:frame.offset, size:frame.size, modifier:frame.modifier };
        let mut raw = ptr::null_mut();
        let result = unsafe { pnegl_owned_image_create(self.0.0, &snapshot, &mut raw) };
        if result != 0 {
            if !raw.is_null() { std::mem::forget((self.0.clone(), fd)); }
            return Err(result);
        }
        Ok(Image { raw, held:Some((self.0.clone(), fd)) })
    }
}
impl Image {
    /// Borrow only through GPU completion; no independent destruction or retention
    /// past this Image's completed destroy is allowed.
    pub(crate) unsafe fn handle(&self) -> *mut c_void { unsafe { pnegl_owned_image_handle(self.raw) } }
    pub(crate) fn pci(&self) -> &CStr {
        let owner = &self.held.as_ref().expect("live image owner").0;
        // Device initialization established this stable NUL-terminated identity.
        unsafe { CStr::from_ptr(pnegl_device_pci(owner.0)) }
    }
    /// All GL/CUDA/NVENC source accesses and imported CUDA views must be finished.
    pub(crate) unsafe fn destroy_completed(mut self) -> Result<(), i32> {
        let result = unsafe { pnegl_owned_image_destroy_completed(&mut self.raw, 1) };
        if result != 0 { return Err(result); }
        drop(self.held.take());
        Ok(())
    }
}
impl Drop for Image {
    fn drop(&mut self) {
        // Dropping or unwinding is not proof of GPU completion. Retain fd + display.
        if let Some(held) = self.held.take() { std::mem::forget(held); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;
    #[test]
    fn rejects_non_render_device_without_closing_caller_fd() {
        let file = std::fs::File::open("/dev/null").unwrap();
        assert!(matches!(Device::open(file.as_fd()), Err(code) if code == -libc::ENODEV));
        assert!(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } >= 0);
    }
    #[test]
    fn unfinished_image_retains_fd_and_device() {
        let file = std::fs::File::open("/dev/null").unwrap();
        let fd = file.as_fd().try_clone_to_owned().unwrap();
        let number = fd.as_raw_fd();
        let owner = Rc::new(Inner(ptr::null_mut()));
        let weak = Rc::downgrade(&owner);
        let image = Image { raw:ptr::null_mut(), held:Some((owner.clone(), fd)) };
        drop(owner); drop(image);
        assert!(weak.upgrade().is_some());
        assert!(unsafe { libc::fcntl(number,libc::F_GETFD) } >= 0);
        // Reclaim only mock state: there was no EGL image or GPU work.
        unsafe { libc::close(number); Rc::decrement_strong_count(weak.as_ptr()); }
        assert!(weak.upgrade().is_none());
    }
    #[test]
    #[ignore = "requires an NVIDIA render node; no Portal or desktop capture"]
    fn rust_device_initialization_and_idle_close() {
        let path = std::env::var_os("UU_TEST_RENDER_NODE").unwrap_or_else(|| "/dev/dri/renderD128".into());
        let file = std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
        for _ in 0..3 { drop(Device::open(file.as_fd()).unwrap()); }
        // Own a self-created GBM BO; import/destroy only, no read of its pixels.
        extern "C" {
            fn gbm_create_device(fd: i32) -> *mut c_void;
            fn gbm_device_destroy(device: *mut c_void);
            fn gbm_bo_create(device: *mut c_void, width:u32, height:u32, format:u32, flags:u32) -> *mut c_void;
            fn gbm_bo_get_fd(bo:*mut c_void) -> i32;
            fn gbm_bo_get_stride(bo:*mut c_void) -> u32;
            fn gbm_bo_get_modifier(bo:*mut c_void) -> u64;
            fn gbm_bo_destroy(bo:*mut c_void);
        }
        use std::os::fd::FromRawFd;
        unsafe {
            let gbm=gbm_create_device(file.as_raw_fd()); assert!(!gbm.is_null());
            let bo=gbm_bo_create(gbm,256,256,0x34325241,4); assert!(!bo.is_null());
            let raw=gbm_bo_get_fd(bo); assert!(raw>=0);
            let pixels=std::fs::File::from_raw_fd(raw);
            let frame=crate::portal::DmaFrame { fd:raw, fourcc:0x34325241, width:256,height:256,
                stride:gbm_bo_get_stride(bo), offset:0, size:u32::try_from(pixels.metadata().unwrap().len()).unwrap(),
                modifier:gbm_bo_get_modifier(bo) };
            let device=Device::open(file.as_fd()).unwrap();
            let image=device.import(&frame).unwrap();
            assert!(!image.handle().is_null()); assert!(!image.pci().to_bytes().is_empty());
            drop(device); // The image must retain its display through destruction.
            image.destroy_completed().unwrap();
            drop(pixels); gbm_bo_destroy(bo); gbm_device_destroy(gbm);
        }
        println!("PASS: Rust GBM DMA-BUF image import, parent retention and completed destruction");
        assert!(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } >= 0);
        println!("PASS: Rust NVIDIA EGL device ownership, three initialize/idle-close cycles");
    }
}
