# Send-side bitrate response

## Problem and cause

Symptom: the viewer's picture freezes while the desktop keeps reacting to its clicks. Capture is
healthy — `Portal frames: new=897 repeated=4 window_s=30` and `Desktop latest-frame stats: sent=900`
keep counting — so the frames leave the host and never arrive. The viewer's own report agrees:
`avg_received_fps` decays 29 → 26 → 24 → 22 → 0 while the data channel (input) still works.

Cause: webrtc-rs 0.10 has no congestion control. An overloaded path only produces NACKs, and the
retransmissions multiply the traffic until nothing gets through. Measured on a hotel Wi-Fi relay path
with 6 Mbit/s requested:

```
21:33:32 Video receiver report: lost=0     fraction_lost=0   rtt=17ms    nack=16
21:33:33 Video receiver report: lost=425   fraction_lost=25  rtt=123ms   nack=10559
21:33:52 Video receiver report: lost=4746  fraction_lost=25  rtt=1701ms  nack=142733
21:34:12 Video receiver report: lost=26677 fraction_lost=25   rtt=2729ms  nack=261952
21:34:02 ICE path: tx_bps=Some(45780471)                                 ← 0.8-2 Mbit/s when healthy
```

Two facts matter for the fix: the desktop is still captured and encoded the whole time (so this is
not a capture or privacy-screen problem), and the viewer's 自动清晰度 is not a substitute — it
translates "auto" into `quality=6, custom_bitrate=u64::MAX` (i.e. unlimited), so the host really does
push 20-30 Mbit/s until the path breaks, and only then does the client lower its preset. By that
point the RTT is seconds and the picture is already dead.

## Implementation

`src/video_governor.rs` keeps the encoder's bitrate cap at or below what RTCP says the path carries.
It reads the receiver reports the feedback task already parses and, when the cap changes, writes
`VideoSettings.net_kbps`; the existing settings watch restarts the encoder through its normal
`reader.restart` path (RTP timeline preserved, first frame is an IDR).

- Signals: `fraction_lost >= 8%` or `rtcp_rtt_ms >= 300`. NACK counts are deliberately **not** a
  trigger: they arrive in bursts (hundreds within 100 ms on a clean path), so an instantaneous rate
  reads as congestion on a perfect link and makes the cap sawtooth (measured 0% loss, 18 ms RTT being
  halved).
- Decrease: halve, floor 800 kbit/s, at most every 3 s. The rate that broke the path is remembered and
  recovery aims at 90% of it, so a path whose capacity is 3 Mbit/s does not retry 3.1 Mbit/s every
  twenty seconds.
- Increase: +25% (at least 500 kbit/s) every 4 s, never above the aim. After 60 s without loss the aim
  is ignored once, which is the only way to discover that the path improved.
- Ceiling: the viewer's own choice (`max_kbps`) stays the ceiling; when it is absent or unlimited, the
  ceiling is `AUTO_CEILING_KBPS` (8 Mbit/s) instead of the encoder default. The viewer therefore stays
  in charge of 画质, and this only lowers it when the path actually loses packets.
- Between the knee and the cliff (1-8% loss, 150-300 ms RTT) nothing changes, so a marginal path does
  not flap the encoder.

Fields on the wire are untouched: `net_kbps` is local to the connection, never sent, and the viewer's
own settings keep their meaning (`requested_kbps()` vs `max_kbps()`).

## Verification

`cargo test --bin uuremote-server video_governor` covers the halving sequence down to the floor, a
swollen RTT alone, 3.5% and 6% jitter leaving the cap alone, recovery stopping at 90% of the broken
rate, the 60 s probe reaching the viewer ceiling again, and an unlimited viewer choice being capped at
8 Mbit/s.

Field check: with a 1-3 Mbit/s mobile path the log shows `Video bitrate cap` only after real loss
(12-27%) and the picture recovers instead of freezing; at 8 Mbit/s over a good path the governor never
fires. Diagnose a fresh freeze by the two lines above: `Portal frames: new=0` means the compositor
stopped painting (capture side), `new≈30` with `sent` climbing and no frames at the viewer means the
transport.

## Tuning

The thresholds are constants at the top of `src/video_governor.rs`
(`CONGESTION_LOSS_PERCENT`, `CLEAN_LOSS_PERCENT`, `DECREASE_AFTER`, `INCREASE_AFTER`, `PROBE_AFTER`,
`FLOOR_KBPS`, `AUTO_CEILING_KBPS`). Raising the loss threshold means fewer encoder restarts and
longer tolerance of jitter; lowering it reacts earlier at the cost of a visible hiccup (each change
restarts the encoder, because the encoder cannot change its bitrate while running).
