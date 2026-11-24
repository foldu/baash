//! Interactive signal behaviour on a real pseudo-terminal.
//!
//! `Ctrl-C` is a terminal-generated `SIGINT` for the foreground process group.
//! With no job control the shell and its foreground child share the shell's
//! group, so the child dies and the shell must survive the wait and report
//! status 130. This is the surviving part of the old job-control pty test: the
//! `^Z`/`jobs`/`fg`/`bg` half was deleted with job control.
//!
//! The second half of the promise is bash's *abort*: `while true; do echo lol;
//! done` must stop, not outlive the key that asked it to. The shell blocks
//! `SIGINT`, so nothing can interrupt it asynchronously; the pending signal is
//! drained at a command boundary, a foreground child that dies of `SIGINT` is
//! recorded at the wait, and the evaluator unwinds to the interactive reader
//! with status 130. The tests below pin every shape that has to keep or lose
//! the session: a builtin loop, a loop that runs a child, `trap '' INT`,
//! `trap 'echo …' INT`, a `$( … )` body, a redirected loop, and a script that
//! must still die.

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
        Self::spawn_with(&["--norc", "-i"])
    }

    /// Same session with explicit shell arguments, still under `PS1=@@ `.
    fn spawn_with(args: &[&str]) -> Self {
        Self::spawn_in(None, args)
    }

    /// Same session, started in `dir` when one is given.
    ///
    /// The test that reads the *machine's* startup files uses this to start
    /// somewhere a per-directory hook has nothing to do: a hook like `direnv`'s
    /// answers for the current directory, and a test that inherits the
    /// checkout's cwd makes it run the project's build tool at every prompt.
    fn spawn_in(dir: Option<&std::path::Path>, args: &[&str]) -> Self {
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

        let directory = dir.map(<std::path::Path>::to_path_buf);
        let child = fork_child_status(move || {
            if let Some(dir) = &directory
                && std::env::set_current_dir(dir).is_err()
            {
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
            let mut argv = vec!["baash"];
            argv.extend_from_slice(args);
            let plan = match SpawnPlan::new_with_env(
                binary,
                argv,
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

    /// Wait until the shell is reading lines, without looking at the prompt.
    ///
    /// [`Session::prompt`] is what every other test uses, but the one test that
    /// runs the *machine's own* startup files cannot: a `.bashrc` that assigns
    /// `PS1` — Ubuntu's does — replaces the marker this harness plants in the
    /// environment, and a hook in it may take seconds to answer (`direnv`
    /// exporting a flake). Arithmetic has one right answer that the echoed
    /// input does not contain, so the output itself is the signal.
    fn ready(&mut self, timeout: Duration) {
        self.line("echo $((40+2))");
        self.expect(b"42", timeout);
    }

    fn line(&mut self, text: &str) {
        self.send(text.as_bytes());
        self.send(b"\n");
    }

    /// Drain for `millis` and report how many bytes arrived.
    ///
    /// The loop tests use this where the output is *not* allowed to stop: an
    /// ignored `INT` keeps producing, and a resumed `INT` trap keeps producing
    /// after the handler's line. The spawning prompt and the handler itself are
    /// well under a thousand bytes, so the threshold separates "still looping"
    /// from "back at a prompt" without counting exact iterations.
    fn output_for(&mut self, millis: u64) -> usize {
        let before = self.output.len();
        let deadline = Instant::now() + Duration::from_millis(millis);
        while Instant::now() < deadline {
            self.drain();
            std::thread::sleep(Duration::from_millis(10));
        }
        self.output.len() - before
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

/// `Ctrl-C` while the shell itself waits inside one command must end *that
/// wait*, not merely the line after it.
///
/// The editor turns the terminal's keyboard signals off for the line, but a
/// command runs with the terminal back in cooked mode — that is what lets
/// `read` use the line discipline — so the same key is a `SIGINT` for the
/// foreground process group, and that group is the shell's own. With the
/// default disposition the shell died; blocking the signal kept the session but
/// let the builtin run to completion, which left `while true; do echo lol;
/// done` printing forever. The block is still what keeps the session alive, so
/// the wait asks the signal source between slices (`Interp::probe_interrupt`)
/// instead of being interrupted by the kernel: that is what ends a `read`, and
/// it is bash's behaviour with bash's asynchronous handler replaced by the poll.
///
/// The `-t` is far longer than the test, so the prompt can only come from the
/// key: `done-42` must not appear, because the `echo` is part of the aborted
/// line.
#[test]
fn ctrl_c_ends_a_read_that_is_waiting_for_a_line() {
    let mut session = Session::spawn();
    session.prompt();

    session.line("read -t 10 ignored; echo done-$((6*7))");
    settle();
    session.send(b"\x03");
    // The `^C` the terminal echoes when it turns the key into `SIGINT`, then the
    // prompt: waiting for the prompt alone can match a frame the editor painted
    // *before* the key, and a line typed then becomes the read's input. Three
    // seconds, where the read's own timeout is ten: this can only pass if the
    // interrupt ended it.
    session.expect(b"^C", Duration::from_secs(3));
    session.expect(PROMPT, Duration::from_secs(3));
    session.line("echo status=$?");
    session.expect(b"status=130", Duration::from_secs(10));
    session.prompt();
    assert!(
        find(&session.output, b"done-42").is_none(),
        "the rest of the interrupted line ran: {}",
        String::from_utf8_lossy(&session.output)
    );

    session.line("echo still-here");
    session.expect(b"still-here", Duration::from_secs(10));
    session.prompt();

    session.line("exit");
    session.wait_child();
}

/// A loop whose body has no command at all still answers `Ctrl-C`.
///
/// `while ((1)); do ((i++)); done` never crosses a command boundary, so the
/// latch that a boundary fills is never filled; bash's `QUIT` macro checks
/// `interrupt_state` on each iteration and throws to the top level, and this is
/// the polled equivalent: the loop top asks the signal source every so many
/// iterations (`SIGNAL_POLL_STRIDE`). Before that, the register moved and the
/// key did nothing at all.
#[test]
fn ctrl_c_aborts_a_loop_whose_body_has_no_command() {
    let mut session = Session::spawn();
    session.prompt();

    session.line("while ((1)); do ((i++)); done");
    settle();
    session.send(b"\x03");
    session.expect(b"^C", Duration::from_secs(5));
    session.expect(PROMPT, Duration::from_secs(5));
    session.line("echo status=$?");
    session.expect(b"status=130", Duration::from_secs(10));
    session.prompt();

    session.line("echo still-here");
    session.expect(b"still-here", Duration::from_secs(10));
    session.prompt();

    session.line("exit");
    session.wait_child();
}

/// `Ctrl-C` ends a `wait` whose child ignores the key.
///
/// `wait` is a builtin, not a foreground command: bash's "ignore `SIGINT` while
/// waiting for a child" rule does not apply to it, and bash's handler makes
/// `waitpid` return `EINTR`. The shell blocks the signal instead, so the wait
/// polls and stops. The child here ignores `SIGINT` so that nothing *else* can
/// end the wait — without the poll, the shell would hang until the child's
/// sleep ran out.
#[test]
fn ctrl_c_ends_a_wait_whose_child_ignores_the_key() {
    let mut session = Session::spawn();
    session.prompt();

    session.line("sh -c 'trap \"\" INT; sleep 300' &");
    session.prompt();

    session.line("wait");
    settle();
    session.send(b"\x03");
    session.expect(b"^C", Duration::from_secs(3));
    session.expect(PROMPT, Duration::from_secs(3));
    session.line("echo status=$?");
    session.expect(b"status=130", Duration::from_secs(10));
    session.prompt();

    // Do not leave a 300-second `sleep` behind: `$!` still names the child.
    session.line("kill $! 2>/dev/null");
    session.prompt();

    session.line("exit");
    session.wait_child();
}

/// A trapped `INT` stops a blocked `sleep` and the line resumes.
///
/// The probe has three answers (`Interrupt`): nothing for an ignored `INT`, an
/// abort latch for an untrapped one, and "stop the wait but run the trap" for a
/// trapped one. This is the third: the sleep stops where bash's `SIGINT` stops
/// its `sleep` child, the trap runs at the next boundary, and the command after
/// the sleep still runs.
#[cfg(feature = "builtin-utils")]
#[test]
fn an_int_trap_stops_a_blocked_sleep_and_the_line_resumes() {
    let mut session = Session::spawn();
    session.prompt();

    // The embedded `sleep`: an external one dies of the terminal's `SIGINT` and
    // the shell *discards* that signal while it waits, so no trap would run.
    session.line("PATH=/nonexistent");
    session.line("trap 'echo caught' INT; sleep 10; echo after-$((6*7))");
    settle();
    session.send(b"\x03");
    session.expect(b"caught", Duration::from_secs(3));
    session.expect(b"after-42", Duration::from_secs(3));
    session.prompt();

    session.line("echo still-here");
    session.expect(b"still-here", Duration::from_secs(10));
    session.prompt();

    session.line("exit");
    session.wait_child();
}

/// `Ctrl-C` ends a `sleep` the shell runs itself.
///
/// On Windows a `PATH` has no `sleep.exe`, so the embedded coreutils `sleep`
/// runs on a worker thread and the shell waits for it; on Unix the same happens
/// when `PATH` cannot resolve one. The wait is sliced and asks the interpreter,
/// so the key ends the sleep where bash's `SIGINT` ends its `sleep` child. The
/// alternative was to wait the whole duration out, which is what the guest
/// showed before this.
#[cfg(feature = "builtin-utils")]
#[test]
fn ctrl_c_ends_an_embedded_sleep() {
    let mut session = Session::spawn();
    session.prompt();

    // Force the embedded table: the harness's `PATH` has the host's `sleep`,
    // which would be a child and would die of the terminal's own `SIGINT`.
    session.line("PATH=/nonexistent");
    session.line("sleep 0.1; echo embedded-$((6*7))");
    session.expect(b"embedded-42", Duration::from_secs(10));
    session.prompt();

    session.line("sleep 10; echo done-$((6*7))");
    settle();
    session.send(b"\x03");
    session.expect(b"^C", Duration::from_secs(3));
    session.expect(PROMPT, Duration::from_secs(3));
    session.line("echo status=$?");
    session.expect(b"status=130", Duration::from_secs(10));
    session.prompt();
    assert!(
        find(&session.output, b"done-42").is_none(),
        "the rest of the interrupted line ran: {}",
        String::from_utf8_lossy(&session.output)
    );

    session.line("echo still-here");
    session.expect(b"still-here", Duration::from_secs(10));
    session.prompt();

    session.line("exit");
    session.wait_child();
}

/// The reported case: a loop of builtins must stop and leave `$?` at 130.
#[test]
fn ctrl_c_aborts_a_builtin_loop_and_keeps_the_session() {
    let mut session = Session::spawn();
    session.prompt();

    session.line("while true; do echo lol; done");
    session.expect(b"lol", Duration::from_secs(10));
    settle();
    session.send(b"\x03");
    session.prompt();
    session.line("echo status=$?");
    session.expect(b"status=130", Duration::from_secs(10));
    session.prompt();

    // The session kept its editor, its variables and its history.
    session.line("echo still-here");
    session.expect(b"still-here", Duration::from_secs(10));
    session.prompt();

    session.line("exit");
    session.wait_child();
}

/// A loop that runs a foreground child per iteration.
///
/// This is the other shape: the shell is inside `wait`, `SIGINT` is ignored for
/// the duration (bash's rule, so no `INT` trap runs), and the `sleep` dies of
/// the terminal's signal. Without a check for that status the loop would spawn
/// the next `sleep` for ever — `^C^C^C` would kill one child per key and never
/// the loop.
#[test]
fn ctrl_c_aborts_a_loop_that_runs_a_child_each_iteration() {
    let mut session = Session::spawn();
    session.prompt();

    session.line("while true; do sleep 5; done");
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

/// `trap '' INT` is `SIG_IGN`, inherited by the child too: the loop must keep
/// running and the shell must keep the ignored disposition.
#[test]
fn an_ignored_int_leaves_the_loop_running() {
    let mut session = Session::spawn();
    session.prompt();

    session.line("trap '' INT; while true; do echo lol; done");
    session.expect(b"lol", Duration::from_secs(10));
    settle();
    session.send(b"\x03");
    let produced = session.output_for(600);
    assert!(
        produced > 1000,
        "the loop stopped {produced} bytes after an ignored Ctrl-C"
    );
    // No `exit`: the shell is still in the loop. `Drop` ends it with SIGTERM,
    // which the ignored `INT` does not cover.
}

/// `trap 'echo …' INT` runs the handler and then *resumes* the loop, exactly as
/// bash does. Only a `break` written in the handler stops it (which this shell
/// does not support from a trap; see the commit).
#[test]
fn an_int_trap_runs_and_the_loop_resumes() {
    let mut session = Session::spawn();
    session.prompt();

    // The marker is computed by the shell, so the editor's echo of the typed
    // `$((6*7))` cannot match it.
    session.line("trap 'echo TRAP-RAN-$((6*7))' INT; while true; do echo lol; done");
    session.expect(b"lol", Duration::from_secs(10));
    settle();
    session.send(b"\x03");
    session.expect(b"TRAP-RAN-42", Duration::from_secs(10));
    let produced = session.output_for(500);
    assert!(
        produced > 1000,
        "the loop did not resume after the handler: {produced} bytes"
    );
}

/// A `while` inside `$( … )` aborts the substitution, not the process, and the
/// rest of the line does not run — bash abandons the line here too.
#[test]
fn ctrl_c_inside_a_command_substitution_keeps_the_session() {
    let mut session = Session::spawn();
    session.prompt();

    session.line("x=$(while true; do echo lol; done); echo AFTER-$((6*7))");
    settle();
    session.send(b"\x03");
    session.prompt();
    session.line("echo status=$?");
    session.expect(b"status=130", Duration::from_secs(10));
    session.prompt();
    assert!(
        find(&session.output, b"AFTER-42").is_none(),
        "the rest of the interrupted line ran: {}",
        String::from_utf8_lossy(&session.output)
    );

    session.line("exit");
    session.wait_child();
}

/// A redirection on the interrupted command must be undone with the abort, or
/// the next prompt writes into the file.
#[test]
fn an_interrupted_loop_restores_its_redirection() {
    let path = std::env::temp_dir().join(format!("baash-ctrlc-redirect-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut session = Session::spawn();
    session.prompt();

    session.line(&format!(
        "while true; do echo lol; done > {}",
        path.display()
    ));
    settle();
    session.send(b"\x03");
    session.prompt();
    assert!(
        std::fs::metadata(&path)
            .map(|meta| meta.len() > 0)
            .unwrap_or(false),
        "the redirected loop produced no file output"
    );

    session.line("echo status=$?");
    session.expect(b"status=130", Duration::from_secs(10));
    // `status=130` is the proof that stdout points at the terminal again.
    session.prompt();
    session.line("exit");
    session.wait_child();
    let _ = std::fs::remove_file(&path);
}

/// `set -e` must not turn the abort into an exit: the status is 130, not a
/// failed command.
#[test]
fn errexit_does_not_exit_the_shell_on_ctrl_c() {
    let mut session = Session::spawn();
    session.prompt();

    session.line("set -e; while true; do echo lol; done");
    session.expect(b"lol", Duration::from_secs(10));
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

/// The default startup path: `-i` reads the shell's own config (and, on a
/// machine where a distributor installed one, that file instead). The first
/// report was that only this path died, so it gets its own case.
#[test]
fn the_default_startup_files_do_not_make_ctrl_c_fatal() {
    // The machine's own files run here, so neither the prompt nor the timing is
    // the test's: readiness is a command's output (see [`Session::ready`]).
    let dir = std::env::temp_dir().join(format!("baash-ctrlc-defaults-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("make a working directory");
    let mut session = Session::spawn_in(Some(&dir), &["-i"]);
    session.ready(Duration::from_secs(60));

    // Nothing in the temporary directory is a project, so a hook in the
    // startup files (`direnv`'s) has nothing to load here.

    session.line("while true; do echo lol; done");
    session.expect(b"lol", Duration::from_secs(10));
    settle();
    session.send(b"\x03");
    session.line("echo status=$?");
    session.expect(b"status=130", Duration::from_secs(10));
    // Still reading lines, which is what a shell that took the interrupt does.
    session.line("echo still=$((40+2))");
    session.expect(b"still=42", Duration::from_secs(10));
    session.line("exit");
    session.wait_child();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The cause of the death, hermetically: a startup file that runs a command
/// substitution. `$( … )` is an in-process scope; it emptied the signal mask
/// and the parent's `sync_traps` then saw an unchanged trap generation and
/// never put `SIGINT` back, so the next `Ctrl-C` in a loop had the default
/// action. No default config, no bash-completion: just one substitution.
#[test]
fn a_startup_file_command_substitution_does_not_leave_sigint_unblocked() {
    let path = std::env::temp_dir().join(format!("baash-ctrlc-rc-{}", std::process::id()));
    std::fs::write(&path, "x=$(echo hi)\n").expect("write the startup file");
    let path_text = path.to_str().expect("a UTF-8 temp path");
    let mut session = Session::spawn_with(&["--rcfile", path_text, "-i"]);
    session.prompt();

    session.line("while true; do echo lol; done");
    session.expect(b"lol", Duration::from_secs(10));
    settle();
    session.send(b"\x03");
    session.prompt();
    session.line("echo status=$?");
    session.expect(b"status=130", Duration::from_secs(10));
    session.prompt();
    session.line("exit");
    session.wait_child();
    let _ = std::fs::remove_file(&path);
}

/// A script is not interactive: `SIGINT` keeps its default action, and the
/// process must die rather than abort a command and carry on.
#[test]
fn a_non_interactive_shell_still_dies_on_ctrl_c() {
    let binary = env!("CARGO_BIN_EXE_baash");
    let mut plan = SpawnPlan::new(binary, ["baash", "-c", "while true; do echo lol; done"])
        .expect("build a spawn plan");
    // The loop's output is not the test's business.
    plan.open(b"/dev/null", baash_sys::flags::WRONLY, 0, 1)
        .expect("redirect stdout");
    plan.sigmask = Some(baash_sys::SignalSet::empty());
    plan.reset_signals =
        baash_sys::SignalSet::of(&[baash_sys::SIGPIPE, baash_sys::SIGINT, baash_sys::SIGQUIT]);
    let child = baash_sys::spawn(plan).expect("spawn the script shell");
    settle();
    send_signal(child.pid(), baash_sys::SIGINT).expect("send SIGINT");
    let status = baash_sys::wait_pid(child.pid()).expect("reap the script shell");
    assert_eq!(
        status,
        baash_sys::WaitOutcome::Signaled(baash_sys::SIGINT),
        "a non-interactive shell must die of SIGINT, not survive it"
    );
}
