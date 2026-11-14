//! The controlling terminal, for a program that owns the whole screen.
//!
//! This is not the line editor's terminal ([`crate::RawMode`] plus whoever is
//! reading the descriptor): it is one object that opens the terminal, holds it
//! in raw mode, and reads and writes bytes with a timeout. `baashzf` needs
//! exactly that, and needs it on both platforms, so the difference between
//! `/dev/tty` + termios and a pair of console handles lives here rather than in
//! a builtin.
//!
//! Two things make this a different job from reading standard input:
//!
//! * the *screen* is the terminal, never the command's output. `ls | baashzf`
//!   must paint on the terminal while its stdout is a pipe, which is why Unix
//!   opens `/dev/tty` and Windows opens `CONIN$`/`CONOUT$` first;
//! * a read has a **deadline**, because a screen-owning program has to keep
//!   painting (a resize, a clock) even when no key arrives, and because a
//!   lone `Esc` is only an `Esc` once nothing follows it.
//!
//! Raw mode is held for the lifetime of the [`Tty`] and restored on drop, so a
//! panic cannot leave a terminal that no longer echoes.

use std::io;
use std::time::Duration;

/// The controlling terminal, in raw mode.
#[cfg(unix)]
pub struct Tty {
    /// The `/dev/tty` handle. Opening it is how this object exists, so there is
    /// no "no terminal" case left to carry.
    file: std::fs::File,
    /// The descriptor reads and writes go to.
    fd: crate::RawFd,
    /// The saved terminal state, put back when this is dropped.
    raw: Option<crate::RawMode>,
}

#[cfg(unix)]
impl Tty {
    /// Open the controlling terminal and put it in raw mode.
    ///
    /// `Err` when the process has no controlling terminal (a script, a systemd
    /// unit, `baash -c` with no tty): a program that paints the screen has
    /// nowhere to paint, and guessing at standard input would paint into
    /// whatever it was redirected to.
    pub fn open() -> io::Result<Self> {
        let file = std::fs::File::options()
            .read(true)
            .write(true)
            .open("/dev/tty")?;
        let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
        let raw = crate::RawMode::enable(fd)?;
        Ok(Self {
            file,
            fd,
            raw: Some(raw),
        })
    }

    /// The terminal's size in cells, when it reports one.
    pub fn size(&self) -> Option<(u16, u16)> {
        crate::window_size_fd(self.fd)
    }

    /// Write bytes to the terminal.
    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write as _;
        self.file.write_all(bytes)?;
        self.file.flush()
    }

    /// Read what is available, waiting at most `timeout`.
    ///
    /// `Ok(None)` means nothing arrived (the deadline passed, no bytes were
    /// consumed); `Ok(Some(0))` means the terminal is gone; `Ok(Some(n))`
    /// appends `n` bytes to `out`.
    pub fn read(&mut self, timeout: Duration, out: &mut Vec<u8>) -> io::Result<Option<usize>> {
        let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        if !crate::poll_readable(self.fd, millis)? {
            return Ok(None);
        }
        let mut buffer = [0u8; 256];
        let read = crate::read_fd(self.fd, &mut buffer)?;
        out.extend_from_slice(&buffer[..read]);
        Ok(Some(read))
    }
}

#[cfg(unix)]
impl Drop for Tty {
    fn drop(&mut self) {
        // Restoring the mode matters more than anything else this object does:
        // a terminal left in raw mode no longer echoes or runs signals, and
        // the user is left with a shell that looks broken.
        if let Some(raw) = self.raw.take() {
            let _ = raw.restore();
        }
    }
}

/// The controlling terminal, in raw mode (Windows).
///
/// The same object as the Unix one over a console instead of `/dev/tty`:
/// [`crate::Console`] already opens `CONIN$`/`CONOUT$`, reports its size,
/// converts UTF-8 both ways and switches to `ENABLE_VIRTUAL_TERMINAL_INPUT`,
/// which is what makes a console send the same escape sequences a Unix
/// terminal does — so the picker's frames and its key decoding are identical
/// on both platforms.
#[cfg(windows)]
pub struct Tty {
    /// The console this object was opened on.
    console: crate::Console,
    /// The console modes to put back when this is dropped.
    raw: Option<crate::ConsoleMode>,
}

#[cfg(windows)]
impl Tty {
    /// Open the console and put it in raw mode.
    pub fn open() -> io::Result<Self> {
        let console = crate::Console::open()?;
        let raw = console.enter_raw()?;
        Ok(Self {
            console,
            raw: Some(raw),
        })
    }

    /// The console's window size in cells.
    pub fn size(&self) -> Option<(u16, u16)> {
        self.console.size().ok()
    }

    /// Write bytes to the console.
    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.console.write(bytes)
    }

    /// Read what is available, waiting at most `timeout`.
    ///
    /// A console handle can be waited on but not for a byte count, so the wait
    /// and the read are two calls — see [`crate::Console::wait_input`].
    pub fn read(&mut self, timeout: Duration, out: &mut Vec<u8>) -> io::Result<Option<usize>> {
        if !self.console.wait_input(timeout)? {
            return Ok(None);
        }
        let before = out.len();
        let read = self.console.read(out)?;
        // A console that reports input and then reads nothing is gone.
        if read == 0 && out.len() == before {
            return Ok(Some(0));
        }
        Ok(Some(out.len() - before))
    }
}

#[cfg(windows)]
impl Drop for Tty {
    fn drop(&mut self) {
        if let Some(raw) = self.raw.take() {
            let _ = raw.restore();
        }
    }
}
