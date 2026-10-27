//! The seams back into `bsh-lang` and `bsh-syntax`.
//!
//! Two things live here:
//!
//! * **a word fragment expander.** `[[ … ]]` operands, `case` patterns and the
//!   `$…` constructs inside a here-document body are words, not chunks, so they
//!   are parsed with [`bsh_syntax::parse_word`] — the real front end, with the
//!   real quoting rules, and caller-relative spans.
//! * **here-document body expansion.** A body is not code, so it cannot be
//!   parsed as one. The scanner below copies literal bytes and hands each
//!   `$…`/`` `…` `` construct to the fragment expander, which is exactly the
//!   double-quote-like expansion POSIX specifies for an unquoted delimiter.
//!
//! Instrumentation is byte-oriented throughout: nothing decodes to `str`.

use bsh_lang::{Field, Mode, Word, expand_parameters, expand_tilde, expand_word_with};

use super::Interp;

impl Interp {
    /// Parse one word fragment out of shell text, or `None` when the text is
    /// not exactly one word.
    pub(crate) fn parse_fragment(&self, text: &[u8]) -> Option<Word> {
        bsh_syntax::parse_word(text, &self.parse_opts()).ok()
    }

    /// Expand a fragment as a scalar (tilde + parameter + quote removal, with
    /// no field splitting and no pathname expansion), keeping the per-byte
    /// quoting mask so `case` and `[[ … == pat ]]` know which metacharacters
    /// are active.
    pub(crate) fn expand_fragment(&mut self, text: &[u8]) -> Result<Field, bsh_lang::Error> {
        let Some(word) = self.parse_fragment(text) else {
            // Not a word (an unterminated construct, say). Treat the bytes as
            // literal text: fully quoted, so no metacharacter is active.
            return Ok(Field::matched(text.to_vec()));
        };
        self.expand_operand(&word)
    }

    /// Expand a [`Word`] to one [`Field`]: stages 1, 2 and 5 of the expansion
    /// pipeline, with no splitting and no globbing.
    pub(crate) fn expand_operand(&mut self, word: &Word) -> Result<Field, bsh_lang::Error> {
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
    pub(crate) fn expand_heredoc_body(&mut self, body: &[u8]) -> Result<Vec<u8>, bsh_lang::Error> {
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

    fn expand_construct(&mut self, text: &[u8]) -> Result<Vec<u8>, bsh_lang::Error> {
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
pub(crate) fn conditional_tokens(body: &[u8]) -> Vec<CondToken> {
    let mut tokens = Vec::new();
    let mut index = 0usize;
    while index < body.len() {
        let byte = body[index];
        if byte.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        let rest = &body[index..];
        let two = |token: CondToken| (token, 2usize);
        let (token, length) = if rest.starts_with(b"&&") {
            two(CondToken::And)
        } else if rest.starts_with(b"||") {
            two(CondToken::Or)
        } else if rest.starts_with(b"==") {
            two(CondToken::Eq)
        } else if rest.starts_with(b"=~") {
            two(CondToken::Match)
        } else if rest.starts_with(b"!=") {
            two(CondToken::Ne)
        } else if rest.starts_with(b"=") {
            (CondToken::Eq, 1)
        } else if rest.starts_with(b"!") {
            (CondToken::Bang, 1)
        } else if rest.starts_with(b"(") {
            (CondToken::LParen, 1)
        } else if rest.starts_with(b")") {
            (CondToken::RParen, 1)
        } else if rest.starts_with(b"<") {
            (CondToken::Lt, 1)
        } else if rest.starts_with(b">") {
            (CondToken::Gt, 1)
        } else {
            let length = word_end(body, index);
            tokens.push(CondToken::Word(index..index + length));
            index += length;
            continue;
        };
        tokens.push(token);
        index += length;
    }
    tokens
}

/// One `[[ … ]]` token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CondToken {
    /// A word, as a byte range into the conditional's body.
    Word(std::ops::Range<usize>),
    And,
    Or,
    Bang,
    LParen,
    RParen,
    Eq,
    Ne,
    Match,
    Lt,
    Gt,
}

/// The end of a conditional word: whitespace, parentheses and the comparison
/// operators end it; quotes, substitutions and escapes are skipped whole.
fn word_end(body: &[u8], start: usize) -> usize {
    let mut index = start;
    while index < body.len() {
        match body[index] {
            b'\\' => index += 2,
            b'\'' => index = skip_single(body, index).unwrap_or(body.len()),
            b'"' => index = skip_double(body, index).unwrap_or(body.len()),
            b'`' => index = construct_end(body, index).unwrap_or(index + 1),
            b'$' => index = construct_end(body, index).unwrap_or(index + 1),
            byte if byte.is_ascii_whitespace() || matches!(byte, b'(' | b')' | b'<' | b'>') => {
                break;
            }
            _ => index += 1,
        }
    }
    index.max(start + 1).min(body.len()) - start
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
    )
}
