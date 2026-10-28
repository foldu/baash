//! `[[ … ]]`: the bash conditional command.
//!
//! `bsh-syntax` keeps the body as a span rather than a tree on purpose (`=~`
//! operands are arbitrary bytes), so the conditional expression is parsed here
//! over that span: `!`, `&&`, `||`, `( … )` with the C-like precedence bash
//! uses, plus the unary and binary operators below.
//!
//! Inside `[[ ]]` the operands undergo tilde, parameter, command and arithmetic
//! expansion and quote removal, but **no field splitting and no pathname
//! expansion** (POSIX XCU 2.13, bash's documented behaviour); that is why every
//! operand is expanded in [`bsh_lang::Mode::Assignment`] and keeps its quoting
//! mask, which decides whether `==`'s right-hand side is a pattern or a string.
//!
//! Deferred: `=~` needs POSIX ERE (DESIGN.md §4.8 says to bind `regcomp`
//! rather than use the `regex` crate), so it reports a deferred-feature
//! diagnostic instead of guessing.

use std::ops::Range;
use std::sync::Arc;

use bsh_lang::Field;
use bsh_lang::Shell as _;
use bsh_runtime::{ShellHost, Status};

use super::words::{CondToken, binary_operator, conditional_tokens, unary_operator};
use super::{Interp, ParsedChunk};

/// Evaluate a `[[ … ]]` body.
pub(crate) fn evaluate(
    interp: &mut Interp,
    chunk: &Arc<ParsedChunk>,
    body: Range<usize>,
) -> Status {
    let text = &chunk.source.bytes()[body.clone()];
    // bash decides whether `!(…)` is an extglob word or a negation at parse
    // time; the tokenizer runs here, so the current option is the closest
    // thing to that.
    let extglob = interp.pattern_flags().extglob;
    let tokens = conditional_tokens(text, extglob);
    let mut parser = Parser {
        interp,
        text,
        tokens,
        pos: 0,
        span: body,
    };
    let result = parser.or_expr();
    match result {
        Ok(value) => {
            if parser.pos < parser.tokens.len() {
                parser.report("unexpected token");
                return Status(2);
            }
            if value {
                Status::SUCCESS
            } else {
                Status::FAILURE
            }
        }
        Err(message) => {
            if !message.is_empty() {
                parser.report(&message);
            }
            Status(2)
        }
    }
}

struct Parser<'a> {
    interp: &'a mut Interp,
    text: &'a [u8],
    tokens: Vec<CondToken>,
    pos: usize,
    span: Range<usize>,
}

/// Split `name[subscript]` into its parts, when the operand ends with `]`.
fn split_subscript(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let open = bytes.iter().position(|&byte| byte == b'[')?;
    if !bytes.ends_with(b"]") {
        return None;
    }
    Some((&bytes[..open], &bytes[open + 1..bytes.len() - 1]))
}

impl Parser<'_> {
    fn report(&mut self, message: &str) {
        self.interp
            .report(crate::codes::CONDITIONAL, self.span.clone(), message);
    }

    fn peek(&self) -> Option<&CondToken> {
        self.tokens.get(self.pos)
    }

    fn eat(&mut self, token: &CondToken) -> bool {
        if self.peek() == Some(token) {
            self.pos += 1;
            return true;
        }
        false
    }

    fn take_word(&mut self) -> Option<Range<usize>> {
        match self.peek()?.clone() {
            CondToken::Word(range) => {
                self.pos += 1;
                Some(range)
            }
            _ => None,
        }
    }

    fn or_expr(&mut self) -> Result<bool, String> {
        let mut value = self.and_expr()?;
        while self.eat(&CondToken::Or) {
            let right = self.and_expr()?;
            value = value || right;
        }
        Ok(value)
    }

    fn and_expr(&mut self) -> Result<bool, String> {
        let mut value = self.unary()?;
        while self.eat(&CondToken::And) {
            let right = self.unary()?;
            value = value && right;
        }
        Ok(value)
    }

    fn unary(&mut self) -> Result<bool, String> {
        if self.eat(&CondToken::Bang) {
            return Ok(!self.unary()?);
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<bool, String> {
        if self.eat(&CondToken::LParen) {
            let value = self.or_expr()?;
            if !self.eat(&CondToken::RParen) {
                return Err("missing `)`".to_owned());
            }
            return Ok(value);
        }
        let Some(range) = self.take_word() else {
            return Err("expected an operand".to_owned());
        };
        let word = self.text[range.clone()].to_vec();

        // A one-operand test: `[[ -f file ]]`.
        //
        // A leading unary operator demands an operand, always: bash rejects
        // `[[ -z ]]` *and* `[[ -z == -z ]]`, because `-z` there is the
        // operator, not a word. Falling through to "treat it as a word" makes
        // `[[ -z ]]` quietly true, which is the wrong answer to a typo.
        if unary_operator(&word) {
            let Some(operand) = self.take_word() else {
                return Err(format!(
                    "`{}` needs an operand",
                    String::from_utf8_lossy(&word)
                ));
            };
            let operand = self.expand(&operand)?;
            return self.unary_test(&word, &operand);
        }

        let left = self.expand(&range)?;

        // A two-operand test, either written with a comparison token
        // (`==`, `!=`, `=~`, `<`, `>`) or with an operator word (`-eq`, …).
        if let Some(operator) = self.peek().cloned() {
            let comparison = match operator {
                CondToken::Eq => Some(b"==".to_vec()),
                CondToken::Ne => Some(b"!=".to_vec()),
                CondToken::Match => Some(b"=~".to_vec()),
                CondToken::Lt => Some(b"<".to_vec()),
                CondToken::Gt => Some(b">".to_vec()),
                CondToken::Word(inner) if binary_operator(&self.text[inner.clone()]) => {
                    Some(self.text[inner].to_vec())
                }
                _ => None,
            };
            if let Some(operator) = comparison {
                self.pos += 1;
                let Some(right) = self.take_word() else {
                    return Err(format!(
                        "`{}` needs a right-hand operand",
                        String::from_utf8_lossy(&operator)
                    ));
                };
                // The right-hand side of `==`/`!=` is a pattern, so an
                // extglob group in the source text belongs to it even though
                // the body is parsed one word at a time.
                let right = if matches!(operator.as_slice(), b"==" | b"=" | b"!=") {
                    self.expand_pattern(&right)?
                } else if operator.as_slice() == b"=~" {
                    self.expand_regex(&right)?
                } else {
                    self.expand(&right)?
                };
                return self.compare(&operator, &left, &right);
            }
        }

        // A single word: true when non-empty.
        Ok(!left.bytes.is_empty())
    }

    /// Expand a `=~` right-hand side, keeping parens and braces as regex text.
    fn expand_regex(&mut self, range: &Range<usize>) -> Result<Field, String> {
        let text = &self.text[range.clone()];
        match self.interp.expand_regex_fragment(text) {
            Ok(field) => Ok(field),
            Err(error) => {
                self.interp.diagnose(&error.diagnostic());
                Err(String::new())
            }
        }
    }

    fn expand(&mut self, range: &Range<usize>) -> Result<Field, String> {
        let text = &self.text[range.clone()];
        match self.interp.expand_fragment(text) {
            Ok(field) => Ok(field),
            Err(error) => {
                self.interp.diagnose(&error.diagnostic());
                Err(String::new())
            }
        }
    }

    /// Expand a right-hand operand of `==`/`!=`, keeping extglob punctuation
    /// active.
    fn expand_pattern(&mut self, range: &Range<usize>) -> Result<Field, String> {
        let text = &self.text[range.clone()];
        match self.interp.expand_pattern_fragment(text) {
            Ok(field) => Ok(field),
            Err(error) => {
                self.interp.diagnose(&error.diagnostic());
                Err(String::new())
            }
        }
    }

    fn compare(&mut self, operator: &[u8], left: &Field, right: &Field) -> Result<bool, String> {
        match operator {
            b"==" | b"=" | b"!=" => {
                // bash matches the right-hand side with extglob semantics even
                // when `shopt -u extglob`; `nocasematch` still applies.
                let flags = crate::pattern::conditional_flags(self.interp.pattern_flags());
                let hit = crate::pattern::matches(right, &left.bytes, flags);
                Ok(if operator == b"!=" { !hit } else { hit })
            }
            b"<" => Ok(left.bytes < right.bytes),
            b">" => Ok(left.bytes > right.bytes),
            b"-eq" | b"-ne" | b"-lt" | b"-le" | b"-gt" | b"-ge" => {
                // bash parses each operand as an *arithmetic expression*, not
                // as an integer literal: `1+2 -eq 3` and `x -eq 0` (an unset
                // `x` is 0) are both true. `parse_int` only takes literals.
                let value = |parser: &mut Self, field: &Field| {
                    crate::arith::eval(&field.bytes, parser.interp)
                };
                let (left_value, right_value) = (value(self, left), value(self, right));
                let (Ok(left), Ok(right)) = (left_value, right_value) else {
                    self.report("integer expression expected");
                    return Err(String::new());
                };
                Ok(match operator {
                    b"-eq" => left == right,
                    b"-ne" => left != right,
                    b"-lt" => left < right,
                    b"-le" => left <= right,
                    b"-gt" => left > right,
                    _ => left >= right,
                })
            }
            b"=~" => self.regex_match(left, right),
            other => Err(format!(
                "unknown operator `{}`",
                String::from_utf8_lossy(other)
            )),
        }
    }

    /// `[[ text =~ re ]]`.
    ///
    /// The right-hand side is not passed through as bytes: bash re-escapes the
    /// metacharacters that were *quoted*, which is the only rule that explains
    /// all four measured behaviours (`a\.b` keeps its backslash, a literal
    /// `\b` loses it, a variable's `\b` keeps it, `"a.b"` is literal). The
    /// quoting mask from expansion is exactly the information needed.
    fn regex_match(&mut self, text: &Field, pattern: &Field) -> Result<bool, String> {
        let source = regex_source(pattern);
        let case_insensitive = self.interp.pattern_flags().nocasematch;
        let compiled = if case_insensitive {
            bsh_sys::Regex::new_icase(&source)
        } else {
            bsh_sys::Regex::new(&source)
        };
        let compiled = match compiled {
            Ok(compiled) => compiled,
            Err(error) => {
                // bash prints the pattern as the user wrote it, then the
                // engine's own message, and the `[[` command fails with 2.
                let shown = String::from_utf8_lossy(&pattern.bytes).into_owned();
                self.report(&format!(
                    "invalid regular expression `{shown}': {}",
                    error.message()
                ));
                self.clear_rematch();
                return Err(String::new());
            }
        };
        match compiled.captures(&text.bytes) {
            Some(captures) => {
                self.set_rematch(&captures, &text.bytes);
                Ok(true)
            }
            None => {
                // bash clears the array on a failed match; leaving the previous
                // match visible is a real bug in a script that loops.
                self.clear_rematch();
                Ok(false)
            }
        }
    }

    /// Write `BASH_REMATCH` from a successful match: element 0 is the whole
    /// match, a group that did not participate is the empty string.
    fn set_rematch(&mut self, captures: &[Option<(usize, usize)>], text: &[u8]) {
        self.clear_rematch();
        for (index, capture) in captures.iter().enumerate() {
            let value = match capture {
                Some((start, end)) => text.get(*start..*end).unwrap_or(&[]),
                None => &[][..],
            };
            let _ = self
                .interp
                .state
                .vars
                .set_indexed(b"BASH_REMATCH", index as i64, value);
        }
    }

    /// Empty the array, keeping it defined: `${BASH_REMATCH[@]}` is empty, not
    /// an unbound-variable error under `set -u`.
    fn clear_rematch(&mut self) {
        let _ = self.interp.state.vars.unset(b"BASH_REMATCH");
        let _ = self.interp.state.vars.set_indexed(b"BASH_REMATCH", 0, b"");
        let _ = self
            .interp
            .state
            .vars
            .unset_element_indexed(b"BASH_REMATCH", 0);
    }

    fn unary_test(&mut self, operator: &[u8], operand: &Field) -> Result<bool, String> {
        use std::os::unix::ffi::OsStrExt;
        let path = || std::path::Path::new(std::ffi::OsStr::from_bytes(&operand.bytes));
        let metadata = || std::fs::metadata(path());
        let link = || std::fs::symlink_metadata(path());
        Ok(match operator {
            b"-z" => operand.bytes.is_empty(),
            b"-n" => !operand.bytes.is_empty(),
            b"-e" | b"-a" => metadata().is_ok(),
            b"-f" => metadata().is_ok_and(|meta| meta.is_file()),
            b"-d" => metadata().is_ok_and(|meta| meta.is_dir()),
            b"-s" => metadata().is_ok_and(|meta| meta.len() > 0),
            b"-L" | b"-h" => link().is_ok_and(|meta| meta.file_type().is_symlink()),
            b"-p" | b"-S" | b"-b" | b"-c" => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::FileTypeExt;
                    let kind = link().map(|meta| meta.file_type()).ok();
                    match operator {
                        b"-p" => kind.is_some_and(|kind| kind.is_fifo()),
                        b"-S" => kind.is_some_and(|kind| kind.is_socket()),
                        b"-b" => kind.is_some_and(|kind| kind.is_block_device()),
                        _ => kind.is_some_and(|kind| kind.is_char_device()),
                    }
                }
                #[cfg(not(unix))]
                {
                    false
                }
            }
            b"-r" | b"-w" | b"-x" => {
                use std::os::unix::fs::PermissionsExt;
                let mode = metadata().map(|meta| meta.permissions().mode());
                let Ok(mode) = mode else {
                    return Ok(false);
                };
                let bit = match operator {
                    b"-r" => 0o444,
                    b"-w" => 0o222,
                    _ => 0o111,
                };
                mode & bit != 0
            }
            b"-v" => {
                // `-v name[subscript]` asks whether one element is set; a bare
                // name asks whether the variable exists at all (an array
                // counts). The subscript is arithmetic for an indexed array
                // and a literal key for an associative one.
                let bytes = &operand.bytes;
                match split_subscript(bytes) {
                    Some((name, subscript)) => {
                        if self.interp.state.vars.shape(name)
                            == Some(bsh_runtime::ArrayShape::Associative)
                        {
                            self.interp
                                .state
                                .vars
                                .element_assoc(name, subscript)
                                .is_some()
                        } else {
                            let index = crate::arith::eval(subscript, self.interp)?;
                            self.interp
                                .state
                                .vars
                                .element_indexed(name, index)
                                .is_some()
                        }
                    }
                    None => self.interp.state.vars.contains(bytes),
                }
            }
            other => {
                self.report(&format!(
                    "`{}`: unsupported unary operator",
                    String::from_utf8_lossy(other)
                ));
                return Err(String::new());
            }
        })
    }
}

/// Build the regex source from an expanded field: quoted metacharacters are
/// escaped, everything else passes through.
///
/// `bsh-pattern`'s matcher has no quote mask, but this one can have one because
/// the expansion produced it. Escaping only metacharacters matters: escaping a
/// quoted `b` would turn it into `\b`, a word boundary, which is how bash
/// itself ends up *not* matching a literal `\b`.
fn regex_source(field: &Field) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.bytes.len());
    for (&byte, &quoted) in field.bytes.iter().zip(&field.quoted) {
        if quoted && is_regex_meta(byte) {
            out.push(b'\\');
        }
        out.push(byte);
    }
    out
}

/// Characters that mean something to a POSIX ERE.
fn is_regex_meta(byte: u8) -> bool {
    matches!(
        byte,
        b'\\'
            | b'.'
            | b'['
            | b']'
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b'?'
            | b'{'
            | b'}'
            | b'|'
            | b'^'
            | b'$'
    )
}
