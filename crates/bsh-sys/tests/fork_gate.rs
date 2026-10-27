//! The fork gate is wired into both fork paths.
//!
//! Run as its own test binary: the gate is process-global, and this test holds
//! a guard, so nothing else in the process may be spawning while it runs.

use bsh_sys::{SpawnPlan, fork_child, fork_gate, spawn_forked};

#[test]
fn fork_paths_refuse_while_a_helper_thread_is_live() {
    let guard = fork_gate().enter();

    let forked = std::panic::catch_unwind(|| fork_child(|| unreachable!()));
    assert!(
        forked.is_err(),
        "fork_child must panic while a helper thread is live"
    );

    let spawned = std::panic::catch_unwind(|| {
        let plan = SpawnPlan::new(b"/bin/true", [b"true".as_slice()]).unwrap();
        spawn_forked(plan)
    });
    assert!(
        spawned.is_err(),
        "spawn_forked must panic while a helper thread is live"
    );

    drop(guard);
    assert!(fork_gate().is_forkable());
}
