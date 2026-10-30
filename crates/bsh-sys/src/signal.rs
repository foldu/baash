//! Signals: masks, dispositions, and a pollable signal source.
//!
//! POSIX runs a trap *after* the foreground command completes, never inside
//! it, so no handler may touch interpreter state (DESIGN.md §4.5). Instead the
//! shell blocks the signals it cares about and reads them from a file
//! descriptor it can poll: [`SignalFd`] on Linux (`signalfd`), which is the
//! only implementation today.
//!
//! `SIGCHLD` is a **wakeup hint only**. Delivery is not reliable per child —
//! the kernel may coalesce several exits into one notification — so the event
//! loop treats a readable [`SignalFd`] as "reap now" and drains
//! [`wait_any_nohang`](crate::wait_any_nohang) until it returns `None`.

use std::fmt;
use std::io;
use std::mem::MaybeUninit;

/// A set of signals, wrapping `sigset_t`.
///
/// Signals are numbered from 1; the iterator covers the range Linux uses
/// (`1..=64`). Signals that do not exist on the host are simply never present.
#[derive(Clone, Copy)]
pub struct SignalSet {
    set: libc::sigset_t,
}

impl SignalSet {
    /// The empty set.
    pub fn empty() -> Self {
        // SAFETY: a zeroed `sigset_t` is a valid representation and
        // `sigemptyset` initialises it for the host libc.
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        // SAFETY: `set` is valid for writes.
        unsafe { libc::sigemptyset(&mut set) };
        Self { set }
    }

    /// A set containing the given signals. Out-of-range entries are ignored.
    pub fn of(signals: &[libc::c_int]) -> Self {
        let mut set = Self::empty();
        for &signal in signals {
            set.insert(signal);
        }
        set
    }

    /// Add `signal`; returns whether it was a valid signal number.
    pub fn insert(&mut self, signal: libc::c_int) -> bool {
        // SAFETY: `self.set` is an initialised `sigset_t`.
        unsafe { libc::sigaddset(&mut self.set, signal) == 0 }
    }

    /// Remove `signal`; returns whether it was present.
    pub fn remove(&mut self, signal: libc::c_int) -> bool {
        // SAFETY: `self.set` is an initialised `sigset_t`.
        unsafe { libc::sigdelset(&mut self.set, signal) == 0 }
    }

    /// Whether `signal` is a member of the set.
    pub fn contains(&self, signal: libc::c_int) -> bool {
        // SAFETY: `self.set` is an initialised `sigset_t`.
        unsafe { libc::sigismember(&self.set, signal) == 1 }
    }

    /// Whether the set contains no signals.
    pub fn is_empty(&self) -> bool {
        self.iter().next().is_none()
    }

    /// Iterate the member signals in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = libc::c_int> + '_ {
        // libc does not export NSIG portably; 64 is Linux's `_NSIG - 1` and
        // covers the BSDs. `contains` is false for numbers the host lacks.
        (1..=64).filter(move |signal| self.contains(*signal))
    }

    /// Block these signals in the calling thread, returning a guard that
    /// restores the previous mask when dropped.
    pub fn block(&self) -> io::Result<MaskGuard> {
        self.apply(libc::SIG_BLOCK)
    }

    /// Unblock these signals in the calling thread, returning a guard that
    /// restores the previous mask when dropped.
    pub fn unblock(&self) -> io::Result<MaskGuard> {
        self.apply(libc::SIG_UNBLOCK)
    }

    /// Replace the calling thread's blocked-signal mask with this set,
    /// returning a guard that restores the previous mask when dropped.
    pub fn set(&self) -> io::Result<MaskGuard> {
        self.apply(libc::SIG_SETMASK)
    }

    /// The calling thread's current blocked-signal mask.
    ///
    /// `sigprocmask` is per-thread even though its name suggests otherwise;
    /// the shell reads it from the thread that owns the event loop.
    pub fn current() -> io::Result<Self> {
        let mut set = MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: `set` is valid for writes. Passing a null set leaves the mask
        // unchanged (`how` is ignored), so this only captures the old mask;
        // `sigprocmask` does not retain the pointer.
        let rc = unsafe { libc::sigprocmask(libc::SIG_BLOCK, std::ptr::null(), set.as_mut_ptr()) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: success means `set` was initialised.
        let set = unsafe { set.assume_init() };
        Ok(Self { set })
    }

    fn apply(&self, how: libc::c_int) -> io::Result<MaskGuard> {
        let mut previous = MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: `&self.set` and `previous` are valid; `sigprocmask` writes
        // the old mask on success and retains neither pointer.
        let rc = unsafe { libc::sigprocmask(how, &self.set, previous.as_mut_ptr()) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: success means `previous` was initialised.
        let previous = unsafe { previous.assume_init() };
        Ok(MaskGuard { previous })
    }

    /// The underlying `sigset_t`, for the spawn and signal-fd entry points.
    pub(crate) fn as_ptr(&self) -> *const libc::sigset_t {
        &self.set
    }
}

impl Default for SignalSet {
    fn default() -> Self {
        Self::empty()
    }
}

impl PartialEq for SignalSet {
    fn eq(&self, other: &Self) -> bool {
        self.iter().eq(other.iter())
    }
}

impl Eq for SignalSet {}

impl fmt::Debug for SignalSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// Restores the calling thread's signal mask when dropped.
///
/// Dropping ignores errors: the guard is normally created in ordinary code
/// paths, and a panic must still restore the mask as far as possible.
#[derive(Debug)]
pub struct MaskGuard {
    previous: libc::sigset_t,
}

impl MaskGuard {
    /// Restore the previous mask explicitly, reporting a failure.
    pub fn restore(self) -> io::Result<()> {
        let result = restore_mask(&self.previous);
        std::mem::forget(self);
        result
    }
}

impl Drop for MaskGuard {
    fn drop(&mut self) {
        let _ = restore_mask(&self.previous);
    }
}

fn restore_mask(mask: &libc::sigset_t) -> io::Result<()> {
    // SAFETY: `mask` is an initialised `sigset_t`; `sigprocmask` retains no
    // pointer.
    if unsafe { libc::sigprocmask(libc::SIG_SETMASK, mask, std::ptr::null_mut()) } != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Send `signal` to `pid`.
pub fn send_signal(pid: libc::pid_t, signal: libc::c_int) -> io::Result<()> {
    // SAFETY: `kill(2)` takes only integers and dereferences no user pointer.
    if unsafe { libc::kill(pid, signal) } != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Send `signal` to every process in process group `pgroup`.
///
/// `pgroup` must be a positive process-group id; zero ("this process's group")
/// and negative values ("every process") are rejected because a shell job
/// controls exactly the groups it created.
pub fn send_group_signal(pgroup: libc::pid_t, signal: libc::c_int) -> io::Result<()> {
    if pgroup <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "process group id must be positive",
        ));
    }
    send_signal(-pgroup, signal)
}

/// A signal disposition the shell sets on itself rather than through a
/// handler.
///
/// `trap '' SIG` is `Ignore`, and resetting a trap is `Default`. There is
/// deliberately no `Catch`: caught signals are never handled by a native
/// handler in this shell (DESIGN.md §4.5) — they are blocked and read from a
/// [`SignalFd`] at command boundaries instead, so no handler can ever touch
/// interpreter state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// `SIG_DFL`: the default action (terminate, stop, or ignore).
    Default,
    /// `SIG_IGN`: the signal is discarded, and children inherit that across
    /// `exec` (which is exactly why `trap '' INT` leaks a Ctrl-C-proof child
    /// unless the shell means it).
    Ignore,
}

/// Set the disposition of `signal`, replacing whatever was there.
pub fn set_disposition(signal: libc::c_int, disposition: Disposition) -> io::Result<()> {
    let handler = match disposition {
        Disposition::Default => libc::SIG_DFL,
        Disposition::Ignore => libc::SIG_IGN,
    };
    // SAFETY: a zeroed `sigaction` is a valid starting point, `sigemptyset`
    // initialises `sa_mask`, and `sigaction` copies the struct rather than
    // retaining the pointer.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler;
        action.sa_flags = 0;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Restores a set of signal dispositions when dropped.
///
/// Used around a foreground `wait`: bash ignores `SIGINT`/`SIGQUIT` delivered
/// to the *shell* while it waits for a job (the job has its own copy of the
/// signal), so a stray `kill -INT <shell>` does not kill the shell in the
/// middle of `trap … EXIT; sleep 1`. The previous actions — possibly `SIG_IGN`
/// from `trap ''` — are put back on drop, so a panic cannot leak an ignore.
pub struct SignalGuard {
    saved: Vec<(libc::c_int, libc::sigaction)>,
}

impl std::fmt::Debug for SignalGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignalGuard")
            .field(
                "signals",
                &self
                    .saved
                    .iter()
                    .map(|(signal, _)| *signal)
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl SignalGuard {
    /// Restore explicitly, reporting the first failure.
    pub fn restore(self) -> io::Result<()> {
        let this = std::mem::ManuallyDrop::new(self);
        let mut result = Ok(());
        for (signal, action) in &this.saved {
            // SAFETY: `action` was filled in by a successful `sigaction` call
            // for `signal`; the call retains no pointer.
            if unsafe { libc::sigaction(*signal, action, std::ptr::null_mut()) } != 0
                && result.is_ok()
            {
                result = Err(io::Error::last_os_error());
            }
        }
        result
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        for (signal, action) in &self.saved {
            // SAFETY: as in `restore`; errors are ignored on the drop path.
            unsafe { libc::sigaction(*signal, action, std::ptr::null_mut()) };
        }
    }
}

/// Ignore `signals` until the returned guard is dropped.
///
/// This is the `SIGINT`/`SIGQUIT` rule around a foreground wait, not a trap:
/// the shell's own trap/block machinery never uses native handlers.
pub fn ignore_signals(signals: &[libc::c_int]) -> io::Result<SignalGuard> {
    let mut saved: Vec<(libc::c_int, libc::sigaction)> = Vec::with_capacity(signals.len());
    for &signal in signals {
        // SAFETY: `action` is valid for writes; `sigaction` fills it in and
        // retains no pointer.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            let mut previous: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = libc::SIG_IGN;
            action.sa_flags = 0;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(signal, &action, &mut previous) != 0 {
                let error = io::Error::last_os_error();
                // Put back whatever this call already changed before failing.
                for (signal, action) in &saved {
                    libc::sigaction(*signal, action, std::ptr::null_mut());
                }
                return Err(error);
            }
            saved.push((signal, previous));
        }
    }
    Ok(SignalGuard { saved })
}

/// Replace the calling thread's blocked-signal mask and keep it.
///
/// [`SignalSet::set`] returns a guard that restores the previous mask; a forked
/// child that must not inherit the shell's blocked trap signals needs the
/// change to persist for its whole life, which is this function.
pub fn replace_signal_mask(set: &SignalSet) -> io::Result<()> {
    // SAFETY: `set` is an initialised `sigset_t`; `sigprocmask` retains no
    // pointer.
    if unsafe { libc::sigprocmask(libc::SIG_SETMASK, set.as_ptr(), std::ptr::null_mut()) } != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// One signal notification read from a [`SignalFd`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignalEvent {
    /// The signal number that was delivered.
    pub signal: libc::c_int,
    /// `si_code`: why the signal was generated (e.g. `SI_USER`, `CLD_EXITED`).
    pub code: i32,
    /// The sending process, when the kernel supplies one.
    pub pid: u32,
    /// The sending user, when the kernel supplies one.
    pub uid: u32,
}

#[cfg(target_os = "linux")]
mod platform {
    use std::io;
    use std::mem::MaybeUninit;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

    use super::{SignalEvent, SignalSet};

    /// A pollable, non-blocking Linux `signalfd` reader.
    ///
    /// The signals in the mask must be blocked with [`SignalSet::block`]
    /// *before* the descriptor is created, in every thread that could
    /// otherwise receive them — otherwise their default action (or a handler)
    /// still runs and the signal may never reach the descriptor. The shell's
    /// event loop is single-threaded at this point, so that means the calling
    /// thread. This type only reads notifications; it does not manage the
    /// mask.
    pub struct SignalFd {
        fd: OwnedFd,
    }

    impl SignalFd {
        /// Create a signal reader for `mask`.
        ///
        /// The descriptor is close-on-exec and non-blocking, so the event loop
        /// can poll it and drain it until [`SignalFd::read`] returns `Ok(None)`.
        pub fn new(mask: &SignalSet) -> io::Result<Self> {
            // SAFETY: `-1` asks for a new descriptor; `mask.as_ptr()` is a
            // valid `sigset_t`; the flags are valid signalfd flags.
            let fd = unsafe {
                libc::signalfd(-1, mask.as_ptr(), libc::SFD_CLOEXEC | libc::SFD_NONBLOCK)
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: the descriptor is newly created and unowned.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            Ok(Self { fd })
        }

        /// Replace the set of signals this reader reports.
        pub fn set_mask(&mut self, mask: &SignalSet) -> io::Result<()> {
            // SAFETY: `self.fd` is a live signalfd and `mask` is a valid set.
            let fd = unsafe { libc::signalfd(self.fd.as_raw_fd(), mask.as_ptr(), 0) };
            if fd < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }

        /// The descriptor, for `poll`/`epoll` registration.
        pub fn as_raw_fd(&self) -> RawFd {
            self.fd.as_raw_fd()
        }

        /// Read the next pending notification.
        ///
        /// Returns `Ok(None)` when the descriptor would block, so an event
        /// loop drains with `while let Some(event) = reader.read()? { … }`.
        pub fn read(&self) -> io::Result<Option<SignalEvent>> {
            let mut info = MaybeUninit::<libc::signalfd_siginfo>::uninit();
            loop {
                // SAFETY: `info` points to a buffer of exactly one
                // `signalfd_siginfo`, which is the required record size.
                let count = unsafe {
                    libc::read(
                        self.fd.as_raw_fd(),
                        info.as_mut_ptr().cast(),
                        std::mem::size_of::<libc::signalfd_siginfo>(),
                    )
                };
                if count < 0 {
                    let error = io::Error::last_os_error();
                    match error.kind() {
                        io::ErrorKind::Interrupted => continue,
                        io::ErrorKind::WouldBlock => return Ok(None),
                        _ => return Err(error),
                    }
                }
                if count == 0 {
                    // signalfd never reports EOF; treat it like "nothing now".
                    return Ok(None);
                }
                if (count as usize) < std::mem::size_of::<libc::signalfd_siginfo>() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "short signalfd record",
                    ));
                }
                // SAFETY: the kernel wrote a full record, so every field is
                // initialised.
                let info = unsafe { info.assume_init() };
                return Ok(Some(SignalEvent {
                    signal: info.ssi_signo as libc::c_int,
                    code: info.ssi_code,
                    pid: info.ssi_pid,
                    uid: info.ssi_uid,
                }));
            }
        }
    }

    impl std::fmt::Debug for SignalFd {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("SignalFd")
                .field("fd", &self.fd.as_raw_fd())
                .finish()
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use std::io;
    use std::os::fd::RawFd;

    use super::{SignalEvent, SignalSet};

    /// A pollable signal reader.
    ///
    /// `signalfd(2)` is Linux-specific. On other Unixes this build has no
    /// working implementation yet: [`SignalFd::new`] fails with
    /// [`io::ErrorKind::Unsupported`] rather than silently pretending that
    /// signals are being collected. The planned portable replacement is a
    /// self-pipe installed with `sigaction` (see DESIGN.md §4.5).
    #[derive(Debug)]
    pub struct SignalFd {
        _private: (),
    }

    impl SignalFd {
        /// Always fails with [`io::ErrorKind::Unsupported`] on this platform.
        pub fn new(_mask: &SignalSet) -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "signalfd is Linux-only; the self-pipe fallback is not implemented yet",
            ))
        }

        /// Always fails with [`io::ErrorKind::Unsupported`] on this platform.
        pub fn set_mask(&mut self, _mask: &SignalSet) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "signalfd is Linux-only; the self-pipe fallback is not implemented yet",
            ))
        }

        /// Always returns `-1` on this platform; no `SignalFd` can be created.
        pub fn as_raw_fd(&self) -> RawFd {
            -1
        }

        /// Always fails with [`io::ErrorKind::Unsupported`] on this platform.
        pub fn read(&self) -> io::Result<Option<SignalEvent>> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "signalfd is Linux-only; the self-pipe fallback is not implemented yet",
            ))
        }
    }
}

pub use platform::SignalFd;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_membership_and_iteration() {
        let mut set = SignalSet::empty();
        assert!(set.is_empty());
        assert!(set.insert(libc::SIGINT));
        assert!(set.insert(libc::SIGTERM));
        assert!(set.contains(libc::SIGINT));
        assert!(!set.contains(libc::SIGUSR1));
        assert_eq!(
            set.iter().collect::<Vec<_>>(),
            vec![libc::SIGINT, libc::SIGTERM]
        );
        assert!(set.remove(libc::SIGINT));
        assert_eq!(set.iter().collect::<Vec<_>>(), vec![libc::SIGTERM]);
        assert_eq!(SignalSet::of(&[libc::SIGTERM]), set);
    }

    #[test]
    fn block_and_unblock_restore_the_previous_mask() {
        let before = SignalSet::current().unwrap();
        let set = SignalSet::of(&[libc::SIGUSR2]);
        {
            let _guard = set.block().unwrap();
            assert!(SignalSet::current().unwrap().contains(libc::SIGUSR2));
        }
        assert_eq!(SignalSet::current().unwrap(), before);
    }

    #[test]
    fn group_signal_rejects_non_job_ids() {
        assert!(send_group_signal(0, libc::SIGTERM).is_err());
        assert!(send_group_signal(-1, libc::SIGTERM).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn signalfd_reads_would_block_as_none() {
        // Deliberately does not raise a signal: tests run on threads, a
        // process-directed signal can be delivered to a thread that does not
        // block it, and SIGUSR1's default action would kill the test harness.
        let set = SignalSet::of(&[libc::SIGUSR1]);
        let _blocked = set.block().unwrap();
        let mut reader = SignalFd::new(&set).unwrap();
        assert!(reader.read().unwrap().is_none());
        reader.set_mask(&SignalSet::of(&[libc::SIGUSR2])).unwrap();
        assert!(reader.as_raw_fd() >= 0);
    }
}
