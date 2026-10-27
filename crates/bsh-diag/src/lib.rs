//! Diagnostic IR for ßhell: what went wrong, where, and how to fix it.
//!
//! This crate is deliberately dependency-free and knows nothing about
//! rendering backends. The front end produces [`Diagnostic`]s; a renderer
//! decides how they look — compact at the prompt, graphical for `-n`/`lint`,
//! JSON for editors. See `DESIGN.md` §4.10 and §4.11.
//!
//! Not yet implemented: the compact and graphical renderers, labels with
//! secondary spans, machine-applicable fixes, and the miette adapter.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::fmt;
use std::ops::Range;
use std::sync::Arc;

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
    pub fn render_line(&self, src: &SourceFile) -> String {
        let (line, col) = src.line_col(self.span.start);
        format!(
            "{}:{line}:{col}: {}[{}]: {}",
            src.name(),
            self.severity.as_str(),
            self.code,
            self.message
        )
    }
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
}
