//! Completion at a real prompt, on a real pseudo-terminal.
//!
//! The engine's unit tests prove the candidate sets; this file proves the
//! product promise: pressing Tab at a normal prompt behaves like bash with
//! `bash-completion` installed. Each test drives the shipped binary through a
//! pty and asserts the bytes a user sees, then executes the completed line so
//! the insertion is shown to mean what it said.

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
/// `cargo test`; `PATH` is kept so `cat` and friends resolve.
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
        // No history database: a test run must not touch the user's.
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
                ["baash", "--norc", "-i"],
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
        self.expect(b"P> ", Duration::from_secs(10));
    }

    /// Type a line and wait for the next prompt.
    fn line(&mut self, text: &str) {
        self.send(text.as_bytes());
        self.send(b"\n");
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
    let path =
        std::env::temp_dir().join(format!("baash-pty-complete-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("create temp dir");
    path
}

/// Where this machine keeps `bash_completion`, when it has one.
///
/// The library is distribution data, not part of this repository, so the test
/// reports the skip instead of failing when it is absent (the same rule as the
/// zoxide harness).
fn bash_completion_library() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("BAASH_BASH_COMPLETION") {
        return Some(PathBuf::from(path));
    }
    [
        "/run/current-system/sw/share/bash-completion/bash_completion",
        "/usr/share/bash-completion/bash_completion",
        "/etc/bash_completion",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|path| path.is_file())
}

/// A shell in a fresh `HOME` and `cwd`, with a stable prompt.
fn session(tag: &str) -> (Session, PathBuf, PathBuf) {
    let home = temp_dir(&format!("{tag}-home"));
    let work = temp_dir(&format!("{tag}-work"));
    let session = Session::spawn(&home, &work, &[("PS1", "P> ")]);
    let mut session = session;
    session.prompt();
    (session, home, work)
}

fn cleanup(session: &mut Session, home: &Path, work: &Path) {
    session.send(b"exit\n");
    session.wait_child();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(work);
}

#[test]
fn tab_completes_a_variable_name() {
    let (mut session, home, work) = session("variable");
    // `HOSTNAME`/`HOSTTYPE` would make `$HO` ambiguous; the menu is sorted, so
    // `$HOME` would still be first, but the test is about the insertion.
    session.line("unset HOSTNAME HOSTTYPE");

    session.send(b"echo $HO\t");
    session.expect(b"$HOME", Duration::from_secs(10));
    // Enter accepts the selected menu entry; the next one submits the line.
    session.send(b"\n");
    session.send(b"\n");
    session.prompt();
    // The line ran with the expanded value: `echo $HOME` printed the path.
    let home_text = home.as_os_str().as_encoded_bytes();
    assert!(
        find(&session.output, home_text).is_some(),
        "the completed line must expand to HOME: {:?}",
        String::from_utf8_lossy(&session.output)
    );
    cleanup(&mut session, &home, &work);
}

#[test]
fn tab_offers_the_for_keyword_in_command_position() {
    let (mut session, home, work) = session("keyword");
    // An empty `PATH` narrows the menu to what the shell knows, so `fo`
    // reaches `for`/`function` rather than `fold`.
    session.line("PATH=");

    session.send(b"fo\t");
    session.expect(b"for", Duration::from_secs(10));
    session.send(b"\n"); // accept the menu selection (`for`)
    session.send(b" x in 1; do echo $x; done\n");
    session.expect(b"1\r\n", Duration::from_secs(10));
    session.prompt();
    cleanup(&mut session, &home, &work);
}

#[test]
fn a_function_defined_in_the_session_completes() {
    let (mut session, home, work) = session("function");
    session.line("frobnicate () { echo FN; }");

    session.send(b"frobni\t");
    session.expect(b"frobnicate", Duration::from_secs(10));
    // A unique match is inserted outright; if the menu opened, Enter accepts
    // it. The second Enter submits either way (an empty line is harmless).
    session.send(b"\n");
    session.send(b"\n");
    session.expect(b"FN\r\n", Duration::from_secs(10));
    session.prompt();
    cleanup(&mut session, &home, &work);
}

#[test]
fn a_word_list_rule_filters_and_inserts() {
    let (mut session, home, work) = session("wordlist");
    session.line("mycmd () { echo \"args:$*\"; }");
    session.line("complete -W 'alpha beta' mycmd");

    session.send(b"mycmd al\t");
    session.expect(b"alpha", Duration::from_secs(10));
    session.send(b"\n");
    session.expect(b"args:alpha\r\n", Duration::from_secs(10));
    session.prompt();
    cleanup(&mut session, &home, &work);
}

#[test]
fn a_function_rule_runs_the_shell_function() {
    // `complete -F` is shell code: the function runs at Tab with `$2` set to
    // the word being completed, its stdout is painted above the prompt (bash's
    // behaviour), and the menu follows.
    let (mut session, home, work) = session("functionrule");
    session.line("mycmd () { echo \"args:$*\"; }");
    session.line("f () { echo \"$1/$2\"; COMPREPLY=(alpha-$2); }");
    session.line("complete -F f mycmd");

    session.send(b"mycmd al\t");
    // The function's `echo` first, where bash puts it, then the menu.
    session.expect(b"mycmd/al", Duration::from_secs(10));
    session.expect(b"alpha-al", Duration::from_secs(10));
    session.send(b"\n");
    session.expect(b"args:alpha-al\r\n", Duration::from_secs(10));
    session.prompt();
    cleanup(&mut session, &home, &work);
}

#[test]
fn a_filename_with_a_space_is_inserted_quoted_and_runs() {
    let (mut session, home, work) = session("spacefile");
    std::fs::write(work.join("spaced name"), b"PAYLOAD\n").expect("write the file");

    session.send(b"cat spa\t");
    // The insertion must escape the space: `cat spaced\ name `.
    session.expect(b"spaced\\ name", Duration::from_secs(10));
    session.send(b"\n");
    session.expect(b"PAYLOAD", Duration::from_secs(10));
    session.prompt();
    cleanup(&mut session, &home, &work);
}

#[test]
fn directory_completion_stops_at_the_directory() {
    let (mut session, home, work) = session("dirfile");
    std::fs::create_dir(work.join("sub dir")).expect("create subdirectory");

    session.send(b"cd sub\t");
    // A directory ends in `/` and never gets a trailing space, so the next
    // thing typed stays inside it.
    session.expect(b"sub\\ dir/", Duration::from_secs(10));
    session.send(b"\x15"); // Ctrl-U: clear the line
    session.send(b"echo ok\n");
    session.expect(b"ok\r\n", Duration::from_secs(10));
    session.prompt();
    cleanup(&mut session, &home, &work);
}

#[test]
fn a_programmable_rule_with_a_default_still_completes_files() {
    let (mut session, home, work) = session("defaultspec");
    std::fs::write(work.join("only-file"), b"CONTENT\n").expect("write the file");
    session.line("mycmd () { cat \"$@\"; }");
    // The rule has no words that match `only`, so `-o default` must fall back
    // to the shell's filename completion.
    session.line("complete -o default -W 'nothing' mycmd");

    session.send(b"mycmd onl\t");
    session.expect(b"only-file", Duration::from_secs(10));
    session.send(b"\n");
    session.expect(b"CONTENT", Duration::from_secs(10));
    session.prompt();
    cleanup(&mut session, &home, &work);
}

/// The history ring is a completion source too, but a command typed *after* a
/// TAB could never be in it; the useful user-visible consequence is that an
/// earlier command word is offered. This keeps the pty suite honest about the
/// wiring rather than only the engine tests.
#[test]
fn history_words_are_offered_after_a_command() {
    let (mut session, home, work) = session("history-word");
    session.line("unique-history-command");
    session.send(b"uniq\t");
    session.expect(b"unique-history-command", Duration::from_secs(10));
    session.send(b"\x15");
    session.send(b"echo done\n");
    session.expect(b"done\r\n", Duration::from_secs(10));
    session.prompt();
    cleanup(&mut session, &home, &work);
}

/// A `PATH` with thousands of entries must not be walked on the keystroke path.
///
/// The index is built on a worker at startup (`docs/direction.md` §6, W3), so
/// by the time the first Tab arrives it is a lookup. Before that change the
/// first command completion walked every directory here on the thread reading
/// keys, which is the one scan that could be felt as lag.
#[test]
fn a_large_path_is_scanned_off_the_keystroke_path() {
    let home = temp_dir("lag-home");
    let work = temp_dir("lag-work");
    let bin = work.join("many-bin");
    std::fs::create_dir_all(&bin).expect("create bin");
    for index in 0..5_000 {
        let path = bin.join(format!("baash-lag-probe-{index:05}"));
        std::fs::write(&path, b"#!/bin/sh\n").expect("write probe");
        let mut permissions = std::fs::metadata(&path).expect("stat probe").permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&path, permissions).expect("chmod probe");
    }
    let bin_arg = bin.to_str().expect("utf-8 path");

    let mut session = Session::spawn(&home, &work, &[("PS1", "P> "), ("PATH", bin_arg)]);
    session.prompt();
    // Typing must echo immediately even while the worker is still scanning.
    let typed_at = Instant::now();
    session.send(b"baash-lag");
    session.expect(b"baash-lag", Duration::from_millis(400));
    assert!(
        typed_at.elapsed() < Duration::from_millis(400),
        "typing waited {:?} on a scan",
        typed_at.elapsed()
    );

    let completed_at = Instant::now();
    session.send(b"\t");
    session.expect(b"baash-lag-probe-", Duration::from_secs(10));
    assert!(
        completed_at.elapsed() < Duration::from_millis(400),
        "the first completion took {:?} with 5000 PATH entries",
        completed_at.elapsed()
    );
    session.send(b"\x15");
    cleanup(&mut session, &home, &work);
}

/// The real bash-completion library, end to end: its `-D` loader installs a
/// command's script on the first Tab (status 124), and the retry runs it.
#[test]
fn bash_completion_loads_a_command_on_the_first_tab() {
    let Some(library) = bash_completion_library() else {
        eprintln!("skipping: no bash_completion library on this machine");
        return;
    };
    let home = temp_dir("bc-home");
    let work = temp_dir("bc-work");
    let completions = home.join("completions");
    std::fs::create_dir_all(&completions).expect("create the completions dir");
    std::fs::write(
        completions.join("mycmd"),
        "\n_mycmd() {\n    local cur=${COMP_WORDS[COMP_CWORD]}\n    COMPREPLY=( $(compgen -W 'start stop status' -- \"$cur\") )\n}\ncomplete -F _mycmd mycmd\n",
    )
    .expect("write the completion script");

    let user_dir = home.to_str().expect("utf-8 home").to_owned();
    let mut session = Session::spawn(
        &home,
        &work,
        &[("PS1", "P> "), ("BASH_COMPLETION_USER_DIR", &user_dir)],
    );
    session.prompt();
    session.line(&format!("source {}", library.display()));
    assert!(
        !session.output.windows(6).any(|window| window == b"error["),
        "sourcing bash_completion reported a diagnostic: {}",
        String::from_utf8_lossy(&session.output)
    );
    session.line("mycmd () { echo \"args:$*\"; }");

    session.send(b"mycmd st\t");
    session.expect(b"start", Duration::from_secs(10));
    // The menu is open; Enter selects and the next Enter submits it.
    session.send(b"\n");
    session.send(b"\n");
    session.expect(b"args:start", Duration::from_secs(10));
    session.prompt();
    cleanup(&mut session, &home, &work);
}
