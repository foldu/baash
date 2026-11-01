//! zoxide's bash integration, on a real pseudo-terminal.
//!
//! `zoxide init bash` is the third-party startup script a shell is measured by
//! in practice. It is not a toy: it defines `z` and `zi`, reads
//! `declare -p PROMPT_COMMAND` to decide between the scalar and array forms,
//! installs a hook that runs before every prompt, and the hook itself needs
//! `[[ -o history ]]`, `\builtin local -r`, `${@: -1}` and `${x:offset}`.
//!
//! The test drives the script the *installed* zoxide generates, not a vendored
//! copy, so a zoxide upgrade that changes it is what the test sees. In a
//! session: the hook records two directories, `z <keyword>` jumps to one of
//! them, `z -` comes back, and nothing on the way writes a diagnostic.
//!
//! `zoxide` comes from the flake's dev shell. Without it the test reports the
//! skip on stderr and does nothing, rather than failing a checkout that has no
//! zoxide installed.

#![forbid(unsafe_code)]

use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use baash_sys::{
    Pty, SpawnPlan, become_session_leader, close, dup2, exec, fork_child_status, open_pty,
    poll_readable, read_fd, send_signal, wait_pid, write_fd,
};

/// One interactive shell on a pty, with a scripted conversation.
///
/// The same shape as `interactive_prompt.rs`: this file asserts on what a user
/// would see on the terminal, so it needs a terminal.
struct Session {
    pty: Pty,
    child: i32,
    output: Vec<u8>,
    cursor: usize,
    reaped: bool,
}

/// Environment to drop before starting, so an inherited prompt, history or
/// zoxide configuration cannot change what the test measures.
const SCRUBBED: &[&[u8]] = &[
    b"PS1",
    b"PS2",
    b"PROMPT_COMMAND",
    b"TERM",
    b"HOME",
    b"LC_ALL",
    b"NO_COLOR",
    b"PWD",
    b"OLDPWD",
    // The test owns the database; a developer's `~/.local/share/zoxide` is
    // none of its business.
    b"_ZO_DATA_DIR",
    b"_ZO_ECHO",
    b"_ZO_DOCTOR",
    b"BAASH_HISTORY_DB",
];

impl Session {
    fn spawn(home: &Path, cwd: &Path, arguments: &[&str], environment: &[(&str, String)]) -> Self {
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
        entries.push(b"TERM=xterm".to_vec());
        entries.push(b"LC_ALL=C".to_vec());
        let mut home_entry = b"HOME=".to_vec();
        home_entry.extend_from_slice(home.as_os_str().as_bytes());
        entries.push(home_entry);
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
            let mut plan =
                match SpawnPlan::new_with_env(binary, argv, entries.iter().map(Vec::as_slice)) {
                    Ok(plan) => plan,
                    Err(_) => return 113,
                };
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

    /// Type a line and wait until it has run and the next prompt is painted.
    ///
    /// The line ends with a marker `echo`, so the wait cannot be satisfied by
    /// the editor repainting the line while it is typed; the prompt after the
    /// marker is painted only once `PROMPT_COMMAND` has run, which is what
    /// makes the zoxide hook observable without a sleep.
    fn line(&mut self, text: &str) {
        self.send(format!("{text}; echo \"MA\"\"RK\"\n").as_bytes());
        self.expect(b"MARK", Duration::from_secs(10));
        self.prompt();
    }

    /// Run `echo "MARK[<expression>]"` and return what it printed between the
    /// brackets.
    ///
    /// The marker is spelled in two pieces on the command line (`MA""RK`), so
    /// the text the editor paints cannot be mistaken for the output the
    /// assertions wait for.
    fn marker(&mut self, expression: &str) -> Vec<u8> {
        self.send(format!("echo \"MA\"\"RK[{expression}]\"\n").as_bytes());
        self.expect(b"MARK[", Duration::from_secs(10));
        let start = self.cursor;
        self.expect(b"]", Duration::from_secs(10));
        self.output[start..self.cursor - 1].to_vec()
    }

    fn wait_child(&mut self) {
        if self.reaped {
            return;
        }
        let status = wait_pid(self.child).expect("reap the shell");
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
            let _ = wait_pid(self.child);
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
    let path = std::env::temp_dir().join(format!("baash-zoxide-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("create temp dir");
    path
}

/// Whether the `zoxide` binary the flake's dev shell installs is available.
fn zoxide_installed() -> bool {
    Command::new("zoxide")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// The script `zoxide init bash` prints for the installed version.
fn zoxide_init() -> Vec<u8> {
    let output = Command::new("zoxide")
        .args(["init", "bash"])
        .output()
        .expect("run zoxide");
    assert!(
        output.status.success(),
        "zoxide init bash failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
fn zoxide_records_directories_and_jumps_between_them() {
    if !zoxide_installed() {
        eprintln!(
            "skipping the zoxide integration test: `zoxide` is not on PATH \
             (the flake's dev shell provides it)"
        );
        return;
    }

    let home = temp_dir("home");
    let work = temp_dir("work");
    let one = work.join("dir-one");
    let two = work.join("dir-two");
    std::fs::create_dir_all(&one).expect("create dir-one");
    std::fs::create_dir_all(&two).expect("create dir-two");
    let data = work.join("zoxide-data");
    std::fs::create_dir_all(&data).expect("create the database directory");

    let rc = work.join("zoxide-init.sh");
    std::fs::write(&rc, zoxide_init()).expect("write the init script");

    let mut session = Session::spawn(
        &home,
        &work,
        &["-i", "--rcfile", &rc.to_string_lossy()],
        &[
            ("PS1", "P> ".to_string()),
            // A database of its own, in the test's directory tree.
            ("_ZO_DATA_DIR", data.display().to_string()),
        ],
    );
    session.prompt();

    // Each `cd` is recorded by the hook that runs before the next prompt.
    session.line(&format!("cd {}", one.display()));
    session.line(&format!("cd {}", two.display()));
    session.line(&format!("cd {}", work.display()));

    // The hook wrote the database; ask zoxide itself rather than trusting the
    // prompt timing.
    let listing = String::from_utf8_lossy(&session.marker("$(zoxide query --list | tr '\\n' ' ')"))
        .into_owned();
    session.prompt();
    assert!(
        listing.contains("dir-one") && listing.contains("dir-two"),
        "the hook records every directory visited: {listing:?}"
    );

    // `z <keyword>` is zoxide's query plus a `cd`; the keyword is deliberately
    // not a path in the working directory, so the plain-`cd` fast path cannot
    // satisfy it.
    session.line("z two");
    let jumped = session.marker("$PWD");
    assert_eq!(
        String::from_utf8_lossy(&jumped),
        two.display().to_string(),
        "`z two` jumps to the recorded directory"
    );
    session.prompt();

    // `z -` is `$OLDPWD`, zoxide's other fast path.
    session.line("z -");
    let back = session.marker("$PWD");
    assert_eq!(
        String::from_utf8_lossy(&back),
        work.display().to_string(),
        "`z -` returns to the previous directory"
    );
    session.prompt();

    session.send(b"exit\n");
    session.wait_child();

    // A broken `PROMPT_COMMAND` makes zoxide print a configuration warning on
    // the first `z`; an unimplemented operator would surface as our own
    // diagnostic instead. Neither is acceptable in a clean session.
    let transcript = String::from_utf8_lossy(&session.output);
    assert!(
        !transcript.contains("zoxide: detected a possible configuration issue"),
        "the PROMPT_COMMAND hook is installed and runs: {transcript}"
    );
    assert!(
        !transcript.contains("BAASH-E"),
        "no shell diagnostic is expected: {transcript}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&work);
}
