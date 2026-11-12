//! Runtime carapace completion, end to end through a real prompt.
//!
//! The fake `carapace` on the test's `PATH` is a shell script: it checks the
//! argv contract itself (and that no `COMP_LINE` leaked into the child), writes
//! what it saw to a log, and prints a fixed `export` envelope. That makes the
//! protocol — not a Rust stub of it — the thing under test.
//!
//! Precedence is the product promise:
//! `external carapace > static tables > complete -F > the data engine`.

#![cfg(unix)]
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
/// `PATH` is scrubbed too: every test chooses it, and a test that wants no
/// `carapace` must not inherit one from the developer's environment.
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
    b"PATH",
    b"COMP_LINE",
    b"COMP_POINT",
    b"COMP_WORDS",
    b"COMP_CWORD",
    b"COMP_TYPE",
    b"COMP_KEY",
    b"COMP_WORDBREAKS",
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
    fn spawn(home: &Path, cwd: &Path, environment: &[(&str, &str)]) -> Self {
        let pty = open_pty().expect("open a pty");
        let master = pty.master.as_raw_fd();
        let slave = pty.slave.as_raw_fd();
        let binary = env!("CARGO_BIN_EXE_baash");

        let mut entries: Vec<Vec<u8>> = Vec::new();
        for (key, value) in std::env::vars_os() {
            use std::os::unix::ffi::OsStrExt;
            let key = key.as_os_str().as_bytes();
            if SCRUBBED.contains(&key) {
                continue;
            }
            let mut entry = key.to_vec();
            entry.push(b'=');
            entry.extend_from_slice(value.as_os_str().as_bytes());
            entries.push(entry);
        }
        entries.push(b"TERM=dumb".to_vec());
        entries.push(b"LC_ALL=C".to_vec());
        let mut home_entry = b"HOME=".to_vec();
        use std::os::unix::ffi::OsStrExt;
        home_entry.extend_from_slice(home.as_os_str().as_bytes());
        entries.push(home_entry);
        entries.push(b"BAASH_HISTORY_DB=".to_vec());
        for (key, value) in environment {
            let mut entry = key.as_bytes().to_vec();
            entry.push(b'=');
            entry.extend_from_slice(value.as_bytes());
            entries.push(entry);
        }

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

    fn prompt(&mut self) {
        self.expect(b"P> ", Duration::from_secs(10));
    }

    fn line(&mut self, text: &str) {
        self.send(text.as_bytes());
        self.send(b"\n");
        // The editor repaints the prompt while the line is typed, so a bare
        // `prompt()` would match a repaint and return before the command ran.
        // The submit newline is the first `\r\n` after the typing.
        self.expect(b"\r\n", Duration::from_secs(10));
        self.prompt();
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
    let path = std::env::temp_dir().join(format!("baash-carapace-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("create temp dir");
    path
}

/// Write a fake `carapace` whose stdout is `mode`'s fixed answer.
///
/// The script asserts the argv contract (and that `COMP_LINE` is absent)
/// before printing, so a wrong invocation fails the test as "no candidates"
/// rather than passing because the Rust side happened to accept anything.
fn fake_carapace(dir: &Path, mode: &str, command: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let log = dir.join("argv.log");
    let script = dir.join("carapace");
    let body = format!(
        r#"#!/bin/sh
log='{log}'
: > "$log"
for arg in "$@"; do printf '%s\n' "$arg" >> "$log"; done
if [ -n "${{COMP_LINE+x}}" ]; then
    printf 'COMP_LINE leaked into carapace\n' >> "$log"
    exit 2
fi
if [ "$1" != '{command}' ] || [ "$2" != 'export' ]; then
    printf 'unexpected argv\n' >> "$log"
    exit 3
fi
case '{mode}' in
    values)
        printf '{{"version":"v-test","messages":[],"nospace":"","usage":"","values":[{{"value":"FAKE-ALPHA","display":"FAKE-ALPHA","description":"first fake"}},{{"value":"FAKE-BETA","display":"FAKE-BETA","description":"second fake"}}]}}\n'
        ;;
    silent)
        ;;
    garbage)
        printf 'this is not json'
        ;;
    slow)
        sleep 10
        printf '{{"version":"v-test","messages":[],"nospace":"","usage":"","values":[{{"value":"FAKE-SLOW","display":"FAKE-SLOW","description":""}}]}}\n'
        ;;
esac
"#,
        log = log.display()
    );
    std::fs::write(&script, body).expect("write fake carapace");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("chmod fake carapace");
    script
}

/// A path with `dir` first and, optionally, the host's tools after it.
fn path_with(dir: &Path, include_host: bool) -> String {
    if include_host {
        let host = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string());
        format!("{}:{host}", dir.display())
    } else {
        dir.display().to_string()
    }
}

/// One session in a fresh home/cwd with an explicit `PATH`.
fn session(tag: &str, path: &str) -> (Session, PathBuf, PathBuf) {
    let home = temp_dir(&format!("{tag}-home"));
    let work = temp_dir(&format!("{tag}-work"));
    let mut session = Session::spawn(&home, &work, &[("PS1", "P> "), ("PATH", path)]);
    session.prompt();
    (session, home, work)
}

fn cleanup(session: &mut Session, home: &Path, work: &Path) {
    // A test may stop with an open menu or a partly completed line; Ctrl-C
    // clears both and returns to a fresh prompt, so `exit` is not appended to
    // whatever was there.
    session.send(b"\x03");
    session.prompt();
    session.send(b"exit 0\n");
    session.wait_child();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(work);
}

fn log_lines(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("argv.log"))
        .expect("the fake wrote its argv")
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn a_fake_carapace_on_path_answers_and_gets_the_words_in_argv() {
    let bin = temp_dir("values-bin");
    fake_carapace(&bin, "values", "git");
    let (mut session, home, work) = session("values", &path_with(&bin, true));
    // A `COMP_LINE` inherited from a parent shell must be stripped: carapace's
    // bash patch would otherwise replace the argv words with its own parse of
    // this variable. The fake exits 2 if it sees one.
    session.line("export COMP_LINE='git ch'");

    session.send(b"git ch\t");
    session.expect(b"FAKE-ALPHA", Duration::from_secs(10));
    session.expect(b"FAKE-BETA", Duration::from_secs(10));
    assert_eq!(
        log_lines(&bin),
        vec![
            "git".to_string(),
            "export".to_string(),
            "git".to_string(),
            "ch".to_string()
        ]
    );

    // An empty current word must still be an argument, or carapace answers
    // for the previous one (`git` instead of a new argument).
    std::fs::remove_file(bin.join("argv.log")).expect("clear the log");
    session.send(b"\x03");
    session.prompt();
    session.send(b"git \t");
    // Both fakes share `FAKE-`, so the Tab inserts that prefix; the point here
    // is the empty argv word, not the menu.
    session.expect(b"git FAKE-", Duration::from_secs(10));
    let lines = log_lines(&bin);
    assert_eq!(lines.len(), 4, "argv {lines:?}");
    assert_eq!(&lines[..3], ["git", "export", "git"]);
    assert!(lines[3].is_empty(), "the empty word is passed: {lines:?}");

    cleanup(&mut session, &home, &work);
}

#[cfg(feature = "carapace")]
#[test]
fn external_carapace_beats_the_static_tables_and_a_function_rule() {
    let bin = temp_dir("precedence-a-bin");
    fake_carapace(&bin, "values", "git");
    let (mut session, home, work) = session("precedence-a", &path_with(&bin, true));

    session.line("_f() { COMPREPLY=(FUNC-ONE FUNC-TWO); }");
    session.line("complete -F _f git");
    let start = session.cursor;
    session.send(b"git ch\t");
    session.expect(b"FAKE-ALPHA", Duration::from_secs(10));
    let after = &session.output[start..];
    assert!(
        find(after, b"checkout").is_none(),
        "the static table must not be shown when carapace answered: {}",
        String::from_utf8_lossy(after)
    );
    session.drain();
    assert!(
        find(&session.output[start..], b"FUNC-ONE").is_none(),
        "the -F function must not run when carapace answered: {}",
        String::from_utf8_lossy(&session.output[start..])
    );

    cleanup(&mut session, &home, &work);
}

#[cfg(feature = "carapace")]
#[test]
fn static_tables_beat_a_function_rule_without_a_binary() {
    let empty = temp_dir("precedence-b-bin");
    let (mut session, home, work) = session("precedence-b", &path_with(&empty, false));

    session.line("_f() { COMPREPLY=(FUNC-ONE FUNC-TWO); }");
    session.line("complete -F _f git");
    // The first Tab inserts the common prefix (`che`), the second opens the
    // menu; both come from the generated table, not from `_f`.
    let start = session.cursor;
    session.send(b"git ch\t\t");
    session.expect(b"checkout", Duration::from_secs(10));
    session.drain();
    assert!(
        find(&session.output[start..], b"FUNC-ONE").is_none(),
        "the function rule must not run while the static table has an answer: {}",
        String::from_utf8_lossy(&session.output[start..])
    );

    cleanup(&mut session, &home, &work);
}

#[test]
fn a_function_rule_still_answers_without_carapace_anywhere() {
    let empty = temp_dir("function-bin");
    let (mut session, home, work) = session("function", &path_with(&empty, false));

    session.line("_mycmd() { COMPREPLY=(FUNC-ONE); }");
    session.line("complete -F _mycmd mycmd");
    // One reply is inserted without a menu, quoting included.
    session.send(b"mycmd \t");
    session.expect(b"FUNC-ONE", Duration::from_secs(10));

    cleanup(&mut session, &home, &work);
}

#[test]
fn an_empty_runtime_answer_falls_through_to_the_function() {
    falls_through("silent");
}

#[test]
fn garbage_runtime_output_falls_through_to_the_function() {
    falls_through("garbage");
}

fn falls_through(mode: &str) {
    let bin = temp_dir(&format!("fallthrough-{mode}-bin"));
    fake_carapace(&bin, mode, "mycmd");
    let (mut session, home, work) = session(&format!("fallthrough-{mode}"), &path_with(&bin, true));

    session.line("_mycmd() { COMPREPLY=(FUNC-ONE); }");
    session.line("complete -F _mycmd mycmd");
    session.send(b"mycmd \t");
    session.expect(b"FUNC-ONE", Duration::from_secs(10));

    cleanup(&mut session, &home, &work);
}

#[test]
fn a_keystroke_cancels_a_pending_lookup() {
    let bin = temp_dir("cancel-bin");
    fake_carapace(&bin, "slow", "mycmd");
    let (mut session, home, work) = session("cancel", &path_with(&bin, true));

    session.line("_mycmd() { COMPREPLY=(FUNC-ONE); }");
    session.line("complete -F _mycmd mycmd");
    let start = session.cursor;
    let started = Instant::now();
    session.send(b"mycmd \t");
    // The fake has not answered; a typed byte must be processed, not queued
    // behind the lookup.
    session.send(b"X");
    session.expect(b"mycmd X", Duration::from_secs(5));
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "the keystroke must cancel the lookup, not wait for its deadline: {elapsed:?}"
    );
    assert!(
        find(&session.output[start..], b"FUNC-ONE").is_none(),
        "a cancelled lookup must not fall through to the function: {}",
        String::from_utf8_lossy(&session.output[start..])
    );

    cleanup(&mut session, &home, &work);
}

#[test]
fn a_hanging_carapace_is_bounded_and_falls_through() {
    let bin = temp_dir("slow-bin");
    fake_carapace(&bin, "slow", "mycmd");
    let (mut session, home, work) = session("slow", &path_with(&bin, true));

    session.line("_mycmd() { COMPREPLY=(FUNC-ONE); }");
    session.line("complete -F _mycmd mycmd");
    let started = Instant::now();
    session.send(b"mycmd \t");
    session.expect(b"FUNC-ONE", Duration::from_secs(10));
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "the 500 ms deadline (plus the fake's response) must not become a hang: {elapsed:?}"
    );

    cleanup(&mut session, &home, &work);
}
