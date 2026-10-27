//! The compact renderer for the interactive prompt.
//!
//! At most three lines, no box drawing: a header, the source line, and one
//! caret line. This is deliberately not miette's graphical report; at a prompt
//! an error competes with the next prompt and with what the user is typing
//! (DESIGN.md §4.10, §4.11). `bash` prints one line for a reason.
//!
//! ```text
//! bsh: 3:4: unclosed '(' [BSH-E0102]
//!   if (echo hi; then
//!      ^
//! ```
//!
//! The header column is the 1-based *byte* column from
//! [`SourceFile::line_col`], which is what a caller can compute without a
//! terminal. The caret is placed by display column, however: tabs expand to
//! [`crate::TAB_WIDTH`] stops and a wide character occupies two cells, so the
//! caret still lands under the right character.

use crate::text;
use crate::{Diagnostic, SourceFile};

/// Render one diagnostic as at most three lines for interactive stderr.
///
/// The result has no trailing newline. The source line and caret line are
/// indented by two spaces. Spans are clamped to the source; a zero-length span
/// and a span that reaches the end of the line each still get a single caret.
pub fn render_compact(diag: &Diagnostic, src: &SourceFile) -> String {
    let span = text::clamp_span(&diag.span, src.len());
    let (line_number, byte_column) = src.line_col(span.start);

    let mut out = String::new();
    out.push_str("bsh: ");
    out.push_str(&line_number.to_string());
    out.push(':');
    out.push_str(&byte_column.to_string());
    out.push_str(": ");
    text::push_one_line(&mut out, diag.message.as_bytes());
    out.push_str(" [");
    out.push_str(&diag.code.to_string());
    out.push_str("]\n");

    // Source line. `line` never contains a newline, and `render_display`
    // replaces tabs and control characters, so this is exactly one line.
    let line = src.line(line_number);
    let (rendered, _) = text::render_display(line);
    out.push_str("  ");
    out.push_str(&rendered);
    out.push('\n');

    // Caret line: two spaces of indent, then spaces to the display column of
    // the span start, then carets covering the span on this line.
    let line_start = src.line_start(line_number);
    let start_in_line = span.start.saturating_sub(line_start).min(line.len());
    let end_in_line = span
        .end
        .saturating_sub(line_start)
        .min(line.len())
        .max(start_in_line);
    let caret_start = text::display_col(&line[..start_in_line]);
    let caret_end = text::display_col(&line[..end_in_line]);
    let caret_count = caret_end.saturating_sub(caret_start).max(1);

    out.push_str("  ");
    out.push_str(&" ".repeat(caret_start));
    out.push_str(&"^".repeat(caret_count));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ErrCode, SourceFile};

    fn src(text: &'static str) -> SourceFile {
        SourceFile::new("t.sh", text.as_bytes())
    }

    #[test]
    fn matches_the_documented_shape() {
        let src = SourceFile::new("t.sh", &b"one\ntwo\nif (echo hi; then\n"[..]);
        let at = src.bytes().windows(2).position(|w| w == b"(e").unwrap();
        let diag = Diagnostic::error(ErrCode::new(102), at..at + 1, "unclosed '('");
        assert_eq!(
            render_compact(&diag, &src),
            "bsh: 3:4: unclosed '(' [BSH-E0102]\n  if (echo hi; then\n     ^"
        );
    }

    #[test]
    fn tabs_expand_to_tab_stops() {
        // Byte column 5 (1-based), display column 7: the tab is four cells.
        let src = src("\tif (x\n");
        let diag = Diagnostic::error(ErrCode::new(102), 4..5, "unclosed '('");
        assert_eq!(
            render_compact(&diag, &src),
            "bsh: 1:5: unclosed '(' [BSH-E0102]\n      if (x\n         ^"
        );
    }

    #[test]
    fn wide_characters_occupy_two_columns() {
        // "if " is three cells, 世界 is four, so `(` is at display column 7.
        let src = src("if 世界(\n");
        let open = src.bytes().iter().position(|&b| b == b'(').unwrap();
        let diag = Diagnostic::error(ErrCode::new(102), open..open + 1, "unclosed '('");
        assert_eq!(
            render_compact(&diag, &src),
            "bsh: 1:10: unclosed '(' [BSH-E0102]\n  if 世界(\n         ^"
        );
    }

    #[test]
    fn zero_width_span_still_gets_a_caret() {
        let src = src("abc\n");
        let diag = Diagnostic::error(ErrCode::new(1), 3..3, "expected more");
        assert_eq!(
            render_compact(&diag, &src),
            "bsh: 1:4: expected more [BSH-E0001]\n  abc\n     ^"
        );
    }

    #[test]
    fn span_past_end_of_input_is_clamped() {
        let src = src("abc");
        let diag = Diagnostic::error(ErrCode::new(1), 10..20, "past the end");
        assert_eq!(
            render_compact(&diag, &src),
            "bsh: 1:4: past the end [BSH-E0001]\n  abc\n     ^"
        );
    }

    #[test]
    fn span_to_end_of_line_underlines_to_the_end() {
        let src = src("abcdef\n");
        let diag = Diagnostic::error(ErrCode::new(1), 2..100, "rest");
        assert_eq!(
            render_compact(&diag, &src),
            "bsh: 1:3: rest [BSH-E0001]\n  abcdef\n    ^^^^"
        );
    }

    #[test]
    fn multi_line_span_shows_its_first_line() {
        let src = src("if (echo\nhi; then\n");
        let diag = Diagnostic::error(ErrCode::new(102), 3..10, "unclosed '('");
        assert_eq!(
            render_compact(&diag, &src),
            "bsh: 1:4: unclosed '(' [BSH-E0102]\n  if (echo\n     ^^^^^"
        );
    }

    #[test]
    fn messages_cannot_add_lines_or_ansi() {
        let src = src("x");
        let diag = Diagnostic::error(ErrCode::new(1), 0..1, "a\nb\x1bc");
        let rendered = render_compact(&diag, &src);
        assert_eq!(rendered.lines().count(), 3);
        assert!(!rendered.contains('\u{1b}'));
        assert!(rendered.starts_with("bsh: 1:1: a b\u{FFFD}c [BSH-E0001]"));
    }

    #[test]
    fn empty_source_still_renders_a_caret() {
        let src = SourceFile::new("empty", &b""[..]);
        let diag = Diagnostic::error(ErrCode::new(1), 0..0, "empty");
        assert_eq!(
            render_compact(&diag, &src),
            "bsh: 1:1: empty [BSH-E0001]\n  \n  ^"
        );
    }

    #[test]
    fn invalid_utf8_before_the_span_does_not_shift_the_caret() {
        // 'a', one invalid byte, 'b', '(': the '(' is at byte 3 and display
        // column 3. A lossy conversion would have shifted every later column.
        let src = SourceFile::new("bad.sh", &b"a\xFFb(c\n"[..]);
        let open = src.bytes().iter().position(|&b| b == b'(').unwrap();
        assert_eq!(open, 3);
        let diag = Diagnostic::error(ErrCode::new(1), open..open + 1, "boom");
        assert_eq!(
            render_compact(&diag, &src),
            "bsh: 1:4: boom [BSH-E0001]\n  a\u{FFFD}b(c\n     ^"
        );
    }
}
