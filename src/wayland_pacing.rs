use std::time::{Duration, Instant};

/// Damage-driven capture still needs a first picture and periodic idle packets.
pub(crate) struct FramePacing {
    dynamic: bool,
    last_sent: Option<Instant>,
}
impl FramePacing {
    pub(crate) fn new(dynamic: bool) -> Self { Self { dynamic, last_sent: None } }
    pub(crate) fn wait_for_damage(&self) -> bool { self.dynamic && self.last_sent.is_some() }
    pub(crate) fn has_frame(&self) -> bool { self.last_sent.is_some() }
    pub(crate) fn keepalive_due(&self, now: Instant, idr: bool) -> bool {
        self.last_sent.is_some_and(|sent| idr || now.saturating_duration_since(sent) >= Duration::from_secs(1))
    }
    pub(crate) fn sent(&mut self, now: Instant) { self.last_sent = Some(now); }
}

/// Protocol readiness retries do not consume a frame-rate slot. Idle damage
/// backs off to 10 ms; new captures get short retries for protocol round trips.
pub(crate) struct CaptureSchedule {
    interval: Duration,
    frame_deadline: Instant,
    attempt: Instant,
    retries: u32,
}
impl CaptureSchedule {
    pub(crate) fn new(now: Instant, fps: u32) -> Self {
        Self { interval: Duration::from_nanos(1_000_000_000 / u64::from(fps.max(1))),
            frame_deadline: now, attempt: now, retries: 0 }
    }
    pub(crate) fn deadline(&self) -> Instant { self.attempt }
    pub(crate) fn completed(&mut self, now: Instant, encoded: bool) {
        if encoded {
            let next = self.frame_deadline + self.interval;
            self.frame_deadline = if next <= now { now + self.interval } else { next };
            self.attempt = self.frame_deadline;
            self.retries = 0;
        } else {
            self.retries = self.retries.saturating_add(1);
            self.attempt = now + Duration::from_millis(if self.retries <= 8 { 1 } else { 10 });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn asynchronous_capture_does_not_halve_the_target_fps() {
        for fps in [30, 60] {
            for latency in [1, 4] {
                let start = Instant::now();
                let mut schedule = CaptureSchedule::new(start, fps);
                let mut pending = None;
                let mut encoded = Vec::new();
                while schedule.deadline() < start + Duration::from_secs(1) {
                    let now = schedule.deadline();
                    let ready = *pending.get_or_insert(now + Duration::from_millis(latency));
                    let sent = now >= ready;
                    if sent { encoded.push(now); pending = None; }
                    schedule.completed(now, sent);
                }
                assert_eq!(encoded.len(), fps as usize, "async capture latency={latency}ms");
                assert!(encoded.windows(2).all(|w| w[1] > w[0]));
            }
        }
    }
    #[test]
    fn idle_retries_back_off_and_readiness_restores_the_frame_schedule() {
        let start = Instant::now();
        let mut schedule = CaptureSchedule::new(start, 60);
        for _ in 0..20 {
            let now = schedule.deadline();
            schedule.completed(now, false);
            assert!(schedule.deadline() > now);
        }
        let now = schedule.deadline();
        schedule.completed(now, false);
        assert_eq!(schedule.deadline() - now, Duration::from_millis(10));
        schedule.completed(schedule.deadline(), true);
        let now = schedule.deadline();
        schedule.completed(now, false);
        assert_eq!(schedule.deadline() - now, Duration::from_millis(1));
    }
    #[test]
    fn idle_damage_idle_and_keyframe_requests_share_the_last_picture() {
        let start = Instant::now();
        let mut pacing = FramePacing::new(true);
        assert!(!pacing.wait_for_damage());
        assert!(!pacing.keepalive_due(start + Duration::from_secs(10), true));
        pacing.sent(start);
        assert!(pacing.wait_for_damage());
        assert!(!pacing.keepalive_due(start + Duration::from_millis(999), false));
        assert!(pacing.keepalive_due(start + Duration::from_secs(1), false));
        pacing.sent(start + Duration::from_secs(1));
        assert!(!pacing.keepalive_due(start + Duration::from_millis(1100), false));
        assert!(pacing.keepalive_due(start + Duration::from_millis(1100), true));
        pacing.sent(start + Duration::from_millis(1100));
        // A changed frame refreshes the same idle deadline.
        pacing.sent(start + Duration::from_millis(1133));
        assert!(!pacing.keepalive_due(start + Duration::from_secs(2), false));
        assert!(pacing.keepalive_due(start + Duration::from_millis(2133), false));
        let restarted = FramePacing::new(true);
        assert!(!restarted.wait_for_damage());
        assert!(!restarted.has_frame());
    }
    #[test]
    fn fixed_pacing_never_requests_damage_only_capture() {
        let mut pacing = FramePacing::new(false);
        pacing.sent(Instant::now());
        assert!(!pacing.wait_for_damage());
    }
}
