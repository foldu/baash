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
#[cfg(unix)]
pub mod fd;
pub mod outcome;
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
    lseek, open_cloexec, poll_readable, read_all, read_fd, seek, set_cloexec, write_all, write_fd,
};
pub use outcome::WaitOutcome;
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
/// The descriptor vocabulary, under one set of names on both platforms.
///
/// Unix gets `std`'s types and traits unchanged, so nothing about the Linux
/// build is a wrapper; Windows gets this crate's (see `windows.rs`).
#[cfg(unix)]
pub use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
#[cfg(unix)]
pub use term::{NoEcho, RawMode, parent_pid, pid, window_size, window_size_fd};
#[cfg(unix)]
pub use wait::{wait_any_nohang, wait_pid};

#[cfg(windows)]
pub use windows::{
    AsRawFd, CaptureRanges, Child, Disposition, FdAction, FromRawFd, IntoRawFd, MaskGuard, NoEcho,
    OwnedFd, Pipe, Pty, RawFd, RawMode, Regex, RegexError, SignalEvent, SignalFd, SignalGuard,
    SignalSet, SpawnPlan, become_session_leader, chdir, close, create_unlinked_temp, dup,
    dup_above, dup2, exec, fd_is_cloexec, fd_is_open, fork_child_status, ignore_signals, lseek,
    open_cloexec, open_pty, parent_pid, pid, poll_readable, read_all, read_fd, replace_signal_mask,
    seek, send_signal, set_cloexec, set_disposition, spawn, wait_any_nohang, wait_pid, window_size,
    window_size_fd, write_all, write_fd,
};

// `FdAction::Open` and the signal APIs name C types in their signatures, so a
// consumer would otherwise need its own `libc` dependency just to call us.
// Re-exporting keeps the rule that this crate is the only one depending on the
// platform.
#[cfg(windows)]
pub use crate::windows::mode_t;
pub use libc::c_int;
#[cfg(unix)]
pub use libc::mode_t;

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
    /// Fail when the file already exists (`set -C`).
    pub const EXCL: c_int = libc::O_EXCL;
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

/// `ENOEXEC`: the file is executable but has no recognizable header (no
/// `#!`). A shell is expected to interpret such a file itself.
pub const ENOEXEC: i32 = libc::ENOEXEC;

/// Whether `fd` refers to a terminal.
///
/// Interactive behaviour (colour, the line editor, the terminal REPL) hangs
/// off this one question, so it lives next to the other syscalls.
pub fn is_tty(fd: i32) -> bool {
    // SAFETY: `isatty` only inspects the descriptor and cannot retain it.
    unsafe { libc::isatty(fd) == 1 }
}

/// The machine's hostname, without a trailing newline.
#[cfg(target_os = "linux")]
pub fn hostname() -> Vec<u8> {
    let bytes = std::fs::read("/proc/sys/kernel/hostname").unwrap_or_default();
    match bytes
        .iter()
        .rposition(|&byte| byte != b'\n' && byte != b'\r')
    {
        Some(end) => bytes[..=end].to_vec(),
        None => Vec::new(),
    }
}

/// The hostname from the environment; Windows sets `COMPUTERNAME`.
#[cfg(not(target_os = "linux"))]
pub fn hostname() -> Vec<u8> {
    std::env::var_os("COMPUTERNAME")
        .or_else(|| std::env::var_os("HOSTNAME"))
        .map(|value| os::bytes_from_os(value))
        .unwrap_or_default()
}

/// The resource limits `ulimit` exposes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limit {
    /// `-c`: size of core dumps.
    Core,
    /// `-d`: data segment size.
    Data,
    /// `-f`: file size.
    Fsize,
    /// `-l`: locked memory.
    Memlock,
    /// `-m`: resident set size.
    Rss,
    /// `-n`: open file descriptors.
    Nofile,
    /// `-s`: stack size.
    Stack,
    /// `-t`: CPU time in seconds.
    Cpu,
    /// `-u`: processes per user.
    Processes,
    /// `-v`: address space.
    AddressSpace,
}

/// The soft and hard value of `limit`; `None` means unlimited.
#[cfg(unix)]
pub fn get_limit(limit: Limit) -> Option<(Option<u64>, Option<u64>)> {
    let resource = resource(limit);
    let value = rustix::process::getrlimit(resource);
    Some((value.current, value.maximum))
}

/// The soft and hard value of `limit`; Windows has no `ulimit`.
#[cfg(windows)]
pub fn get_limit(_limit: Limit) -> Option<(Option<u64>, Option<u64>)> {
    None
}

/// Set the soft and hard value of `limit` (`None` is unlimited).
#[cfg(unix)]
pub fn set_limit(limit: Limit, soft: Option<u64>, hard: Option<u64>) -> std::io::Result<()> {
    rustix::process::setrlimit(
        resource(limit),
        rustix::process::Rlimit {
            current: soft,
            maximum: hard,
        },
    )
    .map_err(std::io::Error::from)
}

/// Set a limit; Windows has no `ulimit`.
#[cfg(windows)]
pub fn set_limit(_limit: Limit, _soft: Option<u64>, _hard: Option<u64>) -> std::io::Result<()> {
    Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
}

#[cfg(unix)]
fn resource(limit: Limit) -> rustix::process::Resource {
    use rustix::process::Resource;
    match limit {
        Limit::Core => Resource::Core,
        Limit::Data => Resource::Data,
        Limit::Fsize => Resource::Fsize,
        Limit::Memlock => Resource::Memlock,
        Limit::Rss => Resource::Rss,
        Limit::Nofile => Resource::Nofile,
        Limit::Stack => Resource::Stack,
        Limit::Cpu => Resource::Cpu,
        Limit::Processes => Resource::Nproc,
        Limit::AddressSpace => Resource::As,
    }
}

/// Sets the process mask to `mask`, returning the previous value.
///
/// The mask is process-wide, so the `umask` built-in is the only caller and it
/// passes permission bits only. `None` means the platform has no mask
/// (Windows); the built-in reports that as unsupported rather than pretending
/// the call succeeded.
#[cfg(unix)]
pub fn umask(mask: u32) -> Option<u32> {
    // SAFETY: `umask` ignores every bit above the permission bits and takes no
    // pointers, so no argument can make the call unsound.
    unsafe { Some(libc::umask(mask as libc::mode_t) as u32) }
}

/// Sets the process mask; Windows has none. See the Unix `umask`.
#[cfg(windows)]
pub fn umask(_mask: u32) -> Option<u32> {
    None
}

/// Whether the C library knows `name` as an `LC_CTYPE` locale.
///
/// `newlocale` is used instead of `setlocale` so asking the question does not
/// change the process locale the regex engine and globbing depend on.
#[cfg(unix)]
pub fn locale_exists(name: &[u8]) -> bool {
    let Ok(name) = std::ffi::CString::new(name) else {
        return false;
    };
    // SAFETY: `name` is a valid NUL-terminated string, the base locale is
    // null (the current one is not modified), and `freelocale` is given back
    // exactly the object `newlocale` returned.
    unsafe {
        let locale = libc::newlocale(libc::LC_CTYPE_MASK, name.as_ptr(), std::ptr::null_mut());
        if locale.is_null() {
            false
        } else {
            libc::freelocale(locale);
            true
        }
    }
}

/// Locale existence is a Unix question; Windows accepts the name.
#[cfg(windows)]
pub fn locale_exists(_name: &[u8]) -> bool {
    true
}

/// Paths and OS strings in shell bytes.
///
/// Shell strings are bytes; `OsStr` is only bytes on Unix. Every crate that
/// needs to convert between them goes through here so the Windows encoding
/// question (`docs/portability.md` §9) has one home instead of a `cfg` per
/// call site.
pub mod os {
    use std::ffi::{OsStr, OsString};
    use std::path::PathBuf;

    /// A shell byte string as a path.
    #[cfg(unix)]
    pub fn path_from_bytes(bytes: &[u8]) -> PathBuf {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(OsStr::from_bytes(bytes))
    }

    /// A shell byte string as a path.
    #[cfg(windows)]
    pub fn path_from_bytes(bytes: &[u8]) -> PathBuf {
        PathBuf::from(os_from_bytes(bytes))
    }

    /// A shell byte string as an `OsString`.
    #[cfg(unix)]
    pub fn os_from_bytes(bytes: &[u8]) -> OsString {
        use std::os::unix::ffi::OsStrExt;
        OsStr::from_bytes(bytes).to_os_string()
    }

    /// A shell byte string as an `OsString`.
    ///
    /// Windows names are UTF-16; a byte string that is not UTF-8 cannot be
    /// spelled, so it is replaced rather than rejected — the gap §9 records.
    #[cfg(windows)]
    pub fn os_from_bytes(bytes: &[u8]) -> OsString {
        OsString::from(String::from_utf8_lossy(bytes).into_owned())
    }

    /// An OS string as shell bytes.
    #[cfg(unix)]
    pub fn bytes_from_os(value: impl AsRef<OsStr>) -> Vec<u8> {
        use std::os::unix::ffi::OsStrExt;
        value.as_ref().as_bytes().to_vec()
    }

    /// An OS string as shell bytes.
    #[cfg(windows)]
    pub fn bytes_from_os(value: impl AsRef<OsStr>) -> Vec<u8> {
        value.as_ref().to_string_lossy().into_owned().into_bytes()
    }

    /// This user's id, when the platform has one.
    #[cfg(unix)]
    pub fn own_uid() -> Option<u32> {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata("/proc/self")
            .ok()
            .map(|metadata| metadata.uid())
    }

    /// This user's id; Windows has no uid.
    #[cfg(windows)]
    pub fn own_uid() -> Option<u32> {
        None
    }

    /// The owner of `path`, when the platform has one.
    #[cfg(unix)]
    pub fn owner_uid(path: &std::path::Path) -> Option<u32> {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).ok().map(|metadata| metadata.uid())
    }

    /// The owner of `path`; Windows has no uid.
    #[cfg(windows)]
    pub fn owner_uid(_path: &std::path::Path) -> Option<u32> {
        None
    }
}
