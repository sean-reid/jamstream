//! Sender-side decision for piggybacking the previous frame's payload.
//! Turns on fast when the peer reports loss, turns off only after the link
//! has been demonstrably clean for a while, so a flapping link does not
//! toggle the doubled bandwidth every report.

const ON_LOSS_RATIO: f32 = 0.01;
const OFF_LOSS_RATIO: f32 = 0.002;

#[derive(Debug, Clone)]
pub struct RedundancyPolicy {
    active: bool,
    quiet_reports: u32,
    off_hold_reports: u32,
}

impl RedundancyPolicy {
    /// `off_hold_reports` is how many consecutive reports must stay under
    /// the off threshold before redundancy switches back off.
    pub fn new(off_hold_reports: u32) -> Self {
        Self {
            active: false,
            quiet_reports: 0,
            off_hold_reports: off_hold_reports.max(1),
        }
    }

    /// Feed one periodic report of the peer's observed loss ratio (0..=1).
    pub fn report(&mut self, loss_ratio: f32) {
        let loss = if loss_ratio.is_finite() {
            loss_ratio.max(0.0)
        } else {
            0.0
        };
        if loss > ON_LOSS_RATIO {
            self.active = true;
            self.quiet_reports = 0;
        } else if self.active {
            if loss < OFF_LOSS_RATIO {
                self.quiet_reports += 1;
                if self.quiet_reports >= self.off_hold_reports {
                    self.active = false;
                    self.quiet_reports = 0;
                }
            } else {
                self.quiet_reports = 0;
            }
        }
    }

    pub fn active(&self) -> bool {
        self.active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_inactive_and_turns_on_above_one_percent() {
        let mut policy = RedundancyPolicy::new(10);
        assert!(!policy.active());
        policy.report(0.009);
        assert!(!policy.active());
        policy.report(0.011);
        assert!(policy.active());
    }

    #[test]
    fn turns_off_only_after_sustained_quiet() {
        let mut policy = RedundancyPolicy::new(10);
        policy.report(0.05);
        assert!(policy.active());
        for _ in 0..9 {
            policy.report(0.001);
            assert!(policy.active());
        }
        policy.report(0.001);
        assert!(!policy.active());
    }

    #[test]
    fn middling_loss_holds_the_current_state() {
        let mut policy = RedundancyPolicy::new(5);
        // Between thresholds while inactive: stays off.
        for _ in 0..20 {
            policy.report(0.005);
        }
        assert!(!policy.active());
        policy.report(0.02);
        assert!(policy.active());
        // Between thresholds while active: stays on and resets the timer.
        for _ in 0..4 {
            policy.report(0.001);
        }
        policy.report(0.005);
        for _ in 0..4 {
            policy.report(0.001);
        }
        assert!(policy.active());
        policy.report(0.001);
        assert!(!policy.active());
    }

    #[test]
    fn garbage_reports_read_as_clean() {
        let mut policy = RedundancyPolicy::new(3);
        policy.report(f32::NAN);
        policy.report(f32::INFINITY);
        policy.report(-1.0);
        assert!(!policy.active());
        policy.report(0.5);
        assert!(policy.active());
        policy.report(f32::NAN);
        policy.report(f32::NAN);
        policy.report(f32::NAN);
        assert!(!policy.active());
    }
}
