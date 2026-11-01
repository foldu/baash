//! Waiting for children.
//!
//! `waitpid` may be interrupted, and a shell must not treat `EINTR` as an exit.
//! The event loop uses the non-blocking form: a readable signal descriptor (or
//! any other wakeup) means "reap whatever is ready", because `SIGCHLD` is a
//! hint and not a per-child event (DESIGN.md §4.5).
//!
//! `rustix` decodes the wait status, so this module never interprets a `status`
//! word by hand.

use std::io;

use rustix::process::{Pid, WaitOptions, WaitStatus};

use crate::WaitOutcome;

/// Wait for one child, retrying when the wait is interrupted.
///
/// There is no `WUNTRACED`: the shell has no job control, so a stopped child
/// is not a state it knows how to resume.
pub fn wait_pid(pid: libc::pid_t) -> io::Result<WaitOutcome> {
    let pid = Pid::from_raw(pid).ok_or_else(|| io::Error::from_raw_os_error(libc::ECHILD))?;
    loop {
        match rustix::process::waitpid(Some(pid), WaitOptions::empty()) {
            Ok(Some((_, status))) => return decode(status),
            // Without `WNOHANG` a `None` means the child is not waitable; POSIX
            // reports that as `ECHILD`, which is what `wait PID` turns into
            // status 127.
            Ok(None) => return Err(io::Error::from_raw_os_error(libc::ECHILD)),
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

/// Reap any child that has changed state, without blocking.
///
/// Returns `None` when no child is ready, which includes "this shell has no
/// children at all". Interrupted waits are retried; `ECHILD` is reported as
/// `None` because it is the steady state of a shell between commands.
pub fn wait_any_nohang() -> Option<(libc::pid_t, WaitOutcome)> {
    loop {
        match rustix::process::waitpid(None, WaitOptions::NOHANG) {
            Ok(Some((pid, status))) => {
                return decode(status)
                    .ok()
                    .map(|outcome| (pid.as_raw_pid(), outcome));
            }
            Ok(None) => return None,
            Err(rustix::io::Errno::INTR) => continue,
            // ECHILD: no unreaped children. Every other error means the same
            // thing for an event loop: nothing to report right now.
            Err(_) => return None,
        }
    }
}

fn decode(status: WaitStatus) -> io::Result<WaitOutcome> {
    if status.exited() {
        Ok(WaitOutcome::Exited(status.exit_status().unwrap_or(0)))
    } else if status.signaled() {
        Ok(WaitOutcome::Signaled(
            status.terminating_signal().unwrap_or(0),
        ))
    } else {
        // Neither `WIFSTOPPED` nor `WIFCONTINUED` can appear: the wait never
        // passes `WUNTRACED` or `WCONTINUED`.
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unrecognised wait status",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_status_matches_shell_convention() {
        assert_eq!(WaitOutcome::Exited(42).exit_status(), 42);
        assert_eq!(WaitOutcome::Signaled(libc::SIGTERM).exit_status(), 143);
    }
}
