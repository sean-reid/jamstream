//! Sliding-window replay protection for out-of-order transport packets,
//! same scheme WireGuard uses: accept the highest counter seen and a
//! 64-packet window behind it, each packet at most once.

const WINDOW: u64 = 64;

#[derive(Debug, Default)]
pub struct ReplayWindow {
    highest: u64,
    bitmap: u64,
    any: bool,
}

impl ReplayWindow {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns true exactly once per counter value within the window;
    /// duplicates, replays, and packets older than the window return false.
    pub fn accept(&mut self, counter: u64) -> bool {
        if !self.any {
            self.any = true;
            self.highest = counter;
            self.bitmap = 1;
            return true;
        }
        if counter > self.highest {
            let shift = counter - self.highest;
            self.bitmap = if shift >= WINDOW {
                0
            } else {
                self.bitmap << shift
            };
            self.bitmap |= 1;
            self.highest = counter;
            return true;
        }
        let offset = self.highest - counter;
        if offset >= WINDOW {
            return false;
        }
        let bit = 1u64 << offset;
        if self.bitmap & bit != 0 {
            return false;
        }
        self.bitmap |= bit;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_in_order() {
        let mut w = ReplayWindow::new();
        for c in 0..1000 {
            assert!(w.accept(c), "counter {c}");
        }
    }

    #[test]
    fn rejects_duplicates() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(5));
        assert!(!w.accept(5));
        assert!(w.accept(6));
        assert!(!w.accept(6));
        assert!(!w.accept(5));
    }

    #[test]
    fn accepts_reordered_within_window() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(100));
        assert!(w.accept(90));
        assert!(w.accept(99));
        assert!(w.accept(37));
        assert!(!w.accept(37));
    }

    #[test]
    fn rejects_older_than_window() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(100));
        assert!(!w.accept(36));
        assert!(w.accept(37));
    }

    #[test]
    fn counter_zero_is_usable_once() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(0));
        assert!(!w.accept(0));
        assert!(w.accept(1));
    }

    #[test]
    fn big_jump_clears_window() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(10));
        assert!(w.accept(10_000));
        assert!(!w.accept(10));
        assert!(w.accept(9_999));
    }
}
