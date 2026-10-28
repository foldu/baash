//! `wait` and `kill` argument handling that needs no child and no signal.
//!
//! The rest of `wait`/`kill` — statuses, job specs, the kill table — is
//! covered end to end by the spec suite, which runs the real binary in its own
//! process. A raised signal from this multi-threaded test binary could be
//! delivered to a thread that has not blocked it, so nothing here sends one.

use bsh_runtime::ShellHost;
use bsh_vm::Interp;

fn new_interp() -> Interp {
    let mut interp = Interp::new(false);
    interp.set_arg0("bsh");
    interp
}

/// A shell with the host `PATH`, so tests can spawn real utilities.
fn spawn_interp() -> Interp {
    use std::os::unix::ffi::OsStrExt;
    let mut interp = new_interp();
    if let Some(path) = std::env::var_os("PATH") {
        let _ = interp
            .state()
            .vars
            .assign_scalar(b"PATH", path.as_os_str().as_bytes());
    }
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
    assert_eq!(run(&mut interp, "wait %nope"), 127);
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

use std::sync::{Mutex, MutexGuard};

/// `waitpid(-1)` is process-wide, so two shells in this test binary must not
/// reap each other's children. Tests that spawn a child take this lock.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Run `script`, which must write its `jobs` output to `path`.
fn capture(interp: &mut Interp, script: &str) -> String {
    assert_eq!(run(interp, script), 0, "script failed: {script}");
    match std::fs::read_to_string(std::env::temp_dir().join("bsh-jobs-test-output")) {
        Ok(text) => {
            let _ = std::fs::remove_file(std::env::temp_dir().join("bsh-jobs-test-output"));
            text
        }
        Err(error) => panic!("no output file from {script}: {error}"),
    }
}

fn output_path() -> &'static str {
    // One path is enough because every capturing test holds SERIAL.
    "/tmp/bsh-jobs-test-output"
}

#[test]
fn jobs_lists_a_running_background_job() {
    let _serial = serial();
    let mut interp = spawn_interp();
    let text = capture(
        &mut interp,
        "sleep 0.4 & jobs > /tmp/bsh-jobs-test-output; wait",
    );
    assert_eq!(text, "[1]+  Running                    sleep 0.4 &\n");
}

#[test]
fn jobs_l_adds_the_pid_and_jobs_p_prints_only_the_pgid() {
    let _serial = serial();
    let mut interp = spawn_interp();
    assert_eq!(
        run(
            &mut interp,
            "sleep 0.4 & jobs -l > /tmp/bsh-jobs-test-output; \
             jobs -p > /tmp/bsh-jobs-test-output2; wait",
        ),
        0
    );
    let text = std::fs::read_to_string(output_path()).unwrap();
    let _ = std::fs::remove_file(output_path());
    // `[1]+ `, a five-wide pid, a space, the status field, the command.
    let rest = text
        .strip_prefix("[1]+ ")
        .unwrap_or_else(|| panic!("job prefix: {text:?}"));
    let (pid_field, rest) = rest.split_at(5);
    let pid: i32 = pid_field
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("pid field {pid_field:?} in {text:?}"));
    assert!(pid > 0);
    assert_eq!(rest, " Running                    sleep 0.4 &\n");

    let pgid: i32 = std::fs::read_to_string("/tmp/bsh-jobs-test-output2")
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let _ = std::fs::remove_file("/tmp/bsh-jobs-test-output2");
    assert_eq!(pgid, pid);
}

#[test]
fn jobs_filters_running_and_stopped() {
    let _serial = serial();
    let mut interp = spawn_interp();
    let running = capture(
        &mut interp,
        "sleep 0.4 & jobs -r > /tmp/bsh-jobs-test-output; wait",
    );
    assert!(running.contains("Running"));
    let stopped = capture(
        &mut interp,
        "sleep 0.4 & jobs -s > /tmp/bsh-jobs-test-output; wait",
    );
    assert_eq!(stopped, "");
}

#[test]
fn job_specs_match_prefix_and_substring() {
    let _serial = serial();
    let mut interp = spawn_interp();
    assert_eq!(
        run(
            &mut interp,
            "sleep 2 & jobs %sleep > /tmp/bsh-jobs-test-output; \
             jobs '%?eep' > /tmp/bsh-jobs-test-output2",
        ),
        0
    );
    let prefix = std::fs::read_to_string(output_path()).unwrap();
    assert!(prefix.contains("sleep 2 &"), "{prefix}");
    let substring = std::fs::read_to_string("/tmp/bsh-jobs-test-output2").unwrap();
    assert!(substring.contains("sleep 2 &"), "{substring}");
    let _ = std::fs::remove_file("/tmp/bsh-jobs-test-output2");
    assert_eq!(run(&mut interp, "kill %1; wait"), 0);
}

#[test]
fn job_specs_resolve_current_previous_and_number() {
    let _serial = serial();
    let mut interp = spawn_interp();
    assert_eq!(
        run(
            &mut interp,
            "sleep 2 & sleep 3 & \
             jobs %% > /tmp/bsh-jobs-test-output; \
             jobs %- > /tmp/bsh-jobs-test-output2; \
             jobs %+ > /tmp/bsh-jobs-test-output3; \
             jobs %1 > /tmp/bsh-jobs-test-output4",
        ),
        0
    );
    assert!(
        std::fs::read_to_string(output_path())
            .unwrap()
            .starts_with("[2]+")
    );
    let previous = std::fs::read_to_string("/tmp/bsh-jobs-test-output2").unwrap();
    assert!(previous.starts_with("[1]-"), "{previous}");
    let current = std::fs::read_to_string("/tmp/bsh-jobs-test-output3").unwrap();
    assert!(current.starts_with("[2]+"), "{current}");
    let numbered = std::fs::read_to_string("/tmp/bsh-jobs-test-output4").unwrap();
    assert!(numbered.starts_with("[1]-"), "{numbered}");
    for path in [
        "/tmp/bsh-jobs-test-output2",
        "/tmp/bsh-jobs-test-output3",
        "/tmp/bsh-jobs-test-output4",
    ] {
        let _ = std::fs::remove_file(path);
    }
    assert_eq!(run(&mut interp, "kill %1 %2; wait"), 0);
}

#[test]
fn bad_specs_and_options_are_statuses() {
    let _serial = serial();
    let mut interp = spawn_interp();
    assert_eq!(run(&mut interp, "jobs %9"), 1);
    assert_eq!(run(&mut interp, "jobs -x"), 2);
    assert_eq!(run(&mut interp, "fg %9"), 1);
    assert_eq!(run(&mut interp, "bg %9"), 1);
    assert_eq!(run(&mut interp, "disown %9"), 1);
    assert_eq!(run(&mut interp, "fg"), 1);
    assert_eq!(run(&mut interp, "bg"), 1);
    assert_eq!(run(&mut interp, "disown"), 1);
    assert_eq!(run(&mut interp, "disown -x"), 2);
}

#[test]
fn disown_removes_the_job_from_jobs_and_wait() {
    let _serial = serial();
    let mut interp = spawn_interp();
    assert_eq!(
        run(
            &mut interp,
            "sleep 0.3 & disown %1; \
             jobs > /tmp/bsh-jobs-test-output; echo st=$?",
        ),
        0
    );
    assert_eq!(std::fs::read_to_string(output_path()).unwrap(), "");
    let _ = std::fs::remove_file(output_path());
    // A disowned child is no longer a job, so `wait` has nothing to say.
    assert_eq!(run(&mut interp, "wait"), 0);
}

#[test]
fn monitor_off_keeps_the_old_wait_semantics() {
    let _serial = serial();
    let mut interp = spawn_interp();
    // `wait` with no operand always says 0 ("the status is lost").
    assert_eq!(run(&mut interp, "false & wait"), 0);
    // `wait PID` reports the real status exactly once.
    assert_eq!(run(&mut interp, "sh -c 'exit 3' & wait $!"), 3);
    assert_eq!(run(&mut interp, "wait $!"), 127);
    // `$!` is the last background pid.
    assert_eq!(run(&mut interp, "sleep 0.2 & echo $!"), 0);
    assert_eq!(run(&mut interp, "wait"), 0);
}
