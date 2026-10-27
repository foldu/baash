//! Diagnostic IR for ßhell: what went wrong, where, and how to fix it.
//!
//! This crate owns the IR and the renderers; `bsh-syntax` and friends produce
//! [`Diagnostic`]s, and the caller decides how they look — compact at the
//! prompt ([`render_compact`]), a multi-line report for `-n`/lint
//! ([`render_report`]), one greppable line ([`Diagnostic::render_line`],
//! [`render_line_with_help`]), or JSON for editors ([`render_json`]). The
//! optional `fancy` feature exposes a miette 7 adapter in the `fancy` module
//! when miette is available. See `DESIGN.md` §4.10 and §4.11.
//!
//! Shell text is bytes. Every renderer is byte-oriented and substitutes a
//! visible replacement for invalid UTF-8 and control characters, so an excerpt
//! cannot corrupt the terminal or shift a caret.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::fmt;
use std::ops::Range;
use std::sync::Arc;

mod compact;
mod json;
mod report;
mod text;

pub use compact::render_compact;
pub use json::{json_escape, render_json};
pub use report::{CONTEXT_LINES, MAX_DIAGNOSTICS, render_report};
pub use text::TAB_WIDTH;

#[cfg(feature = "fancy")]
pub mod fancy;

/// Byte offsets into a [`SourceFile`].
pub type Span = Range<usize>;

/// A source file, owned so a diagnostic can be rendered long after parsing.
///
/// Interactive shells accumulate `eval`'d code, sourced files and redefined
/// functions; a diagnostic therefore may not borrow a parse arena that has
/// already moved on. `Arc` keeps cloning cheap when many diagnostics share one
/// file.
#[derive(Clone, Debug)]
pub struct SourceFile {
    name: String,
    bytes: Arc<[u8]>,
}

impl SourceFile {
    /// Wrap owned source bytes under a display name (a path, `<stdin>`, `-c`).
    pub fn new(name: impl Into<String>, bytes: impl Into<Arc<[u8]>>) -> Self {
        Self {
            name: name.into(),
            bytes: bytes.into(),
        }
    }

    /// The display name used as the location prefix.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Raw bytes. Shell text is bytes, not `str`.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Byte length.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the file is empty.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// 1-based line and 1-based byte column for a byte offset.
    pub fn line_col(&self, offset: usize) -> (u32, u32) {
        let end = offset.min(self.bytes.len());
        let mut line = 1u32;
        let mut line_start = 0usize;
        for (i, &b) in self.bytes[..end].iter().enumerate() {
            if b == b'\n' {
                line += 1;
                line_start = i + 1;
            }
        }
        (line, (end - line_start) as u32 + 1)
    }

    /// The text of a 1-based `line`, without its trailing newline.
    pub fn line(&self, line: u32) -> &[u8] {
        let mut current = 1u32;
        let mut start = 0usize;
        while current < line {
            match self.bytes[start..].iter().position(|&b| b == b'\n') {
                Some(nl) => {
                    start += nl + 1;
                    current += 1;
                }
                None => return &[],
            }
        }
        let rest = &self.bytes[start..];
        let end = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
        &rest[..end]
    }

    /// Byte offset at which a 1-based `line` starts.
    ///
    /// Clamped to [`len`](Self::len) when `line` is past the end, which makes
    /// it safe to pair with [`line`](Self::line) for an off-by-one cursor.
    pub fn line_start(&self, line: u32) -> usize {
        if line <= 1 {
            return 0;
        }
        let mut current = 1u32;
        let mut start = 0usize;
        while current < line {
            match self.bytes[start..].iter().position(|&b| b == b'\n') {
                Some(newline) => {
                    start += newline + 1;
                    current += 1;
                }
                None => return self.bytes.len(),
            }
        }
        start
    }

    /// Number of logical lines, counting a trailing newline as the start of an
    /// empty final line.
    ///
    /// This matches [`line_col`](Self::line_col): for `b"a\n"` the count is 2
    /// and `line_col(2)` is `(2, 1)`. An empty file still has one (empty) line.
    pub fn line_count(&self) -> u32 {
        let mut count = 1u32;
        for &byte in self.bytes.iter() {
            if byte == b'\n' {
                count += 1;
            }
        }
        count
    }
}

/// A stable, searchable diagnostic code, rendered as `BSH-E0102`.
///
/// The code is stable and documented in the error catalog; the message text is
/// not. Anything matching on stderr should match the code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ErrCode(pub u16);

impl ErrCode {
    /// Construct a code.
    pub const fn new(n: u16) -> Self {
        Self(n)
    }
}

impl fmt::Display for ErrCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BSH-E{:04}", self.0)
    }
}

/// How loudly to report a diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// The input is rejected.
    Error,
    /// Suspicious but accepted, e.g. a bashism under `--posix`.
    Warning,
    /// Context attached to another diagnostic.
    Note,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        }
    }
}

/// A message attached to a span.
#[derive(Clone, Debug)]
pub struct Label {
    /// Where the label points.
    pub span: Span,
    /// Short text; `'static` so labels cost nothing to copy.
    pub message: &'static str,
}

/// One thing that is wrong, with everything needed to explain it.
#[derive(Clone, Debug)]
pub struct Diagnostic {
    /// Stable code.
    pub code: ErrCode,
    /// Severity.
    pub severity: Severity,
    /// The primary span.
    pub span: Span,
    /// One-line summary.
    pub message: String,
    /// Secondary spans, e.g. "opened here".
    pub labels: Vec<Label>,
    /// Actionable advice.
    pub help: Option<String>,
}

impl Diagnostic {
    /// An error at `span`.
    pub fn error(code: ErrCode, span: Span, message: impl Into<String>) -> Self {
        Self {
            code,
            severity: Severity::Error,
            span,
            message: message.into(),
            labels: Vec::new(),
            help: None,
        }
    }

    /// A warning at `span`.
    pub fn warning(code: ErrCode, span: Span, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            ..Self::error(code, span, message)
        }
    }

    /// Attach a secondary span.
    #[must_use]
    pub fn with_label(mut self, span: Span, message: &'static str) -> Self {
        self.labels.push(Label { span, message });
        self
    }

    /// Attach actionable advice.
    #[must_use]
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    /// One line, greppable, for non-tty output and logs:
    /// `file.sh:3:5: error[BSH-E0102]: unclosed '('`.
    ///
    /// The result is always exactly one line: newlines and tabs in the name or
    /// message collapse to spaces, and control characters (including ANSI
    /// escapes) become U+FFFD. That matters because scripts match this format.
    pub fn render_line(&self, src: &SourceFile) -> String {
        let (line, col) = src.line_col(self.span.start);
        let mut out = String::new();
        text::push_one_line(&mut out, src.name().as_bytes());
        out.push(':');
        out.push_str(&line.to_string());
        out.push(':');
        out.push_str(&col.to_string());
        out.push_str(": ");
        out.push_str(self.severity.as_str());
        out.push('[');
        out.push_str(&self.code.to_string());
        out.push_str("]: ");
        text::push_one_line(&mut out, self.message.as_bytes());
        out
    }

    /// One line with advice appended when present:
    /// `file.sh:3:5: error[BSH-E0102]: …; help: …`.
    ///
    /// Identical to [`render_line`](Self::render_line) when there is no help.
    /// This variant is for non-tty stderr and logs where a script may want the
    /// advice in the same record, but must still be able to match on one line:
    /// newlines and tabs collapse to spaces, and control characters (including
    /// ANSI escapes) become U+FFFD.
    pub fn render_line_with_help(&self, src: &SourceFile) -> String {
        render_line_with_help(self, src)
    }
}

/// One line, greppable, with `help` appended only when present.
///
/// See [`Diagnostic::render_line_with_help`]. The free function is provided for
/// callers that hold a diagnostic behind a reference and prefer not to use
/// method syntax.
pub fn render_line_with_help(diag: &Diagnostic, src: &SourceFile) -> String {
    let mut out = diag.render_line(src);
    if let Some(help) = &diag.help {
        out.push_str("; help: ");
        text::push_one_line(&mut out, help.as_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src() -> SourceFile {
        SourceFile::new("t.sh", &b"one\ntwo\n\tthree\n"[..])
    }

    #[test]
    fn line_col_is_one_based() {
        assert_eq!(src().line_col(0), (1, 1));
        assert_eq!(src().line_col(4), (2, 1));
        assert_eq!(src().line_col(9), (3, 2)); // after the tab
    }

    #[test]
    fn lines_exclude_newlines() {
        assert_eq!(src().line(1), b"one");
        assert_eq!(src().line(3), b"\tthree");
        assert_eq!(src().line(9), b"");
    }

    #[test]
    fn error_code_renders_padded() {
        assert_eq!(ErrCode::new(1).to_string(), "BSH-E0001");
        assert_eq!(ErrCode::new(102).to_string(), "BSH-E0102");
    }

    #[test]
    fn render_line_has_location_and_code() {
        let d = Diagnostic::error(ErrCode::new(102), 5..8, "unexpected `then`");
        assert_eq!(
            d.render_line(&src()),
            "t.sh:2:2: error[BSH-E0102]: unexpected `then`"
        );
    }

    #[test]
    fn labels_and_help_are_collected() {
        let d = Diagnostic::error(ErrCode::new(7), 0..1, "boom")
            .with_label(3..4, "opened here")
            .with_help("close it");
        assert_eq!(d.labels.len(), 1);
        assert_eq!(d.labels[0].span, 3..4);
        assert_eq!(d.help.as_deref(), Some("close it"));
    }

    #[test]
    fn line_starts_and_line_count_agree_with_line_col() {
        assert_eq!(src().line_start(1), 0);
        assert_eq!(src().line_start(2), 4);
        assert_eq!(src().line_start(3), 8);
        assert_eq!(src().line_start(4), 15);
        assert_eq!(src().line_start(99), src().len());
        assert_eq!(src().line_count(), 4);
        assert_eq!(SourceFile::new("e", &b""[..]).line_count(), 1);
    }

    #[test]
    fn one_line_with_help_matches_plain_when_absent() {
        let src = src();
        let plain = Diagnostic::error(ErrCode::new(102), 5..8, "unexpected `then`");
        assert_eq!(
            plain.render_line_with_help(&src),
            plain.render_line(&src),
            "the variant must not change the no-help format"
        );
    }

    #[test]
    fn one_line_with_help_appends_advice() {
        let src = src();
        let d = Diagnostic::error(ErrCode::new(7), 0..1, "boom").with_help("close it");
        assert_eq!(
            d.render_line_with_help(&src),
            "t.sh:1:1: error[BSH-E0007]: boom; help: close it"
        );
        assert_eq!(
            render_line_with_help(&d, &src),
            d.render_line_with_help(&src)
        );
    }

    #[test]
    fn one_line_never_emits_a_newline_or_ansi() {
        let src = src();
        let d = Diagnostic::error(ErrCode::new(1), 0..1, "a\nb\x1bc").with_help("h\ni\x1b[0m");
        let plain = d.render_line(&src);
        assert_eq!(plain.lines().count(), 1);
        assert!(!plain.contains('\u{1b}'));
        let rendered = d.render_line_with_help(&src);
        assert_eq!(rendered.lines().count(), 1);
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\n'));
    }

    #[test]
    fn every_renderer_agrees_on_the_same_diagnostic() {
        let src = SourceFile::new("t.sh", &b"if (echo hi; then\n"[..]);
        let d = Diagnostic::error(ErrCode::new(102), 3..4, "unclosed '('")
            .with_label(0..2, "opened here")
            .with_help("add ')'");

        let compact = render_compact(&d, &src);
        let report = render_report(std::slice::from_ref(&d), &src);
        let line = d.render_line_with_help(&src);
        let json = render_json(std::slice::from_ref(&d), &src);

        for rendered in [&compact, &report, &line, &json] {
            assert!(rendered.contains("BSH-E0102"), "{rendered}");
            assert!(!rendered.contains('\u{1b}'), "{rendered}");
        }
        assert_eq!(compact.lines().count(), 3);
        assert_eq!(line.lines().count(), 1);
        assert!(report.contains("help: add ')'"));
        assert!(json.starts_with('['));
        assert!(json.contains("\"code\": \"BSH-E0102\""));
    }
}
