//! Interactive history expansion on a real pseudo-terminal.
//!
//! This is the user-visible contract: what `!!`, `!$`, `!echo`, `^old^new^`
//! and `set +H` do at a prompt, what a failed event does, and what `history`
//! shows afterwards. Bash records the *expanded* line, not the typed event, and
//! these tests pin that (measured against bash 5.3.20).

#![forbid(unsafe_code)]

use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use baash_sys::{
    Pty, SpawnPlan, become_session_leader, close, dup2, exec, fork_child_status, open_pty,
    poll_readable, read_fd, send_signal, write_fd,
};

/// The prompt the tests look for. Plain ASCII with no prompt escapes.
const PROMPT: &[u8] = b"@@ ";
const TIMEOUT: Duration = Duration::from_secs(10);

struct Session {
    pty: Pty,
    child: i32,
    output: Vec<u8>,
    cursor: usize,
    reaped: bool,
}

impl Session {
    /// Spawn `baash --norc -i` with a fresh `home`.
    fn spawn(home: &Path) -> Self {
        let pty = open_pty().expect("open a pty");
        let master = pty.master.as_raw_fd();
        let slave = pty.slave.as_raw_fd();
        let binary = env!("CARGO_BIN_EXE_baash");

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
                    | b"BAASH_HISTORY_DB"
                    | b"PROMPT_COMMAND"
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
        environment.push(b"BAASH_HISTORY_DB=".to_vec());

        let working = home.to_path_buf();
        let child = fork_child_status(move || {
            use std::os::unix::ffi::OsStrExt;
            if baash_sys::chdir(working.as_os_str().as_bytes()).is_err() {
                return 110;
            }
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
    fn expect(&mut self, needle: &[u8]) {
        let deadline = Instant::now() + TIMEOUT;
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
        self.expect(PROMPT);
    }

    /// Type a line without waiting: for commands whose output must be matched
    /// before the next prompt, and for `exit`, after which there is none.
    fn send_line(&mut self, text: &str) {
        self.send(text.as_bytes());
        self.send(b"\n");
    }

    /// Type a line, wait for its output, then for the next prompt.
    fn run(&mut self, text: &str, output: &[u8]) {
        self.send_line(text);
        self.expect(output);
        self.prompt();
    }

    /// The bytes from `start` to the current cursor.
    fn segment(&self, start: usize) -> &[u8] {
        &self.output[start..self.cursor]
    }

    fn cursor(&self) -> usize {
        self.cursor
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

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    find(haystack, needle).is_some()
}

fn occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() {
        return 0;
    }
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

fn temp_home(tag: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("baash-pty-histexpand-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("create temp home");
    path
}

/// Two files whose *contents* are not in their names, so an `expect` on the
/// content proves the command actually ran.
fn fixtures(home: &Path) {
    std::fs::write(home.join("alpha.txt"), "ALPHA\n").expect("write alpha");
    std::fs::write(home.join("beta.txt"), "BETA\n").expect("write beta");
}

#[test]
fn double_bang_runs_the_previous_command_and_history_shows_it_expanded() {
    let home = temp_home("double-bang");
    fixtures(&home);
    let mut session = Session::spawn(&home);
    session.prompt();

    session.run("cat alpha.txt", b"ALPHA");
    session.run("cat beta.txt", b"BETA");

    // The expanded line is echoed before it runs, and the command really runs.
    session.send_line("!!");
    session.expect(b"cat beta.txt");
    session.expect(b"BETA");
    session.prompt();

    // `history` shows the expanded text, not `!!` (bash 5.3.20, measured).
    let start = session.cursor();
    session.send_line("history");
    session.expect(b"    3  cat beta.txt");
    session.prompt();
    let history = session.segment(start);
    assert!(
        contains(history, b"    1  cat alpha.txt"),
        "history was: {}",
        String::from_utf8_lossy(history)
    );
    assert!(
        contains(history, b"    2  cat beta.txt"),
        "history was: {}",
        String::from_utf8_lossy(history)
    );
    assert!(
        contains(history, b"    3  cat beta.txt"),
        "the `!!` line must be stored expanded; history was: {}",
        String::from_utf8_lossy(history)
    );
    assert!(
        !contains(history, b"!!"),
        "history must not contain the typed event; history was: {}",
        String::from_utf8_lossy(history)
    );

    session.send_line("exit");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn word_designator_inserts_the_last_word() {
    let home = temp_home("designator");
    fixtures(&home);
    let mut session = Session::spawn(&home);
    session.prompt();

    session.run("cat alpha.txt", b"ALPHA");

    // `!$` is the last word of the previous command, inserted into this one.
    session.send_line("echo !$");
    session.expect(b"echo alpha.txt");
    session.expect(b"alpha.txt");
    session.prompt();

    let start = session.cursor();
    session.send_line("history");
    session.expect(b"    2  echo alpha.txt");
    session.prompt();
    let history = session.segment(start);
    assert!(
        contains(history, b"echo alpha.txt"),
        "history was: {}",
        String::from_utf8_lossy(history)
    );

    session.send_line("exit");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn prefix_search_finds_the_most_recent_command_starting_with_the_string() {
    let home = temp_home("prefix");
    fixtures(&home);
    let mut session = Session::spawn(&home);
    session.prompt();

    session.run("cat alpha.txt", b"ALPHA");
    session.run("echo BETWEEN", b"BETWEEN");

    // `!cat` is the newest command starting with `cat`.
    session.send_line("!cat");
    session.expect(b"cat alpha.txt");
    session.expect(b"ALPHA");
    session.prompt();

    session.send_line("exit");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn quick_substitution_reruns_the_previous_command_with_the_replacement() {
    let home = temp_home("quick-sub");
    fixtures(&home);
    let mut session = Session::spawn(&home);
    session.prompt();

    session.run("cat alpha.txt", b"ALPHA");

    session.send_line("^alpha^beta^");
    session.expect(b"cat beta.txt");
    session.expect(b"BETA");
    session.prompt();

    let start = session.cursor();
    session.send_line("history");
    session.expect(b"    2  cat beta.txt");
    session.prompt();
    let history = session.segment(start);
    assert!(
        contains(history, b"cat beta.txt"),
        "the quick substitution must be stored expanded; history was: {}",
        String::from_utf8_lossy(history)
    );
    assert!(
        !contains(history, b"^alpha^beta^"),
        "history must not contain the quick-substitution line; history was: {}",
        String::from_utf8_lossy(history)
    );

    session.send_line("exit");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn set_plus_h_makes_a_bang_literal() {
    let home = temp_home("plus-h");
    let mut session = Session::spawn(&home);
    session.prompt();

    session.run("echo hi", b"hi");
    session.run("set +H", b"");
    session.send_line("!!");
    session.expect(b"!!: command not found");
    session.prompt();
    session.run("echo alive", b"alive");

    session.send_line("exit 0");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn set_plus_o_histexpand_also_disables_it() {
    let home = temp_home("plus-o-histexpand");
    let mut session = Session::spawn(&home);
    session.prompt();

    session.run("echo hi", b"hi");
    session.run("set +o histexpand", b"");
    session.send_line("!!");
    session.expect(b"!!: command not found");
    session.prompt();

    session.send_line("exit 0");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn set_plus_o_history_disables_expansion_too() {
    let home = temp_home("plus-o-history");
    let mut session = Session::spawn(&home);
    session.prompt();

    session.run("echo hi", b"hi");
    session.run("set +o history", b"");
    session.send_line("!!");
    session.expect(b"!!: command not found");
    session.prompt();

    session.send_line("exit 0");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_failed_event_abandons_the_line_and_the_shell_survives() {
    let home = temp_home("failed");
    let mut session = Session::spawn(&home);
    session.prompt();

    session.run("echo hi", b"hi");
    session.send_line("!nosuch");
    session.expect(b"!nosuch: event not found");
    session.prompt();
    // The failure did not take the shell (or the prompt) with it.
    session.run("echo alive", b"alive");

    let start = session.cursor();
    session.send_line("history");
    session.expect(b"echo alive");
    session.prompt();
    let history = session.segment(start);
    assert!(
        contains(history, b"echo alive"),
        "history was: {}",
        String::from_utf8_lossy(history)
    );
    assert!(
        !contains(history, b"!nosuch"),
        "the failed line must not be recorded; history was: {}",
        String::from_utf8_lossy(history)
    );

    session.send_line("exit");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn colon_p_prints_the_expansion_without_running_it() {
    let home = temp_home("print-only");
    let mut session = Session::spawn(&home);
    session.prompt();

    session.send_line("echo MARKER");
    session.expect(b"MARKER");
    session.prompt();

    let start = session.cursor();
    session.send_line("!!:p");
    session.expect(b"echo MARKER");
    session.prompt();
    let segment = session.segment(start);
    assert!(
        contains(segment, b"echo MARKER"),
        "`:p` must print the expansion; got: {}",
        String::from_utf8_lossy(segment)
    );
    assert_eq!(
        occurrences(segment, b"MARKER"),
        1,
        "`:p` must not run the command; got: {}",
        String::from_utf8_lossy(segment)
    );

    // It is still recorded (expanded), as bash does.
    let start = session.cursor();
    session.send_line("history");
    session.expect(b"    2  echo MARKER");
    session.prompt();
    let history = session.segment(start);
    assert!(
        contains(history, b"echo MARKER"),
        "history was: {}",
        String::from_utf8_lossy(history)
    );

    session.send_line("exit");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn histverify_reviews_the_expanded_line_before_running_it() {
    let home = temp_home("verify");
    let mut session = Session::spawn(&home);
    session.prompt();

    session.run("shopt -s histverify", b"");
    session.run("echo hi", b"hi");

    // Enter on `!!` does not run it: the expanded line comes back on the
    // editing line, and a second Enter submits it.
    let review_start = session.cursor();
    session.send_line("!!");
    session.expect(b"echo hi");
    let review = session.segment(review_start);
    assert!(
        !contains(review, b"\r\nhi\r\n") && !contains(review, b"\nhi\n"),
        "the reviewed line must not have run yet; got: {}",
        String::from_utf8_lossy(review)
    );
    session.send_line("");
    session.expect(b"hi");
    session.prompt();

    let start = session.cursor();
    session.send_line("history");
    session.expect(b"    3  echo hi");
    session.prompt();
    let history = session.segment(start);
    assert!(
        contains(history, b"    3  echo hi"),
        "the verified line is recorded once, expanded; history was: {}",
        String::from_utf8_lossy(history)
    );
    assert!(
        !contains(history, b"  !!"),
        "the typed `!!` must not be recorded; history was: {}",
        String::from_utf8_lossy(history)
    );

    session.send_line("exit");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
}
