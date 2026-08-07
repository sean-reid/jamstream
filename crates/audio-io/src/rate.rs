//! Which rung of the sample-rate ladder each direction of a stream landed
//! on.
//!
//! The wire and engine run 48 kHz whatever the device does; what varies per
//! machine is how a direction gets there, and every answer that is not
//! "natively" costs something a musician should be able to read: a device
//! clock this app moved, an OS converter's latency, or the boundary
//! converter's disclosed milliseconds. The answer is a property of one open
//! stream, never of the process, so it rides
//! [`crate::StreamHandle::rate_outcomes`] rather than a global like the
//! sharing mode: a reopen racing a read must never show one stream the
//! other's outcome.

/// How one direction of the device stream reached the session rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RateOutcome {
    /// The device runs at the session rate; nothing was moved or converted.
    Native,
    /// This app moved the whole device clock to the session rate (rung 2),
    /// away from the `from` it was at. Free of latency, but visible to every
    /// other program using the device, which is why it is disclosed.
    ClockSet { from: u32 },
    /// The OS carries the session-rate stream over a device engine running
    /// at its own rate (WASAPI render's AUTOCONVERTPCM, the PipeWire graph).
    /// Accepted by design because it is disclosed like our own converter.
    OsConverted { device: u32 },
    /// The boundary converter in `resample` carries the difference (rung 3),
    /// adding `added_ms` of latency on this direction. The figure comes from
    /// the converter's own constructor, so it cannot drift from what the
    /// audio experiences.
    Resampled { device: u32, added_ms: f32 },
}

/// Both directions of the most recently opened device stream. The directions
/// are independent: a 44.1 kHz microphone next to 48 kHz speakers converts
/// capture only.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateOutcomes {
    pub capture: RateOutcome,
    pub playback: RateOutcome,
}

/// One log line per direction that landed anywhere other than rung 1, at
/// open: the log is the record the status line's once-per-change notice
/// cannot be. Every backend calls this when it publishes its outcomes.
pub(crate) fn log_rate_outcomes(outcomes: &RateOutcomes) {
    for (direction, outcome) in [
        ("capture", outcomes.capture),
        ("playback", outcomes.playback),
    ] {
        match outcome {
            RateOutcome::Native => {}
            RateOutcome::ClockSet { from } => {
                tracing::info!(
                    direction,
                    from,
                    "moved the device clock to the session rate"
                );
            }
            RateOutcome::OsConverted { device } => {
                tracing::info!(
                    direction,
                    device,
                    "the OS is converting between the stream and the device's own rate"
                );
            }
            RateOutcome::Resampled { device, added_ms } => {
                tracing::info!(
                    direction,
                    device,
                    added_ms,
                    "converting at the device boundary"
                );
            }
        }
    }
}
