//! Video cadence, derived from the audio clock.
//!
//! The session mixes in 2.5 ms ticks: 120 samples per channel at 48 kHz.
//! Video wants 30 fps. The two do not divide:
//!
//! ```text
//!   samples per video frame = 48000 / 30       = 1600
//!   ticks per video frame   = 1600 / 120       = 13 + 1/3
//! ```
//!
//! So there is no fixed tick count per frame at 30 fps, and any
//! implementation that picks one drifts. 16 ticks per frame, for instance, is
//! 40 ms, which is exactly 25 fps, not 30; a 16-tick cadence labelled 30 fps
//! would run the video clock 20% slow against the audio and the drift would
//! grow without bound. [`VideoCadence`] therefore never counts ticks. It
//! counts samples:
//!
//! ```text
//!   frames_due(samples) = samples / 1600 + 1
//! ```
//!
//! The `+ 1` is the startup frame: the first tick emits frame 0 so the video
//! stream begins at presentation time zero alongside the first audio sample.
//! After that, frame `k` is emitted on the first tick whose cumulative sample
//! count reaches `k * 1600`, which produces a repeating 13, 13, 14 tick
//! pattern (40 ticks = 100 ms = 3 frames, exactly). Because the rule is a
//! pure function of the cumulative sample count, error never accumulates:
//! after an hour of ticks the frame count is exactly `30 * 3600 + 1`.
//!
//! Frame `k`'s presentation time in the audio domain is exactly
//! `k * samples_per_frame` samples ([`VideoCadence::pts_samples`]). We hand
//! ffmpeg constant-frame-rate rawvideo, so ffmpeg assigns the PTS itself as
//! `k / fps`; that equals our sample-domain PTS by construction. Keeping the
//! *count* of frames locked to the audio clock is therefore the whole job,
//! and the residual A/V offset is bounded by one frame period (33.3 ms), not
//! by elapsed time.

use crate::SAMPLE_RATE;

/// Audio-mastered frame scheduler. Feed it samples, it tells you which video
/// frames to render.
#[derive(Debug, Clone, Copy)]
pub struct VideoCadence {
    fps: u32,
    samples_per_frame: u64,
    samples: u64,
    frames: u64,
}

impl VideoCadence {
    /// `fps` must divide the sample rate evenly (25 and 30 both do), so the
    /// sample-domain frame period is exact and the clock cannot drift.
    ///
    /// # Panics
    /// If `fps` is zero or does not divide [`SAMPLE_RATE`].
    pub fn new(fps: u32) -> Self {
        assert!(fps > 0, "fps must be positive");
        assert!(
            SAMPLE_RATE % fps == 0,
            "fps must divide {SAMPLE_RATE} exactly, got {fps}"
        );
        VideoCadence {
            fps,
            samples_per_frame: u64::from(SAMPLE_RATE / fps),
            samples: 0,
            frames: 0,
        }
    }

    pub fn fps(&self) -> u32 {
        self.fps
    }

    pub fn samples_per_frame(&self) -> u64 {
        self.samples_per_frame
    }

    /// Keyframes land exactly every `secs` seconds: the GOP length in
    /// frames. 2 s at 30 fps is 60.
    pub fn keyframe_interval(&self, secs: u32) -> u32 {
        self.fps * secs
    }

    /// Consumes one tick's worth of audio (per-channel sample count) and
    /// returns the frame indices now due, in order. Normally empty or a
    /// single frame; a caller that skipped ticks gets the whole backlog so
    /// the video clock catches up with the audio clock rather than sliding.
    pub fn advance(&mut self, samples: u64) -> FrameRun {
        self.samples += samples;
        let due = self.samples / self.samples_per_frame + 1;
        let first = self.frames;
        self.frames = self.frames.max(due);
        FrameRun {
            next: first,
            end: self.frames,
        }
    }

    /// Presentation time of frame `index` in audio samples: exact, integral,
    /// and the reason there is no drift to correct.
    pub fn pts_samples(&self, index: u64) -> u64 {
        index * self.samples_per_frame
    }

    /// Frames emitted so far.
    pub fn frames_emitted(&self) -> u64 {
        self.frames
    }

    /// Audio samples consumed so far.
    pub fn samples_consumed(&self) -> u64 {
        self.samples
    }
}

/// The frame indices due from one [`VideoCadence::advance`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRun {
    next: u64,
    end: u64,
}

impl Iterator for FrameRun {
    type Item = u64;

    fn next(&mut self) -> Option<u64> {
        if self.next >= self.end {
            return None;
        }
        let i = self.next;
        self.next += 1;
        Some(i)
    }
}

impl FrameRun {
    pub fn is_empty(&self) -> bool {
        self.next >= self.end
    }

    pub fn len(&self) -> usize {
        (self.end - self.next) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One session tick: 120 samples per channel.
    const TICK: u64 = 120;

    /// The ticks on which each frame is emitted, for `ticks` ticks.
    fn emit_ticks(fps: u32, ticks: u64) -> Vec<u64> {
        let mut c = VideoCadence::new(fps);
        let mut at = Vec::new();
        for tick in 1..=ticks {
            for _ in c.advance(TICK) {
                at.push(tick);
            }
        }
        at
    }

    #[test]
    fn thirty_fps_settles_into_thirteen_thirteen_fourteen_ticks() {
        assert_eq!(VideoCadence::new(30).samples_per_frame(), 1_600);
        let at = emit_ticks(30, 200);
        // Frame 0 on the first tick (PTS 0), then the 13, 13, 14 cycle. The
        // startup frame absorbs one tick of the first cycle, so the run of
        // gaps starts 13, 13, 13 before settling.
        assert_eq!(&at[..8], &[1, 14, 27, 40, 54, 67, 80, 94]);
        let gaps: Vec<u64> = at.windows(2).map(|w| w[1] - w[0]).collect();
        assert_eq!(&gaps[..7], &[13, 13, 13, 14, 13, 13, 14]);
        // Three frames per 40 ticks (100 ms) forever, exactly.
        for k in 1..at.len() - 3 {
            assert_eq!(at[k + 3] - at[k], 40, "cycle broke at frame {k}");
        }
    }

    #[test]
    fn twenty_five_fps_is_exactly_sixteen_ticks() {
        // The reconciliation of "one frame every 16 ticks" with "30 fps":
        // 16 ticks is 40 ms, which is 25 fps. Both cadences are exact; only
        // 25 fps has an integral tick period, which is why 30 fps counts
        // samples instead of ticks.
        assert_eq!(VideoCadence::new(25).samples_per_frame(), 1_920);
        let at = emit_ticks(25, 100);
        assert_eq!(&at[..5], &[1, 16, 32, 48, 64]);
        for w in at.windows(2).skip(1) {
            assert_eq!(w[1] - w[0], 16);
        }
    }

    #[test]
    fn no_drift_over_an_hour_of_ticks() {
        let mut c = VideoCadence::new(30);
        let ticks = 3_600 * 400; // one hour of 2.5 ms ticks
        let mut frames = 0u64;
        for _ in 0..ticks {
            frames += c.advance(TICK).len() as u64;
        }
        assert_eq!(c.samples_consumed(), 3_600 * u64::from(SAMPLE_RATE));
        // Exactly one hour of 30 fps video, plus the startup frame at PTS 0.
        assert_eq!(frames, 30 * 3_600 + 1);
        assert_eq!(c.frames_emitted(), frames);
        // The last frame's PTS is still inside the hour, and the gap between
        // the audio and video clocks is under one frame period.
        let last_pts = c.pts_samples(frames - 1);
        assert!(last_pts <= c.samples_consumed());
        assert!(c.samples_consumed() - last_pts < c.samples_per_frame());
    }

    #[test]
    fn pts_is_derived_from_samples_not_wall_time() {
        let mut c = VideoCadence::new(30);
        // Emission of frame k always happens on the tick that reaches its
        // sample-domain PTS; never earlier, never a full period later.
        for _ in 0..4_000 {
            for k in c.advance(TICK) {
                let pts = c.pts_samples(k);
                assert!(pts <= c.samples_consumed(), "frame {k} emitted early");
                assert!(
                    c.samples_consumed() - pts < c.samples_per_frame() + TICK,
                    "frame {k} emitted late"
                );
            }
        }
    }

    #[test]
    fn a_skipped_run_of_ticks_catches_up_instead_of_sliding() {
        let mut c = VideoCadence::new(30);
        c.advance(TICK);
        // Half a second of audio arrives in one go: 15 frames come due.
        let run = c.advance(SAMPLE_RATE as u64 / 2);
        assert_eq!(run.len(), 15);
        assert_eq!(c.frames_emitted(), 16);
    }

    #[test]
    fn keyframe_interval_is_two_seconds_of_frames() {
        assert_eq!(VideoCadence::new(30).keyframe_interval(2), 60);
        assert_eq!(VideoCadence::new(25).keyframe_interval(2), 50);
    }

    #[test]
    #[should_panic(expected = "must divide")]
    fn refuses_a_frame_rate_that_would_drift() {
        VideoCadence::new(29);
    }
}
