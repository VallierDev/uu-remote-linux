# Issue 27: display-change-assisted Portal recovery

## Policy

GNOME Portal handles subscribe to Mutter's `MonitorsChanged` on their existing
private D-Bus context. The callback records a monotonic timestamp under the frame
mutex; it does not rebuild a session or block the D-Bus loop.

A previously streaming capture can report unhealthy early only while continuously
`PAUSED` with no valid negotiated format. A notification received within 200 ms
before this interruption, or during it, enables a 200 ms grace from the later of
notification receipt and interruption start. Repeated notifications during the
same invalid pause do not extend that deadline. Accepted valid format or return
to `STREAMING` discards the notification hint.

The existing five-second timeout remains independent. Missing Mutter signals,
non-GNOME desktops and unrelated interruptions keep that fallback. Static video,
a valid-format pause and display notifications alone do not trigger replacement.
Capture failure does not mark input or clipboard unhealthy. Existing Rust recovery
still serializes replacement, preserves permission handling and backs off errors;
its one-second lock-state grace remains unchanged.

The 200 ms value is an initial engineering choice based on measured event skew,
not a demonstrated upper bound on all healthy renegotiations. The grace allows
natural recovery and the existing Rust health recheck cancels replacement if the
stream becomes healthy before the opener runs.

## Measurements

Environment: Ubuntu 26.04, GNOME 50 Wayland, one 3840x2160 display. Tests changed
only scale between 200% and 166.67%; physical frame dimensions stayed unchanged.

Before implementation, direct numeric callback probes used the same monotonic
clock as the D-Bus observer. Across four interruptions, `PAUSED` preceded display
notification receipt by 5.3-8.6 ms; first NULL format followed it by 9.8-36.3 ms.
Three single transitions and a two-second-spaced pair recovered through the
original timeout. Notification-to-session-restored was about 6.15-6.18 s;
post-replacement capture read was about 6.17-6.21 s. A user estimated roughly
three seconds frozen plus two seconds black in one official-viewer trial; those
visual estimates were not synchronized to host timestamps.

The candidate ran as an independent real Portal capture/H.264 encoder probe on
the same desktop, alongside the unchanged production host. It did not register
another cloud host or save screen contents. Results:

| Trial | Notification to restored session |
| --- | ---: |
| 166.67% to 200% | 1.396 s |
| 200% to 166.67% | 1.360 s |
| Rapid pair, first notification to restore | 1.352 s |

The rapid pair's notifications were 163 ms apart and produced one replacement.
A subsequent 30-second static observation produced no additional replacement.
The probe completed 3,600 encoded frames with an initial IDR. Display scale was
restored to its starting 166.67%; temporary Mutter changes did not overwrite the
saved display configuration.

Candidate timing uses monotonic notification receipt and observation of the
restored log in a file polled every 5 ms. It indicates approximately 4.8 seconds
less host recovery delay on this machine. It does not measure official-viewer
rendering, pointer acceptance or black-screen duration for the candidate. No
successful real natural-renegotiation sample was observed; cancellation is covered
by deterministic tests. Other GNOME versions remain unverified. Multi-monitor,
physical-resolution and rotation support are outside this change.

## Validation

- `cargo test --locked`: 183 passed, 5 ignored.
- `cargo test --locked --features wayland,portal`: 191 passed, 6 ignored.
- `cargo build --locked --release --features wayland,portal`: passed.
- Native Portal engine, input, EIS and desktop-power suites passed ASan/UBSan;
  EIS integration covered 13 geometries.
- Deterministic cases cover all six event orders, grace boundaries, repeated and
  delayed notifications, stale hints, startup, static video, valid pauses,
  natural recovery before/after the deadline, and clipboard health during an
  accelerated capture timeout.
- A private test bus verifies actual notification delivery and queued-signal
  teardown both with a D-Bus worker and before worker startup.

The installed/running production binary and service configuration were unchanged
during this validation. Official-viewer acceptance of the candidate remains a
separate final check, not evidence supplied by the independent encoder probe.
