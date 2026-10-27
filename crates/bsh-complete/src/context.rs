//! Where the cursor is: the word, the quote context, and the grammar's
//! expectation.
//!
//! Everything here comes from the front end. [`bsh_syntax::lex_full`] supplies
//! the token spans and the open constructs at the cursor;
//! [`bsh_syntax::expectations_at`] supplies the production the cursor is in.
//! The completer never splits a line on whitespace or guesses a word boundary.
//!
//! # How the context is derived
//!
//! 1. Lex `line[..cursor]` and find the innermost construct still open at the
//!    cursor. The lexer reports one entry per open construct it scanned, so an
//!    open quote hides a substitution inside it; the completer therefore
//!    re-enters the content of an open quote or `${…}` when that content holds
//!    an open `$(`, backtick or `<(`, and treats that substitution as a fresh
//!    shell-code region.
//! 2. In the innermost shell-code region, take the word token under the
//!    cursor. A redirection token carries its target when no space separates
//!    them, so the target span starts after the operator.
//! 3. Ask [`expectations_at`] about the *region* — not always the whole line,
//!    because a command substitution body is a separate parse.
//! 4. Walk the region's tokens backwards to the start of the simple command to
//!    find the command word and the argument index.
//!
//! # Why the region trick is needed
//!
//! `bsh-syntax`'s lexer scans a substitution's body but does not emit tokens
//! for it, and it reports only the first construct it finds still open. For
//! `echo "$(ls <TAB>` the lexer reports an open double quote, and
//! `expectations_at` answers `Word`, while the truth is "argument of `ls`
//! inside a command substitution". Re-lexing the quote's content restores the
//! right answer. The cost is that the completer has to know each construct's
//! opening delimiter length (see [`content_start`]), which the lexer does not
//! expose.
//!
//! # Documented fallbacks
//!
//! * [`expectations_at`] reports `Operator` inside `[[ … ]]` (the parser keeps
//!   the test body opaque), so an unclosed `[[` is detected from the tokens
//!   and the expectation is refined to [`Expect::TestOp`].
//! * [`expectations_at`] reports `Word` when a redirection target is already
//!   being typed. The preceding redirection operator is detected from the
//!   token stream and the expectation is refined to [`Expect::RedirectTarget`].
//! * A word that contains *closed* quotes before the cursor (for example
//!   `"a b"c`) is completed as one opaque word: the whole word is replaced and
//!   the raw text (quotes included) is matched. Only open quotes are tracked
//!   precisely.
//! * Shell keywords that the lexer classifies as plain words (such as the `in`
//!   of a `for` list) are not treated as command boundaries. The expectation
//!   already selects the right providers for those positions.
//! * The lexer keeps a redirection with its target as one token (`>foo`,
//!   `2>&1`), so the completer has to split the operator from the target
//!   itself ([`redirect_op_len`]) to know where the target word starts.
//! * [`expectations_at`] answers about the slot *after* the prefix it parses,
//!   so a partially typed word at command position (`ec`) is reported as
//!   `Word`, as if `ec` were already a finished command name. The completer
//!   asks about the word's start offset instead, which reports the slot the
//!   word occupies (`Command`), and only then applies its own refinements.

use bsh_lang::QuoteCtx;
use bsh_syntax::{Expect, ParseOpts, TokenKind, UnterminatedKind, expectations_at, lex_full};

use crate::candidate::Span;

/// How deep the completer will follow open substitutions.
///
/// A pathological line like `$($($($(…` would otherwise recurse once per
/// nesting level. Eight levels is far past anything a person types.
const MAX_NESTING: usize = 8;

/// Where in the grammar the cursor is, and what has been typed there.
///
/// Built by [`context`]; every provider reads it, and the editor receives the
/// parts it needs through [`crate::Menu`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ctx {
    /// The primary expectation, i.e. what may legally be typed at the cursor.
    pub expect: Expect,
    /// Every expectation [`expectations_at`] reported, outermost first.
    ///
    /// This is the parser's raw answer; [`Ctx::expect`] is the completer's
    /// refinement of it (redirect target, `[[` operator, quote context).
    pub expects: Vec<Expect>,
    /// The full byte span of the word being completed.
    ///
    /// This can extend past the cursor; [`Ctx::replace`] says what to replace.
    pub word: Span,
    /// The span a completion replaces: from after any opening quote or `=`
    /// through the end of the word.
    pub replace: Span,
    /// The bytes between the insertion point and the cursor, i.e. what the
    /// user has typed of this word.
    pub prefix: Vec<u8>,
    /// The quote context at the insertion point.
    pub quote: QuoteCtx,
    /// The command word of the simple command, if one is already present.
    pub command_name: Option<Vec<u8>>,
    /// Index the completed word occupies in the simple command: `0` is the
    /// command name, assignments and redirection targets do not count.
    pub arg_index: usize,
    /// Whether the word continues past the cursor.
    pub has_suffix: bool,
    /// Whether the word is a redirection target.
    pub redirect: bool,
    /// Whether the insertion point is inside `${ … }`.
    pub param: bool,
    /// Whether the insertion point is inside `$(( … ))` or `(( … ))`.
    pub arith: bool,
    /// Whether the insertion point is on a here-document delimiter line.
    pub heredoc: bool,
    /// Whether the word is the value half of a `NAME=` assignment.
    pub assign: bool,
    /// The shell-code region the cursor is in.
    ///
    /// The whole line for top-level code; the body of a substitution when the
    /// cursor is inside one.
    pub code: Span,
}

impl Ctx {
    /// The bytes the user has typed of this word, as a lossy UTF-8 string.
    ///
    /// For display only; insertion always uses [`Ctx::prefix`] bytes.
    pub fn prefix_lossy(&self) -> String {
        String::from_utf8_lossy(&self.prefix).into_owned()
    }
}

/// Work out the completion context for `cursor` in `line`.
///
/// `cursor` is clamped to the line. `opts` is passed straight to the parser, so
/// a `--posix` profile rejects bash syntax in the prefix exactly as it would at
/// the prompt.
pub fn context(line: &[u8], cursor: usize, opts: &ParseOpts) -> Ctx {
    let cursor = cursor.min(line.len());
    let located = locate_from(line, cursor, 0, 0);
    let in_construct =
        located.quote.is_some() || located.param.is_some() || located.arith.is_some();
    // Only top-level plain code can have text after the cursor; an open
    // construct extends to the end of the line by definition.
    let future = !in_construct && located.code.start == 0;

    let prefix_tokens = lex_region(line, located.code.start, cursor);
    let word_tokens = if future {
        lex_region(line, located.code.start, line.len())
    } else {
        prefix_tokens.clone()
    };
    let hit = word_hit(&word_tokens, line, cursor);

    let (word, mut redirect) = match hit {
        Some(hit) => (hit.span, hit.redirect),
        None => (cursor..cursor, false),
    };
    if !redirect {
        // The word itself ends at the cursor, so look for the redirect operator
        // *before* the word starts.
        redirect = follows_redirect(&prefix_tokens, line, word.start);
    }

    let mut replace_start = word.start;
    if let Some((_, content)) = located.quote {
        replace_start = content;
    } else if let Some(content) = located.param {
        replace_start = content;
    } else if let Some(content) = located.arith {
        replace_start = content;
    }
    // A quotation cannot begin after the cursor, but clamp anyway so that a
    // malformed line can never produce a reversed span.
    let mut replace_start = replace_start.min(cursor);

    // The command the cursor is an argument of comes from the tokens *before*
    // this word.
    let empty_word = word.start == cursor && word.end == cursor;
    let up_to = if empty_word { cursor } else { word.start };
    let command_tokens = lex_region(line, located.code.start, up_to);
    let (command_name, arg_index) = command_info(&command_tokens, line);

    // `FOO=/us<TAB>` completes the value, not the assignment name.
    let mut assign = false;
    if !in_construct && !redirect && arg_index == 0 && command_name.is_none() {
        let from = word.start.min(cursor);
        if let Some(offset) = assignment_value_start(&line[from..cursor]) {
            replace_start = (from + offset).min(cursor);
            assign = replace_start > from;
        }
    }

    // What the grammar expects here. The parse runs on the innermost code
    // region, which is the whole prefix unless the cursor is inside a
    // substitution.
    // Ask what may appear at the *start* of the word being completed, not at
    // the cursor: `expectations_at` parses the prefix it is given, so asking
    // about the cursor reports the slot *after* the partial word (`ec` would
    // answer `Word`, as if `ec` were already a finished command name). Asking
    // about the word's start reports the slot the word occupies.
    let asks_at = word.start.max(located.code.start) - located.code.start;
    let expects = expectations_at(&line[located.code.start..cursor], asks_at, opts);
    let mut expect = expects.last().copied().unwrap_or(Expect::Command);
    if located.quote.is_some() {
        expect = Expect::Word;
    }
    if located.param.is_some() {
        expect = Expect::ParamOp;
    }
    if located.arith.is_some() {
        expect = Expect::ArithExpr;
    }
    if located.heredoc {
        expect = Expect::HereDocDelimiter;
    }
    if redirect {
        expect = Expect::RedirectTarget;
    }
    if assign {
        expect = Expect::Word;
    }
    // The parser keeps a `[[ … ]]` body opaque and reports `Operator` inside
    // it; recover the real expectation from the tokens. In POSIX mode the
    // construct itself is rejected, so offering test operators would be wrong.
    if expect == Expect::Operator && !opts.posix_mode && in_test_command(&prefix_tokens, line) {
        expect = Expect::TestOp;
    }

    let prefix = line[replace_start..cursor].to_vec();
    let quote = located
        .quote
        .map(|(ctx, _)| ctx)
        .unwrap_or(QuoteCtx::Unquoted);

    Ctx {
        expect,
        expects,
        word: word.clone(),
        replace: replace_start..word.end,
        prefix,
        quote,
        command_name,
        arg_index,
        has_suffix: word.end > cursor,
        redirect,
        param: located.param.is_some(),
        arith: located.arith.is_some(),
        heredoc: located.heredoc,
        assign,
        code: located.code,
    }
}

/// The innermost context the cursor is in.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Located {
    /// Start of the enclosing shell-code region; `0..cursor` for top-level.
    code: Span,
    /// Quote kind and the byte offset just after the opening delimiter.
    quote: Option<(QuoteCtx, usize)>,
    /// Byte offset just after `${` when inside a parameter expansion.
    param: Option<usize>,
    /// Byte offset just after the arithmetic opener.
    arith: Option<usize>,
    /// Whether the cursor is on a here-document delimiter line.
    heredoc: bool,
}

impl Located {
    fn code(start: usize, cursor: usize) -> Self {
        Self {
            code: start..cursor,
            quote: None,
            param: None,
            arith: None,
            heredoc: false,
        }
    }
}

/// `start` is the beginning of the shell-code region under consideration.
fn locate_from(line: &[u8], cursor: usize, start: usize, depth: usize) -> Located {
    if depth > MAX_NESTING {
        return Located::code(start, cursor);
    }
    let Some(open) = innermost_open(line, start, cursor) else {
        return Located::code(start, cursor);
    };
    match open.kind {
        UnterminatedKind::CmdSub | UnterminatedKind::Backtick | UnterminatedKind::ProcSub => {
            let body = content_start(open.kind, open.opener.end).min(cursor);
            locate_from(line, cursor, body, depth + 1)
        }
        UnterminatedKind::ParamExp => {
            let body = content_start(open.kind, open.opener.end).min(cursor);
            match find_substitution(line, body, cursor, depth + 1) {
                Some(sub) => locate_from(line, cursor, sub, depth + 1),
                None => Located {
                    param: Some(body),
                    ..Located::code(start, cursor)
                },
            }
        }
        UnterminatedKind::SingleQuote
        | UnterminatedKind::DoubleQuote
        | UnterminatedKind::AnsiCQuote => {
            let body = content_start(open.kind, open.opener.end).min(cursor);
            match find_substitution(line, body, cursor, depth + 1) {
                Some(sub) => locate_from(line, cursor, sub, depth + 1),
                None => Located {
                    quote: Some((quote_ctx(open.kind), body)),
                    ..Located::code(start, cursor)
                },
            }
        }
        UnterminatedKind::Arith | UnterminatedKind::ArithCommand => Located {
            arith: Some(content_start(open.kind, open.opener.end).min(cursor)),
            ..Located::code(start, cursor)
        },
        UnterminatedKind::HereDoc => Located {
            heredoc: true,
            ..Located::code(start, cursor)
        },
    }
}

/// An open construct with absolute byte offsets.
#[derive(Clone, Debug)]
struct Open {
    kind: UnterminatedKind,
    opener: Span,
}

/// The innermost construct still open at the end of `line[start..cursor]`.
///
/// A here-document is included: its body is not shell code, so the completer
/// must know it is there in order to offer nothing. Its opener is reported at
/// the `<<`, which is before any body the cursor can be in.
fn innermost_open(line: &[u8], start: usize, cursor: usize) -> Option<Open> {
    if start >= cursor {
        return None;
    }
    let lexed = lex_full(&line[start..cursor]);
    lexed
        .unterminated
        .into_iter()
        .map(|u| Open {
            kind: u.kind,
            opener: (u.opener.start + start)..(u.opener.end + start),
        })
        .max_by_key(|open| open.opener.start)
}

/// Look for an open substitution inside a quoted or `${…}` body.
///
/// Only substitutions are followed here: a quote reported inside an open
/// double quote is an escaped `\"`, not a nested string, and following it would
/// move the insertion point past the escape.
fn find_substitution(line: &[u8], start: usize, cursor: usize, depth: usize) -> Option<usize> {
    if depth > MAX_NESTING || start >= cursor {
        return None;
    }
    let open = innermost_open(line, start, cursor)?;
    match open.kind {
        UnterminatedKind::CmdSub | UnterminatedKind::Backtick | UnterminatedKind::ProcSub => {
            Some(content_start(open.kind, open.opener.end).min(cursor))
        }
        UnterminatedKind::ParamExp
        | UnterminatedKind::SingleQuote
        | UnterminatedKind::DoubleQuote
        | UnterminatedKind::AnsiCQuote => {
            let body = content_start(open.kind, open.opener.end).min(cursor);
            find_substitution(line, body, cursor, depth + 1)
        }
        UnterminatedKind::Arith | UnterminatedKind::ArithCommand | UnterminatedKind::HereDoc => {
            None
        }
    }
}

/// The byte offset just after an open construct's opening delimiter.
///
/// `opener_end` is the end of [`bsh_syntax::Unterminated::opener`], which is
/// the `$` of `${`, `$(` and `$((`, the quote byte of `'`/`"`, the backtick of
/// `` ` ``, the `<`/`>` of a process substitution, and the whole `((` of an
/// arithmetic command. This mismatch is the parser API's sharpest edge for
/// completion: the lexer reports *where* a construct opened but not where its
/// content begins.
fn content_start(kind: UnterminatedKind, opener_end: usize) -> usize {
    opener_end
        + match kind {
            UnterminatedKind::SingleQuote
            | UnterminatedKind::DoubleQuote
            | UnterminatedKind::Backtick
            | UnterminatedKind::ArithCommand
            | UnterminatedKind::HereDoc => 0,
            UnterminatedKind::AnsiCQuote
            | UnterminatedKind::ParamExp
            | UnterminatedKind::CmdSub
            | UnterminatedKind::ProcSub => 1,
            UnterminatedKind::Arith => 2,
        }
}

fn quote_ctx(kind: UnterminatedKind) -> QuoteCtx {
    match kind {
        UnterminatedKind::DoubleQuote => QuoteCtx::DoubleQuoted,
        // `$'…'` is quote-removed like a single-quoted string; the `'\''`
        // dance that [`bsh_lang::quote`] emits for it still works because
        // adjacent quoted parts concatenate.
        UnterminatedKind::SingleQuote | UnterminatedKind::AnsiCQuote => QuoteCtx::SingleQuoted,
        // `find_substitution` only follows substitutions, so these cannot be
        // reported here.
        _ => QuoteCtx::Unquoted,
    }
}

/// A word token under the cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
struct WordHit {
    span: Span,
    redirect: bool,
}

/// Lex `line[start..end]`, returning absolute spans.
fn lex_region(line: &[u8], start: usize, end: usize) -> Vec<(TokenKind, Span)> {
    let end = end.min(line.len());
    let start = start.min(end);
    lex_full(&line[start..end])
        .tokens
        .into_iter()
        .map(|token| {
            (
                token.kind,
                (token.span.start + start)..(token.span.end + start),
            )
        })
        .collect()
}

/// Find the word token the cursor is completing.
fn word_hit(tokens: &[(TokenKind, Span)], line: &[u8], cursor: usize) -> Option<WordHit> {
    // A token that begins exactly at the cursor wins: the cursor is at the
    // start of a word that already has text after it.
    if let Some((kind, span)) = tokens
        .iter()
        .find(|(kind, span)| span.start == cursor && !kind.is_trivia())
    {
        return hit_of(*kind, span, line);
    }
    // Otherwise a token that strictly contains the cursor.
    if let Some((kind, span)) = tokens
        .iter()
        .find(|(kind, span)| span.start < cursor && cursor < span.end && !kind.is_trivia())
    {
        return hit_of(*kind, span, line);
    }
    // A token ending at the cursor, but only when nothing (not even
    // whitespace) starts there: at `echo ` the cursor is at a boundary and a
    // new word begins, while at the end of `echo` it is inside the word.
    if tokens.iter().any(|(_, span)| span.start == cursor) {
        return None;
    }
    let (kind, span) = tokens
        .iter()
        .rev()
        .find(|(kind, span)| span.end == cursor && !kind.is_trivia())?;
    hit_of(*kind, span, line)
}

fn hit_of(kind: TokenKind, span: &Span, line: &[u8]) -> Option<WordHit> {
    match kind {
        TokenKind::Word | TokenKind::WordUnterminated | TokenKind::Keyword => Some(WordHit {
            span: span.clone(),
            redirect: false,
        }),
        TokenKind::Redirect => {
            let text = &line[span.clone()];
            let op = redirect_op_len(text);
            if op >= text.len() {
                // The operator is the whole token: the target goes after it.
                Some(WordHit {
                    span: span.end..span.end,
                    redirect: true,
                })
            } else {
                Some(WordHit {
                    span: (span.start + op)..span.end,
                    redirect: true,
                })
            }
        }
        _ => None,
    }
}

/// Whether the empty word at the cursor follows a redirection operator.
fn follows_redirect(tokens: &[(TokenKind, Span)], line: &[u8], before: usize) -> bool {
    let previous = tokens
        .iter()
        .rev()
        .find(|(kind, span)| !kind.is_trivia() && span.end <= before);
    let Some((TokenKind::Redirect, span)) = previous else {
        return false;
    };
    let text = &line[span.clone()];
    redirect_op_len(text) >= text.len()
}

/// Length of a redirection token's operator prefix.
///
/// The lexer keeps `>foo` as one token, with the file descriptor, the operator
/// and (optionally) the target fused. This splits them.
fn redirect_op_len(text: &[u8]) -> usize {
    let mut i = 0;
    while i < text.len() && text[i].is_ascii_digit() {
        i += 1;
    }
    let rest = &text[i..];
    const OPS: &[&[u8]] = &[
        b"<<<", b"<<-", b"<<", b"<&", b"<>", b"<", b">>", b">&", b">|", b">", b"&>>", b"&>",
    ];
    for op in OPS {
        if rest.starts_with(op) {
            return i + op.len();
        }
    }
    text.len()
}

/// The offset just past the `=` of a `NAME=` prefix, if the text is one.
fn assignment_value_start(text: &[u8]) -> Option<usize> {
    let eq = text.iter().position(|&b| b == b'=')?;
    let name = &text[..eq];
    if name.is_empty() {
        return None;
    }
    let mut bytes = name.iter();
    let first = bytes.next()?;
    if !(first.is_ascii_alphabetic() || *first == b'_') {
        return None;
    }
    if !bytes.all(|b| b.is_ascii_alphanumeric() || *b == b'_') {
        return None;
    }
    Some(eq + 1)
}

/// Whether the cursor is inside an unclosed `[[ … ]]`.
fn in_test_command(tokens: &[(TokenKind, Span)], line: &[u8]) -> bool {
    let mut depth = 0usize;
    for (kind, span) in tokens {
        let text = &line[span.clone()];
        if *kind == TokenKind::Keyword && text == b"[[" {
            depth += 1;
        } else if text == b"]]" && depth > 0 {
            depth -= 1;
        }
    }
    depth > 0
}

/// The command word and argument index for the tokens before the completion
/// word.
fn command_info(tokens: &[(TokenKind, Span)], line: &[u8]) -> (Option<Vec<u8>>, usize) {
    let mut boundary = 0;
    for (index, (kind, span)) in tokens.iter().enumerate() {
        if starts_new_command(*kind, &line[span.clone()]) {
            boundary = index + 1;
        }
    }

    let mut command: Option<Vec<u8>> = None;
    let mut arguments = 0usize;
    let mut after_redirect = false;
    for (kind, span) in &tokens[boundary..] {
        if kind.is_trivia() || *kind == TokenKind::Comment {
            continue;
        }
        let text = &line[span.clone()];
        match kind {
            TokenKind::Redirect => {
                // A redirect that carries its target (`>file`) has no separate
                // target word to skip.
                after_redirect = redirect_op_len(text) >= text.len();
                continue;
            }
            TokenKind::Operator | TokenKind::Newline => {
                after_redirect = false;
                continue;
            }
            _ => {}
        }
        if after_redirect {
            after_redirect = false;
            continue;
        }
        if !is_word_token(*kind) {
            continue;
        }
        if command.is_none() && assignment_value_start(text).is_some() {
            continue;
        }
        if command.is_none() {
            command = Some(text.to_vec());
        } else {
            arguments += 1;
        }
    }

    let arg_index = if command.is_some() { arguments + 1 } else { 0 };
    (command, arg_index)
}

fn is_word_token(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Word | TokenKind::WordUnterminated | TokenKind::Keyword
    )
}

/// Whether a token begins a new simple command.
fn starts_new_command(kind: TokenKind, text: &[u8]) -> bool {
    match kind {
        TokenKind::Newline => true,
        TokenKind::Operator => matches!(
            text,
            b";" | b"&" | b"|" | b"||" | b"&&" | b"(" | b")" | b";;" | b";&" | b";;&" | b"{" | b"}"
        ),
        TokenKind::Keyword => matches!(
            text,
            b"if"
                | b"then"
                | b"else"
                | b"elif"
                | b"while"
                | b"until"
                | b"do"
                | b"for"
                | b"case"
                | b"select"
                | b"function"
                | b"time"
                | b"!"
                | b"[["
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(line: &str, cursor: usize) -> Ctx {
        context(line.as_bytes(), cursor, &ParseOpts::DEFAULT)
    }

    fn ctx_at_end(line: &str) -> Ctx {
        ctx(line, line.len())
    }

    #[test]
    fn empty_line_expects_a_command() {
        let c = ctx_at_end("");
        assert_eq!(c.expect, Expect::Command);
        assert_eq!(c.word, 0..0);
        assert_eq!(c.replace, 0..0);
        assert_eq!(c.prefix, b"");
        assert_eq!(c.command_name, None);
        assert_eq!(c.arg_index, 0);
        assert_eq!(c.quote, QuoteCtx::Unquoted);
    }

    #[test]
    fn command_position_word() {
        let c = ctx_at_end("ec");
        assert_eq!(c.expect, Expect::Command);
        assert_eq!(c.word, 0..2);
        assert_eq!(c.prefix, b"ec");
        assert_eq!(c.command_name, None);
        assert_eq!(c.arg_index, 0);
    }

    #[test]
    fn second_word_is_an_argument() {
        let c = ctx_at_end("git checkout");
        assert_eq!(c.expect, Expect::Word);
        assert_eq!(c.word, 4..12);
        assert_eq!(c.replace, 4..12);
        assert_eq!(c.prefix, b"checkout");
        assert_eq!(c.command_name.as_deref(), Some(&b"git"[..]));
        assert_eq!(c.arg_index, 1);
    }

    #[test]
    fn empty_trailing_word_has_no_prefix() {
        let c = ctx_at_end("git checkout ");
        assert_eq!(c.expect, Expect::Word);
        assert_eq!(c.word, 13..13);
        assert_eq!(c.prefix, b"");
        assert_eq!(c.command_name.as_deref(), Some(&b"git"[..]));
        assert_eq!(c.arg_index, 2);
    }

    #[test]
    fn new_command_after_a_separator() {
        let c = ctx_at_end("echo hi; ");
        assert_eq!(c.expect, Expect::Command);
        assert_eq!(c.command_name, None);
        assert_eq!(c.arg_index, 0);
    }

    #[test]
    fn redirect_with_missing_target() {
        let c = ctx_at_end("echo > ");
        assert_eq!(c.expect, Expect::RedirectTarget);
        assert!(c.redirect);
        assert_eq!(c.word, 7..7);
        assert_eq!(c.replace, 7..7);
    }

    #[test]
    fn redirect_with_partial_target() {
        let c = ctx_at_end("echo > fi");
        assert_eq!(c.expect, Expect::RedirectTarget);
        assert!(c.redirect);
        assert_eq!(c.word, 7..9);
        assert_eq!(c.prefix, b"fi");
        assert_eq!(c.command_name.as_deref(), Some(&b"echo"[..]));
        assert_eq!(c.arg_index, 1);
    }

    #[test]
    fn redirect_with_no_space_before_the_cursor() {
        let c = ctx_at_end("echo >");
        assert_eq!(c.expect, Expect::RedirectTarget);
        assert!(c.redirect);
        assert_eq!(c.word, 6..6);
        assert_eq!(c.replace, 6..6);
        assert_eq!(c.prefix, b"");
    }

    #[test]
    fn redirect_target_fused_with_the_operator() {
        let c = ctx_at_end("cat <fi");
        assert_eq!(c.expect, Expect::RedirectTarget);
        assert_eq!(c.word, 5..7);
        assert_eq!(c.replace, 5..7);
        assert_eq!(c.prefix, b"fi");
        assert_eq!(c.command_name.as_deref(), Some(&b"cat"[..]));
    }

    #[test]
    fn redirect_target_is_not_an_argument() {
        let c = ctx_at_end("echo > foo bar");
        assert!(!c.redirect);
        assert_eq!(c.word, 11..14);
        assert_eq!(c.command_name.as_deref(), Some(&b"echo"[..]));
        assert_eq!(c.arg_index, 1);
    }

    #[test]
    fn parameter_expansion() {
        let c = ctx_at_end("echo ${");
        assert_eq!(c.expect, Expect::ParamOp);
        assert!(c.param);
        assert_eq!(c.word, 5..7);
        assert_eq!(c.replace, 7..7);
        assert_eq!(c.prefix, b"");
        assert_eq!(c.quote, QuoteCtx::Unquoted);
    }

    #[test]
    fn parameter_expansion_name_prefix() {
        let c = ctx_at_end("echo ${x");
        assert_eq!(c.expect, Expect::ParamOp);
        assert_eq!(c.replace, 7..8);
        assert_eq!(c.prefix, b"x");
    }

    #[test]
    fn for_list() {
        let c = ctx_at_end("for f in ");
        assert_eq!(c.expect, Expect::ForList);
        assert_eq!(c.word, 9..9);
        assert_eq!(c.replace, 9..9);
    }

    #[test]
    fn for_list_partial_word() {
        let c = ctx_at_end("for f in sr");
        assert_eq!(c.expect, Expect::ForList);
        assert_eq!(c.word, 9..11);
        assert_eq!(c.prefix, b"sr");
    }

    #[test]
    fn case_pattern() {
        let c = ctx_at_end("case $x in ");
        assert_eq!(c.expect, Expect::CasePattern);
    }

    #[test]
    fn double_quoted_string() {
        let c = ctx_at_end("echo \"foo");
        assert_eq!(c.expect, Expect::Word);
        assert_eq!(c.quote, QuoteCtx::DoubleQuoted);
        assert_eq!(c.word, 5..9);
        assert_eq!(c.replace, 6..9);
        assert_eq!(c.prefix, b"foo");
        assert_eq!(c.command_name.as_deref(), Some(&b"echo"[..]));
        assert_eq!(c.arg_index, 1);
    }

    #[test]
    fn single_quoted_string() {
        let c = ctx_at_end("echo 'foo");
        assert_eq!(c.quote, QuoteCtx::SingleQuoted);
        assert_eq!(c.replace, 6..9);
        assert_eq!(c.prefix, b"foo");
    }

    #[test]
    fn quote_after_a_prefix_keeps_the_prefix() {
        let c = ctx_at_end("echo foo\"bar");
        assert_eq!(c.quote, QuoteCtx::DoubleQuoted);
        assert_eq!(c.word, 5..12);
        assert_eq!(c.replace, 9..12);
        assert_eq!(c.prefix, b"bar");
    }

    #[test]
    fn command_substitution_is_a_fresh_command_position() {
        let c = ctx_at_end("echo $(ec");
        assert_eq!(c.expect, Expect::Command);
        assert_eq!(c.code.start, 7);
        assert_eq!(c.word, 7..9);
        assert_eq!(c.prefix, b"ec");
        assert_eq!(c.command_name, None);
    }

    #[test]
    fn command_substitution_argument() {
        let c = ctx_at_end("echo $(ls ");
        assert_eq!(c.expect, Expect::Word);
        assert_eq!(c.command_name.as_deref(), Some(&b"ls"[..]));
        assert_eq!(c.arg_index, 1);
        assert_eq!(c.replace, 10..10);
    }

    #[test]
    fn substitution_inside_double_quotes_is_still_code() {
        let c = ctx_at_end("echo \"$(ls ");
        assert_eq!(c.expect, Expect::Word);
        assert_eq!(c.quote, QuoteCtx::Unquoted);
        assert_eq!(c.command_name.as_deref(), Some(&b"ls"[..]));
        assert_eq!(c.arg_index, 1);
    }

    #[test]
    fn backtick_substitution() {
        let c = ctx_at_end("echo `ec");
        assert_eq!(c.expect, Expect::Command);
        assert_eq!(c.replace, 6..8);
        assert_eq!(c.prefix, b"ec");
    }

    #[test]
    fn here_document_body_is_not_shell_code() {
        let c = ctx_at_end("cat <<EOF\nhello ");
        assert_eq!(c.expect, Expect::HereDocDelimiter);
        assert!(c.heredoc);
    }

    #[test]
    fn arithmetic_expression() {
        let c = ctx_at_end("echo $((x");
        assert_eq!(c.expect, Expect::ArithExpr);
        assert!(c.arith);
        assert_eq!(c.replace, 8..9);
        assert_eq!(c.prefix, b"x");
    }

    #[test]
    fn test_command_operators_are_a_fallback() {
        let c = ctx_at_end("[[ -");
        assert_eq!(c.expect, Expect::TestOp);
        assert_eq!(c.prefix, b"-");
    }

    #[test]
    fn closed_test_command_is_not_a_test_context() {
        let c = ctx_at_end("[[ -f x ]] ");
        assert_eq!(c.expect, Expect::Command);
    }

    #[test]
    fn assignment_value_is_where_completion_starts() {
        let c = ctx_at_end("PATH=/us");
        assert_eq!(c.expect, Expect::Word);
        assert_eq!(c.word, 0..8);
        assert_eq!(c.replace, 5..8);
        assert_eq!(c.prefix, b"/us");
        assert!(c.assign);
        assert_eq!(c.command_name, None);
    }

    #[test]
    fn assignment_then_command() {
        let c = ctx_at_end("FOO=1 bar x");
        assert_eq!(c.command_name.as_deref(), Some(&b"bar"[..]));
        assert_eq!(c.arg_index, 1);
        assert_eq!(c.word, 10..11);
    }

    #[test]
    fn wrapper_commands_keep_their_argument_index() {
        let c = ctx_at_end("sudo git ch");
        assert_eq!(c.command_name.as_deref(), Some(&b"sudo"[..]));
        assert_eq!(c.arg_index, 2);
        assert_eq!(c.prefix, b"ch");
    }

    #[test]
    fn cursor_inside_a_word_replaces_the_whole_word() {
        let c = ctx("git checkout", 6);
        assert_eq!(c.word, 4..12);
        assert_eq!(c.replace, 4..12);
        assert_eq!(c.prefix, b"ch");
        assert!(c.has_suffix);
    }

    #[test]
    fn cursor_at_the_end_of_a_completed_word_before_a_space() {
        let c = ctx("echo hi", 4);
        assert_eq!(c.word, 4..4);
        assert_eq!(c.prefix, b"");
        // The next word after `echo` is an argument.
        assert_eq!(c.expect, Expect::Word);
        assert_eq!(c.command_name.as_deref(), Some(&b"echo"[..]));
        assert_eq!(c.arg_index, 1);
    }

    #[test]
    fn unterminated_quote_does_not_panic_and_points_inside_the_quote() {
        let c = ctx_at_end("ls 'unterminated");
        assert_eq!(c.quote, QuoteCtx::SingleQuoted);
        assert_eq!(c.replace.start, 4);
        assert_eq!(c.prefix, b"unterminated");
    }

    #[test]
    fn cursor_is_clamped() {
        let c = ctx("echo", 99);
        assert_eq!(c.word, 0..4);
        assert_eq!(c.prefix, b"echo");
    }

    #[test]
    fn ansi_c_quote_is_single_quoted() {
        let c = ctx_at_end("echo $'ab");
        assert_eq!(c.quote, QuoteCtx::SingleQuoted);
        assert_eq!(c.prefix, b"ab");
    }

    #[test]
    fn redirect_op_lengths() {
        assert_eq!(redirect_op_len(b">"), 1);
        assert_eq!(redirect_op_len(b">>"), 2);
        assert_eq!(redirect_op_len(b"2>&1"), 3);
        assert_eq!(redirect_op_len(b"&>>"), 3);
        assert_eq!(redirect_op_len(b"<<-"), 3);
        assert_eq!(redirect_op_len(b"<<<"), 3);
        assert_eq!(redirect_op_len(b"<fi"), 1);
        assert_eq!(redirect_op_len(b">out"), 1);
    }

    #[test]
    fn prefix_lossy_is_available_for_display() {
        let c = ctx_at_end("git ch");
        assert_eq!(c.prefix_lossy(), "ch");
    }
}
