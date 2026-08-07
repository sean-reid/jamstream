//! Fixed-ratio sample-rate conversion at the device boundary, rung 3 of the
//! ladder.
//!
//! A device that cannot clock at the session rate opens at its own rate and
//! each direction's handler half is wrapped here: capture converts
//! device-rate callbacks to session-rate chunks before the bridge sees them,
//! playback pulls session-rate chunks from the bridge and converts them to
//! the rate the device wants. Everything above the device layer keeps seeing
//! 48 kHz.
//!
//! The ratio is a constant (session rate over device rate, not 44.1-specific)
//! and is never steered: a device crystal off by d ppm comes out the far side
//! off by exactly d ppm, which is the signal the session's drift compensators
//! already consume. Steering this converter too would put two controllers on
//! one backlog and have them fight.
//!
//! Latency, measured from rubato's own reported delay at 44.1 kHz: the sinc
//! filter's group delay is 34 output frames on capture (0.71 ms at 48 kHz)
//! and 29 on playback (0.66 ms at 44.1 kHz), and the fixed 120-frame chunk
//! stages up to one 2.5 ms tick more, so a converted direction adds ~3.2 ms.
//! Constructors report the exact figure so the disclosure surface can never
//! drift from the implementation.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};

/// Largest per-callback chunk converted in one pass. Bigger device callbacks
/// are processed in slices of this many frames, so the conversion scratch
/// buffers stay fixed after stream construction.
pub(crate) const MAX_CHUNK_FRAMES: usize = 4096;

/// Frames per conversion chunk on the session-rate side: one 2.5 ms tick at
/// 48 kHz, the granularity everything downstream of the bridge moves in.
const CHUNK: usize = 120;

pub type CaptureFn = Box<dyn FnMut(&[f32]) + Send>;
pub type PlaybackFn = Box<dyn FnMut(&mut [f32]) + Send>;

/// A device-rate callback size in session-rate frames, rounded up: what a
/// converting stream can hand the handler per callback, and therefore what
/// [`crate::StreamHandle::buffer_frames`] reports so everything sized around
/// callbacks keeps one unit.
pub(crate) fn session_frames(device_frames: u32, session_rate: u32, device_rate: u32) -> u32 {
    (u64::from(device_frames) * u64::from(session_rate)).div_ceil(u64::from(device_rate)) as u32
}

/// Sinc quality for the boundary: 64 taps with the automatic cutoff keeps
/// aliasing below the Blackman2 sidelobe level at tens of microseconds per
/// callback, and linear interpolation over 128x oversampling is transparent
/// at these ratios.
fn sinc_params() -> SincInterpolationParameters {
    SincInterpolationParameters {
        sinc_len: 64,
        f_cutoff: None,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 128,
        window: WindowFunction::Blackman2,
    }
}

/// What one converted direction adds to mouth-to-ear: the filter's group
/// delay (reported by rubato in output frames) plus up to one chunk staged
/// while a whole one accumulates.
fn added_ms(delay_out_frames: usize, out_rate: u32, session_rate: u32) -> f32 {
    (delay_out_frames as f32 / out_rate as f32 + CHUNK as f32 / session_rate as f32) * 1000.0
}

fn fixed_ratio_resampler(ratio: f64, channels: usize, fixed: FixedAsync) -> Async<f32> {
    // max_resample_ratio_relative 1.0: the ratio cannot be adjusted, by
    // construction rather than by discipline.
    Async::new_sinc(ratio, 1.0, &sinc_params(), CHUNK, channels, fixed)
        .expect("fixed-ratio resampler construction")
}

/// Wraps a capture half so `inner` keeps receiving session-rate audio from a
/// device clocked at `device_rate`. Returns the wrapper and the latency it
/// adds in milliseconds, for the rate disclosure.
///
/// Device callbacks of any size are accepted; `inner` is called with fixed
/// `CHUNK`-frame session-rate chunks as whole ones become available. The
/// wrapper never allocates after construction.
pub fn converting_capture(
    mut inner: CaptureFn,
    session_rate: u32,
    device_rate: u32,
    channels: u16,
) -> (CaptureFn, f32) {
    assert!(device_rate > 0, "device rate must be nonzero");
    let ch = usize::from(channels);
    let ratio = f64::from(session_rate) / f64::from(device_rate);
    let mut resampler = fixed_ratio_resampler(ratio, ch, FixedAsync::Output);
    let added = added_ms(resampler.output_delay(), session_rate, session_rate);
    // Sliced appends stay under MAX_CHUNK_FRAMES and the drain below leaves
    // less than one input chunk behind, so this capacity is never exceeded
    // and extend_from_slice never reallocates.
    let mut backlog: Vec<f32> =
        Vec::with_capacity((MAX_CHUNK_FRAMES + resampler.input_frames_max()) * ch);
    let mut chunk = vec![0.0f32; CHUNK * ch];
    let capture = Box::new(move |samples: &[f32]| {
        debug_assert_eq!(samples.len() % ch, 0);
        for piece in samples.chunks(MAX_CHUNK_FRAMES * ch) {
            backlog.extend_from_slice(piece);
            loop {
                let need_frames = resampler.input_frames_next();
                let need = need_frames * ch;
                if backlog.len() < need {
                    break;
                }
                let input = InterleavedSlice::new(&backlog[..need], ch, need_frames)
                    .expect("input adapter");
                let mut output =
                    InterleavedSlice::new_mut(&mut chunk, ch, CHUNK).expect("output adapter");
                resampler
                    .process_into_buffer(&input, &mut output, None)
                    .expect("capture resample");
                backlog.copy_within(need.., 0);
                let len = backlog.len() - need;
                backlog.truncate(len);
                inner(&chunk);
            }
        }
    });
    (capture, added)
}

/// Wraps a playback half so a device clocked at `device_rate` keeps being
/// fed from `inner`, which still produces session-rate audio. Returns the
/// wrapper and the latency it adds in milliseconds, for the rate disclosure.
///
/// Device requests of any size are served; `inner` is pulled in fixed
/// `CHUNK`-frame session-rate chunks until enough device-rate audio is
/// staged. The wrapper never allocates after construction.
pub fn converting_playback(
    mut inner: PlaybackFn,
    session_rate: u32,
    device_rate: u32,
    channels: u16,
) -> (PlaybackFn, f32) {
    assert!(device_rate > 0, "device rate must be nonzero");
    let ch = usize::from(channels);
    let ratio = f64::from(device_rate) / f64::from(session_rate);
    let mut resampler = fixed_ratio_resampler(ratio, ch, FixedAsync::Input);
    let added = added_ms(resampler.output_delay(), device_rate, session_rate);
    // Sliced requests stay under MAX_CHUNK_FRAMES and each fill appends at
    // most one output chunk past the request, so resize never reallocates.
    let mut staging: Vec<f32> =
        Vec::with_capacity((MAX_CHUNK_FRAMES + resampler.output_frames_max()) * ch);
    let mut chunk = vec![0.0f32; CHUNK * ch];
    let playback = Box::new(move |out: &mut [f32]| {
        debug_assert_eq!(out.len() % ch, 0);
        for piece in out.chunks_mut(MAX_CHUNK_FRAMES * ch) {
            while staging.len() < piece.len() {
                // The handler contract says the buffer arrives zeroed and
                // untouched means silence; this chunk is reused, so honor
                // that here.
                chunk.fill(0.0);
                inner(&mut chunk);
                let want = resampler.output_frames_next();
                let start = staging.len();
                staging.resize(start + want * ch, 0.0);
                let input = InterleavedSlice::new(&chunk, ch, CHUNK).expect("input adapter");
                let mut output = InterleavedSlice::new_mut(&mut staging[start..], ch, want)
                    .expect("output adapter");
                let (_, produced) = resampler
                    .process_into_buffer(&input, &mut output, None)
                    .expect("playback resample");
                staging.truncate(start + produced * ch);
            }
            piece.copy_from_slice(&staging[..piece.len()]);
            staging.copy_within(piece.len().., 0);
            let len = staging.len() - piece.len();
            staging.truncate(len);
        }
    });
    (playback, added)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    const DEVICE: u32 = 44_100;
    const SESSION: u32 = 48_000;
    /// Device rates the ladder really meets, on both sides of unity. Above
    /// the session rate the capture ratio drops below one and the playback
    /// ratio rises above it, which inverts every buffering argument in this
    /// module, and 96 kHz is what most pro interfaces ship set to. The
    /// telephony pair is a playback endpoint: [`crate::cpal_backend`] refuses
    /// a hands-free microphone, but nothing refuses a hands-free earpiece, so
    /// that direction runs at ratio 6.
    const DEVICE_RATES: [u32; 7] = [8_000, 16_000, 44_100, 88_200, 96_000, 176_400, 192_000];
    /// Callback sizes a mix of hosts really deliver: single frames, odd
    /// remainders, one 44.1 period (441), one 48 k period (480), and cpal's
    /// slicing bound.
    const SIZES: [usize; 7] = [1, 7, 110, 111, 441, 480, 4096];
    /// Where a step of a converted ramp stops being the filter and starts
    /// being a dropped sample, in input samples.
    const SEAM: f64 = 0.5;

    /// Counts heap operations on this thread so the RT-path test can assert
    /// the wrappers are allocation-free after construction. Counting is
    /// thread-local, so parallel tests in this binary do not interfere.
    struct CountingAlloc;

    thread_local! {
        static HEAP_OPS: Cell<u64> = const { Cell::new(0) };
    }

    fn heap_ops() -> u64 {
        HEAP_OPS.with(Cell::get)
    }

    unsafe impl GlobalAlloc for CountingAlloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            HEAP_OPS.with(|c| c.set(c.get() + 1));
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            HEAP_OPS.with(|c| c.set(c.get() + 1));
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            HEAP_OPS.with(|c| c.set(c.get() + 1));
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    #[global_allocator]
    static ALLOC: CountingAlloc = CountingAlloc;

    /// A mono capture wrapper whose inner half collects everything it is
    /// handed, the way the bridge push would.
    fn capture_rig(device: u32) -> (CaptureFn, Arc<Mutex<Vec<f32>>>, f32) {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let inner_sink = Arc::clone(&sink);
        let inner: CaptureFn = Box::new(move |samples: &[f32]| {
            inner_sink.lock().unwrap().extend_from_slice(samples);
        });
        let (capture, added) = converting_capture(inner, SESSION, device, 1);
        (capture, sink, added)
    }

    /// Feeds `input` through a capture wrapper in callbacks cycling over
    /// `sizes` and returns everything the inner half saw.
    fn convert_capture(device: u32, input: &[f32], sizes: &[usize]) -> Vec<f32> {
        let (mut capture, sink, _) = capture_rig(device);
        let mut pos = 0;
        for size in sizes.iter().cycle() {
            let end = (pos + size).min(input.len());
            capture(&input[pos..end]);
            pos = end;
            if pos == input.len() {
                break;
            }
        }
        drop(capture);
        Arc::try_unwrap(sink)
            .expect("the wrapper was the only other holder")
            .into_inner()
            .unwrap()
    }

    fn sine(hz: f64, rate: u32, idx: usize) -> f32 {
        (hz * std::f64::consts::TAU * idx as f64 / f64::from(rate)).sin() as f32 * 0.5
    }

    fn rms(samples: &[f32]) -> f64 {
        (samples
            .iter()
            .map(|&s| f64::from(s) * f64::from(s))
            .sum::<f64>()
            / samples.len() as f64)
            .sqrt()
    }

    /// Positive-going zero crossings, i.e. whole cycles of a sine.
    fn cycles(samples: &[f32]) -> usize {
        samples
            .windows(2)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count()
    }

    /// Samples of a converted ramp past the filter warmup, where the sinc
    /// still sees zero history.
    const SKIP: usize = 480;

    /// The probe the buffering tests run on: sample values are their own
    /// index, so any seam is a step in a straight line.
    fn ramp() -> Vec<f32> {
        (0..30_000).map(|i| i as f32).collect()
    }

    /// The widest a converted ramp departs from the straight line of gradient
    /// `slope` through its first steady sample, and where.
    ///
    /// The ramp carries input-sample indices, so a dropped or doubled sample
    /// moves every later value by exactly 1.0 whatever the ratio, which makes
    /// this one number the seam detector at every rate.
    fn worst_deviation(out: &[f32], slope: f64) -> (usize, f64) {
        let delay = SKIP as f64 * slope - f64::from(out[SKIP]);
        out.iter()
            .enumerate()
            .skip(SKIP)
            .map(|(k, &s)| (k, (f64::from(s) - (k as f64 * slope - delay)).abs()))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .expect("a converted run reaches past the warmup")
    }

    /// Holds a converted ramp to one unbroken line of gradient `slope` and
    /// returns the group delay it measured, in samples of the line's own
    /// clock.
    ///
    /// The bound is half the sample a seam would move the line by;
    /// `one_dropped_sample_is_what_a_seam_looks_like` measures both sides of
    /// it at every rate. What sets the floor is arithmetic, not the ratio: 64
    /// f32 taps summed over values this size drift by a fraction of a sample
    /// over a long run, so the ramp is kept short enough that the drift stays
    /// well inside the bound at the steepest ratio in the table.
    fn assert_line(device: u32, out: &[f32], slope: f64) -> f64 {
        let (at, worst) = worst_deviation(out, slope);
        assert!(
            worst < SEAM,
            "{device} Hz: sample {at} is off the line by {worst:.3}: \
             the ramp has a seam"
        );
        SKIP as f64 * slope - f64::from(out[SKIP])
    }

    /// The seam detector's own teeth, at every ratio: one input sample
    /// dropped from the ramp has to land on the far side of [`SEAM`] from a
    /// clean run. A tolerance loose enough to pass a real seam would be worse
    /// than no test at all, and nothing else in the module would say so.
    #[test]
    fn one_dropped_sample_is_what_a_seam_looks_like() {
        let whole = ramp();
        let mut seamed = whole.clone();
        seamed.remove(whole.len() / 2);
        for device in DEVICE_RATES {
            let slope = f64::from(device) / f64::from(SESSION);
            let (_, clean) = worst_deviation(&convert_capture(device, &whole, &SIZES), slope);
            let (_, seam) = worst_deviation(&convert_capture(device, &seamed, &SIZES), slope);
            assert!(clean < SEAM, "{device} Hz: a clean run measures {clean:.3}");
            assert!(
                seam > SEAM,
                "{device} Hz: a dropped sample measures {seam:.3}, against \
                 {clean:.3} clean: the detector cannot tell a seam from the filter"
            );
        }
    }

    /// Count conservation at every ratio the ladder meets: over five seconds
    /// the output count tracks input * session / device to within the
    /// buffering slack, which is what "no dropped or padded runs at steady
    /// state" means in numbers. Any systematic ratio error diverges linearly
    /// and lands far outside the window.
    #[test]
    fn the_output_count_holds_the_ratio_at_every_device_rate() {
        for device in DEVICE_RATES {
            let input = vec![0.25f32; 5 * device as usize];
            let produced = convert_capture(device, &input, &[device as usize / 100]).len();
            let expected =
                (input.len() as f64 * f64::from(SESSION) / f64::from(device)).round() as usize;
            assert!(
                produced <= expected && expected - produced <= 3 * CHUNK,
                "{device} Hz: fed {} expected ~{expected} produced {produced}",
                input.len()
            );
        }
    }

    /// The boundary-buffering defect class: a ramp in must be one unbroken
    /// line out, whatever callback sizes deliver it. A single dropped or
    /// doubled sample anywhere moves every later sample a whole sample off
    /// the line, which `assert_line` catches. Run at every device rate,
    /// because the backlog and staging arguments this depends on invert when
    /// the ratio crosses one.
    ///
    /// The measured line offset is the converter's real group delay, so this
    /// is also the honesty check on the advertised latency: the figure the
    /// constructor reports must match what the audio actually experienced,
    /// within a fraction of a millisecond.
    #[test]
    fn odd_callback_sizes_preserve_the_ramp() {
        let input = ramp();
        for device in DEVICE_RATES {
            let (mut capture, sink, added) = capture_rig(device);
            let mut pos = 0;
            for size in SIZES.iter().cycle() {
                let end = (pos + size).min(input.len());
                capture(&input[pos..end]);
                pos = end;
                if pos == input.len() {
                    break;
                }
            }
            let out = sink.lock().unwrap();
            let slope = f64::from(device) / f64::from(SESSION);
            let delay = assert_line(device, &out, slope);
            // The measured group delay against the reported added latency:
            // one staged chunk on top of the delay the line actually shows.
            let measured_ms =
                (delay / f64::from(device) + CHUNK as f64 / f64::from(SESSION)) * 1000.0;
            assert!(
                (measured_ms - f64::from(added)).abs() < 0.1,
                "{device} Hz: constructor reports {added} ms, \
                 the audio measured {measured_ms:.3} ms"
            );
        }
    }

    /// The playback direction, same defect class: session-rate pulls of a
    /// ramp must come out as one unbroken device-rate line whatever request
    /// sizes the device makes.
    #[test]
    fn odd_playback_requests_preserve_the_ramp() {
        for device in DEVICE_RATES {
            let next = Arc::new(AtomicUsize::new(0));
            let inner_next = Arc::clone(&next);
            let inner: PlaybackFn = Box::new(move |out: &mut [f32]| {
                for s in out.iter_mut() {
                    *s = inner_next.fetch_add(1, Ordering::Relaxed) as f32;
                }
            });
            let (mut playback, added) = converting_playback(inner, SESSION, device, 1);
            let mut out = Vec::new();
            let mut buf = [0.0f32; 4096];
            for &size in SIZES.iter().cycle().take(70) {
                playback(&mut buf[..size]);
                out.extend_from_slice(&buf[..size]);
            }
            let slope = f64::from(SESSION) / f64::from(device);
            let delay = assert_line(device, &out, slope);
            let measured_ms =
                (delay / f64::from(SESSION) + CHUNK as f64 / f64::from(SESSION)) * 1000.0;
            assert!(
                (measured_ms - f64::from(added)).abs() < 0.1,
                "{device} Hz: constructor reports {added} ms, \
                 the audio measured {measured_ms:.3} ms"
            );
        }
    }

    /// The defect the ladder exists to prevent: a 440 Hz sine captured from a
    /// device on its own clock must still be 440 Hz on the 48 kHz side, at
    /// the same level. An unconverted 44.1 path would read 479 Hz here, and
    /// an unconverted 96 kHz one 220.
    #[test]
    fn a_440_hz_sine_survives_conversion_at_pitch_and_level() {
        for device in DEVICE_RATES {
            let input: Vec<f32> = (0..3 * device as usize)
                .map(|i| sine(440.0, device, i))
                .collect();
            let out = convert_capture(device, &input, &[device as usize / 100]);
            let steady = &out[480..];
            let secs = steady.len() as f64 / f64::from(SESSION);
            let hz = cycles(steady) as f64 / secs;
            assert!(
                (hz - 440.0).abs() < 1.5,
                "{device} Hz: pitch moved to {hz:.2} Hz out of 440 in"
            );
            let level = rms(steady);
            let expected = 0.5 / std::f64::consts::SQRT_2;
            assert!(
                (level - expected).abs() / expected < 0.03,
                "{device} Hz: level moved, rms {level:.4}, fed {expected:.4}"
            );
        }
    }

    /// The composition contract with the session's drift compensators: a
    /// device crystal 200 ppm fast comes out exactly 200 ppm fast at 48 k,
    /// because the ratio is fixed. The converter must pass drift through,
    /// never absorb it; the compensators are the one steered stage.
    #[test]
    fn a_200_ppm_fast_device_is_still_200_ppm_fast_at_48_k() {
        // Both sides of unity: a crystal error is a fraction, so a converter
        // that absorbed it would do so whichever way the ratio points.
        for device in [DEVICE, 96_000] {
            let secs = 60usize;
            let fed = (secs as f64 * f64::from(device) * (1.0 + 200e-6)) as usize;
            let input = vec![0.25f32; fed];
            let produced = convert_capture(device, &input, &[device as usize / 10]).len();
            let expected = (fed as f64 * f64::from(SESSION) / f64::from(device)) as usize;
            assert!(
                produced <= expected && expected - produced <= 3 * CHUNK,
                "{device} Hz: fed {fed} expected ~{expected} produced {produced}"
            );
            // The nominal-rate count would be 200 ppm lower; the excess must
            // still be there for the compensators to measure.
            let nominal = secs * SESSION as usize;
            assert!(
                produced > nominal + 400,
                "{device} Hz: the 200 ppm excess was absorbed, \
                 produced {produced} against nominal {nominal}"
            );
        }
    }

    /// Pins the figures the module doc states, from the constructor's own
    /// report; the empirical cross-check lives in the ramp tests. The bound
    /// across the rest of the table is the product claim: no device the
    /// ladder carries costs more than a few milliseconds here. The widest
    /// figure is capture from 8 kHz, where the filter's delay is six output
    /// frames per input frame; the ladder refuses that direction before it
    /// reaches this converter.
    #[test]
    fn the_reported_added_latency_matches_the_documented_figures() {
        let (_, _, capture_added) = capture_rig(DEVICE);
        let inner: PlaybackFn = Box::new(|_out: &mut [f32]| {});
        let (_, playback_added) = converting_playback(inner, SESSION, DEVICE, 1);
        assert!(
            (capture_added - 3.208).abs() < 0.01,
            "capture adds {capture_added} ms"
        );
        assert!(
            (playback_added - 3.158).abs() < 0.01,
            "playback adds {playback_added} ms"
        );

        for device in DEVICE_RATES {
            let (_, _, capture_added) = capture_rig(device);
            let inner: PlaybackFn = Box::new(|_out: &mut [f32]| {});
            let (_, playback_added) = converting_playback(inner, SESSION, device, 1);
            for (side, added) in [("capture", capture_added), ("playback", playback_added)] {
                assert!(
                    (2.6..7.0).contains(&added),
                    "{device} Hz {side} adds {added} ms"
                );
            }
        }
    }

    /// The scaling behind [`crate::StreamHandle::buffer_frames`]'s one-unit
    /// contract: device-rate callback sizes become session-rate frames,
    /// rounded up so a ring sized from the answer always fits the callback.
    #[test]
    fn callback_sizes_scale_to_session_rate_frames() {
        // The negotiated sizes real 44.1 kHz devices deliver: the WASAPI
        // 10 ms period, a half-period, and the 120-frame request.
        assert_eq!(session_frames(441, SESSION, DEVICE), 480);
        assert_eq!(session_frames(480, SESSION, DEVICE), 523);
        assert_eq!(session_frames(120, SESSION, DEVICE), 131);
        assert_eq!(
            session_frames(240, SESSION, SESSION),
            240,
            "unity is untouched"
        );
        // A faster device delivers more device frames per callback than the
        // handler sees, so the scale runs the other way: the ring must not be
        // sized from the raw figure in either direction.
        assert_eq!(session_frames(1_024, SESSION, 192_000), 256);
        assert_eq!(session_frames(960, SESSION, 96_000), 480);
        assert_eq!(session_frames(128, SESSION, 8_000), 768);
    }

    /// Both wrappers run inside device callbacks, so after construction they
    /// must never touch the heap, across every callback size up to and past
    /// the slicing bound and at every device rate. The backlog and staging
    /// capacities that make this true are reasoned from the ratio, and the
    /// reasoning inverts when the ratio crosses one, so a rate table is the
    /// only way this property is actually asserted.
    #[test]
    fn the_wrappers_do_not_allocate_after_construction() {
        let pcm = [0.1f32; 2 * 5000];
        let mut out = [0.0f32; 2 * 5000];
        for device in DEVICE_RATES {
            let seen = Arc::new(AtomicUsize::new(0));
            let inner_seen = Arc::clone(&seen);
            let capture_inner: CaptureFn = Box::new(move |samples: &[f32]| {
                inner_seen.fetch_add(samples.len(), Ordering::Relaxed);
            });
            let (mut capture, _) = converting_capture(capture_inner, SESSION, device, 2);
            let playback_inner: PlaybackFn = Box::new(|out: &mut [f32]| out.fill(0.25));
            let (mut playback, _) = converting_playback(playback_inner, SESSION, device, 2);

            // Warmup exercises every internal path once, including a callback
            // past MAX_CHUNK_FRAMES that takes the slicing branch.
            capture(&pcm);
            playback(&mut out);

            let before = heap_ops();
            for (i, &size) in SIZES.iter().cycle().take(500).enumerate() {
                capture(&pcm[..2 * size.min(4999) + 2 * (i % 2)]);
                playback(&mut out[..2 * size.min(4999)]);
            }
            assert_eq!(
                heap_ops() - before,
                0,
                "a wrapper allocated after construction at {device} Hz"
            );
            assert!(seen.load(Ordering::Relaxed) > 0, "{device} Hz");
        }
    }
}
