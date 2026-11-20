//! Portal availability is independent of the authenticated CONTROL transport.
use std::future::Future;
use std::sync::{Arc, OnceLock, atomic::{AtomicBool, Ordering}};
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
    capture_while_locked: bool,
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
    /// The attempt's idle inhibitor moves into the session: without it GNOME blanks and
    /// locks an unattended host after idle-delay, which ends the Portal session and stops
    /// both frames and injected input.
    Ready(Arc<ScreenCapture>, IdleGuard),
    Retry(String),
    Terminal(String),
    Deferred,
    Cancelled,
}

fn classify(result: Result<Arc<ScreenCapture>, PortalError>, inhibitor: IdleGuard) -> AttemptOutcome {
    match result {
        Ok(screen) => AttemptOutcome::Ready(screen, inhibitor),
        Err(error) if error.code == 13 => AttemptOutcome::Cancelled,
        // 4/5/14 are fatal session/geometry states; 6/8 are unsupported
        // consent/format shapes which must not loop prompts forever.
        Err(error) if matches!(error.code, 4 | 5 | 6 | 8 | 14) => AttemptOutcome::Terminal(error.to_string()),
        Err(error) => AttemptOutcome::Retry(error.to_string()),
    }
}

/// A consent dialog opened behind the lock screen cannot be approved, so its denied/timed-out
/// error must not end this connection's supervisor: ask again after the local unlock.
fn defer_consent(error: &PortalError, state: Option<bool>) -> bool {
    state == Some(true) && matches!(error.code, 4 | 5)
}

/// True when this iteration sees a lock state it has not attempted under yet. Waking and resetting
/// the retry delay must happen once per transition, not once per 200 ms poll: a locked state would
/// otherwise re-run wake, inhibitor and publish on every tick and defeat the 5 s retry backoff.
fn fresh_state(previous: Option<Option<bool>>, state: Option<bool>) -> bool {
    previous != Some(state)
}

fn should_prepare_power(healthy: bool, force_wake: bool) -> bool {
    !healthy || force_wake
}

/// Whether frames actually reach the viewer. The Portal session object staying alive is not
/// enough: the PipeWire stream can be interrupted past its deadline, or the encoder's own reopen
/// can be backing off, and neither comes with a lock event. Idle inhibition belongs to a working
/// stream, so both read as unhealthy here.
fn screen_capture_healthy(screen: &ScreenCapture) -> bool {
    screen.portal.as_ref().is_some_and(|portal| portal.capture_healthy())
}

/// KWin keeps the Portal screencast, including the lock screen, alive while the session is
/// locked, so a cold connection can show the real lock screen. Mutter closes the session on
/// lock, so other desktops keep the existing black-frame path. `UU_LOCKED_CAPTURE=0` opts out.
fn capture_while_locked() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        match std::env::var("UU_LOCKED_CAPTURE").ok().filter(|value| !value.is_empty()) {
            Some(value) => !matches!(value.to_ascii_lowercase().as_str(), "0" | "off" | "false" | "no"),
            None => ["XDG_CURRENT_DESKTOP", "XDG_SESSION_DESKTOP"].iter().any(|key|
                std::env::var(key).is_ok_and(|desktop| desktop.split(':')
                    .any(|part| part.eq_ignore_ascii_case("KDE")))),
        }
    })
}

/// Capture may start and continue in this lock state. An unknown state keeps the old wait on
/// desktops whose compositor closes the session on lock.
fn attempt_allowed(state: Option<bool>, capture_while_locked: bool) -> bool {
    match state {
        Some(false) => true,
        Some(true) | None => capture_while_locked,
    }
}

fn prepare_attempt<W, I>(
    cancelled: &AtomicBool,
    owner: &DesktopInputSession,
    locked: &watch::Receiver<Option<bool>>,
    allow_locked: bool,
    wake: W,
    inhibit: I,
) -> Option<AttemptPrep>
where
    W: FnOnce() -> Result<(), String>,
    I: FnOnce() -> IdleGuard,
{
    let current = || !cancelled.load(Ordering::Acquire)
        && owner.is_current()
        && attempt_allowed(*locked.borrow(), allow_locked);
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
            opening: Arc::new(Mutex::new(())), locked: crate::session_lock::watch_locked(),
            capture_while_locked: capture_while_locked() }
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
    /// Whether capture may run in this lock state for this desktop. Shares the rule used by
    /// maintain(), so capture-setting requests do not stall behind a lock the compositor does
    /// not honor.
    pub fn capture_allowed(&self, locked: Option<bool>) -> bool {
        attempt_allowed(locked, self.capture_while_locked)
    }
    #[cfg(test)]
    pub fn with_lock_state(screen: Arc<ScreenCapture>, locked: watch::Receiver<Option<bool>>) -> Self {
        Self::with_lock_state_and_capture(screen, locked, false)
    }
    #[cfg(test)]
    pub fn with_lock_state_and_capture(screen: Arc<ScreenCapture>, locked: watch::Receiver<Option<bool>>,
        capture_while_locked: bool) -> Self {
        Self { state: watch::channel(State { waiting: screen.init_error.is_some(), screen }).0,
            opening: Arc::new(Mutex::new(())), locked, capture_while_locked }
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
            screen_capture_healthy,
            |screen, cancel| {
                if let Some(portal) = &screen.portal {
                    portal.recover(&cancel)?;
                    Ok(screen)
                } else {
                    ScreenCapture::open_portal(&cancel).map(Arc::new)
                }
            }).await
    }

    async fn maintain_with<W, I, H, O>(&self, owner: Arc<DesktopInputSession>, wake: W, inhibit: I, health: H, open: O) -> Result<(), String>
    where
        W: Fn() -> Result<(), String> + Send + Sync + 'static,
        I: Fn() -> IdleGuard + Send + Sync + 'static,
        H: Fn(&ScreenCapture) -> bool + Send + Sync + 'static,
        O: Fn(Arc<ScreenCapture>, Arc<AtomicBool>) -> Result<Arc<ScreenCapture>, PortalError> + Send + Sync + 'static,
    {
        let wake = Arc::new(wake);
        let inhibit = Arc::new(inhibit);
        let health = Arc::new(health);
        let open = Arc::new(open);
        let mut locked = self.locked.clone();
        let allow_locked = self.capture_while_locked;
        let mut previous: Option<Option<bool>> = None;
        let mut defer_until_unlock = false;
        let mut retry_at = Instant::now();
        // A drop holder, hence the underscore: held while frames flow for the whole desktop
        // session, not just while the Portal session opens. GNOME blanks and then locks an idle
        // host, which ends the Portal session and stops frames and injected input together. It is
        // dropped whenever capture is not allowed or recovery fails, matching the README promise
        // that locking or a failed initialization releases the inhibition.
        let mut _inhibitor: Option<IdleGuard> = None;
        loop {
            if !owner.is_current() {
                log::info!("Portal maintain stopped: superseded by a newer desktop connection");
                return Ok(());
            }
            let state = *locked.borrow_and_update();
            if state == Some(false) { defer_until_unlock = false; }
            if state != Some(false) && previous == Some(Some(false)) { self.pause(&owner); }
            // A state that forbids capture gives up the inhibition too: the viewer sees nothing
            // while paused, and the host must be free to blank and lock on its own schedule.
            if !attempt_allowed(state, allow_locked) { _inhibitor = None; }
            if state == Some(true) && previous != Some(Some(true)) && allow_locked {
                log::info!("Desktop is locked; opening Portal capture for the lock screen");
            }
            if attempt_allowed(state, allow_locked) && !defer_until_unlock {
                // A fresh transition into an attempted state gets an immediate wake/recovery attempt.
                let force_wake = fresh_state(previous, state);
                if force_wake {
                    retry_at = Instant::now();
                    // One line per lock-state transition: connects that start locked only become
                    // picture-capable after the auto unlock, and this is the marker that the
                    // supervisor actually reacted to the unlock (its absence means the loop is dead).
                    log::info!("Portal desktop lock transition: state={state:?} defer={defer_until_unlock}");
                }
                let mut attempt_locked = locked.clone();
                let outcome = tokio::select! {
                    biased;
                    _ = wait_for_lock(&mut locked, allow_locked) => AttemptOutcome::Cancelled,
                    outcome = self.portal_attempt(&owner, &mut attempt_locked, allow_locked, retry_at, force_wake, wake.clone(), inhibit.clone(), health.clone(), open.clone()) => outcome,
                };
                match outcome {
                    AttemptOutcome::Ready(screen, guard) => {
                        _inhibitor = Some(guard);
                        self.publish(&owner, screen);
                    }
                    AttemptOutcome::Terminal(error) => return Err(error),
                    AttemptOutcome::Retry(error) => {
                        log::warn!("Portal desktop recovery deferred: {error}");
                        // No frames flow while recovery keeps failing; a stale host must not be
                        // held awake through the outage, and the next Ready takes a fresh guard.
                        _inhibitor = None;
                        retry_at = Instant::now() + std::time::Duration::from_secs(5);
                    }
                    AttemptOutcome::Deferred => {
                        defer_until_unlock = true;
                        _inhibitor = None;
                    }
                    AttemptOutcome::Cancelled => {
                        // A Retry released the inhibitor earlier and the encoder's own
                        // stream-loss recovery has since brought the share back: the healthy
                        // cache now exits portal_attempt as Cancelled without touching power,
                        // so re-arm the inhibition here, or a watching-only viewer is locked
                        // out again once idle-delay passes.
                        if _inhibitor.is_none()
                            && attempt_allowed(*locked.borrow(), allow_locked)
                            && health(self.snapshot().screen.as_ref()) {
                            _inhibitor = tokio::task::spawn_blocking({
                                let inhibit = inhibit.clone();
                                move || inhibit()
                            }).await.ok();
                        }
                    }
                }
                if !attempt_allowed(*locked.borrow_and_update(), allow_locked) { self.pause(&owner); }
            }
            previous = Some(*locked.borrow_and_update());
            tokio::select! {
                _ = locked.changed() => {},
                _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {},
            }
        }
    }

    async fn portal_attempt<W, I, H, O>(&self, owner: &Arc<DesktopInputSession>,
        locked: &mut watch::Receiver<Option<bool>>, allow_locked: bool, retry_at: Instant, force_wake: bool,
        wake: Arc<W>, inhibit: Arc<I>, health: Arc<H>, open: Arc<O>) -> AttemptOutcome
    where
        W: Fn() -> Result<(), String> + Send + Sync + 'static,
        I: Fn() -> IdleGuard + Send + Sync + 'static,
        H: Fn(&ScreenCapture) -> bool + Send + Sync + 'static,
        O: Fn(Arc<ScreenCapture>, Arc<AtomicBool>) -> Result<Arc<ScreenCapture>, PortalError> + Send + Sync + 'static,
    {
        if !owner.is_current() || !attempt_allowed(*locked.borrow(), allow_locked) || Instant::now() < retry_at {
            return AttemptOutcome::Cancelled;
        }
        let gate = self.opening.clone().lock_owned().await;
        // The state can change while this attempt waits for an earlier worker
        // to finish rotating the Portal permission token.
        if !owner.is_current() || !attempt_allowed(*locked.borrow(), allow_locked) {
            return AttemptOutcome::Cancelled;
        }
        let screen = self.snapshot().screen;
        let healthy = health(screen.as_ref());
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
            allow_locked,
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
        let inhibitor = prep.inhibitor;
        if !owner.is_current() || !attempt_allowed(*locked.borrow(), allow_locked) {
            return AttemptOutcome::Cancelled;
        }
        // A new controller can arrive while the cached Portal handle is alive
        // but the display has gone to sleep without a lock transition.
        if healthy { return AttemptOutcome::Ready(screen, inhibitor); }
        // Marks every real reopen: paired with the lock-transition log, the absence of an
        // outcome after this line pins a stall to the blocking open/recovery worker below.
        log::info!("Portal attempt: reopening the capture session (force_wake={force_wake})");
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
        if !owner.is_current() || !attempt_allowed(*locked.borrow(), allow_locked) {
            return AttemptOutcome::Cancelled;
        }
        match result {
            Ok(Err(error)) if defer_consent(&error, *locked.borrow()) => {
                // The consent dialog opened behind the lock screen cannot be approved here.
                // Keep this connection alive and ask again after the local unlock. The
                // inhibitor drops with this attempt; the retry after the unlock inhibits again.
                log::info!("Portal consent is behind the lock screen; waiting for the local unlock");
                AttemptOutcome::Deferred
            }
            Ok(result) => classify(result, inhibitor),
            Err(_) => AttemptOutcome::Terminal("Portal initialization worker failed".into()),
        }
    }
}

async fn wait_for_lock(locked: &mut watch::Receiver<Option<bool>>, allow_locked: bool) {
    while locked.changed().await.is_ok() {
        if !attempt_allowed(*locked.borrow_and_update(), allow_locked) { return; }
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
        let failed = |code| classify(Err(PortalError { code }), IdleGuard::default());
        for code in [4, 5, 6, 8, 14] {
            assert!(matches!(failed(code), AttemptOutcome::Terminal(_)), "code {code}");
        }
        for code in [1, 3, 7, 9, 10] {
            assert!(matches!(failed(code), AttemptOutcome::Retry(_)), "code {code}");
        }
        assert!(matches!(failed(13), AttemptOutcome::Cancelled));
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
        assert!(prepare_attempt(&cancelled, &owner, &locked, false,
            || { wakes.fetch_add(1, Ordering::Relaxed); Ok(()) },
            || { inhibitors.fetch_add(1, Ordering::Relaxed); IdleGuard::default() }).is_none());
        assert_eq!(wakes.load(Ordering::Relaxed), 0);
        cancelled.store(false, Ordering::Release);
        lock.send(Some(true)).unwrap();
        assert!(prepare_attempt(&cancelled, &owner, &locked, false,
            || { wakes.fetch_add(1, Ordering::Relaxed); Ok(()) },
            || { inhibitors.fetch_add(1, Ordering::Relaxed); IdleGuard::default() }).is_none());
        assert_eq!(wakes.load(Ordering::Relaxed), 0);
        lock.send(Some(false)).unwrap();
        let locked_during_wake = lock.clone();
        assert!(prepare_attempt(&cancelled, &owner, &locked, false,
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
        assert!(prepare_attempt(&cancelled, &owner, &locked, false,
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
    #[test]
    fn locked_capture_gate_follows_the_compositor_rule() {
        assert!(attempt_allowed(Some(false), false));
        assert!(!attempt_allowed(Some(true), false));
        assert!(!attempt_allowed(None, false));
        assert!(attempt_allowed(Some(true), true));
        assert!(attempt_allowed(None, true));
    }
    #[test]
    fn wake_and_retry_reset_once_per_lock_transition() {
        assert!(fresh_state(None, Some(true)));
        assert!(!fresh_state(Some(Some(true)), Some(true)));
        assert!(fresh_state(Some(Some(true)), Some(false)));
        assert!(!fresh_state(Some(Some(false)), Some(false)));
        assert!(fresh_state(Some(Some(true)), None));
        assert!(!fresh_state(Some(None), None));
    }
    #[test]
    fn consent_failures_are_deferred_only_while_locked() {
        for code in [4, 5] {
            assert!(defer_consent(&PortalError { code }, Some(true)));
            assert!(!defer_consent(&PortalError { code }, Some(false)));
            assert!(!defer_consent(&PortalError { code }, None));
        }
        assert!(!defer_consent(&PortalError { code: 8 }, Some(true)));
        assert!(!defer_consent(&PortalError { code: 13 }, Some(true)));
    }
    #[tokio::test]
    async fn wait_for_lock_ends_on_a_lock_the_compositor_does_not_honor() {
        use std::time::Duration;
        let (lock, mut locked) = watch::channel(Some(false));
        let mut task = tokio::spawn(async move { wait_for_lock(&mut locked, false).await; });
        lock.send(Some(true)).unwrap();
        tokio::time::timeout(Duration::from_secs(1), &mut task).await
            .expect("lock must end the wait").unwrap();
    }
    #[tokio::test]
    async fn wait_for_lock_ignores_lock_changes_when_capture_is_allowed() {
        use std::time::Duration;
        let (lock, mut locked) = watch::channel(Some(false));
        let mut task = tokio::spawn(async move { wait_for_lock(&mut locked, true).await; });
        lock.send(Some(true)).unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(100), &mut task).await.is_err(),
            "lock cancelled an allowed wait");
        lock.send(None).unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(100), &mut task).await.is_err(),
            "unknown lock cancelled an allowed wait");
        task.abort();
        let _ = task.await;
    }
    #[test]
    fn locked_attempt_is_allowed_when_the_compositor_keeps_the_stream() {
        let input = Arc::new(crate::input::InputInjector::without_desktop());
        let owner = input.desktop_session();
        let (lock, locked) = watch::channel(Some(true));
        let cancelled = AtomicBool::new(false);
        let wakes = AtomicUsize::new(0);
        assert!(prepare_attempt(&cancelled, &owner, &locked, true,
            || { wakes.fetch_add(1, Ordering::Relaxed); Ok(()) },
            || IdleGuard::default()).is_some());
        lock.send(None).unwrap();
        assert!(prepare_attempt(&cancelled, &owner, &locked, true,
            || { wakes.fetch_add(1, Ordering::Relaxed); Ok(()) },
            || IdleGuard::default()).is_some());
        assert_eq!(wakes.load(Ordering::Relaxed), 2);
    }
    #[tokio::test]
    async fn locked_capture_keeps_the_attempt_running() {
        use std::time::Duration;
        let (lock, locked) = watch::channel(Some(false));
        let host = Arc::new(PortalDesktop::with_lock_state_and_capture(
            Arc::new(ScreenCapture::unavailable("pending".into())), locked, true));
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
                    |_screen| false,
                    move |screen, cancel| {
                        let _ = started.send(());
                        while !worker_release.load(Ordering::Acquire) && !cancel.load(Ordering::Acquire) {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        if cancel.load(Ordering::Acquire) { return Err(PortalError { code: 13 }); }
                        Ok(screen)
                    }).await
            }
        });
        started_rx.recv().await.unwrap();
        lock.send(Some(true)).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(releases.load(Ordering::Relaxed), 0, "lock released a compositor-backed attempt");
        worker_release.store(true, Ordering::Release);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while host.snapshot().waiting {
            assert!(tokio::time::Instant::now() < deadline, "locked attempt did not publish");
            tokio::task::yield_now().await;
        }
        task.abort();
        let _ = task.await;
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
                    |_screen| false,
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
                    |_screen| false,
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
    #[tokio::test]
    async fn ready_portal_keeps_the_idle_inhibitor_for_the_session() {
        let (_lock, locked) = watch::channel(Some(false));
        let host = Arc::new(PortalDesktop::with_lock_state(
            Arc::new(ScreenCapture::unavailable("pending".into())), locked));
        let input = Arc::new(crate::input::InputInjector::without_desktop());
        let owner = input.desktop_session();
        let releases = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn({
            let host = host.clone();
            let owner = owner.clone();
            let releases = releases.clone();
            async move {
                host.maintain_with(owner, || Ok(()),
                    move || IdleGuard(Some(Inhibitor::Fake(DropSignal(releases.clone())))),
                    |_screen| false,
                    |screen, _cancel| Ok(screen)).await
            }
        });
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while host.snapshot().waiting {
            assert!(tokio::time::Instant::now() < deadline, "Portal desktop was never published");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // GNOME must not see the host idle while the viewer is only watching.
        assert_eq!(releases.load(Ordering::Relaxed), 0);
        task.abort();
        let _ = task.await;
        assert_eq!(releases.load(Ordering::Relaxed), 1);
    }
    #[tokio::test]
    async fn locking_after_ready_releases_the_saved_inhibitor() {
        let (lock, locked) = watch::channel(Some(false));
        let host = Arc::new(PortalDesktop::with_lock_state(
            Arc::new(ScreenCapture::unavailable("pending".into())), locked));
        let input = Arc::new(crate::input::InputInjector::without_desktop());
        let owner = input.desktop_session();
        let releases = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn({
            let host = host.clone();
            let owner = owner.clone();
            let releases = releases.clone();
            async move {
                host.maintain_with(owner, || Ok(()),
                    move || IdleGuard(Some(Inhibitor::Fake(DropSignal(releases.clone())))),
                    |_screen| false,
                    |screen, _cancel| Ok(screen)).await
            }
        });
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while host.snapshot().waiting {
            assert!(tokio::time::Instant::now() < deadline, "Portal desktop was never published");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // The compositor closes the session on lock; the saved guard must not outlive it.
        lock.send_replace(Some(true));
        while releases.load(Ordering::Relaxed) == 0 {
            assert!(tokio::time::Instant::now() < deadline, "locking kept the inhibitor held");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        task.abort();
        let _ = task.await;
        // Dropped exactly once at the lock transition and not replaced while locked.
        assert_eq!(releases.load(Ordering::Relaxed), 1);
    }
    /// A GNOME session can stop delivering frames without any lock event: after a display mode
    /// change the PipeWire stream runs past its interrupt deadline while `uu_portal_alive` keeps
    /// reporting true, the pair `tests/portal_engine.c` covers. The supervisor must judge the
    /// stream rather than the session object and give up the inhibition it saved for a working one.
    #[tokio::test]
    async fn stalled_capture_releases_the_saved_inhibitor_without_a_lock() {
        let (_lock, locked) = watch::channel(Some(false));
        let host = Arc::new(PortalDesktop::with_lock_state(
            Arc::new(ScreenCapture::unavailable("pending".into())), locked));
        let input = Arc::new(crate::input::InputInjector::without_desktop());
        let owner = input.desktop_session();
        let releases = Arc::new(AtomicUsize::new(0));
        // Frames are absent until the first open, then flow until the test stalls them.
        let flowing = Arc::new(AtomicBool::new(false));
        let opens = Arc::new(AtomicUsize::new(0));
        let (flowing_health, opens_health) = (flowing.clone(), opens.clone());
        let (flowing_open, opens_open) = (flowing.clone(), opens.clone());
        let task = tokio::spawn({
            let host = host.clone();
            let owner = owner.clone();
            let releases = releases.clone();
            async move {
                host.maintain_with(owner, || Ok(()),
                    move || IdleGuard(Some(Inhibitor::Fake(DropSignal(releases.clone())))),
                    move |_screen| opens_health.load(Ordering::Relaxed) > 0
                        && flowing_health.load(Ordering::Relaxed),
                    move |screen, _cancel| {
                        // Only the reopen fails: the stream stays interrupted, which is how a
                        // recovery the compositor cannot honour reaches this loop.
                        if opens_open.fetch_add(1, Ordering::Relaxed) == 0 {
                            flowing_open.store(true, Ordering::Release);
                            Ok(screen)
                        } else {
                            Err(PortalError { code: 3 })
                        }
                    }).await
            }
        });
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while host.snapshot().waiting {
            assert!(tokio::time::Instant::now() < deadline, "Portal desktop was never published");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(releases.load(Ordering::Relaxed), 0, "a session with flowing frames gave up its inhibitor");
        // Frames stop on their own. The reopen cannot bring them back, so the attempt must not
        // hold the host awake over an outage the viewer cannot see through.
        flowing.store(false, Ordering::Release);
        let stalled = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        // The failed reopen drops its own guard and Retry drops the session's saved one.
        while releases.load(Ordering::Relaxed) < 2 {
            assert!(tokio::time::Instant::now() < stalled, "a stalled capture kept the host awake");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        task.abort();
        let _ = task.await;
        assert_eq!(releases.load(Ordering::Relaxed), 2, "each guard was released exactly once");
    }

    /// After a Retry released the inhibitor, the encoder's own stream-loss recovery can bring
    /// the share back on its own: the healthy cache then exits portal_attempt as Cancelled
    /// without ever touching power, so nothing would re-arm inhibition and a watching-only
    /// viewer would hit idle-delay lockout again. The supervisor must re-acquire a guard.
    #[tokio::test]
    async fn encoder_side_recovery_reacquires_the_idle_inhibitor() {
        let (_lock, locked) = watch::channel(Some(false));
        let host = Arc::new(PortalDesktop::with_lock_state(
            Arc::new(ScreenCapture::unavailable("pending".into())), locked));
        let input = Arc::new(crate::input::InputInjector::without_desktop());
        let owner = input.desktop_session();
        let releases = Arc::new(AtomicUsize::new(0));
        let acquires = Arc::new(AtomicUsize::new(0));
        let healthy = Arc::new(AtomicBool::new(false));
        let (healthy_health, healthy_rearm) = (healthy.clone(), healthy.clone());
        let (acquires_inhibit, acquires_assert) = (acquires.clone(), acquires.clone());
        let task = tokio::spawn({
            let host = host.clone();
            let owner = owner.clone();
            let releases = releases.clone();
            async move {
                host.maintain_with(owner, || Ok(()),
                    move || {
                        acquires_inhibit.fetch_add(1, Ordering::Relaxed);
                        IdleGuard(Some(Inhibitor::Fake(DropSignal(releases.clone()))))
                    },
                    move |_screen| healthy_health.load(Ordering::Relaxed),
                    // The reopen always fails: only the encoder's side recovery (the flag)
                    // ever makes the share healthy again, exactly like a lost stream that
                    // PipeWire revives without a new portal session.
                    move |_screen, _cancel| Err(PortalError { code: 3 })).await
            }
        });
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while releases.load(Ordering::Relaxed) == 0 {
            assert!(tokio::time::Instant::now() < deadline, "Retry never released the attempt guard");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(acquires_assert.load(Ordering::Relaxed), 1);
        // The encoder-side recovery lands after the Retry: frames flow again, no reopen runs.
        healthy_rearm.store(true, Ordering::Release);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while acquires_assert.load(Ordering::Relaxed) < 2 {
            assert!(tokio::time::Instant::now() < deadline, "healthy encoder-side recovery never re-armed inhibition");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(releases.load(Ordering::Relaxed), 1, "the re-armed guard must stay held");
        task.abort();
        let _ = task.await;
        assert_eq!(releases.load(Ordering::Relaxed), 2);
    }
}
