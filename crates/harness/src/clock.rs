//! Virtual time for deterministic simulation.
//!
//! `VirtualClock` is the single master clock a simulation advances by hand.
//! Endpoints that should experience clock drift read that same master clock
//! through a `SkewedClock`, which remaps master time multiplicatively; drift
//! is a read-side view, never a second time source.

/// Simulation master clock. Stores integer microseconds so repeated advances
/// accumulate exactly; an f64 millisecond store would creep for advance
/// amounts that are not representable in binary (e.g. 1 us = 0.001 ms).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VirtualClock {
    now_us: u64,
}

impl VirtualClock {
    pub fn new() -> Self {
        Self { now_us: 0 }
    }

    /// Current time in whole milliseconds, floored.
    pub fn now_ms(&self) -> u64 {
        self.now_us / 1_000
    }

    pub fn now_us(&self) -> u64 {
        self.now_us
    }

    pub fn advance_us(&mut self, us: u64) {
        self.now_us += us;
    }

    /// Runs `count` ticks of `tick_us` each, yielding each tick's start time.
    /// After the iterator is exhausted the clock sits `count * tick_us` past
    /// where it started.
    pub fn ticks(&mut self, tick_us: u64, count: usize) -> impl Iterator<Item = u64> {
        (0..count).map(move |_| {
            let start = self.now_us;
            self.now_us += tick_us;
            start
        })
    }
}

/// A drifted per-endpoint view of the master clock. Positive ppm runs fast:
/// a +200 ppm endpoint reads 200 extra microseconds per master second. All
/// skewed endpoints read the one master `VirtualClock` through their own map,
/// so scenarios compose clock drift with any network profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkewedClock {
    skew_ppm: i32,
}

impl SkewedClock {
    pub fn new(skew_ppm: i32) -> Self {
        Self { skew_ppm }
    }

    pub fn skew_ppm(&self) -> i32 {
        self.skew_ppm
    }

    /// Maps master microseconds to this endpoint's microseconds:
    /// `t * (1 + ppm / 1e6)`, in exact integer arithmetic.
    pub fn map(&self, master_us: u64) -> u64 {
        let adj = master_us as i128 * i128::from(self.skew_ppm) / 1_000_000;
        (master_us as i128 + adj) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_ms_floors() {
        let mut clock = VirtualClock::new();
        clock.advance_us(1_999);
        assert_eq!(clock.now_ms(), 1);
        assert_eq!(clock.now_us(), 1_999);
        clock.advance_us(1);
        assert_eq!(clock.now_ms(), 2);
    }

    #[test]
    fn ticks_yield_start_times_and_advance() {
        let mut clock = VirtualClock::new();
        clock.advance_us(100);
        let starts: Vec<u64> = clock.ticks(2_500, 3).collect();
        assert_eq!(starts, vec![100, 2_600, 5_100]);
        assert_eq!(clock.now_us(), 7_600);
    }

    #[test]
    fn zero_skew_is_identity() {
        let skew = SkewedClock::new(0);
        for t in [0, 1, 999_999, 3_600_000_000] {
            assert_eq!(skew.map(t), t);
        }
    }
}
