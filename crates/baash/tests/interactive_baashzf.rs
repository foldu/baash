//! `baashzf` at a real prompt, on a real pseudo-terminal.
//!
//! The picker's matching and its frames are unit-tested in `baash-picker`; what
//! only a real terminal can prove is the wiring: a list piped in, the picker on
//! `/dev/tty` (not on the command's stdout), one line printed back, and the
//! alternate screen left behind however the interaction ends.
//!
//! The pty is the terminal *and* the shell's stdout, so the bytes a test reads
//! are exactly what a user would see: the frames while the picker is up, then
//! the chosen line.

#![forbid(unsafe_code)]

use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use baash_sys::{
    Pty, SpawnPlan, become_session_leader, close, dup2, exec, fork_child_status, open_pty,
    poll_readable, read_fd, send_signal, write_fd,
};

/// One interactive shell on a pty, with a scripted conversation.
struct Session {
    pty: Pty,
    child: i32,
    output: Vec<u8>,
    cursor: usize,
    reaped: bool,
}

/// Environment entries the harness removes before starting the shell.
const SCRUBBED: &[&[u8]] = &[
    b"PS1",
    b"PS2",
    b"PROMPT_COMMAND",
    b"TERM",
    b"HOME",
    b"BAASH_HISTORY_DB",
    b"XDG_CONFIG_HOME",
    b"XDG_CONFIG_DIRS",
    b"XDG_DATA_HOME",
    b"XDG_CACHE_HOME",
    b"LC_ALL",
    b"PWD",
    b"OLDPWD",
];

impl Session {
    fn spawn(home: &Path, cwd: &Path) -> Self {
        let pty = open_pty().expect("open a pty");
        let master = pty.master.as_raw_fd();
        let slave = pty.slave.as_raw_fd();
        // Wide enough that no frame wraps, so an assertion on a row is an
        // assertion on the picker's own layout.
        baash_sys::set_window_size_fd(master, 200, 50).expect("size the pty");
        let binary = env!("CARGO_BIN_EXE_baash");

        let mut entries: Vec<Vec<u8>> = Vec::new();
        use std::os::unix::ffi::OsStrExt;
        for (key, value) in std::env::vars_os() {
            let key = key.as_os_str().as_bytes();
            if SCRUBBED.contains(&key) {
                continue;
            }
            let mut entry = key.to_vec();
            entry.push(b'=');
            entry.extend_from_slice(value.as_os_str().as_bytes());
            entries.push(entry);
        }
        entries.push(b"TERM=xterm".to_vec());
        entries.push(b"LC_ALL=C".to_vec());
        entries.push(b"PS1=P> ".to_vec());
        // Plain text, so the frames are readable as rows.
        entries.push(b"NO_COLOR=1".to_vec());
        entries.push(b"BAASH_HISTORY_DB=".to_vec());
        let mut home_entry = b"HOME=".to_vec();
        home_entry.extend_from_slice(home.as_os_str().as_bytes());
        entries.push(home_entry);

        let cwd = cwd.to_path_buf();
        let child = fork_child_status(move || {
            if become_session_leader(slave).is_err() {
                return 111;
            }
            if baash_sys::chdir(cwd.as_os_str().as_bytes()).is_err() {
                return 114;
            }
            for fd in 0..=2 {
                if dup2(slave, fd).is_err() {
                    return 112;
                }
            }
            if slave > 2 {
                let _ = close(slave);
            }
            let _ = close(master);
            let plan = match SpawnPlan::new_with_env(
                binary,
                ["baash", "--norc", "-i"].iter().copied(),
                entries.iter().map(Vec::as_slice),
            ) {
                Ok(plan) => plan,
                Err(_) => return 113,
            };
            let mut plan = plan;
            plan.sigmask = Some(baash_sys::SignalSet::empty());
            plan.reset_signals = baash_sys::SignalSet::of(&[baash_sys::SIGPIPE]);
            let _ = exec(plan);
            127
        })
        .expect("fork the interactive shell");

        Self {
            pty,
            child: child.pid(),
            output: Vec::new(),
            cursor: 0,
            reaped: false,
        }
    }

    fn master(&self) -> RawFd {
        self.pty.master.as_raw_fd()
    }

    fn drain(&mut self) {
        while let Ok(true) = poll_readable(self.master(), 0) {
            let mut buffer = [0u8; 4096];
            match read_fd(self.master(), &mut buffer) {
                Ok(0) => break,
                Ok(count) => self.output.extend_from_slice(&buffer[..count]),
                Err(_) => break,
            }
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        let mut written = 0;
        while written < bytes.len() {
            match write_fd(self.master(), &bytes[written..]) {
                Ok(0) => break,
                Ok(count) => written += count,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("cannot write to the pty: {error}"),
            }
        }
    }

    /// Wait until `needle` appears at or after the cursor, advancing past it.
    fn expect(&mut self, needle: &[u8], timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            self.drain();
            if let Some(position) = find(&self.output[self.cursor..], needle) {
                self.cursor += position + needle.len();
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {:?}\n--- output so far ---\n{}",
                    String::from_utf8_lossy(needle),
                    String::from_utf8_lossy(&self.output)
                );
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Forget everything read so far, so a later `expect` cannot match it.
    fn forget(&mut self) {
        self.drain();
        self.output.clear();
        self.cursor = 0;
    }

    fn prompt(&mut self) {
        self.expect(b"P> ", Duration::from_secs(10));
    }

    fn wait_child(&mut self) {
        if self.reaped {
            return;
        }
        let status = baash_sys::wait_pid(self.child).expect("reap the shell");
        self.reaped = true;
        assert_eq!(
            status,
            baash_sys::WaitOutcome::Exited(0),
            "the shell must exit cleanly at `exit`"
        );
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = send_signal(self.child, baash_sys::SIGTERM);
            let _ = baash_sys::wait_pid(self.child);
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn temp_dir(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("baash-baashzf-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("create temp dir");
    path
}

fn session(tag: &str) -> (Session, PathBuf, PathBuf) {
    let home = temp_dir(&format!("{tag}-home"));
    let work = temp_dir(&format!("{tag}-work"));
    let mut session = Session::spawn(&home, &work);
    session.prompt();
    (session, home, work)
}

fn cleanup(session: &mut Session, home: &Path, work: &Path) {
    session.send(b"exit 0\n");
    session.wait_child();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(work);
}

/// A list piped in is filtered and the chosen line is printed.
///
/// The list goes in on standard input and the choice comes out on standard
/// output, while the picker itself paints on the terminal: that split is the
/// whole point of `ls | baashzf`, and a builtin that painted on stdout would
/// put its frames in the pipe.
#[test]
fn a_piped_list_is_filtered_and_the_choice_is_printed() {
    let (mut session, home, work) = session("choose");

    session.send(b"printf 'alpha\\nbeta\\ngamma\\n' | baashzf\n");
    session.expect(b"\x1b[?1049h", Duration::from_secs(10));
    // The list, in the order it arrived, with the first row selected.
    session.expect(b"> alpha", Duration::from_secs(10));
    session.expect(b"  beta", Duration::from_secs(10));
    session.expect(b"3/3", Duration::from_secs(10));

    // A subsequence, not a prefix: b, e, t.
    session.send(b"bet");
    session.expect(b"> beta", Duration::from_secs(10));
    session.expect(b"1/3", Duration::from_secs(10));

    session.send(b"\r");
    session.expect(b"\x1b[?1049l", Duration::from_secs(10));
    session.expect(b"beta\r\n", Duration::from_secs(10));
    session.forget();

    session.send(b"echo status=$?\n");
    session.expect(b"status=0", Duration::from_secs(10));
    session.prompt();
    cleanup(&mut session, &home, &work);
}

/// `Esc` chooses nothing, and the shell reports an interrupted program.
#[test]
fn escape_cancels_with_status_130() {
    let (mut session, home, work) = session("cancel");

    session.send(b"printf 'one\\ntwo\\n' | baashzf\n");
    session.expect(b"\x1b[?1049h", Duration::from_secs(10));
    session.expect(b"> one", Duration::from_secs(10));
    session.send(b"\x1b");
    session.expect(b"\x1b[?1049l", Duration::from_secs(10));
    session.forget();

    // Nothing was printed, only the status says anything happened.
    session.send(b"echo status=$?\n");
    session.expect(b"status=130", Duration::from_secs(10));
    session.prompt();
    cleanup(&mut session, &home, &work);
}

/// `Enter` with no match chooses nothing.
#[test]
fn accepting_with_no_match_prints_nothing_and_fails() {
    let (mut session, home, work) = session("nomatch");

    session.send(b"printf 'one\\ntwo\\n' | baashzf\n");
    session.expect(b"\x1b[?1049h", Duration::from_secs(10));
    session.send(b"zzz");
    session.expect(b"no match", Duration::from_secs(10));
    session.send(b"\r");
    session.expect(b"\x1b[?1049l", Duration::from_secs(10));
    session.forget();

    session.send(b"echo status=$?\n");
    session.expect(b"status=1", Duration::from_secs(10));
    session.prompt();
    cleanup(&mut session, &home, &work);
}

/// `Up`/`Down` move the selection instead of the filter.
#[test]
fn the_arrows_move_the_selection() {
    let (mut session, home, work) = session("arrows");

    session.send(b"printf 'one\\ntwo\\nthree\\n' | baashzf\n");
    session.expect(b"> one", Duration::from_secs(10));
    session.send(b"\x1b[B\x1b[B");
    session.expect(b"> three", Duration::from_secs(10));
    session.send(b"\x1b[A");
    session.expect(b"> two", Duration::from_secs(10));
    session.send(b"\r");
    session.expect(b"\x1b[?1049l", Duration::from_secs(10));
    session.expect(b"two\r\n", Duration::from_secs(10));
    session.forget();
    session.send(b"true\n");
    session.prompt();
    cleanup(&mut session, &home, &work);
}

/// An empty list, and a terminal on standard input, are refused before the
/// screen is taken — a picker over itself would eat the keys it needs.
#[test]
fn it_refuses_an_empty_list_and_a_terminal_on_standard_input() {
    let (mut session, home, work) = session("refuse");

    session.send(b"true | baashzf; echo empty=$?\n");
    session.expect(b"empty=1", Duration::from_secs(10));
    session.prompt();

    session.send(b"baashzf; echo tty=$?\n");
    session.expect(b"standard input is a terminal", Duration::from_secs(10));
    session.expect(b"tty=2", Duration::from_secs(10));
    session.prompt();

    session.send(b"baashzf --help\n");
    session.expect(b"usage: baashzf", Duration::from_secs(10));
    session.prompt();
    cleanup(&mut session, &home, &work);
}
