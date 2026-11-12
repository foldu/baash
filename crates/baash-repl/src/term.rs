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
    /// The terminal's window changed size. No input was consumed; the frame's
    /// geometry is stale, so the editor re-anchors and paints again.
    Resize,
    /// The requested wait elapsed with nothing to read. No input was consumed;
    /// the editor uses this to poll work that is not a descriptor it owns.
    Timeout,
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

    /// Read one byte, waiting at most `timeout`.
    ///
    /// `Ok(None)` means "no byte, and none promised": the deadline passed, or
    /// this terminal does not do timed reads. The default answers `None`
    /// immediately, which is what a terminal with no cursor-position report
    /// needs — a prompt must never wait on a question the terminal will not
    /// answer.
    fn read_byte_timeout(&mut self, _timeout: std::time::Duration) -> io::Result<Option<u8>> {
        Ok(None)
    }

    /// Whether this terminal can answer a cursor-position query (`ESC [ 6 n`).
    ///
    /// The Windows console reads keys through `ReadConsoleW` and has no report
    /// to give, so it keeps the default and the editor never asks.
    fn reports_cursor_position(&self) -> bool {
        false
    }

    /// Whether the terminal can interpret control sequences at all.
    ///
    /// A `TERM=dumb` terminal shows text and nothing else: prompt marks,
    /// titles and the clipboard payload would be printed as garbage rather
    /// than interpreted. The default is `true`, because a fake in a test and
    /// the Windows console both take escapes; the unix terminal overrides it
    /// with the environment's answer.
    fn supports_control_sequences(&self) -> bool {
        true
    }

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

    /// Wait at most `timeout` for input.
    ///
    /// This is [`Terminal::await_input`] with a bound: the editor uses it while
    /// an asynchronous completion is outstanding, so it can poll that work
    /// without being parked on the keyboard. The default ignores the bound and
    /// blocks, which is correct for a fake with a script to play and wrong for
    /// a real terminal; both real terminals override it.
    fn await_input_timeout(&mut self, _timeout: std::time::Duration) -> io::Result<Input> {
        self.await_input()
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
    /// Bytes read while asking the terminal a question, which turned out to be
    /// keystrokes rather than an answer. They belong to the line editor.
    pending: std::collections::VecDeque<u8>,
    /// The cached answer to "is the background light?", including "asked and
    /// got nothing", which must not be asked again.
    light: Option<Option<bool>>,
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
                pending: std::collections::VecDeque::new(),
                light: None,
            },
            Err(_) => Self {
                tty: None,
                fd: 0,
                raw: None,
                read_source: None,
                watch: None,
                pending: std::collections::VecDeque::new(),
                light: None,
            },
        }
    }
}

/// The real terminal on Windows: the process's console (`baash_sys::console`).
///
/// The shape is the same as on Unix — bytes in, bytes out, raw mode held for
/// one [`crate::Editor::read_line`] — because the difference is entirely inside
/// the console: keys arrive as the ANSI escapes `ENABLE_VIRTUAL_TERMINAL_INPUT`
/// produces, which is exactly what the key decoder already reads, and output
/// goes through `WriteConsoleW`, so the shell's UTF-8 reaches the screen
/// whatever the console code page is.
///
/// There is no console equivalent of polling a directory, so nothing is
/// watched here and the prompt only changes between lines.
#[cfg(windows)]
#[derive(Debug, Default)]
pub struct RealTerminal {
    /// `None` when the process has no console; every operation then reports
    /// `Unsupported` instead of pretending, and the shell falls back to
    /// reading stdin as a script.
    console: Option<baash_sys::Console>,
    /// Why there is no console, when there is not. `CONOUT$: access denied` is
    /// worth more to whoever reads the message than "no terminal attached".
    no_console: Option<String>,
    /// The modes to put back when the line is done.
    raw: Option<baash_sys::ConsoleMode>,
    /// Bytes the console produced that the editor has not consumed.
    pending: Vec<u8>,
    /// Bytes the editor wrote that are not yet a whole character, kept so a
    /// character split across two writes is not painted as two replacements.
    outgoing: Vec<u8>,
    /// The window size at the last wait, so a change is noticed without a
    /// keystroke. The console has no resize event to wait on, so the wait
    /// deadline is also the sampling interval.
    last_size: Option<Size>,
}

#[cfg(windows)]
impl RealTerminal {
    /// The console's background is a user setting and is not reported through
    /// `ReadConsoleW`, so the default palette is used.
    pub fn background_is_light(&mut self) -> Option<bool> {
        None
    }

    /// The process's console, or a terminal whose operations say so when there
    /// is none.
    pub fn new() -> Self {
        match baash_sys::Console::open() {
            Ok(console) => Self {
                console: Some(console),
                ..Self::default()
            },
            Err(error) => Self {
                no_console: Some(error.to_string()),
                ..Self::default()
            },
        }
    }

    /// The error every operation reports without a console.
    fn no_console(&self) -> io::Error {
        let reason = self
            .no_console
            .as_deref()
            .unwrap_or("this process has no terminal attached");
        io::Error::new(
            io::ErrorKind::Unsupported,
            format!("the line editor needs a console ({reason}); see docs/portability.md §10"),
        )
    }

    /// The console, or that error.
    fn host(&self) -> io::Result<&baash_sys::Console> {
        self.console.as_ref().ok_or_else(|| self.no_console())
    }
}

#[cfg(windows)]
impl Terminal for RealTerminal {
    fn read_byte(&mut self) -> io::Result<Option<u8>> {
        loop {
            if !self.pending.is_empty() {
                return Ok(Some(self.pending.remove(0)));
            }
            if self.console.is_none() {
                return Err(self.no_console());
            }
            let console = self.console.as_mut().expect("checked just above");
            // Zero means the console's input is closed, which is the editor's
            // EOF (`Ctrl-Z` on an empty line reaches it as a keystroke first).
            if console.read(&mut self.pending)? == 0 {
                return Ok(None);
            }
        }
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.outgoing.extend_from_slice(bytes);
        let chunk = drain_whole_characters(&mut self.outgoing);
        if chunk.is_empty() {
            return Ok(());
        }
        self.host()?.write(&chunk)
    }

    fn size(&self) -> Size {
        self.console
            .as_ref()
            .and_then(|console| console.size().ok())
            .map(|(columns, rows)| Size { columns, rows })
            .unwrap_or(Size::FALLBACK)
    }

    fn enter_raw(&mut self) -> io::Result<()> {
        let guard = self.host()?.enter_raw()?;
        self.raw = Some(guard);
        // A resize while a command ran is not a change to report at the first
        // prompt: the frame is painted at the current size anyway.
        self.last_size = Some(self.size());
        Ok(())
    }

    fn leave_raw(&mut self) -> io::Result<()> {
        if let Some(guard) = self.raw.take() {
            guard.restore()?;
        }
        Ok(())
    }

    fn await_input(&mut self) -> io::Result<Input> {
        /// How often an idle prompt samples the window size. The console has
        /// no resize event, so this is also the worst-case repaint delay.
        const POLL: std::time::Duration = std::time::Duration::from_millis(250);
        loop {
            match self.await_input_timeout(POLL)? {
                Input::Timeout => {}
                input => return Ok(input),
            }
        }
    }

    fn await_input_timeout(&mut self, timeout: std::time::Duration) -> io::Result<Input> {
        if !self.pending.is_empty() {
            return Ok(Input::Key);
        }
        let ready = {
            let Some(console) = self.console.as_ref() else {
                return Err(self.no_console());
            };
            console.wait_input(timeout)?
        };
        if ready {
            return Ok(Input::Key);
        }
        let size = self.size();
        match self.last_size {
            Some(last) if last != size => {
                self.last_size = Some(size);
                return Ok(Input::Resize);
            }
            None => self.last_size = Some(size),
            Some(_) => {}
        }
        Ok(Input::Timeout)
    }
}

/// Take the bytes at the start of `buffer` that form whole characters.
///
/// The editor writes bytes and the Windows console takes UTF-16, so a character
/// whose bytes are split across two writes has to wait for the rest of itself —
/// otherwise `é` is painted as two replacement characters. Waiting is only
/// right while the tail can still *become* a character, so a tail that cannot
/// (`error_len` is `Some`) is handed over to be painted lossily instead of
/// being kept forever.
///
/// Only the Windows terminal needs this; the tests run everywhere.
#[cfg_attr(not(windows), allow(dead_code))]
fn drain_whole_characters(buffer: &mut Vec<u8>) -> Vec<u8> {
    let complete = match std::str::from_utf8(buffer) {
        Ok(_) => buffer.len(),
        // A prefix that stops in the middle of a character: the rest of it is
        // still to be written.
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        // Bytes that are not the start of anything valid: write them (the
        // console substitutes `U+FFFD`) rather than stall the editor.
        Err(_) => buffer.len(),
    };
    let tail = buffer.split_off(complete);
    std::mem::replace(buffer, tail)
}

/// Which of the sources [`RealTerminal::await_input`] waits on finished first.
#[cfg(unix)]
enum Source {
    Key,
    Watch,
    Resize,
    Backstop,
}

/// Await the keyboard, the directory watch, the resize signal and a timer.
///
/// `compio` has no `select!`: each `PollFd` submits one `PollOnce`
/// (`IORING_OP_POLL_ADD`), so combining them is a `poll_fn` over the set, and
/// that is the whole combinator. The losers are dropped mid-poll, and what one
/// drops is a readiness registration (or, for the signal, its handler), never a
/// half-finished operation.
#[cfg(unix)]
async fn first_ready<A, B, C>(
    keyboard: A,
    watch: B,
    resize: C,
    backstop: std::time::Duration,
) -> io::Result<Source>
where
    A: std::future::Future<Output = io::Result<()>>,
    B: std::future::Future<Output = io::Result<()>>,
    C: std::future::Future<Output = io::Result<()>>,
{
    use std::future::poll_fn;
    use std::task::Poll;

    let mut keyboard = std::pin::pin!(keyboard);
    let mut watch = std::pin::pin!(watch);
    let mut resize = std::pin::pin!(resize);
    let mut backstop = std::pin::pin!(compio::time::sleep(backstop));
    poll_fn(|context| {
        macro_rules! ready {
            ($future:expr) => {
                match $future.as_mut().poll(context) {
                    Poll::Ready(Ok(())) => true,
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Pending => false,
                }
            };
        }
        if ready!(keyboard) {
            return Poll::Ready(Ok(Source::Key));
        }
        if ready!(resize) {
            return Poll::Ready(Ok(Source::Resize));
        }
        if ready!(watch) {
            return Poll::Ready(Ok(Source::Watch));
        }
        if backstop.as_mut().poll(context).is_ready() {
            return Poll::Ready(Ok(Source::Backstop));
        }
        Poll::Pending
    })
    .await
}

#[cfg(unix)]
impl RealTerminal {
    /// Whether the terminal's background is light, asked once with OSC 11.
    ///
    /// The answer picks the syntax theme and the prompt's accent colours. It is
    /// cached: a terminal's theme can change under the shell, but there is no
    /// theme-change event to subscribe to, and re-asking at every prompt would
    /// put a round trip in front of the cursor.
    ///
    /// Bytes typed while the question is outstanding are keystrokes, not an
    /// answer; they are put back for the line editor.
    pub fn background_is_light(&mut self) -> Option<bool> {
        if let Some(answer) = self.light {
            return answer;
        }
        let answer = if self.supports_control_sequences() {
            self.query_background().ok().flatten()
        } else {
            None
        };
        self.light = Some(answer);
        answer
    }

    fn query_background(&mut self) -> io::Result<Option<bool>> {
        // The reply has to be read in raw mode: a cooked terminal would echo
        // the question and wait for a newline that is never coming.
        let entered = self.raw.is_none();
        if entered {
            self.enter_raw()?;
        }
        let answer = self.read_background_reply();
        if entered {
            let _ = self.leave_raw();
        }
        answer
    }

    fn read_background_reply(&mut self) -> io::Result<Option<bool>> {
        const PREFIX: &[u8] = b"\x1b]11;";
        const TIMEOUT_MS: i32 = 100;
        self.write_all(b"\x1b]11;?\x1b\\")?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
        let mut reply = Vec::new();
        loop {
            // A blocking `poll` rather than the runtime's readiness source:
            // this runs once at startup, before the editor exists, and the
            // question is short enough that a parked thread is cheaper than
            // teaching the runtime about a second polled source.
            let remaining = deadline
                .saturating_duration_since(std::time::Instant::now())
                .as_millis()
                .min(TIMEOUT_MS as u128) as i32;
            if !baash_sys::poll_readable(self.fd, remaining)? {
                self.pending.extend(reply);
                return Ok(None);
            }
            let mut byte = [0u8; 1];
            match baash_sys::read_fd(self.fd, &mut byte) {
                Ok(0) => {
                    self.pending.extend(reply);
                    return Ok(None);
                }
                Ok(_) => reply.push(byte[0]),
                Err(error) => return Err(error),
            }
            // A prefix that has already diverged is a keystroke, not an answer;
            // nothing else is expected on this channel.
            if reply.len() <= PREFIX.len() && !PREFIX.starts_with(&reply) {
                self.pending.extend(reply);
                return Ok(None);
            }
            if reply.ends_with(b"\x07") || reply.ends_with(b"\x1b\\") {
                return Ok(baash_sys::osc::background_is_light(&reply));
            }
            if reply.len() > 96 {
                self.pending.extend(reply);
                return Ok(None);
            }
        }
    }

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

    /// Wait for a keystroke, the watch, a resize or `timeout`.
    ///
    /// `None` means the wait bound elapsed. [`Terminal::await_input`] turns
    /// that into its idle tick and [`Terminal::await_input_timeout`] into
    /// [`Input::Timeout`].
    ///
    /// Keyboard readiness is re-checked rather than believed: a character
    /// device can answer a readiness poll with no byte behind it, and taking
    /// that for a keystroke parks the editor inside a read.
    fn await_input_until(&mut self, timeout: std::time::Duration) -> io::Result<Option<Input>> {
        let runtime = compio::runtime::Runtime::current();
        self.prepare_read_source()?;
        let source = self.read_source.as_ref().expect("prepared just above");
        loop {
            let watch = async {
                match self.watch.as_ref() {
                    Some(watch) => watch.read_ready().await,
                    // No watch: a future that never finishes, so the race has
                    // one shape whether or not a directory is watched.
                    None => std::future::pending::<io::Result<()>>().await,
                }
            };
            let outcome = runtime.block_on(first_ready(
                source.read_ready(),
                watch,
                compio::signal::unix::signal(baash_sys::SIGWINCH),
                timeout,
            ))?;
            match outcome {
                Source::Resize => return Ok(Some(Input::Resize)),
                Source::Watch => return Ok(Some(Input::Watch)),
                Source::Backstop => return Ok(None),
                Source::Key => {}
            }
            if baash_sys::bytes_available(self.fd) > 0 {
                return Ok(Some(Input::Key));
            }
        }
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
        // The idle prompt samples the watch once a second even when nothing
        // wakes it: a completion-based poll is only as good as the kernel's
        // willingness to complete it, and a watcher that silently stopped
        // being awaited would leave the prompt frozen.
        const BACKSTOP: std::time::Duration = std::time::Duration::from_secs(1);
        // The backstop is the idle prompt's "look again" tick either way.
        Ok(self.await_input_until(BACKSTOP)?.unwrap_or(Input::Watch))
    }

    fn await_input_timeout(&mut self, timeout: std::time::Duration) -> io::Result<Input> {
        Ok(self.await_input_until(timeout)?.unwrap_or(Input::Timeout))
    }

    fn read_byte_timeout(&mut self, timeout: std::time::Duration) -> io::Result<Option<u8>> {
        if let Some(byte) = self.pending.pop_front() {
            return Ok(Some(byte));
        }
        let runtime = compio::runtime::Runtime::current();
        self.prepare_read_source()?;
        let source = self.read_source.as_ref().expect("prepared just above");
        // The same readiness the blocking read waits on, with the runtime's
        // timer as the bound: a prompt that asks for the cursor position must
        // not hang on a terminal that does not answer.
        match runtime.block_on(compio::time::timeout(timeout, source.read_ready())) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(_elapsed) => return Ok(None),
        }
        let mut byte = [0u8; 1];
        match baash_sys::read_fd(self.fd, &mut byte) {
            Ok(0) => Ok(None),
            Ok(_) => Ok(Some(byte[0])),
            Err(error) => Err(error),
        }
    }

    fn reports_cursor_position(&self) -> bool {
        // A dumb terminal would accept the query bytes and never answer, so it
        // is not asked at all.
        self.supports_control_sequences()
    }

    fn supports_control_sequences(&self) -> bool {
        !baash_sys::is_dumb_terminal()
    }

    fn read_byte(&mut self) -> io::Result<Option<u8>> {
        if let Some(byte) = self.pending.pop_front() {
            return Ok(Some(byte));
        }
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
            // Ask for bracketed paste for as long as the line is being read.
            // Without the mode a terminal sends a pasted newline as a bare
            // CR, which the decoder reads as Enter: a pasted script would run
            // line by line instead of landing in the buffer. The mode also
            // tells the decoder where the payload ends, so a pasted escape
            // sequence is inserted as text rather than executed (the
            // `Key::Paste` handling in `key.rs`). Failing to write it is not
            // fatal — the editor still works, the paste is just unbracketed.
            let _ = self.write_all(b"\x1b[?2004h");
            // Focus reporting, so a frame another program painted over while
            // the line sat idle is drawn again when the window comes back.
            // Like the paste mode it lasts only for the line: a command gets
            // the terminal without either switch on.
            let _ = self.write_all(b"\x1b[?1004h");
            // Unambiguous key reports, for as long as the line is read. The
            // kitty protocol covers terminals that speak it and xterm's
            // `modifyOtherKeys` the ones that do not; a terminal that knows
            // neither ignores both, and the decoder falls back to the legacy
            // bytes unchanged (`docs/terminal.md` §12). Both are cleared
            // again in `leave_raw`, so a command is handed the terminal in
            // the mode it expects.
            if self.supports_control_sequences() {
                let _ = self.write_all(baash_sys::osc::KEYBOARD_ENABLE);
                let _ = self.write_all(baash_sys::osc::MODIFY_OTHER_KEYS_ENABLE);
            }
        }
        Ok(())
    }

    fn leave_raw(&mut self) -> io::Result<()> {
        if self.raw.is_some() {
            // Turn the mode off before a command runs: the child inherits the
            // terminal, and its own line editor must ask for what it wants.
            // Ignoring a write error is deliberate — restoring the terminal
            // mode matters more than a terminal that would not take this.
            if self.supports_control_sequences() {
                let _ = self.write_all(baash_sys::osc::MODIFY_OTHER_KEYS_DISABLE);
                let _ = self.write_all(baash_sys::osc::KEYBOARD_DISABLE);
            }
            let _ = self.write_all(b"\x1b[?1004l");
            let _ = self.write_all(b"\x1b[?2004l");
        }
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

#[cfg(test)]
mod utf8_split_tests {
    use super::drain_whole_characters;

    #[test]
    fn ascii_is_written_at_once() {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(b"\x1b[2mello");
        assert_eq!(drain_whole_characters(&mut buffer), b"\x1b[2mello");
        assert!(buffer.is_empty());
    }

    #[test]
    fn a_character_split_across_two_writes_waits() {
        let mut buffer = Vec::new();
        buffer.push(0xC3); // the first byte of `é`
        assert!(drain_whole_characters(&mut buffer).is_empty());
        assert_eq!(buffer, vec![0xC3], "the first byte is held");

        buffer.push(0xA9);
        assert_eq!(drain_whole_characters(&mut buffer), vec![0xC3, 0xA9]);
        assert!(buffer.is_empty());
    }

    #[test]
    fn a_prefix_and_a_partial_character_are_separated() {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(b"echo \xE2\x9C"); // `echo ` and half of `✓`
        assert_eq!(drain_whole_characters(&mut buffer), b"echo ");
        assert_eq!(buffer, vec![0xE2, 0x9C]);
    }

    #[test]
    fn four_bytes_of_nonsense_are_not_held_forever() {
        let mut buffer = vec![0xFF, 0xFF, 0xFF, 0xFF];
        assert_eq!(drain_whole_characters(&mut buffer).len(), 4);
        assert!(buffer.is_empty());
    }

    #[test]
    fn an_impossible_byte_is_never_a_prefix() {
        // `0xFF` cannot start a character, so holding it would mean holding
        // every later write too.
        let mut buffer = vec![0xFF];
        assert_eq!(drain_whole_characters(&mut buffer), vec![0xFF]);
        assert!(buffer.is_empty());
    }
}
