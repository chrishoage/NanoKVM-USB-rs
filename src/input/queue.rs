//! Input queue with motion coalescing and transition barriers.
//!
//! Motion can be replaced or accumulated only within its current run. Key, button, and
//! wheel transitions preserve order so a click cannot move across the motion around it.

use std::collections::VecDeque;
use std::time::Instant;

use crate::input::stats::Counters;
use crate::proto::report::HidKey;

/// Coalesced motion between transition barriers. Absolute positions replace earlier
/// positions; relative deltas accumulate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct MotionRun {
    /// The newest absolute position submitted in this run, already in `0..=4095`.
    pub(crate) abs: Option<(u16, u16)>,
    /// Summed relative motion. `i32` with saturating addition: the split at flush time is what
    /// keeps the remainder, so the accumulator must not wrap.
    pub(crate) dx: i32,
    pub(crate) dy: i32,
    /// Summed wheel delta.
    pub(crate) wheel: i32,
}

impl MotionRun {
    /// True when the run carries nothing observable and so produces no report at flush.
    pub(crate) fn is_empty(&self) -> bool {
        self.abs.is_none() && self.dx == 0 && self.dy == 0 && self.wheel == 0
    }
}

/// What a queue entry is: a coalesced motion run, or one transition (a barrier).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryKind {
    Motion(MotionRun),
    /// A key transition. Never dropped, never merged, never reordered. Keyboard reports
    /// are never coalesced: merging drops a fast press-release pair entirely.
    Key {
        key: HidKey,
        down: bool,
    },
    /// A mouse button transition, same barrier rules.
    Button {
        mask: u8,
        down: bool,
    },
}

impl EntryKind {
    pub(crate) fn is_barrier(&self) -> bool {
        !matches!(self, EntryKind::Motion(_))
    }
}

/// Queued input with its session epoch and submission time.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Entry {
    pub(crate) kind: EntryKind,
    pub(crate) epoch: u64,
    pub(crate) enqueued: Instant,
}

/// The mutable queue state, guarded by the one mutex the writer and every producer share.
#[derive(Debug)]
pub(crate) struct QueueState {
    entries: VecDeque<Entry>,
    barriers: usize,
    max_barriers: usize,
    /// The reason of the trigger that set the cancellation flag; taken by the writer when it
    /// latches the epoch. Concurrent triggers coalesce into one sequence and so must
    /// coalesce into one reason: see [`QueueState::note_reason`].
    pub(crate) pending_reason: Option<super::ReleaseReason>,
}

/// A barrier could not be admitted: the queue is full of transitions and coalescing cannot
/// reclaim anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Overflow;

impl QueueState {
    pub(crate) fn new(max_barriers: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            barriers: 0,
            // A bound of zero would refuse every transition, which is a wedged session rather
            // than a bounded one.
            max_barriers: max_barriers.max(1),
            pending_reason: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(crate) fn barriers(&self) -> usize {
        self.barriers
    }

    fn publish_depth(&self, counters: &Counters) {
        counters.set_depth(self.entries.len(), self.barriers);
    }

    /// Fold an absolute position into the tail run, or start a new run.
    pub(crate) fn push_abs(
        &mut self,
        x: u16,
        y: u16,
        epoch: u64,
        now: Instant,
        counters: &Counters,
    ) {
        if let Some(run) = self.tail_run(epoch) {
            if run.abs.is_some() {
                Counters::bump(&counters.abs_replaced);
            }
            run.abs = Some((x, y));
        } else {
            self.start_run(
                MotionRun {
                    abs: Some((x, y)),
                    ..MotionRun::default()
                },
                epoch,
                now,
            );
        }
        self.publish_depth(counters);
    }

    /// Sum a relative delta into the tail run, or start a new run.
    pub(crate) fn push_rel(
        &mut self,
        dx: i32,
        dy: i32,
        epoch: u64,
        now: Instant,
        counters: &Counters,
    ) {
        if let Some(run) = self.tail_run(epoch) {
            run.dx = run.dx.saturating_add(dx);
            run.dy = run.dy.saturating_add(dy);
            Counters::bump(&counters.rel_summed);
        } else {
            self.start_run(
                MotionRun {
                    dx,
                    dy,
                    ..MotionRun::default()
                },
                epoch,
                now,
            );
        }
        self.publish_depth(counters);
    }

    /// Sum a wheel delta into the tail run, or start a new run.
    pub(crate) fn push_wheel(&mut self, delta: i32, epoch: u64, now: Instant, counters: &Counters) {
        if let Some(run) = self.tail_run(epoch) {
            run.wheel = run.wheel.saturating_add(delta);
            Counters::bump(&counters.wheel_summed);
        } else {
            self.start_run(
                MotionRun {
                    wheel: delta,
                    ..MotionRun::default()
                },
                epoch,
                now,
            );
        }
        self.publish_depth(counters);
    }

    /// Enqueue a transition barrier. Returns failure if the barrier limit is reached;
    /// the caller must cancel the session rather than discard one transition.
    pub(crate) fn push_barrier(
        &mut self,
        kind: EntryKind,
        epoch: u64,
        now: Instant,
        counters: &Counters,
    ) -> Result<(), Overflow> {
        debug_assert!(kind.is_barrier());
        if self.barriers >= self.max_barriers {
            return Err(Overflow);
        }
        self.entries.push_back(Entry {
            kind,
            epoch,
            enqueued: now,
        });
        self.barriers += 1;
        self.publish_depth(counters);
        Ok(())
    }

    /// Merge a cancellation reason by severity.
    ///
    /// Shutdown outranks link loss, then overflow, then ordinary releases. Equal severity
    /// keeps the first cause.
    pub(crate) fn note_reason(&mut self, reason: super::ReleaseReason) {
        let replace = match self.pending_reason {
            None => true,
            Some(current) => reason.severity() > current.severity(),
        };
        if replace {
            self.pending_reason = Some(reason);
        }
    }

    /// Return the tail motion run only when it belongs to the current epoch.
    fn tail_run(&mut self, epoch: u64) -> Option<&mut MotionRun> {
        match self.entries.back_mut() {
            Some(Entry {
                kind: EntryKind::Motion(run),
                epoch: e,
                ..
            }) if *e == epoch => Some(run),
            _ => None,
        }
    }

    fn start_run(&mut self, run: MotionRun, epoch: u64, now: Instant) {
        self.entries.push_back(Entry {
            kind: EntryKind::Motion(run),
            epoch,
            enqueued: now,
        });
    }

    /// Take the next entry in submission order.
    pub(crate) fn pop(&mut self, counters: &Counters) -> Option<Entry> {
        let entry = self.entries.pop_front()?;
        if entry.kind.is_barrier() {
            self.barriers -= 1;
        }
        self.publish_depth(counters);
        Some(entry)
    }

    /// Discard entries through `epoch` without writing them.
    pub(crate) fn drain_upto(&mut self, epoch: u64, counters: &Counters) -> usize {
        let before = self.entries.len();
        self.entries.retain(|e| e.epoch > epoch);
        self.barriers = self.entries.iter().filter(|e| e.kind.is_barrier()).count();
        self.publish_depth(counters);
        before - self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counters() -> Counters {
        Counters::default()
    }

    #[test]
    fn motion_coalesces_into_one_entry() {
        let c = counters();
        let mut q = QueueState::new(8);
        let now = Instant::now();
        for i in 0..100u16 {
            q.push_abs(i, i, 0, now, &c);
        }
        assert_eq!(q.len(), 1);
        let Some(Entry {
            kind: EntryKind::Motion(run),
            ..
        }) = q.pop(&c)
        else {
            panic!("expected one motion run");
        };
        assert_eq!(run.abs, Some((99, 99)));
    }

    #[test]
    fn rel_is_summed_not_replaced() {
        let c = counters();
        let mut q = QueueState::new(8);
        let now = Instant::now();
        q.push_rel(100, -5, 0, now, &c);
        q.push_rel(200, -5, 0, now, &c);
        let Some(Entry {
            kind: EntryKind::Motion(run),
            ..
        }) = q.pop(&c)
        else {
            panic!("expected one motion run");
        };
        assert_eq!((run.dx, run.dy), (300, -10));
    }

    #[test]
    fn barrier_starts_a_fresh_accumulation() {
        let c = counters();
        let mut q = QueueState::new(8);
        let now = Instant::now();
        q.push_rel(1, 0, 0, now, &c);
        assert!(q
            .push_barrier(
                EntryKind::Button {
                    mask: 1,
                    down: true
                },
                0,
                now,
                &c
            )
            .is_ok());
        q.push_rel(2, 0, 0, now, &c);
        assert_eq!(q.len(), 3);
    }

    #[test]
    fn barrier_bound_is_enforced() {
        let c = counters();
        let mut q = QueueState::new(2);
        let now = Instant::now();
        let kind = EntryKind::Button {
            mask: 1,
            down: true,
        };
        assert!(q.push_barrier(kind, 0, now, &c).is_ok());
        assert!(q.push_barrier(kind, 0, now, &c).is_ok());
        assert_eq!(q.push_barrier(kind, 0, now, &c), Err(Overflow));
        // Motion still fits: it coalesces and never consumes a barrier slot.
        q.push_abs(1, 1, 0, now, &c);
        assert_eq!(q.barriers(), 2);
    }
}
