//! The terminal boundary the editor is written against.
//!
//! [`Terminal`] is deliberately tiny: read one byte, write bytes, ask the size,
//! enter and leave raw mode. Everything that makes the editor interesting —
//! key decoding, insertion, wrapping, the completion menu — sits above it and
//! is tested by driving a fake implementation.
//!
//! There is no pty helper in `bsh-sys` yet, so [`RealTerminal`] is the only
//! implementation that touches a real terminal and it is not unit-tested. A
//! pty harness added later implements the same trait and changes nothing above
//! this module.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};

/// Terminal dimensions, in character cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    /// Visible columns.
    pub columns: u16,
    /// Visible rows.
    pub rows: u16,
}

impl Size {
    /// What to assume when the terminal will not say: 80×24.
    pub const FALLBACK: Size = Size {
        columns: 80,
        rows: 24,
    };

    /// Columns, treating a zero as "the terminal did not say".
    ///
    /// A pty whose size has not been set reports zero in both dimensions, and
    /// a zero-width canvas has no meaningful wrapping.
    pub fn columns_or_fallback(self) -> u16 {
        if self.columns == 0 {
            Self::FALLBACK.columns
        } else {
            self.columns
        }
    }

    /// Rows, treating a zero as "the terminal did not say".
    pub fn rows_or_fallback(self) -> u16 {
        if self.rows == 0 {
            Self::FALLBACK.rows
        } else {
            self.rows
        }
    }
}

impl Default for Size {
    fn default() -> Self {
        Self::FALLBACK
    }
}

/// The byte and terminal-state interface the editor owns.
///
/// Reads are one byte at a time because that is what the key decoder needs and
/// because a buffered line read would defeat raw mode's `VMIN = 1`. A fake used
/// in tests returns `Ok(None)` at end of script, which is how the decoder sees
/// a truncated escape sequence.
pub trait Terminal {
    /// Read one byte, blocking until one is available.
    ///
    /// `Ok(None)` means end of input, not an error.
    fn read_byte(&mut self) -> io::Result<Option<u8>>;

    /// Write all of `bytes`. The editor builds one frame and calls this once.
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()>;

    /// The current size. Implementations may query the terminal on every call.
    fn size(&self) -> Size;

    /// Put the terminal into raw mode.
    ///
    /// Implementations must be idempotent and must restore the previous state
    /// on drop, so a panic while editing cannot leave the user's terminal
    /// unusable.
    fn enter_raw(&mut self) -> io::Result<()>;

    /// Restore the terminal mode saved by [`Terminal::enter_raw`].
    fn leave_raw(&mut self) -> io::Result<()>;
}

/// The real terminal: `bsh-sys` for termios and the window size, stdio or the
/// controlling terminal for bytes.
///
/// Raw mode is entered and left once per [`crate::Editor::read_line`], not for
/// the life of the shell: a command started from the prompt must inherit a
/// cooked terminal, or `read`, `cat` and job control stop working.
///
/// Editing prefers `/dev/tty` over standard output. A user who runs
/// `bsh > transcript` still wants the prompt, the highlighted line and the
/// completion menu on their terminal — not interleaved with the command output
/// they are capturing. Standard output remains the command output's business.
#[derive(Debug, Default)]
pub struct RealTerminal {
    /// The controlling terminal, when one could be opened.
    tty: Option<File>,
    /// The descriptor raw mode and `TIOCGWINSZ` apply to.
    fd: RawFd,
    raw: Option<bsh_sys::RawMode>,
}

impl RealTerminal {
    /// A terminal backed by `/dev/tty`, falling back to standard input and
    /// output when there is no controlling terminal.
    pub fn new() -> Self {
        match File::options().read(true).write(true).open("/dev/tty") {
            Ok(tty) => Self {
                fd: tty.as_raw_fd(),
                tty: Some(tty),
                raw: None,
            },
            Err(_) => Self {
                tty: None,
                fd: 0,
                raw: None,
            },
        }
    }
}

impl Terminal for RealTerminal {
    fn read_byte(&mut self) -> io::Result<Option<u8>> {
        let mut byte = [0u8; 1];
        loop {
            let read = match self.tty.as_mut() {
                Some(tty) => tty.read(&mut byte),
                None => io::stdin().lock().read(&mut byte),
            };
            match read {
                Ok(0) => return Ok(None),
                Ok(_) => return Ok(Some(byte[0])),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self.tty.as_mut() {
            Some(tty) => tty.write_all(bytes).and_then(|()| tty.flush()),
            None => {
                let mut stdout = io::stdout().lock();
                stdout.write_all(bytes)?;
                stdout.flush()
            }
        }
    }

    fn size(&self) -> Size {
        bsh_sys::window_size_fd(self.fd)
            .map(|(columns, rows)| Size { columns, rows })
            .unwrap_or(Size::FALLBACK)
    }

    fn enter_raw(&mut self) -> io::Result<()> {
        if self.raw.is_none() {
            self.raw = Some(bsh_sys::RawMode::enable(self.fd)?);
        }
        Ok(())
    }

    fn leave_raw(&mut self) -> io::Result<()> {
        match self.raw.take() {
            Some(raw) => raw.restore(),
            None => Ok(()),
        }
    }
}

impl Drop for RealTerminal {
    fn drop(&mut self) {
        // `RawMode`'s own `Drop` is the panic-safe half; taking it here just
        // makes the restore explicit and idempotent.
        let _ = self.leave_raw();
    }
}
