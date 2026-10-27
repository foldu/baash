//! miette 7 integration, behind the `fancy` feature.
//!
//! miette is used as a *renderer*, not as the error type (DESIGN.md §4.11).
//! That split is forced by a lifetime in miette's protocol: [`miette::Report`]
//! and `Box<dyn Diagnostic>` demand `'static`, while our spans borrow the
//! [`SourceFile`]. The way through is to implement [`miette::Diagnostic`] for a
//! borrowed view ([`DiagnosticView`]) and hand `&dyn Diagnostic` directly to a
//! report handler. No `Report` is ever constructed.
//!
//! # Byte offsets
//!
//! `miette::SourceCode` is `&str`-oriented, but shell input is bytes. This
//! module projects the source byte-for-byte: valid UTF-8 is borrowed as-is and
//! every invalid byte becomes a single ASCII `?`. A lossy conversion would not
//! work, because U+FFFD is *three* bytes in UTF-8: every span after an invalid
//! byte would slide. The projection keeps `text.len() == bytes.len()`, so
//! miette's byte offsets, columns and our [`Span`]s agree exactly.
//!
//! # Which handler
//!
//! miette 7.6 gates its graphical handler (`GraphicalReportHandler`) behind the
//! `fancy-base` cargo feature, which pulls `owo-colors`, `textwrap`,
//! `terminal_size` and the `supports-*` crates. The workspace declares miette
//! without those features and freezes `Cargo.lock` for parallel crate work, so
//! this adapter uses [`miette::NarratableReportHandler`] instead. It is
//! available unconditionally, consumes the same borrowed `&dyn Diagnostic` and
//! `SourceCode` protocol, and renders snippets, labels, help and the error code.
//! Switching to the graphical handler later is a one-line change in
//! [`render_fancy`] plus a feature declaration in the workspace; nothing in the
//! adapter depends on the choice.

use std::borrow::Cow;
use std::fmt;

use miette::Diagnostic as MietteDiagnostic;
use miette::{
    LabeledSpan, MietteError, NarratableReportHandler, Severity as MietteSeverity, SourceCode,
    SourceSpan, SpanContents,
};

use crate::text;
use crate::{Diagnostic, Severity, SourceFile, Span};

/// Render one diagnostic through miette's report handler.
///
/// The result is miette's plain-text (narratable) report: header, snippet with
/// context, labels, help and code. It always contains valid UTF-8 because the
/// source view projects invalid bytes; it ends with a newline, following
/// miette's own convention.
pub fn render_fancy(diag: &Diagnostic, src: &SourceFile) -> String {
    let view = DiagnosticView::new(src, diag);
    // If the workspace ever enables miette's `fancy-no-backtrace` feature, this
    // can become `GraphicalReportHandler::new_themed(GraphicalTheme::unicode_nocolor())`;
    // both handlers expose the same `render_report(&mut impl fmt::Write, &dyn
    // Diagnostic)` entry point, so nothing else changes.
    let handler = NarratableReportHandler::new()
        .with_context_lines(crate::CONTEXT_LINES)
        .without_cause_chain();
    let mut out = String::new();
    // The handler takes a borrowed `&dyn Diagnostic`; writing into a `String`
    // cannot fail.
    handler
        .render_report(&mut out, &view)
        .expect("writing a diagnostic into a String cannot fail");
    out
}

/// A borrowed `(&SourceFile, &Diagnostic)` that implements
/// [`miette::Diagnostic`].
///
/// Constructing one does not copy the source; it borrows the valid UTF-8 view
/// or allocates only the byte-for-byte projection when the source is invalid
/// UTF-8.
#[derive(Debug)]
pub struct DiagnosticView<'a> {
    src: &'a SourceFile,
    diag: &'a Diagnostic,
    source: SourceView<'a>,
}

impl<'a> DiagnosticView<'a> {
    /// Borrow a source file and a diagnostic.
    pub fn new(src: &'a SourceFile, diag: &'a Diagnostic) -> Self {
        Self {
            src,
            diag,
            source: SourceView::new(src),
        }
    }

    /// The borrowed source file.
    pub fn source_file(&self) -> &'a SourceFile {
        self.src
    }

    /// The borrowed diagnostic.
    pub fn diagnostic(&self) -> &'a Diagnostic {
        self.diag
    }

    /// The byte-for-byte `SourceCode` view used by miette.
    pub fn source_code_view(&self) -> &SourceView<'a> {
        &self.source
    }
}

impl fmt::Display for DiagnosticView<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The report header is one line; flatten a message that is not.
        let mut message = String::new();
        text::push_one_line(&mut message, self.diag.message.as_bytes());
        f.write_str(&message)
    }
}

impl std::error::Error for DiagnosticView<'_> {}

impl MietteDiagnostic for DiagnosticView<'_> {
    fn code<'a>(&'a self) -> Option<Box<dyn fmt::Display + 'a>> {
        Some(Box::new(self.diag.code))
    }

    fn severity(&self) -> Option<MietteSeverity> {
        Some(match self.diag.severity {
            Severity::Error => MietteSeverity::Error,
            Severity::Warning => MietteSeverity::Warning,
            Severity::Note => MietteSeverity::Advice,
        })
    }

    fn help<'a>(&'a self) -> Option<Box<dyn fmt::Display + 'a>> {
        self.diag
            .help
            .as_ref()
            .map(|help| Box::new(help.clone()) as Box<dyn fmt::Display + 'a>)
    }

    fn source_code(&self) -> Option<&dyn SourceCode> {
        Some(&self.source)
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        // The primary span is a label with no text: the message is already in
        // the report header. Secondary labels keep their text.
        let primary =
            LabeledSpan::new_primary_with_span(None, miette_span(&self.diag.span, self.src.len()));
        let secondary = self.diag.labels.iter().map(move |label| {
            LabeledSpan::new_with_span(
                Some(label.message.to_owned()),
                miette_span(&label.span, self.src.len()),
            )
        });
        Some(Box::new(std::iter::once(primary).chain(secondary)))
    }
}

/// A byte-for-byte UTF-8 view of a [`SourceFile`], for miette's `SourceCode`.
///
/// Valid UTF-8 is borrowed. Otherwise every invalid byte is replaced by `?`,
/// one byte for one byte, so all offsets in the original source remain valid
/// offsets into the view.
#[derive(Debug)]
pub struct SourceView<'a> {
    name: &'a str,
    text: Cow<'a, str>,
}

impl<'a> SourceView<'a> {
    /// Build a view over `src`.
    pub fn new(src: &'a SourceFile) -> Self {
        let text = match std::str::from_utf8(src.bytes()) {
            Ok(text) => Cow::Borrowed(text),
            Err(_) => Cow::Owned(project_bytes(src.bytes())),
        };
        Self {
            name: src.name(),
            text,
        }
    }

    /// The projected text. Always valid UTF-8; always the same byte length as
    /// the original source.
    pub fn text(&self) -> &str {
        self.text.as_ref()
    }

    /// The display name reported to miette.
    pub fn name(&self) -> &str {
        self.name
    }
}

impl SourceCode for SourceView<'_> {
    fn read_span<'a>(
        &'a self,
        span: &SourceSpan,
        context_lines_before: usize,
        context_lines_after: usize,
    ) -> Result<Box<dyn SpanContents<'a> + 'a>, MietteError> {
        let inner = <str as SourceCode>::read_span(
            self.text.as_ref(),
            span,
            context_lines_before,
            context_lines_after,
        )?;
        Ok(Box::new(NamedSpanContents {
            inner,
            name: self.name.to_owned(),
        }))
    }
}

/// Project `bytes` to valid UTF-8 without changing the byte length.
fn project_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        out.push_str(chunk.valid());
        for _ in chunk.invalid() {
            out.push('?');
        }
    }
    out
}

/// Clamp a span to the source and convert it to miette's byte-span type.
fn miette_span(span: &Span, len: usize) -> SourceSpan {
    let clamped = text::clamp_span(span, len);
    (clamped.start, clamped.end - clamped.start).into()
}

/// `SpanContents` that adds the source file name to the inner contents.
struct NamedSpanContents<'a> {
    inner: Box<dyn SpanContents<'a> + 'a>,
    name: String,
}

impl<'a> SpanContents<'a> for NamedSpanContents<'a> {
    fn data(&self) -> &'a [u8] {
        self.inner.data()
    }

    fn span(&self) -> &SourceSpan {
        self.inner.span()
    }

    fn line(&self) -> usize {
        self.inner.line()
    }

    fn column(&self) -> usize {
        self.inner.column()
    }

    fn line_count(&self) -> usize {
        self.inner.line_count()
    }

    fn name(&self) -> Option<&str> {
        Some(&self.name)
    }

    fn language(&self) -> Option<&str> {
        self.inner.language()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Diagnostic, ErrCode};

    fn invalid_source() -> SourceFile {
        // 'a', an invalid byte, 'b', 'c': the valid characters after the bad
        // byte are at original offsets 2 and 3.
        SourceFile::new("bad.sh", &b"a\xFFbc d\n"[..])
    }

    #[test]
    fn projection_preserves_byte_length() {
        let bytes = b"a\xFFb\xE4\xB8";
        let projected = project_bytes(bytes);
        assert_eq!(projected, "a?b??");
        assert_eq!(projected.len(), bytes.len());
    }

    #[test]
    fn invalid_utf8_is_borrowed_when_valid() {
        let src = SourceFile::new("ok.sh", &b"plain ascii"[..]);
        let view = SourceView::new(&src);
        assert!(matches!(view.text, Cow::Borrowed(_)));
        assert_eq!(view.text(), "plain ascii");
    }

    #[test]
    fn read_span_after_invalid_bytes_keeps_the_offset() {
        let src = invalid_source();
        let view = SourceView::new(&src);
        // Byte 3 is 'c' in the original source; the projection is "a?bc d".
        let contents = SourceCode::read_span(&view, &SourceSpan::new(3.into(), 1), 0, 0).unwrap();
        assert_eq!(contents.data(), b"c");
        assert_eq!(contents.line(), 0);
        assert_eq!(contents.column(), 3);
        assert_eq!(contents.name(), Some("bad.sh"));
    }

    #[test]
    fn adapter_maps_code_severity_help_and_labels() {
        let src = invalid_source();
        let diag = Diagnostic::warning(ErrCode::new(7), 3..4, "boom")
            .with_label(0..1, "opened here")
            .with_help("fix it");
        let view = DiagnosticView::new(&src, &diag);

        assert_eq!(
            MietteDiagnostic::code(&view).unwrap().to_string(),
            "BSH-E0007"
        );
        assert_eq!(
            MietteDiagnostic::severity(&view),
            Some(MietteSeverity::Warning)
        );
        assert_eq!(MietteDiagnostic::help(&view).unwrap().to_string(), "fix it");

        let labels: Vec<LabeledSpan> = MietteDiagnostic::labels(&view).unwrap().collect();
        assert_eq!(labels.len(), 2);
        assert!(labels[0].primary());
        assert_eq!(labels[0].offset(), 3);
        assert_eq!(labels[0].label(), None);
        assert!(!labels[1].primary());
        assert_eq!(labels[1].offset(), 0);
        assert_eq!(labels[1].label(), Some("opened here"));
    }

    #[test]
    fn source_code_is_the_projected_view() {
        let src = invalid_source();
        let diag = Diagnostic::error(ErrCode::new(1), 3..4, "boom");
        let view = DiagnosticView::new(&src, &diag);
        let source = MietteDiagnostic::source_code(&view).unwrap();
        let contents = source
            .read_span(&SourceSpan::new(3.into(), 1), 0, 0)
            .unwrap();
        assert_eq!(contents.data(), b"c");
    }

    #[test]
    fn miette_report_places_a_label_after_invalid_bytes() {
        let src = invalid_source();
        let diag = Diagnostic::error(ErrCode::new(1), 3..4, "boom");
        let rendered = render_fancy(&diag, &src);
        // 1-based column 4 is 'c'; a lossy conversion would have said column 3.
        assert!(
            rendered.contains("column 4"),
            "expected column 4 in:\n{rendered}"
        );
        assert!(rendered.contains("bad.sh"));
        assert!(rendered.contains("BSH-E0001"));
        assert!(rendered.contains("a?bc d"));
    }

    #[test]
    fn out_of_bounds_and_empty_spans_are_clamped() {
        let src = SourceFile::new("empty.sh", &b""[..]);
        let diag = Diagnostic::error(ErrCode::new(1), 100..200, "past the end");
        let view = DiagnosticView::new(&src, &diag);
        let labels: Vec<LabeledSpan> = MietteDiagnostic::labels(&view).unwrap().collect();
        assert_eq!(labels[0].offset(), 0);
        assert_eq!(labels[0].len(), 0);
        // Rendering must not panic on an empty source.
        let rendered = render_fancy(&diag, &src);
        assert!(rendered.contains("BSH-E0001"));
    }
}
