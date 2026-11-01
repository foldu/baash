//! Trap semantics: `EXIT`, `ERR`, `DEBUG`, `RETURN`, registration and reset.
//!
//! Deliberately **no raised signals and no forks**: this binary runs its tests
//! on several threads in one process, and a process-directed signal can be
//! delivered to a thread that has not blocked it (the default action of
//! `SIGUSR1` would kill the harness). Signal *delivery* is covered end to end
//! by the spec suite, which runs the real single-threaded binary; the
//! disposition machinery is covered in `baash-sys`. What is left here — the
//! execution points, `$?` rules and the table itself — needs neither.
//!
//! Each test builds one script, because a non-interactive `run_named` is "one
//! shell invocation": its EXIT trap runs when that chunk ends.

use std::path::PathBuf;

use baash_lang::Vars;
use baash_runtime::{ShellHost, TrapAction, TrapKey};
use baash_vm::Interp;

fn new_interp() -> Interp {
    let mut interp = Interp::new(false);
    interp.set_arg0("baash");
    interp
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("baash-trap-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn value(interp: &Interp, name: &[u8]) -> Option<String> {
    Vars::get(interp, name).map(|bytes| String::from_utf8_lossy(bytes).into_owned())
}

fn run(interp: &mut Interp, script: &str) -> i32 {
    interp.run_named("<test>", script.as_bytes()).exit_code()
}

#[test]
fn exit_trap_sees_the_pending_status_and_preserves_it() {
    let mut interp = new_interp();
    let status = run(&mut interp, "trap 'seen=$?' EXIT; false");
    assert_eq!(status, 1, "the trap does not replace the pending status");
    assert_eq!(
        value(&interp, b"seen").as_deref(),
        Some("1"),
        "$? inside the EXIT trap is the pending status"
    );
}

#[test]
fn exit_trap_runs_for_every_exit_path() {
    let mut interp = new_interp();
    assert_eq!(run(&mut interp, "trap 'ran=1' EXIT"), 0);
    assert_eq!(value(&interp, b"ran").as_deref(), Some("1"));

    // `exit N` keeps N unless the handler exits.
    let mut interp = new_interp();
    assert_eq!(run(&mut interp, "trap 'true' EXIT; exit 7"), 7);

    // The handler's own status never leaks.
    let mut interp = new_interp();
    assert_eq!(run(&mut interp, "trap 'false' EXIT; true"), 0);

    // The handler may clear the trap and exit with a saved status.
    let mut interp = new_interp();
    assert_eq!(
        run(
            &mut interp,
            "trap 'st=$?; trap - EXIT; true; exit $st' EXIT; exit 7"
        ),
        7
    );

    // ... or replace the status entirely.
    let mut interp = new_interp();
    assert_eq!(run(&mut interp, "trap 'exit 42' EXIT; exit 7"), 42);
}

#[test]
fn bare_exit_in_the_exit_trap_keeps_the_saved_status() {
    // bash keeps a saved exit value for exactly this: `true; exit` must not
    // downgrade the status to 0.
    let mut interp = new_interp();
    assert_eq!(run(&mut interp, "trap 'true; exit' EXIT; exit 3"), 3);

    // The same rule applies through a function called by the trap.
    let mut interp = new_interp();
    let status = run(
        &mut interp,
        "cleanup() { ran=1; true; exit; }\ntrap cleanup EXIT\nfalse",
    );
    assert_eq!(status, 1);
    assert_eq!(value(&interp, b"ran").as_deref(), Some("1"));
}

#[test]
fn exit_trap_runs_after_a_set_e_exit_and_after_eval() {
    let mut interp = new_interp();
    assert_eq!(
        run(
            &mut interp,
            "set -e; trap 'ran=$?' EXIT; eval 'echo hi'; false; echo no"
        ),
        1
    );
    assert_eq!(value(&interp, b"ran").as_deref(), Some("1"));
}

#[test]
fn err_trap_fires_outside_errexit_ignore_contexts() {
    let mut interp = new_interp();
    let status = run(
        &mut interp,
        r#"n=0
trap 'n=$((n + 1))' ERR
false
if false; then :; fi
while false; do :; done
false || false
false && false
true
false
"#,
    );
    // `false` (2), `false || false` (1: the list failed and the last operand
    // ran), and the final `false` (1).
    assert_eq!(status, 1);
    assert_eq!(value(&interp, b"n").as_deref(), Some("3"));
}

#[test]
fn err_trap_then_errexit_still_exits_with_the_command_status() {
    let mut interp = new_interp();
    let status = run(
        &mut interp,
        "set -e; trap 'fired=1' ERR; false; echo not-reached",
    );
    assert_eq!(status, 1);
    assert_eq!(value(&interp, b"fired").as_deref(), Some("1"));
}

#[test]
fn debug_trap_runs_before_each_simple_command_and_keeps_dollar_question() {
    let mut interp = new_interp();
    let status = run(
        &mut interp,
        "trap 'count=$((count + 1))' DEBUG; true; true; false",
    );
    // The trap itself is not traced, and neither handler command re-enters.
    assert_eq!(value(&interp, b"count").as_deref(), Some("3"));
    assert_eq!(status, 1, "the DEBUG handler's success must not clobber $?");
}

#[test]
fn a_failing_debug_trap_is_ignored_but_respects_errexit() {
    let mut interp = new_interp();
    assert_eq!(run(&mut interp, "trap 'false' DEBUG; true"), 0);

    let mut interp = new_interp();
    assert_eq!(
        run(&mut interp, "set -e; trap 'false' DEBUG; true"),
        1,
        "under errexit a failing DEBUG trap is fatal"
    );
}

#[test]
fn return_trap_runs_when_a_sourced_script_returns() {
    let dir = temp_dir("return");
    let script = dir.join("part.sh");
    std::fs::write(&script, b"echo sourced\nreturn 42\n").unwrap();
    let mut interp = new_interp();
    let status = run(
        &mut interp,
        &format!(
            "trap 'seen=$?; rt=1' RETURN; . '{}'; after=$?",
            script.display()
        ),
    );
    assert_eq!(status, 0, "the status of the assignment is 0");
    assert_eq!(value(&interp, b"rt").as_deref(), Some("1"));
    assert_eq!(
        value(&interp, b"seen").as_deref(),
        Some("42"),
        "the handler sees the script's return status"
    );
    assert_eq!(
        value(&interp, b"after").as_deref(),
        Some("42"),
        "and does not replace it"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn trap_registers_ignores_and_resets_by_bash_rules() {
    let mut interp = new_interp();
    run(&mut interp, "trap 'echo x' INT; trap '' TERM; trap - HUP");
    assert_eq!(
        interp.state().traps.get(TrapKey::Signal(2)),
        Some(&TrapAction::Command(b"echo x".to_vec()))
    );
    assert_eq!(
        interp.state().traps.get(TrapKey::Signal(15)),
        Some(&TrapAction::Ignore)
    );
    assert!(interp.state().traps.get(TrapKey::Signal(1)).is_none());

    // A lone reset operand, several of them, and the surprising bash rule that
    // a *signal-shaped first operand* makes every operand a signal to reset.
    let mut interp = new_interp();
    run(
        &mut interp,
        "trap 'echo x' EXIT; trap 'echo i' INT; trap 0 2; trap",
    );
    assert!(interp.state().traps.get(TrapKey::Exit).is_none());
    assert!(interp.state().traps.get(TrapKey::Signal(2)).is_none());

    let mut interp = new_interp();
    run(
        &mut interp,
        "trap 'echo i' INT; trap 'echo e' EXIT; trap 2 EXIT",
    );
    assert!(interp.state().traps.get(TrapKey::Signal(2)).is_none());
    assert!(interp.state().traps.get(TrapKey::Exit).is_none());
}

#[test]
fn trap_operand_errors_are_statuses_not_fatal() {
    let mut interp = new_interp();
    // A bad signal in the *set* form: status 1, the shell keeps going.
    let status = run(&mut interp, "trap 'echo x' SIGINVALID; reached=1");
    assert_eq!(status, 0);
    assert_eq!(value(&interp, b"reached").as_deref(), Some("1"));

    // A lone non-signal operand is a usage error, and still not fatal.
    let mut second = new_interp();
    let status = run(&mut second, "trap foo; reached=1");
    assert_eq!(status, 0);
    assert_eq!(value(&second, b"reached").as_deref(), Some("1"));

    // An invalid signal with no action is an error at exit; the diagnostic is
    // stderr, so only the status is asserted here.
    let mut third = new_interp();
    assert_eq!(run(&mut third, "trap - SIGINVALID"), 1);

    // An action that does not parse is rejected at registration time.
    let mut fourth = new_interp();
    assert_eq!(run(&mut fourth, "trap 'echo <' EXIT; status=$?"), 0);
    assert_eq!(value(&fourth, b"status").as_deref(), Some("1"));
    assert!(fourth.state().traps.get(TrapKey::Exit).is_none());
}
