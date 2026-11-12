# Lock monitor: audit evidence and validation

## Historical audit evidence

Environment: Arch Linux, Hyprland/Wayland, systemd-logind. The retained records were collected for a 2026-11-09 to 2026-11-12 local investigation. The raw audit archive also contains unrelated commands and potentially sensitive arguments, so it is not published. The figures below are an aggregate of that retained archive, not a new measurement of all system activity after this patch.

```json
{
  "syscall_records": 956778,
  "lock_enoent_records": 553957,
  "percentage": 57.9,
  "source_gzip_sha256": "d666c24c30bac42e049ac2ba57d4cfa7114ac465f0f12b528c4247e0ca3d3fb5"
}
```

All 553,957 selected failures were syscall 59 (`execve`) on this x86_64 host.

The denominator is **SYSCALL records**, not all audit lines, bytes or attacks. The archive contains 138,481 successful LockedHint queries attributed to the two historical host processes, one per distinct second. Sampling the first 100 failed lock-thread events found missing loginctl candidates in the four PATH directories preceding /usr/bin. A successful lookup can therefore produce roughly four failed execve records.

The local audit retention was 64 MB per file, 16 files. The oldest approximately 4 hours 39 minutes of the intended 72-hour investigation had already rotated away. This loop is a confirmed large contributor, but the whole retention gap cannot be attributed to UU.

To independently recount the two raw-audit figures, use this read-only script on an exported plain/gzip audit file. It prints only totals, not command arguments, identities, paths or credentials:

```python
import gzip, json, re, sys
path = sys.argv[1]
open_log = gzip.open if path.endswith(".gz") else open
total = failed = 0
with open_log(path, "rt", errors="replace") as records:
    for line in records:
        if not line.startswith("type=SYSCALL "):
            continue
        total += 1
        if (re.search(r'\bcomm="uu-lock-state"(?:\s|$)', line)
                and re.search(r'\bexit=-2(?:\s|$)', line)):
            failed += 1
print(json.dumps({"syscall_records": total,
                  "lock_enoent_records": failed,
                  "percentage": round(100 * failed / total, 2) if total else None}))
```

This record format is the exported audit input used here; other export formats may require adapting the type/field parser. The archive hash identifies the private input used for this count; it is not a substitute for independent access to that input.

## Reproducible integration evidence

The ignored test below starts a **private** dbus-daemon and serves mock logind User/Session properties. It never changes the real desktop state. It covers initial locked state, invalidated LockedHint, Display session replacement, property-read failure remaining unknown, state recovery on a fresh connection, and bus connection closure. Requires dbus-daemon.

```sh
cargo test --locked --features wayland session_lock::logind::tests::private_bus -- --ignored
```

Result: 1 passed, 0 failed. This proves the tested D-Bus property/signal and reconnection primitives; it does not simulate an actual system-bus/logind daemon restart or the entire retry loop.

Read-only comparison with the real logind session:

```sh
cargo test --locked --features wayland session_lock::logind::tests::live_logind_snapshot -- --ignored
```

Result: 1 passed, 0 failed; direct D-Bus LockedHint agreed with loginctl.

## Human-assisted lock/unlock on hardware

The actual watch_locked entry point ran in a separate test process on Arch/Hyprland while the operator kept the existing UU viewer connected, manually locked and then unlocked the desktop. The operator reported the existing UU connection remained normal.

```sh
cargo test --locked --features wayland session_lock::logind::tests::live_lock_unlock_signals -- --ignored --nocapture
```

Sanitized output from that run:

```text
live lock state: None; elapsed_ms=0
live lock state: Some(false); elapsed_ms=2
live lock state: Some(true); elapsed_ms=43484
live lock state: Some(false); elapsed_ms=55507
lock monitor command starts: 0
test result: ok. 1 passed; 0 failed; finished in 55.51s
```

Times are observations since test start, **not response latency measurements**. The command count comes from a test-only counter at the bounded command entry point, not from whole-system auditing. The manual test runs alone, avoiding other tests that deliberately spawn a command. It demonstrates that the real monitoring entry point used no loginctl command across this observed lock/unlock cycle.

The running UU host was still the installed v0.2.57, not a deployment of the full patched binary. Thus this is a hardware check of the new monitor alongside an existing UU session, not complete patched-host end-to-end acceptance. No whole-system audit-volume reduction percentage was measured. Suspend/resume, real daemon restarts, other desktops and long-term running remain to be checked.

## Regression gates

The tests also cover preserving unknown state, suppressing duplicate notifications, and terminating/reaping a timed-out fallback command. Environment-dependent ignored tests above were executed explicitly; ignored is not counted as passed.

The submission PR records the default/Wayland test totals and release-build results for the final branch. The normal monitor uses D-Bus notifications plus a 30-second D-Bus reconciliation; degraded fallback may delay state updates by roughly 30 seconds plus connection/query time. The locker must still report its state to logind. No audit rules or installed service configuration are changed by this patch.

Submission gates after rebasing on upstream 890c46e: default tests 366 passed / 14 ignored; Wayland tests 366 passed / 16 ignored; explicit private-bus test 1 passed; explicit real-session snapshot test 1 passed; Wayland release build passed. The human-assisted result above is retained from the same monitor implementation before the rebase; the intervening upstream commits touch WoL and a terminal test, not this monitor.
