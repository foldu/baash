//! Spawning real processes: statuses, redirects, exec failures and the fork
//! fallback. Each test reaps every child it creates.

use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use baash_sys::{NulByte, SpawnPlan, WaitOutcome, spawn, wait_pid};

/// Resolve an executable the portable way: `/bin`, then `/usr/bin`, then
/// `$PATH`. The task's tests name `/bin/true`, but not every host has it (NixOS
/// keeps coreutils only on `PATH`).
fn program(name: &str) -> PathBuf {
    let mut candidates = vec![
        PathBuf::from(format!("/bin/{name}")),
        PathBuf::from(format!("/usr/bin/{name}")),
    ];
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|dir| dir.join(name)));
    }
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| panic!("no `{name}` executable found"))
}

fn plan(program: &Path, args: &[&str]) -> SpawnPlan {
    let path = program.as_os_str().as_bytes();
    let mut argv: Vec<Vec<u8>> = vec![path.to_vec()];
    argv.extend(args.iter().map(|arg| arg.as_bytes().to_vec()));
    SpawnPlan::new(path, &argv).expect("a plan without NULs")
}

/// A temp-file path removed on drop, so a failed assertion leaves nothing
/// behind.
struct TempPath(PathBuf);

impl TempPath {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!("baash-sys-{tag}-{}-{serial}", std::process::id());
        Self(std::env::temp_dir().join(name))
    }

    fn as_path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn spawn_reports_exit_statuses() {
    let child = spawn(plan(&program("true"), &[])).unwrap();
    assert_eq!(wait_pid(child.pid()).unwrap(), WaitOutcome::Exited(0));

    let child = spawn(plan(&program("false"), &[])).unwrap();
    assert_eq!(wait_pid(child.pid()).unwrap(), WaitOutcome::Exited(1));

    let child = spawn(plan(&program("sh"), &["-c", "exit 42"])).unwrap();
    assert_eq!(wait_pid(child.pid()).unwrap(), WaitOutcome::Exited(42));
}

#[test]
fn a_missing_program_is_an_error_in_the_parent_on_both_paths() {
    let missing = PathBuf::from("/nonexistent-baash/definitely-not-here");
    let error = spawn(plan(&missing, &[])).expect_err("the exec failure must reach the parent");
    assert_eq!(
        error.kind(),
        io::ErrorKind::NotFound,
        "unexpected error: {error}"
    );
}

#[test]
fn embedded_nul_in_argv_is_rejected_not_truncated() {
    let error = SpawnPlan::new(b"/bin/true", [b"true".as_slice(), b"bad\0arg".as_slice()])
        .expect_err("a NUL cannot be represented to execve");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(
        error.get_ref().is_some_and(|inner| inner.is::<NulByte>()),
        "the concrete NulByte error should be preserved"
    );
}

#[test]
fn stdout_redirect_to_a_file_matches_the_written_bytes() {
    let output = TempPath::new("redirect");
    let mut spawn_plan = plan(&program("sh"), &["-c", "printf 'hello\\n'"]);
    spawn_plan
        .open(
            output.as_path().as_os_str().as_bytes(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
            0o600,
            1,
        )
        .unwrap();

    let child = spawn(spawn_plan).unwrap();
    assert_eq!(wait_pid(child.pid()).unwrap(), WaitOutcome::Exited(0));
    assert_eq!(std::fs::read(output.as_path()).unwrap(), b"hello\n");
}

#[test]
fn a_failing_redirect_open_is_reported_before_any_exit_status() {
    {
        let mut spawn_plan = plan(&program("true"), &[]);
        spawn_plan
            .open(
                b"/nonexistent-baash-dir/out",
                libc::O_WRONLY | libc::O_CREAT,
                0o600,
                1,
            )
            .unwrap();
        let error = spawn(spawn_plan).expect_err("the redirect must fail");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}

#[test]
fn environment_entries_reach_the_child() {
    let output = TempPath::new("env");
    let mut spawn_plan = plan(&program("sh"), &["-c", "printf '%s' \"$BAASH_TEST_ENV\""]);
    spawn_plan.push_env(b"BAASH_TEST_ENV", b"present").unwrap();
    spawn_plan
        .open(
            output.as_path().as_os_str().as_bytes(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
            0o600,
            1,
        )
        .unwrap();

    let child = spawn(spawn_plan).unwrap();
    assert_eq!(wait_pid(child.pid()).unwrap(), WaitOutcome::Exited(0));
    assert_eq!(std::fs::read(output.as_path()).unwrap(), b"present");
}

#[test]
fn a_signalled_child_is_reported_as_signalled() {
    let child = spawn(plan(&program("sh"), &["-c", "kill -TERM $$"])).unwrap();
    assert_eq!(
        wait_pid(child.pid()).unwrap(),
        WaitOutcome::Signaled(libc::SIGTERM)
    );
}

#[test]
fn exec_failure_survives_actions_that_mention_low_fds() {
    let missing = PathBuf::from("/nonexistent-baash/low-fd-collision");
    {
        let mut spawn_plan = plan(&missing, &[]);
        spawn_plan.close(3);
        spawn_plan.dup2(1, 4);
        let error = spawn(spawn_plan).expect_err("the exec failure must survive the actions");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
