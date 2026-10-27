//! Terminal state: raw mode, window size, and the foreground process group.
//!
//! Job control (DESIGN.md §7) needs `tcsetpgrp` to be safe while the shell
//! itself may be in the background. A background process group that calls
//! `tcsetpgrp` receives `SIGTTOU`, whose default action stops the shell, so
//! [`set_foreground_pgrp`] blocks that signal across the call.

use std::io;
use std::mem::MaybeUninit;
use std::os::fd::RawFd;

use crate::signal::SignalSet;

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

/// Give the terminal on standard input to process group `pgroup`.
///
/// See [`set_foreground_pgrp_fd`]. Interactive shells call this with the
/// input's controlling terminal; use the `_fd` variant when standard input is
/// redirected.
pub fn set_foreground_pgrp(pgroup: libc::pid_t) -> io::Result<()> {
    set_foreground_pgrp_fd(libc::STDIN_FILENO, pgroup)
}

/// Give the terminal on `fd` to process group `pgroup`.
///
/// `SIGTTOU` is blocked for the duration so that a background shell gets an
/// error from the kernel instead of being stopped by the default signal
/// action. The previous mask is restored on return, including on error. Like
/// the rest of the fork-gated shell, this assumes no helper thread will receive
/// the signal on the caller's behalf.
pub fn set_foreground_pgrp_fd(fd: RawFd, pgroup: libc::pid_t) -> io::Result<()> {
    let ttou = SignalSet::of(&[libc::SIGTTOU]);
    let _blocked = ttou.block()?;
    // SAFETY: `tcsetpgrp` takes a descriptor and a process-group id and
    // dereferences no user pointer.
    if unsafe { libc::tcsetpgrp(fd, pgroup) } != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// The process group that currently owns the terminal on `fd`.
pub fn get_foreground_pgrp(fd: RawFd) -> io::Result<libc::pid_t> {
    // SAFETY: `tcgetpgrp` only reads terminal state for the descriptor.
    let pgroup = unsafe { libc::tcgetpgrp(fd) };
    if pgroup < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(pgroup)
    }
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

/// This process's process-group id.
pub fn process_group() -> libc::pid_t {
    // SAFETY: `getpgrp` cannot fail and touches no pointers.
    unsafe { libc::getpgrp() }
}

/// Put process `pid` into process group `pgroup`.
///
/// `pid == 0` means the calling process; `pgroup == 0` means "create a new
/// group led by `pid`". Both the parent and the child call this after `fork`
/// in the classic job-control dance, because either may get there first.
pub fn set_process_group(pid: libc::pid_t, pgroup: libc::pid_t) -> io::Result<()> {
    // SAFETY: `setpgid` takes only integers.
    if unsafe { libc::setpgid(pid, pgroup) } != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
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
        // `getppid` and `getpgrp` can report 0 in an isolated PID namespace
        // (some CI sandboxes do this), so only the sign is asserted.
        assert!(parent_pid() >= 0);
        assert!(process_group() >= 0);
    }
}
