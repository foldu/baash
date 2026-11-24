//! The latch a console control handler fills.
//!
//! A Unix shell gets `Ctrl-C` through `signalfd`: the interpreter blocks
//! `SIGINT`, and at each command boundary drains the descriptor, which is where
//! the trap table and the abort latch are updated (`baash-vm`'s `traps.rs`).
//! Windows has no equivalent descriptor. A console delivers `CTRL_C_EVENT` to a
//! callback on a thread the console owns, and that callback may do exactly one
//! thing: remember the key. So the callback counts the delivery here with a
//! single atomic — no allocation, no lock, no panic — and the Windows
//! `SignalFd` drains one event per call at the same command boundaries the Unix
//! reader is drained at.
//!
//! This lives outside `signal.rs` because that module is `cfg(unix)` (it is a
//! `signalfd`/`sigprocmask` wrapper), while the counting logic is portable and
//! is worth a test that runs on every platform: the handler itself can only be
//! exercised on a real console, but taking one event out of a counted queue is
//! ordinary logic, and getting it wrong either loses a `Ctrl-C` or aborts the
//! line after it.

use std::sync::atomic::{AtomicUsize, Ordering};

/// How long a blocking wait may go between asking the shell whether an
/// interrupt has arrived.
///
/// A `Ctrl-C` has to be able to end a `read` on a terminal and an embedded
/// `sleep`; on Unix neither can be interrupted by the kernel, because the shell
/// keeps `SIGINT` blocked so that no handler may touch interpreter state. So a
/// wait is cut into slices this long and the interpreter is asked between them:
/// 20 ms is below what a person notices, and it costs a few dozen wakeups a
/// second only while a command is actually waiting.
pub const INTERRUPT_SLICE: std::time::Duration = std::time::Duration::from_millis(20);

/// `Ctrl-C`/`Ctrl-Break` deliveries that a console handler has recorded and the
/// interpreter has not drained yet.
///
/// The count saturates rather than wrapping. A handler must not panic, and a
/// count that large means the key is being held down: the interpreter treats
/// the queue as "abort at the next boundary", so losing the arithmetic above
/// `usize::MAX` loses nothing a user could observe.
#[derive(Debug, Default)]
pub struct ConsoleInterrupts {
    pending: AtomicUsize,
}

impl ConsoleInterrupts {
    /// An empty latch.
    pub const fn new() -> Self {
        Self {
            pending: AtomicUsize::new(0),
        }
    }

    /// Record one delivery.
    ///
    /// This runs on the console's handler thread, so it is one relaxed atomic
    /// operation and nothing else. Relaxed ordering is enough: the only
    /// consumer is the interpreter's next boundary, where the abort it causes
    /// needs no other memory from this thread.
    pub fn record(&self) {
        let mut count = self.pending.load(Ordering::Relaxed);
        loop {
            // Saturate rather than wrap: a handler must not panic, and a count
            // this high is a held-down key, which the interpreter treats as one
            // abort anyway.
            let Some(next) = count.checked_add(1) else {
                return;
            };
            match self.pending.compare_exchange_weak(
                count,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => count = actual,
            }
        }
    }

    /// Take one pending delivery, returning whether there was one.
    ///
    /// A compare-exchange loop rather than `fetch_sub`, because `fetch_sub` on
    /// a zero count wraps to `usize::MAX` and would turn "no `Ctrl-C`" into
    /// "billions of them".
    pub fn take(&self) -> bool {
        let mut count = self.pending.load(Ordering::Relaxed);
        loop {
            if count == 0 {
                return false;
            }
            match self.pending.compare_exchange_weak(
                count,
                count - 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => count = actual,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_latch_has_nothing_to_take() {
        let latch = ConsoleInterrupts::new();
        assert!(!latch.take());
        // Taking from empty must not underflow into a large count.
        assert!(!latch.take());
        assert!(!latch.take());
    }

    #[test]
    fn each_delivery_is_taken_exactly_once() {
        let latch = ConsoleInterrupts::new();
        latch.record();
        latch.record();
        latch.record();
        assert!(latch.take());
        assert!(latch.take());
        assert!(latch.take());
        assert!(!latch.take());
    }

    #[test]
    fn recording_after_draining_starts_a_new_batch() {
        // This is the sequence the interpreter produces: a `Ctrl-C` aborts a
        // command, the line's housekeeping drains, and the next key must still
        // be seen rather than being swallowed by the old batch.
        let latch = ConsoleInterrupts::new();
        latch.record();
        assert!(latch.take());
        latch.record();
        assert!(latch.take());
        assert!(!latch.take());
    }

    #[test]
    fn a_count_is_shared_across_threads() {
        // The handler runs on another thread; the interpreter drains on its
        // own. Every recorded event must still be visible to the drainer.
        use std::sync::Arc;
        let latch = Arc::new(ConsoleInterrupts::new());
        let recorder = Arc::clone(&latch);
        std::thread::spawn(move || {
            for _ in 0..1000 {
                recorder.record();
            }
        })
        .join()
        .unwrap();
        for _ in 0..1000 {
            assert!(latch.take());
        }
        assert!(!latch.take());
    }
}
