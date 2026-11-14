//! Send-side bitrate response to RTCP loss and RTT.
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
    cap: Option<u32>,
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
    /// One RTCP receiver report: `loss_percent` is its fraction lost in percent. Returns the new
    /// encoder cap when the encoder must restart.
    pub fn observe(&mut self, loss_percent: f32, rtt_ms: Option<f64>, ceiling_kbps: u32, now: Instant) -> Option<u32> {
        // The caller supplies the viewer's ceiling; 8 Mbit/s is only its auto/unlimited fallback.
        let ceiling = ceiling_kbps.max(FLOOR_KBPS);
        let aim = self.broken_at
            .filter(|(_, at)| now.duration_since(*at) < PROBE_AFTER)
            .map_or(ceiling, |(broken, _)| (broken * 9 / 10).clamp(FLOOR_KBPS, ceiling));
        let current = self.cap.unwrap_or(aim).min(aim);
        // Loss at that level only shows up when a path genuinely breaks, so one report is enough. A
        // round trip spike looks the same for a single report, so it has to hold across reports.
        let congested = loss_percent >= CONGESTION_LOSS_PERCENT
            || self.note_rtt(rtt_ms) >= RTT_CONFIRMATIONS;
        let clean = loss_percent < CLEAN_LOSS_PERCENT
            && rtt_ms.is_none_or(|rtt| rtt < CLEAN_RTT_MS)
            && self.settled(now);
        let mut broken = None;
        let (target, gate) = if congested {
            broken = Some(current);
            self.rtt_over = 0;
            ((current / 2).max(FLOOR_KBPS), DECREASE_AFTER)
        } else if clean {
            ((current + (current / 4).max(MIN_STEP_KBPS)).min(aim), INCREASE_AFTER)
        } else {
            // Between the knee and the cliff: hold, so a marginal path does not flap.
            self.cap = Some(current);
            return None;
        };
        if target == current || self.applied.is_some_and(|at| now.duration_since(at) < gate) {
            self.cap = Some(current);
            return None;
        }
        if let Some(broken) = broken {
            self.broken_at = Some((broken, now));
            self.decreased_at = Some(now);
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
        governor.observe(loss, rtt, 6_000, at)
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
        assert_eq!(governor.cap, Some(6_000));
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
        assert_eq!(governor.cap, Some(6_000));
    }

    #[test]
    fn an_auto_quality_session_halves_from_the_auto_ceiling() {
        let mut governor = Governor::default();
        let start = Instant::now();
        assert_eq!(governor.observe(20.0, Some(20.0), AUTO_CEILING_KBPS, start), Some(4_000));
    }

    #[test]
    fn an_explicit_high_rate_recovers_to_the_viewers_choice() {
        let mut governor = Governor::default();
        let start = Instant::now();
        assert_eq!(governor.observe(20.0, Some(900.0), 20_000, start), Some(10_000));
        let mut at = start + PROBE_AFTER;
        let mut reached = None;
        while let Some(cap) = governor.observe(0.0, Some(20.0), 20_000, at) {
            reached = Some(cap);
            at += INCREASE_AFTER;
        }
        assert_eq!(reached, Some(20_000));
    }
}
