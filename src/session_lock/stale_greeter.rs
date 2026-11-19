//! KDE lock screen: remove a greeter that outlives its own unlock.
//!
//! kscreenlocker (read at v6.3.5): after a password unlock the greeter prints "Unlocked", ksld
//! unlocks, and the greeter leaves its event loop. `~PamAuthenticator` then waits for the
//! fingerprint PAM thread, which sits in `pam_fprintd.so timeout=-1` until a finger is read
//! (cancellation only reaches a PAM conversation callback). A remote user cannot touch the
//! reader, so the process stays. The next lock grabs input and shows the lock window, but
//! `QProcess::start` refuses ("Process is already running"): no greeter is drawn, and when the
//! old process finally exits 0 ksld takes that as an unlock.
//!
//! SIGTERM is useless here (the greeter handles it in the event loop it already left), so the
//! leftover is killed. ksld ignores that exit while unlocked (`s_lockProcessRequestedExit`); if
//! the user already locked again, ksld sees a crashed greeter and starts a fresh one.
//!
//! Only greeters of the logind session whose unlock was observed (`User.Display`) are touched:
//! the same user can have a second graphical session that is still locked, and its greeter is
//! healthy. A greeter whose session cannot be determined is left alone.

use std::os::unix::fs::MetadataExt;
use std::time::Duration;

/// A healthy greeter is gone within milliseconds of the unlock.
const GRACE: Duration = Duration::from_secs(3);
/// `comm` is cut to 15 bytes: "kscreenlocker_greet".
const COMM: &str = "kscreenlocker_g";

/// (pid, start time in clock ticks): the start time tells a reused pid from the same process.
type Greeter = (libc::pid_t, u64);

/// `/proc/<pid>/stat` → (comm, ppid, starttime). comm may contain spaces and parentheses.
fn parse_stat(stat: &str) -> Option<(&str, libc::pid_t, u64)> {
    let (open, close) = (stat.find('(')?, stat.rfind(')')?);
    let mut rest = stat.get(close + 1..)?.split_ascii_whitespace();
    let parent = rest.nth(1)?.parse().ok()?; // field 4
    let start = rest.nth(17)?.parse().ok()?; // field 22
    Some((stat.get(open + 1..close)?, parent, start))
}

/// (ppid, starttime) of a greeter; None for any other process.
fn greeter_stat(pid: libc::pid_t) -> Option<(libc::pid_t, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(&stat).filter(|(comm, ..)| *comm == COMM).map(|(_, parent, start)| (parent, start))
}

fn start_time(pid: libc::pid_t) -> Option<u64> { greeter_stat(pid).map(|(_, start)| start) }

/// `/proc/<pid>/cgroup` → logind session id, when the process sits in `session-<id>.scope`.
fn cgroup_session(cgroup: &str) -> Option<&str> {
    cgroup.lines().flat_map(|line| line.rsplit('/'))
        .find_map(|part| part.strip_prefix("session-")?.strip_suffix(".scope")).filter(|id| !id.is_empty())
}

/// `/proc/<pid>/environ` → `XDG_SESSION_ID`.
fn environ_session(environ: &[u8]) -> Option<&str> {
    environ.split(|&byte| byte == 0).find_map(|var| var.strip_prefix(b"XDG_SESSION_ID="))
        .and_then(|id| std::str::from_utf8(id).ok()).filter(|id| !id.is_empty())
}

/// The session scope is authoritative where there is one. A systemd-managed Plasma runs ksmserver
/// and the greeter under `user@<uid>.service` instead (observed on Plasma 6.3: cgroup
/// `plasma-ksmserver.service`, and `/proc/<pid>/sessionid` names the user manager's own session,
/// not the graphical one); there the environment imported from the graphical session names it.
fn session_from(cgroup: &str, environ: &[u8]) -> Option<String> {
    cgroup_session(cgroup).or_else(|| environ_session(environ)).map(str::to_owned)
}

fn session_of(pid: libc::pid_t) -> Option<String> {
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap_or_default();
    let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
    session_from(&cgroup, &environ)
}

/// The greeter makes itself non-dumpable (`prctl(PR_SET_DUMPABLE, 0)` in its main.cpp), so its
/// `environ` cannot be read; the process that started it (ksld in ksmserver / kwin_wayland)
/// belongs to the same session and can be.
fn belongs_to(pid: libc::pid_t, parent: libc::pid_t, session: &str) -> bool {
    session_of(pid).or_else(|| session_of(parent)).as_deref() == Some(session)
}

fn greeters(session: &str) -> Vec<Greeter> {
    let uid = unsafe { libc::getuid() };
    let Ok(entries) = std::fs::read_dir("/proc") else { return Vec::new() };
    entries.flatten().filter_map(|entry| {
        let pid: libc::pid_t = entry.file_name().to_str()?.parse().ok()?;
        if entry.metadata().ok()?.uid() != uid { return None; }
        let (parent, start) = greeter_stat(pid)?;
        belongs_to(pid, parent, session).then_some((pid, start))
    }).collect()
}

/// Call when the display session changes from locked to unlocked. Its greeters alive at that
/// moment belong to the lock that just ended; any still running after `GRACE` is stuck and is killed.
pub(super) fn reap_after_unlock() {
    let spawned = std::thread::Builder::new().name("uu-greeter-reap".into()).spawn(|| {
        let Some(session) = super::display_session() else { return };
        let seen = greeters(&session);
        if seen.is_empty() { return; }
        std::thread::sleep(GRACE);
        let killed = seen.into_iter()
            .filter(|&(pid, start)| start_time(pid) == Some(start) && unsafe { libc::kill(pid, libc::SIGKILL) } == 0)
            .count();
        if killed > 0 { log::warn!("Lock screen greeter still running after unlock; removed: count={killed}"); }
    });
    if let Err(error) = spawned { log::warn!("Lock screen greeter check not started: {}", error.kind()); }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_line_gives_comm_and_start_time() {
        let stat = "438154 (kscreenlocker_g) S 2217 2217 2217 0 -1 4194304 9 0 0 0 5 3 0 0 20 0 12 0 7654321 1 2 3";
        assert_eq!(parse_stat(stat), Some(("kscreenlocker_g", 2217, 7654321)));
        let odd = "7 (a b) c) R 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 42 0";
        assert_eq!(parse_stat(odd), Some(("a b) c", 1, 42)));
        assert_eq!(parse_stat("7 (short) R 1"), None);
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn session_comes_from_the_scope_or_the_environment() {
        assert_eq!(cgroup_session("0::/user.slice/user-1000.slice/session-3.scope\n"), Some("3"));
        assert_eq!(cgroup_session("1:name=systemd:/user.slice/user-1000.slice/session-c2.scope/sub\n"), Some("c2"));
        assert_eq!(cgroup_session("0::/user.slice/user-1000.slice/user@1000.service/session.slice/plasma-ksmserver.service\n"), None);
        assert_eq!(cgroup_session("0::/session-.scope\n"), None);
        assert_eq!(environ_session(b"DISPLAY=:0\0XDG_SESSION_ID=3\0XDG_VTNR=2\0"), Some("3"));
        assert_eq!(environ_session(b"MY_XDG_SESSION_ID=9\0XDG_SESSION_ID=\0"), None);
        assert_eq!(environ_session(b""), None);
    }

    #[test]
    fn a_greeter_of_another_or_unknown_session_is_not_selected() {
        let managed = "0::/user.slice/user-1000.slice/user@1000.service/session.slice/plasma-ksmserver.service\n";
        let is = |cgroup: &str, environ: &[u8], session: &str| session_from(cgroup, environ).as_deref() == Some(session);
        // Two graphical sessions of one user, session 3 just unlocked, session 7 still locked.
        assert!(is(managed, b"XDG_SESSION_ID=3\0", "3"));
        assert!(!is(managed, b"XDG_SESSION_ID=7\0", "3"));
        assert!(is("0::/user.slice/user-1000.slice/session-3.scope\n", b"", "3"));
        assert!(!is("0::/user.slice/user-1000.slice/session-7.scope\n", b"", "3"));
        // The scope wins over an inherited variable; "3" must not match "31".
        assert!(!is("0::/user.slice/user-1000.slice/session-7.scope\n", b"XDG_SESSION_ID=3\0", "3"));
        assert!(!is(managed, b"XDG_SESSION_ID=31\0", "3"));
        // Unknown session (the greeter's own unreadable environ, or nothing at all): no answer.
        assert_eq!(session_from(managed, b""), None);
        assert_eq!(session_from(managed, b"DISPLAY=:0\0"), None);
        assert_eq!(session_from("", b""), None);
    }

    #[test]
    fn a_process_without_a_known_session_belongs_to_none() {
        // Neither a missing process nor a missing parent may select a greeter.
        assert!(!belongs_to(libc::pid_t::MAX, libc::pid_t::MAX, "3"));
        assert!(!belongs_to(libc::pid_t::MAX, libc::pid_t::MAX, ""));
    }

    #[test]
    fn only_a_greeter_has_a_start_time() {
        // This test process is not a greeter; a pid that does not exist has no stat at all.
        assert_eq!(start_time(std::process::id() as libc::pid_t), None);
        assert_eq!(start_time(libc::pid_t::MAX), None);
    }
}
