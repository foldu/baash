//! Interactive job control on a real pseudo-terminal.
//!
//! `bsh -i` is spawned on a pty slave that is its controlling terminal, so the
//! kernel's job-control paths are the real ones: `Ctrl-Z` sends `SIGTSTP` to
//! the terminal's foreground process group, `tcsetpgrp` decides which group
//! that is, and a stopped child is visible to `waitpid(WUNTRACED)`.
//!
//! The assertions are on the prompt's own output (`Stopped`, `Running`,
//! `$?`), not on internals: this is the user-visible contract the wave is for.

#![forbid(unsafe_code)]

use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

use bsh_sys::{
    Pty, SpawnPlan, become_session_leader, close, dup2, exec, fork_child_status, open_pty,
    poll_readable, read_fd, send_signal, write_fd,
};

/// The prompt the tests look for. Plain ASCII with no prompt escapes.
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
        let binary = env!("CARGO_BIN_EXE_bsh");

        // A minimal, deterministic environment. `PATH` must include the host's
        // `sleep`; `PS1` is the marker; `TERM=dumb` keeps the editor from
        // painting colour.
        let mut environment: Vec<Vec<u8>> = Vec::new();
        for (key, value) in std::env::vars_os() {
            use std::os::unix::ffi::OsStrExt;
            let key = key.as_os_str().as_bytes();
            if key == b"PS1" || key == b"PS2" || key == b"TERM" || key == b"HISTFILE" {
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
                ["bsh", "--norc", "-i"],
                environment.iter().map(Vec::as_slice),
            ) {
                Ok(plan) => plan,
                Err(_) => return 113,
            };
            let mut plan = plan;
            plan.sigmask = Some(bsh_sys::SignalSet::empty());
            plan.reset_signals = bsh_sys::SignalSet::of(&[bsh_sys::SIGPIPE]);
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
        let status = bsh_sys::wait_pid(self.child, false).expect("reap the shell");
        self.reaped = true;
        assert_eq!(
            status,
            bsh_sys::WaitOutcome::Exited(0),
            "the shell must exit cleanly at `exit`"
        );
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = send_signal(self.child, bsh_sys::SIGTERM);
            let _ = bsh_sys::wait_pid(self.child, false);
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

/// Give a freshly started foreground command time to reach its own process
/// group and `tcsetpgrp`; a `Ctrl-Z` sent before that would go to the shell.
fn settle() {
    std::thread::sleep(Duration::from_millis(400));
}

#[test]
fn ctrl_z_stops_the_foreground_job_and_the_shell_survives() {
    let mut session = Session::spawn();
    session.prompt();

    session.line("sleep 100");
    settle();
    session.send(b"\x1a"); // Ctrl-Z

    // The stop notice names the job, then the prompt returns.
    session.expect(b"Stopped", Duration::from_secs(10));
    session.expect(b"sleep 100", Duration::from_secs(10));
    session.prompt();

    // `jobs` lists it as stopped, with the `+` marker.
    session.line("jobs");
    session.expect(b"[1]+  Stopped", Duration::from_secs(10));
    session.expect(b"sleep 100", Duration::from_secs(10));
    session.prompt();

    // `bg` resumes it in the background and prints bash's line.
    session.line("bg");
    session.expect(b"[1]+ sleep 100 &", Duration::from_secs(10));
    session.prompt();
    session.line("jobs");
    session.expect(b"[1]+  Running", Duration::from_secs(10));
    session.expect(b"sleep 100 &", Duration::from_secs(10));
    session.prompt();

    // `fg` brings it back; Ctrl-C kills it and reports 130.
    session.line("fg");
    session.expect(b"sleep 100", Duration::from_secs(10));
    settle();
    session.send(b"\x03");
    session.prompt();
    session.line("echo $?");
    session.expect(b"130", Duration::from_secs(10));
    session.prompt();
    session.line("jobs");
    session.prompt();

    // The shell is still there.
    session.line("echo alive");
    session.expect(b"alive", Duration::from_secs(10));
    session.prompt();

    session.line("exit");
    session.wait_child();
}

#[test]
fn ctrl_c_on_a_foreground_job_leaves_status_130_and_the_shell_alive() {
    let mut session = Session::spawn();
    session.prompt();

    session.line("sleep 30");
    settle();
    session.send(b"\x03");
    session.prompt();
    session.line("echo status=$?");
    session.expect(b"status=130", Duration::from_secs(10));
    session.prompt();

    // No job was left behind.
    session.line("jobs");
    session.prompt();
    session.line("echo still-here");
    session.expect(b"still-here", Duration::from_secs(10));
    session.prompt();

    session.line("exit");
    session.wait_child();
}

#[test]
fn a_finished_job_is_reported_before_the_next_prompt() {
    let mut session = Session::spawn();
    session.prompt();

    session.line("sleep 0.1 &");
    // The start notice, then a command that gives the child time to finish.
    session.expect(b"[1]", Duration::from_secs(10));
    session.line("sleep 0.4");
    session.expect(b"Done", Duration::from_secs(10));
    session.expect(b"sleep 0.1", Duration::from_secs(10));
    session.prompt();

    // Reported once, and the job is gone.
    session.line("jobs");
    session.prompt();
    session.line("exit");
    session.wait_child();
}
