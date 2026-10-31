//! Front end: a tolerant lexer, a flat syntax tree, and expectations.
//!
//! Three commitments shape this crate, and all three are stated up front
//! because they are expensive to retrofit (`DESIGN.md` §4.2, §4.10, §6.3):
//!
//! * **Tolerance.** Lexing never fails and parsing a truncated line is a
//!   normal outcome ([`ParseOutcome::NeedMoreInput`]), not an error. The line
//!   editor and the completer both run on input that is not finished yet.
//! * **Expectations.** Every rule records which production it is inside, so
//!   [`expectations_at`] can answer "what is valid at this byte offset?" for
//!   completion, for `PS2` continuation and for error messages — one
//!   mechanism, three consumers.
//! * **Lossless.** Token spans partition the input exactly, for every input
//!   including invalid UTF-8. Concatenating the tokens reproduces the source
//!   byte for byte, which is what makes a formatter possible later.
//!
//! Shell text is bytes, never `str`: every structure here is `&[u8]`/`Vec<u8>`
//! or a byte span, and UTF-8 decoding happens only where a human reads a
//! message.
//!
//! # Entry points
//!
//! ```
//! use bsh_diag::SourceFile;
//! use bsh_syntax::{parse, ParseOutcome};
//!
//! let src = SourceFile::new("-c", &b"if true; then echo hi; fi"[..]);
//! assert!(matches!(parse(&src), ParseOutcome::Ok(_)));
//!
//! let src = SourceFile::new("-c", &b"if true; then echo"[..]);
//! assert!(matches!(parse(&src), ParseOutcome::NeedMoreInput { .. }));
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod ast;
mod lexer;
mod parser;
mod role;

pub use ast::{
    AndOrLink, AndOrOp, ArrayArg, ArrayElement, ArrayIndex, Assignment, Ast, CaseArm, CaseTerm,
    IfBranch, ListItem, Node, NodeId, NodeKind, PipeOp, Redirect, RedirectOp, Separator,
    SimpleCommand,
};
pub use bsh_lang::{
    Arithmetic, CaseHow, CmdSubStyle, CommandSub, DoubleQ, Lit, LitKind, ParamExp, ParamName,
    ParamOp, Part, ProcSubDir, ProcessSub, ReplaceMode, TildeSpec, Transform, UnsupportedOp, Word,
};
pub use lexer::{
    HereDoc, Lexed, OpenConstruct, Token, TokenKind, Unterminated, UnterminatedKind, lex, lex_full,
    open_constructs_at, redirect_op_len,
};
pub use parser::{
    Expect, ExpectSite, ParseOpts, ParseOutcome, ParseWordError, expectation_sites_at,
    expectations_after, expectations_at, parse, parse_bytes, parse_with, parse_word, parse_word_at,
};
pub use role::{TokenRole, classify_tokens};

/// Whether the word at `span` has the shape of a variable assignment
/// (`name=value`, `name+=value`, `name[sub]=value`).
///
/// The VM uses this for the arguments of the declaration built-ins:
/// `export x=$v` must expand `$v` as an *assignment value* (one field, no
/// globbing), while an ordinary argument still splits.
pub fn is_assignment_word(src: &[u8], span: &bsh_diag::Span) -> bool {
    lexer::assignment_value_start(src, span).is_some()
}

/// The byte offset just past the `=` of an assignment word, i.e. where its
/// value begins. `None` when the word is not an assignment.
///
/// The declaration built-ins need the boundary: their operand arrives as one
/// word, but `declare x=~/p` expands the tilde at the start of the *value*
/// only, so the front end and the VM must agree on where the value starts.
pub fn assignment_value_start(src: &[u8], span: &bsh_diag::Span) -> Option<usize> {
    lexer::assignment_value_start(src, span)
}

/// Diagnostic codes produced by this crate.
///
/// The code is stable; the message is not. Anything matching on stderr should
/// match the code (`DESIGN.md` §4.10, point 5).
pub mod codes {
    use bsh_diag::ErrCode;

    /// A quoted string, substitution or here-document was never closed.
    pub const UNTERMINATED: ErrCode = ErrCode::new(1);
    /// Input ended while a construct was still open. This drives `PS2`
    /// continuation and is normally reported as
    /// [`crate::ParseOutcome::NeedMoreInput`], not as an error; the code exists
    /// for callers that must turn "incomplete" into a failure.
    pub const NEED_MORE_INPUT: ErrCode = ErrCode::new(2);
    /// A token appeared where the grammar does not allow it, or the expected
    /// token was missing.
    pub const UNEXPECTED: ErrCode = ErrCode::new(3);
    /// A bash-only construct was rejected because `posix_mode` is on.
    pub const BASHISM: ErrCode = ErrCode::new(4);
    /// An array subscript was not a single word, or contains a command
    /// substitution that would be re-parsed as code.
    pub const ARRAY_SUBSCRIPT: ErrCode = ErrCode::new(5);
    /// Nesting exceeded [`crate::ParseOpts::max_depth`].
    pub const DEPTH: ErrCode = ErrCode::new(6);
    /// A here-document is malformed, e.g. `<<` with no delimiter word.
    pub const HERE_DOC: ErrCode = ErrCode::new(7);
}

#[cfg(test)]
mod tests {
    use super::*;
    use bsh_diag::SourceFile;

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
        // Not in command position: an argument.
        assert_eq!(
            kinds(b"echo if"),
            vec![TokenKind::Word, TokenKind::Whitespace, TokenKind::Word]
        );
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
        let mut seen = Vec::new();
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
            seen.extend_from_slice(input);
        }
        assert!(!seen.is_empty());
    }

    #[test]
    fn parse_accepts_complete_input() {
        let src = SourceFile::new("-c", &b"echo hi"[..]);
        assert!(matches!(parse(&src), ParseOutcome::Ok(_)));
    }
}
