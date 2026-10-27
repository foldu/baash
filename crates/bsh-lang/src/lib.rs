//! Words, quoting and expansion.
//!
//! The word model is where most shell bugs live: quotedness is a property of a
//! word *part*, not of the word, because `"$@"`, `${x:-"$@"}` and `$*` cannot
//! be got right otherwise (DESIGN.md §4.2).
//!
//! Landed so far: [`quote`], which the completer needs to insert a candidate
//! without changing the parse (DESIGN.md §6.9). The [`Part`]/[`Word`] model and
//! the five expansion stages are not written yet.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Where a completion is being inserted, which decides what must be escaped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuoteCtx {
    /// Bare, in argument position.
    Unquoted,
    /// Inside double quotes.
    DoubleQuoted,
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
/// Deliberately conservative: an over-escaped word still means the same thing,
/// while an under-escaped one silently becomes a different command.
pub fn quote(bytes: &[u8], ctx: QuoteCtx) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 8);
    for &b in bytes {
        match ctx {
            QuoteCtx::Unquoted => {
                if is_bare_safe(b) {
                    out.push(b);
                } else {
                    out.push(b'\\');
                    out.push(b);
                }
            }
            QuoteCtx::DoubleQuoted => {
                if matches!(b, b'$' | b'`' | b'"' | b'\\') {
                    out.push(b'\\');
                }
                out.push(b);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_words_are_untouched() {
        assert_eq!(quote(b"src/main.rs", QuoteCtx::Unquoted), b"src/main.rs");
    }

    #[test]
    fn spaces_and_globs_are_escaped() {
        assert_eq!(
            quote(b"My File*.txt", QuoteCtx::Unquoted),
            br"My\ File\*.txt"
        );
    }

    #[test]
    fn double_quoted_escapes_only_specials() {
        assert_eq!(quote(b"a b$c", QuoteCtx::DoubleQuoted), br"a b\$c");
    }

    #[test]
    fn empty_input_stays_empty() {
        assert!(quote(b"", QuoteCtx::Unquoted).is_empty());
    }
}
