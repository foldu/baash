//! The seams back into `baash-lang` and `baash-syntax`.
//!
//! Two things live here:
//!
//! * **a word fragment expander.** `[[ … ]]` operands, `case` patterns and the
//!   `$…` constructs inside a here-document body are words, not chunks, so they
//!   are parsed with [`baash_syntax::parse_word`] — the real front end, with the
//!   real quoting rules, and caller-relative spans.
//! * **here-document body expansion.** A body is not code, so it cannot be
//!   parsed as one. The scanner below copies literal bytes and hands each
//!   `$…`/`` `…` `` construct to the fragment expander, which is exactly the
//!   double-quote-like expansion POSIX specifies for an unquoted delimiter.
//!
//! Instrumentation is byte-oriented throughout: nothing decodes to `str`.

use baash_lang::{Field, Mode, Word, expand_parameters, expand_tilde, expand_word_with};
use baash_runtime::ShellHost;

use super::Interp;

impl Interp {
    /// Parse one word fragment out of shell text, or `None` when the text is
    /// not exactly one word.
    pub(crate) fn parse_fragment(&self, text: &[u8]) -> Option<Word> {
        baash_syntax::parse_word(text, &self.parse_opts()).ok()
    }

    /// Parse the text between `[` and `]` of an array element *assignment*.
    ///
    /// `unset` and `declare` receive the operand as one byte string, but a
    /// subscript may span several lexer tokens: `a[1 + 2]` and an associative
    /// key with spaces (`a[x y]`) are both legal, and neither is one
    /// `parse_word` token. The assignment grammar already stitches those spans
    /// back together, so parse a synthetic `name[<text>]=x` and lift its
    /// subscript word out. The text is expanded by the caller through the
    /// ordinary assignment pipeline, so quoting, escapes and `$…` expansion
    /// behave exactly as they do in `a[<text>]=v`.
    pub(crate) fn parse_subscript_word(&self, text: &[u8]) -> Option<Word> {
        let mut source = Vec::with_capacity(text.len() + 12);
        source.extend_from_slice(b"__baash[");
        source.extend_from_slice(text);
        source.extend_from_slice(b"]=x");
        let ast = match baash_syntax::parse_bytes(&source, &self.parse_opts()) {
            baash_syntax::ParseOutcome::Ok(ast) => ast,
            _ => return None,
        };
        ast.nodes.iter().find_map(|node| match &node.kind {
            baash_syntax::NodeKind::Simple(command) => command
                .assignments
                .first()
                .and_then(|assign| assign.index.clone()),
            _ => None,
        })
    }

    /// Expand an array subscript the way an element assignment does.
    ///
    /// `a[1 + 2]` is one assignment subscript but several lexer tokens, which
    /// is why the assignment grammar is the fallback parser (the same rule
    /// `unset 'a[…]'` uses).
    pub(crate) fn expand_subscript(&mut self, subscript: &[u8]) -> Result<Vec<u8>, ()> {
        let word = match baash_syntax::parse_word(subscript, &self.parse_opts()) {
            Ok(word) => word,
            Err(_) => match self.parse_subscript_word(subscript) {
                Some(word) => word,
                None => return Err(()),
            },
        };
        match baash_lang::expand_assignment(&word, self) {
            Ok(text) => Ok(text),
            Err(error) => {
                self.diagnose(&error.diagnostic());
                Err(())
            }
        }
    }

    /// Expand a fragment as a scalar (tilde + parameter + quote removal, with
    /// no field splitting and no pathname expansion), keeping the per-byte
    /// quoting mask so `case` and `[[ … == pat ]]` know which metacharacters
    /// are active.
    pub(crate) fn expand_fragment(&mut self, text: &[u8]) -> Result<Field, baash_lang::Error> {
        let Some(word) = self.parse_fragment(text) else {
            // Not a word (an unterminated construct, say). Treat the bytes as
            // literal text: fully quoted, so no metacharacter is active.
            return Ok(Field::matched(text.to_vec()));
        };
        self.expand_operand(&word)
    }

    /// Expand the right-hand side of `=~`.
    ///
    /// Unlike `expand_fragment`, a text that is not a well-formed shell word
    /// does **not** become a fully quoted literal: `(a)*` legal regex is rarely
    /// a legal shell word, and treating it as literal text would silently turn
    /// every group into an escaped parenthesis. The bytes stay pattern text.
    pub(crate) fn expand_regex_fragment(
        &mut self,
        text: &[u8],
    ) -> Result<Field, baash_lang::Error> {
        match self.parse_fragment(text) {
            Some(word) => self.expand_operand(&word),
            None => Ok(Field::plain(text.to_vec())),
        }
    }

    /// Expand one `[[ … == PAT ]]` right-hand operand.
    ///
    /// The conditional's body is raw text, so an extglob group never became a
    /// parsed word: `parse_word` stops at the `(`. When the text holds one,
    /// split it at the group's structural bytes, expand each literal stretch
    /// through the ordinary pipeline, and stitch the operators back in with no
    /// quoting — so `@(a|b)` reaches the matcher as an active extglob while
    /// `'@(a|b)'` still becomes a quoted literal.
    pub(crate) fn expand_pattern_fragment(
        &mut self,
        text: &[u8],
    ) -> Result<Field, baash_lang::Error> {
        let Some(segments) = pattern_segments(text) else {
            return self.expand_fragment(text);
        };
        let mut bytes = Vec::new();
        let mut quoted = Vec::new();
        for segment in segments {
            match segment {
                Segment::Raw(raw) => {
                    bytes.extend_from_slice(raw);
                    quoted.resize(quoted.len() + raw.len(), false);
                }
                Segment::Text(range) => {
                    let field = self.expand_pattern_text(&text[range])?;
                    bytes.extend_from_slice(&field.bytes);
                    quoted.extend_from_slice(&field.quoted);
                }
            }
        }
        Ok(Field::new(bytes, quoted))
    }

    /// Expand one literal stretch of a `[[ … ]]` pattern.
    ///
    /// A pattern arm may contain unquoted spaces (`@(a b|foo)`), which the
    /// fragment parser reads as the end of the word. Expand each run between
    /// them separately and keep the separator bytes active: a space is a
    /// literal in a pattern, so it needs no quoting.
    fn expand_pattern_text(&mut self, text: &[u8]) -> Result<Field, baash_lang::Error> {
        if text.is_empty() || self.parse_fragment(text).is_some() {
            return self.expand_fragment(text);
        }
        let mut bytes = Vec::new();
        let mut quoted = Vec::new();
        let mut index = 0usize;
        while index < text.len() {
            let start = index;
            while index < text.len() && !text[index].is_ascii_whitespace() {
                index += 1;
            }
            if start == index {
                bytes.push(text[index]);
                quoted.push(false);
                index += 1;
                continue;
            }
            let field = self.expand_fragment(&text[start..index])?;
            bytes.extend_from_slice(&field.bytes);
            quoted.extend_from_slice(&field.quoted);
        }
        Ok(Field::new(bytes, quoted))
    }

    /// Expand a [`Word`] to one [`Field`]: stages 1, 2 and 5 of the expansion
    /// pipeline, with no splitting and no globbing.
    pub(crate) fn expand_operand(&mut self, word: &Word) -> Result<Field, baash_lang::Error> {
        let word = expand_tilde(word, self)?;
        let expanded = expand_parameters(&word, self, Mode::Assignment)?;
        let mut bytes = Vec::new();
        let mut quoted = Vec::new();
        let mut first = true;
        for chunk in &expanded.chunks {
            if chunk.break_before && !first {
                bytes.push(b' ');
                quoted.push(true);
            }
            for &byte in &chunk.bytes {
                bytes.push(byte);
                quoted.push(chunk.quoted);
            }
            first = false;
        }
        Ok(Field::new(bytes, quoted))
    }

    /// Expand a here-document body that is subject to expansion (an unquoted
    /// delimiter).
    ///
    /// Backslash escapes `$`, `` ` ``, `\` and newline, as in a double-quoted
    /// string; everything else is literal. Each `$…`/`` `… `` construct is
    /// expanded through the ordinary word pipeline, so `${x:-"a b"}` and
    /// `$(cmd "arg")` work exactly as they do elsewhere.
    pub(crate) fn expand_heredoc_body(
        &mut self,
        body: &[u8],
    ) -> Result<Vec<u8>, baash_lang::Error> {
        let mut out = Vec::with_capacity(body.len());
        let mut literal = 0usize;
        let mut index = 0usize;
        while index < body.len() {
            match body[index] {
                b'\\' if index + 1 < body.len() => {
                    let next = body[index + 1];
                    if matches!(next, b'$' | b'`' | b'\\') {
                        out.extend_from_slice(&body[literal..index]);
                        out.push(next);
                        index += 2;
                        literal = index;
                    } else if next == b'\n' {
                        out.extend_from_slice(&body[literal..index]);
                        index += 2;
                        literal = index;
                    } else {
                        index += 1;
                    }
                }
                b'$' | b'`' => match construct_end(body, index) {
                    Some(end) => {
                        out.extend_from_slice(&body[literal..index]);
                        let expanded = self.expand_construct(&body[index..end])?;
                        out.extend_from_slice(&expanded);
                        index = end;
                        literal = index;
                    }
                    None => index += 1,
                },
                _ => index += 1,
            }
        }
        out.extend_from_slice(&body[literal..]);
        Ok(out)
    }

    fn expand_construct(&mut self, text: &[u8]) -> Result<Vec<u8>, baash_lang::Error> {
        let Some(word) = self.parse_fragment(text) else {
            return Ok(text.to_vec());
        };
        let mut fields = expand_word_with(&word, self, Mode::Assignment)?;
        Ok(fields.pop().unwrap_or_default())
    }
}

/// The end (exclusive) of the construct starting at `start`, which points at
/// `$` or a backtick. `None` when the bytes there are a literal `$`.
pub(crate) fn construct_end(src: &[u8], start: usize) -> Option<usize> {
    let rest = &src[start..];
    match rest.first()? {
        b'`' => {
            let mut index = 1;
            while index < rest.len() {
                match rest[index] {
                    b'\\' => index += 2,
                    b'`' => return Some(start + index + 1),
                    _ => index += 1,
                }
            }
            None
        }
        b'$' => match rest.get(1)? {
            b'$' | b'?' | b'#' | b'!' | b'-' | b'@' | b'*' | b'0'..=b'9' => Some(start + 2),
            b'{' => Some(start + scan_balanced(rest, 1, b'{', b'}')?),
            b'(' => Some(start + scan_parens(rest, 1)),
            byte if *byte == b'_' || byte.is_ascii_alphabetic() => {
                let length = 1 + rest[1..]
                    .iter()
                    .take_while(|b| **b == b'_' || b.is_ascii_alphanumeric())
                    .count();
                Some(start + length)
            }
            _ => None,
        },
        _ => None,
    }
}

/// Scan `${ … }` from the `{` at `open`, counting nested `${`.
fn scan_balanced(rest: &[u8], open: usize, start: u8, end: u8) -> Option<usize> {
    let mut depth = 0usize;
    let mut index = open;
    while index < rest.len() {
        match rest[index] {
            b'\\' => index += 2,
            b'\'' => index = skip_single(rest, index)?,
            b'"' => index = skip_double(rest, index)?,
            byte if byte == start => {
                depth += 1;
                index += 1;
            }
            byte if byte == end => {
                depth -= 1;
                index += 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => index += 1,
        }
    }
    None
}

/// Scan `$( … )` from the `(` at `open`.
fn scan_parens(rest: &[u8], open: usize) -> usize {
    let mut depth = 0usize;
    let mut index = open;
    while index < rest.len() {
        match rest[index] {
            b'\\' => index += 2,
            b'\'' => index = skip_single(rest, index).unwrap_or(rest.len()),
            b'"' => index = skip_double(rest, index).unwrap_or(rest.len()),
            b'(' => {
                depth += 1;
                index += 1;
            }
            b')' => {
                depth -= 1;
                index += 1;
                if depth == 0 {
                    return index;
                }
            }
            _ => index += 1,
        }
    }
    rest.len()
}

fn skip_single(rest: &[u8], open: usize) -> Option<usize> {
    let mut index = open + 1;
    while index < rest.len() {
        if rest[index] == b'\'' {
            return Some(index + 1);
        }
        index += 1;
    }
    None
}

fn skip_double(rest: &[u8], open: usize) -> Option<usize> {
    let mut index = open + 1;
    while index < rest.len() {
        match rest[index] {
            b'\\' => index += 2,
            b'"' => return Some(index + 1),
            _ => index += 1,
        }
    }
    None
}

/// Split a `[[ … ]]` body into conditional tokens. Words keep their raw
/// source text so the caller can expand them with the word pipeline.
///
/// The tokenizer tracks one bit of context: after `==`/`!=` bash reads the
/// right-hand side as a *pattern*, where `@(…)`, `?(…)`, `*(…)`, `+(…)` and
/// `!(…)` are extglob groups — the parentheses belong to the word, not to a
/// `[[ ]]` grouping. With `extglob` on, the same operators start a word in
/// operand position (`[[ !(a) ]]` is then "is the word `!(a)` non-empty");
/// with it off, `!(` is negation of a group.
pub(crate) fn conditional_tokens(body: &[u8], extglob: bool) -> Vec<CondToken> {
    let mut tokens = Vec::new();
    let mut index = 0usize;
    let mut pattern_context = false;
    // After `=~` the operand is one regex word, and `(`/`)` belong to it: bash
    // reads `[[ x =~ (a|b) ]]` as a pattern, not as a parenthesised
    // expression, and tokens splitting them would leave the operator with no
    // right-hand side at all.
    let mut regex_operand = false;
    while index < body.len() {
        let byte = body[index];
        if byte.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        let rest = &body[index..];
        if regex_operand {
            let length = regex_word_end(body, index);
            tokens.push(CondToken::Word(index..index + length));
            index += length;
            regex_operand = false;
            continue;
        }
        // A pattern operand may begin with an extglob operator, and so may an
        // ordinary operand when the dialect is on; `!` is an operator
        // character everywhere else.
        if (pattern_context || extglob) && starts_extglob(rest) {
            let length = word_end(body, index, true);
            tokens.push(CondToken::Word(index..index + length));
            index += length;
            pattern_context = false;
            continue;
        }
        let two = |token: CondToken| (token, 2usize);
        let (token, length) = if rest.starts_with(b"&&") {
            two(CondToken::And)
        } else if rest.starts_with(b"||") {
            two(CondToken::Or)
        } else if rest.starts_with(b"==") {
            two(CondToken::Eq(index..index + 2))
        } else if rest.starts_with(b"=~") {
            two(CondToken::Match(index..index + 2))
        } else if rest.starts_with(b"!=") {
            two(CondToken::Ne(index..index + 2))
        } else if rest.starts_with(b"=") {
            (CondToken::Eq(index..index + 1), 1)
        } else if rest.starts_with(b"!") {
            (CondToken::Bang(index..index + 1), 1)
        } else if rest.starts_with(b"(") {
            (CondToken::LParen, 1)
        } else if rest.starts_with(b")") {
            (CondToken::RParen, 1)
        } else if rest.starts_with(b"<") {
            (CondToken::Lt(index..index + 1), 1)
        } else if rest.starts_with(b">") {
            (CondToken::Gt(index..index + 1), 1)
        } else if rest.starts_with(b"|") || rest.starts_with(b"&") || rest.starts_with(b";") {
            tokens.push(CondToken::Unexpected(index..index + 1));
            index += 1;
            continue;
        } else {
            let length = word_end(body, index, pattern_context);
            tokens.push(CondToken::Word(index..index + length));
            index += length;
            pattern_context = false;
            continue;
        };
        pattern_context = matches!(token, CondToken::Eq(_) | CondToken::Ne(_));
        regex_operand = matches!(token, CondToken::Match(_));
        tokens.push(token);
        index += length;
    }
    tokens
}

/// End of a `=~` right-hand side: everything to the next unquoted whitespace
/// that is not inside a parenthesised group.
///
/// Quotes, escapes and substitutions are skipped whole so a space inside them
/// does not end the operand, and bash's `( … )` groups are the one place an
/// unquoted space belongs to the regex: `[[ "a  b" =~ (a  b) ]]` is valid
/// while `[[ "a  b" =~ a  b ]]` is a syntax error, and `[a  b]` is not a
/// group either (a bracket expression has no spaces in it).
fn regex_word_end(body: &[u8], start: usize) -> usize {
    let mut index = start;
    let mut depth = 0usize;
    while index < body.len() {
        match body[index] {
            byte if byte.is_ascii_whitespace() => {
                if depth == 0 {
                    break;
                }
                index += 1;
                continue;
            }
            // A group opens a region where spaces are part of the pattern; a
            // `)` at the top level is just a byte, and `a{1,2}` stays whole.
            b'(' => {
                depth += 1;
                index += 1;
                continue;
            }
            b')' if depth == 0 => break,
            b')' => {
                depth -= 1;
                index += 1;
                continue;
            }
            // `;` and `&` are conditional operators outside a group, so they
            // end the operand there (`[[ ';' =~ ; ]]` is a syntax error) but
            // belong to the pattern inside one (`[[ ';' =~ (;) ]]`).
            b';' | b'&' if depth == 0 => break,
            b'\\' => index += 2,
            b'\'' => match body[index + 1..].iter().position(|&byte| byte == b'\'') {
                Some(offset) => index += offset + 2,
                None => return body.len() - start,
            },
            b'"' => {
                index += 1;
                while index < body.len() && body[index] != b'"' {
                    if body[index] == b'\\' {
                        index += 1;
                    }
                    index += 1;
                }
                index += 1;
            }
            b'$' if body.get(index + 1) == Some(&b'(') => {
                // Skip a command substitution or an arithmetic expansion as a
                // unit: its closing paren does not end the operand.
                let mut depth = 0usize;
                while index < body.len() {
                    match body[index] {
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 {
                                index += 1;
                                break;
                            }
                        }
                        b'\\' => index += 1,
                        _ => {}
                    }
                    index += 1;
                }
            }
            _ => index += 1,
        }
        index = index.max(start);
    }
    index.min(body.len()) - start
}

/// Whether `rest` begins with one of the five extglob operators.
fn starts_extglob(rest: &[u8]) -> bool {
    matches!(rest, [b'?' | b'*' | b'+' | b'@' | b'!', b'(', ..])
}

/// One piece of a `[[ … == PAT ]]` operand: source text to expand, or extglob
/// punctuation that must stay active.
enum Segment<'a> {
    /// Operator bytes (`@(`, `|`, `)`, …), passed through unquoted.
    Raw(&'a [u8]),
    /// A range of the operand to expand with the ordinary pipeline.
    Text(std::ops::Range<usize>),
}

/// Split a pattern operand at its extglob punctuation.
///
/// Returns `None` when there is no well-formed extglob group, in which case
/// the whole operand is an ordinary word. Quotes, escapes and substitutions
/// are skipped whole, so a quoted `|` never splits and `"$(echo a|b)"` stays
/// one stretch.
fn pattern_segments(text: &[u8]) -> Option<Vec<Segment<'_>>> {
    let mut segments = Vec::new();
    let mut literal = 0usize;
    let mut index = 0usize;
    let mut depth = 0usize;
    let mut found = false;
    while index < text.len() {
        let byte = text[index];
        match byte {
            b'\\' => {
                index += 2;
                continue;
            }
            b'\'' => {
                index = skip_single(text, index).unwrap_or(text.len());
                continue;
            }
            b'"' => {
                index = skip_double(text, index).unwrap_or(text.len());
                continue;
            }
            b'`' | b'$' => {
                index = construct_end(text, index).unwrap_or(index + 1);
                continue;
            }
            _ => {}
        }
        let operator = depth == 0
            && matches!(byte, b'?' | b'*' | b'+' | b'@' | b'!')
            && text.get(index + 1) == Some(&b'(');
        let nested = depth > 0 && matches!(byte, b'(' | b')' | b'|');
        if !operator && !nested {
            index += 1;
            continue;
        }
        found = true;
        if literal < index {
            segments.push(Segment::Text(literal..index));
        }
        let length = if operator { 2 } else { 1 };
        segments.push(Segment::Raw(&text[index..index + length]));
        if operator || byte == b'(' {
            depth += 1;
        } else if byte == b')' {
            depth = depth.saturating_sub(1);
        }
        index += length;
        literal = index;
    }
    if !found || depth != 0 {
        return None;
    }
    if literal < text.len() {
        segments.push(Segment::Text(literal..text.len()));
    }
    Some(segments)
}

/// One `[[ … ]]` token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CondToken {
    /// A word, as a byte range into the conditional's body.
    Word(std::ops::Range<usize>),
    And,
    Or,
    Bang(std::ops::Range<usize>),
    LParen,
    RParen,
    // The comparison tokens keep their range because they are *operands* when
    // they appear in operand position: `[[ = ]]` is the non-empty word `=`,
    // and `[[ -f == ]]` tests the file named `==`.
    Eq(std::ops::Range<usize>),
    Ne(std::ops::Range<usize>),
    Match(std::ops::Range<usize>),
    Lt(std::ops::Range<usize>),
    Gt(std::ops::Range<usize>),
    /// A control operator that has no meaning inside a conditional (`|`, `&`,
    /// `;` on its own). It is kept as a token rather than part of a word so the
    /// grammar reports it, as bash does.
    Unexpected(std::ops::Range<usize>),
}

/// The end of a conditional word: whitespace, parentheses, the comparison
/// operators and the control operators end it; quotes, substitutions and
/// escapes are skipped whole.
///
/// With `extglob`, a `X(…)` group is part of the word: `[[ x == a@(b|c) ]]`
/// is one operand, not `a@` followed by a group.
fn word_end(body: &[u8], start: usize, extglob: bool) -> usize {
    let mut index = start;
    while index < body.len() {
        match body[index] {
            b'\\' => index += 2,
            b'\'' => index = skip_single(body, index).unwrap_or(body.len()),
            b'"' => index = skip_double(body, index).unwrap_or(body.len()),
            b'`' => index = construct_end(body, index).unwrap_or(index + 1),
            b'$' => index = construct_end(body, index).unwrap_or(index + 1),
            byte if extglob
                && matches!(byte, b'?' | b'*' | b'+' | b'@' | b'!')
                && body.get(index + 1) == Some(&b'(') =>
            {
                index = extglob_end(body, index + 1);
            }
            byte if byte.is_ascii_whitespace()
                || matches!(byte, b'(' | b')' | b'<' | b'>' | b'|' | b'&' | b';') =>
            {
                break;
            }
            _ => index += 1,
        }
    }
    index.max(start + 1).min(body.len()) - start
}

/// The offset just past the `)` that closes the group opened at `open`.
///
/// Quotes and escapes are skipped so a `)` inside them does not close the
/// group; an unterminated group runs to the end of the body, which the matcher
/// then treats as literal text.
fn extglob_end(body: &[u8], open: usize) -> usize {
    let mut depth = 0usize;
    let mut index = open;
    while index < body.len() {
        match body[index] {
            b'\\' => index += 2,
            b'\'' => index = skip_single(body, index).unwrap_or(body.len()),
            b'"' => index = skip_double(body, index).unwrap_or(body.len()),
            b'`' => index = construct_end(body, index).unwrap_or(index + 1),
            b'$' => index = construct_end(body, index).unwrap_or(index + 1),
            b'(' => {
                depth += 1;
                index += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                index += 1;
                if depth == 0 {
                    return index;
                }
            }
            _ => index += 1,
        }
    }
    body.len()
}

/// Whether `word` denotes one of the two-operand test operators.
pub(crate) fn binary_operator(word: &[u8]) -> bool {
    matches!(
        word,
        b"-eq" | b"-ne" | b"-lt" | b"-le" | b"-gt" | b"-ge" | b"==" | b"=" | b"!=" | b"=~"
    )
}

/// Whether `word` denotes a one-operand test operator.
pub(crate) fn unary_operator(word: &[u8]) -> bool {
    matches!(
        word,
        b"-a"
            | b"-b"
            | b"-c"
            | b"-d"
            | b"-e"
            | b"-f"
            | b"-g"
            | b"-h"
            | b"-k"
            | b"-L"
            | b"-N"
            | b"-O"
            | b"-p"
            | b"-r"
            | b"-S"
            | b"-s"
            | b"-t"
            | b"-u"
            | b"-v"
            | b"-w"
            | b"-x"
            | b"-z"
            | b"-n"
            | b"-o"
    )
}
