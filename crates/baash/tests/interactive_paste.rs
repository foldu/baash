//! Bracketed paste at a real prompt, on a real pseudo-terminal.
//!
//! The unit tests in `baash-repl` prove the quoting rule; this file proves the
//! product behaviour: pasting a URL at a prompt runs the command with the URL as
//! *one argument*, and the guess is reversible from the keyboard.
//!
//! The pty sends what a terminal sends — `ESC[200~`, the bytes, `ESC[201~` — so
//! the paste path under test is the shipped one, decoder included.

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
///
/// The prompt and the history must come from the test, not from whoever ran
/// `cargo test`; `PATH` is kept so `printf` and friends resolve.
const SCRUBBED: &[&[u8]] = &[
    b"PS1",
    b"PS2",
    b"PROMPT_COMMAND",
    b"TERM",
    b"HOME",
    b"HISTFILE",
    b"HISTCONTROL",
    b"HISTSIZE",
    b"HISTFILESIZE",
    b"HISTIGNORE",
    b"BAASH_HISTORY_DB",
    b"XDG_CONFIG_HOME",
    b"XDG_CONFIG_DIRS",
    b"XDG_DATA_HOME",
    b"XDG_CACHE_HOME",
    b"BAASH_BASH_COMPLETION",
    b"BAASH_EMBEDDED_BASH_COMPLETION",
    b"BAASH_NO_BASH_COMPLETION",
    b"LC_ALL",
    b"NO_COLOR",
    b"PWD",
    b"OLDPWD",
];

impl Session {
    fn spawn(home: &Path, cwd: &Path) -> Self {
        let pty = open_pty().expect("open a pty");
        let master = pty.master.as_raw_fd();
        let slave = pty.slave.as_raw_fd();
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
        // `dumb` keeps the editor out of colour and out of the cursor-position
        // query, so every byte this test reads is the paste path's.
        entries.push(b"TERM=dumb".to_vec());
        entries.push(b"LC_ALL=C".to_vec());
        entries.push(b"PS1=P> ".to_vec());
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

    /// Send `payload` the way a terminal brackets a paste.
    fn paste(&mut self, payload: &[u8]) {
        let mut bytes = b"\x1b[200~".to_vec();
        bytes.extend_from_slice(payload);
        bytes.extend_from_slice(b"\x1b[201~");
        self.send(&bytes);
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
    let path = std::env::temp_dir().join(format!("baash-paste-{}-{tag}", std::process::id()));
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

/// A pasted URL is one argument, so the `&` in its query string is data.
///
/// Typed, the same text runs `printf` in the background and leaves `t=10` as an
/// assignment; the old behaviour on this line was no output at all plus
/// `BAASH-E0302`. The test prints each argument in angle brackets, so two
/// arguments would show as two pairs.
#[test]
fn a_pasted_url_reaches_the_command_as_one_argument() {
    let (mut session, home, work) = session("url");

    session.send(b"printf '<%s>\\n' ");
    session.paste(b"https://www.youtube.com/watch?v=abc&t=10");
    session.send(b"\n");
    session.expect(
        b"<https://www.youtube.com/watch?v=abc&t=10>\r\n",
        Duration::from_secs(10),
    );
    session.drain();
    let output = String::from_utf8_lossy(&session.output).into_owned();
    assert!(
        !output.contains("BAASH-E0302"),
        "the `&` must not read as a background operator: {output}"
    );

    cleanup(&mut session, &home, &work);
}

/// A pasted command is still a command: the paste rule leaves whitespace alone.
#[test]
fn a_pasted_pipeline_still_runs_as_a_pipeline() {
    let (mut session, home, work) = session("pipeline");

    session.paste(b"printf '%s\\n' one | tr a-z A-Z");
    session.send(b"\n");
    session.expect(b"ONE\r\n", Duration::from_secs(10));

    cleanup(&mut session, &home, &work);
}

/// The guess is reversible from the keyboard: one `Backspace` takes the quotes
/// off, and the pasted text is left as it was typed.
///
/// A path with a space is the visible proof: one quoted argument before the
/// `Backspace`, two words after it.
#[test]
fn a_backspace_removes_the_quotes_the_paste_added() {
    let (mut session, home, work) = session("unquote");

    session.send(b"printf '<%s>\\n' ");
    session.paste(b"/tmp/paste x");
    // The line holds `printf '<%s>\n' '/tmp/paste x'`: one argument. The
    // Backspace takes the pair off, and the same text is now two words.
    session.send(b"\x7f\n");
    session.expect(b"</tmp/paste>\r\n<x>\r\n", Duration::from_secs(10));

    cleanup(&mut session, &home, &work);
}

/// `Ctrl-Z` is one undo step per paste, so an unacceptable guess costs one key.
#[test]
fn ctrl_z_takes_a_paste_back() {
    let (mut session, home, work) = session("undo");

    session.send(b"printf '<%s>\\n' ");
    session.paste(b"a&b");
    session.send(b"\x1a");
    // The line is back to `printf '<%s>\n' `; type the argument by hand.
    session.send(b"plain\n");
    session.expect(b"<plain>\r\n", Duration::from_secs(10));

    cleanup(&mut session, &home, &work);
}

/// A pasted block waits for `Enter`, and its rows are a multiline edit.
///
/// Pasting a script must not run it: the newlines are buffer rows (the terminal
/// brackets the paste, so none of them is Enter), and the editor shows the
/// block with the continuation prompt in front of each inner line.
#[test]
fn a_pasted_block_waits_for_enter() {
    let (mut session, home, work) = session("block");

    // The *output* cannot be mistaken for the echo of the line: the echoed
    // text holds the arithmetic, the result only exists once the command ran.
    session.paste(b"echo $((6*7))\necho $((7*7))");
    // Give the shell every chance to run it without being asked.
    std::thread::sleep(Duration::from_millis(500));
    session.drain();
    let before = String::from_utf8_lossy(&session.output).into_owned();
    assert!(
        !before.contains("42") && !before.contains("49"),
        "nothing runs before Enter: {before}"
    );
    assert!(
        before.contains("> echo $((7*7))"),
        "the block is painted with the continuation prompt: {before}"
    );

    session.send(b"\n");
    session.expect(b"42\r\n49\r\n", Duration::from_secs(10));

    cleanup(&mut session, &home, &work);
}

/// A block copied from a browser carries `\r\n`; those must be rows, not `^M`
/// bytes inside the arguments.
#[test]
fn a_crlf_block_runs_without_carriage_returns() {
    let (mut session, home, work) = session("crlf");

    session.paste(b"printf '<%s>\\n' one\r\nprintf '<%s>\\n' two\r\n");
    session.send(b"\n");
    session.expect(b"<one>\r\n<two>\r\n", Duration::from_secs(10));
    session.drain();
    let output = String::from_utf8_lossy(&session.output).into_owned();
    assert!(
        !output.contains("one\r\r"),
        "no carriage return reaches the argument: {output:?}"
    );

    cleanup(&mut session, &home, &work);
}
