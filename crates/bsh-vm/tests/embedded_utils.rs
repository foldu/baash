//! Embedded coreutils, end to end through the evaluator.
//!
//! The feature is off by default on Linux, so this file is empty unless it is
//! enabled (`cargo test --workspace --all-features` does). The interesting
//! assertion is the *ordering*: an executable in `PATH` must win over the
//! embedded table, because the feature is a fallback and not an override.

#![cfg(all(feature = "builtin-utils", unix))]
#![forbid(unsafe_code)]

use std::path::PathBuf;

use bsh_runtime::Status;
use bsh_vm::Interp;

fn shell() -> Interp {
    let mut interp = Interp::new(false);
    interp.set_arg0("bsh");
    interp
}

fn run(interp: &mut Interp, script: &str) -> Status {
    interp.run_named("<embedded-utils-test>", script.as_bytes())
}

fn temp_dir(tag: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bsh-embedded-utils-{tag}-{}-{count}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn executable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

#[test]
fn printf_runs_when_path_finds_nothing() {
    let dir = temp_dir("printf");
    let out = dir.join("out");
    let mut interp = shell();
    assert!(run(&mut interp, "PATH=''").is_success());
    let status = run(
        &mut interp,
        &format!("printf '%s-%s' a b > '{}'", out.display()),
    );
    assert!(status.is_success(), "printf failed: {status:?}");
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "a-b");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_utility_in_path_still_wins() {
    let dir = temp_dir("ordering");
    let external = dir.join("printf");
    std::fs::write(&external, "#!/bin/sh\nprintf external\n").unwrap();
    executable(&external);
    let out = dir.join("out");
    let mut interp = shell();
    assert!(run(&mut interp, &format!("PATH='{}'", dir.display())).is_success());
    let status = run(
        &mut interp,
        &format!("printf ignored > '{}'", out.display()),
    );
    assert!(status.is_success(), "printf failed: {status:?}");
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        "external",
        "the embedded printf must not shadow a PATH utility"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bracket_and_test_report_their_status() {
    let mut interp = shell();
    assert!(run(&mut interp, "PATH=''").is_success());
    assert_eq!(run(&mut interp, "[ 2 -gt 1 ]").exit_code(), 0);
    assert_eq!(run(&mut interp, "[ 1 -gt 2 ]").exit_code(), 1);
    assert_eq!(run(&mut interp, "test -z ''").exit_code(), 0);
    assert_eq!(run(&mut interp, "[ 1 = 1").exit_code(), 2);
}

#[test]
fn a_prefix_assignment_does_not_persist() {
    let dir = temp_dir("prefix");
    let first = dir.join("first");
    let second = dir.join("second");
    let mut interp = shell();
    assert!(run(&mut interp, "PATH=''").is_success());
    // The assignment is scoped to the utility. Word expansion happens before
    // the assignment takes effect (POSIX), so the observable is what happens
    // to the shell afterwards, not inside this call.
    assert!(
        run(
            &mut interp,
            &format!("GREETING=hi printf '%s' x > '{}'", first.display())
        )
        .is_success()
    );
    assert_eq!(std::fs::read_to_string(&first).unwrap(), "x");
    assert!(
        run(
            &mut interp,
            &format!(
                "printf '%s' \"${{GREETING-unset}}\" > '{}'",
                second.display()
            )
        )
        .is_success()
    );
    assert_eq!(
        std::fs::read_to_string(&second).unwrap(),
        "unset",
        "the prefix assignment must not leak into the shell"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_utility_that_is_not_a_builtin_runs_too() {
    let dir = temp_dir("cat");
    let file = dir.join("data");
    std::fs::write(&file, "one\ntwo\n").unwrap();
    let out = dir.join("out");
    let mut interp = shell();
    assert!(run(&mut interp, "PATH=''").is_success());
    let status = run(
        &mut interp,
        &format!("cat '{}' > '{}'", file.display(), out.display()),
    );
    assert!(status.is_success(), "cat failed: {status:?}");
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "one\ntwo\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn embedded_utilities_work_in_a_pipeline() {
    let dir = temp_dir("pipeline");
    let out = dir.join("out");
    let mut interp = shell();
    assert!(run(&mut interp, "PATH=''").is_success());
    let status = run(
        &mut interp,
        &format!("printf 'a\\nb\\nc\\n' | wc -l > '{}'", out.display()),
    );
    assert!(status.is_success(), "pipeline failed: {status:?}");
    let counted = std::fs::read_to_string(&out).unwrap();
    assert_eq!(counted.trim(), "3");
    let _ = std::fs::remove_dir_all(&dir);
}
