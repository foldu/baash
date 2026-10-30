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
//! * [`spawn()`], [`spawn_forked`], [`fork_child`], [`fork_child_status`],
//!   [`exit_child`], [`exec`] and [`SpawnPlan`] — creating children and
//!   replacing this process. External commands go through `posix_spawn` and
//!   are safe in a threaded parent; shell code in a child uses `fork` and is
//!   gated by [`fork_gate`]. `exec` is the in-place `execve` the `exec`
//!   built-in needs: it keeps the pid and carries the plan's redirections.
//! * [`wait_pid`], [`wait_any_nohang`], [`WaitOutcome`] — reaping, with
//!   `EINTR` retried and `SIGCHLD` treated as a wakeup hint, never as a
//!   per-child event.
//! * [`Pipe`], [`open_cloexec`], [`dup`], [`dup_above`], [`dup2`], [`close`],
//!   [`chdir`], [`fd_is_open`], [`create_unlinked_temp`] — descriptor plumbing,
//!   including here-doc bodies as unlinked files and descriptor save/restore
//!   without a `dup2`-to-a-high-number dance.
//! * [`SignalSet`], [`MaskGuard`], [`SignalFd`] — masks and a pollable signal
//!   source. `SignalFd` is Linux-only (`signalfd`); elsewhere its constructor
//!   fails with [`std::io::ErrorKind::Unsupported`] rather than pretending.
//! * [`RawMode`], [`Pty`], [`open_pty`], [`window_size`] — terminal state for
//!   the line editor, with an owned pty pair so that state can be tested
//!   without a controlling terminal.
//! * [`cstring`] — converting shell bytes for `exec`, failing with
//!   [`NulByte`] instead of truncating.
//! * [`Regex`], [`Error`] — compiled POSIX extended regular expressions for
//!   `[[ x =~ re ]]` and `BASH_REMATCH`, using the same platform engine as
//!   bash (`regcomp`/`regexec`). Byte offsets, POSIX ERE, no `REG_NEWLINE`;
//!   see [`regex`] for the limits and the measured engine quirks.
//!
//! # Platform
//!
//! Linux is the primary target (`x86_64-linux`). The generic POSIX parts
//! (spawn, wait, descriptors, termios) also build elsewhere; `signalfd`,
//! `memfd_create` and `O_TMPFILE` are `#[cfg(target_os = "linux")]` with
//! documented fallbacks. The `regex` module is `#[cfg(unix)]`: POSIX regular
//! expressions are a Unix facility, and on a Unix target whose `libc` does not
//! declare `regex_t`/`REG_*` (Solaris/illumos, Haiku and Hurd today) the crate
//! does not build. There is no fallback engine, because a different engine
//! would not agree with bash.

#![warn(missing_docs)]
// Every `unsafe` block in this crate states the invariant that makes it sound,
// immediately above the block. The lint keeps the rule enforceable rather than
// aspirational: a new block without a `// SAFETY:` comment fails the build
// instead of relying on review.
#![deny(clippy::undocumented_unsafe_blocks)]

pub mod bytes;
pub mod fd;
pub mod pty;
#[cfg(unix)]
pub mod regex;
pub mod signal;
pub mod spawn;
pub mod term;
pub mod threadgate;
pub mod wait;

pub use bytes::{NulByte, cstring, cstrings};
pub use fd::{
    Pipe, chdir, close, create_unlinked_temp, dup, dup_above, dup2, fd_is_cloexec, fd_is_open,
    lseek, open_cloexec, poll_readable, read_fd, set_cloexec, write_all, write_fd,
};
pub use pty::{Pty, become_session_leader, open_pty};
#[cfg(unix)]
pub use regex::{CaptureRanges, Regex, RegexError};
pub use signal::{
    Disposition, MaskGuard, SignalEvent, SignalFd, SignalGuard, SignalSet, ignore_signals,
    replace_signal_mask, send_signal, set_disposition,
};
pub use spawn::{
    Child, FdAction, ProcessGroup, ReportPipe, SpawnPlan, exec, exit_child, fork_child,
    fork_child_status, spawn, spawn_forked,
};
pub use term::{NoEcho, RawMode, parent_pid, pid, window_size, window_size_fd};
pub use threadgate::{ThreadGate, ThreadGuard, fork_gate};
pub use wait::{WaitOutcome, wait_any_nohang, wait_pid};

// `FdAction::Open` and the signal APIs name C types in their signatures, so a
// consumer would otherwise need its own `libc` dependency just to call us.
// Re-exporting keeps the rule that this crate is the only one depending on the
// platform.
pub use libc::{c_int, mode_t};

/// `open(2)` access flags, for building [`spawn::FdAction::Open`] without a
/// `libc` dependency.
pub mod flags {
    use super::c_int;

    /// Open for reading.
    pub const RDONLY: c_int = libc::O_RDONLY;
    /// Open for writing.
    pub const WRONLY: c_int = libc::O_WRONLY;
    /// Open for reading and writing.
    pub const RDWR: c_int = libc::O_RDWR;
    /// Create the file if it does not exist.
    pub const CREAT: c_int = libc::O_CREAT;
    /// Truncate the file.
    pub const TRUNC: c_int = libc::O_TRUNC;
    /// Append to the file.
    pub const APPEND: c_int = libc::O_APPEND;
}

/// `SIGPIPE`, for building a [`SignalSet`] that resets it in children.
pub const SIGPIPE: c_int = libc::SIGPIPE;

/// `SIGALRM`: the status of a `read -t` timeout is `128 + SIGALRM`.
pub const SIGALRM: c_int = libc::SIGALRM;

/// `SIGINT`: ignored by the shell while it waits for a foreground child.
pub const SIGINT: c_int = libc::SIGINT;

/// `SIGQUIT`: ignored by the shell while it waits for a foreground child.
pub const SIGQUIT: c_int = libc::SIGQUIT;

/// `SIGTERM`, the default signal for `kill`.
pub const SIGTERM: c_int = libc::SIGTERM;

/// `SIGSTOP`: cannot be caught, blocked or ignored.
pub const SIGSTOP: c_int = libc::SIGSTOP;

/// `SIGTSTP`: the terminal's stop key (`Ctrl-Z`).
pub const SIGTSTP: c_int = libc::SIGTSTP;

/// `SIGTTIN`: a background process tried to read the terminal.
pub const SIGTTIN: c_int = libc::SIGTTIN;

/// `SIGTTOU`: a background process tried to write the terminal or change its
/// foreground group.
pub const SIGTTOU: c_int = libc::SIGTTOU;

/// `SIGCONT`: resume a stopped process group.
pub const SIGCONT: c_int = libc::SIGCONT;

/// `SIGCHLD`: a child changed state (a wakeup hint, never a per-child event).
pub const SIGCHLD: c_int = libc::SIGCHLD;

/// `ECHILD`: the pid is not a child, i.e. `wait` has nothing to report for it.
pub const ECHILD: c_int = libc::ECHILD;

/// Whether `fd` refers to a terminal.
///
/// Interactive behaviour (colour, the line editor, the terminal REPL) hangs
/// off this one question, so it lives next to the other syscalls.
pub fn is_tty(fd: i32) -> bool {
    // SAFETY: `isatty` only inspects the descriptor and cannot retain it.
    unsafe { libc::isatty(fd) == 1 }
}
