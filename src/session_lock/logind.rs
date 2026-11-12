//! logind signals replace per-second process spawning; loginctl is a bounded fallback.
use super::{publish_lock_state, Duration};
use futures_util::StreamExt;
use tokio::sync::watch;
use zbus::{Connection, MatchRule, MessageStream, Proxy, proxy::CacheProperties};

const SERVICE: &str = "org.freedesktop.login1";
const ROOT: &str = "/org/freedesktop/login1";
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY: Duration = Duration::from_secs(30);

async fn locked(connection: &Connection) -> zbus::Result<Option<bool>> {
    let path = format!("{ROOT}/user/_{}", unsafe { libc::getuid() });
    let user = zbus::proxy::Builder::<Proxy<'_>>::new(connection).destination(SERVICE)?
        .path(path)?.interface("org.freedesktop.login1.User")?
        .cache_properties(CacheProperties::No).build().await?;
    // No cached property reads: signals, including invalidations, trigger a fresh snapshot.
    let display: (String, zbus::zvariant::OwnedObjectPath) = user.get_property("Display").await?;
    if display.0.is_empty() { return Ok(None); }
    let session = zbus::proxy::Builder::<Proxy<'_>>::new(connection).destination(SERVICE)?
        .path(display.1)?.interface("org.freedesktop.login1.Session")?
        .cache_properties(CacheProperties::No).build().await?;
    session.get_property("LockedHint").await.map(Some)
}

async fn snapshot(connection: &Connection) -> zbus::Result<Option<bool>> {
    tokio::time::timeout(CALL_TIMEOUT, locked(connection)).await
        .map_err(|_| zbus::Error::Failure("lock query timed out".into()))?
}

async fn monitor_connection(connection: &Connection, sender: &watch::Sender<Option<bool>>) -> zbus::Result<()> {
    // Subscribe before reading: any change during the first snapshot stays queued.
    // Watch all logind signals so User.Display, SessionNew/Removed and invalidated
    // properties also re-resolve the graphical session instead of trusting a stale path.
    let rule = MatchRule::builder().msg_type(zbus::message::Type::Signal)
        .sender(SERVICE)?.path_namespace(ROOT)?.build();
    let mut signals = tokio::time::timeout(CALL_TIMEOUT, MessageStream::for_match_rule(rule, connection, Some(64))).await
        .map_err(|_| zbus::Error::Failure("lock subscription timed out".into()))??;
    publish_lock_state(sender, snapshot(connection).await?);
    let mut reconcile = tokio::time::interval(RETRY);
    reconcile.tick().await;
    loop {
        tokio::select! {
            event = signals.next() => { event.ok_or_else(|| zbus::Error::Failure("system bus closed".into()))??; },
            _ = reconcile.tick() => {},
        }
        publish_lock_state(sender, snapshot(connection).await?);
    }
}

pub(super) async fn monitor(sender: watch::Sender<Option<bool>>) {
    let mut degraded = false;
    loop {
        let result = tokio::time::timeout(CALL_TIMEOUT, async { zbus::connection::Builder::system()?.method_timeout(CALL_TIMEOUT).build().await }).await;
        if let Ok(Ok(connection)) = result {
            if !degraded { log::debug!("Desktop lock monitor: logind D-Bus"); }
            // Recovery is announced only after a valid snapshot, via the state channel.
            let _ = monitor_connection(&connection, &sender).await;
        }
        publish_lock_state(&sender, None);
        if !degraded { log::warn!("Desktop lock monitor unavailable; retrying D-Bus with bounded loginctl fallback every 30s"); }
        degraded = true;
        let fallback = tokio::task::spawn_blocking(super::locked_now).await.ok().flatten();
        publish_lock_state(&sender, fallback);
        tokio::time::sleep(RETRY).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};
    use zbus::zvariant::{OwnedObjectPath, Value};

    struct User(Arc<Mutex<String>>);
    #[zbus::interface(name = "org.freedesktop.login1.User")]
    impl User {
        #[zbus(property)]
        fn display(&self) -> (String, OwnedObjectPath) {
            let id = self.0.lock().unwrap().clone();
            let path = if id.is_empty() { "/".into() } else { format!("{ROOT}/session/{id}") };
            (id, OwnedObjectPath::try_from(path).unwrap())
        }
    }
    struct Session { locked: Arc<AtomicBool>, valid: Arc<AtomicBool> }
    #[zbus::interface(name = "org.freedesktop.login1.Session")]
    impl Session {
        #[zbus(property)]
        fn locked_hint(&self) -> zbus::fdo::Result<bool> {
            if !self.valid.load(Ordering::SeqCst) { return Err(zbus::fdo::Error::Failed("unavailable".into())); }
            Ok(self.locked.load(Ordering::SeqCst))
        }
    }

    async fn expect(rx: &mut watch::Receiver<Option<bool>>, value: Option<bool>) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let current = *rx.borrow_and_update();
                if current == value { return; }
                rx.changed().await.unwrap();
            }
        }).await.expect("lock state update");
    }

    async fn invalidated(server: &Connection, path: &str, interface: &str, property: &str) {
        // An invalidation has no replacement value: the client must re-read it.
        let changed = std::collections::HashMap::<&str, Value<'_>>::new();
        server.emit_signal(None::<&str>, path, "org.freedesktop.DBus.Properties", "PropertiesChanged",
            &(interface, changed, vec![property])).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "isolated D-Bus integration; requires dbus-daemon, never changes the real desktop"]
    async fn private_bus_tracks_initial_state_invalidation_session_change_and_failure() {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut daemon = tokio::process::Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null())
            .kill_on_drop(true).spawn().unwrap();
        let mut address = String::new();
        tokio::time::timeout(Duration::from_secs(3), BufReader::new(daemon.stdout.take().unwrap()).read_line(&mut address)).await.unwrap().unwrap();
        let address = address.trim();
        let display = Arc::new(Mutex::new(String::new()));
        let locked = Arc::new(AtomicBool::new(true));
        let valid = Arc::new(AtomicBool::new(true));
        let user_path = format!("{ROOT}/user/_{}", unsafe { libc::getuid() });
        let server = zbus::connection::Builder::address(address).unwrap().name(SERVICE).unwrap()
            .serve_at(user_path.clone(), User(display.clone())).unwrap()
            .serve_at(format!("{ROOT}/session/a"), Session { locked: locked.clone(), valid: valid.clone() }).unwrap()
            .serve_at(format!("{ROOT}/session/b"), Session { locked: Arc::new(AtomicBool::new(true)), valid: valid.clone() }).unwrap()
            .build().await.unwrap();
        let client = zbus::connection::Builder::address(address).unwrap().build().await.unwrap();
        let (tx, mut rx) = watch::channel(Some(false));
        let monitor = tokio::spawn(async move {
            let result = monitor_connection(&client, &tx).await;
            publish_lock_state(&tx, None);
            result
        });
        expect(&mut rx, None).await;
        assert!(!monitor.is_finished(), "no graphical session must keep the D-Bus subscription alive");
        *display.lock().unwrap() = "a".into();
        invalidated(&server, &user_path, "org.freedesktop.login1.User", "Display").await;
        expect(&mut rx, Some(true)).await; // login is observed without waiting for the retry timer
        locked.store(false, Ordering::SeqCst);
        invalidated(&server, &format!("{ROOT}/session/a"), "org.freedesktop.login1.Session", "LockedHint").await;
        expect(&mut rx, Some(false)).await;
        *display.lock().unwrap() = "b".into();
        invalidated(&server, &user_path, "org.freedesktop.login1.User", "Display").await;
        expect(&mut rx, Some(true)).await;
        display.lock().unwrap().clear();
        invalidated(&server, &user_path, "org.freedesktop.login1.User", "Display").await;
        expect(&mut rx, None).await;
        assert!(!monitor.is_finished(), "logout must keep the D-Bus subscription alive");
        *display.lock().unwrap() = "b".into();
        invalidated(&server, &user_path, "org.freedesktop.login1.User", "Display").await;
        expect(&mut rx, Some(true)).await;
        valid.store(false, Ordering::SeqCst);
        invalidated(&server, &format!("{ROOT}/session/b"), "org.freedesktop.login1.Session", "LockedHint").await;
        expect(&mut rx, None).await; // failure must not become unlocked
        assert!(monitor.await.unwrap().is_err());
        // A fresh connection must recover after a service/property failure.
        valid.store(true, Ordering::SeqCst);
        let client = zbus::connection::Builder::address(address).unwrap().build().await.unwrap();
        let close = client.clone();
        let (tx, mut rx) = watch::channel(None);
        let monitor = tokio::spawn(async move {
            let result = monitor_connection(&client, &tx).await;
            publish_lock_state(&tx, None);
            result
        });
        expect(&mut rx, Some(true)).await;
        close.close().await.unwrap();
        expect(&mut rx, None).await;
        assert!(monitor.await.unwrap().is_err());
        daemon.kill().await.unwrap();
        daemon.wait().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "manual acceptance: observes a real lock/unlock for up to three minutes"]
    async fn live_lock_unlock_signals() {
        let before = super::super::COMMAND_STARTS.load(Ordering::SeqCst);
        let mut rx = super::super::watch_locked();
        let start = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(180), async {
            let mut saw_lock = false;
            loop {
                let state = *rx.borrow_and_update();
                println!("live lock state: {state:?}; elapsed_ms={}", start.elapsed().as_millis());
                if state == Some(true) { saw_lock = true; }
                if saw_lock && state == Some(false) { break; }
                rx.changed().await.unwrap();
            }
        }).await.expect("please lock and unlock the desktop during this probe");
        let started = super::super::COMMAND_STARTS.load(Ordering::SeqCst) - before;
        println!("lock monitor command starts: {started}");
        assert_eq!(started, 0, "healthy D-Bus monitoring should not spawn loginctl");
    }

    #[tokio::test]
    #[ignore = "read-only live logind probe; requires an active graphical logind session"]
    async fn live_logind_snapshot_matches_loginctl_without_changing_lock_state() {
        let connection = Connection::system().await.unwrap();
        let dbus = snapshot(&connection).await.unwrap();
        let command = tokio::task::spawn_blocking(super::super::locked_now).await.unwrap();
        assert_eq!(dbus, command);
    }
}
