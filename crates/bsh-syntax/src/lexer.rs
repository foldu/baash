//! The lexer: shell bytes to a flat, lossless token stream.
//!
//! The lexer is **tolerant**. It never fails, never reports an error and never
//! gives up on a token: a quote that runs to end of input becomes a
//! [`TokenKind::WordUnterminated`] spanning the rest of the buffer, and the
//! position of the byte that opened it is recorded in [`Lexed::unterminated`].
//! That is what lets the highlighter and the completer run on every keystroke
//! of a half-typed line (`DESIGN.md` §6.10, §6.3).
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

use bsh_diag::Span;
use memchr::memchr;

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
    /// Where the open construct began: the quote byte, the `$` of a
    /// substitution, the `<<` of a here-document.
    pub opener: Span,
    /// What kind of construct is open.
    pub kind: UnterminatedKind,
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
    /// intervening whitespace is part of the same token.
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
pub fn lex_full(src: &[u8]) -> Lexed {
    let mut lx = Lexer::new(src, 0, 0, None);
    lx.run();
    Lexed {
        tokens: lx.tokens,
        heredocs: lx.heredocs,
        unterminated: lx.unterminated,
    }
}

/// The result of scanning one word.
pub(crate) struct WordScan {
    pub end: usize,
    pub unterminated: Option<(usize, UnterminatedKind)>,
}

/// Where a `$` (or a quote) construct ends.
pub(crate) struct Skip {
    pub end: usize,
    pub opener: Option<(usize, UnterminatedKind)>,
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
pub(crate) fn skip_dquote(src: &[u8], from: usize, depth: usize) -> Option<usize> {
    let mut i = from;
    while i < src.len() {
        match src[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            b'`' => i = skip_backtick(src, i + 1)?,
            b'$' => i = skip_dollar_ctx(src, i, depth, true).end,
            _ => i += 1,
        }
    }
    None
}

/// Skip `${ … }`; `from` is just past the opening brace.
pub(crate) fn skip_param(src: &[u8], from: usize, depth: usize) -> Option<usize> {
    if depth >= MAX_NEST {
        return None;
    }
    let mut i = from;
    let mut braces = 1usize;
    while i < src.len() {
        match src[i] {
            b'\\' => i += 2,
            b'\'' => i = skip_squote(src, i + 1)?,
            b'"' => i = skip_dquote(src, i + 1, depth)?,
            b'`' => i = skip_backtick(src, i + 1)?,
            b'$' => {
                if src.get(i + 1) == Some(&b'{') {
                    braces += 1;
                    i += 2;
                } else {
                    i = skip_dollar(src, i, depth).end;
                }
            }
            b'}' => {
                braces -= 1;
                i += 1;
                if braces == 0 {
                    return Some(i);
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// Skip `$(( … ))`; `from` is just past the two opening parentheses.
pub(crate) fn scan_arith(src: &[u8], from: usize, depth: usize) -> Option<usize> {
    if depth >= MAX_NEST {
        return None;
    }
    let mut i = from;
    let mut parens = 0usize;
    while i < src.len() {
        match src[i] {
            b'\\' => i += 2,
            b'\'' => i = skip_squote(src, i + 1)?,
            b'"' => i = skip_dquote(src, i + 1, depth)?,
            b'`' => i = skip_backtick(src, i + 1)?,
            b'$' => {
                if src.get(i + 1) == Some(&b'(') && src.get(i + 2) == Some(&b'(') {
                    i = scan_arith(src, i + 3, depth + 1)?;
                } else {
                    i = skip_dollar(src, i, depth).end;
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

/// Skip one `$`-introduced construct. `from` points at the `$`.
pub(crate) fn skip_dollar(src: &[u8], from: usize, depth: usize) -> Skip {
    skip_dollar_ctx(src, from, depth, false)
}

/// Like [`skip_dollar`], but knows whether it is inside double quotes.
///
/// Inside `"…"`, `$'` and `$"` are *not* quote openers: the `$` is a literal
/// dollar and the quote character is ordinary text (for `"` it closes the
/// enclosing string). Treating them as openers is how a `"${x}$"` word
/// swallows the rest of the line.
fn skip_dollar_ctx(src: &[u8], from: usize, depth: usize, in_dquote: bool) -> Skip {
    let next = src.get(from + 1).copied();
    if in_dquote && matches!(next, Some(b'\'') | Some(b'"')) {
        return Skip {
            end: from + 1,
            opener: None,
        };
    }
    match next {
        Some(b'\'') => match skip_ansic(src, from + 2) {
            Some(end) => Skip { end, opener: None },
            None => Skip {
                end: src.len(),
                opener: Some((from, UnterminatedKind::AnsiCQuote)),
            },
        },
        Some(b'"') => match skip_dquote(src, from + 2, depth) {
            Some(end) => Skip { end, opener: None },
            None => Skip {
                end: src.len(),
                opener: Some((from, UnterminatedKind::DoubleQuote)),
            },
        },
        Some(b'(') => {
            if src.get(from + 2) == Some(&b'(') {
                match scan_arith(src, from + 3, depth) {
                    Some(end) => Skip { end, opener: None },
                    None => Skip {
                        end: src.len(),
                        opener: Some((from, UnterminatedKind::Arith)),
                    },
                }
            } else {
                match region_end(src, from + 2, depth) {
                    Some(rparen) => Skip {
                        end: rparen + 1,
                        opener: None,
                    },
                    None => Skip {
                        end: src.len(),
                        opener: Some((from, UnterminatedKind::CmdSub)),
                    },
                }
            }
        }
        Some(b'{') => match skip_param(src, from + 2, depth) {
            Some(end) => Skip { end, opener: None },
            None => Skip {
                end: src.len(),
                opener: Some((from, UnterminatedKind::ParamExp)),
            },
        },
        _ => Skip {
            end: from + 1,
            opener: None,
        },
    }
}

/// Scan a word, returning where it ends and whether something is left open.
///
/// The word stops at an unquoted metacharacter. `<(` and `>(` are process
/// substitution only at the very start of the word, which is where a shell
/// would recognise them.
pub(crate) fn scan_word(src: &[u8], start: usize, depth: usize) -> WordScan {
    let mut i = start;
    while i < src.len() {
        let b = src[i];
        if is_meta(b) {
            let proc_sub = (b == b'<' || b == b'>') && i == start && src.get(i + 1) == Some(&b'(');
            if proc_sub {
                match region_end(src, i + 2, depth) {
                    Some(rparen) => {
                        i = rparen + 1;
                        continue;
                    }
                    None => {
                        return WordScan {
                            end: src.len(),
                            unterminated: Some((i, UnterminatedKind::ProcSub)),
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
                    return WordScan {
                        end: src.len(),
                        unterminated: Some((i, UnterminatedKind::SingleQuote)),
                    };
                }
            },
            b'"' => match skip_dquote(src, i + 1, depth) {
                Some(end) => i = end,
                None => {
                    return WordScan {
                        end: src.len(),
                        unterminated: Some((i, UnterminatedKind::DoubleQuote)),
                    };
                }
            },
            b'\\' => i = (i + 2).min(src.len()),
            b'`' => match skip_backtick(src, i + 1) {
                Some(end) => i = end,
                None => {
                    return WordScan {
                        end: src.len(),
                        unterminated: Some((i, UnterminatedKind::Backtick)),
                    };
                }
            },
            b'$' => {
                let skip = skip_dollar(src, i, depth);
                if let Some((opener, kind)) = skip.opener {
                    return WordScan {
                        end: src.len(),
                        unterminated: Some((opener, kind)),
                    };
                }
                i = skip.end;
            }
            _ => i += 1,
        }
    }
    WordScan {
        end: i,
        unterminated: None,
    }
}

/// Find the `)` that closes a `$( … )` or `<( … )` whose body starts at
/// `start`.
///
/// This runs a full lexer pass over the body with a stop mode, so quotes,
/// nested substitutions and here-document bodies cannot end the substitution
/// early. `None` means the substitution runs to end of input.
pub(crate) fn region_end(src: &[u8], start: usize, depth: usize) -> Option<usize> {
    if depth >= MAX_NEST {
        return None;
    }
    let mut lx = Lexer::new(src, start, depth + 1, Some(StopMode { depth: 0 }));
    lx.run();
    if lx.stopped { Some(lx.i) } else { None }
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
    /// A redirection was lexed with no adjacent target; the next word is it.
    redirect_target: bool,
    cmd_pos: bool,
    depth: usize,
    stop: Option<StopMode>,
    cases: Vec<CaseState>,
    stopped: bool,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a [u8], i: usize, depth: usize, stop: Option<StopMode>) -> Self {
        Self {
            src,
            i,
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
                opener: op,
                kind: UnterminatedKind::HereDoc,
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
                    opener: op,
                    kind: UnterminatedKind::HereDoc,
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
            match scan_arith(self.src, self.i + 2, self.depth) {
                Some(end) => {
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
        let mut j = start;
        while j < self.src.len() && self.src[j].is_ascii_digit() {
            j += 1;
        }
        let rest = &self.src[j..];
        let len = if rest.starts_with(b"&>>") {
            3
        } else if rest.starts_with(b"&>") {
            2
        } else if rest.starts_with(b"<<<") || rest.starts_with(b"<<-") {
            3
        } else if rest.starts_with(b"<<")
            || rest.starts_with(b">>")
            || rest.starts_with(b">&")
            || rest.starts_with(b"<&")
            || rest.starts_with(b"<>")
            || rest.starts_with(b">|")
        {
            2
        } else if rest.starts_with(b"<") || rest.starts_with(b">") {
            1
        } else {
            // Should not happen: callers only route real redirects here.
            self.word();
            return;
        };
        let op_end = j + len;
        let op_text = &self.src[j..op_end];
        let heredoc = op_text == b"<<" || op_text == b"<<-";
        let strip_tabs = op_text == b"<<-";

        // A target written hard against the operator is part of this token.
        let mut end = op_end;
        let mut target: Option<Span> = None;
        if let Some(&b) = self.src.get(op_end) {
            if !is_meta(b) {
                let scan = scan_word(self.src, op_end, self.depth);
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
        let scan = scan_word(self.src, start, self.depth);
        self.i = scan.end;
        let text = &self.src[start..scan.end];
        if scan.unterminated.is_none() && text == b"\\\n" {
            // A bare backslash-newline between tokens. POSIX deletes it before
            // tokenising, so it is layout, not a word; the bytes stay in the
            // token stream, which keeps the lossless invariant.
            self.push(TokenKind::Whitespace, start, scan.end);
            return;
        }
        if let Some((opener, kind)) = scan.unterminated {
            self.push(TokenKind::WordUnterminated, start, scan.end);
            self.unterminated.push(Unterminated {
                span: start..scan.end,
                opener: opener..opener + 1,
                kind,
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
