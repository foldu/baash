//! Interactive history on a real pseudo-terminal.
//!
//! The point of the wave is that history survives a session, so the assertions
//! are the user-visible ones: the file the shell wrote when it exited, and what
//! the *next* shell lists from it. `HOME` points at a fresh directory and no
//! `HISTFILE` is in the environment, so the default `~/.bsh_history` path and
//! the load/save round trip are both under test.

#![forbid(unsafe_code)]

use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
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
    cursor: usize,
    reaped: bool,
}

impl Session {
    /// Spawn `bsh --norc -i` with `home` as `HOME` and no inherited history
    /// settings.
    fn spawn(home: &Path) -> Self {
        let pty = open_pty().expect("open a pty");
        let master = pty.master.as_raw_fd();
        let slave = pty.slave.as_raw_fd();
        let binary = env!("CARGO_BIN_EXE_bsh");

        let mut environment: Vec<Vec<u8>> = Vec::new();
        for (key, value) in std::env::vars_os() {
            use std::os::unix::ffi::OsStrExt;
            let key = key.as_os_str().as_bytes();
            if matches!(
                key,
                b"PS1"
                    | b"PS2"
                    | b"TERM"
                    | b"HOME"
                    | b"HISTFILE"
                    | b"HISTCONTROL"
                    | b"HISTSIZE"
                    | b"HISTFILESIZE"
                    | b"HISTIGNORE"
            ) {
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
        let mut home_entry = b"HOME=".to_vec();
        use std::os::unix::ffi::OsStrExt;
        home_entry.extend_from_slice(home.as_os_str().as_bytes());
        environment.push(home_entry);

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

    /// Type a line and wait for the next prompt.
    fn line(&mut self, text: &str) {
        self.send_line(text);
        self.prompt();
    }

    /// Type a line without waiting: for commands whose output must be matched
    /// before the next prompt, and for `exit`, after which there is none.
    fn send_line(&mut self, text: &str) {
        self.send(text.as_bytes());
        self.send(b"\n");
    }

    fn wait_child(&mut self) {
        if self.reaped {
            return;
        }
        let status = bsh_sys::wait_pid(self.child).expect("reap the shell");
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
            let _ = bsh_sys::wait_pid(self.child);
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

fn temp_home(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("bsh-pty-history-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("create temp home");
    path
}

#[test]
fn a_session_persists_commands_and_the_next_session_loads_them() {
    let home = temp_home("round-trip");
    {
        let mut session = Session::spawn(&home);
        session.prompt();
        session.line("echo first");
        session.line("echo second");
        session.send_line("exit");
        session.wait_child();
    }

    let file = home.join(".bsh_history");
    let contents = std::fs::read_to_string(&file).expect("default history file was written");
    assert_eq!(contents, "echo first\necho second\nexit\n");

    {
        let mut session = Session::spawn(&home);
        session.prompt();
        session.send_line("history");
        session.expect(b"echo first", Duration::from_secs(10));
        session.expect(b"echo second", Duration::from_secs(10));
        session.prompt();
        session.send_line("exit");
        session.wait_child();
    }

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn histcontrol_ignoredups_skips_a_repeated_command() {
    let home = temp_home("ignoredups");
    {
        let mut session = Session::spawn(&home);
        session.prompt();
        session.line("HISTCONTROL=ignoredups");
        session.line("echo dup");
        session.line("echo dup");
        session.line("echo other");
        session.send_line("exit");
        session.wait_child();
    }

    let contents =
        std::fs::read_to_string(home.join(".bsh_history")).expect("history file was written");
    assert_eq!(
        contents,
        "HISTCONTROL=ignoredups\necho dup\necho other\nexit\n"
    );

    let _ = std::fs::remove_dir_all(&home);
}
