//! The live runtime's watchers: the small stateful things that sample a
//! counter or a clock on a tick, hold a condition over a window, and hand the
//! answer to the log or to the snapshot.
//!
//! Each one owns the constants its window is cut from, so a threshold and the
//! state it governs are read together. Every one that has a clock is handed it
//! by [`Worker::step`](super::Worker::step) rather than reading one, which is
//! what lets a test spend a ninety-second window in no wall-clock time.

use std::time::{Duration, Instant};

use jamstream_audio_io::{EngineSide, ThreadPriority};
use jamstream_engine::{JitterStats, LossWindow};

use super::{CHANNELS, CHUNK_STEREO, TICK, cushion_time, playout_capacity, playout_target};
use crate::runtime::MemberId;

/// Base backoff between attempts to reopen a lost or misconfigured stream.
/// The first attempt of an episode is immediate; each one after it waits
/// twice as long as the last, to [`REOPEN_BACKOFF_MAX`].
const REOPEN_INTERVAL: Duration = Duration::from_millis(500);
/// Longest the reopen loop waits between attempts.
const REOPEN_BACKOFF_MAX: Duration = Duration::from_secs(4);
/// Attempts one episode gets before the loop stops and says so. Six of them
/// span about twelve seconds, which is long enough for a driver to come back
/// and short enough that a musician is not left watching dead meters.
const REOPEN_ATTEMPTS_MAX: u32 = 6;
/// A stream that has run this long has recovered: the episode ends, so the
/// next loss is retried at once and announced again.
pub(super) const STREAM_SETTLED_AFTER: Duration = Duration::from_secs(5);

/// One run of the reopen loop, from the first loss to the stream that stays
/// up for [`STREAM_SETTLED_AFTER`].
///
/// The first attempt of an episode is immediate, so a genuine unplug is
/// reopened on the next tick. Each attempt after it waits twice as long, and
/// the budget stops the loop entirely. Without both, a device that opens and
/// then latches before the next tick was closed and reopened every 2.5 ms,
/// and a real open costs 10-100 ms, so the rings went unserviced for the
/// whole episode.
#[derive(Debug, Default)]
pub(super) struct ReopenEpisode {
    pub(super) attempts: u32,
    /// Whether this episode began with a stream that stopped on its own.
    /// A reopen for a pick somebody just made is not a fault, and the UI
    /// tells them apart by this.
    pub(super) faulted: bool,
}

impl ReopenEpisode {
    /// The wait owed before the next attempt.
    pub(super) fn backoff(&self) -> Duration {
        match self.attempts.checked_sub(1) {
            None => Duration::ZERO,
            Some(n) => REOPEN_INTERVAL
                .saturating_mul(1u32 << n.min(16))
                .min(REOPEN_BACKOFF_MAX),
        }
    }

    /// Whether the budget is spent and the loop should stop trying.
    pub(super) fn spent(&self) -> bool {
        self.attempts >= REOPEN_ATTEMPTS_MAX
    }
}

/// How long playout may hand out nothing but zeros on a joined session before
/// the log says so. The deepest legitimate refill is the buffer's own
/// `MAX_TARGET` of 24 frames, 60 ms, so a second of it is not the buffer
/// filling: it is a member hearing silence.
const SILENT_PLAYOUT_AFTER: Duration = Duration::from_secs(1);
/// How long every pull may conceal on a joined session before the log calls it
/// a dropout. Two bounds set it. Below
/// [`JitterBuffer::HEAL_TICKS`](jamstream_engine::JitterBuffer::HEAL_TICKS),
/// 210 ms, the gap may still be the buffer fixing a playout position it cannot
/// reconcile, so a warning there would name the cure as the disease. And a
/// quarter second is the loosest silence the harness lets the media path pass,
/// so nothing this warns about is a gap the product already calls acceptable.
/// Under it sits ordinary jitter, a frame or a few, which is what concealment
/// exists to hide.
const CONCEALED_GAP_AFTER: Duration = Duration::from_millis(250);
/// Window the refused-frame rate is measured over, and the count inside it
/// that means the arriving stream disagrees with playout rather than the
/// network dropping the odd packet. Media arrives one frame per tick, 400 a
/// second, and reordering strands a few percent of them; half of a second's
/// frames refused cannot be that.
const REFUSED_WINDOW: Duration = Duration::from_secs(1);
const REFUSED_WINDOW_LIMIT: u64 = 200;

/// Watches the local jitter buffer for the three faults that leave a connected
/// session sounding broken and show up nowhere else: playout handing out zeros
/// because the buffer never filled, playout concealing a gap long enough to
/// hear, and frames arriving only to be refused.
///
/// All three are warnings because the log file promises that an empty file is a
/// healthy run, and a member who heard nobody for a whole session found nothing
/// in it. All three are one line per episode, like the ring counters: at
/// 2.5 ms a tick, warning per tick would put hundreds of lines a second in a
/// file people mail us.
///
/// It reads counters rather than pull outcomes, so it needs no seam through the
/// core. `waiting` moving while `pulled` stands still is exactly a run of
/// `Pull::Waiting`, the one branch that writes literal zeros. `lost` moving
/// with `pulled` frame for frame is a run where every pull concealed, the
/// branch that writes invented audio. And `late` is the frames the buffer
/// refused, which no other surface carries at all.
#[derive(Default)]
pub(super) struct PlayoutWatch {
    /// Last tick's counters; the deltas are what they mean here.
    prev: Option<JitterStats>,
    /// When the current run of silence began, and whether it has been said.
    silent_since: Option<Instant>,
    silent_said: bool,
    /// The open run of concealed pulls, and whether it has been said.
    gap: Option<Gap>,
    gap_said: bool,
    /// The open refusal window: when it started and `late` as it stood then.
    refused_window: Option<(Instant, u64)>,
    refused_said: bool,
}

/// A run of ticks whose every pull concealed, from the first one seen and the
/// counters as they stood before it, so the line carries the run's own numbers
/// rather than the session's totals.
#[derive(Clone, Copy)]
struct Gap {
    since: Instant,
    lost: u64,
    late: u64,
}

impl PlayoutWatch {
    /// One tick's worth of observation. `joined_as` is the member this client
    /// is joined as, and None whenever it is not joined: before the session is
    /// up nothing is arriving yet, and silence then is the connection's story
    /// to tell.
    pub(super) fn observe(
        &mut self,
        now: Instant,
        joined_as: Option<MemberId>,
        stats: JitterStats,
    ) {
        let prev = self.prev.replace(stats);
        let Some(member) = joined_as else {
            self.forget();
            return;
        };
        let Some(prev) = prev else { return };
        // A reconnect builds a fresh buffer, so a counter that went backwards
        // is a new stream and not an event.
        if stats.pulled < prev.pulled
            || stats.late < prev.late
            || stats.lost < prev.lost
            || stats.waiting < prev.waiting
        {
            self.forget();
            return;
        }

        // Zeros went out and nothing playable did: the buffer has not filled.
        if stats.waiting > prev.waiting && stats.pulled == prev.pulled {
            let since = *self.silent_since.get_or_insert(now);
            if !self.silent_said && now.duration_since(since) >= SILENT_PLAYOUT_AFTER {
                self.silent_said = true;
                tracing::warn!(
                    member = member.0,
                    depth_frames = stats.depth_frames,
                    target_frames = stats.target_frames,
                    late = stats.late,
                    reanchors = stats.reanchors,
                    silent_ms = now.duration_since(since).as_millis(),
                    "playout is silence: the jitter buffer has not filled"
                );
            }
        } else {
            self.silent_since = None;
            self.silent_said = false;
        }

        // Every pull since the last tick concealed, so what went out was the
        // decoder inventing audio the stream did not carry. A tick that pulled
        // nothing holds the run open rather than ending it: a growth hold
        // conceals too, and a re-anchored buffer plays nothing while it refills.
        let pulled = stats.pulled - prev.pulled;
        if pulled > 0 {
            if stats.lost - prev.lost == pulled {
                self.gap.get_or_insert(Gap {
                    since: now,
                    lost: prev.lost,
                    late: prev.late,
                });
            } else {
                self.gap = None;
                self.gap_said = false;
            }
        }
        if let Some(gap) = self.gap {
            let held = now.duration_since(gap.since);
            if !self.gap_said && held >= CONCEALED_GAP_AFTER {
                self.gap_said = true;
                tracing::warn!(
                    member = member.0,
                    gap_ms = held.as_millis(),
                    concealed = stats.lost - gap.lost,
                    refused = stats.late - gap.late,
                    reanchors = stats.reanchors,
                    depth_frames = stats.depth_frames,
                    target_frames = stats.target_frames,
                    "playout is concealing a gap: nothing arrived in time to play"
                );
            }
        }

        match self.refused_window {
            None => self.refused_window = Some((now, stats.late)),
            Some((from, late_then)) if now.duration_since(from) >= REFUSED_WINDOW => {
                let refused = stats.late - late_then;
                if refused < REFUSED_WINDOW_LIMIT {
                    self.refused_said = false;
                } else if !self.refused_said {
                    self.refused_said = true;
                    tracing::warn!(
                        member = member.0,
                        refused,
                        late = stats.late,
                        depth_frames = stats.depth_frames,
                        target_frames = stats.target_frames,
                        reanchors = stats.reanchors,
                        "media is arriving and being refused: its timing and playout disagree"
                    );
                }
                self.refused_window = Some((now, stats.late));
            }
            Some(_) => {}
        }
    }

    /// Drops every episode without saying anything: the stream this was
    /// watching is gone, and the next one starts its own.
    fn forget(&mut self) {
        self.silent_since = None;
        self.silent_said = false;
        self.gap = None;
        self.gap_said = false;
        self.refused_window = None;
        self.refused_said = false;
    }
}

/// Window the downlink's loss rate is measured over: the server's own Stats
/// interval, because the bar shows the two directions side by side and rates
/// over unequal windows would not be comparable.
const DOWNLINK_LOSS_WINDOW: Duration = Duration::from_millis(jamstream_session::STATS_INTERVAL_MS);

/// The downlink's loss as a rate over the last closed window, from the local
/// jitter buffer's counters: the audio this machine did not play, next to the
/// uplink figure the server sends for the audio the band did not hear.
///
/// A rate, not a ratio since joining. A lifetime ratio only comes down at the
/// frame clock, so one bad moment early keeps the readout high for the rest of
/// the session while the link is healthy, and a windowed figure beside a
/// cumulative one is not one quantity.
#[derive(Default)]
pub(super) struct DownlinkLoss {
    /// When the window now filling opened, and the counters as they stood.
    from: Option<(Instant, JitterStats)>,
    /// The last closed window's rate. `None` until one closes, and again
    /// whenever the buffer is rebuilt, which reads as no figure rather than a
    /// figure for a window that never existed.
    pct: Option<f32>,
}

impl DownlinkLoss {
    /// One tick's worth of observation. Returns the rate to publish, which is
    /// the last closed window's for as long as the next one is filling, the
    /// same way the server's uplink figure stands until its next report.
    /// `joined` is the gate: nothing is owed to a client that is not in a
    /// session, so there is no rate to report either.
    pub(super) fn observe(
        &mut self,
        now: Instant,
        joined: bool,
        stats: JitterStats,
    ) -> Option<f32> {
        if !joined {
            *self = DownlinkLoss::default();
            return None;
        }
        match self.from {
            None => self.from = Some((now, stats)),
            Some((since, prev)) if now.duration_since(since) >= DOWNLINK_LOSS_WINDOW => {
                self.pct = LossWindow::between(&prev, &stats).map(|w| w.wire_loss_pct());
                self.from = Some((now, stats));
            }
            Some(_) => {}
        }
        self.pct
    }
}

/// Wait before the ring counters are reported again, and the ceiling that wait
/// doubles to. A burst at open is then one line, while a ring that keeps
/// dropping says so for as long as it does without filling the file: a single
/// once-per-stream count cannot distinguish a burst from a total that is
/// still climbing.
const RING_REPORT_AGAIN: Duration = Duration::from_secs(1);
const RING_REPORT_MAX: Duration = Duration::from_secs(60);
/// Underruns inside this window that mark a run as crackling, and the window
/// a run of them has to hold inside. One underrun alone is what opening a
/// stream costs; a floor of three sits above that and below the five a
/// crackling run took in ninety seconds on real hardware. The same window
/// closes a run out again: this long without a fresh underrun is a stretch
/// of playing the ring kept up with, so the state clears rather than
/// latching for the rest of the session.
const CRACKLE_EPISODE_COUNT: u64 = 3;
const CRACKLE_EPISODE_WINDOW: Duration = Duration::from_secs(90);
/// Streams that stopped on their own inside this window for the device to read
/// as cutting out, and the window they have to bunch up inside. Each stop is a
/// hole in what everybody else hears, [`SLOW_REOPEN`](super::SLOW_REOPEN) long
/// or worse, and a machine coming out of sleep or handing a device between apps
/// spends two of them; three inside three minutes is a hole a minute, which no
/// device a band can play through produces. The same window without a stop closes the run
/// out again, so this reads as a device that is failing now rather than one
/// that had a bad moment an hour ago.
pub(super) const CUTTING_OUT_COUNT: u64 = 3;
pub(super) const CUTTING_OUT_WINDOW: Duration = Duration::from_secs(180);
/// Window the playout low water mark covers. The bridge tracks the minimum
/// since it was last read and reading resets it, so this is how often it is
/// read: long enough that a reading spans hundreds of device callbacks, short
/// enough that one bad moment ages out of it.
const PLAYOUT_LOW_WINDOW: Duration = Duration::from_secs(1);

/// Reports the bridge's dropped-capture and padded-playout counters as the log
/// sees them: the first movement at once, then again on a doubling wait for as
/// long as the count keeps climbing.
///
/// The cadence is the point: a single total, like `overruns=33` on a stream
/// that has been up for a second, cannot say whether that is a burst while
/// the session came up or the first second of a drip that runs for the
/// whole song. Those want different fixes, and the person who can hear the
/// damage is at the other end of the session, so the log is where it has to be
/// answerable. Each line carries the count since the last one and how long the
/// stream has been up, so the shape reads off the timestamps.
///
/// It also samples the playout low water mark, which is the same question asked
/// before the damage instead of after: the counters say the ring ran out, the
/// water mark says how close it came. That one goes to the snapshot rather than
/// the log, because a number that moves every second is not a log line.
pub(super) struct RingWatch {
    /// When the stream this watches opened; every line is dated from it.
    opened: Instant,
    overruns: CounterWatch,
    underruns: CounterWatch,
    crackling: EpisodeWatch,
    /// The last closed window's low water mark in frames. `None` means no
    /// render callback ran inside it.
    low_water_frames: Option<usize>,
    /// When the window now filling opened.
    low_water_from: Instant,
}

/// One counter's reporting state.
#[derive(Default)]
struct CounterWatch {
    /// The total as the last line reported it, and when that line went out.
    said: Option<(u64, Instant)>,
    /// The wait owed before this counter is reported again.
    wait: Duration,
}

/// Whether a counter is climbing densely enough for the person playing to
/// have noticed, the way [`PlayoutWatch`]'s concealed-gap state holds for a
/// run rather than firing on a sample. A window that reaches `count` fresh
/// increments turns the state on; a window that runs out first without
/// reaching it restarts from where it ran out, so a slow drip that never
/// bunches up inside one window never turns it on at all. Once on, the same
/// window without a fresh increment turns it back off, so the state reads as
/// "is this happening now" rather than "has it ever".
///
/// Two conditions ride on it: the playout ring's underruns, whose counter
/// belongs to a ring and starts again with each stream, and the device
/// streams that stopped on their own, whose counter runs for the session.
pub(super) struct EpisodeWatch {
    /// Fresh increments inside `window` that turn the state on.
    count: u64,
    /// Both the window a run has to bunch up inside and the quiet that ends
    /// one.
    window: Duration,
    prev: Option<u64>,
    /// When the run building toward the floor started, and the total as it
    /// stood then. `None` once the floor is reached; there is nothing left
    /// to build toward while the state is already on.
    since: Option<(Instant, u64)>,
    /// The last tick a fresh increment landed, which is what an active run
    /// measures its own quiet against to decide when it is over.
    last: Option<Instant>,
    active: bool,
}

impl RingWatch {
    pub(super) fn new(opened: Instant) -> RingWatch {
        RingWatch {
            opened,
            overruns: CounterWatch::default(),
            underruns: CounterWatch::default(),
            crackling: EpisodeWatch::new(CRACKLE_EPISODE_COUNT, CRACKLE_EPISODE_WINDOW),
            low_water_frames: None,
            low_water_from: opened,
        }
    }

    /// One tick's worth of observation, against the ring the counters belong
    /// to. Returns whether the ring is in a crackling run as of this tick.
    pub(super) fn observe(&mut self, now: Instant, engine: &EngineSide, ring_frames: u32) -> bool {
        if now.duration_since(self.low_water_from) >= PLAYOUT_LOW_WINDOW {
            self.low_water_from = now;
            // The bridge counts interleaved samples; a frame is one per channel.
            self.low_water_frames = engine
                .take_playout_low_water()
                .map(|samples| samples / usize::from(CHANNELS));
        }
        let up_ms = now.duration_since(self.opened).as_millis();
        let overruns = engine.overruns();
        if let Some(dropped) = self.overruns.due(now, overruns) {
            tracing::warn!(
                dropped,
                overruns,
                ring_frames,
                up_ms,
                "capture ring overflowed; captured audio was dropped"
            );
        }
        let underruns = engine.underruns();
        if let Some(padded) = self.underruns.due(now, underruns) {
            tracing::warn!(
                padded,
                underruns,
                ring_frames,
                up_ms,
                "playout ring ran dry; the device padded silence"
            );
        }
        self.crackling.observe(now, underruns)
    }

    /// The last closed window's low water mark, in frames.
    pub(super) fn playout_low_frames(&self) -> Option<usize> {
        self.low_water_frames
    }

    /// When the window behind that reading closed, so a consumer that acts once
    /// per reading can tell a fresh one from the one it has already acted on.
    pub(super) fn low_water_at(&self) -> Instant {
        self.low_water_from
    }

    /// Drops the water mark without waiting for a window to close: the ring it
    /// was measuring is gone, and the next stream's ring measures its own.
    pub(super) fn forget(&mut self) {
        self.low_water_frames = None;
    }
}

impl CounterWatch {
    /// Whether `total` earns a line now, and the count that line carries.
    fn due(&mut self, now: Instant, total: u64) -> Option<u64> {
        if total == 0 {
            return None;
        }
        match self.said {
            None => {
                self.said = Some((total, now));
                self.wait = RING_REPORT_AGAIN;
                Some(total)
            }
            Some((said, at)) if total > said && now.duration_since(at) >= self.wait => {
                self.said = Some((total, now));
                self.wait = (self.wait * 2).min(RING_REPORT_MAX);
                Some(total - said)
            }
            Some(_) => None,
        }
    }
}

impl EpisodeWatch {
    pub(super) fn new(count: u64, window: Duration) -> EpisodeWatch {
        EpisodeWatch {
            count,
            window,
            prev: None,
            since: None,
            last: None,
            active: false,
        }
    }

    /// Whether the run is open as of this tick, against the counter's total
    /// now.
    pub(super) fn observe(&mut self, now: Instant, total: u64) -> bool {
        let prev = self.prev.replace(total);
        if prev.is_some_and(|p| total < p) {
            // The counter moved backward: a fresh one, whose run starts empty
            // rather than continuing the old one.
            self.since = None;
            self.last = None;
            self.active = false;
            return false;
        }
        let fresh = prev.is_some_and(|p| total > p);
        if fresh {
            self.last = Some(now);
        }
        if self.active {
            if self
                .last
                .is_some_and(|t| now.duration_since(t) >= self.window)
            {
                self.active = false;
            }
            return self.active;
        }
        if !fresh {
            // Nothing new this tick: a window this stale never reached the
            // floor and is not worth carrying forward.
            if self
                .since
                .is_some_and(|(since, _)| now.duration_since(since) > self.window)
            {
                self.since = None;
            }
            return false;
        }
        let base = self
            .since
            .get_or_insert((now, prev.expect("fresh implies a previous total")))
            .1;
        if total - base >= self.count {
            self.active = true;
            self.since = None;
        }
        self.active
    }
}

/// The cushion moves one 2.5 ms frame at a time, in both directions. The
/// jitter buffer sets the size: the top-up loop pulls from it, so a target that
/// moved by more than a frame is pulled in the tick it moves and runs the
/// buffer's playout position past the sender, which the buffer can step back at
/// most one frame of. It is also what one top-up puts in, so nothing smaller
/// changes what the ring survives.
pub(super) const CUSHION_STEP: usize = CHUNK_STEREO;
/// Quiet the cushion needs behind it before it hands a frame of latency back.
/// The stretch that closes a crackling run out, for the measurement that set
/// that stretch: a crackling run took five underruns in ninety seconds on real
/// hardware, so anything shorter gives latency away between two underruns of
/// one run and takes it back on the next.
const CUSHION_QUIET: Duration = CRACKLE_EPISODE_WINDOW;

/// Moves the depth the top-up loop fills to, from how close the playout ring
/// came to empty over the last window.
///
/// Up on the reading that saw it, down only behind a long quiet stretch,
/// because the two directions cost different things. A ring that came within
/// one top-up of padding silence is one late wakeup from something a musician
/// hears, and the frame that covers it costs 2.5 ms nobody can pick out. Coming
/// down buys that latency back and nothing else, while getting it wrong is an
/// audible dropout, so it takes [`CUSHION_QUIET`] of readings that would every
/// one of them have survived the shallower depth, and gives back a frame rather
/// than returning to the floor. While a take is recording or the broadcast is on
/// air it gives nothing back at all, because the dropout it would risk lands in
/// audio nobody can play again.
///
/// Nearly dry is one device callback, which is what the next drain takes, plus
/// one [`CUSHION_STEP`], which is what one top-up puts back. A dip that leaves
/// less than that served every callback and would not have served one more, so
/// it is the last reading before the damage rather than the first one after it.
/// The reading a machine keeping up gives is the whole cushion, measured over
/// twenty seconds of 120-frame callbacks at real-time priority, so there is a
/// whole frame of daylight between a healthy machine and this line.
///
/// Both bounds are latency. The floor is the base cushion of two callbacks, so
/// a machine that keeps up pays exactly what it pays today. The ceiling is
/// twice the floor rather than the
/// [`PLAYOUT_CUSHION_MAX`](super::PLAYOUT_CUSHION_MAX) the ring is cut for: at
/// twice the floor the cushion has spent the latency of the next buffer size up,
/// and that size would have bought a device callback twice as long, which the
/// platform schedules better and which asks this loop for half as many wakeups.
/// A machine still drying out there wants the buffer control, not a deeper ring.
pub(super) struct CushionControl {
    /// Depth the loop fills to, in interleaved samples.
    target: usize,
    /// The base cushion, which is the shallowest this ever asks for.
    floor: usize,
    ceiling: usize,
    /// One device callback in interleaved samples, which is what one drain
    /// takes out of the ring.
    callback: usize,
    /// The window this last acted on, so one reading moves the depth once.
    read_at: Option<Instant>,
    /// Whether the ring spent part of the window now closing reaching a depth it
    /// had just been given, which makes that reading evidence about the depth
    /// before it and grows the cushion twice for one dip. True at the open as
    /// well: the device renders ahead of this loop's first tick, so the window a
    /// stream opens in holds a dip nothing could have prevented.
    catching_up: bool,
    /// When the run of readings deep enough to give a frame back began; None
    /// whenever one broke it.
    quiet_since: Option<Instant>,
    /// Whether the ceiling has been said for the run now sitting at it.
    said: bool,
}

impl CushionControl {
    pub(super) fn new(device_frames: u32) -> CushionControl {
        let floor = playout_target(device_frames);
        CushionControl {
            target: floor,
            floor,
            // The ring is cut for the deeper of the base cushion and
            // PLAYOUT_CUSHION_MAX, so a device whose own period is past that
            // ceiling has no room and starts at it.
            ceiling: (2 * floor).min(playout_capacity(device_frames)),
            callback: device_frames as usize * usize::from(CHANNELS),
            read_at: None,
            catching_up: true,
            quiet_since: None,
            said: false,
        }
    }

    /// The depth [`Worker::top_up_playout`](super::Worker::top_up_playout)
    /// fills to.
    pub(super) fn target(&self) -> usize {
        self.target
    }

    /// One window's reading: `low` is how close the ring came to empty over it
    /// in frames, and `at` is when the window closed. A window already acted
    /// on moves nothing, and a window no render callback ran in is no evidence
    /// in either direction.
    ///
    /// `held` is a take being recorded or a broadcast on air, either of which
    /// makes a step down a dropout in audio nobody can play again. Growing
    /// still happens, because the alternative there is the dropout.
    pub(super) fn observe(&mut self, at: Instant, low: Option<usize>, held: bool) {
        if self.read_at == Some(at) {
            return;
        }
        self.read_at = Some(at);
        let Some(low) = low else {
            self.quiet_since = None;
            return;
        };
        // The bridge counts frames; the target and the callback are
        // interleaved.
        let low = low * usize::from(CHANNELS);
        if self.catching_up {
            self.catching_up = false;
            self.quiet_since = None;
            return;
        }
        if low < self.callback + CUSHION_STEP {
            self.quiet_since = None;
            self.grow(low);
            return;
        }
        self.said = false;
        if held {
            // The quiet a frame back costs starts again once the take stops:
            // this reading is one the controller declined to act on, and
            // handing a frame back on it the tick a take ends is the same
            // decision taken one tick later.
            self.quiet_since = None;
            return;
        }
        self.give_back(at, low);
    }

    /// One frame deeper, or the line that says a deeper ring is the wrong
    /// answer now.
    fn grow(&mut self, low: usize) {
        if self.target >= self.ceiling {
            if !self.said {
                self.said = true;
                tracing::warn!(
                    cushion_ms = as_ms(cushion_time(self.target)),
                    low_frames = low / usize::from(CHANNELS),
                    callback_frames = self.callback / usize::from(CHANNELS),
                    "the playout ring keeps running close to empty at the deepest cushion \
                     this buffer size allows; the device wants a bigger one"
                );
            }
            return;
        }
        self.target = (self.target + CUSHION_STEP).min(self.ceiling);
        self.catching_up = true;
        tracing::info!(
            cushion_ms = as_ms(cushion_time(self.target)),
            low_frames = low / usize::from(CHANNELS),
            "the playout ring came close to empty; the cushion is a frame deeper"
        );
    }

    /// One frame back, once the readings behind it have earned it. A frame given
    /// back has to leave this window's dip above the line the cushion grows on,
    /// or the next reading takes it straight back and the cushion oscillates
    /// around a depth instead of settling on one.
    fn give_back(&mut self, at: Instant, low: usize) {
        if self.target <= self.floor {
            self.quiet_since = None;
            return;
        }
        let step = CUSHION_STEP.min(self.target - self.floor);
        if low < self.callback + CUSHION_STEP + step {
            self.quiet_since = None;
            return;
        }
        let since = *self.quiet_since.get_or_insert(at);
        if at.duration_since(since) < CUSHION_QUIET {
            return;
        }
        self.target -= step;
        self.quiet_since = Some(at);
        self.catching_up = true;
        tracing::info!(
            cushion_ms = as_ms(cushion_time(self.target)),
            low_frames = low / usize::from(CHANNELS),
            quiet_ms = at.duration_since(since).as_millis(),
            "the playout ring has been keeping up; the cushion gave a frame back"
        );
    }
}

/// Window the worker's wakeup pacing is measured over. A second, so the 99th
/// percentile of it is a few wakeups a second: that is the drip of padded
/// silence a musician hears as crackling, rather than the one late wakeup any
/// machine produces.
const WAKE_WINDOW: Duration = Duration::from_secs(1);
/// Buckets [`WakeWatch`] counts wakeup intervals in: one [`TICK`] wide each,
/// with a last one for everything past the ladder, which the window's maximum
/// reports exactly.
const WAKE_BUCKETS: usize = 33;

/// One window of the worker loop's wakeup pacing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct WakePacing {
    /// The 99th percentile interval, as the top of the bucket it fell in.
    pub(super) p99: Duration,
    /// The window's longest interval, which is exact.
    pub(super) max: Duration,
}

/// Times the worker loop's wakeups against the audio the playout ring holds.
///
/// The device side of playout runs on a clock of its own, so the interval
/// between two wakeups of the thread filling that ring is audio the device has
/// to have found there. An interval longer than the ring holds is silence the
/// device padded, and nothing else in the client measures the interval at all.
///
/// Counts per bucket rather than intervals: an exact 99th percentile needs every
/// interval a session ever produced. What the p99 here reports is the top of the
/// tick-wide bucket the 99th of a hundred fell in, which reads high by up to one
/// tick and is an estimate rather than a percentile.
///
/// One warning, on one condition: that the p99 is longer than the cushion. That
/// is the case where the ring cannot survive a single late wakeup, and it is the
/// only reading a person can act on, so a periodic report of the figure would
/// cost the log file its promise to stay empty on a healthy run and say nothing
/// in exchange.
pub(super) struct WakeWatch {
    /// The previous wakeup, and `None` before the first one.
    last: Option<Instant>,
    /// When the window now filling opened.
    from: Instant,
    counts: [u32; WAKE_BUCKETS],
    max: Duration,
    /// The last closed window's reading, and `None` until one closes.
    pacing: Option<WakePacing>,
    /// Whether the warning has gone out for the episode now running, so a
    /// machine that stays late says so once and one that recovers can say it
    /// again.
    said: bool,
}

impl WakeWatch {
    pub(super) fn new(from: Instant) -> WakeWatch {
        WakeWatch {
            last: None,
            from,
            counts: [0; WAKE_BUCKETS],
            max: Duration::ZERO,
            pacing: None,
            said: false,
        }
    }

    /// One wakeup, against the cushion it has to stay inside. `None` for a ring
    /// nothing is draining on a clock of its own, which is measured all the same
    /// and never warned about.
    pub(super) fn observe(
        &mut self,
        now: Instant,
        cushion: Option<Duration>,
        priority: ThreadPriority,
    ) {
        if let Some(last) = self.last.replace(now) {
            let gap = now.saturating_duration_since(last);
            self.counts[wake_bucket(gap)] += 1;
            self.max = self.max.max(gap);
        }
        if now.duration_since(self.from) < WAKE_WINDOW {
            return;
        }
        self.from = now;
        self.pacing = self.close();
        let (Some(pacing), Some(cushion)) = (self.pacing, cushion) else {
            return;
        };
        if pacing.p99 <= cushion {
            self.said = false;
            return;
        }
        if !self.said {
            self.said = true;
            tracing::warn!(
                p99_ms = as_ms(pacing.p99),
                max_ms = as_ms(pacing.max),
                cushion_ms = as_ms(cushion),
                priority = ?priority,
                "the thread filling playout wakes later than the ring holds"
            );
        }
    }

    /// The window's reading, with the counts it was taken from reset for the
    /// next one. `None` when no wakeup landed inside it.
    fn close(&mut self) -> Option<WakePacing> {
        let max = std::mem::replace(&mut self.max, Duration::ZERO);
        let counts = std::mem::replace(&mut self.counts, [0; WAKE_BUCKETS]);
        let counted = u64::from(counts.iter().sum::<u32>());
        if counted == 0 {
            return None;
        }
        let ninety_ninth = (counted * 99).div_ceil(100);
        let bucket = counts
            .iter()
            .scan(0u64, |seen, count| {
                *seen += u64::from(*count);
                Some(*seen)
            })
            .position(|seen| seen >= ninety_ninth)
            .unwrap_or(WAKE_BUCKETS - 1);
        let p99 = if bucket + 1 == WAKE_BUCKETS {
            max
        } else {
            TICK * (bucket as u32 + 1)
        };
        Some(WakePacing { p99, max })
    }

    /// The last closed window's reading.
    pub(super) fn pacing(&self) -> Option<WakePacing> {
        self.pacing
    }
}

/// The bucket a wakeup interval belongs to. One [`TICK`] wide each, with an
/// interval on a bucket's top edge inside it, so a loop waking exactly on the
/// tick reads as one tick rather than as two. Everything past the ladder lands
/// in the last bucket, where the window's maximum is the figure to read.
fn wake_bucket(gap: Duration) -> usize {
    let ticks = gap.as_micros().div_ceil(TICK.as_micros()).max(1) as usize;
    (ticks - 1).min(WAKE_BUCKETS - 1)
}

/// A duration as milliseconds for a log field or a readout. The tick is 2.5 of
/// them, so whole milliseconds would round away the figure being reported.
pub(super) fn as_ms(d: Duration) -> f64 {
    d.as_micros() as f64 / 1000.0
}

/// Mouth to ear at which hearing yourself through the server is worth
/// offering, and how long the figure holds above it before the offer goes out.
///
/// The threshold is where a band stops holding together by ear. Measured mouth
/// to ear is 14.7 ms across one city and 24.3 ms across one region, which bands
/// play through, and about 30 ms is the edge of feeling like one stage. Both
/// figures and this threshold are the same path: capture to the last buffer this
/// app hands the card, the playout cushion included. What the card holds after
/// that is on the far side of the threshold as well as the figure, so a reading
/// of 30 is a path of 30 plus whatever that is.
///
/// The window is what keeps a spike out. Of the terms in the figure only the
/// jitter buffer's depth moves on its own, and it walks its whole range back
/// down in under a second (one frame per 40 ms of patience, across at most 24
/// frames), so ten seconds is an order of magnitude clear of anything the
/// buffer can do.
const HEAR_SELF_OFFER_MS: f32 = 30.0;
const HEAR_SELF_OFFER_WINDOW: Duration = Duration::from_secs(10);

/// Whether this session has been far enough apart, for long enough, that
/// hearing yourself through the server is worth offering, the way
/// [`EpisodeWatch`] holds for a run rather than firing on a sample. A
/// reading over [`HEAR_SELF_OFFER_MS`] with somebody else playing starts a
/// run and anything else ends it, so a spike buys nothing; a run that holds
/// [`HEAR_SELF_OFFER_WINDOW`] puts the offer out, and it then stands, because
/// the person it is for is holding an instrument and looks up when they look
/// up.
///
/// Whoever has used the control has met the question and is never asked
/// again: the offer would otherwise arrive at somebody who turned it off on
/// purpose, and hearing yourself through speakers is a loop into the
/// microphone.
#[derive(Default)]
pub(super) struct HearSelfOffer {
    /// When the run now building started; `None` between runs.
    since: Option<Instant>,
    state: Offer,
}

/// Where the offer stands for the rest of the session.
#[derive(Default, PartialEq)]
enum Offer {
    /// Watching the figure, nothing said.
    #[default]
    Watching,
    /// On the Audio tab, and staying there.
    Standing,
    /// The control has been used, so there is nothing left to offer.
    Settled,
}

impl HearSelfOffer {
    /// Whether the offer stands as of this tick. `mouth_to_ear_ms` is `None`
    /// until a round trip has been measured, which is no evidence rather than
    /// good news, so it ends the run without answering.
    pub(super) fn observe(
        &mut self,
        now: Instant,
        mouth_to_ear_ms: Option<f32>,
        hear_self: bool,
        playing_with_others: bool,
    ) -> bool {
        if hear_self {
            self.state = Offer::Settled;
        }
        match self.state {
            Offer::Settled => {
                self.since = None;
                false
            }
            Offer::Standing => true,
            Offer::Watching => {
                let apart = playing_with_others
                    && mouth_to_ear_ms.is_some_and(|ms| ms > HEAR_SELF_OFFER_MS);
                if !apart {
                    self.since = None;
                    return false;
                }
                let since = *self.since.get_or_insert(now);
                if now.duration_since(since) >= HEAR_SELF_OFFER_WINDOW {
                    self.state = Offer::Standing;
                }
                self.state == Offer::Standing
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use jamstream_audio_io::CallbackBridge;
    use jamstream_engine::{JitterBuffer, MediaPacket};
    use jamstream_protocol::control::{DestinationState, StreamPlatform};
    use jamstream_protocol::ids::DestinationId;

    use super::*;
    use crate::live::tests::top_up;
    use crate::live::{
        FRAME_FRAMES, capture_capacity, fill_playout_to, playout_capacity, playout_target,
    };
    use crate::runtime::{DestinationView, RecordState, RecordView, recording_or_on_air};

    /// The cadence a device that keeps dying is retried on: the first loss at
    /// once, so a genuine unplug comes back on the next tick, then a doubling
    /// wait to the ceiling, then a stop. Before this the wait was cleared on
    /// every loss, so a device that latched between ticks was closed and
    /// reopened ~400 times a second for the rest of the session.
    #[test]
    fn the_reopen_cadence_widens_and_then_gives_up() {
        let mut episode = ReopenEpisode::default();
        let mut waits = Vec::new();
        while !episode.spent() {
            waits.push(episode.backoff());
            episode.attempts += 1;
        }
        assert_eq!(waits.len(), REOPEN_ATTEMPTS_MAX as usize, "{waits:?}");
        assert_eq!(waits[0], Duration::ZERO, "a one-shot loss reopens at once");
        assert_eq!(waits[1], REOPEN_INTERVAL);
        for pair in waits[1..].windows(2) {
            assert_eq!(
                pair[1],
                (pair[0] * 2).min(REOPEN_BACKOFF_MAX),
                "the wait must double to the ceiling and stay there: {waits:?}"
            );
        }
        // Long enough that a driver settling has a chance, short enough that
        // nobody watches dead meters for a minute.
        let span: Duration = waits.iter().sum();
        assert!(
            span > Duration::from_secs(5) && span < Duration::from_secs(30),
            "the whole budget spans {span:?}"
        );
    }

    /// The member the watched buffer plays for.
    const ME: MemberId = MemberId(7);

    /// Formatted log lines, behind the app's own default filter, so a test
    /// says both what the log file would hold and that these events are
    /// warnings rather than something the file never sees.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Captured {
        fn lines(&self) -> Vec<String> {
            String::from_utf8(self.0.lock().expect("captured log").clone())
                .expect("log is utf8")
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("captured log").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;

        fn make_writer(&'a self) -> Captured {
            self.clone()
        }
    }

    /// Runs `body` against a capturing subscriber carrying the CLI's default
    /// filter, which is the one the log file is written through.
    fn captured(body: impl FnOnce()) -> Vec<String> {
        use tracing_subscriber::layer::SubscriberExt as _;
        let cap = Captured::default();
        let subscriber = tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(cap.clone()),
            )
            .with(jamstream_cli::logging::filter(None));
        tracing::subscriber::with_default(subscriber, body);
        cap.lines()
    }

    /// A real jitter buffer, the watch over it, and a synthetic mix clock: one
    /// tick is one 2.5 ms pull, so the watch sees exactly what it sees in the
    /// worker, produced by the buffer itself rather than by a stand-in.
    struct Ticker {
        jitter: JitterBuffer,
        watch: PlayoutWatch,
        start: Instant,
        tick: u32,
    }

    impl Ticker {
        fn new() -> Ticker {
            Ticker {
                jitter: JitterBuffer::new(),
                watch: PlayoutWatch::default(),
                start: Instant::now(),
                tick: 0,
            }
        }

        /// `ticks` mix ticks, with `feed` handed the buffer and the tick number
        /// before each pull.
        fn run(
            &mut self,
            ticks: u32,
            joined_as: Option<MemberId>,
            mut feed: impl FnMut(&mut JitterBuffer, u32),
        ) {
            for _ in 0..ticks {
                feed(&mut self.jitter, self.tick);
                self.jitter.pull();
                let at = self.start + TICK * self.tick;
                self.watch.observe(at, joined_as, self.jitter.stats());
                self.tick += 1;
            }
        }
    }

    fn media(seq: u32) -> MediaPacket {
        MediaPacket {
            seq,
            timestamp: u64::from(seq) * FRAME_FRAMES as u64,
            payload: vec![0u8; 8],
            redundant: None,
        }
    }

    /// This tick's frame, in time, every tick.
    fn healthy(jitter: &mut JitterBuffer, tick: u32) {
        jitter.push(media(tick));
    }

    /// Nothing arrives at all, so every pull has nothing to play.
    fn nothing(_: &mut JitterBuffer, _: u32) {}

    /// This tick's frame every tick, save one that never arrives: the single
    /// concealed frame ordinary jitter produces, and the whole reason the
    /// decoder conceals.
    fn one_frame_short(jitter: &mut JitterBuffer, tick: u32) {
        if tick != 200 {
            jitter.push(media(tick));
        }
    }

    /// This tick's frame plus a copy of the one from 100 ms back, which is
    /// behind playout by more than the buffer is deep and is refused for it.
    fn stale_copies(jitter: &mut JitterBuffer, tick: u32) {
        jitter.push(media(tick));
        if let Some(old) = tick.checked_sub(40) {
            jitter.push(media(old));
        }
    }

    /// A joined client handed no media at all hears silence for the whole
    /// session. The log names it in one line, giving the numbers that
    /// separate "nothing is arriving" from "arriving and being refused", and
    /// one line only: three seconds of it at 2.5 ms a tick would otherwise be
    /// 1200 of them.
    #[test]
    fn a_client_handed_no_media_says_so_once() {
        let lines = captured(|| Ticker::new().run(1_200, Some(ME), |_, _| {}));
        assert_eq!(lines.len(), 1, "{lines:#?}");
        let line = &lines[0];
        assert!(
            line.contains("WARN"),
            "not a warning, so the file never sees it: {line}"
        );
        assert!(line.contains("playout is silence"), "{line}");
        for field in [
            "member=7",
            "depth_frames=0",
            "target_frames=1",
            "late=0",
            "reanchors=0",
        ] {
            assert!(line.contains(field), "no {field} in {line}");
        }
    }

    /// Media that arrives and is refused: the counterpart to a client
    /// hearing nothing at all. Every tick carries this tick's frame and a
    /// copy of one from 100 ms back, which is behind playout and dropped, so
    /// `late` climbs while depth stays at target and audio keeps playing.
    /// The reader has to be able to tell this from hearing nothing, because
    /// the causes are nothing alike.
    #[test]
    fn a_client_whose_media_is_refused_says_something_else() {
        let lines = captured(|| Ticker::new().run(1_200, Some(ME), stale_copies));
        assert_eq!(lines.len(), 1, "{lines:#?}");
        let line = &lines[0];
        assert!(line.contains("WARN"), "{line}");
        assert!(line.contains("being refused"), "{line}");
        assert!(
            !line.contains("playout is silence"),
            "a refused stream must not read as a silent one: {line}"
        );
        for field in [
            "member=7",
            "late=",
            "refused=",
            "depth_frames=",
            "reanchors=0",
        ] {
            assert!(line.contains(field), "no {field} in {line}");
        }
    }

    /// The direction that matters more: an ordinary session says nothing at
    /// all. The banner promises an empty file means a healthy run, so a watch
    /// that fires on the couple of ticks every start spends filling would cost
    /// the file its only claim.
    #[test]
    fn an_ordinary_stream_says_nothing() {
        let lines = captured(|| Ticker::new().run(1_200, Some(ME), healthy));
        assert!(lines.is_empty(), "{lines:#?}");
    }

    /// A stream that stops for a second and comes back. The buffer anchored
    /// long ago, so every pull conceals rather than waiting, and concealment has
    /// energy: no surface but this one can say the musician heard nothing. One
    /// line, and it names the gap's own length.
    #[test]
    fn a_dropout_says_how_long_it_lasted() {
        let lines = captured(|| {
            let mut t = Ticker::new();
            t.run(400, Some(ME), healthy);
            t.run(400, Some(ME), nothing);
            t.run(400, Some(ME), healthy);
        });
        assert_eq!(lines.len(), 1, "{lines:#?}");
        let line = &lines[0];
        assert!(
            line.contains("WARN"),
            "not a warning, so the file never sees it: {line}"
        );
        assert!(line.contains("concealing a gap"), "{line}");
        assert!(
            !line.contains("playout is silence"),
            "a buffer that anchored and ran dry is not one that never filled: {line}"
        );
        // The synthetic clock advances exactly one tick per pull, so the
        // reported length is the threshold to the millisecond.
        for field in [
            "member=7",
            "gap_ms=250",
            "concealed=101",
            "refused=0",
            "reanchors=0",
            "depth_frames=0",
        ] {
            assert!(line.contains(field), "no {field} in {line}");
        }
    }

    /// The frame the decoder exists to hide says nothing. A watch that fired on
    /// one concealed pull would warn on every session that ever loses a packet.
    #[test]
    fn a_single_concealed_frame_says_nothing() {
        let lines = captured(|| Ticker::new().run(1_200, Some(ME), one_frame_short));
        assert!(lines.is_empty(), "{lines:#?}");
    }

    /// 200 ms of concealment says nothing either: it is inside the longest gap
    /// the buffer closes on its own, and inside the silence the harness lets the
    /// media path pass, so it cannot yet be called a fault.
    #[test]
    fn a_gap_the_buffer_could_still_heal_says_nothing() {
        let lines = captured(|| {
            let mut t = Ticker::new();
            t.run(400, Some(ME), healthy);
            t.run(80, Some(ME), nothing);
            t.run(400, Some(ME), healthy);
        });
        assert!(lines.is_empty(), "{lines:#?}");
    }

    /// Recovery ends the episode, so a session that drops out twice says so
    /// twice. Otherwise the second half of a bad session reads as clean.
    #[test]
    fn a_second_dropout_is_its_own_episode() {
        let lines = captured(|| {
            let mut t = Ticker::new();
            for _ in 0..2 {
                t.run(400, Some(ME), healthy);
                t.run(400, Some(ME), nothing);
            }
            t.run(400, Some(ME), healthy);
        });
        assert_eq!(lines.len(), 2, "{lines:#?}");
        for line in &lines {
            assert!(line.contains("concealing a gap"), "{line}");
            assert!(line.contains("gap_ms=250"), "{line}");
        }
    }

    /// The threshold's floor, held against the buffer that sets it: a gap
    /// shorter than the buffer's own healing bound may be the buffer working.
    #[test]
    fn the_dropout_threshold_clears_the_buffer_healing_itself() {
        let heal = TICK * JitterBuffer::HEAL_TICKS;
        assert!(
            CONCEALED_GAP_AFTER > heal,
            "{CONCEALED_GAP_AFTER:?} would warn while the buffer is still \
             recovering, which takes up to {heal:?}"
        );
    }

    /// Silence before the session is up belongs to the connection, which
    /// reports itself. Warning here would put a line in every run that starts
    /// with a server slow to answer.
    #[test]
    fn silence_before_joining_says_nothing() {
        let lines = captured(|| Ticker::new().run(1_200, None, |_, _| {}));
        assert!(lines.is_empty(), "{lines:#?}");
    }

    /// A reconnect hands the watch a fresh buffer whose counters restart at
    /// zero. That is a new stream and not an event: counters that ran up and
    /// then dropped must read as a restart, or the refusal window subtracts a
    /// spent count from an empty one.
    #[test]
    fn a_reconnected_stream_starts_its_own_episode() {
        let lines = captured(|| {
            let mut t = Ticker::new();
            t.run(1_200, Some(ME), stale_copies);
            t.jitter = JitterBuffer::new();
            t.run(1_200, Some(ME), healthy);
        });
        assert_eq!(
            lines.len(),
            1,
            "the healthy stream after it said nothing new"
        );
        assert!(lines[0].contains("being refused"), "{:?}", lines[0]);
    }

    /// One tick of playout against the counters the jitter buffer would have
    /// after it: `lossy` concealing a quarter of the frames, clean losing
    /// none. Returns what the watch would publish on that tick.
    fn play_a_tick(
        watch: &mut DownlinkLoss,
        stats: &mut JitterStats,
        tick: &mut u32,
        start: Instant,
        lossy: bool,
    ) -> Option<f32> {
        stats.pulled += 1;
        if lossy && stats.pulled % 4 == 0 {
            stats.lost += 1;
        }
        *tick += 1;
        watch.observe(start + TICK * *tick, true, *stats)
    }

    /// The reading has to come back down. A lifetime ratio only falls at the
    /// frame clock, so one bad moment early in a session keeps the figure high
    /// for as long as the session lasts: a Windows runner read 70 percent
    /// decaying through 22 over five seconds with a healthy link. A window is a
    /// rate over what happened inside it, so a bad second is gone from the
    /// reading a second after it ends.
    #[test]
    fn the_downlink_reads_clean_again_once_the_bad_stretch_is_over() {
        let start = Instant::now();
        let ticks = (DOWNLINK_LOSS_WINDOW.as_micros() / TICK.as_micros()) as u32 + 1;
        let mut watch = DownlinkLoss::default();
        let mut stats = JitterStats::default();
        let mut tick = 0u32;

        // Nothing is claimed before a window has closed.
        assert_eq!(watch.observe(start, true, stats), None);

        let mut bad = None;
        for _ in 0..ticks {
            bad = play_a_tick(&mut watch, &mut stats, &mut tick, start, true);
        }
        let bad = bad.expect("a window closed");
        assert!(
            (bad - 25.0).abs() < 1.0,
            "a quarter of a bad second concealed is 25%, not {bad}"
        );

        // A clean second, and the bad one is out of the reading.
        let mut clean = None;
        for _ in 0..ticks {
            clean = play_a_tick(&mut watch, &mut stats, &mut tick, start, false);
        }
        assert_eq!(
            clean,
            Some(0.0),
            "a clean window must read clean whatever came before it"
        );
        // And it stays down: a lifetime ratio would still be carrying the
        // bad second here, at the frame clock's pace.
        for _ in 0..ticks * 4 {
            assert_eq!(
                play_a_tick(&mut watch, &mut stats, &mut tick, start, false),
                Some(0.0)
            );
        }
        let lifetime = stats.lost as f32 * 100.0 / stats.pulled as f32;
        assert!(
            lifetime > 1.0,
            "the cumulative figure is the one that cannot come down: {lifetime}"
        );
    }

    /// Leaving ends the window. Nothing is owed to a client that is not in a
    /// session, so there is no rate for one either, and the next session
    /// measures its own rather than inheriting this one's.
    #[test]
    fn the_downlink_figure_goes_with_the_session() {
        let start = Instant::now();
        let ticks = (DOWNLINK_LOSS_WINDOW.as_micros() / TICK.as_micros()) as u32 + 1;
        let mut watch = DownlinkLoss::default();
        let mut stats = JitterStats::default();
        let mut tick = 0u32;
        for _ in 0..ticks {
            play_a_tick(&mut watch, &mut stats, &mut tick, start, true);
        }
        assert!(watch.observe(start + TICK * tick, true, stats).is_some());
        assert_eq!(watch.observe(start + TICK * tick, false, stats), None);

        // A fresh buffer whose counters start again is not a window either.
        let mut fresh = JitterStats::default();
        let mut readings = Vec::new();
        for _ in 0..ticks * 2 {
            readings.push(play_a_tick(&mut watch, &mut fresh, &mut tick, start, false));
        }
        assert_eq!(
            readings.last().copied().flatten(),
            Some(0.0),
            "the new session measures its own window: {readings:?}"
        );
    }

    /// A bridge whose capture ring is full, so every push overruns and every
    /// playback callback underruns: one event per call, on demand.
    fn full_ring() -> (jamstream_audio_io::DeviceSide, EngineSide) {
        let (mut device, engine) = CallbackBridge::new(4, 4);
        device.on_capture(&[1.0; 4]);
        (device, engine)
    }

    /// The shape a single total misses: drops that happen in a burst and
    /// then stop say so once. The count, the ring, and how long the
    /// stream had been up all ride the line, because those are what separate a
    /// burst at open from a drip.
    #[test]
    fn a_burst_of_dropped_capture_says_so_once() {
        let start = Instant::now();
        let lines = captured(|| {
            let (mut device, engine) = full_ring();
            let mut watch = RingWatch::new(start);
            for _ in 0..8 {
                device.on_capture(&[1.0; 4]);
            }
            // A minute of ticks after the burst, at the loop's own cadence.
            for tick in 0..24_000u32 {
                watch.observe(start + TICK * tick, &engine, 120);
            }
        });
        assert_eq!(lines.len(), 1, "{lines:#?}");
        let line = &lines[0];
        assert!(line.contains("WARN"), "{line}");
        assert!(line.contains("capture ring overflowed"), "{line}");
        for field in ["dropped=8", "overruns=8", "ring_frames=120", "up_ms=0"] {
            assert!(line.contains(field), "no {field} in {line}");
        }
    }

    /// The other shape, and the one that matters: a ring that keeps dropping
    /// keeps saying so, on a widening cadence, each line carrying what was lost
    /// since the last. One line per stream would have said 33 and then nothing
    /// for the rest of the song.
    #[test]
    fn capture_that_keeps_dropping_keeps_saying_so() {
        let start = Instant::now();
        let lines = captured(|| {
            let (mut device, engine) = full_ring();
            let mut watch = RingWatch::new(start);
            // Ten seconds, dropping one callback every 100 ms.
            for tick in 0..4_000u32 {
                if tick % 40 == 0 {
                    device.on_capture(&[1.0; 4]);
                }
                watch.observe(start + TICK * tick, &engine, 120);
            }
        });
        assert!(lines.len() >= 4, "{lines:#?}");
        for line in &lines {
            assert!(line.contains("capture ring overflowed"), "{line}");
        }
        // The first line is the first drop; each one after it waits twice as
        // long, so ten seconds of dropping costs four lines and not four
        // hundred.
        let up_ms: Vec<u64> = lines
            .iter()
            .map(|line| {
                let at = line.split("up_ms=").nth(1).expect("up_ms");
                at.split_whitespace()
                    .next()
                    .expect("a value")
                    .parse()
                    .expect("a number")
            })
            .collect();
        assert_eq!(up_ms[0], 0, "{up_ms:?}");
        for pair in up_ms[1..].windows(2) {
            let widened = (pair[1] - pair[0]) as f64 / (pair[0].max(1)) as f64;
            assert!(widened > 0.5, "the wait did not widen: {up_ms:?}");
        }
        // Every drop is accounted for across the lines, none counted twice.
        let dropped: u64 = lines
            .iter()
            .map(|line| {
                let at = line.split("dropped=").nth(1).expect("dropped");
                at.split_whitespace()
                    .next()
                    .expect("a value")
                    .parse::<u64>()
                    .expect("a number")
            })
            .sum();
        let last: u64 = lines
            .last()
            .and_then(|line| line.split("overruns=").nth(1))
            .and_then(|at| at.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .expect("a total on the last line");
        assert_eq!(dropped, last, "the deltas must add up to the total");
    }

    /// A ring nothing has gone wrong with says nothing at all, which is what
    /// lets an empty log file mean a healthy run.
    #[test]
    fn a_ring_with_room_says_nothing() {
        let start = Instant::now();
        let lines = captured(|| {
            let (mut device, mut engine) = CallbackBridge::new(64, 64);
            let mut watch = RingWatch::new(start);
            for tick in 0..4_000u32 {
                device.on_capture(&[1.0; 8]);
                let mut buf = [0.0f32; 64];
                engine.pull_captured(&mut buf);
                engine.push_playout(&[0.5; 8]);
                let mut out = [0.0f32; 8];
                device.on_playback(&mut out);
                watch.observe(start + TICK * tick, &engine, 120);
            }
        });
        assert!(lines.is_empty(), "{lines:#?}");
    }

    /// The two counters are reported apart: a stream that pads playout and
    /// never drops capture says so about playout only, in the sentence that
    /// names the device as the one padding.
    #[test]
    fn a_dry_playout_ring_says_that_instead() {
        let start = Instant::now();
        let lines = captured(|| {
            let (mut device, engine) = CallbackBridge::new(64, 64);
            let mut watch = RingWatch::new(start);
            let mut out = [0.0f32; 8];
            device.on_playback(&mut out);
            for tick in 0..400u32 {
                watch.observe(start + TICK * tick, &engine, 120);
            }
        });
        assert_eq!(lines.len(), 1, "{lines:#?}");
        assert!(lines[0].contains("padded silence"), "{:?}", lines[0]);
        assert!(lines[0].contains("padded=1"), "{:?}", lines[0]);
        assert!(
            !lines[0].contains("capture ring"),
            "a dry playout ring is not a dropped capture: {:?}",
            lines[0]
        );
    }

    /// One underrun anywhere in the window it opens with is what a stream
    /// open costs, and must never turn the state on however long the
    /// session runs afterward: the snapshot reads what to change, not a
    /// count, so a single event a guitarist would never have heard cannot
    /// earn it.
    #[test]
    fn a_single_underrun_never_turns_crackling_on() {
        let start = Instant::now();
        let (mut device, engine) = CallbackBridge::new(64, 64);
        let mut watch = RingWatch::new(start);
        // The window opens on the stream's first tick, before any underrun,
        // exactly as the worker's own ring health check does from the
        // moment the device opens.
        watch.observe(start, &engine, 120);
        let mut out = [0.0f32; 8];
        device.on_playback(&mut out);
        // Two windows and a half, so a stale attempt restarts at least once
        // without a second underrun ever arriving to complete it.
        let ticks = (CRACKLE_EPISODE_WINDOW.as_micros() * 5 / 2 / TICK.as_micros()) as u32;
        for tick in 1..=ticks {
            assert!(
                !watch.observe(start + TICK * tick, &engine, 120),
                "a single underrun must never turn crackling on"
            );
        }
    }

    /// Several underruns inside one window are the shape [`PlayoutWatch`]'s
    /// concealed-gap state already holds for a run rather than firing on a
    /// sample: the state turns on with the underrun that crosses
    /// [`CRACKLE_EPISODE_COUNT`], not later, and holds on for as long as
    /// the run keeps going. The floor sits above the one underrun a stream
    /// open costs and below the five a crackling run took in ninety seconds
    /// on real hardware.
    #[test]
    fn a_run_of_underruns_turns_crackling_on_and_holds_it() {
        let start = Instant::now();
        let (mut device, engine) = CallbackBridge::new(64, 64);
        let mut watch = RingWatch::new(start);
        watch.observe(start, &engine, 120);
        let mut tick = 1u32;
        let mut readings = Vec::new();
        for _ in 0..CRACKLE_EPISODE_COUNT {
            let mut out = [0.0f32; 8];
            device.on_playback(&mut out);
            readings.push(watch.observe(start + TICK * tick, &engine, 120));
            tick += 1;
            // A few seconds of quiet between each: still one stretch of
            // playing, and nowhere near spending the whole window.
            for _ in 0..1_000 {
                readings.push(watch.observe(start + TICK * tick, &engine, 120));
                tick += 1;
            }
        }
        let turned_on = readings
            .iter()
            .position(|&on| on)
            .expect("three underruns in one window must turn crackling on");
        assert_eq!(
            turned_on,
            readings.len() - 1_001,
            "must turn on with the third underrun, not later: {readings:?}"
        );
        assert!(
            readings[turned_on..].iter().all(|&on| on),
            "must hold on once turned on: {readings:?}"
        );
    }

    /// The window running out under the floor is not the same as the state
    /// having ever turned on. Once on, the same window without a fresh
    /// underrun turns it back off, the stretch of playing the ring kept up
    /// with; and a run that starts afterward turns the state on again
    /// rather than finding it already spent, exactly as a second dropout is
    /// its own episode in [`PlayoutWatch`].
    #[test]
    fn crackling_turns_off_after_a_quiet_stretch_and_a_new_run_turns_it_on_again() {
        let start = Instant::now();
        let (mut device, engine) = CallbackBridge::new(64, 64);
        let mut watch = RingWatch::new(start);
        watch.observe(start, &engine, 120);
        let mut tick = 1u32;
        for _ in 0..CRACKLE_EPISODE_COUNT {
            let mut out = [0.0f32; 8];
            device.on_playback(&mut out);
            watch.observe(start + TICK * tick, &engine, 120);
            tick += 1;
        }

        // Comfortably inside the window since the last fresh underrun.
        let half_window = (CRACKLE_EPISODE_WINDOW.as_micros() / 2 / TICK.as_micros()) as u32;
        tick += half_window;
        assert!(
            watch.observe(start + TICK * tick, &engine, 120),
            "must still be on well inside the window"
        );
        tick += 1;

        // Comfortably past a whole window with nothing fresh.
        let past_window = (CRACKLE_EPISODE_WINDOW.as_micros() / TICK.as_micros()) as u32 + 10;
        tick += past_window;
        assert!(
            !watch.observe(start + TICK * tick, &engine, 120),
            "a whole window with nothing fresh must turn crackling off"
        );
        tick += 1;

        // A second run turns it on again rather than finding it spent.
        for _ in 0..CRACKLE_EPISODE_COUNT {
            let mut out = [0.0f32; 8];
            device.on_playback(&mut out);
            watch.observe(start + TICK * tick, &engine, 120);
            tick += 1;
        }
        assert!(
            watch.observe(start + TICK * tick, &engine, 120),
            "a second run of underruns must turn crackling on again"
        );
    }

    /// A playout bridge at the client's own ring size, filled to its own depth
    /// target the way [`Driver::open`] fills it.
    fn playout_ring(frames: u32) -> (jamstream_audio_io::DeviceSide, EngineSide) {
        let (device, mut engine) =
            CallbackBridge::new(capture_capacity(frames), playout_capacity(frames));
        engine.push_playout(&vec![0.0; playout_target(frames)]);
        (device, engine)
    }

    /// The steady state: a producer holding the target reads as the whole
    /// cushion, which is the figure every dip is measured against. In frames,
    /// so the interleaved stereo cushion reads as half its samples.
    #[test]
    fn a_ring_that_is_never_late_reads_as_its_whole_cushion() {
        const FRAMES: u32 = 120;
        let start = Instant::now();
        let (mut device, mut engine) = playout_ring(FRAMES);
        let mut watch = RingWatch::new(start);
        let mut out = vec![0.0f32; FRAMES as usize * usize::from(CHANNELS)];

        // A callback per worker tick, topped straight back up, for a window.
        for tick in 0..=(PLAYOUT_LOW_WINDOW.as_micros() / TICK.as_micros()) as u32 {
            device.on_playback(&mut out);
            top_up(&mut engine, &out, FRAMES);
            watch.observe(start + TICK * tick, &engine, FRAMES);
        }

        assert_eq!(watch.playout_low_frames(), Some(2 * FRAMES as usize));
        assert_eq!(
            playout_target(FRAMES),
            2 * FRAMES as usize * usize::from(CHANNELS),
            "the same cushion in samples is twice the frames"
        );
    }

    /// The case underruns cannot see: the ring dipped to a fraction of its
    /// cushion and served every callback anyway. The reading is in frames, so a
    /// dip to 30 frames of stereo reads 30 and not the 60 samples it holds.
    #[test]
    fn a_dip_short_of_empty_reads_as_the_frames_that_were_left() {
        const FRAMES: u32 = 120;
        const LEFT: usize = 30;
        let start = Instant::now();
        let (mut device, engine) = playout_ring(FRAMES);
        let mut watch = RingWatch::new(start);

        // Drain to LEFT frames, then one callback that fits inside them.
        let mut out = vec![0.0f32; playout_target(FRAMES) - LEFT * usize::from(CHANNELS)];
        device.on_playback(&mut out);
        device.on_playback(&mut vec![0.0f32; LEFT * usize::from(CHANNELS)]);
        watch.observe(start + PLAYOUT_LOW_WINDOW, &engine, FRAMES);

        assert_eq!(watch.playout_low_frames(), Some(LEFT));
        assert_eq!(
            engine.underruns(),
            0,
            "the ring came within {LEFT} frames of empty without running out, which is \
             the whole point of the reading"
        );
    }

    /// Each window reports its own worst moment. A minimum since the stream
    /// opened would pin the figure to one bad second for the rest of the song,
    /// so a ring that recovers has to read as recovered.
    #[test]
    fn each_window_reports_its_own_worst_moment() {
        const FRAMES: u32 = 120;
        let start = Instant::now();
        let (mut device, mut engine) = playout_ring(FRAMES);
        let mut watch = RingWatch::new(start);
        let callback = FRAMES as usize * usize::from(CHANNELS);

        // A window that dips: two callbacks with no top-up between them.
        let mut out = vec![0.0f32; callback];
        device.on_playback(&mut out);
        device.on_playback(&mut out);
        top_up(&mut engine, &out, FRAMES);
        watch.observe(start + PLAYOUT_LOW_WINDOW, &engine, FRAMES);
        assert_eq!(watch.playout_low_frames(), Some(FRAMES as usize));

        // A window that keeps up.
        device.on_playback(&mut out);
        top_up(&mut engine, &out, FRAMES);
        watch.observe(start + PLAYOUT_LOW_WINDOW * 2, &engine, FRAMES);
        assert_eq!(watch.playout_low_frames(), Some(2 * FRAMES as usize));
    }

    /// A window with no render callback in it has no reading at all rather than
    /// the last one's: a device that has stopped rendering is not a ring that is
    /// keeping up, and a stale figure is what a controller would act on.
    #[test]
    fn a_window_without_a_callback_has_no_reading() {
        const FRAMES: u32 = 120;
        let start = Instant::now();
        let (mut device, engine) = playout_ring(FRAMES);
        let mut watch = RingWatch::new(start);

        assert_eq!(
            watch.playout_low_frames(),
            None,
            "the first window has not closed yet"
        );
        device.on_playback(&mut vec![0.0f32; FRAMES as usize * usize::from(CHANNELS)]);
        watch.observe(start + PLAYOUT_LOW_WINDOW, &engine, FRAMES);
        assert!(watch.playout_low_frames().is_some());

        watch.observe(start + PLAYOUT_LOW_WINDOW * 2, &engine, FRAMES);
        assert_eq!(watch.playout_low_frames(), None);
    }

    /// The split an automatic cushion rests on: the ring is cut once for the
    /// deepest cushion, and the depth the top-up loop holds is its own number.
    /// Held at three callbacks, which no buffer size the app offers would give
    /// at this device size, the ring holds three callbacks and a sample waits
    /// three callbacks to be heard. Both follow the target; neither follows the
    /// capacity, which is eight times deeper here.
    #[test]
    fn a_target_below_the_ring_is_the_depth_held_and_the_latency_paid() {
        const FRAMES: u32 = 120;
        let callback = FRAMES as usize * usize::from(CHANNELS);
        let capacity = playout_capacity(FRAMES);
        let target = 3 * callback;
        assert!(
            target > playout_target(FRAMES) && target < capacity,
            "a cushion no device size expresses, inside a ring that fits it"
        );

        let start = Instant::now();
        let (mut device, mut engine) = CallbackBridge::new(capture_capacity(FRAMES), capacity);
        let mut watch = RingWatch::new(start);
        let silence = vec![0.0f32; capacity];
        let marked = vec![1.0f32; capacity];
        fill_playout_to(&mut engine, &silence, target);
        assert_eq!(
            engine.playout_depth(),
            target,
            "the ring opens at the target, not at the capacity"
        );

        // The worker's loop against a device taking a callback per tick. The
        // marked audio goes in behind the first top-up, and every callback
        // after it is watched for the moment it comes out.
        let mut out = vec![0.0f32; callback];
        let mut heard = None;
        for tick in 0..=(PLAYOUT_LOW_WINDOW.as_micros() / TICK.as_micros()) as u32 {
            device.on_playback(&mut out);
            if heard.is_none() && out.contains(&1.0) {
                heard = Some(tick);
            }
            let source = if tick == 0 { &marked } else { &silence };
            while fill_playout_to(&mut engine, source, target) > 0 {}
            assert_eq!(
                engine.playout_depth(),
                target,
                "tick {tick} left the ring off its target"
            );
            watch.observe(start + TICK * tick, &engine, FRAMES);
        }

        let waited = heard.expect("the marked audio reached the device") as usize;
        assert_eq!(
            waited * callback,
            target,
            "a sample waited {waited} callbacks against a {target} sample cushion"
        );
        assert!(
            waited < capacity / callback,
            "{waited} callbacks is the whole {capacity} sample ring, not the cushion"
        );
        assert_eq!(
            watch.playout_low_frames(),
            Some(target / usize::from(CHANNELS)),
            "the water mark reads the cushion the loop holds"
        );
        assert_eq!(engine.underruns(), 0);
    }

    /// The device side of playout against the worker's top-up loop, on a clock
    /// the test drives: a real ring at the client's own sizes, a device draining
    /// its own callbacks on its own clock, and [`CushionControl`] deciding from
    /// the water mark the two of them leave behind. Holding the worker up runs
    /// the ring down for real, so nothing here stands in for the controller's
    /// own inputs.
    struct CushionRig {
        device: jamstream_audio_io::DeviceSide,
        engine: EngineSide,
        frames: u32,
        /// Interleaved samples one device callback asks for.
        callback: usize,
        /// Interleaved samples the device clock has earned and not yet asked
        /// for, so a callback longer than a tick fires every few ticks.
        earned: usize,
        out: Vec<f32>,
        silence: Vec<f32>,
        rings: RingWatch,
        cushion: CushionControl,
        at: Instant,
        /// The two states the worker reads off its snapshot to decide whether a
        /// step down is allowed, asked here through the same predicate the
        /// worker asks, so a test cannot hold the cushion still on a state the
        /// worker would not.
        record: RecordView,
        stream: Vec<DestinationView>,
    }

    impl CushionRig {
        fn new(frames: u32, start: Instant) -> CushionRig {
            let (device, mut engine) =
                CallbackBridge::new(capture_capacity(frames), playout_capacity(frames));
            engine.push_playout(&vec![0.0; playout_target(frames)]);
            CushionRig {
                device,
                engine,
                frames,
                callback: frames as usize * usize::from(CHANNELS),
                earned: 0,
                out: Vec::new(),
                silence: vec![0.0; playout_capacity(frames)],
                rings: RingWatch::new(start),
                cushion: CushionControl::new(frames),
                at: start,
                record: RecordView::default(),
                stream: Vec::new(),
            }
        }

        /// One worker tick: the device drains what its own clock earned, then the
        /// worker tops the ring back up and reads the mark it left, unless it is
        /// held up, in which case the device drains alone.
        fn tick(&mut self, held_up: bool) {
            self.at += TICK;
            self.earned += CHUNK_STEREO;
            while self.earned >= self.callback {
                self.earned -= self.callback;
                self.out.clear();
                self.out.resize(self.callback, 0.0);
                self.device.on_playback(&mut self.out);
            }
            if held_up {
                return;
            }
            while fill_playout_to(&mut self.engine, &self.silence, self.cushion.target()) > 0 {}
            self.rings.observe(self.at, &self.engine, self.frames);
            self.cushion.observe(
                self.rings.low_water_at(),
                self.rings.playout_low_frames(),
                recording_or_on_air(&self.record, &self.stream),
            );
        }

        /// `windows` water-mark windows of worker time, with the worker held up
        /// for `stall` ticks at the start of each one.
        fn run(&mut self, windows: usize, stall: usize) {
            let per_window = (PLAYOUT_LOW_WINDOW.as_micros() / TICK.as_micros()) as usize;
            for _ in 0..windows {
                for tick in 0..per_window {
                    self.tick(tick < stall);
                }
            }
        }

        /// Windows in a [`CUSHION_QUIET`] stretch, plus the window a move costs
        /// while the ring reaches its new depth and the window that opens the
        /// stretch.
        fn quiet_windows() -> usize {
            (CUSHION_QUIET.as_micros() / PLAYOUT_LOW_WINDOW.as_micros()) as usize + 2
        }
    }

    /// One destination somebody is watching. Its figures are not read: the
    /// state is the whole of what holds a broadcast.
    fn watched_destination() -> DestinationView {
        DestinationView {
            id: DestinationId(0),
            platform: StreamPlatform::Twitch,
            state: DestinationState::Live,
            bitrate_kbps: 0,
            dropped_frames: 0,
            repeated_frames: 0,
        }
    }

    /// The cushion a machine keeping up pays is the one it pays today. Two
    /// [`CUSHION_QUIET`] stretches of a worker that never misses a top-up, which
    /// is every window a controller would need to drift in either direction.
    #[test]
    fn a_cushion_the_machine_keeps_up_with_never_moves() {
        const FRAMES: u32 = 120;
        let mut rig = CushionRig::new(FRAMES, Instant::now());
        rig.run(2 * CushionRig::quiet_windows(), 0);

        assert_eq!(
            rig.cushion.target(),
            playout_target(FRAMES),
            "a ring nothing went wrong with paid latency for it"
        );
        assert_eq!(rig.engine.underruns(), 0);
    }

    /// The fault the cushion exists for: a worker held up long enough that the
    /// ring runs down under it. It grows until the stall costs nothing, and then
    /// it stops, which is the whole claim. Asserted as settling rather than as a
    /// count of steps, so the step size is free to move.
    ///
    /// Three lengths, because the shortest is one the base cushion already
    /// survives and only the water mark can see, and the others are ones it does
    /// not.
    #[test]
    fn a_cushion_grows_until_the_stall_costs_nothing_and_then_stops() {
        const FRAMES: u32 = 120;
        for stall in 1..=3 {
            let mut rig = CushionRig::new(FRAMES, Instant::now());
            rig.run(8, stall);
            let settled = rig.cushion.target();
            let padded = rig.engine.underruns();

            assert!(
                settled > playout_target(FRAMES),
                "a {stall}-tick stall left the cushion at its floor of {}",
                playout_target(FRAMES)
            );
            rig.run(8, stall);
            assert_eq!(
                rig.cushion.target(),
                settled,
                "a {stall}-tick stall is still moving the cushion at {settled} samples"
            );
            assert_eq!(
                rig.engine.underruns(),
                padded,
                "the ring is still padding silence at a cushion it settled on"
            );
        }
    }

    /// The other half: a machine that had a rough patch and then settled gets
    /// its latency back, a frame at a time and only behind a whole
    /// [`CUSHION_QUIET`] stretch. A cushion that returned to the floor in one
    /// move would give the whole of it back on the strength of one quiet window.
    #[test]
    fn a_machine_that_goes_quiet_gets_its_latency_back() {
        const FRAMES: u32 = 120;
        let floor = playout_target(FRAMES);
        let mut rig = CushionRig::new(FRAMES, Instant::now());
        rig.run(8, 2);
        let grown = rig.cushion.target();
        assert!(grown >= floor + CUSHION_STEP, "the stall grew nothing");

        rig.run(CushionRig::quiet_windows(), 0);
        assert_eq!(
            rig.cushion.target(),
            grown - CUSHION_STEP,
            "one quiet stretch is worth one frame, not the whole cushion"
        );
        while rig.cushion.target() > floor {
            let before = rig.cushion.target();
            rig.run(CushionRig::quiet_windows(), 0);
            assert!(
                rig.cushion.target() < before,
                "the cushion stopped coming down at {before} samples"
            );
        }
        rig.run(2 * CushionRig::quiet_windows(), 0);
        assert_eq!(
            rig.cushion.target(),
            floor,
            "the floor is the shallowest cushion, however quiet the machine gets"
        );
    }

    /// Growing is the direction a take needs most, because the alternative is
    /// the dry ring a musician hears. A held cushion deepens on the reading
    /// that saw the dip and settles where the same machine settles with
    /// nothing to hold it.
    #[test]
    fn a_take_does_not_stop_the_cushion_growing() {
        const FRAMES: u32 = 120;
        let mut held = CushionRig::new(FRAMES, Instant::now());
        held.record.state = RecordState::Recording;
        held.stream = vec![watched_destination()];
        held.run(8, 2);
        let mut free = CushionRig::new(FRAMES, Instant::now());
        free.run(8, 2);

        assert!(
            held.cushion.target() > playout_target(FRAMES),
            "a take stopped the cushion covering the stall it was there for"
        );
        assert_eq!(
            held.cushion.target(),
            free.cushion.target(),
            "a take moved the depth the same stall settles on"
        );
    }

    /// A cushion a stall grew, three quiet stretches with `hold` applied, and
    /// then the same quiet with nothing holding it: the frame comes back only
    /// once, and only a whole stretch after the hold lifts.
    ///
    /// That last part is the stretch starting again rather than carrying on. A
    /// stretch carried through would hand a frame back on the tick the hold
    /// lifts, on readings the controller had just declined to act on, at the
    /// moment the next take is likeliest to be starting.
    fn a_held_cushion_waits(what: &str, hold: impl Fn(&mut CushionRig)) {
        const FRAMES: u32 = 120;
        let mut rig = CushionRig::new(FRAMES, Instant::now());
        rig.run(8, 2);
        let grown = rig.cushion.target();
        assert!(
            grown >= playout_target(FRAMES) + CUSHION_STEP,
            "{what}: the stall grew nothing"
        );

        hold(&mut rig);
        rig.run(3 * CushionRig::quiet_windows(), 0);
        assert_eq!(
            rig.cushion.target(),
            grown,
            "the cushion handed a frame back while {what}"
        );

        rig.record = RecordView::default();
        rig.stream = Vec::new();
        rig.run(CushionRig::quiet_windows() / 2, 0);
        assert_eq!(
            rig.cushion.target(),
            grown,
            "the quiet while {what} paid for a frame the moment it stopped"
        );
        rig.run(CushionRig::quiet_windows(), 0);
        assert_eq!(
            rig.cushion.target(),
            grown - CUSHION_STEP,
            "the cushion stayed deep after {what} stopped"
        );
    }

    /// A take is audio nobody gets to play again, so the frame the quiet earned
    /// waits for the recorder to stop.
    #[test]
    fn a_cushion_holds_still_while_a_take_records() {
        a_held_cushion_waits("a take was recording", |rig| {
            rig.record.state = RecordState::Recording;
        });
    }

    /// A broadcast is the same claim about somebody watching it: a step down
    /// they hear cannot be taken again either.
    #[test]
    fn a_cushion_holds_still_while_a_destination_is_watched() {
        a_held_cushion_waits("a destination was watched", |rig| {
            rig.stream = vec![watched_destination()];
        });
    }

    /// The depth a machine still missing top-ups settled on is one it keeps. A
    /// cushion that hands a frame back on the strength of a quiet stretch this
    /// machine never has would take it again on the next reading, and a cushion
    /// oscillating around a depth is worse than one fixed at it: every step down
    /// is a chance to pad silence.
    #[test]
    fn a_cushion_that_settled_under_a_stall_never_hands_it_back() {
        const FRAMES: u32 = 120;
        let mut rig = CushionRig::new(FRAMES, Instant::now());
        rig.run(8, 1);
        let settled = rig.cushion.target();
        assert!(settled > playout_target(FRAMES), "the stall grew nothing");

        rig.run(3 * CushionRig::quiet_windows(), 1);
        assert_eq!(
            rig.cushion.target(),
            settled,
            "the cushion moved on a machine whose stall never went away"
        );
    }

    /// A machine no cushion this ring can hold would fix. The depth stops at the
    /// ceiling and says so once, because past there the answer is a device
    /// callback twice the size and that is a question for whoever is playing.
    #[test]
    fn a_cushion_at_its_ceiling_says_so_instead_of_growing() {
        const FRAMES: u32 = 120;
        let mut rig = CushionRig::new(FRAMES, Instant::now());
        let ceiling = rig.cushion.ceiling;
        let lines = captured(|| rig.run(8, 6));

        assert_eq!(
            rig.cushion.target(),
            ceiling,
            "the cushion grew past the ceiling the ring was cut for"
        );
        let asked: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("wants a bigger one"))
            .collect();
        assert_eq!(
            asked.len(),
            1,
            "one line per run at the ceiling: {lines:#?}"
        );
        assert!(asked[0].contains("WARN"), "{}", asked[0]);
        for field in ["cushion_ms=10", "callback_frames=120"] {
            assert!(asked[0].contains(field), "no {field} in {}", asked[0]);
        }
    }

    /// The bounds, at every buffer the Audio tab offers and at negotiated periods
    /// either side of them. The floor is what a machine keeping up pays today.
    /// The ceiling is one buffer size of latency above it and never deeper than
    /// the ring, so no depth this asks for could need the device reopened.
    #[test]
    fn the_cushion_bounds_cost_one_buffer_size_and_fit_the_ring() {
        for frames in [0u32, 32, 120, 240, 480, 960, 2_400] {
            let control = CushionControl::new(frames);
            assert_eq!(
                control.target(),
                playout_target(frames),
                "{frames}-frame callbacks: a stream opens on the base cushion"
            );
            assert_eq!(control.floor, playout_target(frames));
            assert!(
                control.ceiling <= playout_capacity(frames),
                "{frames}-frame callbacks: a ceiling of {} outgrows the {} sample ring",
                control.ceiling,
                playout_capacity(frames)
            );
            assert!(
                control.ceiling <= 2 * control.floor,
                "{frames}-frame callbacks: the ceiling costs more than the next buffer size up"
            );
        }
        for frames in crate::screens::devices::BUFFER_CHOICES {
            let control = CushionControl::new(frames);
            assert_eq!(
                control.ceiling,
                2 * control.floor,
                "{frames}-frame callbacks: the ring leaves the cushion nowhere to grow"
            );
        }
    }

    /// One water-mark window's reading, the pacing behind it, and the cushion as
    /// it stood when the window closed.
    #[derive(Debug)]
    struct RealWindow {
        low_frames: Option<usize>,
        pacing: Option<WakePacing>,
        target: usize,
    }

    /// The client's playout loop against a real device, on the real device's own
    /// clock, with the top-up held up for `stall` ticks at the start of every
    /// water-mark window. Returns one entry per window, and the counters the run
    /// ended on.
    ///
    /// A device clock of its own is the whole point: the only backend a test can
    /// drive pumps playout from this side, so it tops the ring up before every
    /// bite it plays and its water mark cannot dip at all.
    fn real_device_windows(
        frames: u32,
        windows: usize,
        stall: usize,
    ) -> (Vec<RealWindow>, CushionControl, u64) {
        let settings = crate::live::AudioSettings {
            buffer_frames: frames,
            ..crate::live::AudioSettings::default()
        };
        let config = crate::live::stream_config(&settings);
        let (device, mut engine) =
            CallbackBridge::new(capture_capacity(frames), playout_capacity(frames));
        engine.push_playout(&vec![0.0; playout_target(frames)]);
        let backend = jamstream_audio_io::backend();
        let stream = backend
            .open_duplex(None, None, config, device.into_handler())
            .expect("the default capture and playback devices open");
        let negotiated = stream.buffer_frames();
        let ring = crate::live::ring_frames(frames, negotiated);
        let priority = jamstream_audio_io::AudioPriority::raise_current_thread(TICK);
        println!(
            "{frames} frames asked, {negotiated:?} negotiated, ring {ring}, \
             cushion {} samples, priority {:?}",
            playout_target(ring),
            priority.granted()
        );

        let mut capture_buf = vec![0.0f32; capture_capacity(ring)];
        let silence = vec![0.0f32; playout_capacity(ring)];
        let start = Instant::now();
        let mut rings = RingWatch::new(start);
        let mut wake = WakeWatch::new(start);
        let mut cushion = CushionControl::new(ring);
        let mut out = Vec::new();
        let mut window = 1u32;
        let mut held = 0usize;
        let mut next = start + TICK;
        while out.len() < windows {
            let now = Instant::now();
            wake.observe(
                now,
                Some(cushion_time(cushion.target())),
                priority.granted(),
            );
            engine.pull_captured(&mut capture_buf);
            if held < stall {
                held += 1;
            } else {
                while fill_playout_to(&mut engine, &silence, cushion.target()) > 0 {}
                rings.observe(now, &engine, ring);
                // Nothing recorded or watched: this run measures the device.
                cushion.observe(rings.low_water_at(), rings.playout_low_frames(), false);
            }
            if now >= start + PLAYOUT_LOW_WINDOW * window {
                out.push(RealWindow {
                    low_frames: rings.playout_low_frames(),
                    pacing: wake.pacing(),
                    target: cushion.target(),
                });
                window += 1;
                held = 0;
            }
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            }
            next += TICK;
            if next < Instant::now() {
                next = Instant::now() + TICK;
            }
        }
        let underruns = engine.underruns();
        stream.close();
        for (i, w) in out.iter().enumerate() {
            println!("  window {i}: {w:?}");
        }
        println!(
            "underruns {underruns}, cushion {} samples",
            cushion.target()
        );
        (out, cushion, underruns)
    }

    /// The calibration the cushion's trigger rests on, which only a device
    /// running on its own clock can give: a machine keeping up leaves the whole
    /// cushion in the ring, every window, so the line [`CushionControl`] grows on
    /// sits a frame below anything a healthy machine produces and a healthy
    /// machine never pays for the controller. Measured at 240 frames in every one
    /// of twenty windows on the machine this was written on, with no underruns
    /// and a worst wakeup of 2.56 ms.
    #[test]
    #[ignore = "requires a real capture and playback device"]
    fn a_real_device_that_keeps_up_never_deepens_the_cushion() {
        const FRAMES: u32 = 120;
        let (windows, cushion, underruns) = real_device_windows(FRAMES, 20, 0);

        assert_eq!(
            cushion.target(),
            playout_target(FRAMES),
            "a machine keeping up paid latency for the cushion"
        );
        assert_eq!(underruns, 0, "the ring ran dry on a run nothing held up");
        for (i, w) in windows.iter().enumerate() {
            assert_eq!(
                w.low_frames,
                Some(playout_target(FRAMES) / usize::from(CHANNELS)),
                "window {i} came off the whole cushion: {windows:#?}"
            );
            assert_eq!(w.target, playout_target(FRAMES));
            assert!(
                w.pacing.is_some_and(|p| p.p99 <= cushion_time(w.target)),
                "window {i} woke later than the cushion it was holding: {windows:#?}"
            );
        }
    }

    /// The same loop, held up on purpose for two ticks a second, which is a
    /// machine whose worker misses 5 ms of top-ups: deeper than the base cushion
    /// holds. The cushion finds a depth that survives it and the ring stops
    /// padding, against a real device clock rather than a stepped one.
    #[test]
    #[ignore = "requires a real capture and playback device"]
    fn a_real_device_held_up_settles_on_a_deeper_cushion() {
        const FRAMES: u32 = 120;
        let (windows, cushion, _) = real_device_windows(FRAMES, 8, 2);

        assert!(
            cushion.target() > playout_target(FRAMES),
            "a worker missing two ticks a second left the cushion at its floor"
        );
        let settled = windows
            .last()
            .expect("eight windows")
            .low_frames
            .expect("a device that rendered");
        assert!(
            settled * usize::from(CHANNELS) >= cushion.callback + CUSHION_STEP,
            "the depth it settled on still runs the ring close to empty: {windows:#?}"
        );
        assert!(
            windows.iter().all(|w| w.target <= cushion.target()),
            "the cushion handed depth back while the stalls were still coming: {windows:#?}"
        );
    }

    /// The cutting-out watch as [`Worker`] holds it, driven by a counter the
    /// test moves and a clock it advances: the window is read rather than
    /// waited out, and the tick is the worker's own.
    struct Stops {
        watch: EpisodeWatch,
        start: Instant,
        tick: u32,
        total: u64,
    }

    impl Stops {
        /// Observing starts with the session, before any stop, exactly as
        /// [`Worker::publish_cutting_out`] does from the first tick.
        fn new() -> Stops {
            let start = Instant::now();
            let mut watch = EpisodeWatch::new(CUTTING_OUT_COUNT, CUTTING_OUT_WINDOW);
            watch.observe(start, 0);
            Stops {
                watch,
                start,
                tick: 1,
                total: 0,
            }
        }

        /// One stream that stopped on its own, and the reading it lands on.
        fn stop(&mut self) -> bool {
            self.total += 1;
            self.observe()
        }

        /// Ticks out `span` with the device up, asserting every tick in it
        /// against `expect`, so a state that flickered inside the stretch
        /// fails rather than reading right at the end of it.
        fn holding(&mut self, span: Duration, expect: bool) -> bool {
            for _ in 0..(span.as_micros() / TICK.as_micros()) {
                assert_eq!(
                    self.observe(),
                    expect,
                    "the state flickered inside the stretch"
                );
            }
            expect
        }

        /// The same stretch across a change, so only the reading it ends on is
        /// the test's business.
        fn settling(&mut self, span: Duration) -> bool {
            let mut last = false;
            for _ in 0..(span.as_micros() / TICK.as_micros()) {
                last = self.observe();
            }
            last
        }

        fn observe(&mut self) -> bool {
            let on = self
                .watch
                .observe(self.start + TICK * self.tick, self.total);
            self.tick += 1;
            on
        }
    }

    /// One stop is what a machine waking up or a device handed between apps
    /// costs, and it must never say anything however long the session runs
    /// afterward: the whole point of a state over a window is that a single
    /// blip stays quiet.
    #[test]
    fn one_device_stop_never_reads_as_cutting_out() {
        let mut stops = Stops::new();
        assert!(!stops.stop());
        // Two and a half windows, so a stale run restarts at least once
        // without a second stop ever arriving to complete it.
        stops.holding(CUTTING_OUT_WINDOW * 5 / 2, false);
    }

    /// Two stops five minutes apart are the machine that went to sleep twice,
    /// and no window ever holds both of them, so this is the case the floor
    /// and the window are chosen against.
    #[test]
    fn two_device_stops_five_minutes_apart_never_read_as_cutting_out() {
        let mut stops = Stops::new();
        assert!(!stops.stop());
        stops.holding(Duration::from_secs(300), false);
        assert!(!stops.stop());
        stops.holding(Duration::from_secs(300), false);
    }

    /// A device that keeps losing the stream, which is the shape no fault can
    /// carry: each of these stops is healed on the next tick, so nothing is
    /// on screen for any of them until the state turns on. It turns on with
    /// the stop that crosses the floor, not later, and holds through the
    /// playing that follows.
    #[test]
    fn a_run_of_device_stops_reads_as_cutting_out() {
        let mut stops = Stops::new();
        for _ in 1..CUTTING_OUT_COUNT {
            assert!(!stops.stop(), "under the floor must stay quiet");
            // Half a minute of playing between them: three of these fit
            // inside one window, and the device is up for all of it.
            stops.holding(Duration::from_secs(30), false);
        }
        assert!(
            stops.stop(),
            "the stop that crosses the floor must turn the state on"
        );
        assert!(
            stops.holding(CUTTING_OUT_WINDOW / 2, true),
            "and it must hold while somebody is playing through it"
        );
    }

    /// A whole window with the device holding is a device that recovered, so
    /// the state clears rather than latching for the rest of the session; and
    /// a run that starts afterward turns it on again rather than finding it
    /// spent.
    #[test]
    fn cutting_out_clears_after_a_quiet_stretch_and_a_new_run_says_so_again() {
        let mut stops = Stops::new();
        for _ in 0..CUTTING_OUT_COUNT {
            stops.stop();
        }
        // Still on well inside the window since the last stop, and off once a
        // whole one of them has passed with the device up.
        assert!(stops.holding(CUTTING_OUT_WINDOW / 2, true));
        assert!(!stops.settling(CUTTING_OUT_WINDOW));

        for _ in 1..CUTTING_OUT_COUNT {
            assert!(!stops.stop());
        }
        assert!(stops.stop(), "a second run must say so again");
    }

    /// A window's worth of wakeups, every `late_every`th of them arriving after
    /// `late` rather than after a tick. Driven on the clock the watch is handed,
    /// so a stall costs the test no wall-clock time.
    fn wake_for_a_window(
        watch: &mut WakeWatch,
        at: &mut Instant,
        late_every: u32,
        late: Duration,
        cushion: Option<Duration>,
    ) {
        let closes_at = *at + WAKE_WINDOW;
        let mut wakeup = 0u32;
        while *at <= closes_at {
            wakeup += 1;
            *at += if wakeup % late_every == 0 { late } else { TICK };
            watch.observe(*at, cushion, ThreadPriority::RealTime);
        }
    }

    /// The direction that matters more: a loop keeping its own pace says nothing
    /// at all, because the log file's first line promises that an empty file is a
    /// healthy run and this reading moves every second.
    #[test]
    fn a_loop_that_keeps_its_pace_says_nothing() {
        let start = Instant::now();
        let mut watch = WakeWatch::new(start);
        let mut at = start;
        let lines = captured(|| {
            // Never late: every wakeup lands one tick after the last.
            wake_for_a_window(
                &mut watch,
                &mut at,
                u32::MAX,
                TICK,
                Some(cushion_time(playout_target(120))),
            );
        });
        assert!(lines.is_empty(), "{lines:#?}");
        assert_eq!(
            watch.pacing(),
            Some(WakePacing {
                p99: TICK,
                max: TICK
            }),
            "a loop on the tick reads as one tick, not as two"
        );
    }

    /// The fault itself: wakeups late enough, often enough, that the ring cannot
    /// cover one of them. One line, and it carries both numbers, because the
    /// comparison between them is the whole reading; a millisecond figure on its
    /// own is one nobody can judge.
    #[test]
    fn wakeups_the_ring_cannot_cover_name_both_numbers() {
        const STALL: Duration = Duration::from_millis(20);
        let cushion = cushion_time(playout_target(120));
        let start = Instant::now();
        let mut watch = WakeWatch::new(start);
        let mut at = start;
        let lines = captured(|| {
            wake_for_a_window(&mut watch, &mut at, 20, STALL, Some(cushion));
        });

        assert_eq!(lines.len(), 1, "{lines:#?}");
        let line = &lines[0];
        assert!(
            line.contains("WARN"),
            "not a warning, so the file never sees it: {line}"
        );
        assert!(line.contains("wakes later than the ring holds"), "{line}");
        for field in ["p99_ms=20", "max_ms=20", "cushion_ms=5"] {
            assert!(line.contains(field), "no {field} in {line}");
        }
        let pacing = watch.pacing().expect("a closed window");
        assert!(
            pacing.p99 > cushion,
            "{:?} is inside the {cushion:?} the ring holds, so the warning was wrong",
            pacing.p99
        );
    }

    /// A machine that stays late says so once, and a machine that recovers can
    /// say it again. One line per second of a bad session would fill the file
    /// and one line per session would miss the second time.
    #[test]
    fn a_pace_that_recovers_can_say_it_again() {
        const STALL: Duration = Duration::from_millis(20);
        let cushion = Some(cushion_time(playout_target(120)));
        let start = Instant::now();
        let mut watch = WakeWatch::new(start);
        let mut at = start;
        let lines = captured(|| {
            wake_for_a_window(&mut watch, &mut at, 20, STALL, cushion);
            wake_for_a_window(&mut watch, &mut at, 20, STALL, cushion);
            wake_for_a_window(&mut watch, &mut at, u32::MAX, TICK, cushion);
            wake_for_a_window(&mut watch, &mut at, 20, STALL, cushion);
        });
        assert_eq!(
            lines.len(),
            2,
            "two late episodes with a recovery between them: {lines:#?}"
        );
    }

    /// The deadline is the depth the loop is filling to now, so a cushion that
    /// grew to cover a machine's wakeups stops the warning that asked for it. A
    /// deadline read off the device size would keep warning about pacing the ring
    /// already survives.
    #[test]
    fn the_pacing_deadline_follows_the_cushion_that_is_held() {
        const LATE: Duration = Duration::from_millis(6);
        let floor = playout_target(120);
        let start = Instant::now();

        let mut watch = WakeWatch::new(start);
        let mut at = start;
        let lines = captured(|| {
            wake_for_a_window(&mut watch, &mut at, 20, LATE, Some(cushion_time(floor)));
        });
        assert_eq!(lines.len(), 1, "the base cushion cannot cover {LATE:?}");

        let mut watch = WakeWatch::new(start);
        let mut at = start;
        let lines = captured(|| {
            wake_for_a_window(
                &mut watch,
                &mut at,
                20,
                LATE,
                Some(cushion_time(floor + CUSHION_STEP)),
            );
        });
        assert!(
            lines.is_empty(),
            "a cushion a frame deeper covers the same wakeups: {lines:#?}"
        );
    }

    /// Nothing draining the ring on a clock of its own means no deadline to
    /// miss, so the same stalls say nothing while the reading is taken all the
    /// same. The offline driver pumps playout from this thread, and a test suite
    /// that warned about its own scheduling would be noise.
    #[test]
    fn a_ring_nothing_drains_has_no_deadline_to_miss() {
        const STALL: Duration = Duration::from_millis(80);
        let start = Instant::now();
        let mut watch = WakeWatch::new(start);
        let mut at = start;
        let lines = captured(|| {
            wake_for_a_window(&mut watch, &mut at, 4, STALL, None);
        });
        assert!(lines.is_empty(), "{lines:#?}");
        assert!(
            watch.pacing().is_some_and(|p| p.max >= STALL),
            "the pacing still has to be measured: {:?}",
            watch.pacing()
        );
    }

    /// What the p99 is: the top of the bucket the 99th of a hundred fell in,
    /// which reads high by up to one tick. Two wakeups in a hundred at 6.1 ms
    /// put the true 99th percentile at 6.1 ms and this figure at 7.5, while the
    /// maximum beside it is the interval itself.
    #[test]
    fn the_p99_is_the_top_of_the_bucket_and_the_maximum_is_exact() {
        const LATE: Duration = Duration::from_micros(6_100);
        let start = Instant::now();
        let mut watch = WakeWatch::new(start);
        let mut at = start;
        assert_eq!(watch.pacing(), None, "no window has closed yet");

        wake_for_a_window(&mut watch, &mut at, 50, LATE, None);
        let pacing = watch.pacing().expect("a closed window");
        assert_eq!(
            pacing.p99,
            TICK * 3,
            "6.1 ms sits in the 5 to 7.5 ms bucket"
        );
        assert_eq!(pacing.max, LATE, "the maximum is the interval itself");
    }

    /// Each window is its own reading. A maximum kept for the session would pin
    /// the figure to one bad second for the rest of the song, and a machine that
    /// settles has to read as settled.
    #[test]
    fn each_window_reports_its_own_pacing() {
        const STALL: Duration = Duration::from_millis(40);
        let start = Instant::now();
        let mut watch = WakeWatch::new(start);
        let mut at = start;
        captured(|| {
            wake_for_a_window(&mut watch, &mut at, 8, STALL, None);
            assert!(watch.pacing().is_some_and(|p| p.max >= STALL));
            wake_for_a_window(&mut watch, &mut at, u32::MAX, TICK, None);
        });
        assert_eq!(
            watch.pacing(),
            Some(WakePacing {
                p99: TICK,
                max: TICK
            }),
            "the window that kept up has to read as having kept up"
        );
    }

    /// Measured mouth to ear across one region, which is a band that holds
    /// together, and across the country on DSL, which is one that does not.
    /// Both carry the playout cushion, as the figure they are readings of does.
    const REGION_MS: f32 = 24.3;
    const CROSS_COUNTRY_MS: f32 = 69.8;
    /// Ticks in the window the offer waits out.
    const OFFER_TICKS: u32 = (HEAR_SELF_OFFER_WINDOW.as_micros() / TICK.as_micros()) as u32;

    /// One reading per tick, band playing, nobody hearing themselves yet.
    fn offer_readings(figures: impl Iterator<Item = f32>) -> Vec<bool> {
        let start = Instant::now();
        let mut offer = HearSelfOffer::default();
        figures
            .enumerate()
            .map(|(tick, ms)| offer.observe(start + TICK * tick as u32, Some(ms), false, true))
            .collect()
    }

    /// A band inside the range an ensemble holds together in is offered
    /// nothing, however long it plays: the offer is worth having only where
    /// the arrangement it names is worth the headphones it needs.
    #[test]
    fn a_session_that_holds_together_is_never_offered_anything() {
        let readings = offer_readings(std::iter::repeat_n(REGION_MS, OFFER_TICKS as usize * 3));
        assert!(
            readings.iter().all(|&on| !on),
            "a session reading {REGION_MS} ms must never be offered the other arrangement"
        );
    }

    /// The reason this is an episode and not a threshold: the figure carries
    /// the jitter buffer's depth, which grows on one bad moment and comes
    /// back down, and a suggestion that arrives on that is noise. A reading
    /// under the threshold ends the run, so the window has to be spent inside
    /// one stretch of being far apart.
    #[test]
    fn a_spike_over_the_threshold_offers_nothing() {
        // Most of the window over, then one reading under it, then the rest of
        // the window over again: two runs, neither of them whole.
        let spike = OFFER_TICKS - 10;
        let readings = offer_readings(
            std::iter::repeat_n(CROSS_COUNTRY_MS, spike as usize)
                .chain(std::iter::once(REGION_MS))
                .chain(std::iter::repeat_n(CROSS_COUNTRY_MS, spike as usize)),
        );
        assert!(
            readings.iter().all(|&on| !on),
            "two part windows are not one window"
        );
    }

    /// A band far enough apart for the whole window is offered the other
    /// arrangement, on the tick the window closes and not before, and the
    /// offer then stands: it goes out exactly once, because somebody with an
    /// instrument in their hands reads the screen when they read it.
    #[test]
    fn a_session_far_enough_apart_is_offered_it_once_and_the_offer_stands() {
        let readings = offer_readings(std::iter::repeat_n(
            CROSS_COUNTRY_MS,
            OFFER_TICKS as usize * 2,
        ));
        let went_out = readings
            .iter()
            .position(|&on| on)
            .expect("a whole window over the threshold must put the offer out");
        assert_eq!(
            went_out, OFFER_TICKS as usize,
            "the offer waits out the window and no longer"
        );
        assert!(
            readings[went_out..].iter().all(|&on| on),
            "the offer stands once it is out"
        );
        assert_eq!(
            readings.windows(2).filter(|w| w[0] != w[1]).count(),
            1,
            "the offer goes out once, so the state changes once"
        );
    }

    /// Somebody already hearing themselves is not offered it, and neither is
    /// anybody who has touched the control: the offer would then be arriving
    /// at a decision that has been made, and it names headphones because the
    /// wrong answer is a loop into the microphone.
    #[test]
    fn a_session_already_hearing_itself_is_offered_nothing() {
        let start = Instant::now();
        let mut offer = HearSelfOffer::default();
        for tick in 0..OFFER_TICKS * 2 {
            assert!(
                !offer.observe(start + TICK * tick, Some(CROSS_COUNTRY_MS), true, true),
                "a session already hearing itself must stay quiet"
            );
        }

        // And the offer that is already out goes away when it is acted on,
        // rather than standing over a control that now reads the other way.
        let mut offer = HearSelfOffer::default();
        for tick in 0..=OFFER_TICKS {
            offer.observe(start + TICK * tick, Some(CROSS_COUNTRY_MS), false, true);
        }
        assert!(
            !offer.observe(
                start + TICK * (OFFER_TICKS + 1),
                Some(CROSS_COUNTRY_MS),
                true,
                true
            ),
            "the offer must go once the control has been used"
        );
        assert!(
            !offer.observe(
                start + TICK * (OFFER_TICKS + 2),
                Some(CROSS_COUNTRY_MS),
                false,
                true
            ),
            "and must not come back when it is turned off again"
        );
    }

    /// Alone in a session there is nobody to be out of time with, so the
    /// figure alone is not the condition: a soundcheck on a far server earns
    /// nothing, and a figure nothing has measured yet earns nothing either.
    #[test]
    fn a_musician_with_nobody_to_play_with_is_offered_nothing() {
        let start = Instant::now();
        let mut offer = HearSelfOffer::default();
        for tick in 0..OFFER_TICKS * 2 {
            assert!(
                !offer.observe(start + TICK * tick, Some(CROSS_COUNTRY_MS), false, false),
                "nobody else is playing, so there is nothing to keep together with"
            );
        }
        let mut offer = HearSelfOffer::default();
        for tick in 0..OFFER_TICKS * 2 {
            assert!(
                !offer.observe(start + TICK * tick, None, false, true),
                "no round trip has been measured, which is no evidence"
            );
        }
    }
}
