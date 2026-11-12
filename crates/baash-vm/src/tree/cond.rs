//! `[[ … ]]`: the bash conditional command.
//!
//! `baash-syntax` keeps the body as a span rather than a tree on purpose (`=~`
//! operands are arbitrary bytes), so the conditional expression is parsed here
//! over that span: `!`, `&&`, `||`, `( … )` with the C-like precedence bash
//! uses, plus the unary and binary operators below.
//!
//! `-o name` is the one operator that reads shell state: it answers whether an
//! option `set -o` knows is on, which is how a startup file asks "is this shell
//! interactive enough to record anything?" (zoxide's hook does exactly that).
//! An unknown name is false rather than an error, as bash has it.
//!
//! Inside `[[ ]]` the operands undergo tilde, parameter, command and arithmetic
//! expansion and quote removal, but **no field splitting and no pathname
//! expansion** (POSIX XCU 2.13, bash's documented behaviour); that is why every
//! operand is expanded in [`baash_lang::Mode::Assignment`] and keeps its quoting
//! mask, which decides whether `==`'s right-hand side is a pattern or a string.
//!
//! Deferred: `=~` needs POSIX ERE (DESIGN.md §4.8 says to bind `regcomp`
//! rather than use the `regex` crate), so it reports a deferred-feature
//! diagnostic instead of guessing.

use std::ops::Range;
use std::sync::Arc;

use baash_lang::Field;
use baash_lang::Shell as _;
use baash_runtime::{ShellHost, Status, vfs};

use super::words::{CondToken, binary_operator, conditional_tokens, unary_operator};
use super::{Interp, ParsedChunk, SourceFile};

/// Parse a `[[ … ]]` body without expanding or evaluating anything.
///
/// A malformed conditional is a *parse* error in bash: the whole chunk is
/// rejected before any command runs. The evaluator parses lazily, so the chunk
/// entry point asks for this validation pass first; it shares the grammar and
/// the token tables with [`evaluate`] and skips only expansion and side
/// effects.
///
/// `extglob` is ambiguous here for the same reason bash's parser tracks it line
/// by line and this pass cannot: a `shopt -s extglob` earlier in the same chunk
/// changes how `@(…)` tokenizes. The caller therefore tries both settings and
/// accepts the conditional if either accepts it, so the pass never rejects
/// valid input.
pub(crate) fn validate(
    interp: &mut Interp,
    chunk: &Arc<ParsedChunk>,
    body: Range<usize>,
    extglob: bool,
) -> Result<(), (Range<usize>, String)> {
    let text = &chunk.source.bytes()[body.clone()];
    let tokens = conditional_tokens(text, extglob);
    let mut parser = Parser {
        interp,
        source: &chunk.source,
        text,
        tokens,
        pos: 0,
        span: body.clone(),
        base: body.start,
        validate: true,
    };
    let result = parser.or_expr();
    match result {
        Ok(_) if parser.pos >= parser.tokens.len() => Ok(()),
        Ok(_) => Err((parser.peek_span(), "unexpected token".to_owned())),
        Err(message) => Err((
            parser.span.clone(),
            if message.is_empty() {
                "syntax error in a conditional expression".to_owned()
            } else {
                message
            },
        )),
    }
}

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
        source: &chunk.source,
        text,
        tokens,
        pos: 0,
        span: body.clone(),
        base: body.start,
        validate: false,
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
    /// The chunk whose bytes `text` is a slice of: diagnostics are rendered
    /// against it, because nothing else knows which file a span belongs to.
    source: &'a Arc<SourceFile>,
    text: &'a [u8],
    tokens: Vec<CondToken>,
    pos: usize,
    span: Range<usize>,
    /// Where `text` starts inside the chunk: token ranges are relative to the
    /// body, spans in a diagnostic are relative to the chunk.
    base: usize,
    /// Parse the grammar without expanding operands or evaluating anything.
    ///
    /// The chunk-level validator uses this to reject a malformed conditional
    /// before any command in the chunk runs, which is what bash's parser does.
    validate: bool,
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
        if self.validate {
            return;
        }
        self.interp.report_in(
            self.source,
            crate::codes::CONDITIONAL,
            self.span.clone(),
            message,
        );
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

    /// The token at the cursor, as a span in the chunk, for a diagnostic.
    ///
    /// Tokens carry ranges into the conditional's body; a diagnostic wants the
    /// same bytes counted from the start of the chunk, which is what `base`
    /// shifts them by. The bare operators (`&&`, `||`, `(`, `)`) have no range
    /// of their own and fall back to the span the parser was handed.
    fn peek_span(&self) -> Range<usize> {
        let range = match self.peek() {
            Some(
                CondToken::Word(range)
                | CondToken::Bang(range)
                | CondToken::Eq(range)
                | CondToken::Ne(range)
                | CondToken::Match(range)
                | CondToken::Lt(range)
                | CondToken::Gt(range)
                | CondToken::Unexpected(range),
            ) => range.clone(),
            _ => return self.span.clone(),
        };
        self.base + range.start..self.base + range.end
    }

    fn take_word(&mut self) -> Option<Range<usize>> {
        // A comparison token in operand position is a word: bash accepts
        // `[[ = ]]`, `[[ == ]]` and `[[ -f == ]]` (a file test on `==`).
        let range = match self.peek()? {
            CondToken::Word(range)
            | CondToken::Eq(range)
            | CondToken::Ne(range)
            | CondToken::Match(range)
            | CondToken::Bang(range) => range.clone(),
            // `<` and `>` are never operands in bash: `[[ < ]]`,
            // `[[ -f < ]]` and `[[ a == < ]]` are all syntax errors.
            _ => return None,
        };
        self.pos += 1;
        self.span = self.base + range.start..self.base + range.end;
        Some(range)
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
        if matches!(self.peek(), Some(CondToken::Bang(_))) {
            self.pos += 1;
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
                CondToken::Eq(_) => Some(b"==".to_vec()),
                CondToken::Ne(_) => Some(b"!=".to_vec()),
                CondToken::Match(_) => Some(b"=~".to_vec()),
                CondToken::Lt(_) => Some(b"<".to_vec()),
                CondToken::Gt(_) => Some(b">".to_vec()),
                CondToken::Word(inner) if binary_operator(&self.text[inner.clone()]) => {
                    Some(self.text[inner].to_vec())
                }
                _ => None,
            };
            if let Some(operator) = comparison {
                self.span = self.peek_span();
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
        if self.validate {
            return Ok(Field::plain(Vec::new()));
        }
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
        if self.validate {
            return Ok(Field::plain(Vec::new()));
        }
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
        if self.validate {
            return Ok(Field::plain(Vec::new()));
        }
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
        if self.validate {
            return Ok(true);
        }
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
        if self.validate {
            return Ok(true);
        }
        let source = regex_source(pattern);
        let case_insensitive = self.interp.pattern_flags().nocasematch;
        // The interpreter's one-entry cache: `=~` in a loop re-evaluates the
        // same pattern every iteration, and compiling is the dominant cost.
        // `/regex-engine` selects which engine compiles it.
        let compiled = self
            .interp
            .regex_cache
            .captures(&source, case_insensitive, &text.bytes);
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
        match compiled {
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
        let path = || baash_sys::os::path_from_bytes(&operand.bytes);
        let metadata = || std::fs::metadata(path());
        let link = || std::fs::symlink_metadata(path());
        // A mounted tree is read-only shell data: `-e`, `-f`, `-d`, `-s` and
        // the access bits come from the table, and everything else in the tree
        // is not a device, a socket or a link.
        let mounted = self.interp.state.vfs.kind(&operand.bytes);
        let mounted_size = self.interp.state.vfs.file(&operand.bytes).map(<[u8]>::len);
        Ok(match operator {
            b"-z" => operand.bytes.is_empty(),
            b"-n" => !operand.bytes.is_empty(),
            b"-e" | b"-a" => mounted.is_some() || metadata().is_ok(),
            b"-f" => match mounted {
                Some(kind) => kind == vfs::Kind::File,
                None => metadata().is_ok_and(|meta| meta.is_file()),
            },
            b"-d" => match mounted {
                Some(kind) => kind == vfs::Kind::Dir,
                None => metadata().is_ok_and(|meta| meta.is_dir()),
            },
            b"-s" => match mounted_size {
                Some(len) => len > 0,
                None => metadata().is_ok_and(|meta| meta.len() > 0),
            },
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
                if let Some(kind) = mounted {
                    // Files in a mount are readable, a directory is also
                    // searchable, and nothing there is writable.
                    return Ok(operator != b"-w" && (operator != b"-x" || kind == vfs::Kind::Dir));
                }
                #[cfg(unix)]
                {
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
                #[cfg(not(unix))]
                {
                    // Windows has no permission bits; an existing file is as
                    // close as the test can get (recorded in
                    // `docs/portability.md` §9).
                    metadata().is_ok()
                }
            }
            // `[[ -o name ]]`: a shell option by the name `set -o` takes.
            // bash answers false for an unknown one instead of failing, so a
            // typo in a startup file cannot make a conditional a syntax error,
            // and `shopt` names are not options here.
            b"-o" => self
                .interp
                .state
                .opts
                .get_set_option(&operand.bytes)
                .unwrap_or(false),
            b"-v" => {
                // `-v name[subscript]` asks whether one element is set; a bare
                // name asks whether the variable exists at all (an array
                // counts). The subscript is arithmetic for an indexed array
                // and a literal key for an associative one.
                let bytes = &operand.bytes;
                match split_subscript(bytes) {
                    Some((name, subscript)) => {
                        if self.interp.state.vars.shape(name)
                            == Some(baash_runtime::ArrayShape::Associative)
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
/// `baash-pattern`'s matcher has no quote mask, but this one can have one because
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
