//! Property tests for bounded audio buffering and independent drift corrections.

use proptest::prelude::*;

use nanokvm::audio::{DriftAction, DriftPolicy, PeriodRing, PopOutcome, PushOutcome, RingConfig};

/// Whether an operation is a push or a pop. A session is a sequence of these.
#[derive(Clone, Copy, Debug)]
enum Op {
    Push,
    Pop,
}

fn ops() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(prop_oneof![Just(Op::Push), Just(Op::Pop)], 0..400usize)
}

fn configs() -> impl Strategy<Value = RingConfig> {
    (1..12usize, 1..8usize).prop_map(|(periods, period_frames)| RingConfig {
        periods,
        period_frames,
    })
}

proptest! {
    /// The bound is a bound. Nothing a caller can do makes the ring hold more periods than it was
    /// configured for — which is what stops a stalled playback sink turning into unbounded memory.
    #[test]
    fn the_ring_never_holds_more_than_its_configured_periods(config in configs(), ops in ops()) {
        let mut ring = PeriodRing::new(config);
        for op in ops {
            match op {
                Op::Push => { ring.push(vec![1; config.period_samples()]); }
                Op::Pop => { ring.pop(); }
            }
            prop_assert!(ring.len()<= config.periods);
        }
    }

    /// The accounting identity: every period that went in is accounted for once — still
    /// held, played out, or counted as dropped by one of the two drop paths. A period that
    /// vanished from the counters is a click the user hears and nothing reports.
    #[test]
    fn every_period_pushed_is_held_played_or_counted_as_dropped(
        config in configs(),
        ops in ops(),
    ) {
        let mut ring = PeriodRing::new(config);
        for op in ops {
            match op {
                Op::Push => { ring.push(vec![1; config.period_samples()]); }
                Op::Pop => { ring.pop(); }
            }
        }
        let c = ring.counts();
        prop_assert_eq!(
            c.pushed,
            c.popped + c.overruns + c.drift_drops + ring.len() as u64,
            "counts {:?} with {} still held", c, ring.len());
    }

    /// Every period that leaves the ring is one period long, silence included. The
    /// playback side writes whatever it is handed straight to a device that was configured for
    /// one period at a time, so a short buffer would desynchronise the stream for the rest of the
    /// session.
    #[test]
    fn everything_that_comes_out_is_exactly_one_period(config in configs(), ops in ops()) {
        let mut ring = PeriodRing::new(config);
        for op in ops {
            match op {
                Op::Push => { ring.push(vec![1; config.period_samples()]); }
                Op::Pop => {
                    let (period, outcome) = ring.pop();
                    prop_assert_eq!(period.len(), config.period_samples());
                    // Silence is silence: nothing may pass stale samples off as an underrun.
                    if outcome != PopOutcome::Period {
                        prop_assert!(period.iter().all(|s| *s == 0));
                    }
                }
            }
        }
    }

    /// Order is preserved for everything that survives. A ring that reordered periods would
    /// scramble the audio in a way no counter would show.
    #[test]
    fn the_periods_that_survive_come_out_in_the_order_they_went_in(
        config in configs(),
        ops in ops(),
    ) {
        let mut ring = PeriodRing::new(config);
        let mut next = 0i16;
        let mut seen: Vec<i16> = Vec::new();
        for op in ops {
            match op {
                Op::Push => {
                    next = next.wrapping_add(1);
                    ring.push(vec![next; config.period_samples()]);
                }
                Op::Pop => {
                    let (period, outcome) = ring.pop();
                    if outcome == PopOutcome::Period {
                        seen.push(period[0]);
                    }
                }
            }
        }
        // Markers increase by at least one between consecutive real periods: equal or decreasing
        // would mean a repeat or a reorder. (`wrapping_add` never reaches the 400-op limit.)
        for pair in seen.windows(2) {
            prop_assert!(pair[1] > pair[0], "out of order: {:?}", seen);
        }
    }

    /// Keep corrective drift drops separate from capacity overruns in both outcomes
    /// and counters.
    #[test]
    fn a_push_is_counted_once_and_in_one_column(config in configs(), ops in ops()) {
        let mut ring = PeriodRing::new(config);
        let mut before = ring.counts();
        for op in ops {
            match op {
                Op::Push => {
                    let outcome = ring.push(vec![1; config.period_samples()]);
                    let after = ring.counts();
                    let overran = after.overruns - before.overruns;
                    let drifted = after.drift_drops - before.drift_drops;
                    prop_assert!(overran <= 1 && drifted <= 1);
                    match outcome {
                        PushOutcome::Queued => prop_assert_eq!((overran, drifted), (0, 0)),
                        PushOutcome::Overran => prop_assert_eq!(overran, 1),
                        PushOutcome::DriftDropped => prop_assert_eq!(drifted, 1),
                    }
                    before = after;
                }
                Op::Pop => { ring.pop(); before = ring.counts(); }
            }
        }
    }

    /// The drift policy never acts on a level between its marks, however long it stays there.
    /// That is the whole guard against correcting for ordinary scheduling jitter: a healthy ring
    /// must never lose or gain a period.
    #[test]
    fn a_level_between_the_marks_never_triggers_a_correction(
        levels in prop::collection::vec(3..7usize, 0..500usize),
    ) {
        let mut policy = DriftPolicy::with_marks(7, 2, 4);
        for level in levels {
            prop_assert_eq!(policy.on_push(level), DriftAction::Hold);
            prop_assert_eq!(policy.on_pop(level), DriftAction::Hold);
        }
    }

    /// Each side only ever produces its own action, and only after `hold` observations in a row.
    /// A policy that fired earlier would throw away audio on a hiccup; one that fired on the
    /// wrong side would correct drift in the direction that makes it worse.
    #[test]
    fn a_correction_needs_a_whole_streak_and_comes_from_the_right_side(
        hold in 1..20u32,
        run in 0..60usize,
    ) {
        let mut policy = DriftPolicy::with_marks(7, 1, hold);
        let mut drops = 0usize;
        for _ in 0..run {
            match policy.on_push(8) {
                DriftAction::DropOne => drops += 1,
                DriftAction::Hold => {}
                DriftAction::InsertOne => prop_assert!(false, "on_push must never insert"),
            }
        }
        prop_assert_eq!(drops, run / hold as usize);

        let mut policy = DriftPolicy::with_marks(7, 1, hold);
        let mut inserts = 0usize;
        for _ in 0..run {
            match policy.on_pop(0) {
                DriftAction::InsertOne => inserts += 1,
                DriftAction::Hold => {}
                DriftAction::DropOne => prop_assert!(false, "on_pop must never drop"),
            }
        }
        prop_assert_eq!(inserts, run / hold as usize);
    }
}

proptest! {
    /// A stopped producer or consumer causes xruns, not clock drift. Test both
    /// directions long enough to cross the correction window.
    #[test]
    fn an_outage_never_shows_up_as_drift(
        config in configs(),
        ops in ops(),
        outage in 1..300usize,
        drained in any::<bool>(),
    ) {
        let mut ring = PeriodRing::new(config);
        for op in ops {
            match op {
                Op::Push => { ring.push(vec![1; config.period_samples()]); }
                Op::Pop => { ring.pop(); }
            }
        }
        let before = ring.counts();
        // One side stops answering: either nobody drains the ring, or nobody fills it.
        for _ in 0..outage {
            if drained {
                ring.pop();
            } else {
                ring.push(vec![1; config.period_samples()]);
            }
        }
        let after = ring.counts();
        prop_assert_eq!(
            (after.drift_drops, after.drift_inserts),
            (before.drift_drops, before.drift_inserts),
            "counts {:?} after a {}-operation outage", after, outage
        );
        // And once it has lasted longer than the ring is deep, the outage is reported as what it
        // is, on the wall it reached. (A shorter one may still be inside the buffer,
        // which is what the buffer is for.)
        if outage > config.periods {
            if drained {
                prop_assert!(after.underruns > before.underruns, "counts {:?}", after);
            } else {
                prop_assert!(after.overruns > before.overruns, "counts {:?}", after);
            }
        }
    }
}
