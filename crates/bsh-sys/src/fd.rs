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

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicU64, Ordering};

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
        let (read, write) = create_pipe()?;
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

#[cfg(target_os = "linux")]
fn create_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds: [libc::c_int; 2] = [0; 2];
    // SAFETY: `pipe2` writes exactly two descriptors into the array on success
    // and does not retain the pointer.
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just created and are not owned elsewhere.
    unsafe { Ok((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))) }
}

#[cfg(not(target_os = "linux"))]
fn create_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds: [libc::c_int; 2] = [0; 2];
    // SAFETY: `pipe` writes exactly two descriptors into the array.
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just created and are not owned elsewhere.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    set_cloexec(read.as_raw_fd(), true)?;
    set_cloexec(write.as_raw_fd(), true)?;
    Ok((read, write))
}

/// Open `path` with `O_CLOEXEC` added to `flags`.
///
/// `mode` is only used when `flags` contains `O_CREAT`/`O_TMPFILE`, as for
/// `open(2)`. An embedded NUL in `path` is rejected rather than truncated.
pub fn open_cloexec(path: &[u8], flags: libc::c_int, mode: libc::mode_t) -> io::Result<OwnedFd> {
    let path = cstring(path)?;
    // SAFETY: `path` is NUL-terminated and alive for the call; `open` is
    // variadic and takes `mode` only for the creating flags.
    let fd = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC, mode) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor is newly opened and unowned.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Duplicate `from` onto `to`, clearing `FD_CLOEXEC` on the result, exactly as
/// `dup2(2)` does.
///
/// `to` is closed first if it was open. This is the primitive that applies a
/// shell redirection; the destination is the caller's responsibility.
pub fn dup2(from: RawFd, to: RawFd) -> io::Result<()> {
    // SAFETY: `dup2` only reads descriptor numbers.
    if unsafe { libc::dup2(from, to) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Close a descriptor the caller owns.
///
/// On Linux `close` always releases the descriptor, including when it reports
/// `EINTR`; callers must not retry it or use `fd` afterwards. Use
/// [`OwnedFd`] wherever ownership can be expressed in the type instead.
pub fn close(fd: RawFd) -> io::Result<()> {
    // SAFETY: `close` only reads the descriptor number. Ownership was
    // transferred by the caller, so the descriptor is not used afterwards.
    if unsafe { libc::close(fd) } != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Whether `fd` currently names an open descriptor.
///
/// The answer is a snapshot: another thread may close or reuse the descriptor
/// immediately afterwards.
pub fn fd_is_open(fd: RawFd) -> bool {
    // SAFETY: `fcntl(F_GETFD)` only inspects the descriptor table entry.
    unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
}

/// Whether `fd` is open and has `FD_CLOEXEC` set.
pub fn fd_is_cloexec(fd: RawFd) -> bool {
    // SAFETY: `fcntl(F_GETFD)` only inspects the descriptor table entry. A
    // negative result is `-1` (EBADF), which must not be mistaken for a flag.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    flags >= 0 && flags & libc::FD_CLOEXEC != 0
}

/// Set or clear `FD_CLOEXEC` on an open descriptor.
pub fn set_cloexec(fd: RawFd, enabled: bool) -> io::Result<()> {
    // SAFETY: `fcntl(F_GETFD)` only inspects the descriptor table entry.
    let current = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if current < 0 {
        return Err(io::Error::last_os_error());
    }
    let flags = if enabled {
        current | libc::FD_CLOEXEC
    } else {
        current & !libc::FD_CLOEXEC
    };
    // SAFETY: `fcntl(F_SETFD)` only updates this descriptor's flags.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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
    // SAFETY: the name is a valid NUL-terminated string and `MFD_CLOEXEC` is a
    // valid flag; the call returns a fresh descriptor or -1.
    let fd = unsafe { libc::memfd_create(c"bsh-heredoc".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor is newly created and unowned.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn tmpfile_in(dir: &[u8]) -> io::Result<OwnedFd> {
    let dir = cstring(dir)?;
    // SAFETY: `dir` is a NUL-terminated directory path; `O_TMPFILE` makes the
    // opened file nameless, and `open` does not retain the pointer.
    let fd = unsafe {
        libc::open(
            dir.as_ptr(),
            libc::O_TMPFILE | libc::O_RDWR | libc::O_CLOEXEC,
            0o600 as libc::mode_t,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor is newly opened and unowned.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn unlinked_named_temp(dir: &[u8]) -> io::Result<OwnedFd> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let pid = std::process::id();
    for _ in 0..100 {
        let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = Vec::with_capacity(dir.len() + 32);
        path.extend_from_slice(dir);
        path.push(b'/');
        path.extend_from_slice(format!("bsh-heredoc-{pid}-{serial}").as_bytes());
        let name = cstring(&path)?;

        // SAFETY: `name` is a NUL-terminated path; `O_EXCL` makes creation
        // atomic so a stale name can never be reused, and `open` does not
        // retain the pointer.
        let fd = unsafe {
            libc::open(
                name.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR | libc::O_CLOEXEC,
                0o600 as libc::mode_t,
            )
        };
        if fd < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EEXIST) {
                continue;
            }
            return Err(error);
        }
        // SAFETY: the descriptor is newly opened and unowned from here on.
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };

        // SAFETY: `name` is still NUL-terminated and alive; the open
        // descriptor keeps the data readable after the name disappears.
        if unsafe { libc::unlink(name.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(owned);
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
