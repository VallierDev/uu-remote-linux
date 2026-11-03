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

#[cfg(test)]
mod tests {
    use super::*;
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
