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
    session.prompt("$ ");
    // A command the editor continues: `for` is unfinished until `done`.
    session.send(b"for i in 1 2; do\n");
    session.prompt("> ");
    session.send(b"echo $i\n");
    session.prompt("> ");
    session.send(b"done\n");
    session.prompt("$ ");
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
    session.prompt("$ ");
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
    session.prompt(&format!(" {marker} "));
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
    session.prompt(&format!(" {marker} "));
    assert!(
        !session.output.contains(&b'@'),
        "no `user@host` without an SSH marker: {:?}",
        String::from_utf8_lossy(&session.output)
    );
    session.send(b"exit\n");
    session.wait_child();
}

/// A change in the working tree repaints the prompt without a keystroke.
///
/// This is the whole point of the watcher (`docs/prompt-watch.md`): the counts
/// are not a snapshot taken when the line began, so a file created by another
/// program appears in front of the user while they are still thinking.
#[test]
fn the_prompt_updates_while_the_line_is_idle() {
    let home = temp_dir("live-home");
    let work = temp_dir("live-work");
    let initialized = std::process::Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .arg(&work)
        .status()
        .expect("run git init");
    assert!(initialized.success(), "git init failed");
    let mut session = Session::spawn(&home, &work, &["--norc", "-i"], &[]);
    // The stock prompt: path, branch, counters.
    session.prompt("main $ ");
    // Nothing was typed, so only the watcher can have noticed this.
    std::fs::write(work.join("new.txt"), "x").expect("create an untracked file");
    session.expect(b"?1", Duration::from_secs(10));
    session.send(b"exit\n");
    session.wait_child();
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
fn the_default_prompt_is_fish_like() {
    let home = temp_dir("fish-home");
    let work = home.join("src/bshell");
    std::fs::create_dir_all(work.join(".git")).expect("create git directory");
    std::fs::write(work.join(".git/HEAD"), b"ref: refs/heads/main\n").expect("write HEAD");

    // No PS1 in the environment or an rc file: the stock prompt is rendered,
    // ending in `$`, or `#` for root, like a `\$` in a template would.
    let marker = user_marker();
    let mut session = Session::spawn(&home, &work, &["--norc", "-i"], &[]);
    // A space before the marker, and the branch between the path and it.
    session.prompt(&format!("~/src/bshell main {marker} "));
    // The counts are git's, not the shell's: a failing command and a background
    // child leave the prompt alone now.
    session.send(b"false\n");
    session.prompt(&format!("~/src/bshell main {marker} "));
    // Leave through a successful command, so `exit` exits zero.
    session.send(b"true\n");
    session.prompt(&format!("~/src/bshell main {marker} "));
    session.send(b"exit\n");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn the_stock_prompt_is_coloured_on_a_terminal() {
    let home = temp_dir("colour-home");
    std::fs::create_dir_all(home.join("src/bshell/.git")).expect("create git directory");
    std::fs::write(home.join("src/bshell/.git/HEAD"), b"ref: refs/heads/main\n")
        .expect("write HEAD");

    let mut session = Session::spawn_with(
        &home,
        &home.join("src/bshell"),
        &["--norc", "-i"],
        &[],
        false,
    );
    // The whole prompt is painted, not the marker alone: the path cyan, the
    // last component bright cyan, the branch green.
    session.prompt("\x1b[36m~/src\x1b[96m/bshell\x1b[0m \x1b[92mmain\x1b[0m ");
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
