//! Insertion quoting for the completer (DESIGN.md §6.9).
//!
//! The completer has a candidate (a filename, a variable value, a command
//! name) and a cursor position inside some partially typed word. It needs the
//! bytes to insert so that, when the line is lexed and expanded again, exactly
//! the candidate's bytes appear in argv. Over-escaping is *nearly* harmless — an
//! escaped letter still means the same letter — while under-escaping silently
//! turns one argument into two or into a different command, so this module is
//! deliberately conservative.
//!
//! The exception is `~`, which is not a letter: `\~` is a literal tilde, so
//! escaping a home-relative candidate turns `~/src` into a path that does not
//! exist. It is therefore left bare in an unquoted word (where it only expands
//! at the start of the word — see [`quote`]).

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
///
/// Everything from `0x80` up is bare: no shell metacharacter is non-ASCII, so a
/// high byte is a literal byte in any locale, and escaping it splits a UTF-8
/// sequence across backslashes — the bytes still lex back to the same argv, but
/// the line reads as `\<mojibake>` and the terminal's column count goes wrong,
/// because each backslash is its own character between two halves of one.
fn is_bare_safe(b: u8) -> bool {
    b >= 0x80
        || b.is_ascii_alphanumeric()
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
                } else if b == b'~' {
                    // A `~` is special only at the start of an unquoted word,
                    // and there it must stay bare: `\~` makes it a literal
                    // tilde, which is not what a home-relative candidate means
                    // (and not what the user typed to get there). Anywhere else
                    // in the word it is already literal, so escaping it would
                    // only add noise.
                    out.push(b);
                } else if b == b'\n' {
                    out.extend_from_slice(b"$'\\n'");
                } else {
                    out.push(b'\\');
                    out.push(b);
                }
            }
            QuoteCtx::DoubleQuoted => {
                // A `!` cannot be escaped with a backslash here: the lexer
                // keeps `\!` as two bytes inside double quotes, so argv would
                // gain a backslash, and history expansion still runs on the
                // result because double quotes do not inhibit it. Closing the
                // string for one single-quoted bang leaves the argument byte
                // for byte and puts the `!` where the expander cannot see it.
                if b == b'!' {
                    out.extend_from_slice(b"\"'!'\"");
                    continue;
                }
                if matches!(b, b'$' | b'`' | b'"' | b'\\') {
                    out.push(b'\\');
                }
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

#[cfg(test)]
mod tests {
    use super::{QuoteCtx, quote};

    #[test]
    fn a_home_relative_word_keeps_its_tilde_bare() {
        // `cd ~/src/gith<TAB>` completes to `~/src/github.com/`: escaping the
        // tilde would make it a literal `~` directory (`\~/src`), which is the
        // bug this pins.
        assert_eq!(
            quote(b"~/src/github.com/", QuoteCtx::Unquoted),
            b"~/src/github.com/"
        );
        // A tilde that is not at the start of the word never expands, so the
        // escape would only be noise.
        assert_eq!(quote(b"a~b", QuoteCtx::Unquoted), b"a~b");
        // Inside a quote it was never special.
        assert_eq!(quote(b"~/src", QuoteCtx::DoubleQuoted), b"~/src");
        assert_eq!(quote(b"~/src", QuoteCtx::SingleQuoted), b"~/src");
    }

    #[test]
    fn non_ascii_bytes_are_inserted_as_they_are() {
        // A multi-byte character is not a shell metacharacter: escaping its
        // bytes inserts a backslash *inside* the character, which renders as
        // two replacement characters and breaks the column count.
        let path = "ßhell/".as_bytes();
        assert_eq!(quote(path, QuoteCtx::Unquoted), path);
        let mixed = "src/ßhell/a b".as_bytes();
        assert_eq!(
            quote(mixed, QuoteCtx::Unquoted),
            "src/ßhell/a\\ b".as_bytes()
        );
    }

    #[test]
    fn a_bang_inside_double_quotes_is_glued_as_a_single_quoted_piece() {
        // `\!` would keep its backslash inside double quotes, and double
        // quotes do not inhibit history expansion, so the only spelling that
        // is right for both the lexer and the expander closes the string for
        // one single-quoted bang.
        assert_eq!(
            quote(b"bang!name", QuoteCtx::DoubleQuoted),
            b"bang\"'!'\"name"
        );
        // Unquoted, a backslash hides the `!` from history expansion and the
        // lexer removes it, so no glue is needed.
        assert_eq!(quote(b"bang!name", QuoteCtx::Unquoted), b"bang\\!name");
        // Inside single quotes it was never special.
        assert_eq!(quote(b"bang!name", QuoteCtx::SingleQuoted), b"bang!name");
    }

    #[test]
    fn double_quotes_still_escape_what_the_lexer_would_read() {
        assert_eq!(
            quote(b"$x `y` \"z\" \\", QuoteCtx::DoubleQuoted),
            b"\\$x \\`y\\` \\\"z\\\" \\\\"
        );
    }

    #[test]
    fn everything_else_is_still_escaped() {
        assert_eq!(quote(b"a b", QuoteCtx::Unquoted), b"a\\ b");
        assert_eq!(quote(b"x*y", QuoteCtx::Unquoted), b"x\\*y");
        assert_eq!(quote(b"$x", QuoteCtx::Unquoted), b"\\$x");
        // A newline cannot be a backslash escape: that would be a line
        // continuation. It goes through `$'…'`.
        assert_eq!(quote(b"a\nb", QuoteCtx::Unquoted), b"a$'\\n'b");
    }
}
