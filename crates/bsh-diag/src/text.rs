//! Byte-to-display mapping shared by every renderer.
//!
//! Shell text is bytes, not `str`, so a renderer cannot simply call
//! `String::from_utf8_lossy` and count bytes: an invalid byte becomes a
//! three-byte U+FFFD in the lossy string and every later column shifts. These
//! helpers walk the original bytes instead and assign display cells one at a
//! time, which keeps byte offsets and terminal columns in step.
//!
//! Two display rules are deliberate:
//!
//! * a tab advances to the next tab stop ([`TAB_WIDTH`]), so `\t` occupies a
//!   variable number of cells but always starts at a multiple of the stop;
//! * a non-printable character, including an invalid UTF-8 byte, is rendered as
//!   U+FFFD and occupies exactly one cell. That makes the mapping
//!   byte-offset → display-column total and keeps hostile control sequences
//!   (an ESC in a filename, say) out of the terminal.

use std::ops::Range;

use unicode_width::UnicodeWidthChar;

use crate::Span;

/// Tab stop width used by every renderer, in display cells.
///
/// Four is the conventional choice for source excerpts and matches miette's
/// default. A tab at display column `c` advances to the next multiple of this
/// value, so carets never rely on the terminal's own tab handling.
pub const TAB_WIDTH: usize = 4;

/// What a visible character looks like when it cannot be printed literally.
const REPLACEMENT: char = '\u{FFFD}';

/// One decoded unit of shell text.
#[derive(Clone, Copy, Debug)]
enum Unit {
    /// A valid Unicode scalar.
    Char(char),
    /// One byte that is not part of a valid UTF-8 sequence.
    Invalid,
}

/// Visit every display unit in `bytes`, preserving one unit per original byte
/// for invalid sequences.
fn for_each_unit(bytes: &[u8], mut visit: impl FnMut(Unit)) {
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            visit(Unit::Char(c));
        }
        // `invalid()` is the maximal invalid subsequence; emit one unit per
        // byte so a byte offset can always be mapped back to a cell.
        for _ in chunk.invalid() {
            visit(Unit::Invalid);
        }
    }
}

/// The next tab stop strictly after display column `column`.
fn next_tab_stop(column: usize) -> usize {
    column + TAB_WIDTH - (column % TAB_WIDTH)
}

/// The width of a printable character, in display cells.
///
/// Combining marks are zero-width, CJK and most emoji are two, everything else
/// is one. `unicode-width` returns `None` for characters with no defined width;
/// one cell is the safe fallback because we always display something.
fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(1)
}

/// The display column reached after printing `bytes` starting at column 0.
///
/// This is what caret alignment uses: `display_col(&line[..start])` is the
/// number of spaces to emit before the first caret.
pub(crate) fn display_col(bytes: &[u8]) -> usize {
    let mut column = 0usize;
    for_each_unit(bytes, |unit| match unit {
        Unit::Char('\t') => column = next_tab_stop(column),
        // Control characters are shown as U+FFFD, one cell wide.
        Unit::Char(c) if c.is_control() => column += 1,
        Unit::Char(c) => column += char_width(c),
        Unit::Invalid => column += 1,
    });
    column
}

/// Render `bytes` for terminal display, expanding tabs and replacing
/// non-printables.
///
/// Returns the rendered text and its display width. The text never contains a
/// raw control character, so a source line cannot smuggle ANSI escapes through
/// a diagnostic.
pub(crate) fn render_display(bytes: &[u8]) -> (String, usize) {
    let mut out = String::with_capacity(bytes.len());
    let mut column = 0usize;
    for_each_unit(bytes, |unit| match unit {
        Unit::Char('\t') => {
            let stop = next_tab_stop(column);
            for _ in column..stop {
                out.push(' ');
            }
            column = stop;
        }
        Unit::Char(c) if c.is_control() => {
            out.push(REPLACEMENT);
            column += 1;
        }
        Unit::Char(c) => {
            out.push(c);
            column += char_width(c);
        }
        Unit::Invalid => {
            out.push(REPLACEMENT);
            column += 1;
        }
    });
    (out, column)
}

/// Append `bytes` to `out` as a single line.
///
/// Newlines, carriage returns and tabs collapse to a space; every other control
/// character and every invalid byte becomes U+FFFD. Used by the one-line and
/// header renderers, where a message containing a newline or an ANSI escape
/// must not break the format or reach the terminal.
pub(crate) fn push_one_line(out: &mut String, bytes: &[u8]) {
    for_each_unit(bytes, |unit| match unit {
        Unit::Char('\n' | '\r' | '\t') => out.push(' '),
        Unit::Char(c) if c.is_control() => out.push(REPLACEMENT),
        Unit::Char(c) => out.push(c),
        Unit::Invalid => out.push(REPLACEMENT),
    });
}

/// Clamp a span to `len` bytes, preserving `start <= end`.
pub(crate) fn clamp_span(span: &Span, len: usize) -> Range<usize> {
    let start = span.start.min(len);
    let end = span.end.min(len).max(start);
    start..end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_advance_to_stops() {
        assert_eq!(display_col(b"\t"), 4);
        assert_eq!(display_col(b"a\t"), 4);
        assert_eq!(display_col(b"abcd\t"), 8);
        assert_eq!(display_col(b"ab\tc"), 5);
        assert_eq!(render_display(b"\tx").0, "    x");
    }

    #[test]
    fn wide_characters_take_two_cells() {
        // Each CJK ideograph is two columns.
        assert_eq!(display_col("世界".as_bytes()), 4);
        assert_eq!(display_col("a世b".as_bytes()), 4);
    }

    #[test]
    fn combining_marks_take_no_extra_cell() {
        // "e" followed by U+0301 (combining acute accent).
        assert_eq!(display_col("e\u{0301}".as_bytes()), 1);
    }

    #[test]
    fn invalid_bytes_are_one_cell_each() {
        assert_eq!(display_col(b"a\xFFb"), 3);
        assert_eq!(display_col(b"\xFF\xFE"), 2);
        assert_eq!(render_display(b"a\xFFb").0, "a\u{FFFD}b");
    }

    #[test]
    fn control_characters_are_replaced() {
        assert_eq!(render_display(b"a\x1bb").0, "a\u{FFFD}b");
        assert_eq!(display_col(b"a\x1bb"), 3);
    }

    #[test]
    fn one_line_flattens_newlines_and_escapes() {
        let mut out = String::new();
        push_one_line(&mut out, b"a\nb\r\tc\x1bd\xFF");
        assert_eq!(out, "a b  c\u{FFFD}d\u{FFFD}");
        assert!(!out.contains('\u{1b}'));
    }

    #[test]
    fn spans_clamp_to_the_source() {
        assert_eq!(clamp_span(&(2..9), 4), 2..4);
        assert_eq!(clamp_span(&(9..12), 4), 4..4);
        let reversed = std::ops::Range { start: 3, end: 1 };
        assert_eq!(clamp_span(&reversed, 4), 3..3);
    }
}
