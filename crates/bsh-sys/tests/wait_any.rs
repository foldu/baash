//! `wait_any_nohang` is the event loop's reaper.
//!
//! One test in its own binary: it asserts the "no children" answer, which
//! parallel tests in the same process could invalidate by spawning.

use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use bsh_sys::{SpawnPlan, WaitOutcome, spawn, wait_any_nohang, wait_pid};

fn sh() -> PathBuf {
    ["/bin/sh", "/usr/bin/sh"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .expect("a POSIX shell is required")
}

#[test]
fn wait_any_nohang_reaps_children_and_reports_none_when_idle() {
    assert_eq!(
        wait_any_nohang(),
        None,
        "a process with no children has nothing to reap"
    );

    let shell = sh();
    let path = shell.as_os_str().as_bytes();
    let plan = SpawnPlan::new(path, vec![path, b"-c".as_slice(), b"exit 7".as_slice()]).unwrap();
    let child = spawn(plan).unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let reaped = loop {
        if let Some(reaped) = wait_any_nohang() {
            break reaped;
        }
        assert!(Instant::now() < deadline, "child was never reaped");
        std::thread::sleep(Duration::from_millis(1));
    };

    assert_eq!(reaped, (child.pid(), WaitOutcome::Exited(7)));
    assert_eq!(wait_any_nohang(), None);

    // The child was reaped above, so this must fail rather than block.
    assert!(wait_pid(child.pid()).is_err());
}
