// Included in portal::native. Experimental ownership bridge; no automatic activation.
#[repr(C)]
#[derive(Default)]
pub(crate) struct AsyncFrame {
    pub pixels: super::DmaFrame,
    pub acquire_fd: i32,
    pub release_fd: i32,
    pub acquire_point: u64,
    pub release_point: u64,
    pub epoch: u64,
}
type Release = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32;
#[derive(Clone, Copy)]
struct LeaseOps {
    cancelled: unsafe fn(*const c_void) -> i32,
    source_cancelled: unsafe fn(*mut c_void, u64) -> i32,
    finish: unsafe fn(*mut *mut c_void, i32, Release, *mut c_void) -> i32,
    abandon: unsafe fn(*mut *mut c_void, i32) -> i32,
    end: unsafe fn(*mut c_void),
}
// O retains the exact native session. Generic only to test ownership without D-Bus.
pub(crate) struct AsyncLease<O> {
    owner: Option<O>,
    capture_access: Option<std::sync::Arc<tokio::sync::OwnedRwLockWriteGuard<()>>>,
    portal: *mut c_void,
    lease: *mut c_void,
    frame: AsyncFrame,
    ops: LeaseOps,
    attempted: bool,
}
// Move one lease to one worker; never share concurrent finish/abandon calls.
unsafe impl<O: Send> Send for AsyncLease<O> {}
impl AsyncLease<std::sync::Arc<NativeHandle>> {
    pub(crate) fn belongs_to(&self, source: &ExplicitSession) -> bool {
        self.owner.as_ref().is_some_and(|owner| std::sync::Arc::ptr_eq(owner, &source.0))
    }
    /// Obtain before consuming the lease. Retains the source, not the freed lease.
    pub(crate) fn publication_guard(&self) -> super::FrameValidity {
        let owner = self.owner.as_ref().expect("live lease owner").clone();
        let epoch = self.frame.epoch;
        super::FrameValidity::new(move || unsafe {
            uu_portal_capture_cancelled(owner.as_ptr(), epoch) != 0
        })
    }
}
impl<O> AsyncLease<O> {
    pub(crate) fn frame(&self) -> &AsyncFrame { &self.frame }
    pub(crate) fn cancelled(&self) -> bool {
        self.lease.is_null() || unsafe { (self.ops.cancelled)(self.lease) != 0 }
    }
    /// All GPU reads must have completed; release must not unwind or reenter Portal.
    /// Its opaque pointer must stay valid through the call and its metadata update
    /// and signal must obey the C explicit-sync contract. A failure is never retried.
    pub(crate) unsafe fn finish_completed(&mut self, release: Release, data: *mut c_void) -> Result<(), i32> {
        if self.attempted || self.lease.is_null() { return Err(-libc::EINVAL); }
        self.attempted = true;
        let result = unsafe { (self.ops.finish)(&mut self.lease, 1, release, data) };
        if result == 0 { Ok(()) } else { Err(result) }
    }
    /// The worker must have stopped all source access, including on error. Ends
    /// this exact session, never a replacement, before discarding the lease.
    pub(crate) unsafe fn abandon_completed(&mut self) -> Result<(), i32> {
        if self.lease.is_null() { return Err(-libc::EINVAL); }
        unsafe { (self.ops.end)(self.portal) };
        let result = unsafe { (self.ops.abandon)(&mut self.lease, 1) };
        if result == 0 { Ok(()) } else { Err(result) }
    }
}
impl<O> Drop for AsyncLease<O> {
    fn drop(&mut self) {
        if !self.lease.is_null() {
            // Panic/unknown GPU completion is not permission to release a buffer.
            // Retention is deliberate; it does not prove process-exit isolation.
            if let Some(owner) = self.owner.take() { std::mem::forget(owner); }
            if let Some(access)=self.capture_access.take() {std::mem::forget(access);}
        }
    }
}
// The hardware adapter must attest that ALL GPU work is complete and the
// backend is safe to destroy before returning Complete, including on errors.
pub(crate) enum GpuFrameResult {
    Complete(Result<Vec<u8>, i32>),
    Unknown(i32),
}
pub(crate) enum WorkerFrame<O, B, P = Vec<u8>> {
    // Err here means successful release followed by observed cancellation:
    // no lease remains to abandon. The idle backend may be destroyed normally.
    Complete { backend: B, packet: Result<P, i32> },
    NeedsTeardown { backend: B, lease: AsyncLease<O>, code: i32 },
    Quarantined(i32),
}
/// Execute on the encoder's owning thread. The backend must initially be idle.
/// encode must honor acquire synchronization before GPU access, poll cancelled,
/// and truthfully report completion. release receives a pointer to B and live SPA
/// metadata; it must not unwind, reenter Portal, or submit GPU work. Complete
/// output owns its bytes. Unknown completion retains both backend and lease.
/// GPU objects must be owned by backend or otherwise retained during unwinding;
/// this function cannot prevent destruction of encode's stack-local GPU guards.
/// Known-completed errors return NeedsTeardown; no session termination or timeline
/// abandonment is inferred here. This supplies no producer proof.
pub(crate) unsafe fn run_explicit_frame<O, B>(lease: AsyncLease<O>, backend: B,
    encode: impl FnOnce(&mut B, &AsyncFrame, &dyn Fn() -> bool) -> GpuFrameResult,
    release: Release,
) -> WorkerFrame<O, B> {
    unsafe { run_explicit_frame_cancelable(lease,backend,encode,release,|| false) }
}
/// Same source/completion contract as run_explicit_frame. The additional stop
/// predicate must be nonblocking and must not reenter Portal. Panic cancels;
/// cancellation never substitutes for GPU completion or resource retirement.
pub(crate) unsafe fn run_explicit_frame_cancelable<O,B>(mut lease:AsyncLease<O>, backend:B,
    encode:impl FnOnce(&mut B,&AsyncFrame,&dyn Fn()->bool)->GpuFrameResult,
    release:Release, stopped:impl Fn()->bool) -> WorkerFrame<O,B> {
    let stopped=|| std::panic::catch_unwind(std::panic::AssertUnwindSafe(&stopped)).unwrap_or(true);
    if lease.attempted { return WorkerFrame::NeedsTeardown { backend, lease, code: -libc::EINVAL }; }
    let mut backend = std::mem::ManuallyDrop::new(backend);
    let encoded = if lease.cancelled() || stopped() {
        GpuFrameResult::Complete(Err(-libc::ECANCELED))
    } else {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            encode(&mut backend, lease.frame(), &|| lease.cancelled() || stopped())
        })) {
            Ok(result) => result,
            Err(_) => GpuFrameResult::Unknown(-libc::EIO),
        }
    };
    let mut packet = match encoded {
        GpuFrameResult::Unknown(code) => return WorkerFrame::Quarantined(code),
        GpuFrameResult::Complete(packet) => packet,
    };
    // A frame revoked while encoding must not be published or returned as live.
    if lease.cancelled() || stopped() { packet = Err(-libc::ECANCELED); }
    if packet.is_ok() {
        if let Err(code) = unsafe { lease.finish_completed(release, (&mut *backend as *mut B).cast()) } {
            packet = Err(code);
        }
    }
    if let Err(code) = packet {
        // GPU completion alone does not prove producer session termination or
        // permit timeline abandonment. Preserve both owners for coordinated teardown.
        return WorkerFrame::NeedsTeardown {
            backend: std::mem::ManuallyDrop::into_inner(backend), lease, code,
        };
    }
    // finish frees the native lease, so do not query cancelled() on it again.
    // The retained owner keeps the exact source alive through this epoch check.
    // Cancellation after this check must still be handled by the publication layer;
    // this does not make queueing a packet atomic with compositor revocation.
    if stopped() || unsafe { (lease.ops.source_cancelled)(lease.portal, lease.frame.epoch) } != 0 {
        packet = Err(-libc::ECANCELED);
    }
    // Only the known-completed path may return ownership to the encoder loop.
    WorkerFrame::Complete { backend: std::mem::ManuallyDrop::into_inner(backend), packet }
}

// Identity survives failed negotiation and prevents stale worker recovery from
// replacing or ending a newer monitor/session.
pub(crate) struct ExplicitSession(std::sync::Arc<NativeHandle>, pub(crate) super::ExplicitProtocol,
    Option<std::sync::Arc<tokio::sync::OwnedRwLockWriteGuard<()>>>);
fn install_if_current<T>(current: &Mutex<T>, replacement: T,
    matches: impl FnOnce(&T) -> bool, cancelled: &std::sync::atomic::AtomicBool,
) -> Result<bool, super::PortalError> {
    let mut guard = current.lock().map_err(|_| super::PortalError { code: 3 })?;
    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
        drop(guard); return Err(super::PortalError { code: 13 });
    }
    if !matches(&guard) { drop(guard); return Ok(false); }
    let old = std::mem::replace(&mut *guard, replacement);
    drop(guard); drop(old); Ok(true)
}
// Admission for a fresh SHM attempt. Back off failed Close calls as well as
// failed opens; the callback only acknowledges the Portal control plane.
fn close_for_shm_recovery(recovery: &mut Recovery, now: impl Fn() -> std::time::Instant,
    source_status: impl FnOnce() -> i32, close: impl FnOnce() -> i32,
) -> Result<(), super::PortalError> {
    if source_status() != 0 { return Err(super::PortalError { code: 13 }); }
    if recovery.denied || recovery.failed_until.is_some_and(|until| now() < until) {
        return Err(super::PortalError { code: 3 });
    }
    if close() != 0 {
        // Measure after the synchronous call: its timeout can consume seconds.
        recovery.failed(now(), 3);
        return Err(super::PortalError { code: 3 });
    }
    // Only successful replacement clears prior recovery state.
    Ok(())
}
impl Session {
    /// Caller must establish producer/device/modifier/lifetime compatibility and
    /// exclusive capture ownership. Returns source identity plus the native request
    /// status (including failure); outer Err means no request was made. Native
    /// failure requires a fresh SHM session.
    pub(crate) unsafe fn request_explicit(&self, modifier: u64) -> Result<(ExplicitSession, i32), i32> {
        unsafe { self.request_explicit_protocol(modifier, super::ExplicitProtocol::UnscheduledRelease) }
    }
    /// ReleasePoint requires a producer that always waits for release before reuse.
    /// Never select it by inspecting a frame's flags alone.
    pub(crate) unsafe fn request_explicit_protocol(&self, modifier:u64, protocol:super::ExplicitProtocol)
        -> Result<(ExplicitSession, i32), i32> {
        let source=self.reserve_explicit(protocol)?;
        let result=unsafe {self.configure_explicit(&source,modifier)};
        Ok((source,result))
    }
    pub(crate) fn reserve_explicit(&self,protocol:super::ExplicitProtocol)->Result<ExplicitSession,i32> {
        let access=std::sync::Arc::new(self.capture_access.clone().try_write_owned().map_err(|_| -libc::EBUSY)?);
        let handle=self.handle.lock().map_err(|_| -libc::EIO)?;
        Ok(ExplicitSession(handle.0.clone(),protocol,Some(access)))
    }
    /// Same reviewed-producer contract as request_explicit_protocol. The caller
    /// retains the exclusive reservation across initialization and recovery.
    pub(crate) unsafe fn configure_explicit(&self,source:&ExplicitSession,modifier:u64)->i32 {
        let Ok(handle)=self.handle.lock() else {return -libc::EIO;};
        if !std::sync::Arc::ptr_eq(&handle.0,&source.0) || source.2.is_none() {return -libc::ECANCELED;}
        unsafe {uu_portal_request_explicit_protocol(handle.0.as_ptr(),modifier,source.1 as i32)}
    }
    /// After all workers have joined and every lease has finished/been abandoned,
    /// end the failed explicit session and open a new SHM session. Unknown GPU
    /// completion must never enter this recovery path. False means superseded.
    pub(crate) unsafe fn fallback_explicit_to_shm(&self, source: &ExplicitSession,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<bool, super::PortalError> {
        #[cfg(all(test,feature = "inproc-av"))]
        if std::env::var("UU_TEST_PORTAL_EPHEMERAL_RECOVERY").as_deref()==Ok("1") {
            return unsafe {self.fallback_explicit_to_shm_using(source,cancelled,|_,virtual_output| {
                assert!(!virtual_output);
                tests::open_worker_test_session(self.locked.clone())
            })};
        }
        unsafe { self.fallback_explicit_to_shm_using(source, cancelled, |monitor, virtual_output| {
            Self::open_inner(Some(cancelled), monitor, false, virtual_output)
        }) }
    }
    // Same recovery transaction; diagnostic callers can isolate consent storage
    // by providing a real ephemeral opener instead of the persisted opener.
    pub(crate) unsafe fn fallback_explicit_to_shm_using(&self, source: &ExplicitSession,
        cancelled: &std::sync::atomic::AtomicBool,
        open: impl FnOnce(Option<&str>, bool) -> Result<Self, super::PortalError>,
    ) -> Result<bool, super::PortalError> {
        use std::sync::atomic::Ordering;
        let mut recovery = loop {
            if cancelled.load(Ordering::Acquire) { return Err(super::PortalError { code: 13 }); }
            match self.recovery.try_lock() {
                Ok(guard) => break guard,
                Err(TryLockError::WouldBlock) => std::thread::sleep(std::time::Duration::from_millis(20)),
                Err(TryLockError::Poisoned(_)) => return Err(super::PortalError { code: 3 }),
            }
        };
        {
            let handle = self.handle.lock().map_err(|_| super::PortalError { code: 3 })?;
            if !std::sync::Arc::ptr_eq(&handle.0, &source.0) { return Ok(false); }
        }
        // Cleanup must not erase an already observed external Closed event and
        // turn revoked sharing into a new authorization prompt. A missing plugin
        // symbol is also nonzero and fails closed here.
        // End only the retained source; a concurrent monitor switch is independent.
        // A local cancellation alone must not admit a replacement session. This
        // acknowledgment still is NOT the proof required to abandon a timeline.
        close_for_shm_recovery(&mut recovery, std::time::Instant::now,
            || unsafe { uu_portal_recovery_status(source.0.as_ptr()) },
            || unsafe { uu_portal_end_checked(source.0.as_ptr()) })?;
        // Retained source guards must not keep the old consumer link connected
        // while a replacement negotiates its CPU-only format. Preconditions above
        // guarantee workers and leases are gone; this is not a GPU wait/barrier.
        let disconnected = unsafe { uu_portal_disconnect_capture(source.0.as_ptr()) };
        if disconnected != 0 {
            recovery.failed(std::time::Instant::now(), 9);
            return Err(super::PortalError { code: 9 });
        }
        if cancelled.load(Ordering::Acquire) || *self.locked.borrow() != Some(false) ||
            unsafe { uu_portal_recovery_status(source.0.as_ptr()) } != 0 {
            return Err(super::PortalError { code: 13 });
        }
        let monitor = self.monitor();
        let virtual_output = self.is_virtual();
        let result = (|| {
            let replacement = open(monitor.as_deref(), virtual_output)?;
            self.check_geometry(&replacement)?;
            if replacement.monitor() != monitor { return Err(super::PortalError { code: 14 }); }
            if *self.locked.borrow() != Some(false) || self.was_locked.load(Ordering::Acquire) ||
                unsafe { uu_portal_recovery_status(source.0.as_ptr()) } != 0 {
                return Err(super::PortalError { code: 13 });
            }
            let next = replacement.handle.into_inner().map_err(|_| super::PortalError { code: 3 })?;
            install_if_current(&self.handle, next,
                |handle| std::sync::Arc::ptr_eq(&handle.0, &source.0), cancelled)
        })();
        if result.is_err() {
            let handle = self.handle.lock().map_err(|_| super::PortalError { code: 3 })?;
            if !std::sync::Arc::ptr_eq(&handle.0, &source.0) { return Ok(false); }
        }
        match result {
            Ok(true) => {
                recovery.succeeded();
                log::info!("Portal explicit transport replaced by a fresh SHM session");
            }
            Err(error) if error.code != 13 => recovery.failed(std::time::Instant::now(), error.code),
            _ => (),
        }
        result
    }
    /// No GPU work may use returned fds past successful finish/abandon. Descriptor
    /// values are borrowed from the C lease; do not close them independently.
    pub(crate) unsafe fn acquire_explicit(&self, source: &ExplicitSession) -> Result<AsyncLease<std::sync::Arc<NativeHandle>>, i32> {
        let handle = self.handle.lock().map_err(|_| -libc::EIO)?;
        if !std::sync::Arc::ptr_eq(&handle.0, &source.0) { return Err(-libc::ECANCELED); }
        let access=source.2.as_ref().ok_or(-libc::EINVAL)?;
        let mut lease=unsafe {acquire_native_lease(handle.0.clone())}?;
        lease.capture_access=Some(access.clone());
        Ok(lease)
    }
}
// Caller serializes acquisition and retains the exact original native owner.
unsafe fn acquire_native_lease(owner: std::sync::Arc<NativeHandle>)
    -> Result<AsyncLease<std::sync::Arc<NativeHandle>>, i32> {
    let mut lease = std::ptr::null_mut();
    let mut frame = AsyncFrame::default();
    let result = unsafe { uu_portal_async_acquire(owner.as_ptr(), &mut lease, &mut frame) };
    if result != 0 { return Err(result); }
    Ok(AsyncLease {
        owner: Some(owner.clone()), capture_access:None, portal: owner.as_ptr(), lease, frame, attempted: false,
        ops: LeaseOps {
            cancelled: |p| unsafe { uu_portal_async_cancelled(p) },
            source_cancelled: |p, epoch| unsafe { uu_portal_capture_cancelled(p, epoch) },
            finish: |p, c, r, d| unsafe { uu_portal_async_finish(p, c, r, d) },
            abandon: |p, c| unsafe { uu_portal_async_abandon(p, c) },
            end: |p| unsafe { uu_portal_end(p) },
        },
    })
}

#[cfg(test)]
mod async_ownership_tests {
    use super::*;
    use std::sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst}};
    #[test]
    fn explicit_reservation_follows_live_lease_and_unknown_retention() {
        for completed in [true,false] {
            let access=Arc::new(tokio::sync::RwLock::new(()));
            let source=Arc::new(access.clone().try_write_owned().unwrap());
            let (mut lease,_owner)=fixture();
            lease.capture_access=Some(source.clone());
            drop(source); // even early source-token drop must not admit CPU use
            assert!(access.try_read().is_err());
            if completed {unsafe {lease.finish_completed(release,std::ptr::null_mut()).unwrap();}}
            drop(lease);
            assert_eq!(access.try_read().is_ok(),completed);
            assert_eq!(access.clone().try_write_owned().is_ok(),completed);
            // The unknown case deliberately retains its synthetic reservation.
        }
    }

    #[derive(Default)]
    struct State { ended: AtomicBool, finishes: AtomicUsize, abandons: AtomicUsize, fail: AtomicBool,
        epoch: std::sync::atomic::AtomicU64 }
    unsafe fn state<'a>(p: *const c_void) -> &'a State { unsafe { &*p.cast::<State>() } }
    unsafe extern "C" fn release(_: *mut c_void, _: *mut c_void) -> i32 { 0 }
    fn fixture() -> (AsyncLease<Arc<State>>, Arc<State>) {
        let owner = Arc::new(State::default());
        let pointer = Arc::as_ptr(&owner) as *mut c_void;
        (AsyncLease { owner: Some(owner.clone()), capture_access:None, portal: pointer, lease: pointer,
            frame: AsyncFrame::default(), attempted: false,
            ops: LeaseOps {
                cancelled: |p| unsafe { state(p).ended.load(SeqCst) as i32 },
                source_cancelled: |p, epoch| unsafe {
                    (state(p).ended.load(SeqCst) || state(p).epoch.load(SeqCst) != epoch) as i32
                },
                end: |p| unsafe { state(p).ended.store(true, SeqCst) },
                finish: |p, complete, release, data| unsafe {
                    assert_eq!(complete, 1);
                    let s = state(*p); s.finishes.fetch_add(1, SeqCst);
                    if s.fail.load(SeqCst) { return -libc::EIO; }
                    let result = release(data, std::ptr::null_mut());
                    if result != 0 { return result; }
                    *p = std::ptr::null_mut(); 0
                },
                abandon: |p, complete| unsafe {
                    let s = state(*p); assert!(s.ended.load(SeqCst)); assert_eq!(complete, 1);
                    s.abandons.fetch_add(1, SeqCst); *p = std::ptr::null_mut(); 0
                },
            },
        }, owner)
    }
    #[test]
    fn external_revoke_or_missing_plugin_blocks_recovery_before_close() {
        for status in [-libc::ECANCELED,-1] {
            let mut recovery=Recovery::default();
            let result=close_for_shm_recovery(&mut recovery,std::time::Instant::now,|| status,
                || panic!("revoked or unsupported source must not enter recovery Close"));
            assert!(matches!(result,Err(error) if error.code==13));
            assert!(recovery.failed_until.is_none());
            assert!(!recovery.denied); // cancellation is not an encoder failure/backoff
        }
    }
    #[test]
    fn failed_close_backs_off_before_any_replacement_attempt() {
        let start=std::time::Instant::now();
        let now=std::cell::Cell::new(start);
        let mut recovery=Recovery::default();
        assert!(close_for_shm_recovery(&mut recovery, || now.get(), || 0, || {
            now.set(start+std::time::Duration::from_secs(3)); -libc::EIO
        }).is_err());
        assert_eq!(recovery.failed_until,Some(start+std::time::Duration::from_secs(8)));
        assert!(!recovery.denied);
        now.set(start+std::time::Duration::from_secs(7));
        assert!(close_for_shm_recovery(&mut recovery, || now.get(), || 0, || panic!("must back off")).is_err());
        now.set(start+std::time::Duration::from_secs(8));
        assert!(close_for_shm_recovery(&mut recovery, || now.get(), || 0, || 0).is_ok());
        assert!(recovery.failed_until.is_some()); // Close is not successful replacement.
        recovery.denied=true;
        assert!(close_for_shm_recovery(&mut recovery, || now.get(), || 0, || panic!("denied session")).is_err());
    }
    struct Backend<'a>(&'a AtomicUsize);
    impl Drop for Backend<'_> { fn drop(&mut self) { self.0.fetch_add(1, SeqCst); } }
    #[test]
    fn worker_reuses_backend_and_passes_it_to_release() {
        unsafe extern "C" fn count_release(data: *mut c_void, _: *mut c_void) -> i32 {
            unsafe { *data.cast::<u32>() += 1; } 0
        }
        let mut backend = 0u32;
        for expected in 1..=2 {
            let (lease, _) = fixture();
            let result = unsafe { run_explicit_frame(lease, backend, |_, _, _|
                GpuFrameResult::Complete(Ok(vec![1])), count_release) };
            match result {
                WorkerFrame::Complete { backend: next, packet: Ok(_) } => backend = next,
                _ => panic!("completion failed"),
            }
            assert_eq!(backend, expected);
        }
    }
    #[test]
    fn worker_returns_packet_only_after_lease_finish() {
        let (lease, owner) = fixture(); let drops = AtomicUsize::new(0);
        let result = unsafe { run_explicit_frame(lease, Backend(&drops), |_, _, cancelled| {
            assert!(!cancelled()); GpuFrameResult::Complete(Ok(vec![1,2,3]))
        }, release) };
        match result {
            WorkerFrame::Complete { backend, packet } => {
                assert_eq!(packet.unwrap(), vec![1,2,3]); assert_eq!(owner.finishes.load(SeqCst), 1);
                assert_eq!(drops.load(SeqCst), 0); drop(backend);
            }
            _ => panic!("unexpected quarantine"),
        }
        assert_eq!(drops.load(SeqCst), 1); assert_eq!(Arc::strong_count(&owner), 1);
    }
    #[test]
    fn worker_discards_packet_if_source_changes_during_successful_finish() {
        unsafe extern "C" fn revoke(data: *mut c_void, _: *mut c_void) -> i32 {
            let (source, close) = unsafe { &*data.cast::<(Arc<State>, bool)>() };
            if *close { source.ended.store(true, SeqCst); }
            else { source.epoch.fetch_add(1, SeqCst); }
            0
        }
        for close in [false, true] {
            let (mut lease, owner) = fixture();
            lease.frame.epoch = 7;
            owner.epoch.store(7, SeqCst);
            let result = unsafe { run_explicit_frame(lease, (owner.clone(), close),
                |_, _, _| GpuFrameResult::Complete(Ok(vec![1, 2, 3])), revoke) };
            match result {
                WorkerFrame::Complete { backend, packet } => {
                    assert_eq!(packet, Err(-libc::ECANCELED));
                    drop(backend);
                }
                _ => panic!("already released lease must not enter teardown"),
            }
            assert_eq!(owner.finishes.load(SeqCst), 1);
            assert_eq!(owner.abandons.load(SeqCst), 0);
            assert_eq!(Arc::strong_count(&owner), 1);
        }
    }
    #[test]
    fn task_stop_before_during_and_after_finish_preserves_ownership() {
        for stage in 0..3 {
            let (lease,owner)=fixture(); let drops=AtomicUsize::new(0);
            let stopped=AtomicBool::new(stage==0);
            let result=unsafe {run_explicit_frame_cancelable(lease,Backend(&drops),|_,_,cancelled| {
                assert_ne!(stage,0);
                if stage==1 { stopped.store(true,SeqCst); assert!(cancelled()); }
                GpuFrameResult::Complete(Ok(vec![9]))
            },release,|| stopped.load(SeqCst) || (stage==2 && owner.finishes.load(SeqCst)>0))};
            assert!(!owner.ended.load(SeqCst)); // task stop is not Portal revocation
            match result {
                WorkerFrame::NeedsTeardown {backend,mut lease,code}=>{
                    assert!(stage<2); assert_eq!(code,-libc::ECANCELED);
                    assert_eq!(owner.finishes.load(SeqCst),0);
                    unsafe {lease.abandon_completed().unwrap();} drop(backend);
                }
                WorkerFrame::Complete {backend,packet}=>{
                    assert_eq!(stage,2); assert_eq!(packet,Err(-libc::ECANCELED));
                    assert_eq!(owner.finishes.load(SeqCst),1);
                    assert_eq!(owner.abandons.load(SeqCst),0); drop(backend);
                }
                _=>panic!("known-completed task stop must not quarantine"),
            }
            assert_eq!(drops.load(SeqCst),1);
        }
    }
    #[test]
    fn worker_cancellation_before_or_during_encode_discards_packet() {
        for before in [true, false] {
            let (lease, owner) = fixture(); let drops = AtomicUsize::new(0);
            owner.ended.store(before, SeqCst);
            let result = unsafe { run_explicit_frame(lease, Backend(&drops), |_, _, cancelled| {
                assert!(!before); owner.ended.store(true, SeqCst); assert!(cancelled());
                GpuFrameResult::Complete(Ok(vec![9]))
            }, release) };
            assert_eq!(owner.finishes.load(SeqCst), 0); assert_eq!(owner.abandons.load(SeqCst), 0);
            match result {
                WorkerFrame::NeedsTeardown { backend, mut lease, code } => {
                    assert_eq!(code,-libc::ECANCELED);
                    // Mock-only: no producer or GPU work remains in this fixture.
                    unsafe { lease.abandon_completed().unwrap(); }
                    drop(backend);
                }
                _ => panic!("expected retained teardown ownership"),
            }
            assert_eq!(drops.load(SeqCst), 1);
        }
    }
    #[test]
    fn worker_known_failure_requires_explicit_teardown() {
        for encode_failed in [true, false] {
            let (lease, owner) = fixture(); let drops = AtomicUsize::new(0);
            owner.fail.store(!encode_failed, SeqCst);
            let result = unsafe { run_explicit_frame(lease, Backend(&drops), |_, _, _| {
                GpuFrameResult::Complete(if encode_failed { Err(-libc::EIO) } else { Ok(vec![9]) })
            }, release) };
            assert_eq!(owner.finishes.load(SeqCst), usize::from(!encode_failed));
            assert_eq!(owner.abandons.load(SeqCst), 0); assert!(!owner.ended.load(SeqCst));
            match result {
                WorkerFrame::NeedsTeardown { backend, mut lease, code } => {
                    assert_eq!(code,-libc::EIO);
                    unsafe { lease.abandon_completed().unwrap(); }
                    drop(backend);
                }
                _ => panic!("expected retained teardown ownership"),
            }
            assert_eq!(drops.load(SeqCst), 1);
        }
    }
    #[test]
    fn attempted_release_never_reenters_gpu_dispatch() {
        let (mut lease,owner)=fixture(); owner.fail.store(true,SeqCst);
        assert_eq!(unsafe { lease.finish_completed(release,std::ptr::null_mut()) },Err(-libc::EIO));
        let outcome=unsafe { run_explicit_frame(lease,(),|_,_,_| panic!("must not encode again"),release) };
        match outcome {
            WorkerFrame::NeedsTeardown { mut lease,code,.. } => {
                assert_eq!(code,-libc::EINVAL); assert_eq!(owner.finishes.load(SeqCst),1);
                assert_eq!(owner.abandons.load(SeqCst),0);
                unsafe { lease.abandon_completed().unwrap(); }
            }
            _ => panic!("expected teardown"),
        }
    }
    #[test]
    fn worker_unknown_or_panic_retains_backend_and_lease() {
        for panic in [false, true] {
            let (lease, owner) = fixture(); let drops = AtomicUsize::new(0);
            let result = unsafe { run_explicit_frame(lease, Backend(&drops), |_, _, _| {
                if panic { panic!("injected worker panic"); }
                GpuFrameResult::Unknown(-libc::EIO)
            }, release) };
            assert!(matches!(result, WorkerFrame::Quarantined(code) if code == -libc::EIO));
            assert_eq!(drops.load(SeqCst), 0); assert_eq!(owner.finishes.load(SeqCst), 0);
            assert_eq!(owner.abandons.load(SeqCst), 0); assert!(!owner.ended.load(SeqCst));
            assert_eq!(Arc::strong_count(&owner), 2);
            unsafe { Arc::decrement_strong_count(Arc::as_ptr(&owner)); }
        }
    }
    #[test]
    fn fallback_rejects_stale_source_and_preserves_new_session() {
        let current = Mutex::new(2);
        assert!(!install_if_current(&current, 3, |v| *v == 1, &AtomicBool::new(false)).unwrap());
        assert_eq!(*current.lock().unwrap(), 2);
        assert!(install_if_current(&current, 3, |v| *v == 2, &AtomicBool::new(false)).unwrap());
        assert_eq!(*current.lock().unwrap(), 3);
    }
    #[test]
    fn fallback_cancelled_after_open_does_not_install() {
        let current = Mutex::new(1);
        assert_eq!(install_if_current(&current, 2, |v| *v == 1, &AtomicBool::new(true)),
            Err(super::super::PortalError { code: 13 }));
        assert_eq!(*current.lock().unwrap(), 1);
    }
    #[test]
    fn fallback_destroys_old_and_rejected_handles_outside_input_lock() {
        struct Checked { id: u32, current: std::sync::Weak<Mutex<Option<Checked>>> }
        impl Drop for Checked {
            fn drop(&mut self) {
                if let Some(current) = self.current.upgrade() { assert!(current.try_lock().is_ok()); }
            }
        }
        let current = Arc::new(Mutex::new(None));
        let make = |id| Some(Checked { id, current: Arc::downgrade(&current) });
        *current.lock().unwrap() = make(1);
        let cancelled = AtomicBool::new(false);
        assert!(install_if_current(&current, make(2), |v| v.as_ref().unwrap().id == 1, &cancelled).unwrap());
        assert!(!install_if_current(&current, make(3), |v| v.as_ref().unwrap().id == 1, &cancelled).unwrap());
        cancelled.store(true, SeqCst);
        assert!(install_if_current(&current, make(4), |_| true, &cancelled).is_err());
    }
    #[test]
    fn async_frame_matches_c_abi() {
        assert_eq!(std::mem::size_of::<AsyncFrame>(), 72);
        assert_eq!(std::mem::offset_of!(AsyncFrame, acquire_fd), 40);
        assert_eq!(std::mem::offset_of!(AsyncFrame, acquire_point), 48);
        assert_eq!(std::mem::offset_of!(AsyncFrame, epoch), 64);
    }
    #[test]
    fn lease_retains_original_owner_across_worker_move() {
        let (mut lease, owner) = fixture(); let weak = Arc::downgrade(&owner);
        drop(owner);
        let lease = std::thread::spawn(move || {
            assert!(!lease.cancelled());
            unsafe { lease.finish_completed(release, std::ptr::null_mut()).unwrap(); }
            lease
        }).join().unwrap();
        assert!(weak.upgrade().is_some()); drop(lease); assert!(weak.upgrade().is_none());
    }
    #[test]
    fn failed_finish_cannot_retry_and_abandon_ends_exact_owner() {
        let (mut lease, owner) = fixture(); owner.fail.store(true, SeqCst);
        unsafe {
            assert_eq!(lease.finish_completed(release, std::ptr::null_mut()), Err(-libc::EIO));
            assert_eq!(lease.finish_completed(release, std::ptr::null_mut()), Err(-libc::EINVAL));
            assert_eq!(owner.finishes.load(SeqCst), 1);
            lease.abandon_completed().unwrap();
        }
        assert!(owner.ended.load(SeqCst)); assert_eq!(owner.abandons.load(SeqCst), 1);
        drop(lease); assert_eq!(Arc::strong_count(&owner), 1);
    }
    #[test]
    fn unknown_completion_drop_never_ends_signals_or_releases() {
        let (lease, owner) = fixture(); drop(lease);
        assert_eq!(owner.finishes.load(SeqCst), 0); assert_eq!(owner.abandons.load(SeqCst), 0);
        assert!(!owner.ended.load(SeqCst)); assert_eq!(Arc::strong_count(&owner), 2);
        // Only reclaim the test's leaked mock reference: no GPU work exists here.
        unsafe { Arc::decrement_strong_count(Arc::as_ptr(&owner)); }
    }
    #[test]
    fn completed_lease_cannot_be_finished_or_abandoned_twice() {
        let (mut lease, owner) = fixture();
        unsafe {
            lease.finish_completed(release, std::ptr::null_mut()).unwrap();
            assert_eq!(lease.finish_completed(release, std::ptr::null_mut()), Err(-libc::EINVAL));
            assert_eq!(lease.abandon_completed(), Err(-libc::EINVAL));
        }
        assert_eq!(owner.finishes.load(SeqCst), 1); assert!(!owner.ended.load(SeqCst));
    }
}

// Test-only lease transport: real GPU/timeline integration without a Portal session.
#[cfg(test)]
#[repr(C)]
struct SyntheticMetadata { flags:u32, padding:u32, acquire:u64, release:u64 }
#[cfg(test)]
pub(crate) struct SyntheticLease { metadata:SyntheticMetadata, ended:bool,
    cancellation:std::sync::Arc<SyntheticCancellation> }
#[cfg(test)]
#[derive(Default)]
pub(crate) struct SyntheticCancellation {
    pub(crate) cancelled:std::sync::atomic::AtomicBool,
    pub(crate) polls:std::sync::atomic::AtomicUsize,
    pub(crate) abandon_fail_once:std::sync::atomic::AtomicBool,
    pub(crate) finish_fail_after_release_once:std::sync::atomic::AtomicBool,
    pub(crate) release_calls:std::sync::atomic::AtomicUsize,
}
#[cfg(test)]
pub(crate) fn synthetic_lease(frame:AsyncFrame, flags:u32)->AsyncLease<Box<SyntheticLease>> {
    synthetic_lease_cancellable(frame,flags,std::sync::Arc::new(SyntheticCancellation::default()))
}
#[cfg(test)]
pub(crate) fn synthetic_lease_cancellable(frame:AsyncFrame,flags:u32,
    cancellation:std::sync::Arc<SyntheticCancellation>)->AsyncLease<Box<SyntheticLease>> {
    let mut owner=Box::new(SyntheticLease { metadata:SyntheticMetadata {
        flags,padding:0,acquire:frame.acquire_point,release:frame.release_point },ended:false,cancellation });
    let raw=(&mut *owner as *mut SyntheticLease).cast::<c_void>();
    AsyncLease { owner:Some(owner),capture_access:None,portal:raw,lease:raw,frame,attempted:false,
        ops:LeaseOps {
            source_cancelled:|p,_| unsafe {
                let state=&*p.cast::<SyntheticLease>();
                (state.ended || state.cancellation.cancelled.load(std::sync::atomic::Ordering::SeqCst)) as i32
            },
            cancelled:|p| unsafe {
                let state=&*p.cast::<SyntheticLease>();
                state.cancellation.polls.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
                (state.ended || state.cancellation.cancelled.load(std::sync::atomic::Ordering::SeqCst)) as i32
            },
            end:|p| unsafe { (*p.cast::<SyntheticLease>()).ended=true },
            finish:|p,complete,release,data| unsafe {
                assert_eq!(complete,1);
                let state=&mut *(*p).cast::<SyntheticLease>();
                if state.ended || state.cancellation.cancelled.load(std::sync::atomic::Ordering::SeqCst) { return -libc::ECANCELED; }
                state.cancellation.release_calls.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
                let code=release(data,(&mut state.metadata as *mut SyntheticMetadata).cast());
                if code!=0 { return code; }
                assert_eq!(state.metadata.flags,0);
                if state.cancellation.finish_fail_after_release_once
                    .swap(false,std::sync::atomic::Ordering::SeqCst) { return -libc::EIO; }
                *p=std::ptr::null_mut(); 0
            },
            abandon:|p,complete| unsafe {
                let state=&*(*p).cast::<SyntheticLease>();
                assert_eq!(complete,1); assert!(state.ended);
                if state.cancellation.abandon_fail_once
                    .swap(false,std::sync::atomic::Ordering::SeqCst) { return -libc::EIO; }
                *p=std::ptr::null_mut(); 0
            },
        },
    }
}
