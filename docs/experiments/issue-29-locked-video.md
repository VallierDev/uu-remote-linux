# Issue 29: iPhone cold locked auto unlock

## Observed failure and controlled result

On a cold locked GNOME Portal connection, the official iPhone 4.42.0 viewer
completed CONTROL Echo/queries and received secure-desktop state, but did not
send SimpleAction 15. Replaying secure state and announcing CaptureChange early
were insufficient. The experiment varied synthetic video alone; actual pending
capture permissions remained Unknown, with no extra screen/codec initialization.

All times below are October 17, 2026, Asia/Taipei (UTC+08:00). All three trials
used the identical experimental executable, SHA-256
`11b4de0328ca6b4b656d225c1de37f95cdcb104f0ef458b76e2444bcc3102663`.
Only its temporary `UU_LOCKED_VIDEO_PROBE` flag changed. Each trial restarted the
service while GNOME was locked, with iPhone as the first desktop viewer.

| Trial | Black video | Safe host evidence | Human result |
|---|---|---|---|
| First cold ON | Enabled | H.265 frame completed 11:13:55.160912; action 15 at 11:13:55.204820; unlocked 11:13:55.786608; auto unlock Success 11:13:55.819243 | Real desktop recovered |
| Cold OFF | Disabled; restarted 11:17:52.880738 | Connected 11:18:04.342190; no synthetic frame, action 15 or unlock result; closed 11:18:12.439870 | Failed |
| Independent cold ON repeat | Enabled; restarted 11:19:08.293631 | First H.265 frame 11:19:14.108291; action 15 at 11:19:15.868938; auto unlock Success 11:19:16.682968; unlocked 11:19:16.735692; real initialization 11:19:16.993977 | Real desktop recovered |

The first trial sent one synthetic frame, followed by 857 real frames and growing
receiver sequence reports. The independent repeat sent three synthetic frames.
An additional successful connection at 11:16 had prior Portal history and is not
counted as a cold trial. The OFF trial's unlock at 11:18:16.471908 occurred after
closure and is not remote-unlock success.

The ON/OFF/ON result supports missing pending-locked video as the actionable host
dependency for this client. It does not establish the iOS implementation, prove
that one frame is always sufficient, or justify invented permission claims.

## Default implementation

`src/locked_video.rs` generates one independently decodable 1280x720 black IDR
using configured FFmpeg and libx264/libx265 for the negotiated H.264/H.265 codec.
Hardware encoding availability alone does not guarantee these software encoders;
encoding failure is reported. Generation has a five-second timeout and kills the
child on cancellation. The encoded access unit is cached for the desktop peer.

When Portal is pending, lock state is exactly locked and the peer is connected,
the host sends the frame once per second on the existing negotiated video track,
with capture index 0 and new-frame RTP metadata. It shares the real video's SSRC,
sequence and timestamp timeline. Logs count completed sends through the track's
transmission revision; a paused no-op write is not counted as transmitted video.
Each send is bounded to two seconds, and pacing resets after it completes.

The 1280x720 size belongs only to the synthetic bitstream. Screen inventory,
codec CONTROL initialization, actual permissions, input and clipboard gating
continue to follow Portal readiness. Unlock or unknown lock state pauses synthetic
video; capture ready/rejected or peer/SCTP closure ends it. Real streaming starts
after this task returns, so synthetic and live video do not write concurrently.
There is no experimental flag or attempt cap in the default implementation.

The frame is not a capture of GNOME's lock screen and cannot enable native input
there. The viewer still needs auto unlock enabled and the Linux login password.
Existing GNOME screen-off/lock policy and independent tool peers remain intact.

## Validation and limits

Codec tests decode H.264 and H.265 output into exactly 1280x720 black pixels.
Virtual-network WebRTC tests exercise suppression before connect and while
unlocked/unknown, pending-locked transmission, ready/rejected/closed lifecycle,
and transition to live video on the same RTP timeline. These tests require local
FFmpeg and are explicitly run despite being marked ignored in the normal suite.

The physical ON/OFF/ON acceptance above covers the experimental binary on this
GNOME host, iPhone 4.42.0 and H.265. The default implementation was then installed
and restarted while locked, with no experimental flag. Its installed and running
executable hashes both matched SHA-256
`576cf6a01d108c0311fb8139a85ad8fe582c1d43324c9b643246d394963fd379`.
The first desktop connection sent black video at 11:26:13.701815, received
action 15 at 11:26:17.707857 and reported auto unlock Success at
11:26:18.420772. Lock state became unlocked at 11:26:19.247680; synthetic video
ended after six completed frames, followed by real initialization at
11:26:19.342018 and receiver sequence growth. The user also confirmed that the
phone displayed the desktop normally on this final executable, completing
cold-connection acceptance of the default unlock and video handoff path.

Before submission, review identified a race when unlock or Portal readiness
arrived in the encoder's final poll. The source now prioritizes state-change
notifications and rechecks current Portal, lock and connection state before
encoding and before sending. A deterministic virtual-network regression publishes
unlock, unknown lock state or readiness during that final encoder poll and checks
that no synthetic RTP is emitted. This review follow-up is covered by automated
tests; the executable hashes and phone acceptance above describe the preceding
installed build, not a second physical acceptance of this follow-up.

H.264-only iPhone connections, other client versions, prolonged pending sessions,
other host/encoder combinations and slower WAN/relay paths are not established
by these trials. Full suspend or hibernate wake is outside this CONTROL path.

For future acceptance, preserve the executable hash and relevant settings, restart
while actually locked, keep iPhone as the first desktop viewer, and correlate
safe frame counts, action 15, unlock result, lock transition, Portal recovery and
real receiver reports. Do not count manual unlock or a Mac warm-up. RTP reception
alone does not prove phone decoding or visible desktop; obtain a human result.
Never export passwords, tokens, QR URLs or user content with diagnostic evidence.
