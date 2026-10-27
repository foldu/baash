//! Terminal code against a real terminal: a pseudo-terminal pair.
//!
//! These are the integration tests the contract said were owed: the sandbox
//! that built `bsh-sys` had no controlling terminal, so `RawMode`,
//! `window_size_fd` and `set_foreground_pgrp` had never touched a tty. A pty
//! slave is a real terminal, so the same kernel paths run here.
//!
//! Where a platform property cannot be assumed, the test sets it explicitly
//! first (termios flags, window size) rather than relying on defaults; where
//! it genuinely cannot be made reliable, the weaker true assertion is used and
//! the comment says so.
//!
//! The foreground-group test creates its own session and acquires its own
//! controlling terminal, so it passes both when the harness has a terminal and
//! when it has none (verified under `setsid`).

use std::io;
use std::os::fd::{AsRawFd, RawFd};

use bsh_sys::{
    RawMode, WaitOutcome, dup2, fork_child_status, get_foreground_pgrp, is_tty, open_pty,
    process_group, send_signal, set_foreground_pgrp, set_foreground_pgrp_fd, wait_pid,
    window_size_fd,
};

/// Read the terminal settings of `fd`, failing the test if it is not a tty.
fn termios(fd: RawFd) -> libc::termios {
    let mut settings = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: `settings` is valid for writes; `tcgetattr` initialises it on
    // success and does not retain the pointer.
    let rc = unsafe { libc::tcgetattr(fd, settings.as_mut_ptr()) };
    assert_eq!(
        rc,
        0,
        "tcgetattr on the pty slave: {}",
        io::Error::last_os_error()
    );
    // SAFETY: success initialised the structure.
    unsafe { settings.assume_init() }
}

/// Install `settings`, which must be a termios obtained from `tcgetattr`.
fn set_termios(fd: RawFd, settings: &libc::termios) {
    // SAFETY: `settings` is an initialised `termios`; `tcsetattr` reads it and
    // retains no pointer.
    let rc = unsafe { libc::tcsetattr(fd, libc::TCSANOW, settings) };
    assert_eq!(
        rc,
        0,
        "tcsetattr on the pty slave: {}",
        io::Error::last_os_error()
    );
}

/// Assert that the termios fields `RawMode` touches match `expected`.
fn assert_same_termios(actual: &libc::termios, expected: &libc::termios) {
    assert_eq!(actual.c_iflag, expected.c_iflag, "input flags");
    assert_eq!(actual.c_oflag, expected.c_oflag, "output flags");
    assert_eq!(actual.c_cflag, expected.c_cflag, "control flags");
    assert_eq!(actual.c_lflag, expected.c_lflag, "local flags");
    assert_eq!(actual.c_cc[libc::VMIN], expected.c_cc[libc::VMIN]);
    assert_eq!(actual.c_cc[libc::VTIME], expected.c_cc[libc::VTIME]);
}

/// A pty slave configured exactly as `canonical`, so raw-mode tests prove a
/// real change instead of relying on the host's terminal defaults.
fn canonical_slave(fd: RawFd) -> libc::termios {
    let mut settings = termios(fd);
    settings.c_lflag |= libc::ICANON | libc::ECHO | libc::ISIG;
    settings.c_iflag |= libc::ICRNL | libc::IXON;
    settings.c_oflag |= libc::OPOST;
    set_termios(fd, &settings);
    termios(fd)
}

#[test]
fn pty_master_is_nonblocking_and_both_ends_are_tty_cloexec_fds() {
    let pty = open_pty().unwrap();
    let master = pty.master.as_raw_fd();
    let slave = pty.slave.as_raw_fd();
    assert_ne!(master, slave);
    assert!(is_tty(slave), "the slave must be a real terminal");

    // SAFETY: `F_GETFL` only queries this descriptor's status flags.
    let flags = unsafe { libc::fcntl(master, libc::F_GETFL) };
    assert!(flags >= 0, "F_GETFL: {}", io::Error::last_os_error());
    assert_ne!(
        flags & libc::O_NONBLOCK,
        0,
        "the master must be non-blocking"
    );

    assert!(bsh_sys::fd_is_cloexec(master));
    assert!(bsh_sys::fd_is_cloexec(slave));
    // The slave is deliberately not made this process's controlling terminal.
    assert!(bsh_sys::fd_is_open(master) && bsh_sys::fd_is_open(slave));
}

#[test]
fn raw_mode_changes_termios_and_drop_restores_it() {
    let pty = open_pty().unwrap();
    let fd = pty.slave.as_raw_fd();
    let saved = canonical_slave(fd);
    assert_ne!(saved.c_lflag & libc::ICANON, 0);
    assert_ne!(saved.c_lflag & libc::ECHO, 0);

    {
        let guard = RawMode::enable(fd).unwrap();
        assert_eq!(guard.fd(), fd);

        let raw = termios(fd);
        assert_eq!(
            raw.c_lflag & (libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN),
            0,
            "raw mode must clear canonical mode, echo, signal generation and IEXTEN"
        );
        assert_eq!(raw.c_oflag & libc::OPOST, 0, "raw mode must clear OPOST");
        assert_eq!(raw.c_iflag & (libc::ICRNL | libc::IXON), 0);
        assert_eq!(raw.c_cflag & libc::CSIZE, libc::CS8);
        assert_eq!(raw.c_cc[libc::VMIN], 1);
        assert_eq!(raw.c_cc[libc::VTIME], 0);
    }

    // Dropping the guard restores what `tcgetattr` saw when it was created,
    // which is the state this test installed above.
    assert_same_termios(&termios(fd), &saved);
}

#[test]
fn explicit_restore_reports_and_restores() {
    let pty = open_pty().unwrap();
    let fd = pty.slave.as_raw_fd();
    let saved = canonical_slave(fd);

    let guard = RawMode::enable(fd).unwrap();
    assert_eq!(termios(fd).c_lflag & libc::ICANON, 0, "enable did nothing");
    guard.restore().unwrap();
    assert_same_termios(&termios(fd), &saved);
}

#[test]
fn window_size_reports_the_size_that_was_set() {
    let pty = open_pty().unwrap();
    let slave = pty.slave.as_raw_fd();

    // Set a distinctive size first: a fresh pty may report 0x0, which
    // `window_size_fd` correctly treats as "no usable size".
    let size = libc::winsize {
        ws_row: 37,
        ws_col: 101,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `TIOCSWINSZ` reads a `winsize` from the pointer and retains
    // nothing.
    let rc = unsafe { libc::ioctl(slave, libc::TIOCSWINSZ, &size) };
    assert_eq!(rc, 0, "TIOCSWINSZ: {}", io::Error::last_os_error());

    assert_eq!(window_size_fd(slave), Some((101, 37)));

    // The master names the same terminal; on Linux it reports the slave's
    // size. This is the weaker of the two assertions (other hosts may answer
    // differently), so a missing answer is tolerated but a wrong one is not.
    if let Some(reported) = window_size_fd(pty.master.as_raw_fd()) {
        assert_eq!(reported, (101, 37));
    }
}

/// The exit statuses the child below uses to name its failing step.
const STEP_SETSID: i32 = 11;
const STEP_ACQUIRE: i32 = 12;
const STEP_STDIN: i32 = 13;
const STEP_FORK: i32 = 14;
const STEP_GRANDCHILD_GROUP: i32 = 15;
const STEP_SET_FOREIGN: i32 = 16;
const STEP_GET_FOREIGN: i32 = 17;
const STEP_SET_OWN_BACKGROUND: i32 = 19;
const STEP_GET_OWN: i32 = 20;

#[test]
fn foreground_pgrp_round_trips_in_a_child_that_owns_the_pty() {
    let pty = open_pty().unwrap();
    let slave = pty.slave.as_raw_fd();

    let child = fork_child_status(move || {
        // The child must become a session leader and then explicitly acquire
        // the pty: the slave was opened with O_NOCTTY, so it is not anybody's
        // controlling terminal yet. Only raw syscalls and `bsh-sys` wrappers
        // that allocate nothing.
        // SAFETY: `setsid` takes no arguments and only affects this process.
        if unsafe { libc::setsid() } < 0 {
            return STEP_SETSID;
        }
        // SAFETY: `TIOCSCTTY` takes an integer argument and acts on the pty.
        // The cast keeps the variadic third argument the width the kernel
        // expects.
        if unsafe { libc::ioctl(slave, libc::TIOCSCTTY, 0 as libc::c_ulong) } != 0 {
            return STEP_ACQUIRE;
        }
        // Exercise the stdin wrapper as well: point fd 0 at the pty.
        if dup2(slave, libc::STDIN_FILENO).is_err() {
            return STEP_STDIN;
        }

        let own = process_group();

        // A grandchild in its own process group gives the terminal a different
        // owner to switch to, so this is a real round trip rather than setting
        // the group that already owns it.
        // SAFETY: `fork` takes no arguments. This child is single-threaded
        // after the outer fork; the grandchild performs raw syscalls only and
        // then blocks in `pause`, never touching the allocator.
        let grandchild = unsafe { libc::fork() };
        if grandchild < 0 {
            return STEP_FORK;
        }
        if grandchild == 0 {
            // SAFETY: `setpgid` takes integers; the child's failure is
            // tolerated because the parent also sets the group.
            unsafe { libc::setpgid(0, 0) };
            loop {
                // SAFETY: `pause` blocks until a signal and touches no memory.
                unsafe { libc::pause() };
            }
        }
        // SAFETY: `setpgid` takes integers. It is legal for both the parent
        // and the child to name the same target group.
        if unsafe { libc::setpgid(grandchild, grandchild) } != 0 {
            return STEP_GRANDCHILD_GROUP;
        }

        if set_foreground_pgrp_fd(slave, grandchild).is_err() {
            return STEP_SET_FOREIGN;
        }
        match get_foreground_pgrp(slave) {
            Ok(observed) if observed == grandchild => {}
            _ => return STEP_GET_FOREIGN,
        }

        // Restoring from the *background* is the case that would stop this
        // process with SIGTTOU if `set_foreground_pgrp` did not block it. That
        // blocking is exactly what this step proves. Force the default action
        // first: the test harness may have inherited SIGTTOU ignored, which
        // would make the restore pass for the wrong reason.
        // SAFETY: `signal(2)` with `SIG_DFL` for a signal number; no pointers.
        unsafe { libc::signal(libc::SIGTTOU, libc::SIG_DFL) };
        if set_foreground_pgrp(own).is_err() {
            return STEP_SET_OWN_BACKGROUND;
        }
        match get_foreground_pgrp(libc::STDIN_FILENO) {
            Ok(observed) if observed == own => {}
            _ => return STEP_GET_OWN,
        }

        // Reap the grandchild so the test leaves no process behind.
        // SAFETY: the grandchild is ours and paused; SIGKILL cannot be caught.
        // `status` is valid for writes and `waitpid` retains no pointer.
        unsafe {
            libc::kill(grandchild, libc::SIGKILL);
            let mut status: libc::c_int = 0;
            libc::waitpid(grandchild, &mut status, 0);
        }
        0
    })
    .unwrap();

    // WUNTRACED so that a regression which lets SIGTTOU stop the child fails
    // the test instead of blocking the reaper forever.
    let outcome = wait_pid(child.pid(), true).unwrap();
    if outcome != WaitOutcome::Exited(0) {
        // Leave nothing behind: a stopped child would otherwise outlive the
        // test binary. SIGKILL reaches a stopped process as well.
        let _ = send_signal(child.pid(), libc::SIGKILL);
        let _ = wait_pid(child.pid(), false);
    }
    assert_eq!(
        outcome,
        WaitOutcome::Exited(0),
        "the pty child failed at step {outcome:?}; see the STEP_* constants"
    );
}
