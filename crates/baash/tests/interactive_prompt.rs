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
    b"LC_ALL",
    b"NO_COLOR",
    // A stale inherited `PWD` would win over the directory the test chdirs to,
    // and `\w` reads the shell's `PWD`.
    b"PWD",
    b"OLDPWD",
];

impl Session {
    fn spawn(home: &Path, cwd: &Path, arguments: &[&str], environment: &[(&str, &str)]) -> Self {
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
        entries.push(b"TERM=xterm".to_vec());
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

#[test]
fn a_coloured_prompt_with_a_variable_and_a_substitution_keeps_the_cursor_right() {
    let home = temp_dir("colour-home");
    let work = temp_dir("colour-work");
    let prompt = "\\[\\e[1;32m\\]\\u@\\h\\[\\e[0m\\] \\w$(printf sub)\\$ ";
    let mut session = Session::spawn(&home, &work, &["--norc", "-i"], &[("PS1", prompt)]);
    session.prompt("sub$ ");

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
    suffix.extend_from_slice(b"sub$ ");
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
    session.line("echo ok", "sub$ ");
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

    // No PS1 in the environment or an rc file: the stock prompt is rendered.
    let mut session = Session::spawn(&home, &work, &["--norc", "-i"], &[]);
    session.prompt("~/src/bshell main ");
    // A failed command adds the status; a background child adds the job count
    // and the successful `&` clears the status again.
    session.send(b"false\n");
    session.prompt("~/src/bshell main ?1 ");
    session.send(b"sleep 1 &\n");
    session.prompt("~/src/bshell main !1 ");
    session.send(b"wait\n");
    session.prompt("~/src/bshell main ");
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
