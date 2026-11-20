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

/// `awk`, in the child the shell spawns for it.
///
/// It is here for one measured reason: bash-completion's scripts call `awk`
/// through `_comp_awk` in 150 of their files, and a Windows `PATH` has none, so
/// `make`, `python` and `curl` complete nothing without it
/// (`docs/portability.md` §6 has the subset awk-rs implements).
#[test]
fn awk_runs_without_a_path() {
    let output = run("printf 'a b\\nc d\\n' | awk '{print $2}'");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout(&output), "b\nd\n");

    let output = run("awk 'BEGIN { print 1 + 1 }'");
    assert_eq!(stdout(&output), "2\n");

    let output = run("printf 'x:y\\n' | awk -F: '{print $2}'");
    assert_eq!(stdout(&output), "y\n");

    // POSIX: the escapes in a `-v` value are processed like a string literal.
    let output = run("awk -v 'x=a\\tb' 'BEGIN { print \"[\" x \"]\" }'");
    assert_eq!(stdout(&output), "[a\tb]\n");

    // `sub` on a field, which is what most of the completion programs do.
    let output = run("printf 'ab\\n' | awk '{ sub(/b/, \"X\"); print }'");
    assert_eq!(stdout(&output), "aX\n");

    // The library calls it as `command awk` (`_comp_awk`), which has to reach
    // the embedded utility: `command` bypasses functions and aliases, not the
    // shell's own utilities.
    let output = run("printf 'a b\\n' | command awk '{print $1}'");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout(&output), "a\n");

    // The same question for the other shim, `_comp_tail`.
    let output = run("printf '1\\n2\\n' | command tail -n 1");
    assert_eq!(stdout(&output), "2\n");
}

#[test]
fn awk_reads_a_program_file() {
    let path = std::env::temp_dir().join(format!("baash-awk-{}.awk", std::process::id()));
    std::fs::write(&path, "BEGIN { print \"from file\" }\n").expect("write the program");
    let script = format!("awk -f {} </dev/null", path.display());
    let output = run(&script);
    let _ = std::fs::remove_file(&path);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout(&output), "from file\n");
}

/// `ENVIRON` is the shell's exported variables, which is why `awk` is isolated.
///
/// bash-completion's `make` helper reads `ENVIRON["prefix"]` and its caller
/// exports it (`local -x prefix=…`), so an inherited `std::env` would leave
/// every target out: the shell keeps its exports in its own table and only ever
/// hands them to a child explicitly.
#[test]
fn awk_sees_the_shells_exported_variables() {
    let output = run("export FOO=bar; awk 'BEGIN { print ENVIRON[\"FOO\"] }'");
    assert_eq!(stdout(&output), "bar\n");

    let output = run("f() { local -x prefix=al; awk 'BEGIN { print ENVIRON[\"prefix\"] }'; }; f");
    assert_eq!(stdout(&output), "al\n");

    let output = run("FOO=pre command awk 'BEGIN { print ENVIRON[\"FOO\"] }'");
    assert_eq!(stdout(&output), "pre\n");
}

/// An assignment operand would have to run between files, which awk-rs cannot
/// express (it runs one program over all inputs), so it is refused rather than
/// silently moved before `BEGIN`.
#[test]
fn awk_refuses_assignment_operands() {
    let output = run("awk '{print}' a=1 /dev/null");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("assignment operands"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = run("awk -f /nonexistent-baash-test.awk </dev/null");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cannot open program file"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
