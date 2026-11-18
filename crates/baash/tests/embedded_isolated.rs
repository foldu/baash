//! The embedded utilities that have to own a process.
//!
//! `grep`, `sed`, `find` and `xargs` run as *children* of the shell
//! (`baash_utils::ISOLATED`), because their bodies do things a shell may not:
//! `sed` exits from inside `uumain` when it is given no arguments, and
//! `uucore`'s `#[uucore::main]` turns `SIGPIPE` back to its default on the way
//! in — which, in-process, makes the *shell* die on the next broken pipe.
//! These tests drive the real binary, because that is the only way to see a
//! child.
//!
//! Unix-only: the scripts use `/dev/null`, and on Windows the table is forced
//! on by a target dependency rather than by this feature (`docs/windows.md`
//! has the guest procedure the Windows side is verified with).

#![cfg(all(unix, feature = "builtin-utils"))]
#![forbid(unsafe_code)]

use std::io::Read;
use std::process::{Command, Output, Stdio};

fn run(script: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_baash"))
        .arg("-c")
        .arg(script)
        // An empty `PATH` is what makes the embedded table the only source of
        // these utilities: resolution finds nothing and the fallback runs.
        .env("PATH", "")
        .output()
        .expect("spawn baash")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn grep_sed_find_and_xargs_run_without_a_path() {
    let output = run("printf 'one\\ntwo\\n' | grep two");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout(&output), "two\n");

    // No match is status 1, and the message for a bad pattern names `grep` —
    // the child's `argv[0]`, not the shell's.
    let output = run("printf 'one\\n' | grep three");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    let output = run("echo x | grep '['");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("grep: invalid pattern"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = run("printf 'a b\\n' | sed 's/a/X/'");
    assert_eq!(stdout(&output), "X b\n");

    let output = run("find . -maxdepth 0");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout(&output), ".\n");

    let output = run("printf 'a b\\n' | xargs -n1");
    assert_eq!(stdout(&output), "a\nb\n");

    // Both in one pipeline: the shell applies the redirections, the children
    // write where the shell pointed them.
    let output = run("printf 'a\\nb\\n' | grep b | sed 's/b/B/'");
    assert_eq!(stdout(&output), "B\n");
}

#[test]
fn sed_with_no_arguments_does_not_end_the_shell() {
    // uutils sed calls `process::exit(1)` from inside `uumain` when it is
    // handed no script at all. In-process that is the *shell* exiting, which
    // is why the body runs in a child; the status is still sed's.
    let output = run("sed >/dev/null 2>&1; echo \"alive=$?\"");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout(&output), "alive=1\n");
}

#[test]
fn a_broken_pipe_does_not_kill_the_shell_after_an_embedded_utility() {
    // The invariant the shell's `write_out` depends on: `SIGPIPE` ignored, so
    // a write fails with `EPIPE`. The uutils bodies ask for the opposite on the
    // way in (die on a broken pipe), and a child's disposition dies with it.
    let mut child = Command::new(env!("CARGO_BIN_EXE_baash"))
        .arg("-c")
        .arg("grep -c x < /dev/null > /dev/null; i=0; while (( i < 200000 )); do echo x; (( i++ )); done")
        .env("PATH", "")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn baash");
    let mut pipe = child.stdout.take().expect("piped stdout");
    let mut byte = [0u8; 1];
    let _ = pipe.read(&mut byte);
    // Closing the reader is the test: the shell's next write has no reader.
    drop(pipe);
    let status = child.wait().expect("wait for baash");
    assert_eq!(
        status.code(),
        Some(0),
        "the shell must survive a reader that left, not die of SIGPIPE ({status:?})"
    );
}
