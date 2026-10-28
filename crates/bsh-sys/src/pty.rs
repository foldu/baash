//! Pseudo-terminals: an owned master/slave pair for terminal code.
//!
//! Terminal handling ([`RawMode`](crate::RawMode),
//! [`window_size_fd`](crate::window_size_fd),
//! [`set_foreground_pgrp`](crate::set_foreground_pgrp)) cannot be exercised
//! without a terminal, and the sandbox that produced this crate has no
//! controlling terminal. A pseudo-terminal supplies one anyway: the slave is a
//! real tty with real termios state, window size and foreground process group,
//! and the master is the other end of the line discipline that a test (or the
//! line editor) can read and write.
//!
//! # Ownership and flags
//!
//! Both descriptors are [`OwnedFd`]s, so dropping the [`Pty`] closes both ends.
//! They are close-on-exec, because they are the shell's plumbing and must not
//! leak into commands (DESIGN.md §4.3). The **master is non-blocking**, which
//! is what a poll-driven editor needs: a read that would block returns
//! `EAGAIN` instead of wedging the event loop.
//!
//! The slave is opened with `O_NOCTTY`, so opening a pty never silently makes
//! it the caller's controlling terminal. A caller that wants job-control
//! semantics must acquire it deliberately in its own session (`setsid(2)`,
//! then `TIOCSCTTY`), as the pty tests do.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::fd::{open_cloexec, set_cloexec};

/// An open pseudo-terminal pair.
#[derive(Debug)]
pub struct Pty {
    /// The master side, non-blocking and close-on-exec.
    ///
    /// Reads and writes exercise the line discipline; the slave sees the
    /// processed bytes.
    pub master: OwnedFd,
    /// The slave side, close-on-exec, opened with `O_NOCTTY`.
    ///
    /// This is a real terminal: `tcgetattr`/`tcsetattr`, `TIOCGWINSZ` and
    /// `tcsetpgrp` all apply to it.
    pub slave: OwnedFd,
}

/// Open a new pseudo-terminal pair.
///
/// Uses `posix_openpt(3)`, `grantpt(3)`, `unlockpt(3)` and `ptsname(3)`; the
/// master is then set non-blocking. The slave is opened separately so that it
/// can be close-on-exec and `O_NOCTTY` like every other descriptor the shell
/// owns.
///
/// If any step fails, everything opened so far is closed before the error is
/// returned.
pub fn open_pty() -> io::Result<Pty> {
    // `posix_openpt` is defined to accept the open flags; `O_CLOEXEC` is
    // supported by Linux's implementation, while other hosts only guarantee
    // the basic ones. Setting it afterwards is harmless on Linux and the
    // safety net elsewhere.
    #[cfg(target_os = "linux")]
    let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = libc::O_RDWR | libc::O_NOCTTY;

    // SAFETY: `posix_openpt` takes only integer flags and returns a new
    // descriptor or -1; it touches no user pointers.
    let master = unsafe { libc::posix_openpt(flags) };
    if master < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor was just created and is not owned anywhere else;
    // wrapping it here closes it on every error path below.
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let master_fd = master.as_raw_fd();
    set_cloexec(master_fd, true)?;

    // SAFETY: both calls take a live pty master descriptor and no pointers.
    // `grantpt` is a no-op on devpts and `unlockpt` must precede opening the
    // slave.
    if unsafe { libc::grantpt(master_fd) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::unlockpt(master_fd) } != 0 {
        return Err(io::Error::last_os_error());
    }

    let name = slave_name(master_fd)?;
    let slave = open_cloexec(&name, libc::O_RDWR | libc::O_NOCTTY, 0)?;

    set_nonblocking(master_fd, true)?;
    Ok(Pty { master, slave })
}

/// The device path of the slave side of `master`.
#[cfg(target_os = "linux")]
fn slave_name(master: RawFd) -> io::Result<Vec<u8>> {
    let mut buffer = vec![0u8; 256];
    // SAFETY: `buffer` is valid for writes of `buffer.len()` bytes; `ptsname_r`
    // writes a NUL-terminated path and does not retain the pointer. It returns
    // an errno number rather than setting errno.
    let rc = unsafe {
        libc::ptsname_r(
            master,
            buffer.as_mut_ptr().cast::<libc::c_char>(),
            buffer.len(),
        )
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    let length = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    buffer.truncate(length);
    Ok(buffer)
}

/// The device path of the slave side of `master`.
///
/// On hosts without `ptsname_r`, `ptsname` returns libc-owned static storage,
/// so concurrent `open_pty` calls are not safe there. Linux (the primary
/// target) uses the reentrant `ptsname_r`.
#[cfg(not(target_os = "linux"))]
fn slave_name(master: RawFd) -> io::Result<Vec<u8>> {
    // SAFETY: `ptsname` returns either null or a pointer to a NUL-terminated
    // string in libc-owned storage; the bytes are copied immediately and the
    // pointer is not retained.
    let name = unsafe { libc::ptsname(master) };
    if name.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: non-null, NUL-terminated, and copied before any other libc call
    // could invalidate it.
    Ok(unsafe { std::ffi::CStr::from_ptr(name) }
        .to_bytes()
        .to_vec())
}

/// Make the calling process a session leader and acquire `fd` as its
/// controlling terminal.
///
/// This is the `setsid(2)` + `TIOCSCTTY` pair a pty test needs before it can
/// exercise `tcsetpgrp`: a terminal that is not the session's controlling
/// terminal has no foreground process group to set. `fd` must be the slave
/// side of a pty. The caller normally does this in the child of a `fork`
/// immediately before `exec`.
pub fn become_session_leader(fd: RawFd) -> io::Result<()> {
    // SAFETY: `setsid` takes no arguments and cannot fail by dereferencing
    // anything; it only fails when the caller is already a process-group
    // leader, which the error reports.
    if unsafe { libc::setsid() } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `TIOCSCTTY` takes an integer flag (0: steal the terminal) and
    // touches no user pointer. `fd` is a live terminal descriptor.
    if unsafe { libc::ioctl(fd, libc::TIOCSCTTY as _, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Set or clear `O_NONBLOCK` without disturbing the other status flags.
fn set_nonblocking(fd: RawFd, enabled: bool) -> io::Result<()> {
    // SAFETY: `F_GETFL` only queries this descriptor's status flags.
    let current = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if current < 0 {
        return Err(io::Error::last_os_error());
    }
    let flags = if enabled {
        current | libc::O_NONBLOCK
    } else {
        current & !libc::O_NONBLOCK
    };
    // SAFETY: `F_SETFL` only updates this descriptor's status flags.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
