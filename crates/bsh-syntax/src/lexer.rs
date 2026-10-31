//! The lexer: shell bytes to a flat, lossless token stream.
//!
//! The lexer is **tolerant**. It never fails, never reports an error and never
//! gives up on a token: a quote that runs to end of input becomes a
//! [`TokenKind::WordUnterminated`] spanning the rest of the buffer, and every
//! construct left open is recorded in [`Lexed::unterminated`] and, as a nesting
//! stack, in [`Lexed::open`]. That is what lets the highlighter and the
//! completer run on every keystroke of a half-typed line (`DESIGN.md` §6.10,
//! §6.3).
//!
//! The nesting stack is maintained during the one lexing pass: when a
//! construct opens it is pushed, and when it closes it is popped. At end of
//! input what remains is the chain of open constructs, innermost first, each
//! with its opener's span and the byte where its content begins. So
//! `echo "$(ls ` gives `[CmdSub, DoubleQuote]`: the cursor is in a command
//! substitution inside a quoted word. A word containing a substitution is
//! still *one token* — the stack is the nested view, not a second token
//! stream.
//!
//! Two invariants hold for every input, including invalid UTF-8:
//!
//! 1. **Lossless.** Token spans partition the input exactly: the first token
//!    starts at 0, each token starts where the previous ended, and the last
//!    ends at `src.len()`. Concatenating every token's text reproduces the
//!    input byte for byte.
//! 2. **No interpretation.** Token text is raw. Quotes, escapes and
//!    substitutions are *scanned* (so the token boundary and the unterminated
//!    position are right) but never expanded or decoded here.
//!
//! Here-document bodies are not shell code, so they are not tokenised as
//! shell code: the body becomes one [`TokenKind::HereDocBody`] token and the
//! terminating line one [`TokenKind::HereDocDelimiter`] token. [`Lexed`]
//! additionally reports each here-document's operator, delimiter, body span
//! and whether the delimiter line was ever found, so the parser can attach
//! bodies to the right redirections without re-scanning.
//!
//! The lexer commits to the bash readings of the ambiguous constructs listed
//! in `DESIGN.md` §3.3 (Tier A): `((` is arithmetic, `$'…'` is ANSI-C quoting,
//! `<(` is process substitution, `<<<` is a here-string. Profiles do not
//! change the token stream; they only gate acceptance afterwards, in the
//! parser.
//!
//! # Extglob groups
//!
//! The five extglob operators — `?(` `*(` `+(` `@(` `!(` — are lexed into one
//! word **unconditionally**, whether or not `shopt -s extglob` was in effect
//! for the containing script. The bytes stay in the word exactly as written
//! and with their original quoting, because the pattern matcher (`bsh-pattern`)
//! consumes active pattern text: `@("a b"|c)` reaches it as the group
//! `@(` `"a b"` `|c)`, whose quoted stretch stays literal.
//!
//! This is the superset-grammar rule of `DESIGN.md` §3.1/§3.3 applied to a
//! construct bash resolves at parse time. Bash cannot defer the decision: it
//! chooses between `@(` and `@` `(` while tokenising, so with `shopt -u
//! extglob` a closed group is a syntax error. ßhell parses the whole script in
//! one pass, so the option is not available to the lexer — a later
//! `shopt -s extglob` could not retroactively re-lex an earlier command. The
//! lexer therefore takes the superset reading and lets the *matcher* decide
//! semantics from `shopt` at run time, which is where `case`, `${…}` and
//! pathname expansion already get their dialect (see `docs/contracts.md`,
//! `bsh-pattern`'s wiring rules).
//!
//! The visible deviation: with extglob off, `echo @(a)` prints `@(a)` where
//! bash reports a syntax error. We are more permissive on purpose — a stray
//! `@(` is a literal rather than a parse failure — and `[[ x == pat ]]`
//! already forces extglob semantics on, so it is unaffected.
//!
//! The same reading settles the `!(` / `! (` ambiguity from `DESIGN.md` §3.3
//! by spelling, not by the option: `!(` with no space is always an extglob
//! group, so pipeline negation of a subshell must be written `! (…)` with a
//! space. Bash *without* `shopt -s extglob` accepts the unspaced `!(…)` as
//! negation; we do not, because an unspaced `!(` is the operator that the
//! matcher and every extglob pattern need. This is a second, deliberate
//! deviation of the superset parse, not a bug in the scan.
//!
//! A group that never closes is **not** an error: the attempted `(` is simply
//! left as an ordinary word byte and the normal scan continues (`@(a` is the
//! one word `@(a`). That is the same tolerant reading the matcher takes of a
//! malformed pattern, and it keeps the lexer from inventing a diagnostic for
//! an unterminated group.

use bsh_diag::Span;
use memchr::memchr;

use crate::ast::RedirectOp;

/// Deepest nesting of substitutions the lexer will follow before treating the
/// word as unterminated. This is a stack-overflow guard, not a grammar limit.
pub(crate) const MAX_NEST: usize = 64;

/// What a lexer pass produced.
#[derive(Clone, Debug, Default)]
pub struct Lexed {
    /// The token stream. Spans partition the input exactly.
    pub tokens: Vec<Token>,
    /// Every here-document, in source order. The parser consumes this list in
    /// order as it meets `<<` and `<<-` redirections.
    pub heredocs: Vec<HereDoc>,
    /// Every construct still open at end of input, in source order. There is
    /// normally at most one entry from a word; an unterminated here-document
    /// adds its own entry.
    pub unterminated: Vec<Unterminated>,
    /// The chain of constructs still open at end of input, **innermost
    /// first**.
    ///
    /// This is the nesting stack the lexer maintains during its single pass:
    /// for `echo "$(ls ` it is `[CmdSub(…), DoubleQuote(…)]`, so a consumer can
    /// see that the cursor is inside a command substitution that is itself
    /// inside a double-quoted word. The chain is empty when the input is
    /// complete.
    ///
    /// Each entry carries the opener's span and the offset where the
    /// construct's *content* begins, so no consumer has to encode per-kind
    /// opener widths.
    pub open: Vec<OpenConstruct>,
}

/// A here-document as found by the lexer.
///
/// The body is a raw span: for `<<-` the leading tabs are still in the source
/// and the consumer strips them (see [`HereDoc::strip_tabs`]). `body` excludes
/// the delimiting line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HereDoc {
    /// The `<<` or `<<-` operator (with any file-descriptor prefix).
    pub op: Span,
    /// The delimiter word exactly as written, including quotes.
    ///
    /// An empty span means the here-document had no delimiter word at all,
    /// which is a syntax error the parser reports.
    pub delimiter: Span,
    /// The delimiter after quote removal, i.e. the bytes a body line must
    /// equal to end the here-document.
    pub delimiter_text: Vec<u8>,
    /// Whether the delimiter word was quoted, which suppresses expansion in
    /// the body.
    pub quoted: bool,
    /// `true` for `<<-`: leading tabs are stripped from body lines and from
    /// the delimiting line before comparison.
    pub strip_tabs: bool,
    /// The body bytes, excluding the delimiting line.
    pub body: Span,
    /// Whether the delimiting line was found before end of input.
    pub terminated: bool,
}

/// A construct left open at end of input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unterminated {
    /// The span of the affected token (normally the rest of the input).
    pub span: Span,
    /// Where the open construct began: the full opening delimiter — the quote
    /// byte of `'`/`"`, or the `$'`, `$"`, `${`, `$(`, `$((`, `<(` or `>(`
    /// digraph, the whole `((` of an arithmetic command, or the `<<`/`<<-`
    /// operator of a here-document (including any file-descriptor prefix).
    pub opener: Span,
    /// Where the construct's content begins: the byte just past the opening
    /// delimiter. A consumer needs no per-kind width table.
    pub content_start: usize,
    /// What kind of construct is open.
    ///
    /// When one construct contains another (`echo "$(ls `), this is the
    /// **innermost** one; [`Lexed::open`] carries the whole chain.
    pub kind: UnterminatedKind,
}

/// One construct still open at end of input, with the byte where its content
/// begins.
///
/// Returned by [`open_constructs_at`] and [`Lexed::open`], innermost first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenConstruct {
    /// What is open.
    pub kind: UnterminatedKind,
    /// The full opening delimiter: `'`, `"`, `$'`, `$"`, `${`, `$(`, `$((`,
    /// `<(`/`>(`, `((`, a backtick, or a here-document's `<<`/`<<-` operator.
    pub opener: Span,
    /// The byte just past the opening delimiter: the first byte of the
    /// construct's content. For a quote this is the first quoted byte; for a
    /// substitution it is the first byte of the body; for a here-document it
    /// is the first byte of the body (or end of input when no body started).
    pub content_start: usize,
}

/// Which construct [`Unterminated`] describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnterminatedKind {
    /// `'…'` with no closing quote.
    SingleQuote,
    /// `"…"` with no closing quote.
    DoubleQuote,
    /// `$'…'` with no closing quote.
    AnsiCQuote,
    /// `` `…` `` with no closing backtick.
    Backtick,
    /// `${…}` with no closing brace.
    ParamExp,
    /// `$( … )` with no closing parenthesis.
    CmdSub,
    /// `$(( … ))` with no closing `))`.
    Arith,
    /// `(( … ))` with no closing `))`.
    ArithCommand,
    /// `<( … )` or `>( … )` with no closing parenthesis.
    ProcSub,
    /// A here-document whose delimiter line never arrived.
    HereDoc,
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

    /// For a [`TokenKind::Redirect`] token, the byte length of its
    /// file-descriptor prefix plus its redirection operator.
    ///
    /// The lexer keeps a redirection and a target written hard against it
    /// (`>out`, `2>&1`, `&>>log`) in one token. This accessor is the one place
    /// that knows where the operator ends, so consumers never re-derive the
    /// operator set:
    ///
    /// * `src[span.start .. span.start + len]` is the descriptor and operator;
    /// * `src[span.start + len .. span.end]` is the target, empty when it was
    ///   written separately.
    ///
    /// Returns `None` for every other token kind.
    ///
    /// ```
    /// use bsh_syntax::{TokenKind, lex};
    /// let src = b"echo 2>&1";
    /// let token = &lex(src)[2];
    /// assert_eq!(token.kind, TokenKind::Redirect);
    /// assert_eq!(token.redirect_op_len(src), Some(3));
    /// assert_eq!(token.redirect_target(src), Some(8..9));
    /// ```
    pub fn redirect_op_len(&self, src: &[u8]) -> Option<usize> {
        if self.kind != TokenKind::Redirect {
            return None;
        }
        redirect_op_len(self.text(src))
    }

    /// For a [`TokenKind::Redirect`] token, the span of the target written
    /// hard against the operator, if there is one.
    ///
    /// `None` means the token is only the operator and any target is a
    /// separate word.
    pub fn redirect_target(&self, src: &[u8]) -> Option<Span> {
        let op = self.redirect_op_len(src)?;
        let start = self.span.start + op;
        (start < self.span.end).then_some(start..self.span.end)
    }

    /// For a word token, the offset just past an assignment's `=`, when the
    /// word is a valid `NAME=value`, `NAME+=value` or `NAME[sub]=value`
    /// assignment.
    ///
    /// `None` for non-word tokens and for words that are not assignments.
    pub fn assignment_value_start(&self, src: &[u8]) -> Option<usize> {
        if !matches!(
            self.kind,
            TokenKind::Word | TokenKind::WordUnterminated | TokenKind::Keyword
        ) {
            return None;
        }
        assignment_value_start(src, &self.span)
    }
}

/// The byte length of the file-descriptor prefix plus the redirection
/// operator at the start of `text`, or `None` when `text` does not begin with
/// one.
///
/// This is the single definition of the redirection operator set; both the
/// lexer and [`Token::redirect_op_len`] use it.
///
/// ```
/// use bsh_syntax::redirect_op_len;
/// assert_eq!(redirect_op_len(b">>"), Some(2));
/// assert_eq!(redirect_op_len(b"2>&1"), Some(3));
/// assert_eq!(redirect_op_len(b"&>>"), Some(3));
/// assert_eq!(redirect_op_len(b"<<-"), Some(3));
/// assert_eq!(redirect_op_len(b"foo"), None);
/// ```
pub fn redirect_op_len(text: &[u8]) -> Option<usize> {
    let mut digits = 0;
    while digits < text.len() && text[digits].is_ascii_digit() {
        digits += 1;
    }
    let rest = &text[digits..];
    REDIRECT_OPS
        .iter()
        .find(|(bytes, _)| rest.starts_with(bytes))
        .map(|(bytes, _)| digits + bytes.len())
}

/// A token class.
///
/// The variants are the distinctions the parser and the highlighter actually
/// use; see [`TokenKind::is_trivia`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    /// Spaces or tabs.
    Whitespace,
    /// A newline; a command separator.
    Newline,
    /// `# …` to end of line (only at the start of a word).
    Comment,
    /// A word that is also a reserved word in command position, e.g. `if`.
    Keyword,
    /// A plain word.
    Word,
    /// A word containing an unterminated quote or substitution. It spans to
    /// end of input.
    WordUnterminated,
    /// An operator: `|`, `||`, `&&`, `;`, `;;`, `;&`, `;;&`, `&`, `(`, `)`,
    /// `{`, `}`.
    Operator,
    /// A redirection, e.g. `>`, `>>`, `2>&1`, `<<EOF`. A target written with no
    /// intervening whitespace is part of the same token; use
    /// [`Token::redirect_op_len`] or [`Token::redirect_target`] to split them.
    Redirect,
    /// A whole arithmetic command, `(( … ))`, including both closing
    /// parentheses. `((` always starts arithmetic (`DESIGN.md` §3.3).
    ArithCommand,
    /// A here-document body. Never shell code.
    HereDocBody,
    /// The line that terminates a here-document, without its newline.
    HereDocDelimiter,
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

/// Reserved words the lexer classifies when they sit in command position.
///
/// Over-classifying only changes colour; the parser decides which words
/// actually apply by looking at text and position.
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
    b"[[",
    b"]]",
];

/// Reserved words after which a command is expected again.
fn reserved_opens_command(text: &[u8]) -> bool {
    matches!(
        text,
        b"if"
            | b"then"
            | b"else"
            | b"elif"
            | b"do"
            | b"while"
            | b"until"
            | b"!"
            | b"time"
            | b"for"
            | b"case"
            | b"in"
            | b"function"
            | b"[["
    )
}

/// Split `src` into tokens, discarding the here-document side table.
///
/// This is the convenient entry point for highlighting and for tooling that
/// only needs token classes. Use [`lex_full`] when the bodies are needed.
pub fn lex(src: &[u8]) -> Vec<Token> {
    lex_full(src).tokens
}

/// Split `src` into tokens and report here-documents and unterminated
/// constructs.
///
/// The default profile: `extglob` off at parse time, which is what a shell
/// with `shopt -u extglob` sees. Use [`lex_full_with`] to say otherwise.
pub fn lex_full(src: &[u8]) -> Lexed {
    lex_full_with(src, false)
}

/// [`lex_full`] with the parse-affecting options the token stream depends on.
///
/// Only `extglob` changes the token stream: with it on, `!(` is always a
/// group, so the parser (and everything downstream) sees the word bash would
/// see. With it off, `!(` at the start of a statement stays pipeline negation.
pub fn lex_full_with(src: &[u8], extglob: bool) -> Lexed {
    let mut lx = Lexer::new(src, 0, 0, None, extglob);
    lx.run();
    let mut open = lx.nest;
    open.reverse();
    Lexed {
        tokens: lx.tokens,
        heredocs: lx.heredocs,
        unterminated: lx.unterminated,
        open,
    }
}

/// The chain of constructs still open at `offset`, **innermost first**.
///
/// This is [`lex_full`] run on `src[..offset]`, reporting the lexer's nesting
/// stack: for `echo "$(ls ` at the end it answers
/// `[CmdSub, DoubleQuote]`, so a consumer knows the cursor is inside a command
/// substitution that is itself inside a quoted word. Each entry carries the
/// opener's span and the offset where its content begins.
///
/// `offset` is clamped to the input. The stack is maintained during the single
/// lexing pass; this function does not re-scan regions with a second scanner.
pub fn open_constructs_at(src: &[u8], offset: usize) -> Vec<OpenConstruct> {
    let offset = offset.min(src.len());
    if offset == 0 {
        return Vec::new();
    }
    lex_full(&src[..offset]).open
}

/// The result of scanning one word.
pub(crate) struct WordScan {
    /// Where the word ends, or `src.len()` when it is unterminated.
    pub end: usize,
    /// The innermost construct left open, when the word is unterminated.
    pub unterminated: Option<OpenConstruct>,
}

fn is_meta(b: u8) -> bool {
    matches!(
        b,
        b' ' | b'\t' | b'\n' | b'<' | b'>' | b'|' | b'&' | b';' | b'(' | b')'
    )
}

/// Whether `b` can start a name (`[A-Za-z_]`).
pub(crate) fn is_name_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

/// Whether `b` can continue a name (`[A-Za-z0-9_]`).
pub(crate) fn is_name(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Whether `bytes` is a valid shell name.
pub(crate) fn is_valid_name(bytes: &[u8]) -> bool {
    let Some((&first, rest)) = bytes.split_first() else {
        return false;
    };
    if !is_name_start(first) {
        return false;
    }
    rest.iter().all(|&b| is_name(b))
}

/// Redirection operators, longest first, with the descriptor prefix handled
/// separately.
///
/// Order matters: `<<<` must be tried before `<<`, `&>>` before `&>`.
pub(crate) const REDIRECT_OPS: &[(&[u8], RedirectOp)] = &[
    (b"<<<", RedirectOp::HereString),
    (b"<<-", RedirectOp::HereDocStrip),
    (b"&>>", RedirectOp::BothAppend),
    (b"<<", RedirectOp::HereDoc),
    (b">>", RedirectOp::Append),
    (b">&", RedirectOp::DupWrite),
    (b"<&", RedirectOp::DupRead),
    (b"<>", RedirectOp::ReadWrite),
    (b">|", RedirectOp::Clobber),
    (b"&>", RedirectOp::Both),
    (b"<", RedirectOp::Read),
    (b">", RedirectOp::Write),
];

/// Match a redirection operator at the start of `text`, returning it and its
/// length.
pub(crate) fn match_redirect_op(text: &[u8]) -> Option<(RedirectOp, usize)> {
    REDIRECT_OPS
        .iter()
        .find(|(bytes, _)| text.starts_with(bytes))
        .map(|(bytes, op)| (*op, bytes.len()))
}

/// Find the first unquoted, unsubstituted `=` in a word.
pub(crate) fn find_assignment_eq(src: &[u8], span: &Span) -> Option<usize> {
    let mut i = span.start;
    while i < span.end {
        match src[i] {
            b'\\' => i += 2,
            b'\'' => i = skip_squote(src, i + 1)?,
            b'"' => i = skip_dquote(src, i + 1, 0, &mut Vec::new())?,
            b'`' => i = skip_backtick(src, i + 1)?,
            b'$' => i = scan_dollar(src, i, 0, false, &mut Vec::new())?,
            // A subscript may contain `=` (`a[x=1]=v`); the assignment's own
            // `=` is the one after the closing bracket.
            b'[' => match skip_subscript(src, i, span.end) {
                Some(after) => i = after,
                None => i += 1,
            },
            b'=' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// Skip `[ … ]` quote-aware, returning the offset just past the matching `]`.
fn skip_subscript(src: &[u8], open: usize, end: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < end {
        match src[i] {
            b'\\' => i += 2,
            b'\'' => i = skip_squote(src, i + 1)?,
            b'"' => i = skip_dquote(src, i + 1, 0, &mut Vec::new())?,
            b'[' => {
                depth += 1;
                i += 1;
            }
            b']' => {
                depth = depth.checked_sub(1)?;
                i += 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// The offset just past the `=` of a valid `NAME=value` assignment word,
/// `NAME+=value` included. `NAME[sub]=value` also qualifies.
pub fn assignment_value_start(src: &[u8], span: &Span) -> Option<usize> {
    let eq = find_assignment_eq(src, span)?;
    let mut name_end = eq;
    if src.get(eq.wrapping_sub(1)) == Some(&b'+') {
        name_end -= 1;
    }
    if let Some(open) = src[span.start..name_end].iter().position(|&b| b == b'[') {
        let open = span.start + open;
        if !src[open + 1..name_end].contains(&b']') {
            return None;
        }
        name_end = open;
    }
    if name_end <= span.start || !is_valid_name(&src[span.start..name_end]) {
        return None;
    }
    Some(eq + 1)
}

/// Skip a single-quoted string; `from` is just past the opening quote.
pub(crate) fn skip_squote(src: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i < src.len() {
        if src[i] == b'\'' {
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

/// Skip an ANSI-C quoted string; `from` is just past the opening quote.
pub(crate) fn skip_ansic(src: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i < src.len() {
        match src[i] {
            b'\\' => i += 2,
            b'\'' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Skip a backtick command substitution; `from` is just past the backtick.
///
/// A backslash always protects the following byte while scanning for the
/// closing backtick, which is what the legacy quoting rule reduces to for the
/// purpose of finding the end.
pub(crate) fn skip_backtick(src: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i < src.len() {
        match src[i] {
            b'\\' => i += 2,
            b'`' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Skip a double-quoted string; `from` is just past the opening quote.
///
/// On success every construct opened inside the string has been closed and
/// popped from `nest`; on failure the string and whatever is open inside it
/// remain on the stack, innermost last.
pub(crate) fn skip_dquote(
    src: &[u8],
    from: usize,
    depth: usize,
    nest: &mut Vec<OpenConstruct>,
) -> Option<usize> {
    let mut i = from;
    while i < src.len() {
        match src[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            b'`' => {
                push_backtick(nest, i);
                match skip_backtick(src, i + 1) {
                    Some(end) => {
                        nest.pop();
                        i = end;
                    }
                    None => {
                        scan_backtick_body(src, i + 1, depth, nest, false);
                        return None;
                    }
                }
            }
            b'$' => i = scan_dollar(src, i, depth, true, nest)?,
            _ => i += 1,
        }
    }
    None
}

/// Skip `${ … }`; `from` is just past the opening brace.
pub(crate) fn skip_param(
    src: &[u8],
    from: usize,
    depth: usize,
    nest: &mut Vec<OpenConstruct>,
) -> Option<usize> {
    if depth >= MAX_NEST {
        return None;
    }
    let mut i = from;
    while i < src.len() {
        match src[i] {
            b'\\' => i += 2,
            b'\'' => i = skip_squote(src, i + 1)?,
            b'"' => i = skip_dquote(src, i + 1, depth, nest)?,
            b'`' => {
                push_backtick(nest, i);
                match skip_backtick(src, i + 1) {
                    Some(end) => {
                        nest.pop();
                        i = end;
                    }
                    None => {
                        scan_backtick_body(src, i + 1, depth, nest, false);
                        return None;
                    }
                }
            }
            b'$' => {
                if src.get(i + 1) == Some(&b'{') {
                    nest.push(OpenConstruct {
                        kind: UnterminatedKind::ParamExp,
                        opener: i..i + 2,
                        content_start: i + 2,
                    });
                    let end = skip_param(src, i + 2, depth + 1, nest)?;
                    nest.pop();
                    i = end;
                } else {
                    i = scan_dollar(src, i, depth, false, nest)?;
                }
            }
            b'}' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Skip `$(( … ))`; `from` is just past the two opening parentheses.
pub(crate) fn scan_arith(
    src: &[u8],
    from: usize,
    depth: usize,
    nest: &mut Vec<OpenConstruct>,
) -> Option<usize> {
    if depth >= MAX_NEST {
        return None;
    }
    let mut i = from;
    let mut parens = 0usize;
    while i < src.len() {
        match src[i] {
            b'\\' => i += 2,
            b'\'' => i = skip_squote(src, i + 1)?,
            b'"' => i = skip_dquote(src, i + 1, depth, nest)?,
            b'`' => {
                push_backtick(nest, i);
                match skip_backtick(src, i + 1) {
                    Some(end) => {
                        nest.pop();
                        i = end;
                    }
                    None => {
                        scan_backtick_body(src, i + 1, depth, nest, false);
                        return None;
                    }
                }
            }
            b'$' => {
                if src.get(i + 1) == Some(&b'(') && src.get(i + 2) == Some(&b'(') {
                    nest.push(OpenConstruct {
                        kind: UnterminatedKind::Arith,
                        opener: i..i + 3,
                        content_start: i + 3,
                    });
                    let end = scan_arith(src, i + 3, depth + 1, nest)?;
                    nest.pop();
                    i = end;
                } else {
                    i = scan_dollar(src, i, depth, false, nest)?;
                }
            }
            b'(' => {
                parens += 1;
                i += 1;
            }
            b')' => {
                if parens > 0 {
                    parens -= 1;
                    i += 1;
                } else if src.get(i + 1) == Some(&b')') {
                    return Some(i + 2);
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// Scan one `$`-introduced construct. `from` points at the `$`.
///
/// `Some(end)` is just past the construct (or just past the `$` when it
/// introduces nothing). `None` means the construct runs to end of input and
/// has been left open on `nest`.
pub(crate) fn scan_dollar(
    src: &[u8],
    from: usize,
    depth: usize,
    in_dquote: bool,
    nest: &mut Vec<OpenConstruct>,
) -> Option<usize> {
    let next = src.get(from + 1).copied();
    if in_dquote && matches!(next, Some(b'\'') | Some(b'"')) {
        return Some(from + 1);
    }
    match next {
        Some(b'\'') => {
            nest.push(OpenConstruct {
                kind: UnterminatedKind::AnsiCQuote,
                opener: from..from + 2,
                content_start: from + 2,
            });
            match skip_ansic(src, from + 2) {
                Some(end) => {
                    nest.pop();
                    Some(end)
                }
                None => None,
            }
        }
        Some(b'"') => {
            nest.push(OpenConstruct {
                kind: UnterminatedKind::DoubleQuote,
                opener: from..from + 2,
                content_start: from + 2,
            });
            match skip_dquote(src, from + 2, depth, nest) {
                Some(end) => {
                    nest.pop();
                    Some(end)
                }
                None => None,
            }
        }
        Some(b'(') if src.get(from + 2) == Some(&b'(') => {
            nest.push(OpenConstruct {
                kind: UnterminatedKind::Arith,
                opener: from..from + 3,
                content_start: from + 3,
            });
            match scan_arith(src, from + 3, depth, nest) {
                Some(end) => {
                    nest.pop();
                    Some(end)
                }
                None => None,
            }
        }
        Some(b'(') => {
            nest.push(OpenConstruct {
                kind: UnterminatedKind::CmdSub,
                opener: from..from + 2,
                content_start: from + 2,
            });
            match scan_region(src, from + 2, depth, false) {
                Region {
                    end: Some(rparen), ..
                } => {
                    nest.pop();
                    Some(rparen + 1)
                }
                Region { end: None, open } => {
                    nest.extend(open);
                    None
                }
            }
        }
        Some(b'{') => {
            nest.push(OpenConstruct {
                kind: UnterminatedKind::ParamExp,
                opener: from..from + 2,
                content_start: from + 2,
            });
            match skip_param(src, from + 2, depth, nest) {
                Some(end) => {
                    nest.pop();
                    Some(end)
                }
                None => None,
            }
        }
        _ => Some(from + 1),
    }
}

/// Push a backtick construct. The body is not lexed while the closing
/// backtick is still ahead: [`skip_backtick`] only has to find it.
fn push_backtick(nest: &mut Vec<OpenConstruct>, at: usize) {
    nest.push(OpenConstruct {
        kind: UnterminatedKind::Backtick,
        opener: at..at + 1,
        content_start: at + 1,
    });
}

/// Lex the body of a backtick substitution that never closed, so constructs
/// opened inside it join the nesting chain.
fn scan_backtick_body(
    src: &[u8],
    from: usize,
    depth: usize,
    nest: &mut Vec<OpenConstruct>,
    extglob: bool,
) {
    if depth >= MAX_NEST {
        return;
    }
    let mut inner = Lexer::new(src, from, depth + 1, None, extglob);
    inner.run();
    nest.extend(inner.nest);
}

/// Whether `b` is one of the five extglob operators (`?(`, `*(`, `+(`, `@(`,
/// `!(`).
fn is_extglob_op(b: u8) -> bool {
    matches!(b, b'?' | b'*' | b'+' | b'@' | b'!')
}

/// Scan the extglob group whose opening `(` is at `open`, returning the offset
/// just past the matching `)`.
///
/// The scan is structural only: `|` alternatives and the operator bytes are
/// ordinary bytes here, because the matcher parses them later. What it does
/// track is everything that can hide a `)`: quotes, backslash escapes,
/// `$(…)`/`$((…))`/`${…}`/backticks, and nested `(` … `)` groups. A `)`
/// inside any of those does not close the group, and a construct that runs to
/// end of input leaves the whole group unclosed.
///
/// `None` means no balanced group was found; the caller then treats the `(`
/// as an ordinary word byte. That is deliberately not an error: a malformed
/// group is literal pattern text (`bsh-pattern` degrades it the same way), and
/// the lexer never invents a diagnostic of its own.
fn scan_extglob(src: &[u8], open: usize, depth: usize) -> Option<usize> {
    let mut i = open + 1;
    let mut parens = 0usize;
    while i < src.len() {
        match src[i] {
            b'\\' => i += 2,
            b'\'' => i = skip_squote(src, i + 1)?,
            b'"' => {
                let mut inner = Vec::new();
                i = skip_dquote(src, i + 1, depth, &mut inner)?;
            }
            b'`' => i = skip_backtick(src, i + 1)?,
            // A `$` construct may contain its own parentheses and quotes; skip
            // the whole thing so its `)` cannot be mistaken for the group's.
            b'$' => {
                let mut inner = Vec::new();
                i = scan_dollar(src, i, depth, false, &mut inner)?;
            }
            b'(' => {
                parens += 1;
                i += 1;
            }
            b')' if parens == 0 => return Some(i + 1),
            b')' => {
                parens -= 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// Scan a word, returning where it ends and whether something is left open.
///
/// The word stops at an unquoted metacharacter. `<(` and `>(` are process
/// substitution only at the very start of the word, which is where a shell
/// would recognise them. An extglob operator immediately followed by `(`
/// consumes the balanced group as part of the word (see the module docs); if
/// the group does not close, the `(` is just a word byte and nothing special
/// is reported.
pub(crate) fn scan_word(
    src: &[u8],
    start: usize,
    depth: usize,
    nest: &mut Vec<OpenConstruct>,
    statement_start: bool,
    extglob: bool,
) -> WordScan {
    let mut i = start;
    // Once an extglob group runs to end of input, no later group in this word
    // can close either: the rest is inside the unclosed one. Stop rescanning
    // the suffix for each further operator (`@(@(@(…` would otherwise be
    // quadratic), the same shortcut `bsh-pattern`'s parser takes.
    let mut extglob_broken = false;
    while i < src.len() {
        let b = src[i];
        if is_meta(b) {
            let proc_sub = (b == b'<' || b == b'>') && i == start && src.get(i + 1) == Some(&b'(');
            if proc_sub {
                nest.push(OpenConstruct {
                    kind: UnterminatedKind::ProcSub,
                    opener: i..i + 2,
                    content_start: i + 2,
                });
                match scan_region(src, i + 2, depth, extglob) {
                    Region {
                        end: Some(rparen), ..
                    } => {
                        nest.pop();
                        i = rparen + 1;
                        continue;
                    }
                    Region { end: None, open } => {
                        nest.extend(open);
                        return WordScan {
                            end: src.len(),
                            unterminated: nest.last().cloned(),
                        };
                    }
                }
            }
            break;
        }
        match b {
            b'\'' => match skip_squote(src, i + 1) {
                Some(end) => i = end,
                None => {
                    nest.push(OpenConstruct {
                        kind: UnterminatedKind::SingleQuote,
                        opener: i..i + 1,
                        content_start: i + 1,
                    });
                    return WordScan {
                        end: src.len(),
                        unterminated: nest.last().cloned(),
                    };
                }
            },
            b'"' => {
                nest.push(OpenConstruct {
                    kind: UnterminatedKind::DoubleQuote,
                    opener: i..i + 1,
                    content_start: i + 1,
                });
                match skip_dquote(src, i + 1, depth, nest) {
                    Some(end) => {
                        nest.pop();
                        i = end;
                    }
                    None => {
                        return WordScan {
                            end: src.len(),
                            unterminated: nest.last().cloned(),
                        };
                    }
                }
            }
            b'\\' => i = (i + 2).min(src.len()),
            b'`' => {
                push_backtick(nest, i);
                match skip_backtick(src, i + 1) {
                    Some(end) => {
                        nest.pop();
                        i = end;
                    }
                    None => {
                        scan_backtick_body(src, i + 1, depth, nest, extglob);
                        return WordScan {
                            end: src.len(),
                            unterminated: nest.last().cloned(),
                        };
                    }
                }
            }
            b'$' => match scan_dollar(src, i, depth, false, nest) {
                Some(end) => i = end,
                None => {
                    return WordScan {
                        end: src.len(),
                        unterminated: nest.last().cloned(),
                    };
                }
            },
            // `?(` `*(` `+(` `@(` `!(` open an extglob group. It is part of
            // this word through its matching `)`; a group that never closes is
            // not consumed, and the `(` is left as an ordinary word byte.
            //
            // `!(` at the *start of a statement* is the exception: there it is
            // pipeline negation of a subshell (`if !(a && b)`), which is how
            // bash reads it when extglob is off — and extglob is off by
            // default. Only `!` is affected: `@(`, `?(`, `*(` and `+(` cannot
            // begin a command, so they are always groups.
            b if is_extglob_op(b)
                && src.get(i + 1) == Some(&b'(')
                && !(b == b'!' && statement_start && i == start) =>
            {
                i = if extglob_broken {
                    i + 2
                } else {
                    match scan_extglob(src, i + 1, depth) {
                        Some(end) => end,
                        None => {
                            extglob_broken = true;
                            i + 2
                        }
                    }
                };
            }
            _ => i += 1,
        }
    }
    WordScan {
        end: i,
        unterminated: None,
    }
}

/// The result of scanning a `$( … )` or `<( … )` body.
pub(crate) struct Region {
    /// Just past the closing `)` when the region closed.
    pub end: Option<usize>,
    /// Constructs still open inside the region, innermost last, when it did
    /// not close.
    pub open: Vec<OpenConstruct>,
}

/// Find the `)` that closes a `$( … )` or `<( … )` whose body starts at
/// `start`.
///
/// This runs a full lexer pass over the body with a stop mode, so quotes,
/// nested substitutions and here-document bodies cannot end the substitution
/// early. When the region never closes, its nesting stack is returned so the
/// caller can extend its own.
pub(crate) fn scan_region(src: &[u8], start: usize, depth: usize, extglob: bool) -> Region {
    if depth >= MAX_NEST {
        return Region {
            end: None,
            open: Vec::new(),
        };
    }
    let mut lx = Lexer::new(src, start, depth + 1, Some(StopMode { depth: 0 }), extglob);
    lx.run();
    if lx.stopped {
        Region {
            end: Some(lx.i),
            open: Vec::new(),
        }
    } else {
        Region {
            end: None,
            open: lx.nest,
        }
    }
}

/// [`scan_region`] when only the close position is needed.
pub(crate) fn region_end(src: &[u8], start: usize, depth: usize) -> Option<usize> {
    scan_region(src, start, depth, false).end
}

/// Quote-remove a here-document delimiter word.
///
/// Returns the bytes a body line must equal and whether any quoting was
/// present (which suppresses expansion in the body).
pub(crate) fn dequote_delimiter(raw: &[u8]) -> (Vec<u8>, bool) {
    let mut out = Vec::with_capacity(raw.len());
    let mut quoted = false;
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            b'\'' => {
                quoted = true;
                i += 1;
                while i < raw.len() && raw[i] != b'\'' {
                    out.push(raw[i]);
                    i += 1;
                }
                i += 1;
            }
            b'"' => {
                quoted = true;
                i += 1;
                while i < raw.len() && raw[i] != b'"' {
                    if raw[i] == b'\\'
                        && i + 1 < raw.len()
                        && matches!(raw[i + 1], b'$' | b'`' | b'"' | b'\\' | b'\n')
                    {
                        if raw[i + 1] != b'\n' {
                            out.push(raw[i + 1]);
                        }
                        i += 2;
                    } else {
                        out.push(raw[i]);
                        i += 1;
                    }
                }
                i += 1;
            }
            b'\\' => {
                quoted = true;
                if i + 1 < raw.len() {
                    out.push(raw[i + 1]);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    (out, quoted)
}

/// A case-statement state used by the `$()` body scanner.
///
/// The scanner does not parse; it only needs to know that the `)` ending a
/// `case` pattern is not the `)` ending the substitution. That is the one
/// piece of syntax a pure token scan cannot recover without unbounded
/// lookahead (`DESIGN.md` §3.3, Tier B).
#[derive(Clone, Copy)]
struct CaseState {
    arms: bool,
    pattern: bool,
}

/// Stop at an unmatched `)` at this nesting depth.
struct StopMode {
    depth: usize,
}

struct Lexer<'a> {
    src: &'a [u8],
    i: usize,
    tokens: Vec<Token>,
    heredocs: Vec<HereDoc>,
    unterminated: Vec<Unterminated>,
    /// Here-document indices whose bodies start after the next newline.
    pending: Vec<usize>,
    /// Here-document waiting for its delimiter word.
    pending_delim: Option<usize>,
    /// Whether `extglob` was on when this chunk started being parsed.
    ///
    /// It decides one thing: whether a word-initial `!(` is a group or
    /// pipeline negation. bash settles that at parse time, so a chunk parsed
    /// with the option on must not invent the negation reading.
    extglob: bool,
    /// A redirection was lexed with no adjacent target; the next word is it.
    redirect_target: bool,
    cmd_pos: bool,
    depth: usize,
    stop: Option<StopMode>,
    cases: Vec<CaseState>,
    stopped: bool,
    /// Constructs open while scanning, outermost first. Reversed into
    /// [`Lexed::open`] when the pass ends.
    nest: Vec<OpenConstruct>,
}

impl<'a> Lexer<'a> {
    /// Whether the next word starts a statement, so a leading `!(` is pipeline
    /// negation rather than an extglob group.
    ///
    /// `cmd_pos` cannot answer this: `in` also opens a *word* list
    /// (`for x in !(a)`), while `if`/`while`/`;`/`&&` open a command. The last
    /// token therefore decides, and the list is deliberately explicit rather
    /// than "any keyword".
    fn at_statement_start(&self) -> bool {
        // Whitespace is a token here, so look back past it: the decision is
        // about the last *significant* token.
        let Some(last) = self
            .tokens
            .iter()
            .rev()
            .find(|token| token.kind != TokenKind::Whitespace)
        else {
            return true;
        };
        let text = last.text(self.src);
        match last.kind {
            TokenKind::Newline => true,
            TokenKind::Operator => matches!(
                text,
                b";" | b"&" | b"&&" | b"||" | b"|" | b"(" | b"{" | b";;" | b";&" | b";;&" | b"|&"
            ),
            TokenKind::Keyword => matches!(
                text,
                b"if" | b"then" | b"elif" | b"else" | b"do" | b"while" | b"until" | b"time" | b"!"
            ),
            _ => false,
        }
    }

    fn new(src: &'a [u8], i: usize, depth: usize, stop: Option<StopMode>, extglob: bool) -> Self {
        Self {
            src,
            i,
            extglob,
            tokens: Vec::new(),
            heredocs: Vec::new(),
            unterminated: Vec::new(),
            pending: Vec::new(),
            pending_delim: None,
            redirect_target: false,
            cmd_pos: true,
            depth,
            stop,
            cases: Vec::new(),
            stopped: false,
            nest: Vec::new(),
        }
    }

    fn run(&mut self) {
        while self.i < self.src.len() && !self.stopped {
            self.step();
        }
        // Whatever was still queued never got a newline to start its body.
        for idx in std::mem::take(&mut self.pending) {
            self.heredocs[idx].terminated = false;
            let op = self.heredocs[idx].op.clone();
            self.unterminated.push(Unterminated {
                span: op.clone(),
                opener: op.clone(),
                content_start: self.src.len(),
                kind: UnterminatedKind::HereDoc,
            });
            self.nest.push(OpenConstruct {
                kind: UnterminatedKind::HereDoc,
                opener: op,
                content_start: self.src.len(),
            });
        }
    }

    fn step(&mut self) {
        let b = self.src[self.i];
        match b {
            b' ' | b'\t' => {
                let start = self.i;
                while self.i < self.src.len() && matches!(self.src[self.i], b' ' | b'\t') {
                    self.i += 1;
                }
                self.push(TokenKind::Whitespace, start, self.i);
            }
            b'\n' => self.newline(),
            b'#' => {
                let start = self.i;
                match memchr(b'\n', &self.src[self.i..]) {
                    Some(off) => self.i += off,
                    None => self.i = self.src.len(),
                }
                self.push(TokenKind::Comment, start, self.i);
            }
            b'(' => self.lparen(),
            b')' => self.rparen(),
            b'&' => {
                if self.src.get(self.i + 1) == Some(&b'>') {
                    self.redirect();
                } else {
                    self.operator();
                }
            }
            b'|' | b';' => self.operator(),
            b'<' | b'>' => {
                if self.src.get(self.i + 1) == Some(&b'(') {
                    self.word();
                } else {
                    self.redirect();
                }
            }
            b'0'..=b'9' => {
                let mut j = self.i;
                while j < self.src.len() && self.src[j].is_ascii_digit() {
                    j += 1;
                }
                if j < self.src.len() && matches!(self.src[j], b'<' | b'>') {
                    self.redirect();
                } else {
                    self.word();
                }
            }
            _ => self.word(),
        }
    }

    fn push(&mut self, kind: TokenKind, start: usize, end: usize) {
        self.tokens.push(Token {
            kind,
            span: start..end,
        });
    }

    fn text(&self, span: &Span) -> &'a [u8] {
        &self.src[span.start..span.end]
    }

    fn newline(&mut self) {
        let start = self.i;
        self.i += 1;
        self.push(TokenKind::Newline, start, self.i);
        self.cmd_pos = true;
        self.redirect_target = false;
        self.pending_delim = None;
        self.read_heredocs();
    }

    /// Read the bodies of every here-document queued on the line just ended.
    fn read_heredocs(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        for idx in pending {
            let missing = self.heredocs[idx].delimiter.start == self.heredocs[idx].delimiter.end;
            if missing {
                self.heredocs[idx].terminated = false;
                continue;
            }
            let delim = self.heredocs[idx].delimiter_text.clone();
            let strip_tabs = self.heredocs[idx].strip_tabs;
            let body_start = self.i;
            let mut line_start = body_start;
            let mut found: Option<usize> = None;
            loop {
                let line_end = match memchr(b'\n', &self.src[line_start..]) {
                    Some(off) => line_start + off,
                    None => self.src.len(),
                };
                let line = &self.src[line_start..line_end];
                let cmp = if strip_tabs {
                    strip_leading_tabs(line)
                } else {
                    line
                };
                if cmp == delim.as_slice() {
                    found = Some(line_start);
                    break;
                }
                if line_end >= self.src.len() {
                    break;
                }
                line_start = line_end + 1;
                if line_start >= self.src.len() {
                    break;
                }
            }
            let terminated = found.is_some();
            let body_end = found.unwrap_or(self.src.len());
            if body_end > body_start {
                self.tokens.push(Token {
                    kind: TokenKind::HereDocBody,
                    span: body_start..body_end,
                });
            }
            if let Some(delim_start) = found {
                let line_end = match memchr(b'\n', &self.src[delim_start..]) {
                    Some(off) => delim_start + off,
                    None => self.src.len(),
                };
                self.tokens.push(Token {
                    kind: TokenKind::HereDocDelimiter,
                    span: delim_start..line_end,
                });
                self.i = line_end;
                if line_end < self.src.len() {
                    self.i = line_end + 1;
                    self.tokens.push(Token {
                        kind: TokenKind::Newline,
                        span: line_end..line_end + 1,
                    });
                    self.cmd_pos = true;
                }
            } else {
                self.i = self.src.len();
            }
            let op = self.heredocs[idx].op.clone();
            self.heredocs[idx].body = body_start..body_end;
            self.heredocs[idx].terminated = terminated;
            if !terminated {
                self.unterminated.push(Unterminated {
                    span: op.clone(),
                    opener: op.clone(),
                    content_start: body_start,
                    kind: UnterminatedKind::HereDoc,
                });
                self.nest.push(OpenConstruct {
                    kind: UnterminatedKind::HereDoc,
                    opener: op,
                    content_start: body_start,
                });
            }
            // The delimiter line's newline is a real separator; any further
            // pending here-documents were queued on the same line and come
            // next, in order.
        }
    }

    fn lparen(&mut self) {
        let start = self.i;
        if self.src.get(self.i + 1) == Some(&b'(') {
            self.nest.push(OpenConstruct {
                kind: UnterminatedKind::ArithCommand,
                opener: start..start + 2,
                content_start: start + 2,
            });
            match scan_arith(self.src, self.i + 2, self.depth, &mut self.nest) {
                Some(end) => {
                    self.nest.pop();
                    self.i = end;
                    self.push(TokenKind::ArithCommand, start, end);
                    self.observe_case(TokenKind::ArithCommand, b"((..))");
                    self.cmd_pos = false;
                    self.redirect_target = false;
                }
                None => {
                    self.i = self.src.len();
                    self.push(TokenKind::ArithCommand, start, self.i);
                    self.unterminated.push(Unterminated {
                        span: start..self.i,
                        opener: start..start + 2,
                        content_start: start + 2,
                        kind: UnterminatedKind::ArithCommand,
                    });
                    self.cmd_pos = false;
                }
            }
            return;
        }
        let in_pattern = self.cases.last().is_some_and(|c| c.arms && c.pattern);
        if self.stop.is_some() && !in_pattern {
            if let Some(stop) = self.stop.as_mut() {
                stop.depth += 1;
            }
        }
        self.i += 1;
        self.push(TokenKind::Operator, start, self.i);
        self.observe_case(TokenKind::Operator, b"(");
        self.cmd_pos = true;
        self.redirect_target = false;
    }

    fn rparen(&mut self) {
        let start = self.i;
        if let Some(top) = self.cases.last_mut() {
            if top.arms && top.pattern {
                top.pattern = false;
                self.i += 1;
                self.push(TokenKind::Operator, start, self.i);
                self.observe_case(TokenKind::Operator, b")");
                self.cmd_pos = true;
                self.redirect_target = false;
                return;
            }
        }
        if self.stop.is_some() {
            let depth = self.stop.as_ref().map_or(0, |s| s.depth);
            if depth == 0 {
                self.stopped = true;
                return;
            }
            if let Some(stop) = self.stop.as_mut() {
                stop.depth -= 1;
            }
        }
        self.i += 1;
        self.push(TokenKind::Operator, start, self.i);
        self.observe_case(TokenKind::Operator, b")");
        self.cmd_pos = true;
        self.redirect_target = false;
    }

    fn operator(&mut self) {
        let start = self.i;
        let rest = &self.src[self.i..];
        let len = if rest.starts_with(b";;&") {
            3
        } else if rest.starts_with(b";;")
            || rest.starts_with(b";&")
            || rest.starts_with(b"&&")
            || rest.starts_with(b"||")
            || rest.starts_with(b"|&")
        {
            2
        } else {
            1
        };
        self.i += len;
        let span = start..self.i;
        self.push(TokenKind::Operator, start, self.i);
        let text = self.text(&span);
        let owned: Vec<u8> = text.to_vec();
        self.observe_case(TokenKind::Operator, &owned);
        self.cmd_pos = true;
        self.redirect_target = false;
    }

    fn redirect(&mut self) {
        let start = self.i;
        let Some(op_len) = redirect_op_len(&self.src[start..]) else {
            // Should not happen: callers only route real redirects here.
            self.word();
            return;
        };
        let op_end = start + op_len;
        let mut digits = 0;
        while digits < op_len && self.src[start + digits].is_ascii_digit() {
            digits += 1;
        }
        let op_text = &self.src[start + digits..op_end];
        let heredoc = op_text == b"<<" || op_text == b"<<-";
        let strip_tabs = op_text == b"<<-";

        // A target written hard against the operator is part of this token.
        // The scan is speculative: a target that is itself unterminated is
        // left for `word` to scan (and report) in its own right.
        let mut end = op_end;
        let mut target: Option<Span> = None;
        if let Some(&b) = self.src.get(op_end) {
            if !is_meta(b) {
                let scan = scan_word(
                    self.src,
                    op_end,
                    self.depth,
                    &mut Vec::new(),
                    false,
                    self.extglob,
                );
                if scan.unterminated.is_none() {
                    end = scan.end;
                    target = Some(op_end..end);
                }
            }
        }

        let idx = if heredoc {
            let record = HereDoc {
                op: start..op_end,
                delimiter: 0..0,
                delimiter_text: Vec::new(),
                quoted: false,
                strip_tabs,
                body: 0..0,
                terminated: false,
            };
            self.heredocs.push(record);
            let idx = self.heredocs.len() - 1;
            self.pending.push(idx);
            Some(idx)
        } else {
            None
        };

        if heredoc {
            if let Some(target_span) = target.clone() {
                let (text, quoted) = dequote_delimiter(self.text(&target_span));
                if let Some(idx) = idx {
                    self.heredocs[idx].delimiter = target_span;
                    self.heredocs[idx].delimiter_text = text;
                    self.heredocs[idx].quoted = quoted;
                }
            } else {
                self.pending_delim = idx;
            }
        }

        self.i = end;
        self.push(TokenKind::Redirect, start, end);
        self.observe_case(TokenKind::Redirect, b">");
        if target.is_none() && !is_redirect_dup_only(op_text) {
            self.redirect_target = true;
        }
        // A redirection does not change command position: `>f cmd` still has
        // `cmd` in command position.
    }

    fn word(&mut self) {
        let start = self.i;
        let statement_start = self.at_statement_start();
        let scan = scan_word(
            self.src,
            start,
            self.depth,
            &mut self.nest,
            statement_start,
            self.extglob,
        );
        self.i = scan.end;
        let text = &self.src[start..scan.end];
        if scan.unterminated.is_none() && text == b"\\\n" {
            // A bare backslash-newline between tokens. POSIX deletes it before
            // tokenising, so it is layout, not a word; the bytes stay in the
            // token stream, which keeps the lossless invariant.
            self.push(TokenKind::Whitespace, start, scan.end);
            return;
        }
        if let Some(open) = scan.unterminated {
            self.push(TokenKind::WordUnterminated, start, scan.end);
            self.unterminated.push(Unterminated {
                span: start..scan.end,
                opener: open.opener,
                content_start: open.content_start,
                kind: open.kind,
            });
            self.finish_delimiter(start, scan.end);
            self.cmd_pos = false;
            self.redirect_target = false;
            return;
        }

        let kind = if self.redirect_target {
            self.redirect_target = false;
            self.finish_delimiter(start, scan.end);
            TokenKind::Word
        } else if text == b"{" || text == b"}" {
            self.observe_case(TokenKind::Operator, text);
            self.cmd_pos = true;
            TokenKind::Operator
        } else if self.cmd_pos && RESERVED.contains(&text) {
            TokenKind::Keyword
        } else {
            TokenKind::Word
        };

        self.push(kind, start, scan.end);
        if kind == TokenKind::Keyword {
            self.observe_case(kind, text);
            self.cmd_pos = reserved_opens_command(text);
        } else if kind == TokenKind::Word {
            self.observe_case(kind, text);
            self.cmd_pos = false;
        }
        if self.pending_delim.is_some() && kind != TokenKind::Operator {
            self.finish_delimiter(start, scan.end);
        }
    }

    fn finish_delimiter(&mut self, start: usize, end: usize) {
        if let Some(idx) = self.pending_delim.take() {
            let span = start..end;
            let (text, quoted) = dequote_delimiter(&self.src[span.clone()]);
            self.heredocs[idx].delimiter = span;
            self.heredocs[idx].delimiter_text = text;
            self.heredocs[idx].quoted = quoted;
        }
    }

    /// Track just enough `case` state that a pattern's `)` is not mistaken for
    /// the end of an enclosing `$()`.
    fn observe_case(&mut self, kind: TokenKind, text: &[u8]) {
        if self.stop.is_none() {
            return;
        }
        if let Some(top) = self.cases.last_mut() {
            if top.arms && top.pattern {
                if text == b")" {
                    top.pattern = false;
                } else if text == b"esac" && matches!(kind, TokenKind::Keyword | TokenKind::Word) {
                    self.cases.pop();
                }
            } else if top.arms {
                match text {
                    b";;" | b";&" | b";;&" => top.pattern = true,
                    b"esac" if matches!(kind, TokenKind::Keyword | TokenKind::Word) => {
                        self.cases.pop();
                    }
                    _ => {}
                }
            } else if text == b"in" && matches!(kind, TokenKind::Keyword | TokenKind::Word) {
                top.arms = true;
                top.pattern = true;
            }
        }
        if self.cmd_pos && text == b"case" && kind == TokenKind::Keyword {
            self.cases.push(CaseState {
                arms: false,
                pattern: false,
            });
        }
    }
}

fn strip_leading_tabs(line: &[u8]) -> &[u8] {
    let mut i = 0;
    while i < line.len() && line[i] == b'\t' {
        i += 1;
    }
    &line[i..]
}

fn is_redirect_dup_only(op: &[u8]) -> bool {
    matches!(op, b">&" | b"<&")
}
