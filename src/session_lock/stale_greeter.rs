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

use std::os::unix::fs::MetadataExt;
use std::time::Duration;

/// A healthy greeter is gone within milliseconds of the unlock.
const GRACE: Duration = Duration::from_secs(3);
/// `comm` is cut to 15 bytes: "kscreenlocker_greet".
const COMM: &str = "kscreenlocker_g";

/// (pid, start time in clock ticks): the start time tells a reused pid from the same process.
type Greeter = (libc::pid_t, u64);

/// `/proc/<pid>/stat` → (comm, starttime). comm may contain spaces and parentheses.
fn parse_stat(stat: &str) -> Option<(&str, u64)> {
    let (open, close) = (stat.find('(')?, stat.rfind(')')?);
    let start = stat.get(close + 1..)?.split_ascii_whitespace().nth(19)?.parse().ok()?; // field 22
    Some((stat.get(open + 1..close)?, start))
}

fn start_time(pid: libc::pid_t) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(&stat).filter(|(comm, _)| *comm == COMM).map(|(_, start)| start)
}

fn greeters() -> Vec<Greeter> {
    let uid = unsafe { libc::getuid() };
    let Ok(entries) = std::fs::read_dir("/proc") else { return Vec::new() };
    entries.flatten().filter_map(|entry| {
        let pid: libc::pid_t = entry.file_name().to_str()?.parse().ok()?;
        if entry.metadata().ok()?.uid() != uid { return None; }
        Some((pid, start_time(pid)?))
    }).collect()
}

/// Call when the session changes from locked to unlocked. Greeters alive at that moment belong
/// to the lock that just ended; any still running after `GRACE` is stuck and is killed.
pub(super) fn reap_after_unlock() {
    let spawned = std::thread::Builder::new().name("uu-greeter-reap".into()).spawn(|| {
        let seen = greeters();
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
        assert_eq!(parse_stat(stat), Some(("kscreenlocker_g", 7654321)));
        let odd = "7 (a b) c) R 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 42 0";
        assert_eq!(parse_stat(odd), Some(("a b) c", 42)));
        assert_eq!(parse_stat("7 (short) R 1"), None);
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn only_a_greeter_has_a_start_time() {
        // This test process is not a greeter; a pid that does not exist has no stat at all.
        assert_eq!(start_time(std::process::id() as libc::pid_t), None);
        assert_eq!(start_time(libc::pid_t::MAX), None);
    }
}
