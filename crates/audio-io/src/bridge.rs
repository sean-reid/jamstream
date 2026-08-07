//! RT-safe plumbing between device callbacks and the network thread.
//!
//! Two SPSC rings: capture flows device -> engine, playout flows
//! engine -> device. The device side never allocates or locks after
//! construction; counters are relaxed atomics shared with the engine side.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rtrb::{Consumer, Producer, RingBuffer};

use crate::types::DuplexHandler;

#[derive(Debug, Default)]
struct Counters {
    /// Playback callbacks that found fewer samples than they needed and
    /// padded with silence. Counted per callback, not per missing sample.
    underruns: AtomicU64,
    /// Capture callbacks that could not fit everything into the ring and
    /// dropped the tail. Counted per callback, not per dropped sample.
    overruns: AtomicU64,
}

/// Constructor namespace for the device/engine ring pair.
#[derive(Debug)]
pub struct CallbackBridge;

impl CallbackBridge {
    /// Capacities are in f32 samples, one per ring. Multichannel callers
    /// should pass frames times channels since the rings carry interleaved
    /// samples.
    ///
    /// The two are separate because they cost different things. The playout
    /// ring is kept full by its producer, so its capacity is the cushion the
    /// device plays out of and every sample of it is latency. The capture ring
    /// is drained to empty by its consumer, so its capacity is only how long
    /// that consumer may be held up before audio is lost, and costs nothing
    /// while the consumer keeps up.
    // The two halves are the product; CallbackBridge itself is never held.
    #[allow(clippy::new_ret_no_self)]
    #[must_use]
    pub fn new(capture_capacity: usize, playout_capacity: usize) -> (DeviceSide, EngineSide) {
        let (capture_tx, capture_rx) = RingBuffer::new(capture_capacity);
        let (playout_tx, playout_rx) = RingBuffer::new(playout_capacity);
        let counters = Arc::new(Counters::default());
        (
            DeviceSide {
                capture_tx,
                playout_rx,
                counters: Arc::clone(&counters),
            },
            EngineSide {
                capture_rx,
                playout_tx,
                counters,
            },
        )
    }
}

fn push_capture(tx: &mut Producer<f32>, counters: &Counters, samples: &[f32]) {
    let (_, rest) = tx.push_partial_slice(samples);
    if !rest.is_empty() {
        counters.overruns.fetch_add(1, Ordering::Relaxed);
    }
}

fn pull_playout(rx: &mut Consumer<f32>, counters: &Counters, out: &mut [f32]) {
    let (_, rest) = rx.pop_partial_slice(out);
    if !rest.is_empty() {
        rest.fill(0.0);
        counters.underruns.fetch_add(1, Ordering::Relaxed);
    }
}

/// Lives on the device threads. Either call the methods directly (offline
/// backends pump from one thread) or split into a [`DuplexHandler`] whose
/// halves real backends move onto their capture and playback threads.
#[derive(Debug)]
pub struct DeviceSide {
    capture_tx: Producer<f32>,
    playout_rx: Consumer<f32>,
    counters: Arc<Counters>,
}

impl DeviceSide {
    /// Push captured samples toward the engine. Drops the tail and counts an
    /// overrun if the ring is full.
    pub fn on_capture(&mut self, samples: &[f32]) {
        push_capture(&mut self.capture_tx, &self.counters, samples);
    }

    /// Fill `out` from the playout ring. Pads with silence and counts an
    /// underrun if the ring runs dry.
    pub fn on_playback(&mut self, out: &mut [f32]) {
        pull_playout(&mut self.playout_rx, &self.counters, out);
    }

    #[must_use]
    pub fn into_handler(self) -> DuplexHandler {
        let DeviceSide {
            mut capture_tx,
            mut playout_rx,
            counters,
        } = self;
        let capture_counters = Arc::clone(&counters);
        DuplexHandler::new(
            move |samples: &[f32]| push_capture(&mut capture_tx, &capture_counters, samples),
            move |out: &mut [f32]| pull_playout(&mut playout_rx, &counters, out),
        )
    }
}

/// Lives on the network/engine thread.
#[derive(Debug)]
pub struct EngineSide {
    capture_rx: Consumer<f32>,
    playout_tx: Producer<f32>,
    counters: Arc<Counters>,
}

impl EngineSide {
    /// Pull captured samples; returns how many were written into `out`.
    pub fn pull_captured(&mut self, out: &mut [f32]) -> usize {
        let (got, _) = self.capture_rx.pop_partial_slice(out);
        got.len()
    }

    /// Push playout samples toward the device; returns how many fit.
    pub fn push_playout(&mut self, samples: &[f32]) -> usize {
        let (pushed, _) = self.playout_tx.push_partial_slice(samples);
        pushed.len()
    }

    #[must_use]
    pub fn underruns(&self) -> u64 {
        self.counters.underruns.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn overruns(&self) -> u64 {
        self.counters.overruns.load(Ordering::Relaxed)
    }
}
