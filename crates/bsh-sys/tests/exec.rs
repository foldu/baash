//! In-place `exec`: the process image is replaced, the pid is kept, and
//! failures return to the caller instead of killing it.
//!
//! Each test forks its own child, so a successful exec can never replace the
//! test binary itself.

use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use bsh_sys::{SpawnPlan, WaitOutcome, exec, pid, wait_pid};

/// Run `f` in a child process and return its pid.
///
/// `exec` replaces the process image, so the only way to test a successful one
/// without destroying the test harness is to do it in a child. This is test
/// scaffolding, not part of `bsh-sys`: the shell itself no longer forks.
fn forked(f: impl FnOnce()) -> i32 {
    // SAFETY: the child only runs `f` and `_exit`s; the parent waits for it and
    // the test binary has no other threads at this point.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        f();
        // SAFETY: `_exit` is the only correct way out of a fork with live
        // Rust state in the parent.
        unsafe { libc::_exit(0) }
    }
    assert!(pid > 0);
    pid
}

/// Resolve an executable the portable way: `/bin`, then `/usr/bin`, then
/// `$PATH`, matching the other spawn tests.
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

/// A temp-file path removed on drop.
struct TempPath(PathBuf);

impl TempPath {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!("bsh-sys-exec-{tag}-{}-{serial}", std::process::id());
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
fn exec_replaces_the_process_and_keeps_the_pid() {
    let shell = program("sh");
    let output = TempPath::new("pid");
    let mut spawn_plan = plan(&shell, &["-c", "printf '%s' \"$$\""]);
    spawn_plan
        .open(
            output.as_path().as_os_str().as_bytes(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
            0o600,
            1,
        )
        .unwrap();

    // If exec succeeds it never returns; a failure becomes 127.
    let child = forked(move || {
        let _ = exec(spawn_plan);
    });

    assert_eq!(
        wait_pid(child).unwrap(),
        WaitOutcome::Exited(0),
        "the exec'd command should exit 0"
    );
    // `$$` is the exec'd program's own pid. If `exec` forked, it would not be
    // the pid the parent forked — exactly the old approximation's defect.
    let reported = std::fs::read_to_string(output.as_path()).unwrap();
    assert_eq!(reported, child.to_string(), "exec must not change the pid");
}

#[test]
fn exec_of_true_succeeds() {
    let child = forked(|| {
        let _ = exec(plan(&program("true"), &[]));
    });
    assert_eq!(wait_pid(child).unwrap(), WaitOutcome::Exited(0));
}

#[test]
fn exec_passes_argv_and_environment() {
    let output = TempPath::new("argv");
    let mut spawn_plan = plan(
        &program("sh"),
        &[
            "-c",
            "printf '%s:%s' \"$1\" \"$BSH_EXEC_ENV\"",
            "sh",
            "positional",
        ],
    );
    spawn_plan
        .push_env(b"BSH_EXEC_ENV", b"present")
        .expect("an environment entry without NULs");
    spawn_plan
        .open(
            output.as_path().as_os_str().as_bytes(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
            0o600,
            1,
        )
        .unwrap();

    let child = forked(move || {
        let _ = exec(spawn_plan);
    });
    assert_eq!(wait_pid(child).unwrap(), WaitOutcome::Exited(0));
    assert_eq!(
        std::fs::read(output.as_path()).unwrap(),
        b"positional:present"
    );
}

#[test]
fn exec_failure_returns_enoent_and_leaves_the_caller_alive() {
    // No fd actions, no signal changes: this plan is safe to attempt in the
    // test process, which is the point — a failed exec must return rather than
    // replace or kill the caller.
    let before = pid();
    let missing = PathBuf::from("/nonexistent-bsh/exec-target");
    let error = exec(plan(&missing, &[]));

    assert_eq!(
        error.raw_os_error(),
        Some(libc::ENOENT),
        "expected ENOENT, got {error}"
    );
    assert_eq!(pid(), before, "the calling process must still be alive");
    assert_eq!(
        io::Error::from_raw_os_error(libc::ENOENT).kind(),
        error.kind()
    );
}
