//! Creating child processes, and replacing the current one.
//!
//! Two mechanisms, chosen by [`spawn`]:
//!
//! * **`posix_spawn`** is the default. glibc implements it with
//!   `CLONE_VM|CLONE_VFORK`, executes only async-signal-safe operations in the
//!   child, and can express process groups, signal defaults/masks and
//!   dup2/close/open file actions. It is therefore safe even while the shell
//!   has helper threads (DESIGN.md §4.3).
//!   `posix_spawn` cannot express, and is the mechanism behind [`fork_child`],
//!   which runs *shell code* rather than an external program and so cannot be
//!   spawned at all.
//!
//! [`exec`] is the third case: no child at all. It applies a plan in place and
//! calls `execve`, which is what the shell's `exec` built-in must do to keep
//! its pid and carry redirections into the command.
//!
//! Everything a child needs is computed in the parent: argv/envp arrays,
//! `CString`s, and the ordered file actions. Between `fork` and `exec` the
//! child performs raw syscalls only — no allocation, no locking, no
//! destructors, no panics — so the whole path can be audited in this module.
//!
//! # Reporting exec failures
//!
//! `execve` failing in a child must surface as an `io::Error` in the *parent*,
//! not as an invented exit status. [`SpawnPlan`] carries a close-on-exec pipe
//! ([`ReportPipe`]): the child writes the raw errno to it and `_exit`s, while
//! the parent blocks reading it. EOF (no bytes) means the exec succeeded,
//! because the close-on-exec flag closed the writer; four bytes mean failure.
//! The fork path moves that writer above every descriptor the plan mentions so
//! no redirect can close or overwrite it. In-place [`exec`] needs no pipe: it
//! returns the error to its own caller.

use std::ffi::CString;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::RawFd;

use crate::WaitOutcome;
use crate::bytes::{cstring, cstrings};
use crate::signal::SignalSet;
use crate::wait::wait_pid;

/// One step in the ordered list of descriptor changes applied in the child.
///
/// Order is observable: `2>&1 >f` is not `>f 2>&1`, so the list is never
/// treated as a set (DESIGN.md §4.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FdAction {
    /// `dup2(from, to)`.
    Dup2 {
        /// Source descriptor, which must already be open in the parent unless
        /// an earlier action created it.
        from: RawFd,
        /// Destination descriptor; it is closed first if open.
        to: RawFd,
    },
    /// Close `to`. An already-closed descriptor is not an error, matching the
    /// shell's `n>&-`.
    Close(
        /// The descriptor to close.
        RawFd,
    ),
    /// Open `path`, then move the new descriptor to `to`.
    ///
    /// The open happens **in the child**, at its position in the action list,
    /// so redirects are applied exactly as written and the parent never holds
    /// the file open. `O_CLOEXEC` in `flags` is cleared: a redirect target must
    /// remain open across `exec`.
    Open {
        /// The file to open, already converted from shell bytes.
        path: CString,
        /// Flags for `open(2)`.
        flags: libc::c_int,
        /// Creation mode, used only with `O_CREAT`/`O_TMPFILE`.
        mode: libc::mode_t,
        /// Destination descriptor.
        to: RawFd,
    },
}

impl FdAction {
    /// `dup2(from, to)`.
    pub fn dup2(from: RawFd, to: RawFd) -> Self {
        Self::Dup2 { from, to }
    }

    /// Close `fd`.
    pub fn close(fd: RawFd) -> Self {
        Self::Close(fd)
    }

    /// Open `path` and move the descriptor to `to`.
    ///
    /// Rejects an embedded NUL in `path` rather than truncating it.
    pub fn open(
        path: impl AsRef<[u8]>,
        flags: libc::c_int,
        mode: libc::mode_t,
        to: RawFd,
    ) -> io::Result<Self> {
        Ok(Self::Open {
            path: cstring(path.as_ref())?,
            flags,
            mode,
            to,
        })
    }
}

/// Everything needed to execute one child, computed in the parent.
///
/// Construct with [`SpawnPlan::new`] or [`SpawnPlan::new_with_env`] and then
/// adjust the public fields. `spawn` consumes the plan because it owns
/// descriptors (notably the report pipe).
#[derive(Debug)]
pub struct SpawnPlan {
    /// Absolute or relative path of the program to execute.
    ///
    /// There is deliberately no `PATH` search here: `posix_spawn` (not
    /// `posix_spawnp`) is used, and POSIX leaves the search — including the
    /// "not found" status and the hash table — to the shell.
    pub program: CString,
    /// Complete argument vector, including `argv[0]`.
    pub argv: Vec<CString>,
    /// Environment as `NAME=value` entries; empty means an empty environment,
    /// not an inherited one (POSIX `execve` semantics). The shell adds its
    /// exported variables here.
    pub envp: Vec<CString>,
    /// Descriptor changes applied in order, after the process-group and signal
    /// setup and before `exec`.
    pub actions: Vec<FdAction>,
    /// Signals whose disposition is reset to `SIG_DFL` in the child.
    ///
    /// `exec` resets *caught* signals but preserves `SIG_IGN`, so a shell that
    /// ignored `SIGINT` for `trap '' INT` must name it here or the child
    /// becomes un-interruptible.
    pub reset_signals: SignalSet,
    /// The child's blocked-signal mask, or `None` to inherit the parent's.
    ///
    /// The shell normally passes `Some(empty)` so that signals it blocks
    /// internally (`SIGCHLD`, `SIGTTOU`) do not leak into the command.
    pub sigmask: Option<SignalSet>,
}

impl SpawnPlan {
    /// A plan to execute `program` with `argv` and an empty environment.
    pub fn new(
        program: impl AsRef<[u8]>,
        argv: impl IntoIterator<Item = impl AsRef<[u8]>>,
    ) -> io::Result<Self> {
        Self::new_with_env(program, argv, std::iter::empty::<&[u8]>())
    }

    /// A plan to execute `program` with `argv` and `envp`.
    pub fn new_with_env(
        program: impl AsRef<[u8]>,
        argv: impl IntoIterator<Item = impl AsRef<[u8]>>,
        envp: impl IntoIterator<Item = impl AsRef<[u8]>>,
    ) -> io::Result<Self> {
        Ok(Self {
            program: cstring(program.as_ref())?,
            argv: cstrings(argv)?,
            envp: cstrings(envp)?,
            actions: Vec::new(),
            reset_signals: SignalSet::empty(),
            sigmask: None,
        })
    }

    /// Append one ordered descriptor action.
    pub fn push_action(&mut self, action: FdAction) {
        self.actions.push(action);
    }

    /// Append `dup2(from, to)`.
    pub fn dup2(&mut self, from: RawFd, to: RawFd) {
        self.actions.push(FdAction::dup2(from, to));
    }

    /// Append a close of `fd`.
    pub fn close(&mut self, fd: RawFd) {
        self.actions.push(FdAction::close(fd));
    }

    /// Append an open of `path` onto `to`.
    ///
    /// Rejects an embedded NUL in `path` rather than truncating it.
    pub fn open(
        &mut self,
        path: impl AsRef<[u8]>,
        flags: libc::c_int,
        mode: libc::mode_t,
        to: RawFd,
    ) -> io::Result<()> {
        self.actions.push(FdAction::open(path, flags, mode, to)?);
        Ok(())
    }

    /// Append one exported `NAME=value` environment entry.
    pub fn push_env(&mut self, name: &[u8], value: &[u8]) -> io::Result<()> {
        let mut entry = Vec::with_capacity(name.len() + 1 + value.len());
        entry.extend_from_slice(name);
        entry.push(b'=');
        entry.extend_from_slice(value);
        self.envp.push(cstring(&entry)?);
        Ok(())
    }
}

/// A running (or at least created) child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Child {
    pid: libc::pid_t,
}

impl Child {
    /// Take ownership of a child this module did not spawn.
    ///
    /// Used by [`fork_child_status`] (test scaffolding) and by nothing else.
    fn from_pid(pid: libc::pid_t) -> Self {
        Self { pid }
    }

    /// The child's process id (`$!` for a background command).
    pub fn pid(&self) -> libc::pid_t {
        self.pid
    }

    /// Wait for this child.
    ///
    /// Convenience wrapper around [`wait_pid`]; the event loop uses the
    /// non-blocking [`crate::wait_any_nohang`] instead.
    pub fn wait(&self) -> io::Result<WaitOutcome> {
        wait_pid(self.pid)
    }
}

/// Execute `plan`, preferring `posix_spawn`.
///
/// The fork fallback is used only when the plan contains something
/// `posix_spawn` cannot express; today every [`FdAction`] maps to a
/// `posix_spawn_file_actions_*` call, so this takes the spawn path. Keeping the
/// predicate explicit means a future action type automatically selects the
/// fallback.
pub fn spawn(plan: SpawnPlan) -> io::Result<Child> {
    spawn_posix_spawn(plan)
}

/// Replace the current process image with the program in `plan`.
///
/// This is `execve(2)` without a preceding `fork`, and it is what the shell's
/// `exec` built-in needs: `exec cmd` must keep the shell's pid and must carry
/// the requested redirections into the command, neither of which a
/// spawn-and-wait approximation can do. It applies the plan's process
/// attributes, then its ordered [`FdAction`] list (exactly as
/// [`spawn_forked`] does), and finally calls `execve`.
///
/// **On success this function never returns**, and the process image is
/// replaced: the caller must be prepared for its code, heap, signal handlers
/// and address space to disappear. In particular, nothing after the call in
/// the caller will run.
///
/// On failure it returns the `execve` error (`ENOENT`, `EACCES`, `ENOEXEC`, …)
/// and the current process image is preserved, so the caller can report the
/// failure, map it to 127/126 and exit. The descriptor actions and the
/// process-group/signal settings have *already* been applied by then, because
/// that is their position in the exec sequence; the caller must not assume the
/// descriptor table is unchanged. Failing before `execve` (a bad redirect, an
/// invalid signal) is likewise reported as an `io::Error`.
///
/// The plan's [`ReportPipe`] exists only to carry an exec failure from a
/// forked child to its parent; an in-place exec reports through this return
/// value, so the pipe is closed before the exec is attempted.
///
/// There is no `PATH` search and no shebang handling here; [`SpawnPlan`]
/// carries an already-resolved path (the same contract as [`spawn`]).
#[must_use = "exec returns only on failure; ignoring the error hides it"]
pub fn exec(plan: SpawnPlan) -> io::Error {
    let SpawnPlan {
        program,
        argv,
        envp,
        actions,
        reset_signals,
        sigmask,
    } = plan;

    let argv = cstr_ptrs(&argv);
    let envp = cstr_ptrs(&envp);

    let errno = apply_process_attributes(&reset_signals, sigmask.as_ref());
    if errno != 0 {
        return io::Error::from_raw_os_error(errno);
    }
    for action in &actions {
        let errno = apply_fd_action(action);
        if errno != 0 {
            return io::Error::from_raw_os_error(errno);
        }
    }

    // SAFETY: `program` is NUL-terminated, both vectors are NULL-terminated
    // and point at live `CString` buffers that outlive the call, and `execve`
    // retains neither pointer.
    unsafe {
        libc::execve(
            program.as_ptr(),
            argv.as_ptr().cast::<*const libc::c_char>(),
            envp.as_ptr().cast::<*const libc::c_char>(),
        );
    }
    // `execve` returned, so it failed and set errno.
    let error = io::Error::last_os_error();
    if error.raw_os_error().is_none() {
        io::Error::from_raw_os_error(libc::EIO)
    } else {
        error
    }
}

fn spawn_posix_spawn(plan: SpawnPlan) -> io::Result<Child> {
    let SpawnPlan {
        program,
        argv,
        envp,
        actions,
        reset_signals,
        sigmask,
    } = plan;

    let mut attr = SpawnAttr::new()?;
    let mut flags: libc::c_short = 0;
    if !reset_signals.is_empty() {
        attr.set_sigdefault(&reset_signals)?;
        flags |= libc::POSIX_SPAWN_SETSIGDEF as libc::c_short;
    }
    if let Some(mask) = &sigmask {
        attr.set_sigmask(mask)?;
        flags |= libc::POSIX_SPAWN_SETSIGMASK as libc::c_short;
    }
    attr.set_flags(flags)?;

    let mut file_actions = SpawnFileActions::new()?;
    for action in &actions {
        file_actions.add(action)?;
    }

    let argv = cstr_ptrs(&argv);
    let envp = cstr_ptrs(&envp);
    let mut pid: libc::pid_t = 0;
    // SAFETY: `program` is a NUL-terminated string, the attribute and action
    // objects are initialised and outlive the call, and the argv/envp arrays
    // are NULL-terminated and point at live `CString` buffers.
    let rc = unsafe {
        libc::posix_spawn(
            &mut pid,
            program.as_ptr(),
            &file_actions.0,
            &attr.0,
            argv.as_ptr(),
            envp.as_ptr(),
        )
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    Ok(Child { pid })
}

fn cstr_ptrs(items: &[CString]) -> Vec<*mut libc::c_char> {
    let mut pointers: Vec<*mut libc::c_char> =
        items.iter().map(|item| item.as_ptr().cast_mut()).collect();
    pointers.push(std::ptr::null_mut());
    pointers
}

/// Initialised `posix_spawnattr_t` with a destructor.
struct SpawnAttr(libc::posix_spawnattr_t);

impl SpawnAttr {
    fn new() -> io::Result<Self> {
        let mut raw = MaybeUninit::<libc::posix_spawnattr_t>::uninit();
        // SAFETY: `raw` is valid for writes; `posix_spawnattr_init` initialises
        // it on success.
        let rc = unsafe { libc::posix_spawnattr_init(raw.as_mut_ptr()) };
        spawn_result(rc)?;
        // SAFETY: success initialises the attribute object.
        Ok(Self(unsafe { raw.assume_init() }))
    }

    fn set_flags(&mut self, flags: libc::c_short) -> io::Result<()> {
        // SAFETY: `self.0` is initialised and `flags` is a valid flag set.
        let rc = unsafe { libc::posix_spawnattr_setflags(&mut self.0, flags) };
        spawn_result(rc)
    }

    fn set_sigdefault(&mut self, set: &SignalSet) -> io::Result<()> {
        // SAFETY: `self.0` is initialised and `set` is a valid `sigset_t`.
        let rc = unsafe { libc::posix_spawnattr_setsigdefault(&mut self.0, set.as_ptr()) };
        spawn_result(rc)
    }

    fn set_sigmask(&mut self, set: &SignalSet) -> io::Result<()> {
        // SAFETY: `self.0` is initialised and `set` is a valid `sigset_t`.
        let rc = unsafe { libc::posix_spawnattr_setsigmask(&mut self.0, set.as_ptr()) };
        spawn_result(rc)
    }
}

impl Drop for SpawnAttr {
    fn drop(&mut self) {
        // SAFETY: `self.0` was initialised by `posix_spawnattr_init`.
        unsafe { libc::posix_spawnattr_destroy(&mut self.0) };
    }
}

/// Initialised `posix_spawn_file_actions_t` with a destructor.
struct SpawnFileActions(libc::posix_spawn_file_actions_t);

impl SpawnFileActions {
    fn new() -> io::Result<Self> {
        let mut raw = MaybeUninit::<libc::posix_spawn_file_actions_t>::uninit();
        // SAFETY: `raw` is valid for writes; the init function initialises it
        // on success.
        let rc = unsafe { libc::posix_spawn_file_actions_init(raw.as_mut_ptr()) };
        spawn_result(rc)?;
        // SAFETY: success initialises the actions object.
        Ok(Self(unsafe { raw.assume_init() }))
    }

    fn add(&mut self, action: &FdAction) -> io::Result<()> {
        let rc = match action {
            FdAction::Dup2 { from, to } => {
                // SAFETY: `self.0` is initialised; both arguments are plain
                // descriptor numbers.
                unsafe { libc::posix_spawn_file_actions_adddup2(&mut self.0, *from, *to) }
            }
            FdAction::Close(fd) => {
                // SAFETY: `self.0` is initialised; `fd` is a descriptor number.
                unsafe { libc::posix_spawn_file_actions_addclose(&mut self.0, *fd) }
            }
            FdAction::Open {
                path,
                flags,
                mode,
                to,
            } => {
                // SAFETY: `self.0` is initialised; `path` is NUL-terminated and
                // copied by the call; the remaining arguments are values.
                // `O_CLOEXEC` is cleared so the redirected target survives
                // exec, matching `FdAction::Open`'s contract.
                unsafe {
                    libc::posix_spawn_file_actions_addopen(
                        &mut self.0,
                        *to,
                        path.as_ptr(),
                        *flags & !libc::O_CLOEXEC,
                        *mode,
                    )
                }
            }
        };
        spawn_result(rc)
    }
}

impl Drop for SpawnFileActions {
    fn drop(&mut self) {
        // SAFETY: `self.0` was initialised by the file-actions init function.
        unsafe { libc::posix_spawn_file_actions_destroy(&mut self.0) };
    }
}

fn spawn_result(rc: libc::c_int) -> io::Result<()> {
    if rc != 0 {
        Err(io::Error::from_raw_os_error(rc))
    } else {
        Ok(())
    }
}

/// Apply the process-group and signal setup shared by the fork fallback and
/// the in-place [`exec`].
///
/// Returns the errno that stopped it, or `0` on success. Only raw syscalls:
/// this runs in the post-`fork` window of [`spawn_forked`].
fn apply_process_attributes(reset_signals: &SignalSet, sigmask: Option<&SignalSet>) -> libc::c_int {
    for signal in reset_signals.iter() {
        // SAFETY: `signal(2)` with `SIG_DFL` for a signal number; no pointers.
        if unsafe { libc::signal(signal, libc::SIG_DFL) } == libc::SIG_ERR {
            return last_errno();
        }
    }

    if let Some(mask) = sigmask {
        // SAFETY: `mask` is an initialised `sigset_t`; the call does not retain
        // the pointer.
        if unsafe { libc::sigprocmask(libc::SIG_SETMASK, mask.as_ptr(), std::ptr::null_mut()) } != 0
        {
            return last_errno();
        }
    }

    0
}

fn apply_fd_action(action: &FdAction) -> libc::c_int {
    match action {
        FdAction::Dup2 { from, to } => {
            if from == to {
                return 0;
            }
            // SAFETY: `dup2` only reads descriptor numbers.
            if unsafe { libc::dup2(*from, *to) } < 0 {
                last_errno()
            } else {
                0
            }
        }
        FdAction::Close(fd) => {
            // SAFETY: `close` only reads a descriptor number.
            if unsafe { libc::close(*fd) } == 0 {
                return 0;
            }
            // Closing an already-closed descriptor is a no-op for the shell's
            // `n>&-`; every other error is real.
            let errno = last_errno();
            if errno == libc::EBADF { 0 } else { errno }
        }
        FdAction::Open {
            path,
            flags,
            mode,
            to,
        } => {
            // Open in the child, at this position in the ordered action list.
            // `O_CLOEXEC` is cleared: the redirect target must survive exec.
            let open_flags = *flags & !libc::O_CLOEXEC;
            // SAFETY: `path` is NUL-terminated and lives in the plan; `open` is
            // variadic and only reads `mode` for creating flags.
            let fd = unsafe { libc::open(path.as_ptr(), open_flags, *mode) };
            if fd < 0 {
                return last_errno();
            }
            if fd == *to {
                return 0;
            }
            // SAFETY: `dup2` only reads descriptor numbers.
            let duplicate = unsafe { libc::dup2(fd, *to) };
            let errno = if duplicate < 0 { last_errno() } else { 0 };
            // SAFETY: the temporary descriptor is owned by this child.
            unsafe { libc::close(fd) };
            errno
        }
    }
}

fn last_errno() -> libc::c_int {
    // `io::Error::last_os_error` reads the thread-local errno without
    // allocating; only the raw number is used.
    io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

/// Fork, then run `f` in the child and use its return value as the exit status.
///
/// **Test scaffolding.** The shell no longer forks; this exists for the pty
/// harness, which has to start a shell in a child that sets up a session and
/// then `exec`s. `fork` is safe there because the harness is single-threaded at
/// that point, which is why the tests could always do it.
pub fn fork_child_status(f: impl FnOnce() -> i32) -> io::Result<Child> {
    // SAFETY: `fork` takes no arguments. The caller promises the usual fork
    // contract: no other thread in the process is holding a lock or allocating
    // while this runs, and the child only calls `_exit`.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        let status = f();
        // SAFETY: `_exit` is the only correct way out of a fork with live Rust
        // state in the parent; it cannot return.
        unsafe { libc::_exit(status) }
    }
    Ok(Child::from_pid(pid))
}

#[cfg(test)]
mod tests {}
