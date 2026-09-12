//! Bounded PCM period ring and clock-drift correction.
//!
//! Sustained high occupancy drops a period; sustained low occupancy inserts silence.
//! Push and pop track their own streaks so a correction cannot be consumed on the wrong
//! side. Underruns and overruns reset drift history and are counted separately.

use std::collections::VecDeque;
use std::time::Duration;

/// The sample rate the dongle's audio interface offers, and the only one it offers: measured
/// 2026-09-11 from `/proc/asound/card8/stream0`, `Rates: 48000`.
pub const SAMPLE_RATE: u32 = 48_000;
/// Channels, likewise measured and likewise the only value: `Channels: 2`, `Channel map: FL FR`.
pub const CHANNELS: usize = 2;

/// How the ring is shaped, in whole periods.
///
/// A *period* is the unit everything here moves in: the ALSA capture side hands over one period
/// at a time and the playback side takes one at a time, so a partial period never exists and
/// nothing ever has to reason about a half-written buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingConfig {
    /// How many periods the ring holds before an overrun.
    pub periods: usize,
    /// Frames per period. One frame is [`CHANNELS`] samples.
    pub period_frames: usize,
}

impl Default for RingConfig {
    /// Default ring: eight 480-frame periods, or 80 ms capacity at 48 kHz.
    ///
    /// Ten-millisecond periods bound wake-up overhead. The depth provides scheduling
    /// headroom: two recorded 720-second runs had no xruns and about 14 ppm drift.
    /// That drift alone takes roughly twelve minutes to move occupancy by one period.
    /// See `docs/hardware.md` for measurement conditions.
    fn default() -> Self {
        RingConfig {
            periods: 8,
            period_frames: 480,
        }
    }
}

impl RingConfig {
    /// Samples in one period: frames × channels. This is the length of every buffer the ring
    /// holds, and the ring rejects anything else.
    pub fn period_samples(&self) -> usize {
        self.period_frames * CHANNELS
    }

    /// Configured capacity at [`SAMPLE_RATE`]. Excludes device buffers, USB transfer,
    /// and playback processing; this is not measured end-to-end latency.
    pub fn depth(&self) -> Duration {
        Duration::from_nanos(
            (self.periods as u64 * self.period_frames as u64 * 1_000_000_000) / SAMPLE_RATE as u64,
        )
    }

    /// A silent period, the right length for this ring.
    pub fn silence(&self) -> Vec<i16> {
        vec![0; self.period_samples()]
    }
}

/// What the drift policy wants done about the current fill level.
///
/// A *request*, not an instruction: [`PeriodRing`] carries it out only while the ring is still
/// off the wall on that side (see [`PeriodRing::push`] and [`PeriodRing::pop`]). A correction is
/// the thing that happens *instead of* an xrun, so a ring that has already reached the wall has
/// nothing left to correct — it has an overrun or an underrun, and those are counted as what they
/// are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriftAction {
    /// The level is healthy, or the streak is not long enough yet.
    Hold,
    /// The ring has been sitting near full: the producer's clock is faster. Drop one period now —
    /// unless the ring is already full, where the push is an overrun instead.
    DropOne,
    /// The ring has been sitting near empty: the consumer's clock is faster. Insert one period of
    /// silence now — unless the ring is already empty, where the pop is an underrun instead.
    InsertOne,
}

/// When to spend one period to keep the ring off its walls.
///
/// Pure, and trivial to reason about: it sees one number — the fill level — and
/// returns one of three answers. It has no clock, so a test is a list of levels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DriftPolicy {
    /// Level at or above which the ring counts as "near full".
    pub high_water: usize,
    /// Level at or below which it counts as "near empty".
    pub low_water: usize,
    /// How many consecutive observations on one side are needed before acting. The guard against
    /// correcting for scheduling jitter rather than for drift.
    pub hold: u32,
    high_streak: u32,
    low_streak: u32,
}

impl DriftPolicy {
    /// The policy for a ring of this shape.
    ///
    /// The marks are one period in from each wall, so a correction happens while there is still a
    /// period of margin on that side rather than at the moment it runs out. `hold` is four
    /// observations per period of depth — at the default 8 × 10 ms that is 32 observations, about
    /// a third of a second of the level *staying* on one side, which no amount of ordinary
    /// jitter produces and which any real clock difference reaches and holds.
    pub fn new(cfg: &RingConfig) -> Self {
        DriftPolicy {
            high_water: cfg.periods.saturating_sub(1).max(1),
            low_water: 1,
            hold: (cfg.periods as u32 * 4).max(1),
            high_streak: 0,
            low_streak: 0,
        }
    }

    /// A policy with the marks stated outright.
    ///
    /// The streak counters are not public, so this is how a caller — a test, or a future
    /// configuration — builds a policy other than [`DriftPolicy::new`]'s. Both marks are clamped
    /// into a sane order so a transposed pair cannot make the ring correct in both directions at
    /// once.
    pub fn with_marks(high_water: usize, low_water: usize, hold: u32) -> Self {
        DriftPolicy {
            high_water: high_water.max(low_water),
            low_water: low_water.min(high_water),
            hold: hold.max(1),
            high_streak: 0,
            low_streak: 0,
        }
    }

    /// What to do before queueing a period, given the level now. Only ever [`DriftAction::Hold`]
    /// or [`DriftAction::DropOne`].
    ///
    /// One observation per push, so `hold` counts *pushes* the level stayed high for. A single
    /// observation below the mark clears the streak: the correction is for a level that stays on
    /// one side, and one healthy push is proof that it did not. Firing resets it, so two
    /// corrections are always `hold` pushes apart.
    pub fn on_push(&mut self, level: usize) -> DriftAction {
        if level < self.high_water {
            self.high_streak = 0;
            return DriftAction::Hold;
        }
        self.high_streak += 1;
        if self.high_streak >= self.hold {
            self.high_streak = 0;
            return DriftAction::DropOne;
        }
        DriftAction::Hold
    }

    /// The same on the way out, and only ever [`DriftAction::Hold`] or
    /// [`DriftAction::InsertOne`].
    ///
    /// The two sides count separately rather than sharing one observation. A single counter
    /// fed from both push and pop can consume a low-side streak inside a push, which is where the
    /// correction it earned is then thrown away — the level reaches the mark, the streak
    /// completes on the wrong call, and nothing happens. Two counters, one per direction, cannot
    /// do that.
    pub fn on_pop(&mut self, level: usize) -> DriftAction {
        if level > self.low_water {
            self.low_streak = 0;
            return DriftAction::Hold;
        }
        self.low_streak += 1;
        if self.low_streak >= self.hold {
            self.low_streak = 0;
            return DriftAction::InsertOne;
        }
        DriftAction::Hold
    }

    /// Forget both streaks.
    ///
    /// Called by [`PeriodRing`] on every overrun and every underrun, because an xrun means the
    /// level was at a wall and not drifting towards one: one side has stopped. A streak counted
    /// through a stall is a measurement of the stall, and acting on it would drop or insert a
    /// period for a clock difference that was never observed — which is what makes
    /// `drift_drops` readable as a drift rate afterwards.
    pub fn reset(&mut self) {
        self.high_streak = 0;
        self.low_streak = 0;
    }
}

/// Ring transfer, xrun, and clock-drift counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RingCounts {
    /// Periods handed to [`PeriodRing::push`].
    pub pushed: u64,
    /// Periods [`PeriodRing::pop`] returned from the queue — silence does not count.
    pub popped: u64,
    /// Periods dropped because the ring was full when one arrived.
    pub overruns: u64,
    /// Silent periods returned because the ring was empty when one was wanted.
    pub underruns: u64,
    /// Periods dropped by [`DriftPolicy`] before the ring filled.
    pub drift_drops: u64,
    /// Silent periods inserted by [`DriftPolicy`] before the ring emptied.
    pub drift_inserts: u64,
}

impl RingCounts {
    /// Whether anything at all has been dropped, inserted or replaced by silence. The title shows
    /// the counters only when this is true, so a healthy session says nothing about them.
    pub fn any(&self) -> bool {
        self.overruns != 0
            || self.underruns != 0
            || self.drift_drops != 0
            || self.drift_inserts != 0
    }
}

/// What one [`PeriodRing::push`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushOutcome {
    /// Queued with nothing displaced.
    Queued,
    /// The oldest period was dropped by the drift policy first, from a ring that was neither
    /// empty nor at its wall.
    DriftDropped,
    /// The ring was full, so the oldest period was dropped to make room.
    Overran,
}

/// What one [`PeriodRing::pop`] returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopOutcome {
    /// A real captured period.
    Period,
    /// Silence, because the ring was empty.
    Underran,
    /// Silence, inserted by the drift policy while the ring still had periods in it — never from
    /// an empty ring, which is an underrun and is counted as one.
    DriftInserted,
}

/// Bounded FIFO of whole periods with drift correction on both push and pop.
#[derive(Debug)]
pub struct PeriodRing {
    config: RingConfig,
    drift: DriftPolicy,
    queue: VecDeque<Vec<i16>>,
    counts: RingCounts,
}

impl PeriodRing {
    pub fn new(config: RingConfig) -> Self {
        PeriodRing {
            drift: DriftPolicy::new(&config),
            queue: VecDeque::with_capacity(config.periods),
            counts: RingCounts::default(),
            config,
        }
    }

    /// A ring with a drift policy other than the default one. For tests that need a correction to
    /// fire inside a few observations instead of a few hundred.
    pub fn with_drift(config: RingConfig, drift: DriftPolicy) -> Self {
        PeriodRing {
            drift,
            ..PeriodRing::new(config)
        }
    }

    pub fn config(&self) -> RingConfig {
        self.config
    }

    pub fn counts(&self) -> RingCounts {
        self.counts
    }

    /// Periods currently held.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.queue.len() >= self.config.periods
    }

    /// Queue a period, dropping the oldest when full to keep playback current.
    ///
    /// Drift drops apply only while the ring is neither empty nor full. A full ring
    /// is counted as overrun and resets drift streaks.
    pub fn push(&mut self, period: Vec<i16>) -> PushOutcome {
        assert_eq!(
            period.len(),
            self.config.period_samples(),
            "the ring holds whole periods only"
        );
        let mut outcome = PushOutcome::Queued;
        let wanted = self.drift.on_push(self.queue.len());
        if wanted == DriftAction::DropOne && !self.queue.is_empty() && !self.is_full() {
            self.queue.pop_front();
            self.counts.drift_drops += 1;
            outcome = PushOutcome::DriftDropped;
        }
        if self.is_full() {
            self.queue.pop_front();
            self.counts.overruns += 1;
            self.drift.reset();
            outcome = PushOutcome::Overran;
        }
        self.queue.push_back(period);
        self.counts.pushed += 1;
        outcome
    }

    /// Take the oldest period or return silence when empty.
    ///
    /// An empty ring counts as underrun and resets drift streaks. Drift inserts require
    /// a nonempty ring so a capture outage is not classified as clock drift.
    pub fn pop(&mut self) -> (Vec<i16>, PopOutcome) {
        let wanted = self.drift.on_pop(self.queue.len());
        if wanted == DriftAction::InsertOne && !self.queue.is_empty() {
            self.counts.drift_inserts += 1;
            return (self.config.silence(), PopOutcome::DriftInserted);
        }
        match self.queue.pop_front() {
            Some(period) => {
                self.counts.popped += 1;
                (period, PopOutcome::Period)
            }
            None => {
                self.counts.underruns += 1;
                self.drift.reset();
                (self.config.silence(), PopOutcome::Underran)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(periods: usize) -> RingConfig {
        RingConfig {
            periods,
            period_frames: 2,
        }
    }

    /// A drift policy that never fires, so a test of the ring's own two policies is not also a
    /// test of the third.
    fn no_drift() -> DriftPolicy {
        DriftPolicy::with_marks(usize::MAX, 0, u32::MAX)
    }

    fn ring(periods: usize) -> PeriodRing {
        PeriodRing::with_drift(cfg(periods), no_drift())
    }

    /// Periods are distinguishable, so FIFO order is assertable.
    fn period(n: i16) -> Vec<i16> {
        vec![n; cfg(1).period_samples()]
    }

    #[test]
    fn a_full_ring_drops_the_oldest_period_and_counts_it() {
        let mut r = ring(2);
        r.push(period(1));
        r.push(period(2));
        assert!(r.is_full());
        assert_eq!(r.push(period(3)), PushOutcome::Overran);

        assert_eq!(r.counts().overruns, 1);
        assert_eq!(r.len(), 2, "the bound is never exceeded");
        // 1 is gone; 2 and 3 remain, in order.
        assert_eq!(r.pop().0, period(2));
        assert_eq!(r.pop().0, period(3));
    }

    #[test]
    fn an_empty_ring_hands_over_silence_rather_than_nothing() {
        let mut r = ring(4);
        let (samples, outcome) = r.pop();
        assert_eq!(outcome, PopOutcome::Underran);
        assert_eq!(samples, vec![0; cfg(1).period_samples()]);
        assert_eq!(r.counts().underruns, 1);
        assert_eq!(r.counts().popped, 0, "silence is not a period");
    }

    #[test]
    fn a_period_of_the_wrong_length_is_a_caller_bug() {
        let mut r = ring(2);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            r.push(vec![0; 3]);
        }))
        .is_err());
    }

    #[test]
    fn the_drift_policy_holds_until_the_level_stays_on_one_side() {
        let mut p = DriftPolicy::with_marks(6, 1, 3);
        // Two high observations then a healthy one: the streak is broken, nothing fires.
        assert_eq!(p.on_push(7), DriftAction::Hold);
        assert_eq!(p.on_push(7), DriftAction::Hold);
        assert_eq!(p.on_push(4), DriftAction::Hold);
        assert_eq!(p.on_push(7), DriftAction::Hold);
        assert_eq!(p.on_push(7), DriftAction::Hold);
        assert_eq!(p.on_push(7), DriftAction::DropOne);
        // And firing resets it, so the next correction is another three pushes away.
        assert_eq!(p.on_push(7), DriftAction::Hold);
    }

    /// The two sides are independent counters: pops on a low ring do not disturb a high streak
    /// that pushes are building, and a `DropOne` never comes out of `on_pop`.
    #[test]
    fn the_two_drift_directions_count_separately() {
        let mut p = DriftPolicy::with_marks(6, 1, 2);
        assert_eq!(p.on_push(7), DriftAction::Hold);
        assert_eq!(p.on_pop(0), DriftAction::Hold);
        assert_eq!(p.on_pop(0), DriftAction::InsertOne);
        // The high streak survived both pops.
        assert_eq!(p.on_push(7), DriftAction::DropOne);
    }

    #[test]
    fn a_ring_that_stays_empty_inserts_silence_before_it_underruns() {
        let drift = DriftPolicy::with_marks(3, 1, 2);
        let mut r = PeriodRing::with_drift(cfg(4), drift);
        r.push(period(1));
        // Level 1 is at the low-water mark twice over.
        assert_eq!(r.pop().1, PopOutcome::Period);
        r.push(period(2));
        let (samples, outcome) = r.pop();
        assert_eq!(outcome, PopOutcome::DriftInserted);
        assert_eq!(samples, vec![0; cfg(1).period_samples()]);
        assert_eq!(r.counts().drift_inserts, 1);
        assert_eq!(r.counts().underruns, 0, "the ring was never actually empty");
        // The real period is still there, so nothing was lost to the correction.
        assert_eq!(r.pop().0, period(2));
    }

    #[test]
    fn a_ring_that_stays_full_drops_one_period_before_it_overruns() {
        let drift = DriftPolicy::with_marks(2, 0, 2);
        let mut r = PeriodRing::with_drift(cfg(4), drift);
        r.push(period(1));
        r.push(period(2)); // level 1 then 2 at push time... the first observation is level 0.
        r.push(period(3)); // observes 2 -> streak 1
        assert_eq!(r.push(period(4)), PushOutcome::DriftDropped); // observes 3 -> streak 2
        assert_eq!(r.counts().drift_drops, 1);
        assert_eq!(r.counts().overruns, 0, "the ring never reached its wall");
    }

    /// A stopped consumer produces overruns, not clock-drift corrections.
    #[test]
    fn a_ring_nobody_drains_overruns_rather_than_reporting_drift() {
        let drift = DriftPolicy::with_marks(3, 1, 2);
        let mut r = PeriodRing::with_drift(cfg(4), drift);
        for n in 0..50 {
            r.push(period(n as i16));
        }
        assert_eq!(r.counts().drift_drops, 0, "a stalled sink is not drift");
        assert!(r.counts().overruns > 0, "it is an overrun, and says so");
        assert_eq!(r.len(), 4);
    }

    /// The same on the other side: a capture side that has stopped leaves the ring empty, and
    /// every pop is an underrun rather than an inserted period of "drift" silence.
    #[test]
    fn an_empty_ring_underruns_rather_than_inserting_drift_silence() {
        let drift = DriftPolicy::with_marks(3, 1, 2);
        let mut r = PeriodRing::with_drift(cfg(4), drift);
        for _ in 0..50 {
            assert_eq!(r.pop().1, PopOutcome::Underran);
        }
        assert_eq!(r.counts().drift_inserts, 0, "a stalled card is not drift");
        assert_eq!(r.counts().underruns, 50);
    }

    /// An xrun clears the streaks, so a stall cannot leave a correction armed for the moment the
    /// other side comes back — which would spend a period of real audio on drift that was never
    /// observed.
    #[test]
    fn an_xrun_clears_the_streak_a_stall_was_building() {
        let mut r = PeriodRing::with_drift(cfg(2), DriftPolicy::with_marks(1, 0, 3));
        // Two pushes fill the ring; the third overruns, and the observations at level 1 that
        // preceded it are forgotten.
        r.push(period(1));
        r.push(period(2));
        assert_eq!(r.push(period(3)), PushOutcome::Overran);
        assert_eq!(r.counts().overruns, 1);
        // One period out, so the ring is off its wall and doing its job again — but still at the
        // high-water mark. Were the streak the stall built still standing, this push would fire a
        // correction and spend a period of real audio on it.
        assert_eq!(r.pop().1, PopOutcome::Period);
        assert_eq!(r.push(period(4)), PushOutcome::Queued);
        assert_eq!(r.counts().drift_drops, 0);
    }

    #[test]
    fn the_configured_depth_is_periods_times_period_size() {
        let c = RingConfig {
            periods: 8,
            period_frames: 480,
        };
        assert_eq!(c.period_samples(), 960);
        assert_eq!(c.depth(), Duration::from_millis(80));
    }
}
