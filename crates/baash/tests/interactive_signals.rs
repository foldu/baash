//! Interactive signal behaviour on a real pseudo-terminal.
//!
//! `Ctrl-C` is a terminal-generated `SIGINT` for the foreground process group.
//! With no job control the shell and its foreground child share the shell's
//! group, so the child dies and the shell must survive the wait and report
//! status 130. This is the surviving part of the old job-control pty test: the
//! `^Z`/`jobs`/`fg`/`bg` half was deleted with job control.

#![forbid(unsafe_code)]

use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

use baash_sys::{
    Pty, SpawnPlan, become_session_leader, close, dup2, exec, fork_child_status, open_pty,
    poll_readable, read_fd, send_signal, write_fd,
};

/// The prompt the test looks for. Plain ASCII with no prompt escapes.
const PROMPT: &[u8] = b"@@ ";

struct Session {
    pty: Pty,
    child: i32,
    output: Vec<u8>,
    /// Bytes before this offset have already been matched.
    cursor: usize,
    reaped: bool,
}

impl Session {
    fn spawn() -> Self {
        let pty = open_pty().expect("open a pty");
        let master = pty.master.as_raw_fd();
        let slave = pty.slave.as_raw_fd();
        let binary = env!("CARGO_BIN_EXE_baash");

        // A minimal, deterministic environment. `PATH` must include the host's
        // `sleep`; `PS1` is the marker; `TERM=dumb` keeps the editor from
        // painting colour.
        let mut environment: Vec<Vec<u8>> = Vec::new();
        for (key, value) in std::env::vars_os() {
            use std::os::unix::ffi::OsStrExt;
            let key = key.as_os_str().as_bytes();
            if key == b"PS1" || key == b"PS2" || key == b"TERM" || key == b"HISTFILE" {
                // No history database: a test run must not touch the user's.
                continue;
            }
            let mut entry = key.to_vec();
            entry.push(b'=');
            entry.extend_from_slice(value.as_os_str().as_bytes());
            environment.push(entry);
        }
        environment.push(b"PS1=@@ ".to_vec());
        environment.push(b"PS2=>> ".to_vec());
        environment.push(b"TERM=dumb".to_vec());
        environment.push(b"BAASH_HISTORY_DB=".to_vec());

        let child = fork_child_status(move || {
            if become_session_leader(slave).is_err() {
                return 111;
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
                ["baash", "--norc", "-i"],
                environment.iter().map(Vec::as_slice),
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
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
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

    fn prompt(&mut self) {
        self.expect(PROMPT, Duration::from_secs(10));
    }

    fn line(&mut self, text: &str) {
        self.send(text.as_bytes());
        self.send(b"\n");
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

/// Give a freshly started foreground command time to install its signal
/// handling; a `Ctrl-C` sent before that would only be seen by the shell.
fn settle() {
    std::thread::sleep(Duration::from_millis(400));
}

#[test]
fn ctrl_c_on_a_foreground_command_leaves_status_130_and_the_shell_alive() {
    let mut session = Session::spawn();
    session.prompt();

    session.line("sleep 30");
    settle();
    session.send(b"\x03");
    session.prompt();
    session.line("echo status=$?");
    session.expect(b"status=130", Duration::from_secs(10));
    session.prompt();

    session.line("echo still-here");
    session.expect(b"still-here", Duration::from_secs(10));
    session.prompt();

    session.line("exit");
    session.wait_child();
}

/// `Ctrl-C` while the shell itself runs a command must not end the session.
///
/// The editor turns the terminal's keyboard signals off for the line, but a
/// command runs with the terminal back in cooked mode — that is what lets
/// `read` use the line discipline — so the same key is a `SIGINT` for the
/// foreground process group, and that group is the shell's own. With the
/// default disposition the shell died: an in-process command, or the gap
/// between a command's output and the next prompt, lost the whole session
/// (`interactive_carapace::a_fast_enter_waits_for_a_pending_lookup` met the
/// gap and failed about one run in ten).
///
/// The signal is blocked for the life of an interactive shell, so the builtin
/// finishes and the next command still runs. The marker is computed by the
/// shell rather than typed, so the editor's echo of the line cannot match it.
#[test]
fn ctrl_c_during_in_process_shell_code_does_not_end_the_session() {
    let mut session = Session::spawn();
    session.prompt();

    // `read -t` is the only builtin that blocks without a child, so it is the
    // one way to be inside the shell — and not inside the editor — when the
    // key arrives. The timeout ends it two seconds later, which is what the
    // test waits for.
    session.line("read -t 2 ignored; echo done-$((6*7))");
    settle();
    session.send(b"\x03");
    session.expect(b"done-42", Duration::from_secs(15));

    session.line("echo still-here");
    session.expect(b"still-here", Duration::from_secs(10));
    session.prompt();

    session.line("exit");
    session.wait_child();
}
