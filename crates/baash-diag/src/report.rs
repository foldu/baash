//! The multi-line report renderer used by `baash -n`, `lint` and `fmt`.
//!
//! This is the "full report" of DESIGN.md §4.10: a header, a source excerpt
//! with a line-number gutter, an underline for the primary span, one excerpt
//! per secondary label (so `"opened here"` gets its own line), and finally
//! `help:`. It is hand-written rather than miette's graphical theme so that the
//! crate stays renderable without the optional dependency.
//!
//! Output is bounded in two independent ways:
//!
//! * [`CONTEXT_LINES`] lines of context are shown before and after the primary
//!   span. The span's own lines are always shown, however long the span is.
//! * [`MAX_DIAGNOSTICS`] diagnostics are rendered per call. The remainder is
//!   reported as a single `note:` line, never silently dropped.
//!
//! The exact layout is a snapshot surface: changing a message or a glyph is a
//! breaking change for anyone matching `-n` output, which is why the diagnostic
//! *code* is the stable part.

use crate::text;
use crate::{Diagnostic, SourceFile};

/// Lines of context shown before and after a primary span.
///
/// Three is enough to see the enclosing `if`/`for`/function without turning a
/// lint run into a full source listing.
pub const CONTEXT_LINES: usize = 3;

/// Maximum number of diagnostics [`render_report`] will render.
///
/// A single unterminated quote can produce a cascade of follow-on errors; the
/// renderer caps the cascade and says how many were hidden. The cap is part of
/// the output contract, not an implementation detail.
pub const MAX_DIAGNOSTICS: usize = 32;

/// Render `diags` against `src` as a multi-line report.
///
/// Diagnostics are separated by one blank line. The result has no trailing
/// newline. At most [`MAX_DIAGNOSTICS`] diagnostics are rendered; if more were
/// supplied, a final `note:` line reports the count that was suppressed.
pub fn render_report(diags: &[Diagnostic], src: &SourceFile) -> String {
    let shown = diags.len().min(MAX_DIAGNOSTICS);
    let mut blocks: Vec<String> = diags[..shown]
        .iter()
        .map(|diag| render_one(diag, src))
        .collect();
    if diags.len() > shown {
        let hidden = diags.len() - shown;
        blocks.push(format!(
            "note: {hidden} more diagnostic{} not shown (cap {MAX_DIAGNOSTICS})",
            if hidden == 1 { "" } else { "s" }
        ));
    }
    blocks.join("\n\n")
}

/// Render one diagnostic block, without a trailing newline.
fn render_one(diag: &Diagnostic, src: &SourceFile) -> String {
    let mut out = String::new();

    // Header: the same machine-friendly shape as the one-line renderer.
    // `render_line` already guarantees a single, ANSI-free line.
    out.push_str(&diag.render_line(src));
    out.push('\n');

    let span = text::clamp_span(&diag.span, src.len());
    let start_line = src.line_col(span.start).0;
    let raw_end_line = src.line_col(span.end).0;
    // A span that ends exactly at the start of a line (typically because it
    // included the previous newline) should not paint a phantom caret on the
    // following, empty line.
    let end_line =
        if span.end > span.start && raw_end_line > 1 && span.end == src.line_start(raw_end_line) {
            raw_end_line - 1
        } else {
            raw_end_line
        };

    let first = start_line.saturating_sub(CONTEXT_LINES as u32).max(1);
    let last = (end_line + CONTEXT_LINES as u32).min(context_limit(src, end_line));

    let label_lines: Vec<u32> = diag
        .labels
        .iter()
        .map(|label| src.line_col(label.span.start).0)
        .collect();
    let gutter_line = label_lines.iter().copied().max().unwrap_or(last).max(last);
    let gutter = digit_count(gutter_line);

    for line_number in first..=last {
        push_excerpt(&mut out, src, line_number, gutter);
        if (start_line..=end_line).contains(&line_number) {
            let line = src.line(line_number);
            let line_start = src.line_start(line_number);
            let from = if line_number == start_line {
                span.start.saturating_sub(line_start)
            } else {
                0
            }
            .min(line.len());
            let to = if line_number == end_line {
                span.end.saturating_sub(line_start)
            } else {
                line.len()
            }
            .min(line.len())
            .max(from);
            let start_col = text::display_col(&line[..from]);
            let end_col = text::display_col(&line[..to]);
            push_underline(
                &mut out,
                gutter,
                start_col,
                end_col.saturating_sub(start_col).max(1),
                None,
            );
        }
    }

    // Secondary labels each get their own excerpt. Repeating a line that is
    // also in the context window is intentional: "opened here" reads as a
    // separate, self-contained annotation.
    for (label, &label_line) in diag.labels.iter().zip(label_lines.iter()) {
        push_excerpt(&mut out, src, label_line, gutter);
        let line = src.line(label_line);
        let line_start = src.line_start(label_line);
        let label_span = text::clamp_span(&label.span, src.len());
        let label_end_line = src.line_col(label_span.end).0;
        let from = label_span.start.saturating_sub(line_start).min(line.len());
        let to = if label_end_line > label_line || label_span.end >= line_start + line.len() {
            line.len()
        } else {
            label_span.end.saturating_sub(line_start).min(line.len())
        }
        .max(from);
        let start_col = text::display_col(&line[..from]);
        let end_col = text::display_col(&line[..to]);
        let message = if label.message.is_empty() {
            None
        } else {
            Some(label.message)
        };
        push_underline(
            &mut out,
            gutter,
            start_col,
            end_col.saturating_sub(start_col).max(1),
            message,
        );
    }

    if let Some(help) = &diag.help {
        push_help(&mut out, help);
    }

    let len = out.trim_end_matches('\n').len();
    out.truncate(len);
    out
}

/// The largest line number worth showing as trailing context.
///
/// A file ending in a newline has a final, empty logical line (see
/// [`SourceFile::line_count`]). It is noise after a normal span, but it must
/// still be shown when the span itself starts there.
fn context_limit(src: &SourceFile, end_line: u32) -> u32 {
    let count = src.line_count();
    let last_visible = if count > 1 && src.bytes().ends_with(b"\n") {
        count - 1
    } else {
        count
    };
    last_visible.max(end_line)
}

fn digit_count(n: u32) -> usize {
    n.to_string().len().max(1)
}

/// `  N | source text`, with tabs expanded and non-printables replaced.
fn push_excerpt(out: &mut String, src: &SourceFile, line_number: u32, gutter: usize) {
    let (rendered, _) = text::render_display(src.line(line_number));
    out.push_str("  ");
    out.push_str(&format!("{line_number:>gutter$}"));
    out.push_str(" | ");
    out.push_str(&rendered);
    out.push('\n');
}

/// `    | ^^^^ message`, aligned under the span by display column.
fn push_underline(
    out: &mut String,
    gutter: usize,
    start_col: usize,
    count: usize,
    message: Option<&str>,
) {
    out.push_str("  ");
    out.push_str(&" ".repeat(gutter));
    out.push_str(" | ");
    out.push_str(&" ".repeat(start_col));
    out.push_str(&"^".repeat(count));
    if let Some(message) = message {
        out.push(' ');
        text::push_one_line(out, message.as_bytes());
    }
    out.push('\n');
}

/// `  help: text`, with continuation lines aligned after the prefix.
fn push_help(out: &mut String, help: &str) {
    const PREFIX: &str = "  help: ";
    let mut first = true;
    for line in help.split('\n') {
        if first {
            out.push_str(PREFIX);
            first = false;
        } else {
            out.push('\n');
            out.push_str(&" ".repeat(PREFIX.len()));
        }
        text::push_one_line(out, line.as_bytes());
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ErrCode, SourceFile};

    fn open_at(src: &SourceFile) -> usize {
        src.bytes().iter().position(|&b| b == b'(').unwrap()
    }

    #[test]
    fn empty_input_renders_nothing() {
        let src = SourceFile::new("e.sh", &b"x\n"[..]);
        assert_eq!(render_report(&[], &src), "");
    }

    #[test]
    fn full_report_has_header_gutter_labels_and_help() {
        let src = SourceFile::new(
            "t.sh",
            &b"one\ntwo\nif (echo hi; then\n  echo oops\nfi\n"[..],
        );
        let at = open_at(&src);
        let diag = Diagnostic::error(ErrCode::new(102), at..at + 1, "unclosed '('")
            .with_label(0..3, "opened here")
            .with_help("add ')' before 'then'");
        assert_eq!(
            render_report(&[diag], &src),
            concat!(
                "t.sh:3:4: error[BAASH-E0102]: unclosed '('\n",
                "  1 | one\n",
                "  2 | two\n",
                "  3 | if (echo hi; then\n",
                "    |    ^\n",
                "  4 |   echo oops\n",
                "  5 | fi\n",
                "  1 | one\n",
                "    | ^^^ opened here\n",
                "  help: add ')' before 'then'",
            )
        );
    }

    #[test]
    fn context_is_capped_around_the_primary_span() {
        let mut text = String::new();
        for i in 1..=10 {
            text.push_str(&format!("line {i}\n"));
        }
        // The `5` in "line 5".
        let at = text.find("line 5").unwrap() + 5;
        let src = SourceFile::new("long.sh", text.into_bytes());
        let diag = Diagnostic::error(ErrCode::new(1), at..at + 1, "boom");
        let rendered = render_report(&[diag], &src);
        assert!(rendered.contains("  2 | line 2"));
        assert!(rendered.contains("  8 | line 8"));
        assert!(!rendered.contains("  1 | line 1"));
        assert!(!rendered.contains("  9 | line 9"));
    }

    #[test]
    fn multi_line_span_is_underlined_on_every_line() {
        let src = SourceFile::new("m.sh", &b"if (echo\nhi; then\n"[..]);
        let diag = Diagnostic::error(ErrCode::new(102), 3..10, "unclosed '('");
        assert_eq!(
            render_report(&[diag], &src),
            concat!(
                "m.sh:1:4: error[BAASH-E0102]: unclosed '('\n",
                "  1 | if (echo\n",
                "    |    ^^^^^\n",
                "  2 | hi; then\n",
                "    | ^",
            )
        );
    }

    #[test]
    fn tabs_and_wide_characters_align_the_underline() {
        let src = SourceFile::new("w.sh", &b"\tif \xe4\xb8\x96(\n"[..]);
        let at = open_at(&src);
        let diag = Diagnostic::error(ErrCode::new(1), at..at + 1, "boom");
        let rendered = render_report(&[diag], &src);
        let lines: Vec<&str> = rendered.lines().collect();
        // "  N | " then the tab expands to four spaces: 2 + 1 + 3 + 4 = 10.
        assert_eq!(lines[1], "  1 |     if 世(");
        // The `(` is at display column 9, after the three-cell gutter prefix.
        assert_eq!(lines[2], "    |          ^");
    }

    #[test]
    fn zero_width_span_still_gets_a_caret() {
        let src = SourceFile::new("z.sh", &b"abc\n"[..]);
        let diag = Diagnostic::error(ErrCode::new(1), 3..3, "expected more");
        let rendered = render_report(&[diag], &src);
        assert!(rendered.contains("    |    ^"));
    }

    #[test]
    fn the_diagnostic_cap_is_reported_not_silent() {
        let src = SourceFile::new("many.sh", &b"x\n"[..]);
        let diags: Vec<Diagnostic> = (0..MAX_DIAGNOSTICS + 8)
            .map(|i| Diagnostic::error(ErrCode::new(i as u16), 0..1, "boom"))
            .collect();
        let rendered = render_report(&diags, &src);
        assert_eq!(rendered.matches("error[BAASH-E").count(), MAX_DIAGNOSTICS);
        assert!(rendered.ends_with(&format!(
            "note: 8 more diagnostics not shown (cap {MAX_DIAGNOSTICS})"
        )));
    }

    #[test]
    fn help_continuation_lines_are_indented() {
        let src = SourceFile::new("h.sh", &b"x\n"[..]);
        let diag =
            Diagnostic::error(ErrCode::new(1), 0..1, "boom").with_help("first line\nsecond line");
        let rendered = render_report(&[diag], &src);
        assert!(rendered.contains("  help: first line\n        second line"));
    }

    #[test]
    fn a_span_ending_at_a_newline_does_not_paint_the_empty_line() {
        let src = SourceFile::new("n.sh", &b"abc\nnext\n"[..]);
        let diag = Diagnostic::error(ErrCode::new(1), 0..4, "whole first line");
        let rendered = render_report(&[diag], &src);
        assert!(rendered.contains("  1 | abc\n    | ^^^\n"));
        assert!(!rendered.contains("  2 | next\n    | ^\n"));
    }
}
