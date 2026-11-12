//! Interactive prompts and `Ctrl-R` on a real pseudo-terminal.
//!
//! The prompt is user-visible correctness, so the assertions here are the
//! bytes a user sees: the rendered `PS1` (with `\u`, `\w` and a command
//! substitution), the cursor column the editor computed for it,
//! `PROMPT_COMMAND`'s output above the prompt, a `PS1` assigned by a command
//! taking effect at the next prompt, and `Ctrl-R` finding and running an
//! earlier line.
//!
//! The prompt-escape *table* is tested in `baash-lang`; this file only proves it
//! is wired to the shell's variables and to the terminal.

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

/// The environment every test starts from: no inherited prompt, history or
/// terminal state, `LC_ALL=C` so the widths are byte widths.
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
    // A remote session is a prompt *feature*, so the tests must opt in rather
    // than inherit it from whatever runs them.
    b"SSH_TTY",
    b"SSH_CONNECTION",
    b"SSH_CLIENT",
    b"LC_ALL",
    b"NO_COLOR",
    // A stale inherited `PWD` would win over the directory the test chdirs to,
    // and `\w` reads the shell's `PWD`.
    b"PWD",
    b"OLDPWD",
];

impl Session {
    fn spawn(home: &Path, cwd: &Path, arguments: &[&str], environment: &[(&str, &str)]) -> Self {
        Self::spawn_with(home, cwd, arguments, environment, true)
    }

    /// `plain` adds `NO_COLOR=1`, so an assertion can read the prompt as text;
    /// the colour test passes false and looks for the escapes.
    fn spawn_with(
        home: &Path,
        cwd: &Path,
        arguments: &[&str],
        environment: &[(&str, &str)],
        plain: bool,
    ) -> Self {
        let pty = open_pty().expect("open a pty");
        let master = pty.master.as_raw_fd();
        let slave = pty.slave.as_raw_fd();
        // The geometry these tests assert about. Without it the pty reports
        // zero columns and the editor falls back to a narrow default, so a
        // prompt that is legitimately long — `$HOME`, `TMPDIR` and the
        // hostname all appear in it, and a CI runner's name alone is forty
        // columns — wraps and the cursor column is no longer the prompt width.
        baash_sys::set_window_size_fd(master, 200, 50).expect("size the pty");
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
        entries.push(b"TERM=xterm".to_vec());
        entries.push(b"LC_ALL=C".to_vec());
        // Plain text by default: these assertions read the prompt as a string,
        // and the stock one is coloured on a terminal.
        if plain {
            entries.push(b"NO_COLOR=1".to_vec());
        }
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

        let arguments: Vec<String> = arguments.iter().map(|arg| (*arg).to_string()).collect();
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
            let mut argv: Vec<&str> = vec!["baash"];
            argv.extend(arguments.iter().map(String::as_str));
            let plan =
                match SpawnPlan::new_with_env(binary, argv, entries.iter().map(Vec::as_slice)) {
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

    /// Wait for a prompt containing `needle` (a suffix of the visible prompt).
    fn prompt(&mut self, needle: &str) {
        self.expect(needle.as_bytes(), Duration::from_secs(10));
    }

    fn line(&mut self, text: &str, next_prompt: &str) {
        self.send(text.as_bytes());
        self.send(b"\n");
        self.prompt(next_prompt);
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

/// The marker the stock prompt ends with: `#` for root, `$` otherwise.
///
/// The renderer follows bash's `\$`, and a CI container runs the tests as
/// root, so the expectation has to be built from the same answer the shell
/// reaches rather than written down as `$`.
fn user_marker() -> &'static str {
    match std::process::Command::new(env!("CARGO_BIN_EXE_baash"))
        .args(["-c", "test \"$EUID\" -eq 0"])
        .status()
    {
        Ok(status) if status.success() => "#",
        _ => "$",
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("baash-pty-prompt-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("create temp dir");
    path
}

/// The `n` in the first `\x1b[<n>C` at or after `from`.
fn cursor_right_after(output: &[u8], from: usize) -> Option<usize> {
    let rest = &output[from..];
    let start = find(rest, b"\x1b[")?;
    let digits_start = start + 2;
    let end = rest[digits_start..].iter().position(|&byte| byte == b'C')? + digits_start;
    std::str::from_utf8(&rest[digits_start..end])
        .ok()?
        .parse()
        .ok()
}

/// A multi-line command recalled from history is shown as the lines it is.
///
/// The editor paints continuation lines as scrollback while you type, so a
/// recalled entry is the first time it has to draw a buffer that contains
/// newlines. Caret notation (`for i in 1 2; do^Jecho $i^Jdone`) made a working
/// command look unparseable.
#[test]
fn a_multiline_entry_is_recalled_as_lines() {
    let home = temp_dir("multiline-home");
    let work = temp_dir("multiline-work");
    let mut session = Session::spawn(&home, &work, &["--norc", "-i"], &[]);
    session.prompt(" $");
    // A command the editor continues: `for` is unfinished until `done`.
    session.send(b"for i in 1 2; do\n");
    session.prompt("> ");
    session.send(b"echo $i\n");
    session.prompt("> ");
    session.send(b"done\n");
    session.prompt(" $");
    // Recalling it must paint three rows, not one with literal `^J`.
    session.send(b"\x1b[A");
    session.expect(b"do", Duration::from_secs(10));
    session.expect(b"echo $i", Duration::from_secs(10));
    session.expect(b"done", Duration::from_secs(10));
    let recalled = session.output.clone();
    assert!(
        !recalled.windows(2).any(|window| window == b"^J"),
        "no caret-J in a recalled line: {:?}",
        String::from_utf8_lossy(&recalled)
    );
    // Abandon the recalled line; `Ctrl-C` leaves `$?` at 130, so `exit` takes
    // the status explicitly.
    session.send(b"\x03");
    session.prompt(" $");
    session.send(b"exit 0\n");
    session.wait_child();
}

/// An `ssh` session says which machine the command will run on.
///
/// `sshd` sets one of these for every session, including the nested shells and
/// tmux that inherit it, so the marker is what decides — not `$TERM`, and not
/// whether the terminal is local.
#[test]
fn an_ssh_session_shows_user_and_host() {
    let home = temp_dir("ssh-home");
    let work = temp_dir("ssh-work");
    let marker = user_marker();
    let mut session = Session::spawn(
        &home,
        &work,
        &["--norc", "-i"],
        &[("SSH_TTY", "/dev/pts/0")],
    );
    // `user@host`, then the path, then the marker: `@` can only come from the
    // remote segment, which the local test below proves by its absence.
    session.expect(b"@", Duration::from_secs(10));
    session.prompt(&format!(" {marker}"));
    session.send(b"exit\n");
    session.wait_child();
}

/// Without a marker the prompt stays local.
#[test]
fn a_local_session_shows_no_user_or_host() {
    let home = temp_dir("local-home");
    let work = temp_dir("local-work");
    let marker = user_marker();
    let mut session = Session::spawn(&home, &work, &["--norc", "-i"], &[]);
    session.prompt(&format!(" {marker}"));
    assert!(
        !session.output.contains(&b'@'),
        "no `user@host` without an SSH marker: {:?}",
        String::from_utf8_lossy(&session.output)
    );
    session.send(b"exit\n");
    session.wait_child();
}

/// The stock prompt has tide's shape: abbreviated path, branch, counters.
///
/// `!N` is a tracked file that differs from `HEAD` or the index, `?N` is
/// untracked, and `↑N` is ahead of the upstream. Zero suppression is part of
/// the shape, so a clean branch shows only the path, the branch and `$`.
#[test]
fn the_default_prompt_is_tide_like() {
    let home = temp_dir("tide-home");
    let work = home.join("src/bshell");
    std::fs::create_dir_all(work.join(".git")).expect("create git directory");
    std::fs::write(work.join(".git/HEAD"), b"ref: refs/heads/main\n").expect("write HEAD");

    // No PS1 in the environment or an rc file: the stock prompt is rendered.
    // `src` is shortened to its unique first character because the scratch
    // home has no sibling that starts with `s`; the last component is whole.
    let marker = user_marker();
    let mut session = Session::spawn(&home, &work, &["--norc", "-i"], &[]);
    let prompt = format!("\u{f115} ~/s/bshell  \u{f418} main {marker}");
    session.prompt(&prompt);
    // The counts are git's, not the shell's: a failing command and a background
    // child leave the prompt alone.
    session.send(b"false\n");
    session.prompt(&prompt);
    // Leave through a successful command, so `exit` exits zero.
    session.send(b"true\n");
    session.prompt(&prompt);
    session.send(b"exit\n");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
}

/// A real repository: one commit ahead of a local upstream, one modified
/// tracked file and one untracked file. This is the brief's shape, through the
/// real binary and a pty.
#[test]
fn the_prompt_counts_ahead_behind_modified_and_untracked() {
    let home = temp_dir("counts-home");
    let work = home.join("repo");
    std::fs::create_dir_all(&work).expect("create work tree");
    let git = |directory: &Path, arguments: &[&str]| {
        let status = std::process::Command::new("git")
            .arg("-c")
            .arg("commit.gpgsign=false")
            .args(arguments)
            .current_dir(directory)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Baash Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Baash Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .status()
            .expect("run git");
        assert!(status.success(), "git {arguments:?}");
    };
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("tracked.txt"), "one\n").expect("write tracked file");
    git(&work, &["add", "tracked.txt"]);
    git(&work, &["commit", "-q", "-m", "one"]);
    let upstream = home.join("upstream.git");
    git(&home, &["init", "-q", "--bare", "upstream.git"]);
    git(
        &work,
        &["remote", "add", "origin", upstream.to_str().unwrap()],
    );
    git(&work, &["push", "-q", "-u", "origin", "main"]);
    // One commit the upstream does not have.
    std::fs::write(work.join("tracked.txt"), "two\n").expect("write tracked file");
    let base = String::from_utf8(
        std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&work)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .expect("rev-parse")
            .stdout,
    )
    .expect("utf-8 sha")
    .trim()
    .to_string();
    git(&work, &["commit", "-q", "-am", "two"]);
    // A second commit on the upstream's side of the same base, so the branch
    // is one ahead and one behind: a diverged branch, which the prompt has to
    // show as `↑1 ↓1`, not as a clean or ahead-only branch.
    git(&work, &["checkout", "-q", "-b", "upstream-side", &base]);
    std::fs::write(work.join("upstream.txt"), "upstream\n").expect("write upstream file");
    git(&work, &["add", "upstream.txt"]);
    git(&work, &["commit", "-q", "-m", "upstream"]);
    git(&work, &["push", "-q", "origin", "upstream-side:main"]);
    git(&work, &["checkout", "-q", "main"]);
    // One tracked file with an unstaged change, one untracked file.
    std::fs::write(work.join("tracked.txt"), "three\n").expect("write tracked file");
    std::fs::write(work.join("untracked.txt"), "x\n").expect("write untracked file");

    let mut session = Session::spawn(&home, &work, &["--norc", "-i"], &[]);
    // The directory is `~/repo`; the counters arrive from the worker, so wait
    // for the whole suffix rather than for the first prompt.
    session.prompt("\u{f418} main  \u{2191}1 \u{2193}1 !1 ?1 $");
    // A long line and an editing key must not corrupt the prompt's column
    // arithmetic (the icons are width 1, the SGR sequences width 0).
    session.send(&[b'x'; 240]);
    session.send(b"\x15"); // Ctrl-U: discard the long line
    session.send(b"echo ok\n");
    session.prompt("ok\r\n");
    session.send(b"exit\n");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_coloured_prompt_with_a_variable_and_a_substitution_keeps_the_cursor_right() {
    let home = temp_dir("colour-home");
    let work = temp_dir("colour-work");
    let prompt = "\\[\\e[1;32m\\]\\u@\\h\\[\\e[0m\\] \\w$(printf sub)\\$ ";
    let marker = user_marker();
    let mut session = Session::spawn(&home, &work, &["--norc", "-i"], &[("PS1", prompt)]);
    session.prompt(&format!("sub{marker} "));

    let output = session.output.clone();
    // The coloured prefix is the identity, then the plain suffix.
    let green = find(&output, b"\x1b[1;32m").expect("the green prefix");
    let reset = find(&output[green..], b"\x1b[0m").expect("the reset") + green;
    let identity = &output[green + b"\x1b[1;32m".len()..reset];
    assert!(
        identity.contains(&b'@'),
        "\\u@\\h rendered: {:?}",
        String::from_utf8_lossy(identity)
    );
    let work_text = work.as_os_str().as_encoded_bytes();
    let mut suffix = vec![b' '];
    suffix.extend_from_slice(work_text);
    suffix.extend_from_slice(format!("sub{marker} ").as_bytes());
    assert!(
        find(&output[reset..], &suffix).is_some(),
        "the prompt text after the colour is {:?}\noutput: {:?}",
        String::from_utf8_lossy(&suffix),
        String::from_utf8_lossy(&output)
    );
    // What the user sees: the identity from the coloured stretch, then the
    // plain suffix.
    let visible: Vec<u8> = identity.iter().copied().chain(suffix).collect();
    // The editor moves the cursor to the end of the *visible* prompt, so the
    // count must not include the SGR bytes: `\u@\h /path sub$ `.
    let moved = cursor_right_after(&output, reset + b"\x1b[0m".len())
        .expect("a cursor motion after the prompt");
    assert_eq!(
        moved,
        visible.len(),
        "the cursor column is the visible width, not the byte length\noutput: {:?}",
        String::from_utf8_lossy(&output)
    );

    // And the line edits and runs normally at that column.
    session.line("echo ok", &format!("sub{marker} "));
    session.expect(b"ok", Duration::from_secs(10));
    session.send(b"exit\n");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn a_stale_inherited_pwd_is_replaced_by_the_real_directory() {
    // `su`, a chroot or a wrapper script can leave a `PWD` that names some
    // other directory; `\w` must show where the shell actually is.
    let home = temp_dir("pwd-home");
    let work = temp_dir("pwd-work");
    let mut session = Session::spawn(
        &home,
        &work,
        &["--norc", "-i"],
        &[("PS1", "[\\w] $ "), ("PWD", "/nonexistent")],
    );
    let expected = format!("[{}] $ ", work.display());
    session.prompt(&expected);
    session.send(b"exit\n");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn prompt_command_output_appears_above_the_prompt() {
    let home = temp_dir("pc-home");
    let mut session = Session::spawn(&home, &home, &["--norc", "-i"], &[("PS1", "P> ")]);
    session.prompt("P> ");
    session.line("PROMPT_COMMAND='echo hello'", "P> ");
    // The next prompt is preceded by `hello` on its own line.
    session.expect(b"hello\r\n", Duration::from_secs(10));

    // Output without a trailing newline is terminated before the prompt, so
    // the editor's column arithmetic still holds.
    session.line("PROMPT_COMMAND='printf partial'", "P> ");
    session.expect(b"partial\r\n", Duration::from_secs(10));
    session.expect(b"P> ", Duration::from_secs(10));

    session.send(b"exit\n");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_ps1_assigned_by_a_command_takes_effect_at_the_next_prompt() {
    let home = temp_dir("ps1-home");
    let mut session = Session::spawn(&home, &home, &["--norc", "-i"], &[("PS1", "OLD> ")]);
    session.prompt("OLD> ");
    session.line("PS1='NEW> '", "NEW> ");
    session.line("echo hi", "NEW> ");
    session.expect(b"hi", Duration::from_secs(10));
    session.send(b"exit\n");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_ps1_from_the_startup_file_is_used() {
    let home = temp_dir("rc-home");
    let rc = home.join("rc");
    std::fs::write(&rc, "PS1='RC> '\n").expect("write rc file");
    let rc = rc.to_str().expect("utf-8 path").to_string();
    let mut session = Session::spawn(&home, &home, &["--rcfile", &rc, "-i"], &[]);
    session.prompt("RC> ");
    session.send(b"exit\n");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn prompt_escapes_read_real_history_and_job_numbers() {
    let home = temp_dir("numbers-home");
    let mut session = Session::spawn(
        &home,
        &home,
        &["--norc", "-i"],
        &[("PS1", "[\\!][\\#][\\j] ")],
    );
    session.prompt("[1][1][0] ");
    session.line("true", "");
    session.expect(b"[2][2][0] ", Duration::from_secs(10));
    session.send(b"exit\n");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn ctrl_r_finds_an_earlier_command_and_enter_runs_it() {
    let home = temp_dir("search-home");
    let mut session = Session::spawn(&home, &home, &["--norc", "-i"], &[("PS1", "S> ")]);
    session.prompt("S> ");
    session.line("echo first", "S> ");
    session.line("echo second", "S> ");

    session.send(b"\x12fir");
    session.expect(
        b"(reverse-i-search)`fir': echo first",
        Duration::from_secs(10),
    );
    session.send(b"\r");
    session.expect(b"first\r\n", Duration::from_secs(10));
    session.prompt("S> ");

    session.send(b"exit\n");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
}

/// The terminal is asked for bracketed paste while a line is read, and only
/// then.
///
/// `key.rs` has always understood `\x1b[200~`/`201~`, but a terminal only
/// sends them when the mode is on, and the shell never asked. Without the
/// mode a pasted script arrives as bare newlines, each of which the decoder
/// reads as Enter: the first line runs before the rest is even in the buffer.
/// The mode must also be off while a command runs, so an editor or `cat`
/// started from the line does not inherit the wrapper and print the markers.
#[test]
fn bracketed_paste_is_enabled_only_while_a_line_is_read() {
    let home = temp_dir("paste-home");
    let work = temp_dir("paste-work");
    let mut session = Session::spawn(&home, &work, &["--norc", "-i"], &[]);
    // The mode is on before the prompt text is painted: the editor writes it
    // when it enters raw mode, and only then draws the frame.
    session.expect(b"\x1b[?2004h", Duration::from_secs(10));
    session.prompt(" $");
    session.send(b"true\n");
    // Off again before the command gets the terminal.
    session.expect(b"\x1b[?2004l", Duration::from_secs(10));
    session.prompt(" $");
    session.send(b"exit 0\n");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&work);
}

/// Every prompt tells the terminal which directory it belongs to.
///
/// OSC 7 is what makes a new tab, a split or a tmux pane start where the
/// shell is, and it is re-sent on every prompt rather than only after a `cd`:
/// a child process (`ssh`, a container) can leave the terminal's copy stale
/// while the shell's `PWD` is right. The payload is a file URI, so the path
/// goes out percent-encoded, not as shell bytes.
#[test]
fn every_prompt_reports_the_working_directory() {
    use std::os::unix::ffi::OsStrExt;

    let home = temp_dir("osc-home");
    let work = temp_dir("osc-work");
    std::fs::create_dir_all(work.join("sub")).expect("create a subdirectory");
    let mut session = Session::spawn(&home, &work, &["--norc", "-i"], &[]);

    // The first prompt names the starting directory, before the prompt bytes.
    session.expect(b"\x1b]7;file://", Duration::from_secs(10));
    session.expect(work.as_os_str().as_bytes(), Duration::from_secs(10));
    session.expect(b"\x07", Duration::from_secs(10));
    session.prompt(" $");

    // A new directory is announced as well.
    session.send(b"cd sub\n");
    session.expect(b"\x1b]7;file://", Duration::from_secs(10));
    session.expect(
        work.join("sub").as_os_str().as_bytes(),
        Duration::from_secs(10),
    );
    session.expect(b"\x07", Duration::from_secs(10));
    session.prompt(" $");

    // And an unchanged directory is repeated on the next prompt, which is the
    // point: the terminal's state can be wrong even when the shell's is not.
    session.send(b"true\n");
    session.expect(b"\x1b]7;file://", Duration::from_secs(10));
    session.expect(
        work.join("sub").as_os_str().as_bytes(),
        Duration::from_secs(10),
    );
    session.expect(b"\x07", Duration::from_secs(10));
    session.prompt(" $");

    session.send(b"exit 0\n");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&work);
}

/// The window and tab titles name the shell's directory.
///
/// A title is not part of the prompt: it has no width, the user's `PS1`
/// cannot set it, and it must be re-sent at every prompt for the same reason
/// the cwd report is. Both selectors are set, because the shell cannot guess
/// which one a terminal shows.
#[test]
fn the_window_and_tab_titles_name_the_directory() {
    use std::os::unix::ffi::OsStrExt;

    let home = temp_dir("title-home");
    let work = temp_dir("title-work");
    let mut session = Session::spawn(&home, &work, &["--norc", "-i"], &[]);
    let directory = work.as_os_str().as_bytes();

    session.expect(b"\x1b]0;", Duration::from_secs(10));
    session.expect(directory, Duration::from_secs(10));
    // The tab title follows the window title immediately, then the cwd report.
    session.expect(b"\x07\x1b]1;", Duration::from_secs(10));
    session.expect(directory, Duration::from_secs(10));
    session.expect(b"\x07\x1b]7;file://", Duration::from_secs(10));
    session.prompt(" $");

    session.send(b"exit 0\n");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&work);
}

/// The prompt is bracketed by OSC 133 marks, once per prompt.
///
/// `A` before the prompt text and `B` after it are what let a terminal treat
/// a command line as a unit: prompt-jump, selecting a command's output, and
/// per-command cwd are all built on the pair. The marks are not visible; they
/// exist so the terminal can tell the prompt from what the user typed. They
/// belong to the prompt event, not the frame, so the next prompt has them
/// again.
#[test]
fn the_prompt_is_marked_for_the_terminal() {
    let home = temp_dir("marks-home");
    let work = temp_dir("marks-work");
    let mut session = Session::spawn(&home, &work, &["--norc", "-i"], &[]);

    session.expect(b"\x1b]133;A\x1b\\", Duration::from_secs(10));
    session.prompt(" $");
    session.expect(b"\x1b]133;B\x1b\\", Duration::from_secs(10));

    session.send(b"true\n");
    session.expect(b"\x1b]133;A\x1b\\", Duration::from_secs(10));
    session.prompt(" $");
    session.expect(b"\x1b]133;B\x1b\\", Duration::from_secs(10));

    session.send(b"exit 0\n");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&work);
}

/// A command is bracketed by OSC 133 `C` and `D`, with its text and status.
///
/// `C` tells the terminal what is about to run; `D` tells it the exit status so
/// the prompt can show it and the command's output can be selected as a unit.
/// The pair is written by the loop, which is the only party that sees a
/// command start and finish.
#[test]
fn a_command_is_marked_with_its_text_and_status() {
    let home = temp_dir("command-marks-home");
    let work = temp_dir("command-marks-work");
    let mut session = Session::spawn(&home, &work, &["--norc", "-i"], &[]);
    session.prompt(" $");

    session.send(b"true\n");
    session.expect(
        b"\x1b]133;C;cmdline_url=true\x1b\\",
        Duration::from_secs(10),
    );
    session.expect(b"\x1b]133;D;0\x1b\\", Duration::from_secs(10));
    // The next prompt starts with its own `A` mark.
    session.expect(b"\x1b]133;A\x1b\\", Duration::from_secs(10));
    session.prompt(" $");

    session.send(b"false\n");
    session.expect(
        b"\x1b]133;C;cmdline_url=false\x1b\\",
        Duration::from_secs(10),
    );
    session.expect(b"\x1b]133;D;1\x1b\\", Duration::from_secs(10));
    session.prompt(" $");

    session.send(b"exit 0\n");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&work);
}

/// Focus changes are requested while a line is read; a refocus repaints.
///
/// Another window may paint over a terminal while a prompt sits idle, and a
/// shared terminal can be reattached showing a stale frame. Focus reporting
/// is on for the line only: a command gets the terminal without the switch,
/// like bracketed paste.
#[test]
fn a_refocused_terminal_repaints_the_prompt() {
    let home = temp_dir("focus-home");
    let work = temp_dir("focus-work");
    let mut session = Session::spawn(&home, &work, &["--norc", "-i"], &[]);
    session.expect(b"\x1b[?1004h", Duration::from_secs(10));
    session.prompt(" $");

    // Gaining focus repaints, although the line itself did not change.
    session.send(b"\x1b[I");
    session.prompt(" $");

    // Losing focus changes nothing on screen.
    let mark = session.output.len();
    session.send(b"\x1b[O");
    std::thread::sleep(Duration::from_millis(100));
    session.drain();
    assert_eq!(
        &session.output[mark..],
        b"",
        "focus out must not repaint: {:?}",
        String::from_utf8_lossy(&session.output[mark..])
    );

    // Off while the command runs, on again for the next line.
    session.send(b"true\n");
    session.expect(b"\x1b[?1004l", Duration::from_secs(10));
    session.expect(b"\x1b[?1004h", Duration::from_secs(10));
    session.prompt(" $");

    session.send(b"exit 0\n");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&work);
}

/// The prompt asks the terminal where the cursor is before painting.
///
/// The answer decides whether the output before the prompt ended with a
/// newline; without it the frame paints over a partial last line. A pty has no
/// terminal emulator behind it, so nothing answers here: the test proves the
/// question is asked and that an unanswered question costs only the short
/// deadline, never the prompt.
#[test]
fn the_prompt_asks_for_the_cursor_position() {
    let home = temp_dir("cpr-home");
    let work = temp_dir("cpr-work");
    let mut session = Session::spawn(&home, &work, &["--norc", "-i"], &[]);
    session.expect(b"\x1b[6n", Duration::from_secs(10));
    session.prompt(" $");
    session.send(b"exit 0\n");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&work);
}

/// A resize repaints the prompt at the new width.
///
/// The terminal reflows its own contents on a resize, but the editor's frame
/// still assumes the old rows and columns; without a repaint the cursor and the
/// menu land in the wrong place until the next keystroke. `TIOCSWINSZ` on the
/// master delivers `SIGWINCH` to the foreground group of the slave, which is
/// the shell.
#[test]
fn resizing_the_terminal_repaints_the_prompt() {
    let home = temp_dir("resize-home");
    let work = temp_dir("resize-work");
    let mut session = Session::spawn(&home, &work, &["--norc", "-i"], &[]);
    session.prompt(" $");
    baash_sys::set_window_size_fd(session.pty.master.as_raw_fd(), 120, 40).expect("resize the pty");
    // The prompt is painted again without any keystroke.
    session.prompt(" $");
    session.send(b"exit 0\n");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&work);
}
