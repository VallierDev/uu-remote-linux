//! Send-side bitrate (and, when adapted, frame-rate) response to RTCP loss and RTT.
//!
//! webrtc-rs 0.10 has no congestion control: an overloaded path only produces NACKs, and the
//! retransmissions multiply the traffic until the peer receives nothing while its input still
//! works (measured: 6 Mbit/s requested on a ~1-2 Mbit/s relay, RTT 17 ms -> 2.9 s, 260k NACKs,
//! 80 Mbit/s on the wire, frozen picture until a reconnect).
//!
//! The viewer adapts its own 自动清晰度 preset, so this is only a safety net below that ceiling:
//! it reacts to real loss or a swollen round trip and never fights a healthy path. NACK counts are
//! not a trigger — they arrive in bursts (hundreds within 100 ms on a clean path), so an
//! instantaneous NACK rate reads as congestion on a perfect link and makes the cap sawtooth.
use std::time::{Duration, Instant};

/// Lowest cap we still encode at; below this the picture is not worth the stream.
pub const FLOOR_KBPS: u32 = 800;
/// Frame-rate ladder for adaptive frame drop, floored at the protocol's minimum of 5 fps:
/// once the bitrate cap is on the floor, fewer complete frames beat a frozen 60 fps stream.
const FPS_LADDER: [u32; 13] = [144, 120, 90, 60, 45, 30, 24, 20, 15, 12, 10, 8, 5];

/// Adaptive frame rate is opt-in (`UU_VIDEO_FPS_ADAPT=1`): with it off the governor behaves
/// exactly as before and never returns a frame-rate decision.
#[derive(Debug, Clone, Copy, Default)]
pub struct FpsAdapt { pub enabled: bool }

/// One report's encoder-affecting decisions. `fps` nests: Some(Some(v)) runs the encoder at v,
/// Some(None) lifts the governor's frame-rate floor (the viewer's nominal rate is free again),
/// None changes nothing.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Decision { pub cap: Option<u32>, pub fps: Option<Option<u32>> }
/// Ceiling when the viewer has no explicit limit (自动清晰度, or the viewer's "unlimited" custom rate).
pub const AUTO_CEILING_KBPS: u32 = 8_000;
/// A decrease restarts the encoder, so give each step time to show up in a report.
const DECREASE_AFTER: Duration = Duration::from_secs(3);
/// Increases are slower than decreases, and a restart costs a keyframe.
const INCREASE_AFTER: Duration = Duration::from_secs(4);
const MIN_STEP_KBPS: u32 = 500;
/// Recovery stops at 90% of the rate that broke the path; only a probe may go above it, this often.
const PROBE_AFTER: Duration = Duration::from_secs(60);
/// Loss that means the path is overrun. Wi-Fi jitters to 5%, and halving for that costs a keyframe
/// and a visible hiccup, so only a real burst (measured 12-27% when a path breaks) counts.
const CONGESTION_LOSS_PERCENT: f32 = 8.0;
/// Below this the path is fine and the cap may climb.
const CLEAN_LOSS_PERCENT: f32 = 1.0;
/// Round trip above which the path is backed up, and below which it has room to grow.
const CONGESTION_RTT_MS: f64 = 300.0;
const CLEAN_RTT_MS: f64 = 150.0;
/// RTT is the noisy input: one session measured 52 ms, 127 ms, 321 ms, 94 ms, 72 ms and 1300 ms,
/// all on a path that was working. Acting on one sample that crosses the knee halves the cap, the
/// next clean report raises it again, and every step restarts the encoder - 25 rebuilds in four
/// minutes. A decrease therefore needs the knee crossed in this many reports in a row.
const RTT_CONFIRMATIONS: u8 = 2;
/// A halved rate needs more than one clean report before climbing back, otherwise a decrease is
/// followed straight by an increase and the cap seesaws across the point where the path broke.
const RECOVERY_AFTER_DECREASE: Duration = Duration::from_secs(10);

#[derive(Default)]
pub struct Governor {
    /// The cap last returned to the caller, i.e. what settings.net_kbps holds. The encoder runs
    /// at `ceiling.min(cap)`, so this is only the governor's view while the ceiling matches;
    /// `effective` reconciles the two.
    cap: Option<u32>,
    /// The frame-rate step the governor has lowered the stream to; None = not intervening
    /// (or already climbed back to the viewer's rate).
    fps: Option<u32>,
    applied: Option<Instant>,
    /// Reports in a row that asked for the knee.
    rtt_over: u8,
    /// When the cap was last cut, to hold off the recovery that would undo it.
    decreased_at: Option<Instant>,
    /// Rate that last broke the path and when: recovery aims below it, so a path whose capacity is
    /// 3 Mbit/s does not retry 3.1 Mbit/s every twenty seconds (each retry is a keyframe and a
    /// burst of loss).
    broken_at: Option<(u32, Instant)>,
}

impl Governor {
    /// One RTCP receiver report: `loss_percent` is its fraction lost in percent, `ceiling_kbps`
    /// the viewer's ceiling, `current_fps`/`ceiling_fps` the running and viewer-nominal frame
    /// rates, `adapt` the frame-drop opt-in. Returns what the encoder must restart at.
    pub fn observe(&mut self, loss_percent: f32, rtt_ms: Option<f64>, ceiling_kbps: u32,
        current_fps: u32, ceiling_fps: u32, adapt: &FpsAdapt, now: Instant) -> Decision {
        // The current rate is measured against the viewer's ceiling itself: clamping the
        // ceiling up to the floor would leave the governor aiming at a rate the encoder never
        // had (a 500k custom stream capped below the floor looked like an applied 800k cap,
        // and after a raise to 6M congestion found "current == floor" and never stepped down).
        let current = self.effective(ceiling_kbps);
        let aim = self.broken_at
            .filter(|(_, at)| now.duration_since(*at) < PROBE_AFTER)
            .map_or(ceiling_kbps, |(broken, _)| (broken * 9 / 10).max(FLOOR_KBPS).min(ceiling_kbps));
        // Loss at that level only shows up when a path genuinely breaks, so one report is enough. A
        // round trip spike looks the same for a single report, so it has to hold across reports.
        let congested = loss_percent >= CONGESTION_LOSS_PERCENT
            || self.note_rtt(rtt_ms) >= RTT_CONFIRMATIONS;
        let clean = loss_percent < CLEAN_LOSS_PERCENT
            && rtt_ms.is_none_or(|rtt| rtt < CLEAN_RTT_MS)
            && self.settled(now);
        if congested {
            self.rtt_over = 0;
            // The cooldown runs from the most recent congestion, not from the last applied
            // decrease: at the floor no step is left to apply, and a stale timestamp let a
            // brief recovery probe back into the rate that broke the path a moment earlier.
            self.broken_at = Some((current, now));
            self.decreased_at = Some(now);
            if let Some(cap) = self.apply((current / 2).max(FLOOR_KBPS).min(current), current, DECREASE_AFTER, now) {
                return Decision { cap: Some(cap), ..Default::default() };
            }
            // The bitrate cannot go lower: at the floor the choice is dropped frames or fewer
            // frames, and a slow-moving picture beats a frozen one (60 fps over a broken
            // relay delivers neither, just a queue that only a reconnect empties).
            if adapt.enabled && current <= FLOOR_KBPS {
                if let Some(step) = self.step_fps(FPS_LADDER.iter().filter(|&&s| s < current_fps).max().copied(), DECREASE_AFTER, now) {
                    return Decision { fps: Some(Some(step)), ..Default::default() };
                }
            }
            return Decision::default();
        }
        if clean {
            if let Some(cap) = self.apply((current + (current / 4).max(MIN_STEP_KBPS)).min(aim.max(current)),
                current, INCREASE_AFTER, now) {
                return Decision { cap: Some(cap), ..Default::default() };
            }
        }
        // Frames climb after a quiet probe window even when the report is not "clean", and
        // not only once the bitrate is back at the ceiling: a relay path that has recovered
        // keeps a 1-8% loss or 150-300 ms RTT baseline that never reads clean (measured:
        // frames pinned at 5 fps for minutes on a path quietly streaming a megabyte; a
        // healthy 300 ms line pins the cap at 800-2000k for an hour with 0% loss), starving
        // exactly the links this feature rescues. Bitrate still climbs only on clean
        // reports — every cap step is a keyframe burst into a marginal path — but a frame
        // raise adds no load (the cap holds the total) and a mis-timed one costs only the
        // next congested report stepping back down; that, plus the window a fresh
        // congestion report restarts, is the anti-flap.
        let quiet = self.broken_at.map_or(true, |(_, at)| now.duration_since(at) >= PROBE_AFTER);
        if adapt.enabled && self.fps.is_some() && quiet {
            if current_fps >= ceiling_fps { self.fps = None; return Decision { fps: Some(None), ..Default::default() }; }
            let up = FPS_LADDER.iter().find(|&&s| s > current_fps).map(|&s| s.min(ceiling_fps));
            if let Some(step) = self.step_fps(up, INCREASE_AFTER, now) {
                return Decision { fps: Some(Some(step)), ..Default::default() };
            }
        }
        Decision::default()
    }

    /// Move the frame rate to a ladder step unless there is none or a restart gate is still
    /// closed; `applied` is shared with the bitrate steps so one encoder never restarts twice.
    fn step_fps(&mut self, target: Option<u32>, gate: Duration, now: Instant) -> Option<u32> {
        let target = target.filter(|&t| Some(t) != self.fps && self.applied.map_or(true, |at| now.duration_since(at) >= gate))?;
        self.applied = Some(now);
        self.fps = Some(target);
        Some(target)
    }

    /// What the encoder is actually capped at right now.
    fn effective(&self, ceiling_kbps: u32) -> u32 {
        self.cap.map_or(ceiling_kbps, |cap| cap.min(ceiling_kbps))
    }

    /// Hand out a new cap unless it equals the current rate or the restart gate has not passed.
    fn apply(&mut self, target: u32, current: u32, gate: Duration, now: Instant) -> Option<u32> {
        if target == current || self.applied.is_some_and(|at| now.duration_since(at) < gate) {
            return None;
        }
        self.applied = Some(now);
        self.cap = Some(target);
        Some(target)
    }

    /// How many reports in a row asked for the knee. A single spike is Wi-Fi, not a broken path.
    fn note_rtt(&mut self, sample: Option<f64>) -> u8 {
        self.rtt_over = if sample.is_some_and(|sample| sample >= CONGESTION_RTT_MS) {
            self.rtt_over.saturating_add(1)
        } else {
            0
        };
        self.rtt_over
    }

    /// Whether the path has had time to prove that a decrease it needed holds.
    fn settled(&self, now: Instant) -> bool {
        self.decreased_at.is_none_or(|at| now.duration_since(at) >= RECOVERY_AFTER_DECREASE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(governor: &mut Governor, at: Instant, loss: f32, rtt: Option<f64>) -> Option<u32> {
        governor.observe(loss, rtt, 6_000, 60, 60, &FpsAdapt::default(), at).cap
    }

    #[test]
    fn congestion_halves_the_cap_down_to_the_floor() {
        let mut governor = Governor::default();
        let start = Instant::now();
        assert_eq!(report(&mut governor, start, 20.0, Some(20.0)), Some(3_000));
        // A restart needs time to show up in the next report.
        assert_eq!(report(&mut governor, start + Duration::from_secs(1), 20.0, Some(20.0)), None);
        assert_eq!(report(&mut governor, start + DECREASE_AFTER, 20.0, Some(20.0)), Some(1_500));
        assert_eq!(report(&mut governor, start + DECREASE_AFTER * 2, 20.0, Some(20.0)), Some(FLOOR_KBPS));
        assert_eq!(report(&mut governor, start + DECREASE_AFTER * 3, 20.0, Some(20.0)), None);
    }

    #[test]
    fn a_round_trip_spike_leaves_the_cap_alone_but_a_sustained_one_halves_it() {
        let mut governor = Governor::default();
        let start = Instant::now();
        assert_eq!(report(&mut governor, start, 0.0, Some(20.0)), None);
        // One 1300 ms report on a working path, which is what a session actually measured: a single
        // sample over the knee is not a congested path, so the cap stays where it is.
        assert_eq!(report(&mut governor, start + Duration::from_secs(1), 0.0, Some(1_300.0)), None);
        assert_eq!(report(&mut governor, start + Duration::from_secs(2), 0.0, Some(72.0)), None);
        assert_eq!(governor.effective(6_000), 6_000);
        // A round trip that stays over the knee halves the cap on the second confirming report.
        assert_eq!(report(&mut governor, start + Duration::from_secs(3), 0.0, Some(900.0)), None);
        assert_eq!(report(&mut governor, start + Duration::from_secs(4), 0.0, Some(900.0)), Some(3_000));
    }

    #[test]
    fn a_decrease_holds_before_the_cap_climbs_again() {
        let mut governor = Governor::default();
        let start = Instant::now();
        assert_eq!(report(&mut governor, start, 20.0, Some(20.0)), Some(3_000));
        // Clean reports straight after the cut do not undo it.
        assert_eq!(report(&mut governor, start + INCREASE_AFTER, 0.0, Some(20.0)), None);
        assert_eq!(governor.cap, Some(3_000));
        assert_eq!(report(&mut governor, start + RECOVERY_AFTER_DECREASE, 0.0, Some(20.0)), Some(3_750));
    }

    #[test]
    fn a_clean_path_climbs_back_but_not_into_the_rate_that_broke_it() {
        let mut governor = Governor::default();
        let start = Instant::now();
        assert_eq!(report(&mut governor, start, 20.0, Some(900.0)), Some(3_000));
        // Inside the increase gate the cap stays put.
        assert_eq!(report(&mut governor, start + Duration::from_secs(2), 0.0, Some(20.0)), None);
        // Past the recovery cooldown the climb resumes, and still stops below the break.
        let mut at = start + RECOVERY_AFTER_DECREASE;
        while let Some(cap) = report(&mut governor, at, 0.0, Some(20.0)) {
            assert!(cap <= 5_400, "cap {cap} probed into the rate that broke the path");
            at += INCREASE_AFTER;
        }
        assert_eq!(governor.cap, Some(5_400));
    }

    #[test]
    fn a_probe_after_a_quiet_minute_may_reach_the_viewer_ceiling_again() {
        let mut governor = Governor::default();
        let start = Instant::now();
        assert_eq!(report(&mut governor, start, 20.0, Some(900.0)), Some(3_000));
        let mut at = start + PROBE_AFTER;
        let mut reached = None;
        while let Some(cap) = report(&mut governor, at, 0.0, Some(20.0)) {
            reached = Some(cap);
            at += INCREASE_AFTER;
        }
        assert_eq!(reached, Some(6_000));
    }

    #[test]
    fn wifi_jitter_below_the_threshold_leaves_the_cap_alone() {
        let mut governor = Governor::default();
        let start = Instant::now();
        assert_eq!(report(&mut governor, start, 3.5, Some(60.0)), None);
        assert_eq!(report(&mut governor, start + Duration::from_secs(2), 6.0, Some(90.0)), None);
        assert_eq!(governor.effective(6_000), 6_000);
    }

    #[test]
    fn an_auto_quality_session_halves_from_the_auto_ceiling() {
        let mut governor = Governor::default();
        let start = Instant::now();
        assert_eq!(governor.observe(20.0, Some(20.0), AUTO_CEILING_KBPS, 60, 60, &FpsAdapt::default(), start).cap, Some(4_000));
    }

    #[test]
    fn an_explicit_high_rate_recovers_to_the_viewers_choice() {
        let mut governor = Governor::default();
        let start = Instant::now();
        assert_eq!(governor.observe(20.0, Some(900.0), 20_000, 60, 60, &FpsAdapt::default(), start).cap, Some(10_000));
        let mut at = start + PROBE_AFTER;
        let mut reached = None;
        while let Some(cap) = governor.observe(0.0, Some(20.0), 20_000, 60, 60, &FpsAdapt::default(), at).cap {
            reached = Some(cap);
            at += INCREASE_AFTER;
        }
        assert_eq!(reached, Some(20_000));
    }

    #[test]
    fn a_rate_below_the_floor_does_not_blind_the_governor_to_a_raise() {
        let mut governor = Governor::default();
        let start = Instant::now();
        // 500k custom sits below the floor: there is no cap to write, the encoder runs at 500.
        assert_eq!(governor.observe(20.0, Some(20.0), 500, 60, 60, &FpsAdapt::default(), start).cap, None);
        // Raised to 6M: the governor must see the real 6M and halve it, not "current == floor".
        assert_eq!(governor.observe(20.0, Some(20.0), 6_000, 60, 60, &FpsAdapt::default(), start + Duration::from_secs(1)).cap, Some(3_000));
    }

    #[test]
    fn sustained_congestion_at_the_floor_keeps_moving_the_probe_clock() {
        let mut governor = Governor::default();
        let start = Instant::now();
        let mut at = start;
        while let Some(cap) = report(&mut governor, at, 20.0, Some(20.0)) {
            assert_eq!(Some(cap), governor.cap);
            at += DECREASE_AFTER;
        }
        // At the floor, ongoing congestion applies no step but must keep the clock fresh.
        assert_eq!(report(&mut governor, at, 20.0, Some(20.0)), None);
        // A brief recovery inside the probe window (from this last congested report) climbs
        // nothing: the aim is still the floor the path just broke at.
        at += PROBE_AFTER - Duration::from_secs(1);
        assert_eq!(report(&mut governor, at, 0.0, Some(20.0)), None);
        // Past the window, the climb restarts from the floor.
        at += Duration::from_secs(2);
        assert_eq!(report(&mut governor, at, 0.0, Some(20.0)), Some(1_300));
    }

    /// One ceiling whose halving reaches the floor in a single step, so the frame ladder is
    /// exercised without walking the whole bitrate ladder first.
    fn adapt() -> FpsAdapt { FpsAdapt { enabled: true } }

    #[test]
    fn congestion_at_the_bitrate_floor_walks_the_frame_ladder_down_to_five() {
        let mut governor = Governor::default();
        let mut at = Instant::now();
        assert_eq!(governor.observe(20.0, Some(20.0), 1_500, 60, 60, &adapt(), at).cap, Some(FLOOR_KBPS));
        // The cap step and the frame step never restart the encoder together: inside the gate
        // the ongoing congestion decides nothing.
        at += Duration::from_secs(1);
        assert_eq!(governor.observe(20.0, Some(20.0), 1_500, 60, 60, &adapt(), at), Decision::default());
        let mut fps = 60;
        for step in [45, 30, 24, 20, 15, 12, 10, 8, 5] {
            at += DECREASE_AFTER;
            assert_eq!(governor.observe(20.0, Some(20.0), 1_500, fps, 60, &adapt(), at).fps, Some(Some(step)), "ladder step {step}");
            fps = step;
        }
        // On the protocol floor there is no smaller frame rate to ask for.
        at += DECREASE_AFTER;
        assert_eq!(governor.observe(20.0, Some(20.0), 1_500, fps, 60, &adapt(), at).fps, None);
    }

    #[test]
    fn without_the_opt_in_a_floor_stays_a_floor() {
        let mut governor = Governor::default();
        let start = Instant::now();
        assert_eq!(governor.observe(20.0, Some(20.0), 1_500, 60, 60, &FpsAdapt::default(), start).cap, Some(FLOOR_KBPS));
        for secs in 3..30 {
            assert_eq!(governor.observe(20.0, Some(20.0), 1_500, 60, 60, &FpsAdapt::default(), start + Duration::from_secs(secs)), Decision::default());
        }
    }

    #[test]
    fn frames_climb_back_after_a_quiet_probe_window() {
        let mut governor = Governor::default();
        let start = Instant::now();
        assert_eq!(governor.observe(20.0, Some(20.0), 1_500, 60, 60, &adapt(), start).cap, Some(FLOOR_KBPS));
        assert_eq!(governor.observe(20.0, Some(20.0), 1_500, 60, 60, &adapt(), start + DECREASE_AFTER).fps, Some(Some(45)));
        // Clean reports lift the bitrate first, and no frame climbs inside the probe window
        // of the last congestion (a relay at 300 ms+ never lifts the cap to its ceiling).
        let mut at = start + DECREASE_AFTER + RECOVERY_AFTER_DECREASE;
        let mut cap = FLOOR_KBPS;
        while at < start + DECREASE_AFTER + PROBE_AFTER {
            let d = governor.observe(0.0, Some(20.0), 1_500, 45, 60, &adapt(), at);
            assert_eq!(d.fps, None, "frames must not climb before the probe window passed");
            if let Some(c) = d.cap { cap = c; }
            at += INCREASE_AFTER;
        }
        assert_eq!(cap, FLOOR_KBPS, "the broken path must not lift its cap above the aim yet");
        // Past the window the bitrate resumes its climb to the ceiling…
        assert_eq!(governor.observe(0.0, Some(20.0), 1_500, 45, 60, &adapt(), at).cap, Some(1_300));
        at += INCREASE_AFTER;
        assert_eq!(governor.observe(0.0, Some(20.0), 1_500, 45, 60, &adapt(), at).cap, Some(1_500));
        // …and once the cap has no step left, frames climb, capped by the viewer's nominal rate.
        at += INCREASE_AFTER;
        assert_eq!(governor.observe(0.0, Some(20.0), 1_500, 45, 60, &adapt(), at).fps, Some(Some(60)));
        // …and once the stream runs at the viewer's rate again the floor is lifted outright,
        // so a later viewer raise to 90 fps is not clamped by a stale governor step.
        let later = at + DECREASE_AFTER * 2;
        assert_eq!(governor.observe(0.0, Some(20.0), 1_500, 60, 60, &adapt(), later).fps, Some(None));
        assert_eq!(governor.fps, None);
    }

    #[test]
    fn frames_climb_back_on_a_marginal_path_that_never_reads_clean() {
        let mut governor = Governor::default();
        let start = Instant::now();
        assert_eq!(governor.observe(20.0, Some(20.0), 1_500, 60, 60, &adapt(), start).cap, Some(FLOOR_KBPS));
        assert_eq!(governor.observe(20.0, Some(20.0), 1_500, 60, 60, &adapt(), start + DECREASE_AFTER).fps, Some(Some(45)));
        // 3% loss over 200 ms never reads clean: it holds the bitrate exactly as before…
        let mut at = start + DECREASE_AFTER + RECOVERY_AFTER_DECREASE;
        while at < start + DECREASE_AFTER + PROBE_AFTER {
            assert_eq!(governor.observe(3.0, Some(200.0), 1_500, 45, 60, &adapt(), at), Decision::default(),
                "a marginal report must neither lift the cap nor climb frames early");
            at += INCREASE_AFTER;
        }
        // …but past the quiet window the path has demonstrably stopped collapsing, and the
        // frames that a real relay path recovers to matter more than the last cap step.
        assert_eq!(governor.observe(3.0, Some(200.0), 1_500, 45, 60, &adapt(), at).fps, Some(Some(60)));
        assert_eq!(governor.cap, Some(FLOOR_KBPS), "the marginal path keeps its bitrate floor");
    }
}
