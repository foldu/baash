//! The terminal boundary the editor is written against.
//!
//! [`Terminal`] is deliberately tiny: read one byte, write bytes, ask the size,
//! enter and leave raw mode. Everything that makes the editor interesting —
//! key decoding, insertion, wrapping, the completion menu — sits above it and
//! is tested by driving a fake implementation.
//!
//! There is no pty helper in `baash-sys` yet, so [`RealTerminal`] is the only
//! implementation that touches a real terminal and it is not unit-tested. A
//! pty harness added later implements the same trait and changes nothing above
//! this module.

#[cfg(unix)]
use std::fs::File;
use std::io;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
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

/// What [`Terminal::await_input`] was woken by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    /// A byte is available; `read_byte` will not block.
    Key,
    /// The watched path changed, or the terminal's backstop says it is time to
    /// ask. No input was consumed, so the editor can repaint and wait again.
    Watch,
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

    /// Watch `fd` alongside the keyboard, taking ownership of it.
    ///
    /// `None` stops watching. The default ignores it, which is what a terminal
    /// with no waitable directory does: the editor then never reports
    /// [`Input::Watch`] and the prompt only changes between lines.
    #[cfg(unix)]
    fn set_watch(&mut self, _fd: Option<std::os::fd::OwnedFd>) {}

    /// Wait until there is a keystroke or a watched change.
    ///
    /// [`Input::Key`] promises the next `read_byte` will not block; it consumes
    /// nothing. The default blocks on the keyboard, so a fake is unaffected by
    /// this method existing.
    fn await_input(&mut self) -> io::Result<Input> {
        Ok(Input::Key)
    }
}

/// The real terminal: `baash-sys` for termios and the window size, stdio or the
/// controlling terminal for bytes.
///
/// Raw mode is entered and left once per [`crate::Editor::read_line`], not for
/// the life of the shell: a command started from the prompt must inherit a
/// cooked terminal, or `read`, `cat` and job control stop working.
///
/// Editing prefers `/dev/tty` over standard output. A user who runs
/// `baash > transcript` still wants the prompt, the highlighted line and the
/// completion menu on their terminal — not interleaved with the command output
/// they are capturing. Standard output remains the command output's business.
#[cfg(unix)]
#[derive(Debug, Default)]
pub struct RealTerminal {
    /// The controlling terminal, when one could be opened.
    tty: Option<File>,
    /// The descriptor raw mode and `TIOCGWINSZ` apply to.
    fd: RawFd,
    raw: Option<baash_sys::RawMode>,
    /// The runtime handle used to read, opened lazily on `/dev/tty`.
    ///
    /// Writes stay on the synchronous handle: a frame is a few kilobytes to a
    /// terminal, and keeping them ordered with `flush` is simpler than a second
    /// async writer.
    read_source: Option<compio::runtime::fd::PollFd<std::os::fd::OwnedFd>>,
    /// The directory watch the prompt refreshes from, when one is armed
    /// (`docs/prompt-watch.md`).
    watch: Option<compio::runtime::fd::PollFd<std::os::fd::OwnedFd>>,
}

#[cfg(unix)]
impl RealTerminal {
    /// A terminal backed by `/dev/tty`, falling back to standard input and
    /// output when there is no controlling terminal.
    pub fn new() -> Self {
        match File::options().read(true).write(true).open("/dev/tty") {
            Ok(tty) => Self {
                fd: tty.as_raw_fd(),
                tty: Some(tty),
                raw: None,
                read_source: None,
                watch: None,
            },
            Err(_) => Self {
                tty: None,
                fd: 0,
                raw: None,
                read_source: None,
                watch: None,
            },
        }
    }
}

/// On Windows the editor has no console host yet, so every operation reports
/// `Unsupported`; the type exists so `baash` builds and `-c` runs. ConPTY is the
/// last W4 step (`docs/portability.md` §10).
#[cfg(windows)]
#[derive(Debug, Default)]
pub struct RealTerminal {
    _private: (),
}

#[cfg(windows)]
impl RealTerminal {
    /// Windows has no terminal to open until ConPTY exists.
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(windows)]
impl Terminal for RealTerminal {
    fn read_byte(&mut self) -> io::Result<Option<u8>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "interactive input needs a console host (docs/portability.md §10)",
        ))
    }

    fn write_all(&mut self, _bytes: &[u8]) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "interactive output needs a console host (docs/portability.md §10)",
        ))
    }

    fn size(&self) -> Size {
        Size::FALLBACK
    }

    fn enter_raw(&mut self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "raw mode needs a console host (docs/portability.md §10)",
        ))
    }

    fn leave_raw(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Await two readiness futures, reporting whether the second finished first.
///
/// `compio` has no `select!`: `PollFd` submits one `PollOnce`
/// (`IORING_OP_POLL_ADD`) per descriptor, so combining two of them is a
/// `poll_fn` over the pair, and that is the whole combinator. The loser is
/// dropped mid-poll, and what it drops is a readiness registration rather than
/// a read, so there is no half-finished operation to unwind.
#[cfg(unix)]
async fn which_ready<A, B>(first: A, second: B) -> io::Result<bool>
where
    A: std::future::Future<Output = io::Result<()>>,
    B: std::future::Future<Output = io::Result<()>>,
{
    use std::future::poll_fn;
    use std::task::Poll;

    let mut first = std::pin::pin!(first);
    let mut second = std::pin::pin!(second);
    poll_fn(|cx| {
        if let Poll::Ready(result) = first.as_mut().poll(cx) {
            return Poll::Ready(result.map(|()| false));
        }
        if let Poll::Ready(result) = second.as_mut().poll(cx) {
            return Poll::Ready(result.map(|()| true));
        }
        Poll::Pending
    })
    .await
}

#[cfg(unix)]
impl RealTerminal {
    /// Open the private read handle for the keyboard, once.
    ///
    /// A private handle, so the poll registration cannot disturb the descriptor
    /// the editor writes through.
    fn prepare_read_source(&mut self) -> io::Result<()> {
        use std::os::fd::AsFd;
        if self.read_source.is_none() {
            let owned = match self.tty.as_ref() {
                Some(tty) => tty.try_clone()?.into(),
                None => std::io::stdin().as_fd().try_clone_to_owned()?,
            };
            self.read_source = Some(compio::runtime::fd::PollFd::new(owned)?);
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Terminal for RealTerminal {
    fn set_watch(&mut self, fd: Option<std::os::fd::OwnedFd>) {
        // A registration failure is not worth failing the prompt over: the
        // editor keeps waiting on the keyboard alone, which is the behaviour
        // before there was a watcher.
        self.watch = fd.and_then(|fd| compio::runtime::fd::PollFd::new(fd).ok());
    }

    fn await_input(&mut self) -> io::Result<Input> {
        let runtime = compio::runtime::Runtime::current();
        self.prepare_read_source()?;
        let source = self.read_source.as_ref().expect("prepared just above");
        let Some(watch) = self.watch.as_ref() else {
            runtime.block_on(async { source.read_ready().await })?;
            return Ok(Input::Key);
        };
        // The keyboard and the watch are awaited together. The pair is raced
        // against the runtime's timer as a backstop: a completion-based poll is
        // only as good as the kernel's willingness to complete it for this file
        // type, and a watcher that silently stopped being awaited would leave
        // the prompt frozen until the next keystroke. A backstop that finds
        // nothing changed reads the watch and paints nothing.
        //
        // Keyboard readiness is re-checked rather than believed: a character
        // device can answer a readiness poll with no byte behind it, and taking
        // that for a keystroke parks the editor inside a read.
        const BACKSTOP: std::time::Duration = std::time::Duration::from_secs(1);
        loop {
            let watched = match runtime.block_on(compio::time::timeout(
                BACKSTOP,
                which_ready(source.read_ready(), watch.read_ready()),
            )) {
                Ok(Ok(watched)) => watched,
                Ok(Err(error)) => return Err(error),
                // The backstop: a chance to refresh, not an event.
                Err(_elapsed) => return Ok(Input::Watch),
            };
            if watched {
                return Ok(Input::Watch);
            }
            if baash_sys::bytes_available(self.fd) > 0 {
                return Ok(Input::Key);
            }
        }
    }

    fn read_byte(&mut self) -> io::Result<Option<u8>> {
        // Readiness comes from the runtime; the read itself is one byte from a
        // descriptor already known to be ready. A keystroke must not park the
        // thread that also has to answer a scan or reap a child
        // (`docs/direction.md` §2), and `Runtime::current()` is set by `baash`
        // around the loop, so this needs no runtime of its own.
        let runtime = compio::runtime::Runtime::current();
        self.prepare_read_source()?;
        let source = self.read_source.as_ref().expect("prepared just above");
        runtime.block_on(async { source.read_ready().await })?;

        let mut byte = [0u8; 1];
        match baash_sys::read_fd(self.fd, &mut byte) {
            Ok(0) => Ok(None),
            Ok(_) => Ok(Some(byte[0])),
            Err(error) => Err(error),
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
        baash_sys::window_size_fd(self.fd)
            .map(|(columns, rows)| Size { columns, rows })
            .unwrap_or(Size::FALLBACK)
    }

    fn enter_raw(&mut self) -> io::Result<()> {
        if self.raw.is_none() {
            self.raw = Some(baash_sys::RawMode::enable(self.fd)?);
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

#[cfg(unix)]
impl Drop for RealTerminal {
    fn drop(&mut self) {
        // `RawMode`'s own `Drop` is the panic-safe half; taking it here just
        // makes the restore explicit and idempotent.
        let _ = self.leave_raw();
    }
}
