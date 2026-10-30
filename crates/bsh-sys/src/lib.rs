//! The syscall boundary.
//!
//! This is the **only** crate in the workspace that may contain `unsafe`
//! (DESIGN.md §4.1). Every `unsafe` block carries a `// SAFETY:` comment stating
//! what makes it sound, and `clippy::undocumented_unsafe_blocks` is denied so a
//! new block cannot land without one.
//!
//! Nothing here forks any more: the shell runs subshells, command
//! substitutions and pipeline stages in process (`docs/direction.md` §2), so
//! the only child creation left is `posix_spawn` and the only `exec` is the
//! in-place one the built-in needs.
//!
//! # What is here
//!
//! * [`spawn()`], [`exec`] and [`SpawnPlan`] — creating children and
//!   replacing this process. External commands go through `posix_spawn` and
//!   are safe in a threaded parent. `exec` is the in-place `execve` the `exec`
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
pub mod outcome;
#[cfg(unix)]
pub mod fd;
#[cfg(unix)]
pub mod pty;
#[cfg(unix)]
pub mod regex;
#[cfg(unix)]
pub mod signal;
#[cfg(unix)]
pub mod spawn;
#[cfg(unix)]
pub mod term;
#[cfg(unix)]
pub mod wait;
#[cfg(windows)]
pub mod windows;

pub use bytes::{NulByte, cstring, cstrings};
#[cfg(unix)]
pub use fd::{
    Pipe, chdir, close, create_unlinked_temp, dup, dup_above, dup2, fd_is_cloexec, fd_is_open,
    lseek, open_cloexec, poll_readable, read_fd, seek, set_cloexec, write_all, write_fd,
};
#[cfg(unix)]
pub use pty::{Pty, become_session_leader, open_pty};
#[cfg(unix)]
pub use regex::{CaptureRanges, Regex, RegexError};
#[cfg(unix)]
pub use signal::{
    Disposition, MaskGuard, SignalEvent, SignalFd, SignalGuard, SignalSet, ignore_signals,
    replace_signal_mask, send_signal, set_disposition,
};
#[cfg(unix)]
pub use spawn::{Child, FdAction, SpawnPlan, exec, fork_child_status, spawn};
#[cfg(unix)]
pub use term::{NoEcho, RawMode, parent_pid, pid, window_size, window_size_fd};
#[cfg(unix)]
pub use wait::{wait_any_nohang, wait_pid};
pub use outcome::WaitOutcome;

#[cfg(windows)]
pub use windows::{
    Child, Disposition, FdAction, MaskGuard, NoEcho, Pipe, Pty, RawMode, SignalEvent, SignalFd,
    SignalGuard, SignalSet, SpawnPlan, become_session_leader, chdir, close,
    create_unlinked_temp, dup, dup_above, dup2, exec, fd_is_cloexec, fd_is_open, fork_child_status,
    ignore_signals, lseek, open_cloexec, open_pty, parent_pid, pid, poll_readable, read_fd,
    replace_signal_mask, seek, send_signal, set_cloexec, set_disposition, spawn, wait_any_nohang,
    wait_pid, window_size, window_size_fd, write_all, write_fd,
};

// `FdAction::Open` and the signal APIs name C types in their signatures, so a
// consumer would otherwise need its own `libc` dependency just to call us.
// Re-exporting keeps the rule that this crate is the only one depending on the
// platform.
pub use libc::c_int;
#[cfg(unix)]
pub use libc::mode_t;
#[cfg(windows)]
pub use crate::windows::mode_t;

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

// Signal numbers.
//
// On Unix these come from `libc`. Windows has no signals in the POSIX sense —
// `SetConsoleCtrlHandler` delivers a console event, not a numbered signal — so
// the numbers there are the Linux values, kept so the shell's tables, messages
// and `128 + n` arithmetic have something consistent to compare. Only `SIGINT`
// and `SIGTERM` reach the platform: the former as a console event (once ConPTY
// exists) and the latter as `TerminateProcess`.

/// `SIGPIPE`, for building a [`SignalSet`] that resets it in children.
#[cfg(unix)]
pub const SIGPIPE: c_int = libc::SIGPIPE;
/// `SIGPIPE`; see the note on Windows signal numbers.
#[cfg(windows)]
pub const SIGPIPE: c_int = 13;

/// `SIGALRM`: the status of a `read -t` timeout is `128 + SIGALRM`.
#[cfg(unix)]
pub const SIGALRM: c_int = libc::SIGALRM;
/// `SIGALRM`; see the note on Windows signal numbers.
#[cfg(windows)]
pub const SIGALRM: c_int = 14;

/// `SIGINT`: ignored by the shell while it waits for a foreground child.
#[cfg(unix)]
pub const SIGINT: c_int = libc::SIGINT;
/// `SIGINT`: the one console event Windows does deliver.
#[cfg(windows)]
pub const SIGINT: c_int = 2;

/// `SIGQUIT`: ignored by the shell while it waits for a foreground child.
#[cfg(unix)]
pub const SIGQUIT: c_int = libc::SIGQUIT;
/// `SIGQUIT`; see the note on Windows signal numbers.
#[cfg(windows)]
pub const SIGQUIT: c_int = 3;

/// `SIGTERM`, the default signal for `kill`.
#[cfg(unix)]
pub const SIGTERM: c_int = libc::SIGTERM;
/// `SIGTERM`: `TerminateProcess` on Windows.
#[cfg(windows)]
pub const SIGTERM: c_int = 15;

/// `SIGSTOP`: cannot be caught, blocked or ignored.
#[cfg(unix)]
pub const SIGSTOP: c_int = libc::SIGSTOP;
/// `SIGSTOP`; see the note on Windows signal numbers.
#[cfg(windows)]
pub const SIGSTOP: c_int = 19;

/// `SIGTSTP`: the terminal's stop key (`Ctrl-Z`).
#[cfg(unix)]
pub const SIGTSTP: c_int = libc::SIGTSTP;
/// `SIGTSTP`; see the note on Windows signal numbers.
#[cfg(windows)]
pub const SIGTSTP: c_int = 20;

/// `SIGTTIN`: a background process tried to read the terminal.
#[cfg(unix)]
pub const SIGTTIN: c_int = libc::SIGTTIN;
/// `SIGTTIN`; see the note on Windows signal numbers.
#[cfg(windows)]
pub const SIGTTIN: c_int = 21;

/// `SIGTTOU`: a background process tried to write the terminal or change its
/// foreground group.
#[cfg(unix)]
pub const SIGTTOU: c_int = libc::SIGTTOU;
/// `SIGTTOU`; see the note on Windows signal numbers.
#[cfg(windows)]
pub const SIGTTOU: c_int = 22;

/// `SIGCONT`: resume a stopped process group.
#[cfg(unix)]
pub const SIGCONT: c_int = libc::SIGCONT;
/// `SIGCONT`; see the note on Windows signal numbers.
#[cfg(windows)]
pub const SIGCONT: c_int = 18;

/// `SIGCHLD`: a child changed state (a wakeup hint, never a per-child event).
#[cfg(unix)]
pub const SIGCHLD: c_int = libc::SIGCHLD;
/// `SIGCHLD`; see the note on Windows signal numbers.
#[cfg(windows)]
pub const SIGCHLD: c_int = 17;

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
