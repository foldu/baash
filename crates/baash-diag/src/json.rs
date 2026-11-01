//! The deterministic JSON renderer for editors and `--error-format=json`.
//!
//! Hand-written rather than serde-derived: the IR is tiny, the output shape is
//! a compatibility surface, and the crate must not grow a dependency for it
//! (DESIGN.md §4.11). The key order is fixed:
//!
//! ```json
//! [
//!   {
//!     "file": "t.sh",
//!     "line": 3,
//!     "column": 5,
//!     "code": "BAASH-E0102",
//!     "severity": "error",
//!     "message": "unclosed '('",
//!     "help": "add ')'",
//!     "labels": [
//!       { "line": 1, "column": 1, "length": 1, "message": "opened here" }
//!     ]
//!   }
//! ]
//! ```
//!
//! `line` and `column` are 1-based; `column` and `length` are **byte** units,
//! matching [`Span`](crate::Span) and miette's `SourceSpan`. Invalid UTF-8 in
//! any byte string is replaced with U+FFFD, which keeps the document valid
//! UTF-8 and valid JSON.

use crate::text;
use crate::{Diagnostic, SourceFile};

/// Render `diags` as a JSON array.
///
/// The result is pretty-printed with two-space indentation, has no trailing
/// newline, and is deterministic: the same input always produces byte-for-byte
/// identical output. `help` is `null` when absent; `labels` is `[]` when there
/// are no secondary spans.
pub fn render_json(diags: &[Diagnostic], src: &SourceFile) -> String {
    let mut out = String::new();
    out.push('[');
    if !diags.is_empty() {
        out.push('\n');
        for (index, diag) in diags.iter().enumerate() {
            if index > 0 {
                out.push_str(",\n");
            }
            push_diagnostic(&mut out, diag, src);
        }
        out.push('\n');
    }
    out.push(']');
    out
}

/// Escape arbitrary bytes as the body of a JSON string (without the quotes).
///
/// Quotes, backslashes and control characters are escaped. Bytes that are not
/// part of a valid UTF-8 sequence are replaced by U+FFFD: the byte length is
/// allowed to change here because JSON is a text interchange format, and the
/// replacement keeps the result valid UTF-8 and valid JSON.
pub fn json_escape(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            push_escaped_char(&mut out, c);
        }
        for _ in chunk.invalid() {
            out.push('\u{FFFD}');
        }
    }
    out
}

/// Append `bytes` as a quoted JSON string.
fn push_string(out: &mut String, bytes: &[u8]) {
    out.push('"');
    out.push_str(&json_escape(bytes));
    out.push('"');
}

fn push_escaped_char(out: &mut String, c: char) {
    match c {
        '"' => out.push_str("\\\""),
        '\\' => out.push_str("\\\\"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\u{08}' => out.push_str("\\b"),
        '\u{0C}' => out.push_str("\\f"),
        // Remaining C0 controls (and DEL, for terminal safety) use \u escapes.
        c if u32::from(c) < 0x20 || c == '\u{7F}' => {
            out.push_str(&format!("\\u{:04x}", u32::from(c)));
        }
        c => out.push(c),
    }
}

fn push_diagnostic(out: &mut String, diag: &Diagnostic, src: &SourceFile) {
    let (line, column) = src.line_col(diag.span.start);

    out.push_str("  {\n");
    out.push_str("    \"file\": ");
    push_string(out, src.name().as_bytes());
    out.push_str(",\n");
    out.push_str(&format!("    \"line\": {line},\n"));
    out.push_str(&format!("    \"column\": {column},\n"));
    out.push_str("    \"code\": ");
    push_string(out, diag.code.to_string().as_bytes());
    out.push_str(",\n");
    out.push_str("    \"severity\": ");
    push_string(out, diag.severity.as_str().as_bytes());
    out.push_str(",\n");
    out.push_str("    \"message\": ");
    push_string(out, diag.message.as_bytes());
    out.push_str(",\n");
    out.push_str("    \"help\": ");
    match &diag.help {
        Some(help) => push_string(out, help.as_bytes()),
        None => out.push_str("null"),
    }
    out.push_str(",\n");

    out.push_str("    \"labels\": [");
    if diag.labels.is_empty() {
        out.push_str("]\n");
    } else {
        out.push('\n');
        for (index, label) in diag.labels.iter().enumerate() {
            if index > 0 {
                out.push_str(",\n");
            }
            let span = text::clamp_span(&label.span, src.len());
            let (label_line, label_column) = src.line_col(span.start);
            out.push_str("      {\n");
            out.push_str(&format!("        \"line\": {label_line},\n"));
            out.push_str(&format!("        \"column\": {label_column},\n"));
            out.push_str(&format!("        \"length\": {},\n", span.len()));
            out.push_str("        \"message\": ");
            push_string(out, label.message.as_bytes());
            out.push('\n');
            out.push_str("      }");
        }
        out.push_str("\n    ]\n");
    }
    out.push_str("  }");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ErrCode, SourceFile};

    fn sample() -> (SourceFile, Diagnostic) {
        let src = SourceFile::new("t.sh", &b"one\ntwo\nif (echo hi; then\n"[..]);
        let at = src.bytes().iter().position(|&b| b == b'(').unwrap();
        let diag = Diagnostic::error(ErrCode::new(102), at..at + 1, "unclosed '('")
            .with_label(0..3, "opened here")
            .with_help("add ')' before 'then'");
        (src, diag)
    }

    #[test]
    fn exact_output_is_stable() {
        let (src, diag) = sample();
        assert_eq!(
            render_json(&[diag], &src),
            concat!(
                "[\n",
                "  {\n",
                "    \"file\": \"t.sh\",\n",
                "    \"line\": 3,\n",
                "    \"column\": 4,\n",
                "    \"code\": \"BAASH-E0102\",\n",
                "    \"severity\": \"error\",\n",
                "    \"message\": \"unclosed '('\",\n",
                "    \"help\": \"add ')' before 'then'\",\n",
                "    \"labels\": [\n",
                "      {\n",
                "        \"line\": 1,\n",
                "        \"column\": 1,\n",
                "        \"length\": 3,\n",
                "        \"message\": \"opened here\"\n",
                "      }\n",
                "    ]\n",
                "  }\n",
                "]"
            )
        );
    }

    #[test]
    fn empty_input_and_empty_labels() {
        let src = SourceFile::new("e.sh", &b"x"[..]);
        assert_eq!(render_json(&[], &src), "[]");
        let diag = Diagnostic::warning(ErrCode::new(7), 0..1, "hmm");
        let rendered = render_json(&[diag], &src);
        assert!(rendered.contains("\"severity\": \"warning\""));
        assert!(rendered.contains("\"help\": null"));
        assert!(rendered.contains("\"labels\": []"));
        assert!(valid_json(&rendered));
    }

    #[test]
    fn escapes_quotes_backslashes_and_controls() {
        let escaped = json_escape(b"q:\" b:\\ n:\n t:\t c:\x01 del:\x7f");
        assert_eq!(escaped, "q:\\\" b:\\\\ n:\\n t:\\t c:\\u0001 del:\\u007f");
    }

    #[test]
    fn invalid_utf8_becomes_replacement_characters() {
        let escaped = json_escape(b"a\xFFb\xFE");
        assert_eq!(escaped, "a\u{FFFD}b\u{FFFD}");
    }

    #[test]
    fn line_and_column_are_byte_offsets_even_past_invalid_utf8() {
        let src = SourceFile::new("bad.sh", &b"a\xFFb(c\n"[..]);
        let open = src.bytes().iter().position(|&b| b == b'(').unwrap();
        let diag = Diagnostic::error(ErrCode::new(1), open..open + 1, "boom")
            .with_label(0..1, "opened here");
        let rendered = render_json(&[diag], &src);
        assert!(rendered.contains("\"line\": 1"));
        assert!(rendered.contains("\"column\": 4"));
        assert!(rendered.contains("\"length\": 1"));
        assert!(valid_json(&rendered));
    }

    #[test]
    fn document_with_hostile_strings_is_valid_json() {
        let src = SourceFile::new("weird\"name\\.sh", &b"x"[..]);
        let diag = Diagnostic::error(ErrCode::new(1), 0..1, "line\nbreak \x01 \"quoted\"")
            .with_label(0..1, "a\\b")
            .with_help("help\rwith\ttabs");
        let rendered = render_json(&[diag], &src);
        assert!(valid_json(&rendered));
        assert!(!rendered.contains('\u{1b}'));
    }

    /// A deliberately small JSON validator: enough to prove the renderer emits
    /// well-formed JSON, not a general-purpose parser.
    fn valid_json(text: &str) -> bool {
        let mut parser = Parser {
            bytes: text.as_bytes(),
            at: 0,
        };
        parser.value() && {
            parser.ws();
            parser.at == parser.bytes.len()
        }
    }

    struct Parser<'a> {
        bytes: &'a [u8],
        at: usize,
    }

    impl Parser<'_> {
        fn ws(&mut self) {
            while matches!(self.bytes.get(self.at), Some(b' ' | b'\n' | b'\r' | b'\t')) {
                self.at += 1;
            }
        }

        fn value(&mut self) -> bool {
            self.ws();
            match self.bytes.get(self.at) {
                Some(b'{') => self.object(),
                Some(b'[') => self.array(),
                Some(b'"') => self.string(),
                Some(b't') => self.literal(b"true"),
                Some(b'f') => self.literal(b"false"),
                Some(b'n') => self.literal(b"null"),
                Some(b'-' | b'0'..=b'9') => self.number(),
                _ => false,
            }
        }

        fn literal(&mut self, word: &[u8]) -> bool {
            if self.bytes[self.at..].starts_with(word) {
                self.at += word.len();
                true
            } else {
                false
            }
        }

        fn object(&mut self) -> bool {
            self.at += 1; // '{'
            self.ws();
            if self.bytes.get(self.at) == Some(&b'}') {
                self.at += 1;
                return true;
            }
            loop {
                self.ws();
                if !self.string() {
                    return false;
                }
                self.ws();
                if self.bytes.get(self.at) != Some(&b':') {
                    return false;
                }
                self.at += 1;
                if !self.value() {
                    return false;
                }
                self.ws();
                match self.bytes.get(self.at) {
                    Some(b',') => self.at += 1,
                    Some(b'}') => {
                        self.at += 1;
                        return true;
                    }
                    _ => return false,
                }
            }
        }

        fn array(&mut self) -> bool {
            self.at += 1; // '['
            self.ws();
            if self.bytes.get(self.at) == Some(&b']') {
                self.at += 1;
                return true;
            }
            loop {
                if !self.value() {
                    return false;
                }
                self.ws();
                match self.bytes.get(self.at) {
                    Some(b',') => self.at += 1,
                    Some(b']') => {
                        self.at += 1;
                        return true;
                    }
                    _ => return false,
                }
            }
        }

        fn string(&mut self) -> bool {
            if self.bytes.get(self.at) != Some(&b'"') {
                return false;
            }
            self.at += 1;
            loop {
                match self.bytes.get(self.at) {
                    None => return false,
                    Some(b'"') => {
                        self.at += 1;
                        return true;
                    }
                    Some(b'\\') => {
                        self.at += 1;
                        match self.bytes.get(self.at) {
                            Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {
                                self.at += 1;
                            }
                            Some(b'u') => {
                                self.at += 1;
                                for _ in 0..4 {
                                    if !matches!(
                                        self.bytes.get(self.at),
                                        Some(b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F')
                                    ) {
                                        return false;
                                    }
                                    self.at += 1;
                                }
                            }
                            _ => return false,
                        }
                    }
                    Some(&byte) if byte >= 0x20 => self.at += 1,
                    Some(_) => return false,
                }
            }
        }

        fn number(&mut self) -> bool {
            if self.bytes.get(self.at) == Some(&b'-') {
                self.at += 1;
            }
            let start = self.at;
            while matches!(self.bytes.get(self.at), Some(b'0'..=b'9')) {
                self.at += 1;
            }
            if self.at == start {
                return false;
            }
            if self.bytes.get(self.at) == Some(&b'.') {
                self.at += 1;
                let frac = self.at;
                while matches!(self.bytes.get(self.at), Some(b'0'..=b'9')) {
                    self.at += 1;
                }
                if self.at == frac {
                    return false;
                }
            }
            if matches!(self.bytes.get(self.at), Some(b'e' | b'E')) {
                self.at += 1;
                if matches!(self.bytes.get(self.at), Some(b'+' | b'-')) {
                    self.at += 1;
                }
                let exp = self.at;
                while matches!(self.bytes.get(self.at), Some(b'0'..=b'9')) {
                    self.at += 1;
                }
                if self.at == exp {
                    return false;
                }
            }
            true
        }
    }
}
