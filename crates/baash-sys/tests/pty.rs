//! Terminal code against a real terminal: a pseudo-terminal pair.
//!
//! These are the integration tests the contract said were owed: the sandbox
//! that built `baash-sys` had no controlling terminal, so `RawMode` and
//! `window_size_fd` had never touched a tty. A pty slave is a real terminal,
//! so the same kernel paths run here.
//!
//! Where a platform property cannot be assumed, the test sets it explicitly
//! first (termios flags, window size) rather than relying on defaults; where
//! it genuinely cannot be made reliable, the weaker true assertion is used and
//! the comment says so.
//!
use std::io;
use std::os::fd::{AsRawFd, RawFd};

use baash_sys::{NoEcho, RawMode, is_tty, open_pty, window_size_fd};

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

    assert!(baash_sys::fd_is_cloexec(master));
    assert!(baash_sys::fd_is_cloexec(slave));
    // The slave is deliberately not made this process's controlling terminal.
    assert!(baash_sys::fd_is_open(master) && baash_sys::fd_is_open(slave));
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

#[test]
fn no_echo_clears_echo_only_and_restores_it() {
    // `read -s` turns echo off but must leave canonical mode alone: disabling
    // canonical mode would also stop line editing and signal generation, which
    // is what `RawMode` is for.
    let pty = open_pty().unwrap();
    let slave = pty.slave.as_raw_fd();
    let before = canonical_slave(slave);
    assert_ne!(before.c_lflag & libc::ECHO, 0, "test starts with echo on");

    {
        let guard = NoEcho::enable(slave).expect("disable echo");
        let quiet = termios(slave);
        assert_eq!(quiet.c_lflag & (libc::ECHO | libc::ECHONL), 0, "echo off");
        assert_ne!(quiet.c_lflag & libc::ICANON, 0, "canonical mode is kept");
        assert_ne!(quiet.c_lflag & libc::ISIG, 0, "signal generation is kept");
        guard.restore().expect("explicit restore");
    }
    assert_same_termios(&termios(slave), &before);

    // Dropping the guard restores too, including on an early return.
    {
        let _guard = NoEcho::enable(slave).expect("disable echo");
    }
    assert_same_termios(&termios(slave), &before);
}
