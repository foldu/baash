//! Counting gate that makes the fork rule checkable at runtime.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Tracks live helper threads so that `fork()` can refuse to run while any
/// exist.
///
/// POSIX allows a multithreaded process to fork, but the child may then call
/// only async-signal-safe functions: a mutex held by a thread that does not
/// exist in the child stays held forever. Rather than reason about that per
/// call site, the shell keeps helper threads (a full-screen picker, the
/// debugger) in scopes where no process is created, and this gate asserts it.
#[derive(Debug, Default)]
pub struct ThreadGate {
    live: AtomicUsize,
}

impl ThreadGate {
    /// A gate with no threads registered.
    pub const fn new() -> Self {
        Self {
            live: AtomicUsize::new(0),
        }
    }

    /// Register a helper thread for as long as the guard lives.
    pub fn enter(&self) -> ThreadGuard<'_> {
        self.live.fetch_add(1, Ordering::SeqCst);
        ThreadGuard { gate: self }
    }

    /// Number of registered threads.
    pub fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    /// Whether it is currently legal to `fork()`.
    pub fn is_forkable(&self) -> bool {
        self.live() == 0
    }

    /// Panic if a `fork()` would violate the rule.
    ///
    /// # Panics
    ///
    /// Panics when helper threads are registered.
    pub fn assert_forkable(&self) {
        assert!(
            self.is_forkable(),
            "fork() with {} helper thread(s) live",
            self.live()
        );
    }
}

/// Decrements the count when dropped.
#[derive(Debug)]
pub struct ThreadGuard<'a> {
    gate: &'a ThreadGate,
}

impl Drop for ThreadGuard<'_> {
    fn drop(&mut self) {
        self.gate.live.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_opens_and_closes() {
        let gate = ThreadGate::new();
        assert!(gate.is_forkable());
        {
            let _guard = gate.enter();
            assert_eq!(gate.live(), 1);
            assert!(!gate.is_forkable());
        }
        assert_eq!(gate.live(), 0);
        assert!(gate.is_forkable());
    }

    #[test]
    fn nested_guards_are_counted() {
        let gate = ThreadGate::new();
        let a = gate.enter();
        let b = gate.enter();
        assert_eq!(gate.live(), 2);
        drop(a);
        assert_eq!(gate.live(), 1);
        drop(b);
        assert!(gate.is_forkable());
    }

    #[test]
    #[should_panic(expected = "fork() with 1 helper thread(s) live")]
    fn assert_forkable_panics_when_busy() {
        let gate = ThreadGate::new();
        let _guard = gate.enter();
        gate.assert_forkable();
    }
}
