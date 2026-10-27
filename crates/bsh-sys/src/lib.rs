//! The syscall boundary.
//!
//! This is the **only** crate in the workspace that may contain `unsafe`
//! (DESIGN.md §4.1, §4.3). Everything reachable from `fork()` must be safe
//! Rust, so that the code which runs between `fork()` and `exec()` can be
//! audited by reading one crate. Every `unsafe` block carries a `// SAFETY:`
//! comment stating what makes it sound.
//!
//! # What is here
//!
//! * [`spawn()`], [`spawn_forked`], [`fork_child`] and [`SpawnPlan`] — creating
//!   children. External commands go through `posix_spawn` and are safe in a
//!   threaded parent; shell code in a child uses `fork` and is gated by
//!   [`fork_gate`].
//! * [`wait_pid`], [`wait_any_nohang`], [`WaitOutcome`] — reaping, with
//!   `EINTR` retried and `SIGCHLD` treated as a wakeup hint, never as a
//!   per-child event.
//! * [`Pipe`], [`open_cloexec`], [`dup2`], [`close`], [`fd_is_open`],
//!   [`create_unlinked_temp`] — descriptor plumbing, including here-doc
//!   bodies as unlinked files.
//! * [`SignalSet`], [`MaskGuard`], [`SignalFd`] — masks and a pollable signal
//!   source. `SignalFd` is Linux-only (`signalfd`); elsewhere its constructor
//!   fails with [`std::io::ErrorKind::Unsupported`] rather than pretending.
//! * [`RawMode`], [`window_size`], [`set_foreground_pgrp`] — terminal state
//!   for the line editor and job control.
//! * [`cstring`] — converting shell bytes for `exec`, failing with
//!   [`NulByte`] instead of truncating.
//!
//! # Platform
//!
//! Linux is the primary target (`x86_64-linux`). The generic POSIX parts
//! (spawn, wait, descriptors, termios) also build elsewhere; `signalfd`,
//! `memfd_create` and `O_TMPFILE` are `#[cfg(target_os = "linux")]` with
//! documented fallbacks.

#![warn(missing_docs)]

pub mod bytes;
pub mod fd;
pub mod signal;
pub mod spawn;
pub mod term;
pub mod threadgate;
pub mod wait;

pub use bytes::{NulByte, cstring, cstrings};
pub use fd::{
    Pipe, close, create_unlinked_temp, dup2, fd_is_cloexec, fd_is_open, open_cloexec, set_cloexec,
};
pub use signal::{MaskGuard, SignalEvent, SignalFd, SignalSet, send_group_signal, send_signal};
pub use spawn::{
    Child, FdAction, ProcessGroup, ReportPipe, SpawnPlan, fork_child, spawn, spawn_forked,
};
pub use term::{
    RawMode, get_foreground_pgrp, parent_pid, pid, process_group, set_foreground_pgrp,
    set_foreground_pgrp_fd, set_process_group, window_size, window_size_fd,
};
pub use threadgate::{ThreadGate, ThreadGuard, fork_gate};
pub use wait::{WaitOutcome, wait_any_nohang, wait_any_nohang_wuntraced, wait_pid};

/// Whether `fd` refers to a terminal.
///
/// Interactive behaviour (colour, the line editor, job control) hangs off this
/// one question, so it lives next to the other syscalls.
pub fn is_tty(fd: i32) -> bool {
    // SAFETY: `isatty` only inspects the descriptor and cannot retain it.
    unsafe { libc::isatty(fd) == 1 }
}
