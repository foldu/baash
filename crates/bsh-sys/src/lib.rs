//! The syscall boundary.
//!
//! This is the **only** crate in the workspace that may contain `unsafe`
//! (DESIGN.md §4.3, §4.1). Everything reachable from `fork()` must be safe
//! Rust, so that the code which runs between `fork()` and `exec()` can be
//! audited by reading one crate.
//!
//! Landed so far: [`ThreadGate`], which mechanically enforces the fork rule
//! "no process creation while helper threads are live", and [`is_tty`].

pub mod threadgate;

pub use threadgate::{ThreadGate, ThreadGuard};

/// Whether `fd` refers to a terminal.
///
/// Interactive behaviour (colour, the line editor, job control) hangs off this
/// one question, so it lives next to the other syscalls.
pub fn is_tty(fd: i32) -> bool {
    // SAFETY: `isatty` only inspects the descriptor and cannot retain it.
    unsafe { libc::isatty(fd) == 1 }
}
