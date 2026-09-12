//! Held-key state owned by the input writer.
//!
//! The six-key HID limit suppresses additional keys until their release. Promoting a
//! previously suppressed key would create a press the user did not just make.

use crate::proto::report::{HidKey, KeyboardReport};

/// Non-modifier keys the 8-byte report can carry.
pub(crate) const SLOTS: usize = 6;

/// The status a key is admitted with at press time, fixed for the duration of the hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    /// Pressed while fewer than six keys were `Reported`; occupies a slot.
    Reported,
    /// Pressed while six were already `Reported`; tracked, never entering any report.
    Suppressed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeldKey {
    usage: u8,
    status: Status,
}

/// The physical held set with its per-key statuses, the modifier mask and the button mask.
#[derive(Debug, Default)]
pub(crate) struct HeldState {
    /// Non-modifier keys in press order. Unbounded: this is the physical set, not the report.
    keys: Vec<HeldKey>,
    modifiers: u8,
    buttons: u8,
}

impl HeldState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The current mouse button mask (`proto::button` bits).
    pub(crate) fn buttons(&self) -> u8 {
        self.buttons
    }

    /// Project the physical set into the 8-byte report: the `Reported` keys in press order,
    /// remaining slots zero. Never more than [`SLOTS`], and often fewer than the number of keys
    /// physically held.
    pub(crate) fn report(&self) -> KeyboardReport {
        let mut keys = [0u8; SLOTS];
        for (slot, key) in keys
            .iter_mut()
            .zip(self.keys.iter().filter(|k| k.status == Status::Reported))
        {
            *slot = key.usage;
        }
        KeyboardReport {
            modifiers: self.modifiers,
            keys,
        }
    }

    fn reported_count(&self) -> usize {
        self.keys
            .iter()
            .filter(|k| k.status == Status::Reported)
            .count()
    }

    /// Apply one key transition and return the report to send, or `None` when nothing
    /// observable changed.
    ///
    /// `None` is returned for:
    /// - a key-down for a key already held (a host auto-repeat that slipped past the viewer's
    ///   own `repeat` filter, or a duplicated event);
    /// - a key-down admitted as `Suppressed` — six keys already occupy the slots;
    /// - a key-up for a `Suppressed` key, or for a key that is not held at all (which is what a
    ///   key-up arriving after a release-all looks like).
    pub(crate) fn apply_key(&mut self, key: HidKey, down: bool) -> Option<KeyboardReport> {
        match key {
            HidKey::Modifier(bit) => {
                // Modifiers live in the report's first byte and never occupy a slot.
                let next = if down {
                    self.modifiers | bit
                } else {
                    self.modifiers & !bit
                };
                if next == self.modifiers {
                    return None;
                }
                self.modifiers = next;
                Some(self.report())
            }
            HidKey::Usage(usage) => {
                if down {
                    if self.keys.iter().any(|k| k.usage == usage) {
                        return None;
                    }
                    if self.reported_count() < SLOTS {
                        self.keys.push(HeldKey {
                            usage,
                            status: Status::Reported,
                        });
                        Some(self.report())
                    } else {
                        self.keys.push(HeldKey {
                            usage,
                            status: Status::Suppressed,
                        });
                        None
                    }
                } else {
                    let pos = self.keys.iter().position(|k| k.usage == usage)?;
                    let removed = self.keys.remove(pos);
                    // A freed slot is never filled by a suppressed key: the report is rebuilt
                    // from the remaining `Reported` keys and may carry five where six were
                    // held.
                    match removed.status {
                        Status::Reported => Some(self.report()),
                        Status::Suppressed => None,
                    }
                }
            }
        }
    }

    /// Apply one button transition and return the new button mask, or `None` when the mask did
    /// not change. `mask` is normally a single bit from `proto::button`; a multi-bit mask is
    /// applied wholesale and a zero mask is a no-op.
    pub(crate) fn apply_button(&mut self, mask: u8, down: bool) -> Option<u8> {
        let next = if down {
            self.buttons | mask
        } else {
            self.buttons & !mask
        };
        if next == self.buttons {
            return None;
        }
        self.buttons = next;
        Some(next)
    }

    /// Clear the physical held set with its per-key statuses, the modifier mask and the button
    /// mask. Part of the cancellation sequence.
    pub(crate) fn clear(&mut self) {
        self.keys.clear();
        self.modifiers = 0;
        self.buttons = 0;
    }

    /// Number of keys physically held, reported or suppressed. Test/diagnostic use.
    #[cfg(test)]
    pub(crate) fn held_count(&self) -> usize {
        self.keys.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::report::modifier;

    #[test]
    fn modifiers_never_occupy_slots() {
        let mut h = HeldState::new();
        let r = h.apply_key(HidKey::Modifier(modifier::LEFT_SHIFT), true);
        assert_eq!(
            r,
            Some(KeyboardReport {
                modifiers: modifier::LEFT_SHIFT,
                keys: [0; 6]
            })
        );
        assert_eq!(h.held_count(), 0);
    }

    #[test]
    fn duplicate_press_is_a_no_op() {
        let mut h = HeldState::new();
        assert!(h.apply_key(HidKey::Usage(4), true).is_some());
        assert_eq!(h.apply_key(HidKey::Usage(4), true), None);
        assert_eq!(h.held_count(), 1);
    }

    #[test]
    fn status_is_fixed_at_press_time() {
        let mut h = HeldState::new();
        for usage in 4..=10u8 {
            h.apply_key(HidKey::Usage(usage), true);
        }
        assert_eq!(h.held_count(), 7);
        assert_eq!(h.report().keys, [4, 5, 6, 7, 8, 9]);
        // Releasing a reported key frees a slot; the suppressed key is not promoted.
        let after = h.apply_key(HidKey::Usage(4), false);
        assert_eq!(
            after,
            Some(KeyboardReport {
                modifiers: 0,
                keys: [5, 6, 7, 8, 9, 0]
            })
        );
    }
}
