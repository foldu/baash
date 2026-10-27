//! Creating child processes.
//!
//! Two mechanisms, chosen by [`spawn`]:
//!
//! * **`posix_spawn`** is the default. glibc implements it with
//!   `CLONE_VM|CLONE_VFORK`, executes only async-signal-safe operations in the
//!   child, and can express process groups, signal defaults/masks and
//!   dup2/close/open file actions. It is therefore safe even while the shell
//!   has helper threads (DESIGN.md §4.3).
//! * **`fork` + `execve`** is the fallback ([`spawn_forked`]) for plans that
//!   `posix_spawn` cannot express, and is the mechanism behind [`fork_child`],
//!   which runs *shell code* rather than an external program and so cannot be
//!   spawned at all.
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
//! no redirect can close or overwrite it.

use std::convert::Infallible;
use std::ffi::CString;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::bytes::{cstring, cstrings};
use crate::fd::Pipe;
use crate::signal::SignalSet;
use crate::threadgate::fork_gate;
use crate::wait::{WaitOutcome, wait_pid};

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

    /// Every descriptor number this action names.
    fn descriptors(&self) -> [RawFd; 2] {
        match self {
            Self::Dup2 { from, to } => [*from, *to],
            Self::Close(fd) | Self::Open { to: fd, .. } => [*fd, *fd],
        }
    }
}

/// What process group the child should join.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProcessGroup {
    /// Leave the child in the parent's process group.
    #[default]
    Inherit,
    /// `setpgid(0, pgroup)` in the child. `pgroup == 0` creates a new group
    /// led by the child, which is what job control wants for a pipeline.
    SetPgid {
        /// The process group to join, or 0 for a new group.
        pgroup: libc::pid_t,
    },
}

/// A close-on-exec pipe that carries an exec failure from child to parent.
///
/// The child writes the raw errno (four bytes, native endianness) to
/// [`ReportPipe::write`] and exits; the parent reads [`ReportPipe::read`].
/// Both ends are `O_CLOEXEC`, so a successful `exec` closes the writer and the
/// parent's read returns EOF. Only the fork fallback needs this channel:
/// `posix_spawn` returns the child's exec error synchronously.
#[derive(Debug)]
pub struct ReportPipe {
    /// Parent's read end.
    pub read: OwnedFd,
    /// Child's write end.
    pub write: OwnedFd,
}

impl ReportPipe {
    /// Create a fresh close-on-exec pipe.
    pub fn new() -> io::Result<Self> {
        let Pipe { read, write } = Pipe::new()?;
        Ok(Self { read, write })
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
    /// Process-group placement.
    pub pgroup: ProcessGroup,
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
    /// Exec-failure report channel; see [`ReportPipe`].
    ///
    /// `posix_spawn` reports exec failures through its return value and closes
    /// this unused; the fork fallback uses it.
    pub report: ReportPipe,
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
            pgroup: ProcessGroup::Inherit,
            reset_signals: SignalSet::empty(),
            sigmask: None,
            report: ReportPipe::new()?,
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

    /// The largest descriptor number any action mentions.
    fn max_action_fd(&self) -> Option<RawFd> {
        self.actions.iter().flat_map(FdAction::descriptors).max()
    }

    /// Move the report writer above every descriptor an action mentions.
    ///
    /// A redirect such as `3>file` would otherwise dup2 over the report pipe's
    /// writer, and a failed exec would then be reported as a bogus success.
    /// `open(2)` in the child cannot pick the raised number either, because the
    /// descriptor is already open. The duplicate is done in the parent so a
    /// failure can be returned cleanly.
    fn raise_report_fd(&mut self) -> io::Result<()> {
        let floor = self
            .max_action_fd()
            .map_or(10, |fd| fd.saturating_add(1).max(10));
        let current = self.report.write.as_raw_fd();
        if current >= floor {
            return Ok(());
        }
        // SAFETY: `F_DUPFD_CLOEXEC` duplicates a live descriptor; `floor` is a
        // valid non-negative minimum and the call touches no pointers.
        let duplicate = unsafe { libc::fcntl(current, libc::F_DUPFD_CLOEXEC, floor) };
        if duplicate < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `duplicate` is a fresh descriptor; taking ownership closes
        // the previous writer when it is replaced.
        self.report.write = unsafe { OwnedFd::from_raw_fd(duplicate) };
        Ok(())
    }
}

/// A running (or at least created) child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Child {
    pid: libc::pid_t,
}

impl Child {
    /// The child's process id (`$!` for background jobs).
    pub fn pid(&self) -> libc::pid_t {
        self.pid
    }

    /// Wait for this child.
    ///
    /// Convenience wrapper around [`wait_pid`]; the event loop uses the
    /// non-blocking [`crate::wait_any_nohang`] instead.
    pub fn wait(&self, wuntraced: bool) -> io::Result<WaitOutcome> {
        wait_pid(self.pid, wuntraced)
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
    if posix_spawn_can_express(&plan) {
        spawn_posix_spawn(plan)
    } else {
        spawn_forked(plan)
    }
}

fn posix_spawn_can_express(plan: &SpawnPlan) -> bool {
    plan.actions.iter().all(|action| {
        matches!(
            action,
            FdAction::Dup2 { .. } | FdAction::Close(_) | FdAction::Open { .. }
        )
    })
}

/// Execute `plan` with `fork(2)` + `execve(2)`.
///
/// This is the fallback for plans `posix_spawn` cannot express. It is public
/// because it is also the path exercised when testing exec-failure reporting,
/// and because a caller on a platform without a needed spawn attribute may
/// need it directly. It consults [`fork_gate`] and panics while helper threads
/// are live.
///
/// # Panics
///
/// Panics if [`fork_gate`] has live guards (DESIGN.md §4.3).
pub fn spawn_forked(mut plan: SpawnPlan) -> io::Result<Child> {
    fork_gate().assert_forkable();
    plan.raise_report_fd()?;

    let argv = cstr_ptrs(&plan.argv);
    let envp = cstr_ptrs(&plan.envp);
    let report_write = plan.report.write.as_raw_fd();

    // SAFETY: `fork` takes no arguments. The gate above guarantees there are
    // no helper threads whose locks the child could inherit held.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        // Child. From here until `execve` succeeds or fails: raw syscalls
        // only. `child_exec` returns solely on failure, with an errno.
        let errno = child_exec(&plan, &argv, &envp);
        // SAFETY: `report_write` is the report pipe's writer, still open in
        // this child; a four-byte pipe write is atomic. `errno` is a valid
        // `c_int`.
        unsafe {
            libc::write(
                report_write,
                (&raw const errno).cast(),
                std::mem::size_of::<libc::c_int>(),
            );
        }
        // SAFETY: `_exit` terminates immediately, running no destructors,
        // allocators or atexit handlers — exactly what a post-fork child needs.
        unsafe { libc::_exit(127) }
    }

    // Parent: close the writer so EOF on the read end means exec succeeded.
    drop(plan.report.write);
    let reported = match read_exec_report(&plan.report.read) {
        Ok(reported) => reported,
        Err(error) => {
            // The child cannot be reaped deterministically here; do not block
            // on it, just make sure a finished child does not stay a zombie.
            try_reap(pid);
            return Err(error);
        }
    };
    match reported {
        None => Ok(Child { pid }),
        Some(errno) => {
            // The child reported a failed exec and is exiting with status 127.
            reap(pid);
            Err(io::Error::from_raw_os_error(errno))
        }
    }
}

/// Fork, then run `f` in the child, which is expected to finish with `_exit`.
///
/// This is the only way to run *shell code* in a child: subshells, pipeline
/// segments that are functions or compound commands, `$( … )` around shell
/// code, and `&` all need the interpreter in the child, so they cannot be
/// spawned (DESIGN.md §4.3).
///
/// The gate is what makes `f` allowed to do real work: because no helper
/// threads existed at the instant of `fork`, the child inherits a
/// single-threaded, consistent heap and may allocate, lock and unwind like any
/// single-threaded process. That is exactly the case the design reserves `fork`
/// for; external commands use [`spawn`] or [`spawn_forked`], whose own
/// post-fork window stays raw-syscall-only.
///
/// `f` never returns `Ok`: its success type is uninhabited. Returning `Err` is
/// the "this should not happen" path; the child makes no attempt to print a
/// diagnostic (the caller owns diagnostics and should exit inside `f`) and
/// exits with status 127. The returned [`Child`] must be reaped by the caller.
///
/// # Panics
///
/// Panics if [`fork_gate`] has live guards (DESIGN.md §4.3).
pub fn fork_child(f: impl FnOnce() -> io::Result<Infallible>) -> io::Result<Child> {
    fork_gate().assert_forkable();

    // SAFETY: `fork` takes no arguments; the gate above guarantees no helper
    // threads are live.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        let status = match f() {
            // The closure's contract says it never returns successfully.
            Ok(never) => match never {},
            Err(_) => 127,
        };
        // SAFETY: `_exit` runs no destructors; the child has made no
        // allocations of its own by contract, and diagnostics belong to the
        // closure.
        unsafe { libc::_exit(status) }
    }
    Ok(Child { pid })
}

fn spawn_posix_spawn(plan: SpawnPlan) -> io::Result<Child> {
    let SpawnPlan {
        program,
        argv,
        envp,
        actions,
        pgroup,
        reset_signals,
        sigmask,
        report,
    } = plan;
    // `posix_spawn` returns the child's exec error synchronously, so the
    // report pipe is only needed by the fork fallback.
    drop(report);

    let mut attr = SpawnAttr::new()?;
    let mut flags: libc::c_short = 0;
    if let ProcessGroup::SetPgid { pgroup } = pgroup {
        attr.set_pgroup(pgroup)?;
        flags |= libc::POSIX_SPAWN_SETPGROUP as libc::c_short;
    }
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

    fn set_pgroup(&mut self, pgroup: libc::pid_t) -> io::Result<()> {
        // SAFETY: `self.0` is initialised.
        let rc = unsafe { libc::posix_spawnattr_setpgroup(&mut self.0, pgroup) };
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

/// Apply the process attributes, descriptor actions and `execve`.
///
/// Returns the errno that stopped it. It never returns on success.
fn child_exec(
    plan: &SpawnPlan,
    argv: &[*mut libc::c_char],
    envp: &[*mut libc::c_char],
) -> libc::c_int {
    if let ProcessGroup::SetPgid { pgroup } = plan.pgroup {
        // SAFETY: `setpgid` takes only integers.
        if unsafe { libc::setpgid(0, pgroup) } != 0 {
            return last_errno();
        }
    }

    for signal in plan.reset_signals.iter() {
        // SAFETY: `signal(2)` with `SIG_DFL` for a signal number; no pointers.
        if unsafe { libc::signal(signal, libc::SIG_DFL) } == libc::SIG_ERR {
            return last_errno();
        }
    }

    if let Some(mask) = &plan.sigmask {
        // SAFETY: `mask` is an initialised `sigset_t`; the call does not retain
        // the pointer.
        if unsafe { libc::sigprocmask(libc::SIG_SETMASK, mask.as_ptr(), std::ptr::null_mut()) } != 0
        {
            return last_errno();
        }
    }

    for action in &plan.actions {
        let errno = apply_fd_action(action);
        if errno != 0 {
            return errno;
        }
    }

    // SAFETY: `program` is NUL-terminated, the argument vectors are
    // NULL-terminated and point at live buffers inherited from the parent, and
    // `execve` does not retain them.
    unsafe {
        libc::execve(
            plan.program.as_ptr(),
            argv.as_ptr().cast::<*const libc::c_char>(),
            envp.as_ptr().cast::<*const libc::c_char>(),
        );
    }
    last_errno()
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

fn read_exec_report(read: &OwnedFd) -> io::Result<Option<libc::c_int>> {
    let mut errno: libc::c_int = 0;
    loop {
        // SAFETY: the destination points at a `c_int` and the length is its
        // size; `read` does not retain the pointer.
        let count = unsafe {
            libc::read(
                read.as_raw_fd(),
                (&raw mut errno).cast(),
                std::mem::size_of::<libc::c_int>(),
            )
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if count == 0 {
            // EOF: the child exec'd and the close-on-exec flag closed the
            // writer.
            return Ok(None);
        }
        if (count as usize) >= std::mem::size_of::<libc::c_int>() {
            return Ok(Some(errno));
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "short exec-failure report",
        ));
    }
}

/// Block until `pid` is reaped, retrying interruptions.
fn reap(pid: libc::pid_t) {
    let mut status: libc::c_int = 0;
    loop {
        // SAFETY: `status` is valid for writes; `waitpid` does not retain it.
        let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
        if rc >= 0 {
            return;
        }
        if last_errno() != libc::EINTR {
            return;
        }
    }
}

/// Reap `pid` only if it has already exited.
fn try_reap(pid: libc::pid_t) {
    let mut status: libc::c_int = 0;
    // SAFETY: `status` is valid for writes; `waitpid` does not retain it.
    unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_fd_is_raised_above_every_action() {
        let mut plan = SpawnPlan::new(b"/bin/true", [b"true".as_slice()]).unwrap();
        plan.actions.push(FdAction::dup2(1, 3));
        plan.actions.push(FdAction::close(7));
        plan.raise_report_fd().unwrap();
        assert!(plan.report.write.as_raw_fd() > 7);
    }

    #[test]
    fn raised_report_fd_is_cloexec() {
        let mut plan = SpawnPlan::new(b"/bin/true", [b"true".as_slice()]).unwrap();
        plan.raise_report_fd().unwrap();
        assert!(crate::fd::fd_is_cloexec(plan.report.write.as_raw_fd()));
    }
}
