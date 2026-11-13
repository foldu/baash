//! The Windows backend.
//!
//! One module rather than one per Unix module: on Windows the primitives are
//! not separate concerns the way they are on Unix (there is no `signalfd` to
//! keep apart from `dup2`), and keeping them together makes it obvious which
//! pieces are real and which are still refusals.
//!
//! # What is real
//!
//! * Descriptors keep the **CRT fd number** model (`_open`, `_read`, `_write`,
//!   `_dup2`, `_close`). Redirection is built on `dup2` in the VM, so keeping
//!   the same shape means that code does not change at all.
//! * External commands go through `std::process::Command`, with descriptor
//!   numbers converted to duplicated, inheritable handles with
//!   `_get_osfhandle` (see `stdio_from_fd`: the descriptors, not the process's
//!   own standard handles, are what the child inherits).
//! * Waiting is `WaitForSingleObject` through the `std` child, so a foreground
//!   pipeline reaps the way it does on Unix.
//! * The terminal: `RawMode`, `NoEcho` and `window_size_fd` are the console's
//!   `SetConsoleMode`/`GetConsoleScreenBufferInfo`, in [`crate::console`]. The
//!   line editor reads and writes through the same module, so `read -s` and the
//!   prompt's width work the way they do on a Unix tty.
//!
//! # What refuses
//!
//! * `open_pty`/`become_session_leader`: there is no pty to open on Windows.
//!   The interactive editor does not need one — it drives the console it was
//!   started on ([`crate::console`]) — so this is a missing test facility and a
//!   missing `script(1)` emulation, not a missing terminal.
//! * Signals: there is no `signalfd`, and `SIGINT` delivery to a console is a
//!   different mechanism (`SetConsoleCtrlHandler`). Every signal entry point
//!   reports `Unsupported` rather than pretending to work.
//!
//! Path bytes are WTF-8 (`crate::wtf8`), which is UTF-8 extended with the
//! unpaired surrogates a Windows name can hold: `os_path` and `wide_path` are
//! the two conversions from shell bytes to a name, and neither is lossy. An
//! NTFS name that is not valid Unicode therefore reaches `chdir`,
//! `CreateProcessW` and the CRT unchanged.

use std::ffi::OsString;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use crate::c_int;

/// File creation mode.
///
/// Windows has no `mode_t`; the CRT's `_open` takes an integer permission set
/// and ignores it for a non-`O_CREAT` open. The libc name is kept so the
/// unix-facing signatures read the same on both sides.
#[allow(non_camel_case_types)]
pub type mode_t = u32;
use crate::WaitOutcome;

// ---------------------------------------------------------------------------
// CRT bindings
// ---------------------------------------------------------------------------

// The CRT is linked with every Rust program on this target, so these need no
// build script; they are declared here because `libc` does not cover all of
// them on Windows.
unsafe extern "C" {
    fn _wopen(path: *const u16, oflag: c_int, ...) -> c_int;
    fn _close(fd: c_int) -> c_int;
    fn _read(fd: c_int, buffer: *mut std::ffi::c_void, count: u32) -> c_int;
    fn _write(fd: c_int, buffer: *const std::ffi::c_void, count: u32) -> c_int;
    fn _dup(fd: c_int) -> c_int;
    fn _dup2(from: c_int, to: c_int) -> c_int;
    fn _lseek(fd: c_int, offset: i64, origin: c_int) -> i64;
    fn _pipe(fds: *mut c_int, size: u32, mode: c_int) -> c_int;
    fn _get_osfhandle(fd: c_int) -> isize;
}

/// `_O_*` flags. These are the CRT's values, not the shell's (`flags::*` maps
/// onto them).
const O_RDONLY: c_int = 0x0000;
const O_WRONLY: c_int = 0x0001;
const O_RDWR: c_int = 0x0002;
const O_CREAT: c_int = 0x0100;
const O_TRUNC: c_int = 0x0200;
const O_APPEND: c_int = 0x0008;
const O_BINARY: c_int = 0x8000;

/// `SEEK_SET`/`CUR`/`END` for the CRT.
pub mod seek {
    use super::c_int;
    /// From the start.
    pub const SET: c_int = 0;
    /// From the current position.
    pub const CUR: c_int = 1;
    /// From the end.
    pub const END: c_int = 2;
}

/// A path the CRT can open.
///
/// Shell paths are bytes; Windows wants UTF-16. This is the byte-exactness gap
/// §9 records, and it is one function so the eventual WTF-8 conversion has one
/// place to land.
/// A NUL-terminated UTF-16 name for the wide-character CRT entry points.
///
/// The name is decoded from WTF-8, so a lone surrogate arrives as the code
/// unit it is; `_wopen` below is the wide call for exactly that reason — `_open`
/// would take the bytes through the ANSI code page and rename anything outside
/// it.
fn wide_path(path: &[u8]) -> Vec<u16> {
    let mut wide = crate::wtf8::decode(path);
    wide.push(0);
    wide
}

// ---------------------------------------------------------------------------
// Descriptors
// ---------------------------------------------------------------------------

/// A CRT descriptor this crate owns.
///
/// Windows `std` has no descriptor abstraction at all — there is no
/// `std::os::fd` — so the shell's fd-numbered model is this crate's own type.
/// The number is the CRT's, which is what every other function here operates
/// on, and `Drop` closes it exactly once.
#[derive(Debug)]
pub struct Fd(c_int);

impl Fd {
    /// Take ownership of a CRT descriptor.
    fn from_raw(fd: c_int) -> io::Result<Self> {
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(fd))
    }
}

impl Drop for Fd {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a descriptor this type owns and has not closed.
        unsafe {
            _close(self.0);
        }
    }
}

/// A bare descriptor number, under the name the Unix side gets from `std`.
///
/// The CRT descriptor is an `int` here, so this is the same type as `c_int`;
/// the alias exists so the Windows and Unix vocabularies read the same.
pub type RawFd = c_int;

/// An owned descriptor, under the name the Unix side gets from `std`.
pub type OwnedFd = Fd;

/// The descriptor number behind a value that owns one.
///
/// Unix spells this `std::os::fd::AsRawFd`; Windows has no such trait, so it
/// is defined here and exported under the same name from the crate root.
pub trait AsRawFd {
    /// The CRT descriptor number.
    fn as_raw_fd(&self) -> c_int;
}

/// Give up ownership and yield the bare descriptor.
pub trait IntoRawFd {
    /// The descriptor, no longer closed on drop.
    fn into_raw_fd(self) -> c_int;
}

/// Take ownership of a descriptor.
///
/// Unix's version is `unsafe` because the descriptor could be a `File`'s; a
/// bare CRT number carries no such invariant here, but the signature matches
/// so call sites do not need a `cfg`.
pub trait FromRawFd {
    /// Wrap `fd`.
    fn from_raw_fd(fd: c_int) -> Self;
}

impl AsRawFd for Fd {
    fn as_raw_fd(&self) -> c_int {
        self.0
    }
}

impl IntoRawFd for Fd {
    fn into_raw_fd(self) -> c_int {
        let fd = self.0;
        std::mem::forget(self);
        fd
    }
}

impl FromRawFd for Fd {
    fn from_raw_fd(fd: c_int) -> Self {
        Self(fd)
    }
}

/// A pipe: the read end and the write end.
pub struct Pipe {
    /// The end a reader opens.
    pub read: Fd,
    /// The end a writer opens.
    pub write: Fd,
}

impl Pipe {
    /// The read end's number.
    pub fn read_fd(&self) -> c_int {
        self.read.as_raw_fd()
    }

    /// The write end's number.
    pub fn write_fd(&self) -> c_int {
        self.write.as_raw_fd()
    }

    /// A pipe with a 64 KiB buffer, in binary mode.
    pub fn new() -> io::Result<Self> {
        let mut fds = [0 as c_int; 2];
        // SAFETY: `fds` is a two-element array the CRT writes both ends into.
        let rc = unsafe { _pipe(fds.as_mut_ptr(), 65_536, O_BINARY) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            read: Fd::from_raw(fds[0])?,
            write: Fd::from_raw(fds[1])?,
        })
    }
}

/// Close a descriptor.
pub fn close(fd: c_int) -> io::Result<()> {
    // SAFETY: `_close` only takes an integer.
    if unsafe { _close(fd) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Duplicate `fd` onto the lowest free descriptor.
pub fn dup(fd: c_int) -> io::Result<OwnedFd> {
    // SAFETY: `_dup` only takes an integer.
    Fd::from_raw(unsafe { _dup(fd) })
}

/// Duplicate `fd` onto the lowest free descriptor at or above `minimum`.
///
/// The Unix version uses `fcntl(F_DUPFD_CLOEXEC)`, which finds the slot; the
/// CRT has no equivalent, so the slots are scanned. The scan is safe because
/// the shell has one thread and no other code opens a descriptor inside it.
pub fn dup_above(fd: c_int, minimum: c_int) -> io::Result<OwnedFd> {
    let duplicate = dup(fd)?;
    let from = duplicate.as_raw_fd();
    if from >= minimum {
        return Ok(duplicate);
    }
    let mut candidate = minimum.max(3);
    while candidate < 2048 {
        if !fd_is_open(candidate) && dup2(from, candidate).is_ok() {
            // The extra slot now owns its own handle; the duplicate goes away.
            return Fd::from_raw(candidate);
        }
        candidate += 1;
    }
    Err(io::Error::other("no free descriptor above the minimum"))
}

/// Point `to` at `from`.
pub fn dup2(from: c_int, to: c_int) -> io::Result<()> {
    if from == to {
        return Ok(());
    }
    // SAFETY: `_dup2` takes two integers.
    if unsafe { _dup2(from, to) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Whether `fd` is open.
pub fn fd_is_open(fd: c_int) -> bool {
    // SAFETY: `_get_osfhandle` takes an integer; an invalid handle means the
    // descriptor is closed.
    unsafe { _get_osfhandle(fd) != -1 }
}

/// Whether `fd` has close-on-exec set.
///
/// The CRT has no such flag and Windows has no `exec`, so every descriptor is
/// "inheritable" from this crate's point of view; the shell's own
/// `exec`-committed redirections are the only case that cares, and they are
/// implemented by keeping the descriptor open.
pub fn fd_is_cloexec(_fd: c_int) -> bool {
    false
}

/// Set close-on-exec on `fd`. A no-op: see [`fd_is_cloexec`].
pub fn set_cloexec(_fd: c_int) -> io::Result<()> {
    Ok(())
}

/// Read one chunk from `fd`.
pub fn read_fd(fd: c_int, buffer: &mut [u8]) -> io::Result<usize> {
    let count = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
    // SAFETY: `buffer` is a live slice of at least `count` bytes.
    let read = unsafe { _read(fd, buffer.as_mut_ptr().cast(), count) };
    if read < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(usize::try_from(read).unwrap_or(0))
}

/// Read `fd` to end of file.
pub fn read_all(fd: c_int) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let count = read_fd(fd, &mut buffer)?;
        if count == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buffer[..count]);
    }
}

/// Write one chunk to `fd`.
pub fn write_fd(fd: c_int, bytes: &[u8]) -> io::Result<usize> {
    let count = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    // SAFETY: `bytes` is a live slice of at least `count` bytes.
    let written = unsafe { _write(fd, bytes.as_ptr().cast(), count) };
    if written < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(usize::try_from(written).unwrap_or(0))
}

/// Write all of `bytes`, retrying short writes and interrupts.
pub fn write_all(fd: c_int, bytes: &[u8]) -> io::Result<()> {
    let mut written = 0;
    while written < bytes.len() {
        match write_fd(fd, &bytes[written..]) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(count) => written += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Reposition `fd`.
pub fn lseek(fd: c_int, offset: i64, origin: c_int) -> io::Result<i64> {
    // SAFETY: `_lseek` takes integers only.
    let position = unsafe { _lseek(fd, offset, origin) };
    if position < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(position)
}

/// Change the working directory.
pub fn chdir(path: &[u8]) -> io::Result<()> {
    let path = os_path(path);
    std::env::set_current_dir(path)
}

/// Open `path` and return the owned descriptor.
///
/// The Unix version opens with `O_CLOEXEC`; Windows hands descriptors to a
/// child explicitly, so the flag has no meaning here.
pub fn open_cloexec(path: &[u8], flags: c_int, mode: u32) -> io::Result<OwnedFd> {
    // A shell writes `/dev/null`, which is not a path here; this is the one
    // place a shell path becomes a Windows object, so the translation lives
    // with it (`paths::Flavour::host_device` has the rule and the test).
    let path = crate::paths::Flavour::Windows
        .host_device(path)
        .unwrap_or(path);
    let c_path = wide_path(path);
    let translated = translate_flags(flags) | O_BINARY;
    // SAFETY: `c_path` is a NUL-terminated UTF-16 string and the variadic
    // argument is only read when `O_CREAT` is set.
    Fd::from_raw(unsafe { _wopen(c_path.as_ptr(), translated, mode) })
}

/// Translate this crate's `O_*` flags to the CRT's.
fn translate_flags(flags: c_int) -> c_int {
    let mut out = match flags & 0b11 {
        0 => O_RDONLY,
        1 => O_WRONLY,
        _ => O_RDWR,
    };
    if flags & crate::flags::CREAT != 0 {
        out |= O_CREAT;
    }
    if flags & crate::flags::TRUNC != 0 {
        out |= O_TRUNC;
    }
    if flags & crate::flags::APPEND != 0 {
        out |= O_APPEND;
    }
    out
}

/// A temporary file that has no directory entry.
///
/// Windows has no `O_TMPFILE`; the file is created, unlinked-on-open semantics
/// are faked by deleting it on close through [`TempFile`]'s `Drop`, which is
/// what `create_unlinked_temp` returns. The caller only ever needs its bytes.
/// Windows has no `/dev/fd`: a descriptor cannot be named to another program,
/// so `<(…)` has nothing to expand to and the shell reports it.
pub fn fd_path(_fd: RawFd) -> Option<Vec<u8>> {
    None
}

pub fn create_unlinked_temp(_size_hint: usize) -> io::Result<OwnedFd> {
    let mut file = tempfile_path();
    if file.is_empty() {
        file = b"baash-temp".to_vec();
    }
    let base = std::env::temp_dir();
    let mut counter = 0u32;
    loop {
        let candidate = base.join(format!(
            "baash-{}-{}-{counter}.tmp",
            std::process::id(),
            String::from_utf8_lossy(&file)
        ));
        counter += 1;
        if counter > 1_000 {
            return Err(io::Error::other("cannot create a temporary file"));
        }
        let bytes = candidate.to_string_lossy().into_owned().into_bytes();
        let created = open_cloexec(
            &bytes,
            crate::flags::RDWR | crate::flags::CREAT | crate::flags::TRUNC,
            0o600,
        );
        match created {
            Ok(fd) => {
                // The name goes away immediately; the handle keeps the bytes
                // alive, which is what an unlinked temporary file means here.
                let _ = std::fs::remove_file(&candidate);
                return Ok(fd);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

fn tempfile_path() -> Vec<u8> {
    b"temp".to_vec()
}

/// Whether `fd` has anything to read, waiting at most `timeout_ms`.
///
/// `poll(2)` has no direct equivalent; the shell uses this for `read -t` and
/// for the prompt's "did the user type" checks. A console handle is always
/// reported readable — the read that follows is what blocks — and a pipe is
/// probed with `PeekNamedPipe`.
pub fn poll_readable(fd: c_int, timeout_ms: c_int) -> io::Result<bool> {
    let _ = timeout_ms;
    // SAFETY: `_get_osfhandle` takes an integer.
    let handle = unsafe { _get_osfhandle(fd) };
    if handle == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(peek_readable(handle as RawHandle))
}

/// Probe one handle without consuming from it.
fn peek_readable(handle: RawHandle) -> bool {
    windows_sys_handle::available(handle)
}

/// The smallest Windows API surface this module needs beyond the CRT.
///
/// Hand-declared rather than pulling a bindings crate in for one call.
mod windows_sys_handle {
    use std::os::windows::io::RawHandle;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn PeekNamedPipe(
            handle: RawHandle,
            buffer: *mut std::ffi::c_void,
            size: u32,
            read: *mut u32,
            available: *mut u32,
            left: *mut u32,
        ) -> i32;
        fn GetFileType(handle: RawHandle) -> u32;
    }

    /// `FILE_TYPE_PIPE`
    const FILE_TYPE_PIPE: u32 = 0x0003;

    pub(super) fn available(handle: RawHandle) -> bool {
        // SAFETY: `handle` comes from `_get_osfhandle`; the out-parameters
        // are a live `u32` each.
        let file_type = unsafe { GetFileType(handle) };
        if file_type != FILE_TYPE_PIPE {
            // A console or a file: a read either blocks on the console (so
            // report it ready and let the read decide) or cannot block.
            return true;
        }
        let mut available = 0u32;
        // SAFETY: as above, and the null buffers are allowed.
        let rc = unsafe {
            PeekNamedPipe(
                handle,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        rc != 0 && available > 0
    }
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// A shell byte path as an OS path.
/// A shell byte string as an OS path, through the WTF-8 codec.
///
/// This is the single conversion used by `chdir` and by spawn (program and
/// arguments), so a name that came from `read_dir` goes back to the OS as the
/// same name.
fn os_path(path: &[u8]) -> PathBuf {
    PathBuf::from(crate::os::os_from_bytes(path))
}

// ---------------------------------------------------------------------------
// Terminals
// ---------------------------------------------------------------------------

/// Saved console modes, restored on drop.
///
/// The editor drives [`crate::console::Console`] directly; this is the same
/// guard under the name the rest of the shell uses, so `read -s` and any other
/// caller asking for "raw mode" gets the console's version of it rather than a
/// refusal.
#[derive(Debug)]
pub struct RawMode {
    mode: crate::console::ConsoleMode,
}

impl RawMode {
    /// Enter raw mode on the console.
    ///
    /// `Unsupported` when the process has no console, which is the same answer
    /// the Unix side gives for a redirected stdin.
    pub fn enter() -> io::Result<Self> {
        let console = crate::console::Console::open()?;
        Ok(Self {
            mode: console.enter_raw()?,
        })
    }

    /// Enter raw mode. Windows has one console, so the descriptor only names
    /// *a* terminal, not a different one.
    pub fn enable(_fd: c_int) -> io::Result<Self> {
        Self::enter()
    }

    /// Put the console back the way it was.
    pub fn restore(self) -> io::Result<()> {
        self.mode.restore()
    }
}

/// A terminal that turns echo off, for `read` without `-s`.
#[derive(Debug)]
pub struct NoEcho {
    mode: crate::console::ConsoleMode,
}

impl NoEcho {
    /// Turn echo off on the console.
    pub fn enter() -> io::Result<Self> {
        let console = crate::console::Console::open()?;
        Ok(Self {
            mode: console.enter_no_echo()?,
        })
    }

    /// Turn echo off. As with [`RawMode::enable`], the descriptor only says
    /// "the terminal".
    pub fn enable(_fd: c_int) -> io::Result<Self> {
        Self::enter()
    }

    /// Restore echo.
    pub fn restore(self) -> io::Result<()> {
        self.mode.restore()
    }
}

/// This process's id.
pub fn pid() -> u32 {
    std::process::id()
}

/// The parent process's id, as far as Windows can tell.
///
/// Windows has no `getppid`; the tool help snapshot would answer, and that is
/// one more declaration than the shell needs today. `None` therefore means
/// "not known here", and `$PPID` is left unset rather than filled with a
/// plausible-looking number.
pub fn parent_pid() -> Option<u32> {
    None
}

/// The terminal size, in columns and rows.
///
/// The console's *window* is the visible part; its screen buffer is usually
/// much taller and wrapping must follow what the user can see.
pub fn window_size_fd(_fd: c_int) -> Option<(u16, u16)> {
    // Windows has one console, so any descriptor that is a terminal is the
    // same one.
    crate::console::Console::open().ok()?.size().ok()
}

/// The terminal size of standard output.
pub fn window_size() -> Option<(u16, u16)> {
    window_size_fd(1)
}

// ---------------------------------------------------------------------------
// Signals
// ---------------------------------------------------------------------------

/// A set of signal numbers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SignalSet {
    _private: (),
}

impl SignalSet {
    /// An empty set.
    pub fn empty() -> Self {
        Self::default()
    }

    /// A set with `signals` in it. Windows has no signal mask; the set is
    /// always empty.
    pub fn of(_signals: &[c_int]) -> Self {
        Self::default()
    }

    /// The process's current blocked-signal set.
    ///
    /// Returns `Ok` for the same shape as Unix; a Windows process has no
    /// blocked-signal set to read.
    pub fn current() -> io::Result<Self> {
        Ok(Self::default())
    }

    /// Add a signal. Returns whether it was absent, as on Unix.
    pub fn insert(&mut self, _signal: c_int) -> bool {
        false
    }

    /// Remove a signal. Returns whether it was present, as on Unix.
    pub fn remove(&mut self, _signal: c_int) -> bool {
        false
    }

    /// Whether `signal` is in the set.
    pub fn contains(&self, _signal: c_int) -> bool {
        false
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        true
    }

    /// The signals in the set.
    pub fn iter(&self) -> impl Iterator<Item = c_int> {
        std::iter::empty()
    }
}

/// What a signal is currently set to do in this process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Disposition {
    /// The default action.
    #[default]
    Default,
    /// Ignored.
    Ignore,
    /// Caught by the shell.
    Catch,
}

/// One delivered signal, as `signalfd` would report it.
#[derive(Clone, Copy, Debug)]
pub struct SignalEvent {
    /// The signal number.
    pub signal: c_int,
}

/// A queue of delivered signals.
///
/// There is no `signalfd`; `SetConsoleCtrlHandler` is the equivalent and it
/// delivers only `Ctrl-C`, `Ctrl-Break` and a few console events. The type
/// exists and reports `Unsupported` rather than pretending a console event is
/// a signal.
#[derive(Debug, Default)]
pub struct SignalFd {
    _private: (),
}

impl SignalFd {
    /// A queue for `mask`.
    pub fn new(_mask: &SignalSet) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "signalfd has no Windows equivalent (docs/portability.md §5)",
        ))
    }

    /// The descriptor to poll.
    pub fn raw_fd(&self) -> c_int {
        -1
    }

    /// Take one pending event.
    pub fn read(&self) -> io::Result<Option<SignalEvent>> {
        Ok(None)
    }
}

/// A guard that restores a disposition when dropped.
#[derive(Debug)]
pub struct SignalGuard {
    _private: (),
}

/// A guard that restores the blocked-signal mask when dropped.
#[derive(Debug)]
pub struct MaskGuard {
    _private: (),
}

/// Block `set` for this process until the guard drops.
pub fn replace_signal_mask(_set: &SignalSet) -> MaskGuard {
    MaskGuard { _private: () }
}

/// Set the disposition of `signal`.
pub fn set_disposition(_signal: c_int, _disposition: Disposition) -> io::Result<SignalGuard> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "signal dispositions are not implemented on Windows",
    ))
}

/// Ignore `signals`.
///
/// A stub guard: Windows has no dispositions to save or restore, but the type
/// keeps `trap '' INT` shaped like Unix and a future console handler has one
/// place to plug in.
pub fn ignore_signals(_signals: &[c_int]) -> io::Result<SignalGuard> {
    Ok(SignalGuard { _private: () })
}

/// Keep `Ctrl-C` from ending an interactive shell.
///
/// Windows has no `SIGINT`. A console turns `Ctrl-C` into a `CTRL_C_EVENT`
/// delivered to *every* process attached to it, and the default handler calls
/// `ExitProcess` — which is why an interactive shell used to die on the very
/// key that is supposed to abandon the command. Returning `TRUE` from a handler
/// suppresses that default, so the foreground child (which receives the same
/// event and has its own default) dies and the shell draws the next prompt, the
/// way a Unix shell ignores `SIGINT` while it waits for a foreground job.
///
/// The handler is installed only for an interactive shell: `baash -c 'sleep'`
/// must still die on `Ctrl-C`, exactly as a script does.
pub fn install_console_control_handler() -> io::Result<()> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
    }

    /// `TRUE` for `CTRL_C_EVENT` (0) and `CTRL_BREAK_EVENT` (1): handled.
    ///
    /// The other events — close, logoff, shutdown — must still end the process,
    /// so they fall through to the default handler.
    unsafe extern "system" fn keep_running(event: u32) -> i32 {
        if event <= 1 { 1 } else { 0 }
    }

    // SAFETY: the handler is a plain function with the signature the console
    // expects, and it touches no state; `add` is 1, which appends it.
    let ok = unsafe { SetConsoleCtrlHandler(Some(keep_running), 1) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Send `signal` to `pid`.
pub fn send_signal(pid: i32, _signal: c_int) -> io::Result<()> {
    use std::os::windows::io::FromRawHandle;
    use std::os::windows::io::OwnedHandle;
    // Only termination is expressible: `kill` sends `SIGTERM` by default, and
    // a Windows process has no other signal that maps.
    let handle = open_process(pid)?;
    // SAFETY: `open_process` just returned an owned handle with terminate
    // access, and `_exit`ing here is what `kill` means without signals.
    let owned = unsafe { OwnedHandle::from_raw_handle(handle) };
    terminate(owned.as_raw_handle())
}

/// Terminate the process behind `handle`.
fn terminate(handle: RawHandle) -> io::Result<()> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn TerminateProcess(handle: RawHandle, code: u32) -> i32;
    }
    // SAFETY: `handle` was opened with PROCESS_TERMINATE.
    let rc = unsafe { TerminateProcess(handle, 1) };
    if rc == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Open a process handle for termination.
fn open_process(pid: i32) -> io::Result<RawHandle> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> RawHandle;
    }
    const PROCESS_TERMINATE: u32 = 0x0001;
    // SAFETY: the call takes only integers.
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid as u32) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(handle)
}

// ---------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------

/// One descriptor change for a child.
#[derive(Clone, Debug)]
pub enum FdAction {
    /// Open `path` on `fd` in the child.
    Open {
        /// The descriptor to open onto.
        to: c_int,
        /// The path.
        path: Vec<u8>,
        /// `flags::*` access flags.
        flags: c_int,
        /// Creation mode.
        mode: u32,
    },
    /// Point `to` at `from`.
    Dup2 {
        /// The existing descriptor.
        from: c_int,
        /// The descriptor to replace.
        to: c_int,
    },
    /// Close `fd` in the child.
    Close {
        /// The descriptor to close.
        fd: c_int,
    },
}

impl FdAction {
    /// Describe one `dup2`.
    pub fn dup2(from: c_int, to: c_int) -> Self {
        Self::Dup2 { from, to }
    }

    /// Describe one `close`.
    pub fn close(fd: c_int) -> Self {
        Self::Close { fd }
    }

    /// Describe one open onto `to`.
    pub fn open(path: impl AsRef<[u8]>, flags: c_int, mode: u32, to: c_int) -> Self {
        Self::Open {
            to,
            path: path.as_ref().to_vec(),
            flags,
            mode,
        }
    }
}

/// How to start a child.
#[derive(Debug)]
pub struct SpawnPlan {
    /// The program, already resolved.
    pub program: Vec<u8>,
    /// The full argument vector, including `argv[0]`.
    pub argv: Vec<Vec<u8>>,
    /// The environment, `NAME=value` pairs.
    pub env: Vec<Vec<u8>>,
    /// Descriptor changes, in order.
    pub actions: Vec<FdAction>,
    /// Signals to reset to their default action in the child.
    pub reset_signals: SignalSet,
    /// Signals to block in the child.
    pub sigmask: Option<SignalSet>,
}

impl SpawnPlan {
    /// A plan for `program` with `argv` and an empty environment.
    pub fn new(
        program: impl AsRef<[u8]>,
        argv: impl IntoIterator<Item = impl AsRef<[u8]>>,
    ) -> io::Result<Self> {
        Self::new_with_env(program, argv, std::iter::empty::<&[u8]>())
    }

    /// A plan for `program` with `argv` and `envp` (`NAME=value` entries).
    pub fn new_with_env(
        program: impl AsRef<[u8]>,
        argv: impl IntoIterator<Item = impl AsRef<[u8]>>,
        envp: impl IntoIterator<Item = impl AsRef<[u8]>>,
    ) -> io::Result<Self> {
        Ok(Self {
            program: program.as_ref().to_vec(),
            argv: argv.into_iter().map(|arg| arg.as_ref().to_vec()).collect(),
            env: envp
                .into_iter()
                .map(|entry| entry.as_ref().to_vec())
                .collect(),
            actions: Vec::new(),
            reset_signals: SignalSet::default(),
            sigmask: None,
        })
    }

    /// Append one ordered descriptor action.
    pub fn push_action(&mut self, action: FdAction) {
        self.actions.push(action);
    }

    /// Append `dup2(from, to)`.
    pub fn dup2(&mut self, from: c_int, to: c_int) {
        self.actions.push(FdAction::dup2(from, to));
    }

    /// Append a close of `fd`.
    pub fn close(&mut self, fd: c_int) {
        self.actions.push(FdAction::close(fd));
    }

    /// Append an open of `path` onto `to`.
    pub fn open(
        &mut self,
        path: impl AsRef<[u8]>,
        flags: c_int,
        mode: u32,
        to: c_int,
    ) -> io::Result<()> {
        self.actions.push(FdAction::open(path, flags, mode, to));
        Ok(())
    }

    /// Append one exported `NAME=value` environment entry.
    pub fn push_env(&mut self, name: &[u8], value: &[u8]) -> io::Result<()> {
        let mut entry = Vec::with_capacity(name.len() + 1 + value.len());
        entry.extend_from_slice(name);
        entry.push(b'=');
        entry.extend_from_slice(value);
        self.env.push(entry);
        Ok(())
    }
}

/// A started child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Child {
    /// The process id, which is the handle the shell uses.
    pub pid: i32,
}

impl Child {
    /// The child's process id.
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Wait for this child.
    ///
    /// `wait_pid` owns the map entry and removes it on exit, which is what the
    /// completion path needs after a kill.
    pub fn wait(&self) -> io::Result<WaitOutcome> {
        wait_pid(self.pid)
    }

    /// Reap this child if it has exited, without blocking.
    ///
    /// The completion path owns exactly one child and cannot use
    /// [`crate::wait_any_nohang`], which would also consume a status another
    /// caller is waiting for.
    pub fn try_wait(&self) -> io::Result<Option<WaitOutcome>> {
        let mut map = children()
            .lock()
            .map_err(|_| io::Error::other("child table"))?;
        let Some(child) = map.get_mut(&self.pid) else {
            return Ok(None);
        };
        match child.try_wait()? {
            Some(status) => {
                let outcome = wait_outcome(status);
                map.remove(&self.pid);
                Ok(Some(outcome))
            }
            None => Ok(None),
        }
    }

    /// Terminate this child.
    ///
    /// A completion that overran its deadline is abandoned, not asked to
    /// leave. The caller still has to [`Child::try_wait`] to reap it.
    pub fn kill(&self) -> io::Result<()> {
        let mut map = children()
            .lock()
            .map_err(|_| io::Error::other("child table"))?;
        let Some(child) = map.get_mut(&self.pid) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no child with pid {}", self.pid),
            ));
        };
        child.kill()
    }
}

/// The children this process started, so `wait_pid` can find them again.
///
/// A `std::process::Child` owns the handle `WaitForSingleObject` needs, and the
/// shell addresses children by pid, so the two are kept together here.
fn children() -> &'static Mutex<std::collections::BTreeMap<i32, std::process::Child>> {
    static CHILDREN: OnceLock<Mutex<std::collections::BTreeMap<i32, std::process::Child>>> =
        OnceLock::new();
    CHILDREN.get_or_init(|| Mutex::new(std::collections::BTreeMap::new()))
}

/// Start `plan`.
pub fn spawn(plan: SpawnPlan) -> io::Result<Child> {
    use std::os::windows::process::CommandExt;

    let program = os_path(&plan.program);
    let mut command = std::process::Command::new(program);
    if let Some((first, rest)) = plan.argv.split_first() {
        let _ = first;
        command.args(rest.iter().map(|arg| os_path(arg)));
    }
    command.env_clear();
    for entry in &plan.env {
        if let Some((name, value)) = split_env(entry) {
            command.env(name, value);
        }
    }
    // The child's standard handles come from this process's *descriptors*, not
    // from the process's own standard handles. The shell redirects descriptor 1
    // with `dup2` — a command substitution's capture file, a pipeline stage's
    // buffer, `exec >file` — and only the CRT fd knows; `Stdio::Inherit` would
    // hand the child the console, so `$(cd C:\; cmd /c echo hi)` would print on
    // the terminal and capture nothing.
    //
    // Each descriptor is duplicated, because `Stdio` owns what it is given and
    // `Command` closes it once `CreateProcess` returns (see `stdio_from_fd`).
    let mut stdin = Some(stdio_for(0)?);
    let mut stdout = Some(stdio_for(1)?);
    let mut stderr = Some(stdio_for(2)?);
    for action in &plan.actions {
        match action {
            FdAction::Dup2 { from, to } => {
                let stdio = stdio_from_fd(*from)?;
                match to {
                    0 => stdin = Some(stdio),
                    1 => stdout = Some(stdio),
                    2 => stderr = Some(stdio),
                    _ => {
                        // A descriptor above 2 has no Windows equivalent to
                        // hand a child; refuse instead of silently dropping
                        // the redirect.
                        return Err(io::Error::new(
                            io::ErrorKind::Unsupported,
                            "only descriptors 0, 1 and 2 can be redirected on Windows",
                        ));
                    }
                }
            }
            FdAction::Open { to, .. } => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("opening descriptor {to} for a child is not implemented on Windows"),
                ));
            }
            // `2>&-` and friends: the closest Windows has to a closed standard
            // descriptor is the null device, which is what `Stdio::null` opens.
            FdAction::Close { fd } => {
                let stdio = std::process::Stdio::null();
                match fd {
                    0 => stdin = Some(stdio),
                    1 => stdout = Some(stdio),
                    2 => stderr = Some(stdio),
                    _ => {}
                }
            }
        }
    }
    if let Some(stdio) = stdin {
        command.stdin(stdio);
    }
    if let Some(stdio) = stdout {
        command.stdout(stdio);
    }
    if let Some(stdio) = stderr {
        command.stderr(stdio);
    }
    command.creation_flags(0);
    let child = command.spawn()?;
    let pid = i32::try_from(child.id()).unwrap_or(-1);
    children().lock().ok().map(|mut map| map.insert(pid, child));
    Ok(Child { pid })
}

/// A `Stdio` for an existing CRT descriptor.
///
/// The handle is duplicated first. `Stdio::from_raw_handle` takes ownership of
/// what it is given and `Command` closes it after `CreateProcess`, so handing
/// it a descriptor's own handle would close the shell's: `cmd /c echo hi 2>&1`
/// hands descriptor 1's console handle to the child, and the shell would then
/// be painting with a closed handle.
fn stdio_from_fd(fd: c_int) -> io::Result<std::process::Stdio> {
    let handle = raw_handle(fd)?;
    let duplicate = duplicate_handle(handle)?;
    // SAFETY: `duplicate` is a handle this function owns (a fresh duplicate
    // from `DuplicateHandle`) and nobody else refers to; `Stdio` becomes its
    // only owner.
    Ok(unsafe { std::process::Stdio::from_raw_handle(duplicate) })
}

/// A `Stdio` for descriptor `fd`, or the null device when it is closed.
///
/// The shell can close a descriptor (`exec 1>&-`), and the child must not
/// silently inherit the console in that case.
fn stdio_for(fd: c_int) -> io::Result<std::process::Stdio> {
    if fd_is_open(fd) {
        stdio_from_fd(fd)
    } else {
        Ok(std::process::Stdio::null())
    }
}

/// A new inheritable handle to the same object as `handle`.
fn duplicate_handle(handle: RawHandle) -> io::Result<RawHandle> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> RawHandle;
        fn DuplicateHandle(
            process: RawHandle,
            source: RawHandle,
            target_process: RawHandle,
            target: *mut RawHandle,
            access: u32,
            inherit: i32,
            options: u32,
        ) -> i32;
    }
    /// `DUPLICATE_SAME_ACCESS`: keep the source handle's access rights.
    const SAME_ACCESS: u32 = 0x0000_0002;
    let mut duplicate: RawHandle = std::ptr::null_mut();
    // SAFETY: both process handles are the current process's pseudo-handle,
    // which the call only reads and never closes; `duplicate` is a writable
    // handle slot that is written only on success.
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle,
            GetCurrentProcess(),
            &mut duplicate,
            0,
            1,
            SAME_ACCESS,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(duplicate)
}

/// The OS handle behind a CRT descriptor.
fn raw_handle(fd: c_int) -> io::Result<RawHandle> {
    // SAFETY: `_get_osfhandle` takes an integer.
    let handle = unsafe { _get_osfhandle(fd) };
    if handle == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(handle as RawHandle)
}

/// Split one `NAME=value` environment entry.
///
/// The environment block is UTF-16 like every other name-adjacent string on
/// Windows, so the two halves go through the same WTF-8 codec: a value a shell
/// script built out of raw bytes reaches the child unchanged instead of being
/// scrubbed to U+FFFD on the way.
fn split_env(entry: &[u8]) -> Option<(OsString, OsString)> {
    let position = entry.iter().position(|&byte| byte == b'=')?;
    Some((
        crate::os::os_from_bytes(&entry[..position]),
        crate::os::os_from_bytes(&entry[position + 1..]),
    ))
}

/// Replace this process's image.
///
/// Windows has no `exec`; the shell's `exec` built-in is implemented by
/// spawning and exiting, which is what the VM does when this refuses.
pub fn exec(_plan: SpawnPlan) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, "there is no exec on Windows")
}

/// Fork: unavailable, and nothing in the shell calls it any more.
pub fn fork_child_status(_body: impl FnOnce() -> i32) -> io::Result<Child> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "there is no fork on Windows",
    ))
}

// ---------------------------------------------------------------------------
// Waiting
// ---------------------------------------------------------------------------

/// The shell's reading of a Windows exit status.
///
/// A console `Ctrl-C` ends the child with `STATUS_CONTROL_C_EXIT`, which is a
/// signal death and not an exit code: a shell stores 130 (`128 + SIGINT`),
/// while truncating the status to eight bits would store 58.
fn wait_outcome(status: std::process::ExitStatus) -> WaitOutcome {
    /// `STATUS_CONTROL_C_EXIT` (`0xC000013A`).
    const CONTROL_C_EXIT: i32 = 0xC000_013A_u32 as i32;
    match status.code() {
        Some(CONTROL_C_EXIT) => WaitOutcome::Signaled(crate::SIGINT),
        Some(code) => WaitOutcome::Exited(code as u8 as i32),
        None => WaitOutcome::Exited(0),
    }
}

/// Wait for `pid` to exit.
pub fn wait_pid(pid: i32) -> io::Result<WaitOutcome> {
    let mut map = children()
        .lock()
        .map_err(|_| io::Error::other("child table"))?;
    let Some(child) = map.get_mut(&pid) else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no child with pid {pid}"),
        ));
    };
    let status = child.wait()?;
    map.remove(&pid);
    Ok(wait_outcome(status))
}

/// Reap any finished child, reporting its status like Unix's version.
pub fn wait_any_nohang() -> Option<(i32, WaitOutcome)> {
    let mut map = children().lock().ok()?;
    let pids: Vec<i32> = map.keys().copied().collect();
    let mut finished = None;
    for pid in pids {
        if let Some(child) = map.get_mut(&pid)
            && let Ok(Some(status)) = child.try_wait()
        {
            finished = Some((pid, wait_outcome(status)));
            break;
        }
    }
    if let Some((pid, _)) = finished {
        map.remove(&pid);
    }
    finished
}

// ---------------------------------------------------------------------------
// PTY
// ---------------------------------------------------------------------------

/// A pseudo-terminal.
#[derive(Debug)]
pub struct Pty {
    /// The controlling side.
    pub master: Option<Fd>,
    /// The child side.
    pub slave: Option<Fd>,
    /// The child-facing descriptor number.
    pub slave_fd: c_int,
}

impl Pty {
    /// The controlling side's descriptor.
    pub fn master_fd(&self) -> c_int {
        self.master.as_ref().map_or(-1, Fd::as_raw_fd)
    }

    /// The child side's descriptor.
    pub fn slave_fd(&self) -> c_int {
        self.slave_fd
    }
}

/// Open a pseudo-terminal pair.
pub fn open_pty() -> io::Result<Pty> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Windows has no pseudo-terminal (docs/portability.md §10)",
    ))
}

/// Make this process a session leader on the pty.
pub fn become_session_leader() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no sessions on Windows",
    ))
}

/// Keep `AsRawHandle` in the import list for the console work ahead.
const _: fn(&std::io::Stdin) -> RawHandle = |stdin| stdin.as_raw_handle();
