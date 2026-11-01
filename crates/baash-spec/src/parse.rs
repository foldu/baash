//! Parser for the Oils `*.test.sh` spec-test format.
//!
//! The format is line oriented.  A case starts at a `#### name` line and runs
//! until the next `####` line or end of file.  The case body is shell code up
//! to the first `## key: value` directive; more directives may follow the code.
//!
//! The parser mirrors the reference implementation (`test/sh_spec.py`):
//!
//! * `## STDOUT:` / `## STDERR:` open a block that runs until `## END` or the
//!   next directive or case; blank lines in the block are significant and
//!   comment lines are skipped;
//! * `## stdout:` / `## stderr:` are one-line forms whose newline is implicit;
//! * `## stdout-json:` / `## stderr-json:` are JSON strings;
//! * `## status:` defaults to 0;
//! * a directive may be qualified: `## OK bash/ninja2 STDOUT:` (the shell list
//!   may be slash separated) or `## OK-2 bash status:`.
//!
//! Files are line-normalised from CRLF to LF.  A malformed file produces a
//! [`ParseError`] with a line number; the parser never panics.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

/// What kind of test file this is, following the upstream suite convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suite {
    /// POSIX / bash style shell tests (the ones we care about).
    Osh,
    /// YSH tests.  baash does not implement YSH, so these are out of scope.
    Ysh,
    /// Tea language tests (not present in the vendored tree today).
    Tea,
    /// Unknown suite, run it.
    Other,
}

impl Suite {
    fn classify(name: &str, meta: &BTreeMap<String, String>) -> Self {
        if let Some(suite) = meta.get("suite") {
            match suite.trim() {
                "osh" => return Self::Osh,
                "ysh" => return Self::Ysh,
                "tea" => return Self::Tea,
                _ => return Self::Other,
            }
        }
        // Upstream `_DefaultSuite`: ysh-* and hay* are YSH, tea-* is Tea.
        if name.starts_with("ysh-") || name.starts_with("hay") {
            Self::Ysh
        } else if name.starts_with("tea-") {
            Self::Tea
        } else {
            Self::Osh
        }
    }
}

/// A parsed spec file.
#[derive(Debug, Clone)]
pub struct TestFile {
    /// Basename, e.g. `quote.test.sh`.
    pub name: String,
    pub path: std::path::PathBuf,
    pub suite: Suite,
    /// File-level metadata (`## compare_shells: …` before the first case).
    pub meta: BTreeMap<String, String>,
    pub cases: Vec<Case>,
}

impl TestFile {
    /// The `compare_shells` list, if declared.
    pub fn compare_shells(&self) -> Vec<&str> {
        self.meta
            .get("compare_shells")
            .map(|v| v.split_whitespace().collect())
            .unwrap_or_default()
    }

    /// True when the file declares `## legacy_tmp_dir`.
    pub fn legacy_tmp_dir(&self) -> bool {
        self.meta.contains_key("legacy_tmp_dir")
    }
}

/// One `#### name` case.
#[derive(Debug, Clone)]
pub struct Case {
    pub name: String,
    /// 1-based line number of the `####` line.
    pub line: usize,
    /// Shell code fed to the shell on stdin.  Empty when the case uses
    /// `## code:`.
    pub code: String,
    /// Expected stdout/stderr/status declarations, including per-shell ones.
    pub specs: Vec<Spec>,
    /// Unqualified non-assertion metadata (e.g. `our_shell`, `code`).
    pub meta: BTreeMap<String, String>,
}

impl Case {
    /// Case metadata `our_shell`, if present.
    pub fn our_shell(&self) -> Option<&str> {
        self.meta.get("our_shell").map(String::as_str)
    }

    /// Whether the case's expected stdout is checked at all.
    pub fn has_stdout(&self, shell: &str, primary: bool) -> bool {
        !self.stdout_for(shell, primary).is_empty()
    }

    /// The unqualified (ideal) stdout declarations only.
    pub fn default_stdout(&self) -> Vec<&[u8]> {
        self.specs
            .iter()
            .filter(|spec| spec.qualifier.is_none() && spec.key == SpecKey::Stdout)
            .filter_map(|spec| match &spec.value {
                SpecValue::Bytes(bytes) => Some(bytes.as_slice()),
                SpecValue::Status(_) => None,
            })
            .collect()
    }

    /// All acceptable stdout values for `shell`.
    ///
    /// For the shell under test (`primary`) an `## OK <shell> STDOUT:` block
    /// is an allowed alternative regardless of which shell it names: that is
    /// the harness contract.  For a reference shell only blocks naming that
    /// shell count, falling back to the unqualified declaration.
    pub fn stdout_for(&self, shell: &str, primary: bool) -> Vec<&[u8]> {
        let mut out = Vec::new();
        if primary {
            for spec in &self.specs {
                let SpecValue::Bytes(bytes) = &spec.value else {
                    continue;
                };
                if spec.key != SpecKey::Stdout {
                    continue;
                }
                let ok = match &spec.qualifier {
                    None => true,
                    Some(q) => q.starts_with("OK"),
                };
                if ok {
                    out.push(bytes.as_slice());
                }
            }
        } else {
            for spec in &self.specs {
                let SpecValue::Bytes(bytes) = &spec.value else {
                    continue;
                };
                if spec.key == SpecKey::Stdout && spec.shells.iter().any(|s| s == shell) {
                    out.push(bytes.as_slice());
                }
            }
            if out.is_empty() {
                for spec in &self.specs {
                    let SpecValue::Bytes(bytes) = &spec.value else {
                        continue;
                    };
                    if spec.key == SpecKey::Stdout && spec.qualifier.is_none() {
                        out.push(bytes.as_slice());
                    }
                }
            }
        }
        out
    }

    /// Expected stderr for `shell`, if the case compares stderr at all.
    pub fn stderr_for(&self, shell: &str, primary: bool) -> Option<&[u8]> {
        let mut specific = None;
        let mut default = None;
        for spec in &self.specs {
            let SpecValue::Bytes(bytes) = &spec.value else {
                continue;
            };
            if spec.key != SpecKey::Stderr {
                continue;
            }
            if spec.qualifier.is_none() {
                default = Some(bytes.as_slice());
            } else if !primary && spec.shells.iter().any(|s| s == shell) {
                specific = Some(bytes.as_slice());
            }
        }
        if specific.is_some() {
            specific
        } else {
            default
        }
    }

    /// Expected exit status for `shell`.  Defaults to 0.
    pub fn status_for(&self, shell: &str, primary: bool) -> i32 {
        let mut specific = None;
        let mut default = None;
        for spec in &self.specs {
            let SpecValue::Status(status) = spec.value else {
                continue;
            };
            if spec.key != SpecKey::Status {
                continue;
            }
            if spec.qualifier.is_none() {
                default = Some(status);
            } else if !primary && spec.shells.iter().any(|s| s == shell) {
                specific = Some(status);
            }
        }
        specific.or(default).unwrap_or(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecKey {
    Stdout,
    Stderr,
    Status,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecValue {
    Bytes(Vec<u8>),
    Status(i32),
}

/// A single assertion declaration.  `qualifier` is `None` for the ideal
/// (unqualified) declaration.
#[derive(Debug, Clone)]
pub struct Spec {
    pub qualifier: Option<String>,
    pub shells: Vec<String>,
    pub key: SpecKey,
    pub value: SpecValue,
}

/// A malformed spec file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl ParseError {
    fn new(line: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

/// Parse a spec file from bytes.  A trailing newline is not required and CRLF
/// is accepted.
pub fn parse_bytes(name: &str, path: &Path, bytes: &[u8]) -> Result<TestFile, ParseError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| ParseError::new(0, format!("file is not valid UTF-8: {e}")))?;
    parse_text(name, path, text)
}

/// Parse a spec file from text.
pub fn parse_text(name: &str, path: &Path, text: &str) -> Result<TestFile, ParseError> {
    let normalized = text.replace("\r\n", "\n");
    let mut parser = Parser {
        lines: normalized.split_inclusive('\n').collect(),
        pos: 0,
    };
    parser.parse(name, path)
}

struct Parser<'a> {
    lines: Vec<&'a str>,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn parse(&mut self, name: &str, path: &Path) -> Result<TestFile, ParseError> {
        let mut meta = BTreeMap::new();

        // File-level metadata must precede the first case.  Blank lines and
        // comments are skipped by the upstream tokenizer.
        loop {
            self.skip_blank_and_comments();
            match self.peek() {
                None => break,
                Some(line) if is_case_start(line) => break,
                Some(line) if line.starts_with("##") => {
                    let line_no = self.pos + 1;
                    match parse_directive(line) {
                        Directive::Key {
                            qualifier: None,
                            key,
                            value,
                            ..
                        } => {
                            meta.insert(key, value);
                            self.pos += 1;
                        }
                        Directive::Key {
                            qualifier: Some(q), ..
                        } => {
                            return Err(ParseError::new(
                                line_no,
                                format!("file-level metadata cannot be qualified with {q}"),
                            ));
                        }
                        Directive::End => {
                            return Err(ParseError::new(line_no, "unexpected ## END"));
                        }
                        Directive::Invalid => {
                            return Err(ParseError::new(
                                line_no,
                                format!("invalid directive {line:?}"),
                            ));
                        }
                    }
                }
                Some(line) => {
                    return Err(ParseError::new(
                        self.pos + 1,
                        format!("expected a test case or file metadata, got {line:?}"),
                    ));
                }
            }
        }

        let mut cases = Vec::new();
        loop {
            self.skip_blank_and_comments();
            match self.peek() {
                None => break,
                Some(line) if is_case_start(line) => {
                    cases.push(self.parse_case()?);
                }
                Some(line) => {
                    return Err(ParseError::new(
                        self.pos + 1,
                        format!("expected a test case, got {line:?}"),
                    ));
                }
            }
        }

        let suite = Suite::classify(name, &meta);
        Ok(TestFile {
            name: name.to_string(),
            path: path.to_path_buf(),
            suite,
            meta,
            cases,
        })
    }

    fn parse_case(&mut self) -> Result<Case, ParseError> {
        let line_no = self.pos + 1;
        let line = self.lines[self.pos];
        self.pos += 1;
        let name = line.strip_prefix("####").unwrap_or("").trim().to_string();

        let mut specs = Vec::new();
        let mut meta = BTreeMap::new();
        self.parse_directives(&mut specs, &mut meta)?;

        // `## code:` supplies the script instead of a body (used for parse
        // errors, where the text is deliberately not valid shell).
        if let Some(code) = meta.remove("code") {
            return Ok(Case {
                name,
                line: line_no,
                code,
                specs,
                meta,
            });
        }

        let mut code = String::new();
        let mut saw_code = false;
        loop {
            match self.peek() {
                None => break,
                Some(line) if is_case_start(line) || line.starts_with("##") => break,
                Some(line) if is_comment(line) => {
                    self.pos += 1;
                }
                Some(line) => {
                    code.push_str(line);
                    saw_code = true;
                    self.pos += 1;
                }
            }
        }

        if !saw_code {
            return Err(ParseError::new(
                line_no,
                "test case has no shell code (and no `## code:`)",
            ));
        }

        self.parse_directives(&mut specs, &mut meta)?;

        Ok(Case {
            name,
            line: line_no,
            code,
            specs,
            meta,
        })
    }

    /// Consume contiguous directives, skipping blank lines and comments
    /// between them (as the upstream tokenizer does).
    fn parse_directives(
        &mut self,
        specs: &mut Vec<Spec>,
        meta: &mut BTreeMap<String, String>,
    ) -> Result<(), ParseError> {
        loop {
            self.skip_blank_and_comments();
            let Some(line) = self.peek() else {
                return Ok(());
            };
            if !line.starts_with("##") || is_case_start(line) {
                return Ok(());
            }
            let line_no = self.pos + 1;
            let directive = parse_directive(line);
            match directive {
                Directive::End => {
                    return Err(ParseError::new(line_no, "unexpected ## END"));
                }
                Directive::Invalid => {
                    return Err(ParseError::new(
                        line_no,
                        format!("invalid directive {line:?}"),
                    ));
                }
                Directive::Key {
                    qualifier,
                    shells,
                    key,
                    value,
                } => {
                    self.pos += 1;
                    let is_multiline = key == "STDOUT" || key == "STDERR";
                    let value = if is_multiline {
                        if !value.trim().is_empty() {
                            return Err(ParseError::new(
                                line_no,
                                format!("{key} must have its value on the following lines"),
                            ));
                        }
                        self.parse_multiline()?
                    } else {
                        value
                    };
                    add_spec(specs, meta, line_no, qualifier, shells, &key, value)?;
                }
            }
        }
    }

    /// Read the body of a `## STDOUT:` block.
    fn parse_multiline(&mut self) -> Result<String, ParseError> {
        let mut value = String::new();
        while let Some(line) = self.peek() {
            if is_case_start(line) {
                break;
            }
            if line.starts_with("##") {
                // `## END` terminates; any other directive ends the block and
                // is handled by the caller.
                if let Directive::End = parse_directive(line) {
                    self.pos += 1;
                }
                break;
            }
            if is_comment(line) {
                self.pos += 1;
                continue;
            }
            value.push_str(line);
            self.pos += 1;
        }
        Ok(value)
    }

    fn peek(&self) -> Option<&'a str> {
        self.lines.get(self.pos).copied()
    }

    fn skip_blank_and_comments(&mut self) {
        while let Some(line) = self.peek() {
            if line.trim().is_empty() || is_comment(line) {
                self.pos += 1;
            } else {
                break;
            }
        }
    }
}

fn is_case_start(line: &str) -> bool {
    line.starts_with("####")
}

fn is_comment(line: &str) -> bool {
    // A `##` directive at column 0 is metadata, not a comment.  An indented
    // `##` line is a comment, matching the upstream tokenizer.
    !line.starts_with("##") && line.trim_start().starts_with('#')
}

enum Directive {
    Key {
        qualifier: Option<String>,
        shells: Vec<String>,
        key: String,
        value: String,
    },
    End,
    Invalid,
}

/// `## key: value`, `## END`, or `## OK shell/shell key: value`.
fn parse_directive(line: &str) -> Directive {
    let Some(rest) = line.strip_prefix("##") else {
        return Directive::Invalid;
    };
    // The upstream regex requires whitespace after the `##`.
    if !(rest.starts_with(' ') || rest.starts_with('\t')) {
        return Directive::Invalid;
    }
    let rest = rest.trim_start();
    // Upstream's END_MULTILINE_RE is a prefix match (`##\s+END`), so the
    // vendored suite's occasional `## END:` typo terminates a block too.
    if rest.starts_with("END") {
        return Directive::End;
    }

    let Some(colon) = rest.find(':') else {
        return Directive::Invalid;
    };
    let prefix = rest[..colon].trim();
    let value = rest[colon + 1..].trim_start();
    let value = value.strip_suffix('\n').unwrap_or(value).to_string();
    let tokens: Vec<&str> = prefix.split_whitespace().collect();

    let (qualifier, shells, key) = match tokens.as_slice() {
        [key] => (None, Vec::new(), *key),
        [qualifier, shells, key] if is_qualifier(qualifier) => {
            let parts: Vec<&str> = shells.split('/').collect();
            if parts.iter().any(|s| s.is_empty()) {
                return Directive::Invalid;
            }
            let shells: Vec<String> = parts.into_iter().map(str::to_string).collect();
            (Some((*qualifier).to_string()), shells, *key)
        }
        // A three-token form whose first token is not a qualifier, or any
        // other shape, is malformed.
        _ => return Directive::Invalid,
    };
    if key.is_empty()
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Directive::Invalid;
    }
    if shells.iter().any(|s| {
        !s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '+' || c == '/')
    }) {
        return Directive::Invalid;
    }
    Directive::Key {
        qualifier,
        shells,
        key: key.to_string(),
        value,
    }
}

fn is_qualifier(token: &str) -> bool {
    if token == "N-I" || token == "OK" || token == "BUG" {
        return true;
    }
    let Some(rest) = token
        .strip_prefix("OK-")
        .or_else(|| token.strip_prefix("BUG-"))
    else {
        return false;
    };
    !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
}

fn add_spec(
    specs: &mut Vec<Spec>,
    meta: &mut BTreeMap<String, String>,
    line_no: usize,
    qualifier: Option<String>,
    shells: Vec<String>,
    key: &str,
    value: String,
) -> Result<(), ParseError> {
    let spec_key = match key {
        "STDOUT" | "stdout" | "stdout-json" => Some(SpecKey::Stdout),
        "STDERR" | "stderr" | "stderr-json" => Some(SpecKey::Stderr),
        "status" => Some(SpecKey::Status),
        _ => None,
    };

    if let Some(spec_key) = spec_key {
        if qualifier.is_none() {
            // Upstream overwrites an unqualified declaration silently; the
            // last one wins (e.g. builtin-history.test.sh declares STDOUT
            // twice for different shell versions).
            specs.retain(|existing| !(existing.qualifier.is_none() && existing.key == spec_key));
        }
        // Duplicate qualified declarations for the same key and shell are a
        // file bug in the upstream format; report them rather than silently
        // picking one.
        let duplicate = qualifier.is_some()
            && specs.iter().any(|existing| {
                existing.key == spec_key
                    && if let Some(q) = &qualifier {
                        existing.qualifier.as_deref() == Some(q.as_str())
                            && shells.iter().any(|s| existing.shells.contains(s))
                    } else {
                        existing.qualifier.is_none()
                    }
            });
        if duplicate {
            return Err(ParseError::new(
                line_no,
                format!("duplicate {key} declaration"),
            ));
        }

        let value = if key == "status" {
            let status = value
                .trim()
                .parse::<i32>()
                .map_err(|_| ParseError::new(line_no, format!("invalid status {value:?}")))?;
            SpecValue::Status(status)
        } else if key.ends_with("-json") {
            SpecValue::Bytes(
                decode_json_string(&value)
                    .map_err(|e| ParseError::new(line_no, format!("invalid {key} value: {e}")))?,
            )
        } else if key == "STDOUT" || key == "STDERR" {
            SpecValue::Bytes(value.into_bytes())
        } else {
            // `stdout:` / `stderr:`: the trailing newline is implicit.
            let mut bytes = value.into_bytes();
            bytes.push(b'\n');
            SpecValue::Bytes(bytes)
        };
        specs.push(Spec {
            qualifier,
            shells,
            key: spec_key,
            value,
        });
        Ok(())
    } else if qualifier.is_none() {
        meta.insert(key.to_string(), value);
        Ok(())
    } else {
        // Qualified non-assertion metadata carries no meaning for us.
        Ok(())
    }
}

/// Decode a JSON string literal (`## stdout-json: "..."`).
///
/// Returns UTF-8 bytes, so `\u03bc` becomes the two bytes of `μ`, matching the
/// bytes a shell writes to stdout under `LC_ALL=C.UTF-8`-style locales.
pub fn decode_json_string(input: &str) -> Result<Vec<u8>, String> {
    let s = input.trim();
    let Some(body) = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return Err("expected a double-quoted JSON string".to_string());
    };
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let Some(esc) = chars.next() else {
                    return Err("trailing backslash".to_string());
                };
                match esc {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    '/' => out.push('/'),
                    'b' => out.push('\u{08}'),
                    'f' => out.push('\u{0c}'),
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    'u' => {
                        let first = read_hex4(&mut chars)?;
                        let code = if (0xD800..=0xDBFF).contains(&first) {
                            // High surrogate: must be followed by `\uXXXX`.
                            if chars.next() != Some('\\') || chars.next() != Some('u') {
                                return Err("unpaired high surrogate".to_string());
                            }
                            let second = read_hex4(&mut chars)?;
                            if !(0xDC00..=0xDFFF).contains(&second) {
                                return Err("invalid low surrogate".to_string());
                            }
                            0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00)
                        } else if (0xDC00..=0xDFFF).contains(&first) {
                            return Err("unpaired low surrogate".to_string());
                        } else {
                            first
                        };
                        let ch = char::from_u32(code)
                            .ok_or_else(|| format!("invalid unicode escape U+{code:04X}"))?;
                        out.push(ch);
                    }
                    other => return Err(format!("invalid escape \\{other}")),
                }
            }
            c if (c as u32) < 0x20 => {
                return Err("unescaped control character in string".to_string());
            }
            c => out.push(c),
        }
    }
    Ok(out.into_bytes())
}

fn read_hex4(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Result<u32, String> {
    let mut value = 0u32;
    for _ in 0..4 {
        let c = chars
            .next()
            .ok_or_else(|| "truncated \\u escape".to_string())?;
        let digit = c
            .to_digit(16)
            .ok_or_else(|| format!("invalid hex digit {c:?} in \\u escape"))?;
        value = value * 16 + digit;
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn vendored(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../spec/oils")
            .join(name)
    }

    fn parse_vendored(name: &str) -> TestFile {
        let path = vendored(name);
        let bytes = std::fs::read(&path).expect("read vendored spec file");
        parse_bytes(name, &path, &bytes).expect("parse vendored spec file")
    }

    #[test]
    fn parses_real_file_case_counts() {
        for (name, count) in [
            ("parse-errors.test.sh", 27),
            ("alias.test.sh", 48),
            ("command_.test.sh", 16),
            ("quote.test.sh", 35),
            ("ysh-expr-sub.test.sh", 7),
            ("redirect.test.sh", 41),
        ] {
            let file = parse_vendored(name);
            assert_eq!(file.cases.len(), count, "{name}");
        }
    }

    #[test]
    fn parses_single_line_stdout() {
        let file = parse_vendored("quote.test.sh");
        let case = &file.cases[0];
        assert_eq!(case.name, "Unquoted words");
        assert_eq!(case.code, "echo unquoted    words\n");
        assert_eq!(
            case.stdout_for("baash", true),
            vec![b"unquoted words\n".as_slice()]
        );
        assert_eq!(case.status_for("baash", true), 0);
    }

    #[test]
    fn parses_multiline_stdout() {
        let file = parse_vendored("quote.test.sh");
        let case = file
            .cases
            .iter()
            .find(|c| c.name == "newline inside single-quoted string")
            .unwrap();
        assert_eq!(
            case.stdout_for("baash", true),
            vec![b"newline\ninside single-quoted string\n".as_slice()]
        );
    }

    #[test]
    fn parses_ok_blocks_as_alternatives() {
        let file = parse_vendored("quote.test.sh");
        let case = file
            .cases
            .iter()
            .find(|c| c.name == "$? split over multiple lines")
            .unwrap();
        assert_eq!(
            case.stdout_for("baash", true),
            vec![b"$?\n".as_slice(), b"0\n".as_slice()]
        );
    }

    #[test]
    fn parses_status_and_code_directive() {
        let file = parse_vendored("quote.test.sh");
        let case = file
            .cases
            .iter()
            .find(|c| c.name == "Unterminated single quote")
            .unwrap();
        assert_eq!(case.code, "ls foo bar '");
        assert_eq!(case.status_for("baash", true), 2);
        assert_eq!(case.status_for("mksh", false), 1);
    }

    #[test]
    fn parses_stdout_json_escapes() {
        let file = parse_vendored("quote.test.sh");
        let case = file
            .cases
            .iter()
            .find(|c| c.name == "C-style backslash escapes inside double quoted string")
            .unwrap();
        // The ideal expectation is a normal one-liner.
        assert_eq!(
            case.stdout_for("baash", true),
            vec![b"\\a \\b\n".as_slice()]
        );
        // The BUG dash/mksh alternative is JSON with control characters; it is
        // not an alternative for the shell under test.
        assert!(
            !case
                .stdout_for("baash", true)
                .iter()
                .any(|b| b.contains(&0x07))
        );
        assert_eq!(
            case.stdout_for("dash", false),
            vec![b"\x07 \x08\n".as_slice()]
        );
    }

    #[test]
    fn parses_file_metadata_and_suite() {
        let file = parse_vendored("alias.test.sh");
        assert_eq!(file.suite, Suite::Osh);
        assert_eq!(file.compare_shells(), vec!["bash", "dash", "mksh", "zsh"]);

        let file = parse_vendored("ysh-expr-sub.test.sh");
        assert_eq!(file.suite, Suite::Ysh);
        assert_eq!(file.meta.get("our_shell").map(String::as_str), Some("ysh"));

        // Many ysh files have no header; the filename convention decides.
        let file = parse_vendored("ysh-augmented.test.sh");
        assert_eq!(file.suite, Suite::Ysh);
    }

    #[test]
    fn tolerates_crlf_and_missing_final_newline() {
        let text =
            "#### crlf\r\necho hi\r\n## STDOUT:\r\nhi\r\n## END\r\n\r\n#### no newline\r\necho hi";
        let file = parse_text("x.test.sh", Path::new("x.test.sh"), text).unwrap();
        assert_eq!(file.cases.len(), 2);
        assert_eq!(file.cases[0].code, "echo hi\n");
        assert_eq!(
            file.cases[0].stdout_for("baash", true),
            vec![b"hi\n".as_slice()]
        );
        assert_eq!(file.cases[1].code, "echo hi");
    }

    #[test]
    fn reports_malformed_file_instead_of_panicking() {
        let err = parse_text(
            "bad.test.sh",
            Path::new("bad.test.sh"),
            "#### case\necho hi\n## status: nope\n",
        )
        .unwrap_err();
        assert_eq!(err.line, 3);
        assert!(err.message.contains("invalid status"), "{}", err.message);

        let err = parse_text(
            "bad.test.sh",
            Path::new("bad.test.sh"),
            "#### case\necho hi\n## bogus line\n",
        )
        .unwrap_err();
        assert_eq!(err.line, 3);
    }

    #[test]
    fn rejects_case_without_code() {
        let err = parse_text(
            "bad.test.sh",
            Path::new("bad.test.sh"),
            "#### case\n## status: 1\n",
        )
        .unwrap_err();
        assert!(err.message.contains("no shell code"), "{}", err.message);
    }

    #[test]
    fn decode_json_string_handles_surrogates_and_nul() {
        assert_eq!(decode_json_string(r#""a\tb""#).unwrap(), b"a\tb");
        assert_eq!(decode_json_string(r#""\u0000x""#).unwrap(), b"\0x");
        assert_eq!(
            decode_json_string(r#""\ud83d\ude00""#).unwrap(),
            "😀".as_bytes()
        );
        assert_eq!(decode_json_string(r#""\u03bc""#).unwrap(), "μ".as_bytes());
        assert!(decode_json_string(r#""\ud83d""#).is_err());
        assert!(decode_json_string("no quotes").is_err());
    }
}
