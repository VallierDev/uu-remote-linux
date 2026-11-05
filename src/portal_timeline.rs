//! Experimental binding to pnt, NOT real Portal admission. Its process-wide
//! guard cannot establish exclusive signaling authority across processes.
use std::{ffi::c_void, os::fd::{AsRawFd, BorrowedFd, OwnedFd}, ptr, time::Duration};
extern "C" {
    fn pnt_create_protocol(out:*mut *mut c_void, drm:i32, acquire:i32, ap:u64, release:i32, rp:u64, ops:*const c_void, data:*mut c_void, protocol:i32) -> i32;
    fn pnt_wait_cancelable(ctx:*mut c_void, deadline:i64, cancel:unsafe extern "C" fn(*mut c_void)->i32,
        now:Option<unsafe extern "C" fn(*mut c_void)->i64>, data:*mut c_void) -> i32;
    fn pnt_gpu_begin(ctx:*mut c_void)->i32;
    fn pnt_gpu_complete(ctx:*mut c_void)->i32;
    fn pnt_release(ctx:*mut c_void, meta:*mut c_void)->i32;
    fn pnt_abandon_session(ctx:*mut c_void)->i32;
    fn pnt_discard_unsubmitted(ctx:*mut *mut c_void)->i32;
    fn pnt_discard_before_release(ctx:*mut *mut c_void)->i32;
    fn pnt_discard_completed(ctx:*mut *mut c_void)->i32;
    fn pnt_destroy(ctx:*mut *mut c_void)->i32;
    fn pnt_retire_proven_session(ctx:*mut *mut c_void)->i32;
}
fn status(code:i32)->Result<(),i32> { if code==0 { Ok(()) } else { Err(code) } }
pub(crate) struct Timeline { raw:*mut c_void, fds:Option<[OwnedFd;3]> }
/// Native create may import one handle and acquire the process guard before
/// failing. Keep that context recoverable; no GPU work has been submitted.
#[derive(Debug)]
pub(crate) struct CreateError { pub(crate) code:i32, pub(crate) retained:Option<Timeline> }
impl From<i32> for CreateError {
    fn from(code:i32)->Self { Self { code,retained:None } }
}
impl std::fmt::Debug for Timeline {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {
        f.debug_struct("Timeline").field("live", &!self.raw.is_null()).finish()
    }
}

impl Timeline {
    /// Caller independently establishes exclusive signal authority and lifetime
    /// for the entire producer session, including absence of higher pending
    /// signals; see portal_nvenc_timeline.h. No fork/other pnt module copy allowed.
    /// Keep the exclusive original frame lease until completion or proven teardown.
    pub(crate) unsafe fn new(render:BorrowedFd<'_>, frame:&crate::portal::AsyncFrame)->Result<Self,CreateError> {
        unsafe { Self::new_protocol(render,frame,crate::portal::ExplicitProtocol::UnscheduledRelease) }
    }
    /// Same authority/lifetime proof as new, plus the independently established
    /// producer contract must match the selected Portal session protocol.
    pub(crate) unsafe fn new_protocol(render:BorrowedFd<'_>, frame:&crate::portal::AsyncFrame,
        protocol:crate::portal::ExplicitProtocol)->Result<Self,CreateError> {
        if frame.acquire_fd<0 || frame.release_fd<0 { return Err((-libc::EINVAL).into()); }
        let clone=|fd:BorrowedFd<'_>| fd.try_clone_to_owned().map_err(|e| -e.raw_os_error().unwrap_or(libc::EIO));
        let fds=[clone(render)?,clone(unsafe { BorrowedFd::borrow_raw(frame.acquire_fd) })?,
            clone(unsafe { BorrowedFd::borrow_raw(frame.release_fd) })?];
        let mut owner=Self { raw:ptr::null_mut(),fds:None };
        let result=unsafe { pnt_create_protocol(&mut owner.raw,fds[0].as_raw_fd(),fds[1].as_raw_fd(),frame.acquire_point,
            fds[2].as_raw_fd(),frame.release_point,ptr::null(),ptr::null_mut(),protocol as i32) };
        owner.fds=Some(fds);
        if result != 0 {
            let retained=if owner.raw.is_null() { None } else { Some(owner) };
            return Err(CreateError { code:result,retained });
        }
        Ok(owner)
    }
    pub(crate) fn wait(&mut self, timeout:Duration, cancel:impl Fn()->bool)->Result<(),i32> {
        unsafe extern "C" fn cancelled<F:Fn()->bool>(data:*mut c_void)->i32 {
            let f=unsafe { &*data.cast::<F>() };
            // A polling panic cancels admission; it cannot unwind across C.
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(true) as i32
        }
        fn invoke<F:Fn()->bool>(raw:*mut c_void, deadline:i64, f:&F)->i32 {
            unsafe { pnt_wait_cancelable(raw,deadline,cancelled::<F>,None,(f as *const F).cast_mut().cast()) }
        }
        let mut now=libc::timespec { tv_sec:0,tv_nsec:0 };
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC,&mut now) }!=0 { return Err(-libc::EIO); }
        let deadline=(now.tv_sec as i128)*1_000_000_000+now.tv_nsec as i128+
            i128::try_from(timeout.as_nanos()).map_err(|_| -libc::EOVERFLOW)?;
        let deadline=i64::try_from(deadline).map_err(|_| -libc::EOVERFLOW)?;
        status(invoke(self.raw,deadline,&cancel))
    }
    /// Immediately precedes source GPU access; the caller still validates lease cancellation.
    pub(crate) unsafe fn begin(&mut self)->Result<(),i32> { status(unsafe { pnt_gpu_begin(self.raw) }) }
    /// All source GPU accesses must independently be known complete.
    pub(crate) unsafe fn complete(&mut self)->Result<(),i32> { status(unsafe { pnt_gpu_complete(self.raw) }) }
    /// Only under Portal's live metadata lock, with matching points and sole ownership.
    pub(crate) unsafe fn release(&mut self, metadata:*mut c_void)->Result<(),i32> {
        status(unsafe { pnt_release(self.raw,metadata) })
    }
    /// Requires irrevocable entire-session termination and no possible producer
    /// reuse, plus all GPU completion. A stream close/timeout alone is insufficient.
    pub(crate) unsafe fn abandon_proven_session(&mut self)->Result<(),i32> {
        status(unsafe { pnt_abandon_session(self.raw) })
    }
    /// Same independent termination/non-reuse/GPU proof as abandon_proven_session.
    /// Handles an already signaled timeline without signaling again. Retains fds
    /// and context on failure, including partial handle destruction.
    pub(crate) unsafe fn retire_proven_session(&mut self)->Result<(),i32> {
        status(unsafe { pnt_retire_proven_session(&mut self.raw) })?;
        drop(self.fds.take()); Ok(())
    }
    /// Local handle cleanup only. No source work may have been submitted outside
    /// begin/complete; the native guard rejects any past successful begin or release.
    /// The caller still owns the PipeWire lease and must not recycle it here.
    pub(crate) unsafe fn discard_unsubmitted(&mut self)->Result<(),i32> {
        status(unsafe { pnt_discard_unsubmitted(&mut self.raw) })?;
        drop(self.fds.take()); Ok(())
    }
    /// All source GPU work must be independently known complete, if begun. No
    /// release promise may have been made. Never recycle the retained lease.
    pub(crate) unsafe fn discard_before_release(&mut self)->Result<(),i32> {
        status(unsafe { pnt_discard_before_release(&mut self.raw) })?;
        drop(self.fds.take()); Ok(())
    }
    /// Permanently cease all source access and discard local references only.
    /// All GPU work must independently be complete. A previous release attempt
    /// is never retried or undone, and the lease must never be requeued here.
    pub(crate) unsafe fn discard_completed(&mut self)->Result<(),i32> {
        status(unsafe { pnt_discard_completed(&mut self.raw) })?;
        drop(self.fds.take()); Ok(())
    }
    pub(crate) fn destroy(&mut self)->Result<(),i32> {
        status(unsafe { pnt_destroy(&mut self.raw) })?;
        drop(self.fds.take()); Ok(())
    }
}
impl Drop for Timeline {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            // Never turn unknown completion or a failed transaction into cleanup.
            if let Some(fds)=self.fds.take() { std::mem::forget(fds); }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{AsFd,FromRawFd};
    #[test]
    fn invalid_timeline_descriptors_have_no_retained_context() {
        let render=std::fs::File::open("/dev/null").unwrap();
        let frame=crate::portal::AsyncFrame { acquire_fd:-1,release_fd:-1,..Default::default() };
        let error=unsafe { Timeline::new(render.as_fd(),&frame) }.unwrap_err();
        assert_eq!(error.code,-libc::EINVAL); assert!(error.retained.is_none());
    }
    #[test]
    #[ignore = "requires DRM timeline support; self-created syncobj, no Portal or GPU work"]
    fn failed_timeline_import_retains_cleanup_ownership() {
        let path=std::env::var_os("UU_TEST_RENDER_NODE").unwrap_or_else(|| "/dev/dri/renderD128".into());
        let render=std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
        let invalid=std::fs::File::open("/dev/null").unwrap();
        let mut handle=0; let mut raw=-1;
        unsafe {
            assert_eq!(drmSyncobjCreate(render.as_raw_fd(),0,&mut handle),0);
            assert_eq!(drmSyncobjHandleToFD(render.as_raw_fd(),handle,&mut raw),0);
        }
        let sync=unsafe { OwnedFd::from_raw_fd(raw) };
        for fail_first in [true,false] {
            let frame=crate::portal::AsyncFrame {
                acquire_fd:if fail_first {invalid.as_raw_fd()} else {sync.as_raw_fd()},
                release_fd:invalid.as_raw_fd(),acquire_point:1,release_point:2,..Default::default() };
            let error=unsafe { Timeline::new(render.as_fd(),&frame) }.unwrap_err();
            assert!(error.code<0);
            let mut retained=error.retained.expect("native failure must retain the process guard");
            let busy=unsafe { Timeline::new(render.as_fd(),&frame) }.unwrap_err();
            assert_eq!(busy.code,-libc::EBUSY); assert!(busy.retained.is_none());
            // Local imports only; do not assert producer/session termination.
            unsafe { retained.discard_unsubmitted().unwrap(); }
        }
        let frame=crate::portal::AsyncFrame { acquire_fd:sync.as_raw_fd(),release_fd:sync.as_raw_fd(),
            acquire_point:1,release_point:2,..Default::default() };
        let mut next=unsafe { Timeline::new(render.as_fd(),&frame) }.unwrap();
        unsafe { next.discard_unsubmitted().unwrap(); }
        let mut point=0;
        assert_eq!(unsafe { drmSyncobjQuery(render.as_raw_fd(),&handle,&mut point,1) },0);
        assert_eq!(point,0);
        drop(sync);
        assert_eq!(unsafe { drmSyncobjDestroy(render.as_raw_fd(),handle) },0);
        println!("PASS: first/second import failures retain ownership; busy context rejected; unsubmitted cleanup frees guard without signaling");
    }
    extern "C" {
        fn drmSyncobjCreate(fd:i32,flags:u32,handle:*mut u32)->i32;
        fn drmSyncobjHandleToFD(fd:i32,handle:u32,out:*mut i32)->i32;
        fn drmSyncobjTimelineSignal(fd:i32,handles:*const u32,points:*const u64,count:u32)->i32;
        fn drmSyncobjQuery(fd:i32,handles:*const u32,points:*mut u64,count:u32)->i32;
        fn drmSyncobjDestroy(fd:i32,handle:u32)->i32;
    }
    #[repr(C)]
    struct Metadata { flags:u32,padding:u32,acquire:u64,release:u64 }
    #[test]
    #[ignore = "requires DRM timeline syncobj; self-created timelines only, no Portal/GPU pixels"]
    fn rust_timeline_acquire_release_cancel_timeout() {
        let path=std::env::var_os("UU_TEST_RENDER_NODE").unwrap_or_else(|| "/dev/dri/renderD128".into());
        let fd=std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
        for protocol in [crate::portal::ExplicitProtocol::UnscheduledRelease, crate::portal::ExplicitProtocol::ReleasePoint] {
        for mode in 0..4 {
            let mut handle=0; let mut raw=-1;
            unsafe {
                assert_eq!(drmSyncobjCreate(fd.as_raw_fd(),0,&mut handle),0);
                assert_eq!(drmSyncobjHandleToFD(fd.as_raw_fd(),handle,&mut raw),0);
            }
            let sync=unsafe { OwnedFd::from_raw_fd(raw) };
            let frame=crate::portal::AsyncFrame { acquire_fd:sync.as_raw_fd(),release_fd:sync.as_raw_fd(),
                acquire_point:1,release_point:2,..Default::default() };
            // This test owns every signaler; no producer session or GPU work exists.
            let mut timeline=unsafe { Timeline::new_protocol(fd.as_fd(),&frame,protocol).unwrap() };
            let mut point=0;
            if mode==0 {
                let one=1;
                assert_eq!(unsafe { drmSyncobjTimelineSignal(fd.as_raw_fd(),&handle,&one,1) },0);
                timeline.wait(Duration::from_secs(1),|| false).unwrap();
                unsafe { timeline.begin().unwrap(); timeline.complete().unwrap(); }
                let mut metadata=Metadata { flags:if protocol==crate::portal::ExplicitProtocol::ReleasePoint {0} else {1},padding:0,acquire:1,release:2 };
                unsafe { timeline.release((&mut metadata as *mut Metadata).cast()).unwrap(); }
                assert_eq!(metadata.flags,0);
                assert_eq!(unsafe { timeline.release((&mut metadata as *mut Metadata).cast()) },Err(-libc::EINVAL));
                assert_eq!(unsafe { drmSyncobjQuery(fd.as_raw_fd(),&handle,&mut point,1) },0);
                assert_eq!(point,2);
                unsafe { timeline.discard_completed().unwrap(); }
                assert_eq!(unsafe { drmSyncobjQuery(fd.as_raw_fd(),&handle,&mut point,1) },0);
                assert_eq!(point,2); // Local disposal neither resets nor signals.
                let next=3;
                assert_eq!(unsafe { drmSyncobjTimelineSignal(fd.as_raw_fd(),&handle,&next,1) },0);
                assert_eq!(unsafe { drmSyncobjQuery(fd.as_raw_fd(),&handle,&mut point,1) },0);
                assert_eq!(point,3); // Independent producer handle remains usable.
            } else {
                let start=std::time::Instant::now();
                let result=match mode {
                    1 => timeline.wait(Duration::from_secs(1),|| start.elapsed()>=Duration::from_millis(30)),
                    2 => timeline.wait(Duration::from_millis(20),|| false),
                    _ => timeline.wait(Duration::from_secs(1),|| panic!("injected cancellation poll panic")),
                };
                assert_eq!(result,Err(if mode==2 { -libc::ETIMEDOUT } else { -libc::ECANCELED }));
                assert!(start.elapsed()<Duration::from_millis(500));
                assert_eq!(unsafe { timeline.begin() },Err(-libc::EINVAL));
                assert_eq!(unsafe { drmSyncobjQuery(fd.as_raw_fd(),&handle,&mut point,1) },0);
                assert_eq!(point,0); // Cancellation/timeout never signals release.
                unsafe { timeline.abandon_proven_session().unwrap(); }
                timeline.destroy().unwrap();
            }
            drop(timeline); drop(sync);
            assert_eq!(unsafe { drmSyncobjDestroy(fd.as_raw_fd(),handle) },0);
        }
        }
        println!("PASS: Rust real syncobj acquire/release, unsignaled cancellation, timeout and panic containment; no unexpected signals");
    }
}
