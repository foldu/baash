//! Blanking the terminal: the part of `clear` that is not just bytes.
//!
//! On Unix a terminal is a byte stream, so the ECMA-48 sequence *is* the
//! operation: [`clear`] writes it to standard output and flushes. On Windows a
//! console interprets escapes only when the process asked for it
//! (`ENABLE_VIRTUAL_TERMINAL_PROCESSING`), and one that has not been asked
//! renders them literally, so asking is part of the operation — see
//! [`crate::console::write_escapes`], which puts the mode back afterwards.
//!
//! A standard output that is not a terminal — a file, a pipe, a redirection —
//! gets the bytes and nothing else happens on both platforms. That is what
//! ncurses' `clear` does too: `clear > file` records the sequence, because the
//! bytes are the answer and where they land is the redirect's business.

use std::io;

/// Write `bytes` to standard output as a terminal-clearing sequence.
///
/// The caller owns the *choice* of sequence (`baash-utils`'s `clear` builds it
/// from its options); this owns what the platform needs around the write.
///
/// # Errors
///
/// The write's error: a closed descriptor, a full disk when the output was
/// redirected, or a console that refused the mode change.
#[cfg(not(windows))]
pub fn clear(bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut stdout = io::stdout().lock();
    stdout.write_all(bytes)?;
    stdout.flush()
}

/// Write `bytes` to standard output as a terminal-clearing sequence.
///
/// The caller owns the *choice* of sequence (`baash-utils`'s `clear` builds it
/// from its options); this owns what the platform needs around the write, which
/// on Windows is the console's mode.
///
/// # Errors
///
/// The write's error: a closed descriptor, a full disk when the output was
/// redirected, or a console that refused the mode change.
#[cfg(windows)]
pub fn clear(bytes: &[u8]) -> io::Result<()> {
    crate::console::write_escapes(bytes)
}
