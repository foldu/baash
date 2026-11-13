//! File descriptors: pipes, close-on-exec opens, and unlinked temporary files.
//!
//! # Why here-doc bodies are files, not pipes
//!
//! A pipe has a bounded capacity (64 KiB by default on Linux) and blocks the
//! writer once it is full. A here-doc body is expanded in full *before* the
//! command runs, while the command may exit without ever reading its stdin, or
//! read only a prefix. Writing such a body into a pipe therefore deadlocks the
//! shell: nobody will ever drain the rest. A file has no producer/consumer
//! coupling — write the whole body, seek back to the start, and hand the
//! descriptor to the command (DESIGN.md §4.4).
//!
//! [`create_unlinked_temp`] additionally makes the body invisible in the
//! filesystem: `memfd_create(2)` and `O_TMPFILE` never create a name at all,
//! and the named fallback unlinks immediately after opening.
//!
//! # Why `rustix` rather than `libc`
//!
//! These are leaf syscalls, and `rustix` already wraps every one of them with
//! the right `io-safety` shape (`OwnedFd` in, `OwnedFd` out), the right
//! `Errno` type and, unlike these declarations, the right layout on every
//! architecture it supports. Hand-written `libc` bindings buy nothing here.
//!
//! The two calls that stay raw are the ones `rustix` deliberately does not let
//! a safe caller express: [`dup2`] installs a descriptor at a *number* the
//! caller chose (which is exactly what a shell redirection is, and which
//! `rustix::io::dup2` refuses because the number is not a slot it can own), and
//! [`close`] is `unsafe` in `rustix` for the same reason it is here.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicU64, Ordering};

use rustix::fs::{AtFlags, Mode, OFlags, SeekFrom};
use rustix::io::FdFlags;

use crate::bytes::cstring;

/// A read/write pipe pair, both ends close-on-exec.
///
/// The shell's plumbing must not leak into children (DESIGN.md §4.3): an
/// exec'd program that inherits a pipe writer keeps `wait` hanging forever.
#[derive(Debug)]
pub struct Pipe {
    /// The reading end.
    pub read: OwnedFd,
    /// The writing end.
    pub write: OwnedFd,
}

impl Pipe {
    /// Create a pipe with `O_CLOEXEC` on both ends.
    pub fn new() -> io::Result<Self> {
        let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
        Ok(Self { read, write })
    }

    /// The raw reading end.
    pub fn read_fd(&self) -> RawFd {
        self.read.as_raw_fd()
    }

    /// The raw writing end.
    pub fn write_fd(&self) -> RawFd {
        self.write.as_raw_fd()
    }
}

/// Borrow a descriptor number for the duration of one call.
///
/// # Why this is one `unsafe` block instead of thirty
///
/// The shell stores descriptors as numbers: `exec 3<&1`, `>&2` and a here-doc's
/// saved stdin are all "the descriptor with this number", and `dup2` needs a
/// number to install onto. `rustix` takes `BorrowedFd`, which carries a
/// lifetime because closing a descriptor while another owner still holds a
/// `BorrowedFd` breaks `io-safety`.
///
/// The borrow here never outlives the call it was made for: every use is
/// `rustix::…(borrowed(fd), …)` on the shell thread, which owns the descriptor
/// table. Misuse would address the wrong file — a bug to fix, not undefined
/// behaviour — because a descriptor is a number, not a pointer.
pub(crate) fn borrowed(fd: RawFd) -> BorrowedFd<'static> {
    // SAFETY: the number names a descriptor the caller owns; the borrow is
    // consumed by the immediately following call, and every caller is shell
    // code on the thread that owns the descriptor table.
    unsafe { BorrowedFd::borrow_raw(fd) }
}

/// Convert `libc` open flags into `rustix` flags.
///
/// The public API speaks `libc` constants because that is what the shell's
/// redirection data structures store; the syscall layer speaks `rustix`. A flag
/// this build does not know is rejected rather than dropped.
fn open_flags(flags: libc::c_int) -> io::Result<OFlags> {
    // `libc` `O_*` values are non-negative `c_int` bit patterns.
    match OFlags::from_bits(flags as u32) {
        Some(flags) => Ok(flags),
        None => Err(io::Error::from_raw_os_error(libc::EINVAL)),
    }
}

/// Open `path` with `O_CLOEXEC` added to `flags`.
///
/// `mode` is only used when `flags` contains `O_CREAT`/`O_TMPFILE`, as for
/// `open(2)`. An embedded NUL in `path` is rejected rather than truncated.
/// Bytes the kernel has queued for `fd`, or zero if it will not say.
///
/// The line editor asks this to decide whether a readiness report is backed by
/// data: a character device may answer a poll without input queued, and acting
/// on that would leave the editor waiting for a keystroke that never comes.
pub fn bytes_available(fd: RawFd) -> usize {
    // SAFETY: `fd` is an open descriptor the caller owns; the borrow does not
    // outlive this call and `ioctl_fionread` only reads the queue length.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    usize::try_from(rustix::io::ioctl_fionread(borrowed).unwrap_or(0)).unwrap_or(0)
}

/// `open` with `O_CLOEXEC` set.
pub fn open_cloexec(path: &[u8], flags: libc::c_int, mode: libc::mode_t) -> io::Result<OwnedFd> {
    let path = cstring(path)?;
    let flags = open_flags(flags)? | OFlags::CLOEXEC;
    let mode = Mode::from_bits_truncate(mode);
    Ok(rustix::fs::open(&path, flags, mode)?)
}

/// Duplicate `from` onto `to`, clearing `FD_CLOEXEC` on the result, exactly as
/// `dup2(2)` does.
///
/// `to` is closed first if it was open. This is the primitive that applies a
/// shell redirection; the destination is the caller's responsibility.
///
/// `rustix::io::dup2` cannot express this: it installs into an `&mut OwnedFd`
/// (refusing to clobber a descriptor it does not own), whereas a redirection
/// names a destination number and *must* clobber it.
pub fn dup2(from: RawFd, to: RawFd) -> io::Result<()> {
    // SAFETY: `dup2` only reads descriptor numbers, closes the destination and
    // installs a duplicate. `to` is the caller's to clobber by contract.
    if unsafe { libc::dup2(from, to) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Duplicate `fd` onto the lowest-numbered free descriptor, setting
/// `FD_CLOEXEC` on the result.
///
/// The original descriptor is untouched and the duplicate is owned by the
/// caller. `FD_CLOEXEC` is set by the kernel as part of the duplicate
/// (`F_DUPFD_CLOEXEC`), so there is no window in which a concurrent `exec`
/// could inherit the descriptor.
///
/// This is the primitive the shell needs to *save* a descriptor before
/// redirecting it: [`dup2`] destroys the saved descriptor's identity, whereas
/// a duplicate can be restored later.
pub fn dup(fd: RawFd) -> io::Result<OwnedFd> {
    dup_above(fd, 0)
}

/// Duplicate `fd` onto the lowest-numbered free descriptor at or above `min`,
/// setting `FD_CLOEXEC` on the result.
///
/// The shell keeps its own plumbing at descriptors ≥ 10 and historically saved
/// user descriptors by `dup2`-ing them to ≥ 20; this is the exact primitive
/// that replaces the dance, because it never closes the caller's descriptor
/// and the duplicate's number cannot collide with the saved one.
///
/// `min` must be non-negative; a negative value is rejected by the kernel with
/// `EINVAL` rather than being clamped.
pub fn dup_above(fd: RawFd, min: RawFd) -> io::Result<OwnedFd> {
    Ok(rustix::io::fcntl_dupfd_cloexec(borrowed(fd), min)?)
}

/// Change the working directory to `path`, which is shell bytes.
///
/// Shell filenames are arbitrary non-NUL byte sequences, so this takes `&[u8]`
/// rather than `&str`. An embedded NUL is rejected with
/// [`NulByte`](crate::NulByte) instead of silently truncating the path (which
/// would `chdir` somewhere unintended).
///
/// The change is process-wide, so a caller that is not the only thread in the
/// process must serialise it with every other user of the working directory.
pub fn chdir(path: &[u8]) -> io::Result<()> {
    let path = cstring(path)?;
    Ok(rustix::process::chdir(&path)?)
}

/// Close a descriptor the caller owns.
///
/// On Linux `close` always releases the descriptor, including when it reports
/// `EINTR`; callers must not retry it or use `fd` afterwards. Use
/// [`OwnedFd`] wherever ownership can be expressed in the type instead.
pub fn close(fd: RawFd) -> io::Result<()> {
    // SAFETY: ownership was transferred by the caller, who promises not to use
    // the number again; `close` touches nothing else.
    if unsafe { libc::close(fd) } != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Run `op`, retrying while it reports `EINTR`.
fn retrying<T>(mut op: impl FnMut() -> Result<T, rustix::io::Errno>) -> io::Result<T> {
    loop {
        match op() {
            Ok(value) => return Ok(value),
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

/// Read up to `buf.len()` bytes from `fd`.
///
/// Returns the number of bytes read; `0` means end of file. `EINTR` is
/// retried. This is the byte source for the `read` built-in, which must not
/// over-read: it reads exactly one buffer at a time and keeps any surplus
/// itself, so a second reader on the same descriptor still sees the rest.
pub fn read_fd(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    retrying(|| rustix::io::read(borrowed(fd), &mut *buf))
}

/// Read `fd` to end of file.
pub fn read_all(fd: RawFd) -> io::Result<Vec<u8>> {
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

/// Write `buf` to `fd` in one call, returning the number of bytes accepted.
///
/// Short writes are the caller's problem; use [`write_all`] unless you are
/// testing partial-write behaviour.
pub fn write_fd(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    retrying(|| rustix::io::write(borrowed(fd), buf))
}

/// Write all of `buf` to `fd`, retrying short writes and `EINTR`.
///
/// Returns the number of bytes written (always `buf.len()` on success).
pub fn write_all(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    let mut written = 0;
    while written < buf.len() {
        match write_fd(fd, &buf[written..])? {
            0 => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "write returned zero bytes",
                ));
            }
            n => written += n,
        }
    }
    Ok(written)
}

/// Wait until `fd` is readable, or `timeout_ms` elapses.
///
/// Returns `true` when a read will not block, `false` on timeout. A negative
/// timeout waits indefinitely. This is what `read -t` needs; it must not be
/// used to poll anything but readability.
pub fn poll_readable(fd: RawFd, timeout_ms: i32) -> io::Result<bool> {
    let source = borrowed(fd);
    let mut pollfd = [rustix::event::PollFd::new(
        &source,
        rustix::event::PollFlags::IN,
    )];
    // `poll(2)` takes a `timespec` on Linux and a millisecond count elsewhere;
    // `rustix` accepts the portable form and refuses a timeout it cannot
    // represent.
    let timeout = (timeout_ms >= 0).then(|| rustix::event::Timespec {
        tv_sec: i64::from(timeout_ms / 1000),
        // `tv_nsec` is `i64` or `c_long` depending on the chosen backend.
        tv_nsec: (i64::from(timeout_ms % 1000) * 1_000_000) as _,
    });
    loop {
        match rustix::event::poll(&mut pollfd, timeout.as_ref()) {
            Ok(0) => return Ok(false),
            Ok(_) => return Ok(true),
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

/// `whence` values for [`lseek`].
pub mod seek {
    /// From the start of the file.
    pub const SET: i32 = libc::SEEK_SET;
    /// From the current offset.
    pub const CUR: i32 = libc::SEEK_CUR;
    /// From the end of the file.
    pub const END: i32 = libc::SEEK_END;
}

/// Reposition the file offset of `fd`, returning the new offset.
///
/// The `read` built-in needs this to give back the bytes it read but did not
/// consume: a chunked read from a seekable descriptor is rewound by the
/// surplus, so a later command sees exactly the input the shell did not take
/// (DESIGN.md §4.4). On a pipe the call fails with `ESPIPE`, which is also how
/// a caller discovers that the descriptor is not seekable.
pub fn lseek(fd: RawFd, offset: i64, whence: i32) -> io::Result<i64> {
    let position = match whence {
        seek::SET => SeekFrom::Start(offset as u64),
        seek::CUR => SeekFrom::Current(offset),
        seek::END => SeekFrom::End(offset),
        _ => return Err(io::Error::from_raw_os_error(libc::EINVAL)),
    };
    let offset = retrying(|| rustix::fs::seek(borrowed(fd), position))?;
    Ok(offset as i64)
}

/// Whether `fd` currently names an open descriptor.
///
/// The answer is a snapshot: another thread may close or reuse the descriptor
/// immediately afterwards.
pub fn fd_is_open(fd: RawFd) -> bool {
    rustix::io::fcntl_getfd(borrowed(fd)).is_ok()
}

/// Whether `fd` is open and has `FD_CLOEXEC` set.
pub fn fd_is_cloexec(fd: RawFd) -> bool {
    rustix::io::fcntl_getfd(borrowed(fd))
        .map(|flags| flags.contains(FdFlags::CLOEXEC))
        .unwrap_or(false)
}

/// Set or clear `FD_CLOEXEC` on an open descriptor.
pub fn set_cloexec(fd: RawFd, enabled: bool) -> io::Result<()> {
    let mut flags = rustix::io::fcntl_getfd(borrowed(fd))?;
    flags.set(FdFlags::CLOEXEC, enabled);
    Ok(rustix::io::fcntl_setfd(borrowed(fd), flags)?)
}

/// The path that names an open descriptor, for process substitution.
///
/// `<(…)` hands a command a *path*, not a descriptor, so the shell has to spell
/// the path the consumer will open. `/dev/fd` is the portable one — Linux makes
/// it a symlink to `/proc/self/fd`, and the fallback is that directory itself.
/// A kernel with neither cannot name an open file at all, and the caller
/// reports that rather than handing back a path that does not resolve.
pub fn fd_path(fd: RawFd) -> Option<Vec<u8>> {
    let dir = ["/dev/fd", "/proc/self/fd"]
        .into_iter()
        .find(|dir| std::path::Path::new(dir).exists())?;
    Some(format!("{dir}/{fd}").into_bytes())
}

/// Create a writable, close-on-exec file with no directory entry.
///
/// Used for here-doc bodies: the body is written in full, the descriptor is
/// rewound and then inherited by the command, so the command can read it at
/// its own pace without the shell ever blocking (see the module docs).
///
/// The implementation prefers `memfd_create(2)`, then `O_TMPFILE`, and finally
/// a named file in `$TMPDIR` that is unlinked immediately after opening. All
/// three leave the data readable through the returned descriptor only.
///
/// `size_hint` is advisory. It is deliberately not turned into
/// `ftruncate`/`fallocate` size reservation: that would extend the *logical*
/// size, and a reader consuming the whole file would see trailing NULs after
/// the body.
pub fn create_unlinked_temp(size_hint: usize) -> io::Result<OwnedFd> {
    // See the doc comment: reserving space must not change the logical size.
    let _ = size_hint;

    #[cfg(target_os = "linux")]
    {
        if let Ok(fd) = memfd() {
            return Ok(fd);
        }
        if let Ok(fd) = tmpfile_in(&temp_dir()) {
            return Ok(fd);
        }
    }

    unlinked_named_temp(&temp_dir())
}

/// The directory used for the named fallback, as bytes.
fn temp_dir() -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;

    let dir = std::env::temp_dir();
    let bytes = dir.as_os_str().as_bytes();
    if bytes.is_empty() {
        b"/tmp".to_vec()
    } else {
        bytes.to_vec()
    }
}

#[cfg(target_os = "linux")]
fn memfd() -> io::Result<OwnedFd> {
    Ok(rustix::fs::memfd_create(
        "baash-heredoc",
        rustix::fs::MemfdFlags::CLOEXEC,
    )?)
}

#[cfg(target_os = "linux")]
fn tmpfile_in(dir: &[u8]) -> io::Result<OwnedFd> {
    let dir = cstring(dir)?;
    let flags = OFlags::TMPFILE | OFlags::RDWR | OFlags::CLOEXEC;
    Ok(rustix::fs::open(
        &dir,
        flags,
        Mode::from_bits_truncate(0o600),
    )?)
}

fn unlinked_named_temp(dir: &[u8]) -> io::Result<OwnedFd> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let pid = std::process::id();
    let flags = OFlags::CREATE | OFlags::EXCL | OFlags::RDWR | OFlags::CLOEXEC;
    for _ in 0..100 {
        let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = Vec::with_capacity(dir.len() + 32);
        path.extend_from_slice(dir);
        path.push(b'/');
        path.extend_from_slice(format!("baash-heredoc-{pid}-{serial}").as_bytes());
        let name = cstring(&path)?;

        let fd = match rustix::fs::open(&name, flags, Mode::from_bits_truncate(0o600)) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::EXIST) => continue,
            Err(error) => return Err(error.into()),
        };
        // The open descriptor keeps the data readable after the name goes.
        rustix::fs::unlinkat(rustix::fs::CWD, &name, AtFlags::empty())?;
        return Ok(fd);
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique here-doc temporary file",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};

    #[test]
    fn pipe_ends_are_cloexec_and_connected() {
        let pipe = Pipe::new().unwrap();
        assert!(fd_is_cloexec(pipe.read_fd()));
        assert!(fd_is_cloexec(pipe.write_fd()));

        let mut write = std::fs::File::from(pipe.write);
        write.write_all(b"ping").unwrap();
        drop(write);

        let mut read = std::fs::File::from(pipe.read);
        let mut buffer = [0u8; 4];
        read.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, b"ping");
    }

    #[test]
    fn dup2_close_and_fd_is_open_agree() {
        let pipe = Pipe::new().unwrap();
        // A high, almost certainly unused descriptor, so parallel tests cannot
        // accidentally reuse it between these calls.
        let target = 900;
        assert!(!fd_is_open(target));
        dup2(pipe.read_fd(), target).unwrap();
        assert!(fd_is_open(target));
        close(target).unwrap();
        assert!(!fd_is_open(target));
        assert!(!fd_is_open(i32::MAX));
        assert!(!fd_is_cloexec(i32::MAX));
    }

    #[test]
    fn dup_is_cloexec_and_shares_the_open_file_description() {
        let pipe = Pipe::new().unwrap();
        let duplicate = dup(pipe.read_fd()).unwrap();
        assert_ne!(duplicate.as_raw_fd(), pipe.read_fd());
        assert!(fd_is_cloexec(duplicate.as_raw_fd()));

        // The duplicate is a second name for the same pipe, so a byte written
        // on the write end is readable through it.
        let mut write = std::fs::File::from(pipe.write);
        write.write_all(b"x").unwrap();
        let mut read = std::fs::File::from(duplicate);
        let mut byte = [0u8; 1];
        read.read_exact(&mut byte).unwrap();
        assert_eq!(&byte, b"x");
    }

    #[test]
    fn dup_above_honours_the_floor_and_leaves_the_source_open() {
        let pipe = Pipe::new().unwrap();
        // Anything from 600 upwards is far above the descriptors this test
        // binary normally holds, so the duplicate lands at the floor without
        // racing the fixed 900 used by the `dup2` test.
        let duplicate = dup_above(pipe.read_fd(), 600).unwrap();
        assert!(duplicate.as_raw_fd() >= 600);
        assert!(fd_is_cloexec(duplicate.as_raw_fd()));
        assert!(fd_is_open(pipe.read_fd()));
    }

    #[test]
    fn dup_rejects_a_descriptor_that_is_not_open() {
        let error = dup(i32::MAX).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
        let error = dup_above(i32::MAX, 600).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
    }

    #[test]
    fn unlinked_temp_is_readable_after_writing() {
        let fd = create_unlinked_temp(4096).unwrap();
        let mut file = std::fs::File::from(fd);
        file.write_all(b"heredoc body").unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut body = Vec::new();
        file.read_to_end(&mut body).unwrap();
        assert_eq!(body, b"heredoc body");
    }

    #[test]
    fn embedded_nul_path_is_rejected() {
        let error = open_cloexec(b"/tmp/a\0b", libc::O_RDONLY, 0).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.get_ref().is_some_and(|e| e.is::<crate::NulByte>()));
    }
}
