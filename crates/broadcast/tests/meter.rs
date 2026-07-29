//! The meter's peak-hold, through the public renderer.
//!
//! The hold is a duration a viewer perceives, so what matters is how long it
//! lasts in seconds, not how many frames it spans. It was written as 45 frames,
//! correct only at 30 fps, while frame rate is data in the platform catalog.

mod common;

use common::{H, W, musician};
use jamstream_broadcast::meter::{HOLD_FADE_SECS, HOLD_SECS, frames_for};
use jamstream_broadcast::{Renderer, SceneConfig};

/// Renders a loud frame, then a silent one `later` frames on, and returns the
/// pixels. Whether the held segment is still lit is the only thing that can
/// differ between two of these.
fn peak_then_silence(fps: u32, later: u64) -> Vec<u8> {
    let mut r = Renderer::new(SceneConfig {
        fps,
        ..SceneConfig::default()
    });
    let mut out = vec![0u8; (W * H * 4) as usize];
    let mut members = vec![musician("Ana Solari", None, 0.9, 0.9)];
    r.render(0, &members, 0, &mut out);
    members[0].level_peak = 0.0;
    members[0].level_rms = 0.0;
    r.render(later, &members, 0, &mut out);
    out
}

/// A silent card long after any hold could survive: the reference for "nothing
/// is being held". Geometry does not depend on frame rate, so one reference
/// serves every rate.
fn long_since(fps: u32) -> Vec<u8> {
    peak_then_silence(fps, 100_000)
}

#[test]
fn the_hold_outlives_the_same_frame_count_at_a_higher_frame_rate() {
    let whole_hold_at_30 = frames_for(HOLD_SECS, 30) + frames_for(HOLD_FADE_SECS, 30);
    assert_eq!(whole_hold_at_30, 60, "two seconds at 30 fps");

    // At 30 fps those 60 frames are the whole hold and its fade: the segment
    // is gone.
    assert_eq!(
        peak_then_silence(30, whole_hold_at_30),
        long_since(30),
        "at 30 fps the hold must be over after {whole_hold_at_30} frames"
    );

    // The same 60 frames at 60 fps is one second, well inside a 1.5 s hold, so
    // the segment is still lit and the frame differs. This is the assertion the
    // hardcoded 45 failed: it made the stream drop the segment at 0.75 s while
    // the musician's own meter still showed it.
    assert_ne!(
        peak_then_silence(60, 60),
        long_since(60),
        "at 60 fps a 1.5 s hold must still be lit one second in"
    );

    // And it is over on time at 60 fps too, two seconds in.
    let whole_hold_at_60 = frames_for(HOLD_SECS, 60) + frames_for(HOLD_FADE_SECS, 60);
    assert_eq!(whole_hold_at_60, 120, "two seconds at 60 fps");
    assert_eq!(peak_then_silence(60, whole_hold_at_60), long_since(60));
}

/// The stream's peak-hold and the app's are one number. The app spells it in
/// seconds, so this crate has to as well, or the two disagree the moment the
/// catalog's frame rate changes.
#[test]
fn the_hold_is_a_duration_not_a_frame_count() {
    for fps in [24u32, 30, 50, 60] {
        let held = frames_for(HOLD_SECS, fps) as f32 / fps as f32;
        assert!(
            (held - HOLD_SECS).abs() < 0.03,
            "at {fps} fps the stream holds a peak for {held}s, the app for {HOLD_SECS}s"
        );
    }
}
