//! Dispositions the shell sets on itself: `trap '' SIG` and the
//! `SIGINT`/`SIGQUIT` ignore around a foreground wait.
//!
//! The tests use `SIGUSR2` and deliberately *do* raise it while it is ignored:
//! that is the only way to prove the disposition took effect. A process-directed
//! signal may be handled by any thread, which is fine here because the action is
//! process-wide `SIG_IGN`; the guard then puts the previous action back, so this
//! test cannot leave the harness ignoring a signal it cares about.

use bsh_sys::{
    Disposition, SignalSet, ignore_signals, pid, replace_signal_mask, send_signal, set_disposition,
};

#[test]
fn an_ignored_signal_is_discarded_and_default_restores() {
    set_disposition(libc::SIGUSR2, Disposition::Ignore).expect("ignore SIGUSR2");
    // Surviving this call is the assertion.
    send_signal(pid(), libc::SIGUSR2).expect("send an ignored signal");
    set_disposition(libc::SIGUSR2, Disposition::Default).expect("restore SIGUSR2");
}

#[test]
fn ignore_signals_restores_the_previous_action_on_drop() {
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
}

#[test]
fn replacing_the_signal_mask_persists() {
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
