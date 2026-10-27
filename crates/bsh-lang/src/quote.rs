//! Insertion quoting for the completer (DESIGN.md §6.9).
//!
//! The completer has a candidate (a filename, a variable value, a command
//! name) and a cursor position inside some partially typed word. It needs the
//! bytes to insert so that, when the line is lexed and expanded again, exactly
//! the candidate's bytes appear in argv. Over-escaping is harmless — an
//! escaped letter still means the same letter — while under-escaping silently
//! turns one argument into two or into a different command, so this module is
//! deliberately conservative.

/// Where a completion is being inserted, which decides what must be escaped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuoteCtx {
    /// Bare, in argument position.
    Unquoted,
    /// Inside an open double-quoted string.
    DoubleQuoted,
    /// Inside an open single-quoted string.
    ///
    /// The solution is the familiar `'\''`: close the quote, escape the
    /// quote, reopen it. Everything else is literal inside `'…'`, including
    /// newlines, so no other byte needs attention.
    SingleQuoted,
}

/// Bytes that need no escaping in an unquoted word.
fn is_bare_safe(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'_' | b'@' | b'%' | b'+' | b'=' | b':' | b',' | b'.' | b'/' | b'-'
        )
}

/// Escape `bytes` so that inserting it at `ctx` does not change the parse.
///
/// For [`QuoteCtx::SingleQuoted`] the returned bytes are meant to be inserted
/// between the existing opening and closing `'`; every quote in the candidate
/// closes and reopens the string, so the cursor ends up back inside it.
///
/// A newline in unquoted position is emitted as `$'\n'`: a backslash-newline
/// would be a *line continuation* and disappear (and a raw newline would
/// terminate the command). This relies on the `$'…'` support DESIGN.md §3.3
/// commits to.
pub fn quote(bytes: &[u8], ctx: QuoteCtx) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 8);
    for &b in bytes {
        match ctx {
            QuoteCtx::Unquoted => {
                if is_bare_safe(b) {
                    out.push(b);
                } else if b == b'\n' {
                    out.extend_from_slice(b"$'\\n'");
                } else {
                    out.push(b'\\');
                    out.push(b);
                }
            }
            QuoteCtx::DoubleQuoted => {
                if matches!(b, b'$' | b'`' | b'"' | b'\\') {
                    out.push(b'\\');
                }
                // `!` is deliberately not escaped: the interactive profile has
                // history expansion off (DESIGN.md §3.2), and `\!` inside
                // double quotes would keep the backslash as a literal byte.
                out.push(b);
            }
            QuoteCtx::SingleQuoted => {
                if b == b'\'' {
                    out.extend_from_slice(b"'\\''");
                } else {
                    out.push(b);
                }
            }
        }
    }
    out
}
