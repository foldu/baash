//! Dispositions the shell sets on itself: `trap '' SIG` and the
//! `SIGINT`/`SIGQUIT` ignore around a foreground wait.
//!
//! The tests use `SIGUSR2` and deliberately *do* raise it while it is ignored:
//! that is the only way to prove the disposition took effect. A process-directed
//! signal may be handled by any thread, which is fine here because the action is
//! process-wide `SIG_IGN`; the guard then puts the previous action back, so this
//! test cannot leave the harness ignoring a signal it cares about.

use bsh_sys::{
    Disposition, SignalSet, WaitOutcome, ignore_signals, pid, replace_signal_mask, send_signal,
    set_disposition, wait_pid,
};
use std::sync::Mutex;

/// These tests change a **process-wide** signal disposition and then raise the
/// signal to prove it took effect. Cargo runs the tests in one binary
/// concurrently, so without this lock one test can restore `SIG_DFL` between
/// another's `SIG_IGN` and its `raise`, killing the test process — which is
/// exactly the intermittent failure this file used to produce under load.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    // A panicking test poisons the mutex; the next test still needs to run.
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Run `body` in a forked child and return its exit status.
///
/// Raising a signal to prove a disposition is inherently racy in a
/// multi-threaded test process: a process-directed signal may stay pending and
/// be delivered after the disposition is restored, killing the harness. In a
/// child, a stray delivery kills only the child, and the parent asserts on the
/// status — which is a stronger assertion than "we survived".
fn in_child(body: impl FnOnce() -> i32) -> i32 {
    // SAFETY: the child only runs `body` and `_exit`s; the parent waits for it.
    let child = unsafe { libc::fork() };
    assert!(child >= 0, "fork failed");
    if child == 0 {
        let status = body();
        // SAFETY: `_exit` is the only correct way out of a fork.
        unsafe { libc::_exit(status) }
    }
    match wait_pid(child).expect("wait for the child") {
        WaitOutcome::Exited(status) => status,
        other => panic!("child did not exit normally: {other:?}"),
    }
}

#[test]
fn an_ignored_signal_is_discarded_and_default_restores() {
    let status = in_child(|| {
        set_disposition(libc::SIGUSR2, Disposition::Ignore).expect("ignore SIGUSR2");
        // Surviving this call is the assertion.
        send_signal(pid(), libc::SIGUSR2).expect("send an ignored signal");
        set_disposition(libc::SIGUSR2, Disposition::Default).expect("restore SIGUSR2");
        0
    });
    assert_eq!(status, 0, "an ignored SIGUSR2 must not kill the process");
}

#[test]
fn ignore_signals_restores_the_previous_action_on_drop() {
    let status = in_child(|| {
        // Start from the default action, so the guard has something real to
        // restore, and return to it even if an assertion fails below.
        set_disposition(libc::SIGUSR2, Disposition::Default).expect("start at SIG_DFL");
        {
            let guard = ignore_signals(&[libc::SIGUSR2]).expect("ignore SIGUSR2");
            send_signal(pid(), libc::SIGUSR2).expect("send an ignored signal");
            guard.restore().expect("restore explicitly");
        }
        // A second ignore/drop cycle exercises the `Drop` path.
        {
            let _guard = ignore_signals(&[libc::SIGUSR2]).expect("ignore SIGUSR2 again");
            send_signal(pid(), libc::SIGUSR2).expect("send an ignored signal");
        }
        set_disposition(libc::SIGUSR2, Disposition::Default).expect("leave SIGUSR2 at SIG_DFL");
        0
    });
    assert_eq!(
        status, 0,
        "the guard must keep SIGUSR2 ignored while it lives"
    );
}

#[test]
fn replacing_the_signal_mask_persists() {
    let _serial = serial();
    let before = SignalSet::current().expect("read the current mask");
    let set = SignalSet::of(&[libc::SIGUSR2]);
    replace_signal_mask(&set).expect("replace the mask");
    assert!(
        SignalSet::current()
            .expect("read back")
            .contains(libc::SIGUSR2)
    );
    replace_signal_mask(&before).expect("restore the mask");
    assert_eq!(SignalSet::current().expect("read back"), before);
}

#[test]
fn a_disposition_for_a_signal_the_host_lacks_is_an_error() {
    // `EINVAL` for a signal number outside the range, never a silent success.
    let error = set_disposition(9999, Disposition::Ignore).expect_err("invalid signal");
    assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
}
