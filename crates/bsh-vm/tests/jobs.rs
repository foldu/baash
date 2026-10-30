//! `wait` and `kill` argument handling that needs no child and no signal.
//!
//! The rest of `wait`/`kill` — statuses, the kill table — is covered end to
//! end by the spec suite, which runs the real binary in its own process. A raised signal from this multi-threaded test binary could be
//! delivered to a thread that has not blocked it, so nothing here sends one.

use bsh_vm::Interp;

fn new_interp() -> Interp {
    let mut interp = Interp::new(false);
    interp.set_arg0("bsh");
    interp
}

fn run(interp: &mut Interp, script: &str) -> i32 {
    interp.run_named("<test>", script.as_bytes()).exit_code()
}

#[test]
fn wait_without_children_succeeds_and_bad_operands_fail() {
    let mut interp = new_interp();
    assert_eq!(run(&mut interp, "wait"), 0);
    assert_eq!(run(&mut interp, "wait zzz"), 2);
    // Not a child of this shell: 127, the same code bash overloads as "no
    // such job".
    assert_eq!(run(&mut interp, "wait 4194304"), 127);
    // `%` operands are the job-control gap, not a missing job: capability
    // error, status 1.
    assert_eq!(run(&mut interp, "wait %nope"), 1);
    assert_eq!(run(&mut interp, "kill %1"), 1);
}

#[test]
fn kill_checks_arguments_without_signalling_anything() {
    let mut interp = new_interp();
    // Signal 0 is the existence check: `$$` is this very process.
    assert_eq!(run(&mut interp, "kill -0 $$"), 0);
    // A signal that does not exist is an operand error, not a crash.
    assert_eq!(run(&mut interp, "kill -9999 $$"), 1);
    assert_eq!(run(&mut interp, "kill -s NOSUCH $$"), 1);
    // A signal name in operand position is not a pid. The second operand is a
    // pid that cannot exist, so nothing is ever signalled — sending a real
    // signal from this multi-threaded test process would be exactly the hazard
    // the module docs warn about.
    assert_eq!(run(&mut interp, "kill HUP 4194304"), 1);
    assert_eq!(run(&mut interp, "kill -TERM 4194304"), 1);
}
