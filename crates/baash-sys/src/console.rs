//! The Windows console, as the line editor needs it.
//!
//! On Unix the editor opens `/dev/tty` and speaks termios. Windows has no such
//! files and no termios: a console is a pair of handles, and the modes that
//! matter are bits in `GetConsoleMode`. This module is that boundary, and it is
//! the counterpart of `term.rs` — [`Console`] is the terminal itself,
//! [`ConsoleMode`] is the raw-mode guard.
//!
//! # Why the console and not stdin/stdout
//!
//! The editor must paint on the terminal even when the command's output is
//! redirected (`baash > transcript`), which is why the Unix side prefers
//! `/dev/tty`. The same is true here, so `CONIN$`/`CONOUT$` are opened first and
//! the standard handles are only a fallback for a process whose stdio *is* the
//! console.
//!
//! # Input and output are UTF-16
//!
//! `ReadConsoleW` and `WriteConsoleW` exchange UTF-16, while the rest of the
//! shell is bytes. [`Console::read`] appends UTF-8 to a buffer and
//! [`Console::write`] takes UTF-8, so the editor is unaware of the difference.
//! What the console *sends*, though, is a byte-oriented abstraction: with
//! [`ENABLE_VIRTUAL_TERMINAL_INPUT`] the key sequences arrive as the same ANSI
//! escapes a Unix terminal sends, which is what lets one key decoder serve both
//! platforms.
//!
//! # What a console still cannot do
//!
//! There is no `poll` on a console handle, so the prompt watcher has nothing to
//! wait on here: `await_input` blocks in a read. That is why the watcher is
//! `#[cfg(target_os = "linux")]` rather than merely non-Windows.

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::io::{AsRawHandle, RawHandle};

/// `(DWORD)-10`, the pseudo-handle kind `GetStdHandle` expects for stdin.
const STD_INPUT_HANDLE: u32 = (-10i32) as u32;
/// `(DWORD)-11`, the pseudo-handle kind `GetStdHandle` expects for stdout.
const STD_OUTPUT_HANDLE: u32 = (-11i32) as u32;

/// `ENABLE_PROCESSED_INPUT`: Ctrl-C is turned into a signal instead of a byte.
const ENABLE_PROCESSED_INPUT: u32 = 0x0001;
/// `ENABLE_LINE_INPUT`: reads wait for a newline and the console edits the line.
const ENABLE_LINE_INPUT: u32 = 0x0002;
/// `ENABLE_ECHO_INPUT`: the console echoes what is typed.
const ENABLE_ECHO_INPUT: u32 = 0x0004;
/// `ENABLE_VIRTUAL_TERMINAL_INPUT`: keys arrive as ANSI escape sequences.
const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
/// `ENABLE_PROCESSED_OUTPUT`: the console interprets control characters.
const ENABLE_PROCESSED_OUTPUT: u32 = 0x0001;
/// `ENABLE_VIRTUAL_TERMINAL_PROCESSING`: the console interprets ANSI escapes.
const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

/// `COORD`: a cell in the screen buffer.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Coord {
    x: i16,
    y: i16,
}

/// `SMALL_RECT`: the window, in cells, inclusive on all four sides.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SmallRect {
    left: i16,
    top: i16,
    right: i16,
    bottom: i16,
}

/// `CONSOLE_SCREEN_BUFFER_INFO`, in the console's own layout.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ScreenBufferInfo {
    size: Coord,
    cursor: Coord,
    attributes: u16,
    window: SmallRect,
    maximum_window_size: Coord,
}

// The console API is in kernel32, which every Windows process links.
unsafe extern "system" {
    fn GetStdHandle(kind: u32) -> RawHandle;
    fn GetConsoleMode(console: RawHandle, mode: *mut u32) -> i32;
    fn SetConsoleMode(console: RawHandle, mode: u32) -> i32;
    fn GetConsoleScreenBufferInfo(console: RawHandle, info: *mut ScreenBufferInfo) -> i32;
    fn ReadConsoleW(
        console: RawHandle,
        buffer: *mut u16,
        length: u32,
        read: *mut u32,
        reserved: *mut c_void,
    ) -> i32;
    fn WriteConsoleW(
        console: RawHandle,
        buffer: *const u16,
        length: u32,
        written: *mut u32,
        reserved: *mut c_void,
    ) -> i32;
}

/// One end of the console, owned when we opened it and borrowed when it is a
/// standard handle.
enum End {
    /// A handle this module opened (`CONIN$`/`CONOUT$`) and closes on drop.
    Owned(File),
    /// `GetStdHandle`'s handle, which belongs to the process and is not ours.
    Standard(RawHandle),
}

impl End {
    fn handle(&self) -> RawHandle {
        match self {
            Self::Owned(file) => file.as_raw_handle(),
            Self::Standard(handle) => *handle,
        }
    }

    /// One end of the console: the standard handle when it is a console, the
    /// device when this end is redirected.
    ///
    /// The standard handle comes first because when the process was started in
    /// a console it *is* that console, with both access bits already. The
    /// device is what makes `baash > transcript` still paint on the terminal
    /// when stdout is a file.
    ///
    /// Both devices are opened `read`+`write`, and that is not tidiness:
    /// `GetConsoleMode` requires read access, so a write-only `CONOUT$` opens
    /// perfectly and then fails its own mode query with "access denied" — the
    /// console is there, the check says it is not, and the editor reports
    /// having no terminal. Opening the output end for reading costs nothing.
    fn console(device: &str, standard: u32) -> io::Result<Self> {
        if let Some(handle) = standard_handle(standard)
            && mode_of(handle).is_ok()
        {
            return Ok(Self::Standard(handle));
        }
        let error = match File::options().read(true).write(true).open(device) {
            Ok(file) => {
                let end = Self::Owned(file);
                return match mode_of(end.handle()) {
                    Ok(_) => Ok(end),
                    Err(error) => Err(error),
                };
            }
            Err(error) => error,
        };
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("{device}: {error}"),
        ))
    }
}

impl std::fmt::Debug for End {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Owned(file) => f.debug_tuple("Owned").field(&file.as_raw_handle()).finish(),
            Self::Standard(handle) => f.debug_tuple("Standard").field(handle).finish(),
        }
    }
}

/// The console the line editor paints on and reads from.
#[derive(Debug)]
pub struct Console {
    input: End,
    output: End,
}

impl Console {
    /// Open the process's console.
    ///
    /// Fails with [`io::ErrorKind::Unsupported`] when either end is not a
    /// console — a pipe, a file, a socket — and the message says which end and
    /// why, because "no terminal" with no detail is what made this hard to see
    /// the first time.
    pub fn open() -> io::Result<Self> {
        let input = End::console("CONIN$", STD_INPUT_HANDLE)
            .map_err(|error| io::Error::new(error.kind(), format!("input: {error}")))?;
        let output = End::console("CONOUT$", STD_OUTPUT_HANDLE)
            .map_err(|error| io::Error::new(error.kind(), format!("output: {error}")))?;
        Ok(Self { input, output })
    }

    /// The window size in cells: `(columns, rows)`.
    ///
    /// The *window* is the visible part; the screen buffer is usually much
    /// taller, and wrapping must follow what the user can see.
    pub fn size(&self) -> io::Result<(u16, u16)> {
        let mut info = ScreenBufferInfo::default();
        // SAFETY: `info` is a writable `CONSOLE_SCREEN_BUFFER_INFO` and the
        // handle is a console, checked in `open`. The call writes only `info`.
        let ok = unsafe { GetConsoleScreenBufferInfo(self.output.handle(), &mut info) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let columns = info.window.right - info.window.left + 1;
        let rows = info.window.bottom - info.window.top + 1;
        Ok((
            u16::try_from(columns.max(0)).unwrap_or(u16::MAX),
            u16::try_from(rows.max(0)).unwrap_or(u16::MAX),
        ))
    }

    /// Read whatever the console has, as UTF-8, appending to `out`.
    ///
    /// Returns the number of bytes appended. Zero means end of input: the
    /// console reports it when its input is closed, which is the editor's EOF.
    pub fn read(&mut self, out: &mut Vec<u8>) -> io::Result<usize> {
        let mut units = [0u16; 256];
        let mut read = 0u32;
        // SAFETY: the buffer is `units.len()` UTF-16 code units, which is what
        // `length` promises, and the console writes at most that many.
        let ok = unsafe {
            ReadConsoleW(
                self.input.handle(),
                units.as_mut_ptr(),
                units.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let read = usize::try_from(read).unwrap_or(0).min(units.len());
        if read == 0 {
            return Ok(0);
        }
        let text = String::from_utf16_lossy(&units[..read]);
        out.extend_from_slice(text.as_bytes());
        Ok(text.len())
    }

    /// Write bytes to the console.
    ///
    /// Invalid UTF-8 becomes `U+FFFD` rather than failing: a frame is a
    /// mixture of escape sequences and the line's bytes, and half a sequence
    /// painted is easier to recover from than an editor that stops painting.
    ///
    /// Long writes are split: `WriteConsoleW` refuses a buffer that is too
    /// large, and a pasted screenful is easily large enough. The split is
    /// between characters, so a surrogate pair is never cut in half.
    pub fn write(&self, bytes: &[u8]) -> io::Result<()> {
        /// The largest chunk `WriteConsoleW` is documented to accept.
        const CHUNK: usize = 8192;
        let text = String::from_utf8_lossy(bytes);
        let mut chunk: Vec<u16> = Vec::with_capacity(CHUNK);
        for character in text.chars() {
            if chunk.len() >= CHUNK {
                self.write_units(&chunk)?;
                chunk.clear();
            }
            let mut pair = [0u16; 2];
            chunk.extend_from_slice(character.encode_utf16(&mut pair));
        }
        if !chunk.is_empty() {
            self.write_units(&chunk)?;
        }
        Ok(())
    }

    /// One `WriteConsoleW` call.
    fn write_units(&self, units: &[u16]) -> io::Result<()> {
        let mut written = 0u32;
        // SAFETY: `units` holds `units.len()` UTF-16 code units and stays alive
        // for the call; the console writes at most that many.
        let ok = unsafe {
            WriteConsoleW(
                self.output.handle(),
                units.as_ptr(),
                units.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Turn echo off, keeping the console's line editing, for `read -s`.
    ///
    /// The console still gathers a line and still processes signals, which is
    /// what a `read` wants; only the echo is gone, so a password does not
    /// appear on screen while it is typed.
    pub fn enter_no_echo(&self) -> io::Result<ConsoleMode> {
        let input = self.input.handle();
        let output = self.output.handle();
        let saved_input = mode_of(input)?;
        let saved_output = mode_of(output)?;
        set_mode(input, saved_input & !ENABLE_ECHO_INPUT)?;
        Ok(ConsoleMode {
            input,
            output,
            saved_input,
            saved_output,
        })
    }

    /// Put the console into the mode a line editor needs, returning a guard
    /// that restores what was there before.
    ///
    /// Input: line editing, echo and Ctrl-C processing are the console's, and
    /// all three fight the editor — it draws the line, and `Ctrl-C` has to
    /// arrive as a byte so the line can be abandoned without a signal. Virtual
    /// terminal input then turns keys into the ANSI sequences the editor
    /// already decodes.
    ///
    /// Output: ANSI escapes have to be interpreted, which is what virtual
    /// terminal processing turns on.
    pub fn enter_raw(&self) -> io::Result<ConsoleMode> {
        let input = self.input.handle();
        let output = self.output.handle();
        let saved_input = mode_of(input)?;
        let saved_output = mode_of(output)?;
        let raw_input = (saved_input
            & !(ENABLE_PROCESSED_INPUT | ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT))
            | ENABLE_VIRTUAL_TERMINAL_INPUT;
        let raw_output =
            saved_output | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING;
        set_mode(input, raw_input)?;
        if let Err(error) = set_mode(output, raw_output) {
            // The input mode is already changed; put it back before reporting,
            // or a failed prompt leaves the console unable to read a line.
            let _ = set_mode(input, saved_input);
            return Err(error);
        }
        Ok(ConsoleMode {
            input,
            output,
            saved_input,
            saved_output,
        })
    }
}

/// The standard handle `kind` names, if the process has one.
fn standard_handle(kind: u32) -> Option<RawHandle> {
    // SAFETY: `GetStdHandle` only looks up a process-wide handle and retains
    // nothing; whether it is a console is the caller's `GetConsoleMode`.
    let handle = unsafe { GetStdHandle(kind) };
    (!handle.is_null() && handle as isize != -1).then_some(handle)
}

/// `GetConsoleMode` for one handle.
fn mode_of(console: RawHandle) -> io::Result<u32> {
    let mut mode = 0u32;
    // SAFETY: `mode` is a writable `DWORD` and the call writes only that.
    let ok = unsafe { GetConsoleMode(console, &mut mode) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(mode)
}

/// `SetConsoleMode` for one handle.
fn set_mode(console: RawHandle, mode: u32) -> io::Result<()> {
    // SAFETY: the handle is a console and `mode` is a plain bit set.
    let ok = unsafe { SetConsoleMode(console, mode) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The console modes saved by [`Console::enter_raw`], restored on drop.
///
/// `Drop` is the panic-safe half of the editor's terminal handling: a panic
/// while editing must not leave the user's console in a mode that neither the
/// shell nor the next program can use.
#[derive(Debug)]
pub struct ConsoleMode {
    input: RawHandle,
    output: RawHandle,
    saved_input: u32,
    saved_output: u32,
}

impl ConsoleMode {
    /// Restore the saved modes explicitly, reporting a failure.
    pub fn restore(self) -> io::Result<()> {
        let this = std::mem::ManuallyDrop::new(self);
        set_mode(this.input, this.saved_input)?;
        set_mode(this.output, this.saved_output)?;
        Ok(())
    }
}

impl Drop for ConsoleMode {
    fn drop(&mut self) {
        // Best effort: a panic must still leave the console usable.
        let _ = set_mode(self.input, self.saved_input);
        let _ = set_mode(self.output, self.saved_output);
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn a_non_console_handle_is_unsupported() {
        // A test harness has no console (or has one it does not own); either
        // way the answer must be an error, never a panic.
        let opened = Console::open();
        if let Ok(console) = opened {
            assert!(console.size().is_ok(), "a console reports its window");
        }
    }

    #[test]
    fn the_guard_restores_what_it_saved() {
        let Ok(console) = Console::open() else {
            return; // no console in this environment: nothing to check
        };
        let before = mode_of(console.input.handle()).expect("input mode");
        {
            let guard = console.enter_raw().expect("raw mode");
            let during = mode_of(console.input.handle()).expect("input mode");
            assert_ne!(before, during, "raw mode changes the input mode");
            assert_eq!(during & ENABLE_LINE_INPUT, 0, "line input is off");
            assert_eq!(during & ENABLE_ECHO_INPUT, 0, "echo is off");
            assert_ne!(
                during & ENABLE_VIRTUAL_TERMINAL_INPUT,
                0,
                "keys arrive as ANSI sequences"
            );
            guard.restore().expect("restore");
        }
        assert_eq!(
            mode_of(console.input.handle()).expect("input mode"),
            before,
            "the mode is back to what it was"
        );
    }
}
