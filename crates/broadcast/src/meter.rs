//! The meter law: where the LEDs are green, where they turn amber and red,
//! and how long the peak segment holds.
//!
//! Public, and in a module of its own, for the same reason [`crate::palette`]
//! is: the desktop client draws a meter for the same signal, and a viewer
//! watching the stream beside a musician looking at the app must not see two
//! different readings of one level. This is the copy to read from; nothing
//! here needs a second one (#232).
//!
//! The hold is in seconds because that is what a viewer perceives. It used to
//! be 45 frames in the renderer, which is 1.5 s only at 30 fps, while the app
//! held for 1.5 s outright. Frame rate is data in `data/platforms.json`, so
//! the two already disagreed at any rate other than the one the constant was
//! written for: at 60 fps the stream's peak segment dropped after 0.75 s while
//! the musician's own meter still showed it, and the same level looked like two
//! different performances.

/// Bottom of the scale. At or below this a channel reads as silent.
pub const FLOOR_DB: f32 = -60.0;
/// Amber from here up: approaching the ceiling.
pub const AMBER_FROM_DB: f32 = -12.0;
/// Red from here up: the last 3 dB before clipping.
pub const RED_FROM_DB: f32 = -3.0;

/// How long the peak segment stays fully lit.
pub const HOLD_SECS: f32 = 1.5;
/// How long it takes to fade out after the hold.
pub const HOLD_FADE_SECS: f32 = 0.5;

/// The zones run green, amber, red, and red stops short of clipping. Both the
/// renderer and the app index the scale on that order holding.
const _: () = {
    assert!(FLOOR_DB < AMBER_FROM_DB);
    assert!(AMBER_FROM_DB < RED_FROM_DB);
    assert!(RED_FROM_DB < 0.0);
};

/// A duration in frames at a given frame rate, rounded, never zero: a hold
/// the renderer cannot see is a hold that does not exist.
pub fn frames_for(secs: f32, fps: u32) -> u64 {
    let frames = (secs * fps as f32).round() as i64;
    frames.max(1) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frame counts the renderer used to spell out, and what they become
    /// at the rates a catalog edit could ask for. The hold is 1.5 s at every
    /// one of them, which is the whole point of deriving it.
    #[test]
    fn the_hold_is_the_same_time_at_any_frame_rate() {
        assert_eq!(frames_for(HOLD_SECS, 30), 45);
        assert_eq!(frames_for(HOLD_FADE_SECS, 30), 15);
        assert_eq!(frames_for(HOLD_SECS, 60), 90);
        assert_eq!(frames_for(HOLD_FADE_SECS, 60), 30);
        assert_eq!(frames_for(HOLD_SECS, 24), 36);
        for fps in [24u32, 25, 30, 50, 60] {
            let held_secs = frames_for(HOLD_SECS, fps) as f32 / fps as f32;
            assert!(
                (held_secs - HOLD_SECS).abs() < 0.03,
                "at {fps} fps the hold lasts {held_secs}s, not {HOLD_SECS}s"
            );
        }
    }

    /// A frame rate too low to represent the fade still gets one frame of it,
    /// rather than a division by zero in the renderer's alpha ramp.
    #[test]
    fn a_duration_never_rounds_away_to_nothing() {
        assert_eq!(frames_for(HOLD_FADE_SECS, 1), 1);
        assert_eq!(frames_for(0.0, 30), 1);
    }
}
