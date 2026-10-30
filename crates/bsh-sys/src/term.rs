//! Terminal state: raw mode and window size.

use std::io;
use std::mem::MaybeUninit;
use std::os::fd::RawFd;

/// A saved `termios`, restored when the guard is dropped.
///
/// `Drop` is the panic-safe half of the line editor's terminal handling: a
/// panic while editing must not leave the user's terminal in raw mode. The
/// descriptor must stay open and refer to the same terminal for the guard's
/// whole lifetime, or the restore would apply to a different open file.
#[derive(Debug)]
pub struct RawMode {
    fd: RawFd,
    saved: libc::termios,
}

impl RawMode {
    /// Put `fd` into raw mode, returning a guard that restores the previous
    /// settings.
    ///
    /// The exact flags are the ones `cfmakeraw(3)` clears: input translation
    /// and flow control off, output post-processing off, canonical mode, echo,
    /// signal generation and extended input processing off, 8-bit characters,
    /// and one-byte reads (`VMIN = 1`, `VTIME = 0`).
    pub fn enable(fd: RawFd) -> io::Result<Self> {
        let mut saved = MaybeUninit::<libc::termios>::uninit();
        // SAFETY: `saved` is valid for writes; `tcgetattr` does not retain it.
        if unsafe { libc::tcgetattr(fd, saved.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: success initialises `saved`.
        let saved = unsafe { saved.assume_init() };

        let mut raw = saved;
        raw.c_iflag &= !(libc::IGNBRK
            | libc::BRKINT
            | libc::PARMRK
            | libc::ISTRIP
            | libc::INLCR
            | libc::IGNCR
            | libc::ICRNL
            | libc::IXON);
        raw.c_oflag &= !libc::OPOST;
        raw.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN);
        raw.c_cflag &= !(libc::CSIZE | libc::PARENB);
        raw.c_cflag |= libc::CS8;
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;

        // SAFETY: `raw` is an initialised `termios` and `fd` is a terminal (or
        // the call fails); `tcsetattr` does not retain the pointer.
        if unsafe { libc::tcsetattr(fd, libc::TCSADRAIN, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd, saved })
    }

    /// The descriptor this guard controls.
    pub fn fd(&self) -> RawFd {
        self.fd
    }

    /// Restore explicitly, reporting a failure instead of ignoring it.
    pub fn restore(self) -> io::Result<()> {
        let fd = self.fd;
        let saved = self.saved;
        std::mem::forget(self);
        // SAFETY: `saved` came from `tcgetattr` on this descriptor.
        if unsafe { libc::tcsetattr(fd, libc::TCSADRAIN, &saved) } != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: `self.saved` came from `tcgetattr` on `self.fd`. Errors are
        // ignored: a panic must still make a best-effort restore.
        unsafe { libc::tcsetattr(self.fd, libc::TCSADRAIN, &self.saved) };
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
    saved: libc::termios,
}

impl NoEcho {
    /// Turn echo off on `fd`, returning a guard that restores the settings.
    ///
    /// `fd` must be a terminal; on anything else `tcgetattr` fails with
    /// `ENOTTY` and the caller degrades to reading without echo control.
    pub fn enable(fd: RawFd) -> io::Result<Self> {
        let mut saved = MaybeUninit::<libc::termios>::uninit();
        // SAFETY: `saved` is valid for writes; `tcgetattr` does not retain it.
        if unsafe { libc::tcgetattr(fd, saved.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: success initialises `saved`.
        let saved = unsafe { saved.assume_init() };

        let mut quiet = saved;
        quiet.c_lflag &= !(libc::ECHO | libc::ECHONL);

        // SAFETY: `quiet` is an initialised `termios` and `fd` is a terminal
        // (or the call fails); `tcsetattr` does not retain the pointer.
        if unsafe { libc::tcsetattr(fd, libc::TCSADRAIN, &quiet) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd, saved })
    }

    /// The descriptor this guard controls.
    pub fn fd(&self) -> RawFd {
        self.fd
    }

    /// Restore explicitly, reporting a failure instead of ignoring it.
    pub fn restore(self) -> io::Result<()> {
        let fd = self.fd;
        let saved = self.saved;
        std::mem::forget(self);
        // SAFETY: `saved` came from `tcgetattr` on this descriptor.
        if unsafe { libc::tcsetattr(fd, libc::TCSADRAIN, &saved) } != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Drop for NoEcho {
    fn drop(&mut self) {
        // SAFETY: `self.saved` came from `tcgetattr` on `self.fd`. Errors are
        // ignored: the guard's Drop runs on every path, including a panic.
        unsafe { libc::tcsetattr(self.fd, libc::TCSADRAIN, &self.saved) };
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
    // SAFETY: `zeroed` is a valid `winsize`; `TIOCGWINSZ` fills it on success.
    let mut size = MaybeUninit::<libc::winsize>::zeroed();
    // SAFETY: `size` is valid for writes and the ioctl does not retain it.
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, size.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: success means the kernel filled in `size`.
    let size = unsafe { size.assume_init() };
    (size.ws_col > 0 && size.ws_row > 0).then_some((size.ws_col, size.ws_row))
}

/// This process's id (`$$`).
pub fn pid() -> libc::pid_t {
    // SAFETY: `getpid` cannot fail and touches no pointers.
    unsafe { libc::getpid() }
}

/// This process's parent id (`$PPID`).
pub fn parent_pid() -> libc::pid_t {
    // SAFETY: `getppid` cannot fail and touches no pointers.
    unsafe { libc::getppid() }
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
