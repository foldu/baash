//! A set of signals, as a bitmask — the platform-neutral half of the signal
//! layer.
//!
//! Linux numbers signals 1 to 64, which is exactly a `u64`: membership is a
//! shift, iteration a bit scan, and two sets compare as two words. The
//! `sigset_t` that `sigprocmask` and `signalfd` want is built only at those
//! calls — asking libc `sigismember` in a loop was 10% of a subshell's CPU
//! after the scope stopped copying the variable table, because `sync_traps`
//! walks both the wanted and the blocked set on every change and `PartialEq`
//! used to walk all 64 entries too (`docs/performance.md`).
//!
//! The type lives here, rather than in the Unix `signal.rs`, because the
//! interpreter's bookkeeping is shared: `sync_traps` builds the wanted set on
//! every platform to decide whether a signal reader is needed and whether the
//! mask moved. Windows has no mask to apply and no `signalfd` to open, but it
//! does have one signal a console delivers — `Ctrl-C` — and its reader is only
//! created when this set says `SIGINT` was wanted. A set that answered "no" to
//! every `insert` made that bookkeeping silently empty and the console event
//! unreachable, so the bitmask is real on both platforms and only the
//! syscalls are Unix-only.

use std::fmt;

use crate::c_int;

/// A set of signals, as a bitmask.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct SignalSet {
    /// Bit `n - 1` is signal `n`; signals are numbered from 1.
    bits: u64,
}

impl SignalSet {
    /// The empty set.
    pub fn empty() -> Self {
        Self { bits: 0 }
    }

    /// A set containing the given signals. Out-of-range entries are ignored.
    pub fn of(signals: &[c_int]) -> Self {
        let mut set = Self::empty();
        for &signal in signals {
            set.insert(signal);
        }
        set
    }

    /// Add `signal`; returns whether it was a valid signal number.
    pub fn insert(&mut self, signal: c_int) -> bool {
        let Some(bit) = bit_of(signal) else {
            return false;
        };
        self.bits |= bit;
        true
    }

    /// Remove `signal`; returns whether it was present.
    pub fn remove(&mut self, signal: c_int) -> bool {
        let Some(bit) = bit_of(signal) else {
            return false;
        };
        let present = self.bits & bit != 0;
        self.bits &= !bit;
        present
    }

    /// Whether `signal` is a member of the set.
    pub fn contains(&self, signal: c_int) -> bool {
        bit_of(signal).is_some_and(|bit| self.bits & bit != 0)
    }

    /// The set with exactly `bits`; bit `n - 1` is signal `n`.
    ///
    /// For a platform that reads the whole mask at once (`sigprocmask` returns
    /// a `sigset_t`, not a membership answer); the bitmask's own views are what
    /// the interpreter uses.
    pub(crate) fn from_bits(bits: u64) -> Self {
        Self { bits }
    }

    /// Whether the set contains no signals.
    pub fn is_empty(&self) -> bool {
        self.bits == 0
    }

    /// Iterate the member signals in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = c_int> + '_ {
        let mut rest = self.bits;
        std::iter::from_fn(move || {
            if rest == 0 {
                return None;
            }
            // Lowest set bit: its index is the signal number minus one.
            let index = rest.trailing_zeros();
            rest &= rest - 1;
            Some(index as c_int + 1)
        })
    }
}

/// The bit for a signal number, or `None` when the host has no such signal.
///
/// Linux numbers signals 1 to 64; asking for anything else is not an error a
/// caller can act on, and every set operation ignores it. Windows uses the same
/// 1..=64 numbering (see the constants in `lib.rs`) so one table serves both.
fn bit_of(signal: c_int) -> Option<u64> {
    (1..=64).contains(&signal).then(|| 1u64 << (signal - 1))
}

impl fmt::Debug for SignalSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_membership_and_iteration() {
        let mut set = SignalSet::empty();
        assert!(set.is_empty());
        assert!(set.insert(2));
        assert!(set.insert(15));
        assert!(set.contains(2));
        assert!(!set.contains(10));
        assert_eq!(set.iter().collect::<Vec<_>>(), vec![2, 15]);
        assert!(set.remove(2));
        assert_eq!(set.iter().collect::<Vec<_>>(), vec![15]);
        assert_eq!(SignalSet::of(&[15]), set);
    }

    #[test]
    fn signals_outside_the_host_range_are_ignored() {
        // The bitmask has room for exactly the 64 numbers Linux defines; a
        // caller that passes something else (a BSD number, a zero) must not
        // set a bit that would shift into the wrong signal.
        let mut set = SignalSet::empty();
        assert!(!set.insert(0));
        assert!(!set.insert(-1));
        assert!(!set.insert(65));
        assert!(set.is_empty());
        assert!(!set.contains(65));
        assert!(!set.remove(0));
        assert!(set.insert(64));
        assert!(set.contains(64));
        assert_eq!(set.iter().collect::<Vec<_>>(), vec![64]);
    }

    #[test]
    fn removing_an_absent_signal_reports_absent() {
        let mut set = SignalSet::of(&[2]);
        assert!(!set.remove(15));
        assert!(set.contains(2));
    }
}
