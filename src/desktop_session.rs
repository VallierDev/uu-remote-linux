//! Portal availability is independent of the authenticated CONTROL transport.
use std::future::Future;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::time::Instant;
use tokio::sync::{watch, Mutex};
use crate::{capture::ScreenCapture, input::DesktopInputSession};
use crate::portal::PortalError;
use webrtc::{data_channel::{RTCDataChannel, data_channel_state::RTCDataChannelState},
    peer_connection::{RTCPeerConnection, peer_connection_state::RTCPeerConnectionState}};

#[derive(Clone)]
pub struct State {
    pub screen: Arc<ScreenCapture>,
    pub waiting: bool,
}
impl State {
    pub fn permission(&self, desktop: bool) -> crate::system_metrics::Permission {
        use crate::system_metrics::Permission;
        if !desktop { Permission::Allow }
        else if self.waiting { Permission::Unknown }
        else if self.screen.init_error.is_none() { Permission::Allow }
        else { Permission::Reject }
    }
    /// Capture exists and is not waiting for a replacement of a closed Portal session.
    pub fn ready(&self) -> bool { self.screen.init_error.is_none() && !self.waiting }
}

pub struct PortalDesktop {
    state: watch::Sender<State>,
    opening: Arc<Mutex<()>>,
    locked: watch::Receiver<Option<bool>>,
}

struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) { self.0.store(true, Ordering::Release); }
}

enum Inhibitor {
    Real(crate::desktop_power::IdleInhibitor),
    #[cfg(test)]
    Fake(DropSignal),
}

#[cfg(test)]
struct DropSignal(Arc<std::sync::atomic::AtomicUsize>);
#[cfg(test)]
impl Drop for DropSignal {
    fn drop(&mut self) { self.0.fetch_add(1, Ordering::Relaxed); }
}

struct AttemptPrep {
    wake: Result<(), String>,
    inhibitor: IdleGuard,
}

#[derive(Default)]
struct IdleGuard(Option<Inhibitor>);
impl Drop for IdleGuard {
    fn drop(&mut self) {
        match self.0.take() {
            Some(Inhibitor::Real(guard)) => { tokio::task::spawn_blocking(move || drop(guard)); }
            #[cfg(test)]
            Some(Inhibitor::Fake(signal)) => drop(signal),
            None => {}
        }
    }
}

enum AttemptOutcome {
    Ready(Arc<ScreenCapture>),
    Retry(String),
    Terminal(String),
    Cancelled,
}

fn classify(result: Result<Arc<ScreenCapture>, PortalError>) -> AttemptOutcome {
    match result {
        Ok(screen) => AttemptOutcome::Ready(screen),
        Err(error) if error.code == 13 => AttemptOutcome::Cancelled,
        // 4/5/14 are fatal session/geometry states; 6/8 are unsupported
        // consent/format shapes which must not loop prompts forever.
        Err(error) if matches!(error.code, 4 | 5 | 6 | 8 | 14) => AttemptOutcome::Terminal(error.to_string()),
        Err(error) => AttemptOutcome::Retry(error.to_string()),
    }
}

fn should_prepare_power(healthy: bool, force_wake: bool) -> bool {
    !healthy || force_wake
}

fn prepare_attempt<W, I>(
    cancelled: &AtomicBool,
    owner: &DesktopInputSession,
    locked: &watch::Receiver<Option<bool>>,
    wake: W,
    inhibit: I,
) -> Option<AttemptPrep>
where
    W: FnOnce() -> Result<(), String>,
    I: FnOnce() -> IdleGuard,
{
    let current = || !cancelled.load(Ordering::Acquire)
        && owner.is_current()
        && *locked.borrow() == Some(false);
    if !current() { return None; }
    let wake = wake();
    if !current() { return None; }
    let inhibitor = inhibit();
    if !current() { return None; }
    Some(AttemptPrep { wake, inhibitor })
}

impl PortalDesktop {
    pub fn new(screen: Arc<ScreenCapture>) -> Self {
        Self { state: watch::channel(State { waiting: screen.init_error.is_some(), screen }).0,
            opening: Arc::new(Mutex::new(())), locked: crate::session_lock::watch_locked() }
    }
    pub fn begin(&self) {
        // GNOME closes the Portal session on lock; a cached screen with a dead session
        // is not ready (no Allow, no stale video path) until maintain() rebuilds it.
        self.state.send_modify(|state| state.waiting = state.screen.init_error.is_some()
            || state.screen.portal.as_ref().is_some_and(|portal| !portal.is_alive()));
    }
    pub fn snapshot(&self) -> State { self.state.borrow().clone() }
    /// No desktop controller left: stop sharing the screen. begin() then sees the dead
    /// session and the next controller's maintain() reopens it with the saved consent.
    pub fn end_idle(&self) {
        if let Some(portal) = self.snapshot().screen.portal.as_ref().filter(|portal| portal.is_alive()) {
            portal.end();
            log::info!("Portal screen sharing ended: no desktop controller");
        }
    }
    pub fn subscribe(&self) -> watch::Receiver<State> { self.state.subscribe() }
    pub fn lock_state(&self) -> watch::Receiver<Option<bool>> { self.locked.clone() }
    #[cfg(test)]
    pub fn with_lock_state(screen: Arc<ScreenCapture>, locked: watch::Receiver<Option<bool>>) -> Self {
        Self { state: watch::channel(State { waiting: screen.init_error.is_some(), screen }).0,
            opening: Arc::new(Mutex::new(())), locked }
    }
    #[cfg(test)]
    pub fn publish_test_screen(&self, screen: Arc<ScreenCapture>) {
        self.state.send_replace(State { screen, waiting: false });
    }

    fn publish(&self, owner: &DesktopInputSession, screen: Arc<ScreenCapture>) {
        owner.with_current(|| {
            if screen.init_error.is_some() { owner.release_input(); }
            owner.bind_portal(screen.portal.clone());
            self.state.send_replace(State { screen, waiting: false });
        });
    }

    fn pause(&self, owner: &DesktopInputSession) {
        let screen = self.snapshot().screen;
        owner.with_current(|| {
            owner.release_input();
            if let Some(portal) = &screen.portal { portal.pause(); }
        });
    }

    pub async fn run(self: Arc<Self>, pc: Arc<RTCPeerConnection>, channel: Arc<RTCDataChannel>,
        owner: Arc<DesktopInputSession>) {
        self.run_until(owner, closed(&pc, &channel)).await;
    }

    pub async fn run_until<F>(self: Arc<Self>, owner: Arc<DesktopInputSession>, wait_closed: F)
    where F: Future<Output = ()> {
        tokio::select! {
            result = self.maintain(owner.clone()) => {
                if let Err(error) = result {
                    log::warn!("Portal desktop unavailable: {error}");
                    self.publish(&owner, Arc::new(ScreenCapture::unavailable(error)));
                }
            },
            _ = wait_closed => {},
        }
    }

    async fn maintain(&self, owner: Arc<DesktopInputSession>) -> Result<(), String> {
        self.maintain_with(owner,
            crate::desktop_power::wake,
            || match crate::desktop_power::IdleInhibitor::acquire() {
                Ok(guard) => IdleGuard(Some(Inhibitor::Real(guard))),
                Err(error) => { log::warn!("Desktop idle inhibition unavailable: {error}"); IdleGuard::default() }
            },
            |screen, cancel| {
                if let Some(portal) = &screen.portal {
                    portal.recover(&cancel)?;
                    Ok(screen)
                } else {
                    ScreenCapture::open_portal(&cancel).map(Arc::new)
                }
            }).await
    }

    async fn maintain_with<W, I, O>(&self, owner: Arc<DesktopInputSession>, wake: W, inhibit: I, open: O) -> Result<(), String>
    where
        W: Fn() -> Result<(), String> + Send + Sync + 'static,
        I: Fn() -> IdleGuard + Send + Sync + 'static,
        O: Fn(Arc<ScreenCapture>, Arc<AtomicBool>) -> Result<Arc<ScreenCapture>, PortalError> + Send + Sync + 'static,
    {
        let wake = Arc::new(wake);
        let inhibit = Arc::new(inhibit);
        let open = Arc::new(open);
        let mut locked = self.locked.clone();
        let mut previous = None;
        let mut retry_at = Instant::now();
        loop {
            if !owner.is_current() { return Ok(()); }
            let state = *locked.borrow_and_update();
            if state != Some(false) {
                if previous == Some(false) { self.pause(&owner); }
            } else {
                // A fresh unlocked transition gets an immediate wake/recovery attempt.
                let force_wake = previous != Some(false);
                if force_wake { retry_at = Instant::now(); }
                let mut attempt_locked = locked.clone();
                let outcome = tokio::select! {
                    biased;
                    _ = wait_for_lock(&mut locked) => AttemptOutcome::Cancelled,
                    outcome = self.portal_attempt(&owner, &mut attempt_locked, retry_at, force_wake, wake.clone(), inhibit.clone(), open.clone()) => outcome,
                };
                match outcome {
                    AttemptOutcome::Ready(screen) => self.publish(&owner, screen),
                    AttemptOutcome::Terminal(error) => return Err(error),
                    AttemptOutcome::Retry(error) => {
                        log::warn!("Portal desktop recovery deferred: {error}");
                        retry_at = Instant::now() + std::time::Duration::from_secs(5);
                    }
                    AttemptOutcome::Cancelled => {}
                }
                if *locked.borrow_and_update() != Some(false) { self.pause(&owner); }
            }
            previous = *locked.borrow_and_update();
            tokio::select! {
                _ = locked.changed() => {},
                _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {},
            }
        }
    }

    async fn portal_attempt<W, I, O>(&self, owner: &Arc<DesktopInputSession>,
        locked: &mut watch::Receiver<Option<bool>>, retry_at: Instant, force_wake: bool,
        wake: Arc<W>, inhibit: Arc<I>, open: Arc<O>) -> AttemptOutcome
    where
        W: Fn() -> Result<(), String> + Send + Sync + 'static,
        I: Fn() -> IdleGuard + Send + Sync + 'static,
        O: Fn(Arc<ScreenCapture>, Arc<AtomicBool>) -> Result<Arc<ScreenCapture>, PortalError> + Send + Sync + 'static,
    {
        if !owner.is_current() || *locked.borrow() != Some(false) || Instant::now() < retry_at {
            return AttemptOutcome::Cancelled;
        }
        let gate = self.opening.clone().lock_owned().await;
        // The state can change while this attempt waits for an earlier worker
        // to finish rotating the Portal permission token.
        if !owner.is_current() || *locked.borrow() != Some(false) {
            return AttemptOutcome::Cancelled;
        }
        let screen = self.snapshot().screen;
        let healthy = screen.portal.as_ref().is_some_and(|portal| portal.is_alive());
        if !should_prepare_power(healthy, force_wake) {
            return AttemptOutcome::Cancelled;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let _cancel = CancelOnDrop(cancel.clone());
        let prep_cancel = cancel.clone();
        let prep_owner = owner.clone();
        let prep_locked = locked.clone();
        let prep = match tokio::task::spawn_blocking(move || prepare_attempt(
            &prep_cancel,
            &prep_owner,
            &prep_locked,
            || (*wake)(),
            || (*inhibit)(),
        )).await {
            Ok(Some(prep)) => prep,
            Ok(None) => return AttemptOutcome::Cancelled,
            Err(_) => return AttemptOutcome::Terminal("Desktop power worker failed".into()),
        };
        if let Err(error) = prep.wake { log::warn!("Portal screen wake failed: {error}"); }
        // Held by the cancellable async helper, not the blocking worker, so a
        // lock transition releases it while worker cleanup may still be running.
        let _inhibitor = prep.inhibitor;
        if !owner.is_current() || *locked.borrow() != Some(false) {
            return AttemptOutcome::Cancelled;
        }
        // A new controller can arrive while the cached Portal handle is alive
        // but the display has gone to sleep without a lock transition.
        if healthy { return AttemptOutcome::Ready(screen); }
        let worker_cancel = cancel.clone();
        let result = tokio::task::spawn_blocking(move || {
            // The worker owns the gate until its real cleanup completes; a
            // cancelled helper only signals it and cannot start an overlap.
            let _gate = gate;
            if worker_cancel.load(Ordering::Acquire) {
                return Err(PortalError { code: 13 });
            }
            open(screen, worker_cancel)
        }).await;
        if !owner.is_current() || *locked.borrow() != Some(false) {
            return AttemptOutcome::Cancelled;
        }
        match result {
            Ok(result) => classify(result),
            Err(_) => AttemptOutcome::Terminal("Portal initialization worker failed".into()),
        }
    }
}

async fn wait_for_lock(locked: &mut watch::Receiver<Option<bool>>) {
    while locked.changed().await.is_ok() {
        if *locked.borrow_and_update() != Some(false) { return; }
    }
}

pub async fn closed(pc: &RTCPeerConnection, channel: &RTCDataChannel) {
    while !matches!(pc.connection_state(), RTCPeerConnectionState::Closed | RTCPeerConnectionState::Failed)
        && channel.ready_state() == RTCDataChannelState::Open {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

pub async fn wait_ready(state: &mut watch::Receiver<State>, channel: &RTCDataChannel) -> Option<Arc<ScreenCapture>> {
    loop {
        let current = state.borrow_and_update().clone();
        if current.ready() { return Some(current.screen); }
        if !current.waiting || channel.ready_state() != RTCDataChannelState::Open { return None; }
        tokio::select! {
            changed = state.changed() => if changed.is_err() { return None; },
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    fn host() -> PortalDesktop {
        let (_, locked) = watch::channel(Some(true));
        PortalDesktop::with_lock_state(Arc::new(ScreenCapture::unavailable("pending".into())), locked)
    }
    fn ready_screen() -> Arc<ScreenCapture> {
        let mut screen = ScreenCapture::unavailable("synthetic".into());
        screen.backend = crate::desktop_backend::DesktopBackend::Portal;
        screen.init_error = None;
        screen.width = 1920;
        screen.height = 1080;
        Arc::new(screen)
    }
    #[test]
    fn cached_screen_waiting_for_a_closed_portal_is_not_ready_or_allowed() {
        use crate::system_metrics::Permission;
        let live = State { screen: ready_screen(), waiting: false };
        assert!(live.ready() && live.permission(true) == Permission::Allow);
        let stale = State { screen: ready_screen(), waiting: true };
        assert!(!stale.ready());
        assert!(stale.permission(true) == Permission::Unknown);
        assert!(stale.permission(false) == Permission::Allow);
    }
    #[test]
    fn recovered_screen_is_retained_across_transient_disconnect() {
        let host = host();
        let mut updates = host.subscribe();
        let input = Arc::new(crate::input::InputInjector::without_desktop());
        let owner = input.desktop_session();
        owner.connected();owner.disconnected();
        let screen = ready_screen();
        host.publish(&owner, screen.clone());
        assert!(updates.has_changed().unwrap());
        assert!(!updates.borrow_and_update().waiting);
        assert!(Arc::ptr_eq(&host.snapshot().screen, &screen));
        assert!(!host.snapshot().waiting);
        assert_eq!(host.snapshot().permission(true), crate::system_metrics::Permission::Allow);
        owner.with_input(|| panic!("publication must not reactivate input"));
        owner.connected();
        assert!(Arc::ptr_eq(&host.snapshot().screen, &screen));
    }
    #[test]
    fn terminal_failure_publishes_rejection_while_disconnected() {
        let host = host();
        let mut updates = host.subscribe();
        let input = Arc::new(crate::input::InputInjector::without_desktop());
        let owner = input.desktop_session();
        owner.connected();owner.disconnected();
        host.publish(&owner, Arc::new(ScreenCapture::unavailable("consent rejected".into())));
        assert!(updates.has_changed().unwrap());
        assert!(!updates.borrow_and_update().waiting);
        assert!(!host.snapshot().waiting);
        assert_eq!(host.snapshot().permission(true), crate::system_metrics::Permission::Reject);
        owner.connected();
        assert!(!host.snapshot().waiting);
        assert_eq!(host.snapshot().screen.init_error.as_deref(), Some("consent rejected"));
    }
    #[test]
    fn replaced_peer_cannot_publish_success_or_failure() {
        let host = host();
        let input = Arc::new(crate::input::InputInjector::without_desktop());
        let old = input.desktop_session();
        let current = input.desktop_session();
        let screen = ready_screen();
        host.publish(&current, screen.clone());
        host.publish(&old, Arc::new(ScreenCapture::unavailable("late failure".into())));
        host.publish(&old, ready_screen());
        assert!(Arc::ptr_eq(&host.snapshot().screen, &screen));
        assert!(!host.snapshot().waiting);
    }
    #[test]
    fn unavailable_desktop_does_not_reject_independent_tools() {
        let host = PortalDesktop::new(Arc::new(ScreenCapture::unavailable("not authorized".into())));
        assert_eq!(host.snapshot().permission(true), crate::system_metrics::Permission::Unknown);
        assert_eq!(host.snapshot().permission(false), crate::system_metrics::Permission::Allow);
        host.state.send_modify(|state| state.waiting = false);
        assert_eq!(host.snapshot().permission(true), crate::system_metrics::Permission::Reject);
        assert_eq!(host.snapshot().permission(false), crate::system_metrics::Permission::Allow);
        host.begin();
        assert!(host.snapshot().waiting);
    }
    #[test]
    fn unsupported_portal_shapes_are_terminal_while_transient_codes_retry() {
        for code in [4, 5, 6, 8, 14] {
            assert!(matches!(classify(Err(PortalError { code })), AttemptOutcome::Terminal(_)), "code {code}");
        }
        for code in [1, 3, 7, 9, 10] {
            assert!(matches!(classify(Err(PortalError { code })), AttemptOutcome::Retry(_)), "code {code}");
        }
        assert!(matches!(classify(Err(PortalError { code: 13 })), AttemptOutcome::Cancelled));
    }
    #[test]
    fn fresh_unlock_wakes_even_when_cached_portal_is_healthy() {
        assert!(should_prepare_power(true, true));
        assert!(should_prepare_power(false, false));
        assert!(!should_prepare_power(true, false));
    }
    #[test]
    fn queued_power_prep_checks_cancel_lock_and_current_owner_before_wake() {
        let input = Arc::new(crate::input::InputInjector::without_desktop());
        let owner = input.desktop_session();
        let (lock, locked) = watch::channel(Some(false));
        let wakes = AtomicUsize::new(0);
        let inhibitors = AtomicUsize::new(0);
        let cancelled = AtomicBool::new(true);
        assert!(prepare_attempt(&cancelled, &owner, &locked,
            || { wakes.fetch_add(1, Ordering::Relaxed); Ok(()) },
            || { inhibitors.fetch_add(1, Ordering::Relaxed); IdleGuard::default() }).is_none());
        assert_eq!(wakes.load(Ordering::Relaxed), 0);
        cancelled.store(false, Ordering::Release);
        lock.send(Some(true)).unwrap();
        assert!(prepare_attempt(&cancelled, &owner, &locked,
            || { wakes.fetch_add(1, Ordering::Relaxed); Ok(()) },
            || { inhibitors.fetch_add(1, Ordering::Relaxed); IdleGuard::default() }).is_none());
        assert_eq!(wakes.load(Ordering::Relaxed), 0);
        lock.send(Some(false)).unwrap();
        let locked_during_wake = lock.clone();
        assert!(prepare_attempt(&cancelled, &owner, &locked,
            || {
                wakes.fetch_add(1, Ordering::Relaxed);
                locked_during_wake.send(Some(true)).unwrap();
                Ok(())
            },
            || { inhibitors.fetch_add(1, Ordering::Relaxed); IdleGuard::default() }).is_none());
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        assert_eq!(inhibitors.load(Ordering::Relaxed), 0);
        lock.send(Some(false)).unwrap();
        let replacement = input.desktop_session();
        assert!(prepare_attempt(&cancelled, &owner, &locked,
            || { wakes.fetch_add(1, Ordering::Relaxed); Ok(()) },
            || { inhibitors.fetch_add(1, Ordering::Relaxed); IdleGuard::default() }).is_none());
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        assert!(replacement.is_current());
        assert_eq!(inhibitors.load(Ordering::Relaxed), 0);
    }
    #[test]
    fn cancelling_a_peer_signals_its_blocking_permission_worker() {
        let flag = Arc::new(AtomicBool::new(false));
        let guard = CancelOnDrop(flag.clone());
        assert!(!flag.load(Ordering::Acquire));
        drop(guard);
        assert!(flag.load(Ordering::Acquire));
    }
    #[tokio::test]
    async fn lock_transition_cancels_blocked_attempt_and_releases_inhibitor() {
        let (lock, locked) = watch::channel(Some(false));
        let host = Arc::new(PortalDesktop::with_lock_state(
            Arc::new(ScreenCapture::unavailable("pending".into())), locked));
        let input = Arc::new(crate::input::InputInjector::without_desktop());
        let owner = input.desktop_session();
        let releases = Arc::new(AtomicUsize::new(0));
        let wakes = Arc::new(AtomicUsize::new(0));
        let (started, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn({
            let host = host.clone();
            let owner = owner.clone();
            let releases = releases.clone();
            let wakes = wakes.clone();
            async move {
                host.maintain_with(owner,
                    move || {
                        wakes.fetch_add(1, Ordering::Relaxed);
                        Ok(())
                    },
                    move || IdleGuard(Some(Inhibitor::Fake(DropSignal(releases.clone())))),
                    move |_screen, cancel| {
                        let _ = started.send(());
                        while !cancel.load(Ordering::Acquire) {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        Err(PortalError { code: 13 })
                    }).await
            }
        });
        started_rx.recv().await.unwrap();
        lock.send(Some(true)).unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while releases.load(Ordering::Relaxed) == 0 {
            assert!(tokio::time::Instant::now() < deadline, "cancelled attempt did not release inhibitor");
            tokio::task::yield_now().await;
        }
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while host.opening.try_lock().is_err() {
            assert!(tokio::time::Instant::now() < deadline, "cancelled worker did not finish");
            tokio::task::yield_now().await;
        }
        task.abort();
        let _ = task.await;
    }
    #[tokio::test]
    async fn inhibitor_release_does_not_wait_for_blocked_worker_cleanup() {
        let (lock, locked) = watch::channel(Some(false));
        let host = Arc::new(PortalDesktop::with_lock_state(
            Arc::new(ScreenCapture::unavailable("pending".into())), locked));
        let input = Arc::new(crate::input::InputInjector::without_desktop());
        let owner = input.desktop_session();
        let releases = Arc::new(AtomicUsize::new(0));
        let worker_release = Arc::new(AtomicBool::new(false));
        let (started, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn({
            let host = host.clone();
            let owner = owner.clone();
            let releases = releases.clone();
            let worker_release = worker_release.clone();
            async move {
                host.maintain_with(owner,
                    || Ok(()),
                    move || IdleGuard(Some(Inhibitor::Fake(DropSignal(releases.clone())))),
                    move |screen, _cancel| {
                        let _ = started.send(());
                        while !worker_release.load(Ordering::Acquire) {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        Ok(screen)
                    }).await
            }
        });
        started_rx.recv().await.unwrap();
        lock.send(Some(true)).unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while releases.load(Ordering::Relaxed) == 0 {
            assert!(tokio::time::Instant::now() < deadline, "cancelled attempt did not release inhibitor");
            tokio::task::yield_now().await;
        }
        assert!(host.opening.try_lock().is_err(), "worker cleanup must still own the gate");
        worker_release.store(true, Ordering::Release);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if host.opening.try_lock().is_ok() { break; }
            assert!(tokio::time::Instant::now() < deadline, "worker cleanup did not release the gate");
            tokio::task::yield_now().await;
        }
        task.abort();
        let _ = task.await;
    }
}
