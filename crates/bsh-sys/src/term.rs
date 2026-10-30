//! Terminal state: raw mode and window size.
//!
//! `rustix` owns the `termios` and `winsize` layouts, so this module only
//! decides *which* settings apply; the two calls that mutate the terminal are
//! `tcsetattr` and nothing else.

use std::io;
use std::os::fd::RawFd;

use rustix::termios::{LocalModes, OptionalActions, Termios};

use crate::fd::borrowed;

/// A saved `termios`, restored when the guard is dropped.
///
/// `Drop` is the panic-safe half of the line editor's terminal handling: a
/// panic while editing must not leave the user's terminal in raw mode. The
/// descriptor must stay open and refer to the same terminal for the guard's
/// whole lifetime, or the restore would apply to a different open file.
#[derive(Debug)]
pub struct RawMode {
    fd: RawFd,
    saved: Termios,
}

impl RawMode {
    /// Put `fd` into raw mode, returning a guard that restores the previous
    /// settings.
    ///
    /// The settings are `cfmakeraw(3)`: input translation and flow control off,
    /// output post-processing off, canonical mode, echo, signal generation and
    /// extended input processing off, 8-bit characters, and one-byte reads
    /// (`VMIN = 1`, `VTIME = 0`).
    pub fn enable(fd: RawFd) -> io::Result<Self> {
        let saved = rustix::termios::tcgetattr(borrowed(fd))?;
        let mut raw = saved.clone();
        raw.make_raw();
        rustix::termios::tcsetattr(borrowed(fd), OptionalActions::Drain, &raw)?;
        Ok(Self { fd, saved })
    }

    /// The descriptor this guard controls.
    pub fn fd(&self) -> RawFd {
        self.fd
    }

    /// Restore explicitly, reporting a failure instead of ignoring it.
    pub fn restore(self) -> io::Result<()> {
        // `Termios` is not `Copy` and the guard implements `Drop`, so take a
        // reference out of the forgotten value instead of moving it.
        let this = std::mem::ManuallyDrop::new(self);
        Ok(rustix::termios::tcsetattr(
            borrowed(this.fd),
            OptionalActions::Drain,
            &this.saved,
        )?)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // Errors are ignored: a panic must still make a best-effort restore.
        let _ = rustix::termios::tcsetattr(borrowed(self.fd), OptionalActions::Drain, &self.saved);
    }
}

/// A saved `termios` with echo turned off, restored when the guard is dropped.
///
/// This is `read -s`: the terminal stays in canonical mode (line editing and
/// signal generation keep working) but the typed characters are not echoed.
/// The full raw mode [`RawMode`] would also disable canonical mode, which is
/// wrong for a line read. As with `RawMode`, the descriptor must keep referring
/// to the same terminal for the guard's lifetime.
#[derive(Debug)]
pub struct NoEcho {
    fd: RawFd,
    saved: Termios,
}

impl NoEcho {
    /// Turn echo off on `fd`, returning a guard that restores the settings.
    ///
    /// `fd` must be a terminal; on anything else `tcgetattr` fails with
    /// `ENOTTY` and the caller degrades to reading without echo control.
    pub fn enable(fd: RawFd) -> io::Result<Self> {
        let saved = rustix::termios::tcgetattr(borrowed(fd))?;
        let mut quiet = saved.clone();
        quiet.local_modes -= LocalModes::ECHO | LocalModes::ECHONL;
        rustix::termios::tcsetattr(borrowed(fd), OptionalActions::Drain, &quiet)?;
        Ok(Self { fd, saved })
    }

    /// The descriptor this guard controls.
    pub fn fd(&self) -> RawFd {
        self.fd
    }

    /// Restore explicitly, reporting a failure instead of ignoring it.
    pub fn restore(self) -> io::Result<()> {
        // `Termios` is not `Copy` and the guard implements `Drop`, so take a
        // reference out of the forgotten value instead of moving it.
        let this = std::mem::ManuallyDrop::new(self);
        Ok(rustix::termios::tcsetattr(
            borrowed(this.fd),
            OptionalActions::Drain,
            &this.saved,
        )?)
    }
}

impl Drop for NoEcho {
    fn drop(&mut self) {
        // Errors are ignored: the guard's Drop runs on every path, including a
        // panic.
        let _ = rustix::termios::tcsetattr(borrowed(self.fd), OptionalActions::Drain, &self.saved);
    }
}

/// The terminal size of standard output, if it is a terminal.
///
/// Returns `(columns, rows)`. A zero in either dimension is treated as "not a
/// terminal" because the kernel reports zero for a pty whose size has not been
/// set yet.
pub fn window_size() -> Option<(u16, u16)> {
    window_size_fd(libc::STDOUT_FILENO)
}

/// The terminal size of `fd`, if it is a terminal.
///
/// See [`window_size`] for the meaning of the returned pair.
pub fn window_size_fd(fd: RawFd) -> Option<(u16, u16)> {
    let size = rustix::termios::tcgetwinsize(borrowed(fd)).ok()?;
    (size.ws_col > 0 && size.ws_row > 0).then_some((size.ws_col, size.ws_row))
}

/// This process's id (`$$`).
pub fn pid() -> libc::pid_t {
    rustix::process::getpid().as_raw_pid()
}

/// This process's parent id (`$PPID`).
pub fn parent_pid() -> libc::pid_t {
    rustix::process::getppid().map_or(0, |pid| pid.as_raw_pid())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_terminal_fd_has_no_window_size() {
        // A deliberately invalid descriptor; ioctl fails and we report "no
        // terminal" rather than inventing a size.
        assert_eq!(window_size_fd(i32::MAX), None);
    }

    #[test]
    fn ids_are_plausible() {
        assert!(pid() > 0);
        // `getppid` can report 0 in an isolated PID namespace (some CI
        // sandboxes do this), so only the sign is asserted.
        assert!(parent_pid() >= 0);
    }
}
