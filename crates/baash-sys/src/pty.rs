//! Pseudo-terminals: an owned master/slave pair for terminal code.
//!
//! Terminal handling ([`RawMode`](crate::RawMode),
//! [`window_size_fd`](crate::window_size_fd)) cannot be exercised without a
//! terminal, and the sandbox that produced this crate has no controlling
//! terminal. A pseudo-terminal supplies one anyway: the slave is a real tty
//! with real termios state and window size, and the master is the other end of
//! the line discipline that a test (or the line editor) can read and write.
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
//! it the caller's controlling terminal. A caller that wants a real
//! interactive terminal must acquire it deliberately in its own session
//! (`setsid(2)`, then `TIOCSCTTY`), as the pty tests do.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use rustix::pty::OpenptFlags;

use crate::fd::{borrowed, open_cloexec, set_cloexec};

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
    /// This is a real terminal: `tcgetattr`/`tcsetattr` and `TIOCGWINSZ`
    /// apply to it.
    pub slave: OwnedFd,
}

/// Open a new pseudo-terminal pair.
///
/// `posix_openpt(3)`, `grantpt(3)`, `unlockpt(3)` and `ptsname(3)` supply the
/// pair; the master is then set non-blocking. The slave is opened separately so
/// that it can be close-on-exec and `O_NOCTTY` like every other descriptor the
/// shell owns.
///
/// If any step fails, everything opened so far is closed before the error is
/// returned.
pub fn open_pty() -> io::Result<Pty> {
    // `O_CLOEXEC` is not portable through `posix_openpt`, and setting it
    // afterwards is harmless, so it is not requested up front.
    let master = rustix::pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY)?;
    let master_fd = master.as_raw_fd();
    set_cloexec(master_fd, true)?;

    // `grantpt` is a no-op on devpts and `unlockpt` must precede opening the
    // slave.
    rustix::pty::grantpt(borrowed(master_fd))?;
    rustix::pty::unlockpt(borrowed(master_fd))?;

    let name = slave_name(master_fd)?;
    let slave = open_cloexec(&name, libc::O_RDWR | libc::O_NOCTTY, 0)?;

    rustix::io::ioctl_fionbio(borrowed(master_fd), true)?;
    Ok(Pty { master, slave })
}

/// The device path of the slave side of `master`, as bytes.
fn slave_name(master: RawFd) -> io::Result<Vec<u8>> {
    let name = rustix::pty::ptsname(borrowed(master), Vec::with_capacity(256))?;
    Ok(name.into_bytes())
}

/// Make the calling process a session leader and acquire `fd` as its
/// controlling terminal.
///
/// This is the `setsid(2)` + `TIOCSCTTY` pair a pty test needs before the
/// terminal can deliver `Ctrl-C` to the test's shell. `fd` must be the slave
/// side of a pty. The caller normally does this in the child of a `fork`
/// immediately before `exec`.
pub fn become_session_leader(fd: RawFd) -> io::Result<()> {
    rustix::process::setsid()?;
    Ok(rustix::process::ioctl_tiocsctty(borrowed(fd))?)
}
