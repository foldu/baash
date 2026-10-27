//! Front end: a tolerant lexer, spans, and the parse outcomes.
//!
//! Two design commitments live here and are easy to retrofit badly, so they are
//! stated up front (see `DESIGN.md` §4.10 and §6.3):
//!
//! * **Tolerance.** Lexing never fails. Unterminated constructs, partial lines
//!   and mid-keystroke input are normal inputs, not errors, because the line
//!   editor and the completer both run on them continuously.
//! * **Expectations.** The parser records which production it is inside at
//!   every rule push, so a truncated line can be asked what is legal at a byte
//!   offset. That one mechanism serves error messages, `PS2` continuation and
//!   completion.
//!
//! Status: the token model, the [`Expect`] vocabulary and the [`ParseOutcome`]
//! shape are settled. [`lex`] is a first slice that splits on whitespace,
//! newlines, comments and operator characters — it does **not** yet track
//! quoting, `$( … )` nesting or `$'…'`, and it never emits
//! [`TokenKind::WordUnterminated`]. The parser itself is not written.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use bsh_diag::{Diagnostic, SourceFile, Span};

/// Diagnostic codes produced by this crate.
pub mod codes {
    use bsh_diag::ErrCode;

    /// A quoted string, substitution or here-document was never closed.
    pub const UNTERMINATED: ErrCode = ErrCode::new(1);
    /// Input ended while a construct was still open.
    pub const NEED_MORE_INPUT: ErrCode = ErrCode::new(2);
    /// A token appeared where the grammar does not allow it.
    pub const UNEXPECTED: ErrCode = ErrCode::new(3);
}

/// Token classes that both the parser and the highlighter care about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    /// Spaces or tabs.
    Whitespace,
    /// A newline; a command separator.
    Newline,
    /// `# …` to end of line.
    Comment,
    /// A word that is also a reserved word, e.g. `if`, `then`, `esac`.
    Keyword,
    /// A plain word.
    Word,
    /// A word containing an unterminated quote or substitution.
    WordUnterminated,
    /// `|`, `||`, `&&`, `;`, `&`, `(`, `)`.
    Operator,
    /// A redirection operator, e.g. `>`, `>>`, `2>&1`.
    Redirect,
}

impl TokenKind {
    /// Whether the grammar ignores this token. Comments are trivia for parsing
    /// but are still worth colouring.
    pub fn is_trivia(self) -> bool {
        self.is_layout() || self == TokenKind::Comment
    }

    /// Whether this token is pure layout: invisible, never styled.
    pub fn is_layout(self) -> bool {
        matches!(self, TokenKind::Whitespace | TokenKind::Newline)
    }
}

/// A lexed token: a class and a byte span into the source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// What kind of token this is.
    pub kind: TokenKind,
    /// Byte range into the source buffer.
    pub span: Span,
}

impl Token {
    /// The token's source text.
    pub fn text<'s>(&self, src: &'s [u8]) -> &'s [u8] {
        &src[self.span.start..self.span.end]
    }
}

/// Reserved words that the lexer classifies for highlighting. Membership is
/// deliberately generous: over-classifying a word as a keyword only changes its
/// colour, while the parser decides what actually applies.
const RESERVED: &[&[u8]] = &[
    b"if",
    b"then",
    b"else",
    b"elif",
    b"fi",
    b"for",
    b"while",
    b"until",
    b"do",
    b"done",
    b"case",
    b"esac",
    b"in",
    b"function",
    b"select",
    b"time",
    b"!",
    b"{",
    b"}",
    b"[[",
    b"]]",
];

/// What may legally appear at a byte offset. Maps 1:1 to a completion provider
/// set (`DESIGN.md` §6.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect {
    /// A command name position.
    Command,
    /// An argument or other word.
    Word,
    /// The target of a redirection.
    RedirectTarget,
    /// The word list of a `for` loop.
    ForList,
    /// A `case` pattern.
    CasePattern,
    /// An operator inside `${ … }`.
    ParamOp,
    /// A `[[ … ]]` operator.
    TestOp,
    /// An arithmetic expression.
    ArithExpr,
}

/// Parsed source. A placeholder until the real parser lands.
#[derive(Clone, Debug, Default)]
pub struct Ast {
    /// The token stream the AST was built from, kept for tooling.
    pub tokens: Vec<Token>,
}

/// The outcome of parsing a possibly-truncated piece of source.
#[derive(Clone, Debug)]
pub enum ParseOutcome {
    /// The input is a complete chunk.
    Ok(Ast),
    /// The input is fine but incomplete; the caller should read more and retry.
    /// This is what drives `PS2` continuation, and it is *not* an error.
    NeedMoreInput {
        /// The span of the construct left open.
        open: Span,
        /// What would be legal next.
        expected: Vec<Expect>,
    },
    /// The input is rejected.
    Failed(Vec<Diagnostic>),
}

/// Split `src` into tokens.
///
/// First slice: whitespace, newlines, `#` comments, operator and redirection
/// characters, and words. Quoting and substitutions are not yet tracked, so a
/// `#` inside a word is only a comment when it starts the word, and an
/// apostrophe is just an ordinary byte.
pub fn lex(src: &[u8]) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < src.len() {
        let start = i;
        match src[i] {
            b' ' | b'\t' => {
                while i < src.len() && matches!(src[i], b' ' | b'\t') {
                    i += 1;
                }
                tokens.push(Token {
                    kind: TokenKind::Whitespace,
                    span: start..i,
                });
            }
            b'\n' => {
                i += 1;
                tokens.push(Token {
                    kind: TokenKind::Newline,
                    span: start..i,
                });
            }
            b'#' => {
                while i < src.len() && src[i] != b'\n' {
                    i += 1;
                }
                tokens.push(Token {
                    kind: TokenKind::Comment,
                    span: start..i,
                });
            }
            b'<' | b'>' => {
                while i < src.len() && matches!(src[i], b'<' | b'>' | b'&' | b'-') {
                    i += 1;
                }
                tokens.push(Token {
                    kind: TokenKind::Redirect,
                    span: start..i,
                });
            }
            b'|' | b';' | b'&' | b'(' | b')' => {
                while i < src.len() && matches!(src[i], b'|' | b';' | b'&') {
                    i += 1;
                }
                tokens.push(Token {
                    kind: TokenKind::Operator,
                    span: start..i,
                });
            }
            _ => {
                while i < src.len()
                    && !matches!(
                        src[i],
                        b' ' | b'\t' | b'\n' | b'<' | b'>' | b'|' | b';' | b'&' | b'(' | b')'
                    )
                {
                    i += 1;
                }
                let text = &src[start..i];
                let kind = if RESERVED.contains(&text) {
                    TokenKind::Keyword
                } else {
                    TokenKind::Word
                };
                tokens.push(Token {
                    kind,
                    span: start..i,
                });
            }
        }
    }
    tokens
}

/// Parse a source file. Placeholder: complete input is accepted, everything
/// else is reported as needing more input.
pub fn parse(src: &SourceFile) -> ParseOutcome {
    let tokens = lex(src.bytes());
    let open = tokens
        .iter()
        .find(|t| t.kind == TokenKind::WordUnterminated);
    match open {
        Some(t) => ParseOutcome::NeedMoreInput {
            open: t.span.clone(),
            expected: vec![Expect::Word],
        },
        None => ParseOutcome::Ok(Ast { tokens }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &[u8]) -> Vec<TokenKind> {
        lex(src).into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn lexes_a_simple_command() {
        assert_eq!(
            kinds(b"echo hi"),
            vec![TokenKind::Word, TokenKind::Whitespace, TokenKind::Word]
        );
    }

    #[test]
    fn comment_only_at_word_start() {
        assert_eq!(kinds(b"# hi"), vec![TokenKind::Comment]);
        // `a#b` is one word, not a word plus a comment.
        assert_eq!(kinds(b"a#b"), vec![TokenKind::Word]);
    }

    #[test]
    fn recognises_reserved_words() {
        assert_eq!(kinds(b"if"), vec![TokenKind::Keyword]);
        // As a prefix of a longer word it is not a keyword.
        assert_eq!(kinds(b"ifconfig"), vec![TokenKind::Word]);
    }

    #[test]
    fn operators_and_redirects() {
        assert_eq!(
            kinds(b"a && b >> c"),
            vec![
                TokenKind::Word,
                TokenKind::Whitespace,
                TokenKind::Operator,
                TokenKind::Whitespace,
                TokenKind::Word,
                TokenKind::Whitespace,
                TokenKind::Redirect,
                TokenKind::Whitespace,
                TokenKind::Word,
            ]
        );
    }

    #[test]
    fn spans_cover_the_input_exactly() {
        let mut src = Vec::new();
        for input in [
            &b"echo hi"[..],
            &b"  # c\n"[..],
            &b"a|b;c&&d"[..],
            &b""[..],
            &b"\xff\xfe binary"[..],
        ] {
            let tokens = lex(input);
            let mut at = 0;
            for t in &tokens {
                assert_eq!(t.span.start, at, "gap or overlap in {input:?}");
                at = t.span.end;
            }
            assert_eq!(at, input.len(), "tokens do not cover {input:?}");
            src.extend_from_slice(input);
        }
        assert!(!src.is_empty());
    }

    #[test]
    fn parse_accepts_complete_input() {
        let src = SourceFile::new("-c", &b"echo hi"[..]);
        assert!(matches!(parse(&src), ParseOutcome::Ok(_)));
    }
}
