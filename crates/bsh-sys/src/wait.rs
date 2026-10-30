//! Waiting for children.
//!
//! `waitpid` may be interrupted, and a shell must not treat `EINTR` as an exit.
//! The event loop uses the non-blocking form: a readable signal descriptor (or
//! any other wakeup) means "reap whatever is ready", because `SIGCHLD` is a
//! hint and not a per-child event (DESIGN.md §4.5).

use std::io;

/// What happened to a child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    /// The child exited normally with this status.
    Exited(i32),
    /// The child was killed by this signal.
    Signaled(i32),
}

impl WaitOutcome {
    /// The status a shell stores in `$?`.
    ///
    /// A signalled process yields `128 + signal`, which is what POSIX shells
    /// report. An ordinary exit keeps its status (the VM masks it to 8 bits
    /// when expanding `$?`).
    pub fn exit_status(self) -> i32 {
        match self {
            Self::Exited(status) => status,
            Self::Signaled(signal) => 128 + signal,
        }
    }
}

/// Wait for one child, retrying when the wait is interrupted.
///
/// There is no `WUNTRACED`: the shell has no job control, so a stopped child
/// is not a state it knows how to resume.
pub fn wait_pid(pid: libc::pid_t) -> io::Result<WaitOutcome> {
    let mut status: libc::c_int = 0;
    loop {
        // SAFETY: `status` is valid for writes and `waitpid` does not retain
        // the pointer. `pid` is a child of this process by contract.
        let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
        if rc < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if rc == 0 {
            // Not possible without WNOHANG; avoid spinning if a platform
            // disagrees.
            return Err(io::Error::from_raw_os_error(libc::ECHILD));
        }
        return decode(status);
    }
}

/// Reap any child that has changed state, without blocking.
///
/// Returns `None` when no child is ready, which includes "this shell has no
/// children at all". Interrupted waits are retried; `ECHILD` is reported as
/// `None` because it is the steady state of a shell between commands.
pub fn wait_any_nohang() -> Option<(libc::pid_t, WaitOutcome)> {
    let mut status: libc::c_int = 0;
    loop {
        // SAFETY: `status` is valid for writes and `waitpid` does not retain
        // the pointer.
        let rc = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if rc > 0 {
            return decode(status).ok().map(|outcome| (rc, outcome));
        }
        if rc == 0 {
            return None;
        }
        let raw = io::Error::last_os_error().raw_os_error();
        match raw {
            Some(libc::EINTR) => continue,
            // ECHILD: no unreaped children. Every other error means the same
            // thing for an event loop: nothing to report right now.
            _ => return None,
        }
    }
}

fn decode(status: libc::c_int) -> io::Result<WaitOutcome> {
    if libc::WIFEXITED(status) {
        Ok(WaitOutcome::Exited(libc::WEXITSTATUS(status)))
    } else if libc::WIFSIGNALED(status) {
        Ok(WaitOutcome::Signaled(libc::WTERMSIG(status)))
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
