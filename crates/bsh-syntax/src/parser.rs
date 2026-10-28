//! The recursive-descent parser, and the expectation stack that makes a
//! half-typed line answerable.
//!
//! One mechanism serves three consumers (`DESIGN.md` §4.10, §6.3):
//!
//! * **error messages** — an unexpected token is reported against what the
//!   parser was expecting;
//! * **`PS2` continuation** — running out of input inside an open construct is
//!   [`ParseOutcome::NeedMoreInput`], not an error;
//! * **completion** — [`expectations_at`] replays the parse of a prefix and
//!   reports what could be typed at an offset.
//!
//! The parser is written by hand because the errors are the product. It is
//! flat: every construct is a [`NodeId`] into one vector, so `((((((…` hits an
//! explicit depth limit and produces a diagnostic instead of a stack overflow.
//!
//! The ambiguity decisions of `DESIGN.md` §3.3 are implemented literally:
//! `((` always starts arithmetic, `a[1 + 2]=v` is rejected, and a function
//! definition requires an adjacent `()` pair. Bash-only constructs still
//! parse; [`ParseOpts::posix_mode`] gates them after the fact with a span and
//! an actionable help message, which is what makes `bsh -n --posix` a
//! portability checker rather than a second grammar.

use crate::ast::*;
use crate::codes;
use crate::lexer::{
    self, HereDoc, Unterminated, UnterminatedKind, find_assignment_eq, is_valid_name,
};
use crate::{Token, TokenKind};
use bsh_diag::{Diagnostic, Label, SourceFile, Span};
use bsh_lang::Transform;
use bsh_lang::{
    Arithmetic, CaseHow, CommandSub, Lit, LitKind, ParamExp, ParamName, ParamOp, Part, ProcSubDir,
    ProcessSub, ReplaceMode, TildeSpec, UnsupportedOp, Word,
};

/// The most diagnostics one parse will emit before giving up. Recovery is for
/// `bsh -n`, not for turning one missing quote into fifty complaints.
const MAX_ERRORS: usize = 24;

/// What may legally appear at a byte offset.
///
/// This maps 1:1 to a completion provider set (`DESIGN.md` §6.4): the parser
/// says `ForList` and the completer offers paths, without the completer
/// knowing anything about `for` syntax.
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
    /// The `then` of an `if`.
    Then,
    /// The `do` of a loop.
    Do,
    /// The `done` of a loop.
    Done,
    /// The `fi` of an `if`.
    Fi,
    /// The `esac` of a `case`.
    Esac,
    /// The `in` of a `for` or `case`.
    In,
    /// A function body: a compound command.
    FunctionBody,
    /// A closing or separating operator.
    Operator,
    /// The delimiter line of a here-document.
    HereDocDelimiter,
}

impl Expect {
    /// How specific this expectation is: smaller values are more specific.
    ///
    /// One slot can admit several productions (`if true; ` may be followed by
    /// another condition command *or* by `then`). [`expectations_at`] reports
    /// the construct's own continuation first and the generic command/word
    /// slot last, so a consumer that wants the "headline" answer reads the
    /// first entry and one that wants the slot the cursor is actually in reads
    /// the last.
    pub(crate) fn rank(self) -> u8 {
        match self {
            // The compound command's own keyword or body: the most specific
            // thing that can appear.
            Expect::FunctionBody | Expect::Then | Expect::Do | Expect::Done | Expect::In => 0,
            // The closing keyword of an open compound.
            Expect::Fi | Expect::Esac => 1,
            // A word or expression slot.
            Expect::RedirectTarget
            | Expect::ForList
            | Expect::CasePattern
            | Expect::ParamOp
            | Expect::TestOp
            | Expect::ArithExpr
            | Expect::HereDocDelimiter => 2,
            // A command or argument slot.
            Expect::Command | Expect::Word => 3,
            // A closing or separating operator.
            Expect::Operator => 4,
        }
    }

    /// A short human description, used in diagnostics and by pickers that have
    /// no better label.
    pub fn describe(self) -> &'static str {
        match self {
            Expect::Command => "a command",
            Expect::Word => "a word",
            Expect::RedirectTarget => "a redirection target",
            Expect::ForList => "a `for` word list",
            Expect::CasePattern => "a `case` pattern",
            Expect::ParamOp => "a parameter operator",
            Expect::TestOp => "a `[[` operator",
            Expect::ArithExpr => "an arithmetic expression",
            Expect::Then => "`then`",
            Expect::Do => "`do`",
            Expect::Done => "`done`",
            Expect::Fi => "`fi`",
            Expect::Esac => "`esac`",
            Expect::In => "`in`",
            Expect::FunctionBody => "a function body",
            Expect::Operator => "an operator",
            Expect::HereDocDelimiter => "a here-document delimiter",
        }
    }
}

/// One expectation together with the source span it governs.
///
/// [`expectation_sites_at`] returns these; [`expectations_at`] is the same
/// answer with the spans dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpectSite {
    /// What may appear here.
    pub expect: Expect,
    /// Where the completion applies.
    ///
    /// * For a word slot this is the word being typed, which may extend past
    ///   the cursor (`git che|ckout` → `4..12`).
    /// * For [`Expect::TestOp`] it is the operator's span: the complete
    ///   operator just typed (`[[ x == ` → `5..7`) or the partial one under
    ///   the cursor (`[[ -` → `3..4`), and an empty slot when no operator has
    ///   been typed yet.
    /// * For [`Expect::RedirectTarget`] it is the target word, or an empty
    ///   slot where the target belongs when none has been typed.
    /// * Otherwise it is an empty span at the cursor.
    pub span: Span,
}

/// Parser options.
///
/// This is deliberately a small local struct rather than `bsh_runtime`'s
/// option set: the syntax crate is below the runtime crate in the dependency
/// graph (`DESIGN.md` §4.1), so the shell maps its own options onto this by
/// value. Only flags that change *acceptance* live here; expansion-time
/// switches such as `nullglob` never affect the parse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseOpts {
    /// Reject bash-only syntax with [`codes::BASHISM`] diagnostics. This gates
    /// acceptance only; the grammar is always the bash superset
    /// (`DESIGN.md` §3.1).
    pub posix_mode: bool,
    /// Present for callers that map the shell's `shopt -s extglob` onto the
    /// front end, but it does **not** affect lexing or parsing: the lexer
    /// always takes the extglob reading of `?(` `*(` `+(` `@(` `!(` (the
    /// superset-grammar rule of `DESIGN.md` §3.3), and only pattern matching
    /// consults the option at run time. See [`crate::lexer`].
    pub extglob: bool,
    /// Maximum nesting depth of compound commands and substitutions. Exceeding
    /// it produces [`codes::DEPTH`] instead of overflowing the stack.
    pub max_depth: usize,
}

impl ParseOpts {
    /// The interactive default: bash syntax, extglob on, depth 200.
    pub const DEFAULT: ParseOpts = ParseOpts {
        posix_mode: false,
        extglob: true,
        max_depth: 200,
    };

    /// Strict POSIX.
    pub const POSIX: ParseOpts = ParseOpts {
        posix_mode: true,
        ..ParseOpts::DEFAULT
    };
}

impl Default for ParseOpts {
    fn default() -> Self {
        ParseOpts::DEFAULT
    }
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

/// Why [`parse_word`] or [`parse_word_at`] did not return exactly one word.
#[derive(Clone, Debug)]
pub enum ParseWordError {
    /// No word starts at the requested offset: input ended, or an operator, a
    /// redirection or here-document data is there instead.
    Missing {
        /// The offset that was asked about (clamped to the input).
        at: usize,
    },
    /// A non-layout token follows the word: the fragment is more than one
    /// word, or contains an operator such as `;` or `|`.
    Trailing {
        /// The span of the first such token.
        span: Span,
    },
    /// The word's token is an unterminated quote or substitution.
    Unterminated {
        /// The opening delimiter of the innermost construct left open.
        opener: Span,
        /// Where that construct's content begins.
        content_start: usize,
    },
    /// The parser rejected the word. Every diagnostic span is relative to the
    /// caller's `src`; nothing is synthetic.
    Invalid(Vec<Diagnostic>),
}

/// Parse exactly one word, starting at the first non-layout byte of `src`.
///
/// This is the entry point for fragments that are words but not commands —
/// `[[ … ]]` operands, `case` patterns (without their terminating `)`) and the
/// `$…` constructs inside a here-document body. It consumes the word's token,
/// rejects anything that would be a second word or an operator, and returns
/// spans relative to `src`: no synthetic command is built and no offset is
/// rewritten, so a diagnostic points at the caller's own bytes.
///
/// Leading layout (spaces, tabs, newlines, comments) is skipped; trailing
/// layout is allowed. The profile in `opts` applies, so a POSIX profile
/// rejects `$'…'` and other bashisms as [`ParseWordError::Invalid`].
///
/// See [`parse_word_at`] to start at an offset and learn where the word ended.
pub fn parse_word(src: &[u8], opts: &ParseOpts) -> Result<Word, ParseWordError> {
    parse_word_at(src, 0, opts).map(|(word, _)| word)
}

/// Parse exactly one word starting at `offset`, returning it and the offset
/// just past it (before any trailing layout).
///
/// `offset` must be the start of a word, or inside leading layout before one;
/// an offset in the middle of a word is [`ParseWordError::Missing`]. The same
/// contract as [`parse_word`] applies: exactly one word, spans relative to
/// `src`.
pub fn parse_word_at(
    src: &[u8],
    offset: usize,
    opts: &ParseOpts,
) -> Result<(Word, usize), ParseWordError> {
    let offset = offset.min(src.len());
    let lexed = lexer::lex_full_with(src, opts.extglob);
    let mut index = None;
    for (i, token) in lexed.tokens.iter().enumerate() {
        if matches!(
            token.kind,
            TokenKind::Whitespace | TokenKind::Newline | TokenKind::Comment
        ) {
            continue;
        }
        if token.span.end <= offset {
            continue;
        }
        if token.span.start < offset {
            return Err(ParseWordError::Missing { at: offset });
        }
        index = Some(i);
        break;
    }
    let Some(index) = index else {
        return Err(ParseWordError::Missing { at: offset });
    };
    let token = lexed.tokens[index].clone();

    match token.kind {
        TokenKind::Word | TokenKind::Keyword => {}
        TokenKind::WordUnterminated => {
            let open = lexed
                .unterminated
                .iter()
                .rev()
                .find(|u| u.span.start >= token.span.start && u.span.end <= token.span.end)
                .map(|u| (u.opener.clone(), u.content_start))
                .or_else(|| {
                    lexed
                        .open
                        .first()
                        .map(|o| (o.opener.clone(), o.content_start))
                });
            return match open {
                Some((opener, content_start)) => Err(ParseWordError::Unterminated {
                    opener,
                    content_start,
                }),
                None => Err(ParseWordError::Missing {
                    at: token.span.start,
                }),
            };
        }
        _ => {
            return Err(ParseWordError::Missing {
                at: token.span.start,
            });
        }
    }

    if let Some(trailing) = lexed.tokens[index + 1..].iter().find(|t| {
        !matches!(
            t.kind,
            TokenKind::Whitespace | TokenKind::Newline | TokenKind::Comment
        )
    }) {
        return Err(ParseWordError::Trailing {
            span: trailing.span.clone(),
        });
    }

    let mut sh = Shared::default();
    let mut p = Parser {
        src,
        toks: lexed.tokens,
        pos: index + 1,
        opts: *opts,
        depth: 0,
        sh: &mut sh,
        expects: Vec::new(),
        open: Vec::new(),
        pending_heredocs: 0,
        next_heredoc: 0,
        lexed_heredocs: lexed.heredocs,
        unterminated: lexed.unterminated,
    };
    let word = p.parse_word_span(token.span.clone());
    let errors = std::mem::take(&mut sh.errors);
    if !errors.is_empty() {
        return Err(ParseWordError::Invalid(errors));
    }
    Ok((word, token.span.end))
}

/// Parse with the default profile.
pub fn parse(src: &SourceFile) -> ParseOutcome {
    parse_with(src, &ParseOpts::DEFAULT)
}

/// Parse with an explicit profile.
pub fn parse_with(src: &SourceFile, opts: &ParseOpts) -> ParseOutcome {
    parse_bytes(src.bytes(), opts)
}

/// Parse raw bytes with an explicit profile.
pub fn parse_bytes(src: &[u8], opts: &ParseOpts) -> ParseOutcome {
    parse_inner(src, opts).0
}

/// Report what could legally be typed at `offset` in `src`.
///
/// `offset` is clamped to the input, and the answer is for **the cursor**:
/// when it sits in a word that is still being typed, that word's own slot is
/// reported rather than the slot after it. `ec` at offset 2 answers
/// [`Expect::Command`], `ls ` at 3 answers [`Expect::Word`], and
/// `for f in ` at 9 answers [`Expect::ForList`].
///
/// A cursor inside an unterminated quote, substitution or here-document
/// reports the **innermost** open construct (see [`expectations_after`] for
/// the pre-cursor-semantics answer). Inside `[[ … ]]` the answer is
/// [`Expect::TestOp`].
///
/// The vector is ordered most-specific first: a compound command's own
/// keyword comes before the generic command slot, so `if true; ` yields
/// `[Then, Command]`.
pub fn expectations_at(src: &[u8], offset: usize, opts: &ParseOpts) -> Vec<Expect> {
    expectation_sites_at(src, offset, opts)
        .into_iter()
        .map(|site| site.expect)
        .collect()
}

/// [`expectations_at`] with the source span each expectation governs.
pub fn expectation_sites_at(src: &[u8], offset: usize, opts: &ParseOpts) -> Vec<ExpectSite> {
    sites_at(src, offset, opts, 0)
}

fn sites_at(src: &[u8], offset: usize, opts: &ParseOpts, depth: usize) -> Vec<ExpectSite> {
    let offset = offset.min(src.len());
    let prefix = &src[..offset];
    let lexed = lexer::lex_full(prefix);

    // 1. A construct is still open at the cursor: the lexer's nesting stack is
    //    the answer, innermost first.
    if !lexed.open.is_empty() {
        return sites_for_open(&lexed, src, offset, opts, depth);
    }

    // 2. The cursor is in a word that is still being typed, or in a
    //    redirection. Ask about the slot that token occupies.
    if let Some(token) = token_under_cursor(src, offset, &lexed) {
        match token.kind {
            TokenKind::Word | TokenKind::WordUnterminated | TokenKind::Keyword => {
                let up_to = &src[..token.span.start];
                let mut sites = parse_inner(up_to, opts).1;
                if sites.is_empty() {
                    sites.push(ExpectSite {
                        expect: Expect::Command,
                        span: token.span.clone(),
                    });
                }
                for site in &mut sites {
                    site.span = token.span.clone();
                }
                return sites;
            }
            TokenKind::Redirect => {
                let op_len = token.redirect_op_len(src).unwrap_or(0);
                let op_end = token.span.start + op_len;
                return if offset >= op_end {
                    vec![ExpectSite {
                        expect: Expect::RedirectTarget,
                        span: op_end..token.span.end,
                    }]
                } else {
                    vec![ExpectSite {
                        expect: Expect::Operator,
                        span: token.span.clone(),
                    }]
                };
            }
            _ => {}
        }
    }

    // 3. The slot after the prefix.
    let mut sites = parse_inner(prefix, opts).1;
    if sites.is_empty() {
        sites.push(ExpectSite {
            expect: Expect::Command,
            span: offset..offset,
        });
    }
    sites
}

/// The expectations for the innermost open construct at the cursor.
///
/// A substitution body is shell code of its own, so the answer is computed for
/// that region and shifted back into the caller's coordinates. Only a quote,
/// `${ … }`, arithmetic or a here-document is a leaf: there the innermost
/// construct itself names the slot.
///
/// The enclosing word is reported as a redirection target when it is one, so
/// `echo > "abc` yields `[RedirectTarget, Word]` rather than losing the fact
/// that the word names a file.
fn sites_for_open(
    lexed: &lexer::Lexed,
    src: &[u8],
    offset: usize,
    opts: &ParseOpts,
    depth: usize,
) -> Vec<ExpectSite> {
    let open = lexed
        .open
        .first()
        .expect("caller checked that something is open");
    let mut sites = Vec::with_capacity(2);

    // The outer slot: a construct inside a redirection target is still part of
    // that target's word.
    let word = lexed.tokens.iter().rev().find(|t| {
        matches!(
            t.kind,
            TokenKind::Word | TokenKind::WordUnterminated | TokenKind::Keyword
        ) && t.span.start <= open.opener.start
            && open.opener.start < t.span.end
    });
    if let Some(word) = word {
        let before = lexed
            .tokens
            .iter()
            .rev()
            .find(|t| !is_ignorable(t.kind) && t.span.end <= word.span.start);
        if before.is_some_and(|t| t.kind == TokenKind::Redirect) {
            sites.push(ExpectSite {
                expect: Expect::RedirectTarget,
                span: word.span.clone(),
            });
        }
    }

    // A substitution body is a fresh code region; answer for it directly.
    if depth < lexer::MAX_NEST {
        if let Some(code) = lexed.open.iter().find(|open| {
            matches!(
                open.kind,
                UnterminatedKind::CmdSub | UnterminatedKind::Backtick | UnterminatedKind::ProcSub
            )
        }) {
            if code.content_start <= offset {
                let mut inner = sites_at(
                    &src[code.content_start..offset],
                    offset - code.content_start,
                    opts,
                    depth + 1,
                );
                for site in &mut inner {
                    site.span = (site.span.start + code.content_start)
                        ..(site.span.end + code.content_start);
                }
                sites.extend(inner);
                return sites;
            }
        }
    }

    sites.push(ExpectSite {
        expect: expect_for_unterminated(open.kind),
        span: open.content_start.min(offset)..offset,
    });
    sites
}

/// The significant token the cursor is inside, when that token is still being
/// typed.
///
/// A token ending at a cursor that is not at end of input has finished (the
/// next byte is layout or a metacharacter), so the cursor is between words.
fn token_under_cursor(src: &[u8], offset: usize, prefix: &lexer::Lexed) -> Option<Token> {
    if offset < src.len() {
        // The prefix lexer truncates the token the cursor is in; lex the whole
        // input to find its real extent.
        return lexer::lex_full(src)
            .tokens
            .into_iter()
            .find(|t| !is_ignorable(t.kind) && t.span.start < offset && offset < t.span.end);
    }
    prefix
        .tokens
        .iter()
        .rev()
        .find(|t| !is_ignorable(t.kind) && t.span.start < offset && t.span.end == offset)
        .cloned()
}

/// What may follow the prefix `src[..offset]`, ignoring a token that is still
/// being typed at the cursor.
///
/// This is the behaviour [`expectations_at`] had before it learned cursor
/// semantics: `ec` at 2 answers [`Expect::Word`] (the argument slot after the
/// command word) rather than [`Expect::Command`], and `echo > fi` at 9 answers
/// [`Expect::Word`] rather than [`Expect::RedirectTarget`]. Completion should
/// use [`expectations_at`]; this stays available for callers that want to look
/// past the current token.
pub fn expectations_after(src: &[u8], offset: usize, opts: &ParseOpts) -> Vec<Expect> {
    let offset = offset.min(src.len());
    let prefix = &src[..offset];
    parse_inner(prefix, opts)
        .1
        .into_iter()
        .map(|site| site.expect)
        .collect()
}

fn parse_inner(src: &[u8], opts: &ParseOpts) -> (ParseOutcome, Vec<ExpectSite>) {
    parse_lexed(src, opts, lexer::lex_full(src))
}

fn parse_lexed(
    src: &[u8],
    opts: &ParseOpts,
    lexed: lexer::Lexed,
) -> (ParseOutcome, Vec<ExpectSite>) {
    let mut sh = Shared::default();
    let mut p = Parser {
        src,
        toks: lexed.tokens,
        pos: 0,
        opts: *opts,
        depth: 0,
        sh: &mut sh,
        expects: Vec::new(),
        open: Vec::new(),
        pending_heredocs: 0,
        next_heredoc: 0,
        lexed_heredocs: lexed.heredocs,
        unterminated: lexed.unterminated,
    };
    let root = p.parse_list(Stop::Top);
    let tokens = std::mem::take(&mut p.toks);
    drop(p);

    let mut sites = std::mem::take(&mut sh.sites);
    if sites.is_empty() {
        sites.push(ExpectSite {
            expect: Expect::Command,
            span: src.len()..src.len(),
        });
    }
    let outcome = if !sh.errors.is_empty() {
        ParseOutcome::Failed(std::mem::take(&mut sh.errors))
    } else if let Some(inc) = sh.incomplete.take() {
        let expected = if inc.expected.is_empty() {
            sites.iter().map(|site| site.expect).collect()
        } else {
            inc.expected
        };
        ParseOutcome::NeedMoreInput {
            open: inc.open,
            expected,
        }
    } else {
        ParseOutcome::Ok(Ast {
            root,
            nodes: std::mem::take(&mut sh.b.nodes),
            heredocs: std::mem::take(&mut sh.b.heredocs),
            tokens,
        })
    };
    (outcome, sites)
}

fn expect_for_unterminated(kind: UnterminatedKind) -> Expect {
    match kind {
        UnterminatedKind::SingleQuote
        | UnterminatedKind::DoubleQuote
        | UnterminatedKind::AnsiCQuote => Expect::Word,
        UnterminatedKind::ParamExp => Expect::ParamOp,
        UnterminatedKind::CmdSub | UnterminatedKind::Backtick | UnterminatedKind::ProcSub => {
            Expect::Command
        }
        UnterminatedKind::Arith | UnterminatedKind::ArithCommand => Expect::ArithExpr,
        UnterminatedKind::HereDoc => Expect::HereDocDelimiter,
    }
}

#[derive(Default)]
struct Shared {
    b: Builder,
    errors: Vec<Diagnostic>,
    incomplete: Option<Incomplete>,
    /// Expectations recorded at end of input, most specific first.
    sites: Vec<ExpectSite>,
    /// Whether `incomplete` was recorded by this parser rather than adopted
    /// from a command-substitution body (whose cursor context differs).
    incomplete_local: bool,
}

#[derive(Default)]
struct Builder {
    nodes: Vec<Node>,
    heredocs: Vec<HereDoc>,
}

struct Incomplete {
    open: Span,
    expected: Vec<Expect>,
}

/// Which reserved words end a nested list.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    Top,
    Brace,
    Paren,
    IfBody,
    LoopBody,
    CaseArm,
}

struct Parser<'a, 's> {
    src: &'a [u8],
    toks: Vec<Token>,
    pos: usize,
    opts: ParseOpts,
    depth: usize,
    sh: &'s mut Shared,
    expects: Vec<ExpectSite>,
    /// Spans of constructs currently open, innermost last.
    open: Vec<Span>,
    pending_heredocs: usize,
    next_heredoc: usize,
    lexed_heredocs: Vec<HereDoc>,
    unterminated: Vec<Unterminated>,
}

impl<'a> Parser<'a, '_> {
    // ---------------------------------------------------------------- tokens

    fn text_of(&self, t: &Token) -> &'a [u8] {
        let src: &'a [u8] = self.src;
        &src[t.span.clone()]
    }

    fn skip_trivia(&mut self) {
        while let Some(t) = self.toks.get(self.pos) {
            if is_ignorable(t.kind) {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn at_end(&mut self) -> bool {
        self.skip_trivia();
        self.pos >= self.toks.len()
    }

    fn peek_sig(&mut self) -> Option<Token> {
        self.skip_trivia();
        self.toks.get(self.pos).cloned()
    }

    fn peek_kind(&mut self) -> Option<TokenKind> {
        self.peek_sig().map(|t| t.kind)
    }

    fn peek_sig_n(&mut self, n: usize) -> Option<Token> {
        let mut pos = self.pos;
        let mut seen = 0;
        while pos < self.toks.len() {
            let t = &self.toks[pos];
            if is_ignorable(t.kind) {
                pos += 1;
                continue;
            }
            if seen == n {
                return Some(t.clone());
            }
            seen += 1;
            pos += 1;
        }
        None
    }

    fn advance_exact(&mut self) -> Option<Token> {
        let t = self.toks.get(self.pos).cloned()?;
        self.pos += 1;
        if t.kind == TokenKind::Newline && self.pending_heredocs > 0 {
            self.skip_heredoc_tokens();
        }
        Some(t)
    }

    /// Skip trivia and consume the next significant token.
    ///
    /// Use this after a lookahead that did *not* move `pos` (such as
    /// [`Parser::peek_sig_n`]); [`Parser::advance_exact`] assumes `pos` is
    /// already on the token.
    fn bump_sig(&mut self) -> Option<Token> {
        self.skip_trivia();
        self.advance_exact()
    }

    fn skip_heredoc_tokens(&mut self) {
        let n = std::mem::take(&mut self.pending_heredocs);
        let mut seen = 0;
        while seen < n {
            match self.toks.get(self.pos).map(|t| t.kind) {
                Some(TokenKind::HereDocBody) => self.pos += 1,
                Some(TokenKind::HereDocDelimiter) => {
                    self.pos += 1;
                    if self.toks.get(self.pos).map(|t| t.kind) == Some(TokenKind::Newline) {
                        self.pos += 1;
                    }
                    seen += 1;
                }
                _ => break,
            }
        }
    }

    fn skip_newlines(&mut self) {
        loop {
            self.skip_trivia();
            match self.toks.get(self.pos).map(|t| t.kind) {
                Some(TokenKind::Newline) => {
                    self.pos += 1;
                    if self.pending_heredocs > 0 {
                        self.skip_heredoc_tokens();
                    }
                }
                _ => break,
            }
        }
    }

    fn prev_end(&self) -> usize {
        self.toks[..self.pos]
            .iter()
            .rev()
            .find(|t| !is_ignorable(t.kind))
            .map_or(0, |t| t.span.end)
    }

    fn here(&mut self) -> Option<Token> {
        self.peek_sig()
    }

    fn is_keyword(&mut self, word: &[u8]) -> bool {
        match self.peek_sig() {
            Some(t) => t.kind == TokenKind::Keyword && self.text_of(&t) == word,
            None => false,
        }
    }

    /// Whether the next token is `word`, accepting a `Word` token as well as a
    /// `Keyword` one.
    ///
    /// The lexer only classifies a reserved word as `Keyword` in command
    /// position. A few grammar slots take a reserved word right after an
    /// ordinary word (`for name in …`, `case word in …`), where the lexer
    /// cannot know; those slots use this predicate.
    fn is_text_keyword(&mut self, word: &[u8]) -> bool {
        match self.peek_sig() {
            Some(t) => {
                matches!(t.kind, TokenKind::Keyword | TokenKind::Word) && self.text_of(&t) == word
            }
            None => false,
        }
    }

    fn eat_text_keyword(&mut self, word: &'static [u8], e: Expect) -> bool {
        match self.take_expect(e) {
            None => false,
            Some(t) => {
                if matches!(t.kind, TokenKind::Keyword | TokenKind::Word)
                    && self.text_of(&t) == word
                {
                    true
                } else {
                    self.expected(e, &t);
                    false
                }
            }
        }
    }

    fn is_operator(&mut self, op: &[u8]) -> bool {
        match self.peek_sig() {
            Some(t) => t.kind == TokenKind::Operator && self.text_of(&t) == op,
            None => false,
        }
    }

    fn eat_operator(&mut self, op: &[u8]) -> bool {
        if self.is_operator(op) {
            self.advance_exact();
            true
        } else {
            false
        }
    }

    // -------------------------------------------------------- expectations

    /// Record `e` as the expectation for the current slot without consuming
    /// anything.
    fn record_expect_only(&mut self, e: Expect) {
        let span = self.slot_span();
        self.record_site(e, span);
        self.create_incomplete();
    }

    /// Remember that input ended inside the innermost construct still open.
    fn create_incomplete(&mut self) {
        if self.sh.incomplete.is_none() {
            if let Some(open) = self.open.last().cloned() {
                self.sh.incomplete = Some(Incomplete {
                    open,
                    expected: self.sites_expects(),
                });
                self.sh.incomplete_local = true;
            }
        }
    }

    /// Record `e` for a slot whose span is already known.
    fn record_site(&mut self, e: Expect, span: Span) {
        self.offer_site(ExpectSite { expect: e, span });
    }

    /// The span the next token would occupy: the next significant token's
    /// span, or an empty slot at end of input.
    fn slot_span(&mut self) -> Span {
        self.skip_trivia();
        match self.toks.get(self.pos) {
            Some(t) => t.span.clone(),
            None => {
                let at = self.src.len();
                at..at
            }
        }
    }

    fn sites_expects(&self) -> Vec<Expect> {
        self.sh.sites.iter().map(|site| site.expect).collect()
    }

    /// Add one expectation to the end-of-input answer, keeping the vector
    /// ordered most-specific first and free of duplicates.
    fn offer_site(&mut self, site: ExpectSite) {
        if self
            .sh
            .sites
            .iter()
            .any(|candidate| candidate.expect == site.expect)
        {
            return;
        }
        let rank = site.expect.rank();
        let at = self
            .sh
            .sites
            .iter()
            .position(|candidate| candidate.expect.rank() > rank)
            .unwrap_or(self.sh.sites.len());
        self.sh.sites.insert(at, site);
        if self.sh.incomplete_local {
            let expected = self.sites_expects();
            if let Some(inc) = self.sh.incomplete.as_mut() {
                inc.expected = expected;
            }
        }
    }

    /// Snapshot the expectation stack at end of input.
    fn record_eof(&mut self) {
        let frames = std::mem::take(&mut self.expects);
        for frame in &frames {
            self.offer_site(frame.clone());
        }
        self.expects = frames;
        self.create_incomplete();
    }

    fn incomplete_at(&mut self, open: Span) {
        if self.sh.incomplete.is_none() {
            self.sh.incomplete = Some(Incomplete {
                open,
                expected: self.sites_expects(),
            });
            self.sh.incomplete_local = true;
        }
    }

    fn incomplete_at_expect(&mut self, open: Span, e: Expect) {
        let span = self.slot_span();
        self.record_site(e, span);
        self.incomplete_at(open);
    }

    fn peek_expect(&mut self, e: Expect) -> Option<Token> {
        self.skip_trivia();
        let span = self.slot_span();
        self.expects.push(ExpectSite { expect: e, span });
        let t = self.toks.get(self.pos).cloned();
        if t.is_none() {
            self.record_eof();
        }
        self.expects.pop();
        t
    }

    fn take_expect(&mut self, e: Expect) -> Option<Token> {
        let t = self.peek_expect(e)?;
        self.advance_exact();
        Some(t)
    }

    // -------------------------------------------------------------- errors

    fn push_error(&mut self, d: Diagnostic) {
        if self.sh.errors.len() >= MAX_ERRORS {
            return;
        }
        let duplicate = self
            .sh
            .errors
            .iter()
            .any(|e| e.code == d.code && e.span == d.span && e.message == d.message);
        if !duplicate {
            self.sh.errors.push(d);
        }
    }

    /// Attach a secondary "opened here" label to the most recent diagnostic.
    fn label_last(&mut self, span: Span, message: &'static str) {
        if let Some(d) = self.sh.errors.last_mut() {
            if d.labels
                .iter()
                .any(|l| l.span == span && l.message == message)
            {
                return;
            }
            d.labels.push(Label { span, message });
        }
    }

    fn unexpected(&mut self, t: &Token) {
        let what = show_token(self.text_of(t), t.kind);
        let mut d = Diagnostic::error(
            codes::UNEXPECTED,
            t.span.clone(),
            format!("unexpected {what}"),
        );
        if let Some(help) = orphan_help(self.text_of(t), t.kind) {
            d = d.with_help(help);
        }
        self.push_error(d);
    }

    fn unexpected_current(&mut self) {
        if let Some(t) = self.here() {
            self.unexpected(&t);
        }
    }

    fn expected(&mut self, e: Expect, t: &Token) {
        let what = show_token(self.text_of(t), t.kind);
        let d = Diagnostic::error(
            codes::UNEXPECTED,
            t.span.clone(),
            format!("expected {}, found {what}", e.describe()),
        )
        .with_help(format!("expected {}", e.describe()));
        self.push_error(d);
    }

    fn expected_current(&mut self, e: Expect) {
        if let Some(t) = self.peek_sig() {
            self.expected(e, &t);
        }
    }

    fn posix(&mut self, span: Span, message: &str, help: &str) {
        if !self.opts.posix_mode {
            return;
        }
        let d = Diagnostic::error(codes::BASHISM, span.clone(), message)
            .with_help(help)
            .with_label(span, "not valid in POSIX mode");
        self.push_error(d);
    }

    fn recover(&mut self) {
        let mut guard = 0usize;
        while let Some(t) = self.toks.get(self.pos).cloned() {
            if guard > 4096 {
                break;
            }
            guard += 1;
            match t.kind {
                TokenKind::Newline => break,
                TokenKind::Operator
                    if matches!(
                        self.text_of(&t),
                        b";" | b"&" | b";;" | b";&" | b";;&" | b")" | b"}"
                    ) =>
                {
                    break;
                }
                TokenKind::Keyword if is_reserved_stop(self.text_of(&t)) => break,
                _ => {}
            }
            self.advance_exact();
        }
    }

    fn recover_to_keyword(&mut self, words: &[&[u8]]) {
        let mut guard = 0usize;
        while let Some(t) = self.toks.get(self.pos).cloned() {
            if guard > 4096 {
                break;
            }
            guard += 1;
            if t.kind == TokenKind::Keyword && words.iter().any(|w| *w == self.text_of(&t)) {
                break;
            }
            self.advance_exact();
        }
    }

    // --------------------------------------------------------------- nodes

    fn new_node(&mut self, kind: NodeKind, span: Span) -> NodeId {
        let id = self.sh.b.nodes.len() as NodeId;
        self.sh.b.nodes.push(Node { kind, span });
        id
    }

    fn empty_node(&mut self, span: Span) -> NodeId {
        self.new_node(NodeKind::Empty, span)
    }

    // ---------------------------------------------------------------- lists

    fn parse_list(&mut self, stop: Stop) -> NodeId {
        let start = self.toks.get(self.pos).map_or(0, |t| t.span.start);
        let mut items: Vec<ListItem> = Vec::new();
        loop {
            self.skip_newlines();
            if self.at_end() {
                self.record_expect_only(Expect::Command);
                break;
            }
            if self.at_stop(stop) {
                break;
            }
            if self.sh.errors.len() >= MAX_ERRORS {
                break;
            }
            let before = self.pos;
            let node = match self.parse_and_or() {
                Some(n) => n,
                None => {
                    if self.pos == before {
                        self.unexpected_current();
                        self.advance_exact();
                        self.recover();
                    }
                    if self.at_end() {
                        break;
                    }
                    continue;
                }
            };
            if self.pos == before {
                self.advance_exact();
            }
            self.skip_trivia();
            let sep = self.consume_separator();
            items.push(ListItem { node, sep });
            if sep.is_none() {
                self.skip_trivia();
                if self.at_end() || self.at_stop(stop) {
                    break;
                }
                self.unexpected_current();
                self.recover();
                if self.at_end() {
                    break;
                }
            }
        }
        let end = self.prev_end().max(start);
        self.new_node(NodeKind::List { items }, start..end)
    }

    fn consume_separator(&mut self) -> Option<Separator> {
        self.skip_trivia();
        let t = self.toks.get(self.pos).cloned()?;
        match t.kind {
            TokenKind::Newline => {
                self.advance_exact();
                Some(Separator::Newline)
            }
            TokenKind::Operator => match self.text_of(&t) {
                b";" => {
                    self.advance_exact();
                    Some(Separator::Semi)
                }
                b"&" => {
                    self.advance_exact();
                    Some(Separator::Amp)
                }
                _ => None,
            },
            _ => None,
        }
    }

    fn at_stop(&mut self, stop: Stop) -> bool {
        let Some(t) = self.peek_sig() else {
            return false;
        };
        let text = self.text_of(&t);
        match stop {
            Stop::Top => false,
            Stop::Brace => t.kind == TokenKind::Operator && text == b"}",
            Stop::Paren => t.kind == TokenKind::Operator && text == b")",
            Stop::IfBody => {
                t.kind == TokenKind::Keyword && matches!(text, b"then" | b"elif" | b"else" | b"fi")
            }
            Stop::LoopBody => t.kind == TokenKind::Keyword && matches!(text, b"do" | b"done"),
            Stop::CaseArm => {
                (t.kind == TokenKind::Operator && matches!(text, b";;" | b";&" | b";;&"))
                    || (t.kind == TokenKind::Keyword && text == b"esac")
            }
        }
    }

    /// Whether a list node ended at a separator, so a reserved word may follow
    /// it immediately. An empty list does not qualify: `if ` still needs a
    /// condition command before `then`.
    fn list_ends_with_separator(&self, id: NodeId) -> bool {
        match &self.sh.b.nodes[id as usize].kind {
            NodeKind::List { items } => items.last().is_some_and(|item| item.sep.is_some()),
            _ => false,
        }
    }

    /// Whether a reserved word may follow a list node: it is empty, or its
    /// last item ended at a separator.
    fn list_allows_keyword(&self, id: NodeId) -> bool {
        match &self.sh.b.nodes[id as usize].kind {
            NodeKind::List { items } => {
                items.is_empty() || items.last().is_some_and(|item| item.sep.is_some())
            }
            _ => false,
        }
    }

    // ------------------------------------------------------- and-or / pipes

    fn parse_and_or(&mut self) -> Option<NodeId> {
        let first = self.parse_pipeline()?;
        let mut rest: Vec<AndOrLink> = Vec::new();
        loop {
            self.skip_trivia();
            let Some(t) = self.toks.get(self.pos).cloned() else {
                break;
            };
            if t.kind != TokenKind::Operator {
                break;
            }
            let op = match self.text_of(&t) {
                b"&&" => AndOrOp::And,
                b"||" => AndOrOp::Or,
                _ => break,
            };
            let op_span = t.span.clone();
            self.advance_exact();
            self.skip_newlines();
            let rhs = match self.parse_pipeline() {
                Some(n) => n,
                None => {
                    if self.at_end() {
                        self.incomplete_at_expect(op_span, Expect::Command);
                    } else {
                        self.unexpected_current();
                        self.advance_exact();
                        self.recover();
                    }
                    break;
                }
            };
            rest.push(AndOrLink { op, node: rhs });
        }
        let end = self.prev_end();
        let start = self.sh.b.nodes[first as usize].span.start;
        Some(self.new_node(NodeKind::AndOr { first, rest }, start..end))
    }

    fn parse_pipeline(&mut self) -> Option<NodeId> {
        self.skip_trivia();
        let start = self.toks.get(self.pos).map(|t| t.span.start)?;
        let mut negated = false;
        if let Some(t) = self.peek_sig() {
            if t.kind == TokenKind::Keyword && self.text_of(&t) == b"!" {
                let bang = t.span.clone();
                self.advance_exact();
                self.skip_newlines();
                negated = true;
                if self.at_end() {
                    self.incomplete_at_expect(bang, Expect::Command);
                    return None;
                }
            }
        }
        let first = self.parse_command()?;
        let mut stages = vec![first];
        let mut links: Vec<PipeOp> = Vec::new();
        loop {
            self.skip_trivia();
            let Some(t) = self.toks.get(self.pos).cloned() else {
                break;
            };
            if t.kind != TokenKind::Operator {
                break;
            }
            let link = match self.text_of(&t) {
                b"|" => PipeOp::Pipe,
                b"|&" => PipeOp::PipeStderr,
                _ => break,
            };
            let op_span = t.span.clone();
            self.advance_exact();
            self.skip_newlines();
            let next = match self.parse_command() {
                Some(n) => n,
                None => {
                    if self.at_end() {
                        self.incomplete_at_expect(op_span, Expect::Command);
                    } else {
                        self.unexpected_current();
                        self.advance_exact();
                        self.recover();
                    }
                    break;
                }
            };
            stages.push(next);
            links.push(link);
            if link == PipeOp::PipeStderr {
                self.posix(
                    op_span,
                    "`|&` is a bash extension",
                    "POSIX: write `2>&1 |`, or use a pipe from a subshell",
                );
            }
        }
        if stages.len() == 1 && !negated {
            return Some(stages[0]);
        }
        let end = self.prev_end();
        Some(self.new_node(
            NodeKind::Pipeline {
                negated,
                stages,
                links,
            },
            start..end,
        ))
    }

    // ------------------------------------------------------------- commands

    fn parse_command(&mut self) -> Option<NodeId> {
        if self.depth >= self.opts.max_depth {
            let span = self.toks.get(self.pos).map_or(0..0, |t| t.span.clone());
            let d = Diagnostic::error(codes::DEPTH, span, "nesting is too deep to parse")
                .with_help(format!(
                    "the limit is {} levels; simplify the command or raise the limit",
                    self.opts.max_depth
                ));
            self.push_error(d);
            // Consume one token so the caller cannot loop forever.
            self.advance_exact();
            return None;
        }
        self.depth += 1;
        let result = self.parse_command_inner();
        self.depth -= 1;
        result
    }

    fn parse_command_inner(&mut self) -> Option<NodeId> {
        let t = self.peek_expect(Expect::Command)?;
        let text = self.text_of(&t);
        match t.kind {
            TokenKind::Keyword => match text {
                b"if" => {
                    let n = self.parse_if();
                    Some(self.parse_trailing_redirects(n))
                }
                b"for" => {
                    let n = self.parse_for();
                    Some(self.parse_trailing_redirects(n))
                }
                b"while" => {
                    let n = self.parse_while(false);
                    Some(self.parse_trailing_redirects(n))
                }
                b"until" => {
                    let n = self.parse_while(true);
                    Some(self.parse_trailing_redirects(n))
                }
                b"case" => {
                    let n = self.parse_case();
                    Some(self.parse_trailing_redirects(n))
                }
                b"function" => Some(self.parse_function_keyword()),
                b"[[" => {
                    let n = self.parse_test();
                    Some(self.parse_trailing_redirects(n))
                }
                b"then" | b"elif" | b"else" | b"fi" | b"do" | b"done" | b"esac" | b"in" => None,
                _ => self.parse_simple_or_function(),
            },
            TokenKind::Operator => match text {
                b"{" => Some(self.parse_brace_group()),
                b"(" => Some(self.parse_subshell()),
                _ => None,
            },
            TokenKind::ArithCommand => {
                let n = self.parse_arith_command();
                Some(self.parse_trailing_redirects(n))
            }
            TokenKind::Redirect | TokenKind::Word | TokenKind::WordUnterminated => {
                self.parse_simple_or_function()
            }
            _ => None,
        }
    }

    fn parse_simple_or_function(&mut self) -> Option<NodeId> {
        let before = self.pos;
        if let Some(node) = self.try_function_def() {
            return Some(node);
        }
        if self.pos != before {
            // A function definition was attempted and consumed its header; do
            // not re-read those tokens as a simple command.
            return None;
        }
        self.parse_simple_command()
    }

    fn try_function_def(&mut self) -> Option<NodeId> {
        let name_tok = self.peek_sig()?;
        if name_tok.kind != TokenKind::Word {
            return None;
        }
        let name_text = self.text_of(&name_tok);
        if name_text.is_empty() || find_assignment_eq(self.src, &name_tok.span).is_some() {
            return None;
        }
        let open = self.peek_sig_n(1)?;
        let close = self.peek_sig_n(2)?;
        if open.kind != TokenKind::Operator || self.text_of(&open) != b"(" {
            return None;
        }
        if close.kind != TokenKind::Operator || self.text_of(&close) != b")" {
            return None;
        }
        let start = name_tok.span.start;
        if open.span.end != close.span.start {
            // DESIGN.md §3.3: require `()` adjacency, so the decision needs one
            // token of lookahead and never a re-lex.
            let d = Diagnostic::error(
                codes::UNEXPECTED,
                open.span.start..close.span.end,
                "a function definition needs `()` with no space",
            )
            .with_help("write `f() { …; }`, or run the command with a space");
            self.push_error(d);
            self.label_last(name_tok.span.clone(), "this is the function name");
        }
        self.bump_sig(); // name
        self.skip_trivia();
        self.bump_sig(); // (
        self.skip_trivia();
        self.bump_sig(); // )
        self.skip_newlines();
        let body = match self.parse_compound_body() {
            Some(b) => b,
            None => {
                if self.at_end() {
                    self.incomplete_at(name_tok.span.clone());
                }
                return None;
            }
        };
        let body = self.parse_trailing_redirects(body);
        let end = self.prev_end();
        let name = self.parse_word_span(name_tok.span.clone());
        Some(self.new_node(
            NodeKind::Function {
                name,
                body,
                keyword: false,
            },
            start..end,
        ))
    }

    fn parse_function_keyword(&mut self) -> NodeId {
        let start_tok = self.peek_sig().expect("caller checked");
        let start = start_tok.span.start;
        let kw_span = start_tok.span.clone();
        self.advance_exact();
        self.posix(
            kw_span,
            "the `function` keyword is a bash extension",
            "POSIX: write `name() { …; }`",
        );
        self.open.push(start_tok.span.clone());
        self.skip_trivia();
        let name = match self.take_literal_word(Expect::FunctionBody) {
            Some(w) => w,
            None => {
                self.open.pop();
                return self.empty_node(start..self.prev_end());
            }
        };
        self.skip_trivia();
        if self.is_operator(b"(") {
            let open_tok = self.peek_sig().unwrap();
            self.advance_exact();
            if self.is_operator(b")") && open_tok.span.end == self.peek_sig().unwrap().span.start {
                self.advance_exact();
            } else {
                let d = Diagnostic::error(
                    codes::UNEXPECTED,
                    open_tok.span.clone(),
                    "a function definition needs `()` with no space",
                )
                .with_help("write `function name() { …; }`");
                self.push_error(d);
                self.recover();
            }
        }
        self.skip_newlines();
        let body = match self.parse_compound_body() {
            Some(b) => b,
            None => {
                self.open.pop();
                return self.empty_node(start..self.prev_end());
            }
        };
        let body = self.parse_trailing_redirects(body);
        let end = self.prev_end();
        self.open.pop();
        self.new_node(
            NodeKind::Function {
                name,
                body,
                keyword: true,
            },
            start..end,
        )
    }

    fn parse_compound_body(&mut self) -> Option<NodeId> {
        let Some(t) = self.peek_sig() else {
            // End of input where the body belongs: report the body slot.
            self.record_expect_only(Expect::FunctionBody);
            return None;
        };
        let text = self.text_of(&t);
        match t.kind {
            TokenKind::Keyword => match text {
                b"if" => Some(self.parse_if()),
                b"for" => Some(self.parse_for()),
                b"while" => Some(self.parse_while(false)),
                b"until" => Some(self.parse_while(true)),
                b"case" => Some(self.parse_case()),
                b"[[" => Some(self.parse_test()),
                b"{" => None,
                _ => {
                    self.expected(Expect::FunctionBody, &t);
                    None
                }
            },
            TokenKind::Operator if text == b"{" => Some(self.parse_brace_group()),
            TokenKind::Word if text == b"{" => Some(self.parse_brace_group()),
            TokenKind::Operator if text == b"(" => Some(self.parse_subshell()),
            _ => {
                self.expected(Expect::FunctionBody, &t);
                None
            }
        }
    }

    fn parse_simple_command(&mut self) -> Option<NodeId> {
        let start = self.peek_sig().map_or(0, |t| t.span.start);
        let mut assignments: Vec<Assignment> = Vec::new();
        let mut words: Vec<Word> = Vec::new();
        let mut redirects: Vec<Redirect> = Vec::new();
        let mut array_args: Vec<ArrayArg> = Vec::new();
        while let Some(t) = self.peek_expect(Expect::Word) {
            match t.kind {
                TokenKind::Redirect => {
                    let redirect = self.parse_redirect(&t);
                    let target_missing = redirect.target.is_none();
                    redirects.push(redirect);
                    if target_missing && self.at_end() {
                        // The next word would be the target, not an argument.
                        break;
                    }
                }
                TokenKind::Word | TokenKind::WordUnterminated => {
                    let text = self.text_of(&t);
                    let _ = text;
                    if words.is_empty() {
                        if let Some(a) = self.try_spaced_assignment(&t) {
                            assignments.push(a);
                            continue;
                        }
                        if let Some(a) = self.try_assignment(&t) {
                            assignments.push(a);
                            continue;
                        }
                    } else if self.declaration_command(&words)
                        && t.kind == TokenKind::Word
                        && self.array_argument_follows(&t)
                    {
                        let (word, mut argument) = self.take_array_argument(&t);
                        argument.word = words.len();
                        array_args.push(argument);
                        words.push(word);
                        continue;
                    }
                    words.push(self.take_word_token(&t));
                }
                TokenKind::Keyword => {
                    let text = self.text_of(&t);
                    if matches!(text, b"time" | b"select") && words.is_empty() {
                        words.push(self.take_word_token(&t));
                        continue;
                    }
                    break;
                }
                TokenKind::ArithCommand => {
                    let d = Diagnostic::error(
                        codes::UNEXPECTED,
                        t.span.clone(),
                        "`((` starts an arithmetic command and cannot be an argument",
                    )
                    .with_help("`((` begins arithmetic in bash; write `( (` for a nested subshell");
                    self.push_error(d);
                    self.advance_exact();
                }
                TokenKind::Operator if self.text_of(&t) == b"(" => break,
                _ => break,
            }
        }
        if assignments.is_empty() && words.is_empty() && redirects.is_empty() {
            return None;
        }
        let end = self.prev_end().max(start);
        Some(self.new_node(
            NodeKind::Simple(SimpleCommand {
                assignments,
                words,
                redirects,
                array_args,
            }),
            start..end,
        ))
    }

    /// Whether the command word is one of the declarations bash lets take an
    /// array literal argument.
    fn declaration_command(&self, words: &[Word]) -> bool {
        let Some(first) = words.first() else {
            return false;
        };
        let src: &'a [u8] = self.src;
        matches!(
            &src[first.span.clone()],
            b"local" | b"declare" | b"typeset" | b"export" | b"readonly" | b"alias" | b"eval"
        )
    }

    /// `name=(` written as an argument: the `(` must be hard against the word.
    fn array_argument_follows(&mut self, t: &Token) -> bool {
        let src: &'a [u8] = self.src;
        if !src[t.span.clone()].ends_with(b"=") {
            return false;
        }
        let Some(open) = self.peek_sig_n(1) else {
            return false;
        };
        open.kind == TokenKind::Operator
            && self.text_of(&open) == b"("
            && open.span.start == t.span.end
    }

    fn take_array_argument(&mut self, t: &Token) -> (Word, ArrayArg) {
        let src: &'a [u8] = self.src;
        let raw = &src[t.span.clone()];
        let append = raw.ends_with(b"+=");
        let name_len = raw.len() - if append { 2 } else { 1 };
        let name = t.span.start..t.span.start + name_len;
        self.posix(
            t.span.clone(),
            "array assignment is a bash extension",
            "POSIX has no arrays; use a plain string",
        );
        self.advance_exact();
        self.skip_trivia();
        let open = self
            .peek_sig()
            .expect("caller verified the adjacent parenthesis");
        self.advance_exact();
        let mut elements: Vec<ArrayElement> = Vec::new();
        loop {
            self.skip_newlines();
            if self.at_end() {
                self.incomplete_at_expect(open.span.clone(), Expect::Operator);
                break;
            }
            if self.is_operator(b")") {
                self.advance_exact();
                break;
            }
            match self.take_array_element() {
                Some(element) => elements.push(element),
                None => {
                    self.unexpected_current();
                    self.advance_exact();
                    self.recover();
                }
            }
        }
        let end = self.prev_end();
        let span = t.span.start..end;
        // A quoted placeholder keeps ordinary expansion inert while
        // `SimpleCommand::array_args` carries the structure for the
        // declaration builtins.
        let placeholder = Lit::new(LitKind::Quoted, src[span.clone()].to_vec());
        let word = Word {
            parts: vec![Part::Lit(placeholder)],
            span: span.clone(),
        };
        (
            word,
            ArrayArg {
                word: 0,
                span,
                name,
                append,
                elements,
            },
        )
    }

    /// One element of an array literal: `[subscript]=value`,
    /// `[subscript]+=value`, or a bare `value`.
    ///
    /// The subscript is structural, not textual: `[i]=$v` assigns one scalar
    /// element while a bare `$v` is field-split, and that difference has to
    /// survive expansion.
    fn take_array_element(&mut self) -> Option<ArrayElement> {
        let t = self.peek_sig()?;
        if !matches!(
            t.kind,
            TokenKind::Word | TokenKind::WordUnterminated | TokenKind::Keyword
        ) {
            return None;
        }
        let src: &'a [u8] = self.src;
        let span = t.span.clone();
        let split = array_element_spans(src, span.clone());
        if t.kind == TokenKind::WordUnterminated {
            self.mark_unterminated(t.span.clone());
        }
        self.advance_exact();
        Some(match split {
            Some((subscript_span, append, value_span)) => ArrayElement {
                span,
                index: Some(ArrayIndex {
                    subscript: self.parse_word_span(subscript_span),
                    append,
                }),
                value: self.parse_word_span(value_span),
            },
            None => ArrayElement {
                span: span.clone(),
                index: None,
                value: self.parse_word_span(span),
            },
        })
    }

    fn take_word_token(&mut self, t: &Token) -> Word {
        if t.kind == TokenKind::WordUnterminated {
            self.mark_unterminated(t.span.clone());
        }
        self.advance_exact();
        self.parse_word_span(t.span.clone())
    }

    fn take_literal_word(&mut self, e: Expect) -> Option<Word> {
        let t = self.peek_expect(e)?;
        if !matches!(
            t.kind,
            TokenKind::Word | TokenKind::WordUnterminated | TokenKind::Keyword
        ) {
            self.expected(e, &t);
            self.advance_exact();
            return None;
        }
        Some(self.take_word_token(&t))
    }

    /// Inside a redirection target an unterminated quote is still a request
    /// for a file name, so report `RedirectTarget` rather than `Word`.
    fn record_unterminated_target(&mut self, tok: &Token) {
        let found = self
            .unterminated
            .iter()
            .rev()
            .find(|u| u.span.start >= tok.span.start && u.span.end <= tok.span.end)
            .cloned();
        let Some(u) = found else { return };
        let quote = matches!(
            u.kind,
            UnterminatedKind::SingleQuote
                | UnterminatedKind::DoubleQuote
                | UnterminatedKind::AnsiCQuote
        );
        if quote {
            self.record_site(Expect::RedirectTarget, tok.span.clone());
            self.incomplete_at(u.opener);
        }
    }

    fn mark_unterminated(&mut self, span: Span) {
        let found = self
            .unterminated
            .iter()
            .rev()
            .find(|u| u.span.start >= span.start && u.span.end <= span.end)
            .cloned();
        if let Some(u) = found {
            let expected = expect_for_unterminated(u.kind);
            self.record_site(expected, span);
            self.incomplete_at(u.opener);
        }
    }

    // ------------------------------------------------------------ redirects

    fn parse_redirect(&mut self, t: &Token) -> Redirect {
        self.advance_exact();
        let text = self.text_of(t);
        let mut k = 0usize;
        while k < text.len() && text[k].is_ascii_digit() {
            k += 1;
        }
        let fd = if k > 0 {
            std::str::from_utf8(&text[..k])
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
        } else {
            None
        };
        let rest = &text[k..];
        let (op, oplen) = match lexer::match_redirect_op(rest) {
            Some((op, len)) => (Some(op), len),
            None => (None, 1),
        };
        let op_span = (t.span.start + k)..(t.span.start + k + oplen);
        let target_in_token = k + oplen < text.len();
        let mut redirect = Redirect {
            span: t.span.clone(),
            fd,
            op: op.unwrap_or(RedirectOp::Read),
            target: None,
            heredoc: None,
        };
        let mut heredoc_missing = false;
        if let Some(opd) = op {
            // Here-documents are already fully scanned; attach the record.
            if opd.is_heredoc() {
                self.pending_heredocs += 1;
                let rec = self.lexed_heredocs.get(self.next_heredoc).cloned();
                if let Some(rec) = rec {
                    if rec.op.start == t.span.start {
                        self.next_heredoc += 1;
                        let terminated = rec.terminated;
                        let delimiter_missing = rec.delimiter.start == rec.delimiter.end;
                        let idx = self.sh.b.heredocs.len();
                        self.sh.b.heredocs.push(rec.clone());
                        redirect.heredoc = Some(idx);
                        if delimiter_missing {
                            heredoc_missing = true;
                            if self.at_end() {
                                self.incomplete_at_expect(
                                    op_span.clone(),
                                    Expect::HereDocDelimiter,
                                );
                            } else {
                                let d = Diagnostic::error(
                                    codes::HERE_DOC,
                                    t.span.clone(),
                                    "here-document has no delimiter word",
                                )
                                .with_help(
                                    "write `<<EOF` (or any word) and repeat it on a line of its own",
                                );
                                self.push_error(d);
                            }
                        } else if !terminated {
                            self.incomplete_at_expect(op_span.clone(), Expect::HereDocDelimiter);
                        }
                    }
                } else {
                    let d = Diagnostic::error(
                        codes::HERE_DOC,
                        t.span.clone(),
                        "here-document has no delimiter word",
                    );
                    self.push_error(d);
                }
            }
            match opd {
                RedirectOp::HereString => self.posix(
                    t.span.clone(),
                    "`<<<` is a bash extension",
                    "POSIX: write `<<EOF … EOF`",
                ),
                RedirectOp::Both | RedirectOp::BothAppend => self.posix(
                    t.span.clone(),
                    "`&>` is a bash extension",
                    "POSIX: write `>file 2>&1`",
                ),
                _ => {}
            }
        } else {
            let d = Diagnostic::error(
                codes::UNEXPECTED,
                t.span.clone(),
                "not a redirection operator",
            );
            self.push_error(d);
        }

        if target_in_token {
            let target_span = t.span.start + k + oplen..t.span.end;
            redirect.target = Some(self.parse_word_span(target_span));
        } else {
            // The target may be written separately.
            match self.peek_expect(Expect::RedirectTarget) {
                None => {
                    self.incomplete_at_expect(op_span.clone(), Expect::RedirectTarget);
                }
                Some(target_tok) => {
                    if matches!(
                        target_tok.kind,
                        TokenKind::Word | TokenKind::Keyword | TokenKind::WordUnterminated
                    ) {
                        if target_tok.kind == TokenKind::WordUnterminated {
                            self.record_unterminated_target(&target_tok);
                        }
                        redirect.target = Some(self.take_word_token(&target_tok));
                    } else if !heredoc_missing {
                        self.expected(Expect::RedirectTarget, &target_tok);
                    }
                }
            }
        }
        redirect.span = t.span.start..self.prev_end();
        redirect
    }

    // ---------------------------------------------------------- assignments

    fn try_assignment(&mut self, t: &Token) -> Option<Assignment> {
        // Validate first: a non-assignment word must not be consumed, and the
        // array-literal branch needs to look at the *next* token for `(`.
        if !assignment_span_ok(self.src, &t.span) {
            return None;
        }
        self.advance_exact();
        self.assignment_at(t.span.clone())
    }

    /// Recognise `name[expr]=value` whose subscript is spread over several
    /// tokens (`a[1 + 2]=v`). The lexer splits on white space, so the parser
    /// stitches the source span back together, exactly as bash's parser does.
    fn try_spaced_assignment(&mut self, t: &Token) -> Option<Assignment> {
        let src: &'a [u8] = self.src;
        let span = t.span.clone();
        // Only look ahead when this token cannot be a complete assignment.
        if find_assignment_eq(src, &span).is_some() {
            return None;
        }
        let mut i = span.start;
        if !src.get(i).is_some_and(|&byte| lexer::is_name_start(byte)) {
            return None;
        }
        i += 1;
        while i < src.len() && lexer::is_name(src[i]) {
            i += 1;
        }
        if src.get(i) != Some(&b'[') {
            return None;
        }
        let line_end = src[span.start..]
            .iter()
            .position(|&byte| byte == b'\n')
            .map_or(src.len(), |offset| span.start + offset);
        let close = matching_bracket(src, i, line_end)?;
        let mut after = close + 1;
        if src.get(after) == Some(&b'+') {
            after += 1;
        }
        if src.get(after) != Some(&b'=') {
            return None;
        }
        // The value is the rest of the token that contains `=`, if any.
        let mut value_end = after + 1;
        let mut probe = 0usize;
        while let Some(token) = self.peek_sig_n(probe) {
            if token.span.start <= after && after < token.span.end {
                value_end = value_end.max(token.span.end);
                break;
            }
            if token.span.start > after {
                break;
            }
            probe += 1;
        }
        let assignment = self.assignment_at(span.start..value_end)?;
        while self
            .peek_sig()
            .is_some_and(|next| next.span.start < value_end)
        {
            self.advance_exact();
        }
        Some(assignment)
    }

    /// Build an assignment from an explicit word span. The span may cover one
    /// token (`a[i]=v`) or several (`a[1 + 2]=v`).
    fn assignment_at(&mut self, span: Span) -> Option<Assignment> {
        let src: &'a [u8] = self.src;
        let eq = find_assignment_eq(src, &span)?;
        let mut name_end = eq;
        let mut append = false;
        if src.get(eq.wrapping_sub(1)) == Some(&b'+') {
            append = true;
            name_end -= 1;
        }
        let mut index_span = None;
        let mut plain_end = name_end;
        if let Some(open) = src[span.start..name_end].iter().position(|&b| b == b'[') {
            let open = span.start + open;
            let close = memrchr_byte(src, b']', open + 1, name_end)?;
            index_span = Some(open + 1..close);
            plain_end = open;
            let sub = &src[open + 1..close];
            if sub.windows(2).any(|w| w == b"$(") || sub.contains(&b'`') {
                let d = Diagnostic::error(
                    codes::ARRAY_SUBSCRIPT,
                    span.start..eq,
                    "a subscript containing a command substitution is refused",
                )
                .with_help("the subscript would be re-parsed as code; assign to a variable first");
                self.push_error(d);
            }
            self.posix(
                span.start..eq,
                "array assignment is a bash extension",
                "POSIX has no arrays; use a plain string",
            );
        }
        let name = span.start..plain_end;
        if name.start >= name.end || !is_valid_name(&src[name.clone()]) {
            return None;
        }
        let value_span = eq + 1..span.end;
        let has_index = index_span.is_some();
        let index = index_span.map(|index_span| self.parse_word_span(index_span));

        // `name=( … )`, with the `(` hard against the `=`.
        let array = if value_span.is_empty() {
            let adjacent_paren = self.peek_sig().is_some_and(|p| {
                p.kind == TokenKind::Operator
                    && self.text_of(&p) == b"("
                    && p.span.start == span.end
            });
            if adjacent_paren {
                if has_index {
                    let d = Diagnostic::error(
                        codes::ARRAY_SUBSCRIPT,
                        span.clone(),
                        "an array literal cannot be assigned to one element",
                    )
                    .with_help("assign a scalar, or drop the subscript to replace the array");
                    self.push_error(d);
                }
                self.advance_exact();
                let mut elems = Vec::new();
                loop {
                    self.skip_newlines();
                    if self.is_operator(b")") {
                        self.advance_exact();
                        break;
                    }
                    if self.at_end() {
                        self.incomplete_at(span.clone());
                        break;
                    }
                    match self.take_array_element() {
                        Some(element) => elems.push(element),
                        None => {
                            self.unexpected_current();
                            self.advance_exact();
                            self.recover();
                        }
                    }
                }
                self.posix(
                    span.clone(),
                    "array assignment is a bash extension",
                    "POSIX has no arrays; use a plain string",
                );
                Some(elems)
            } else {
                None
            }
        } else {
            None
        };

        let value = if array.is_some() {
            Word {
                span: value_span.clone(),
                parts: Vec::new(),
            }
        } else {
            self.parse_word_span(value_span)
        };
        Some(Assignment {
            span: span.clone(),
            name,
            append,
            index,
            value,
            array,
        })
    }

    // -------------------------------------------------------------- words

    /// Parse one word span into the shared `bsh-lang` word model.
    ///
    /// The front end interprets quoting before `bsh-lang` sees the word: the
    /// library never re-reads source. Single quotes are literal bytes, double
    /// quotes become a [`Part::DoubleQ`] group, a backslash-escaped byte
    /// becomes a [`LitKind::Quoted`] literal, and `$'…'` escapes are decoded
    /// here.
    fn parse_word_span(&mut self, span: Span) -> Word {
        let src: &'a [u8] = self.src;
        let end = span.end;
        let mut parts: Vec<Part> = Vec::new();
        let mut i = span.start;

        // A word-initial unquoted tilde is the only place tilde expansion is
        // permitted (`~`, `~/x`, `~user`, `~user/x`).
        if i < end && src[i] == b'~' {
            if let Some(after) = tilde_prefix_end(src, i, end) {
                parts.push(Part::Tilde(TildeSpec::new(src[i + 1..after].to_vec())));
                i = after;
            }
        }

        let mut run: Vec<u8> = Vec::new();
        while i < end {
            match src[i] {
                b'\'' => {
                    flush_run(&mut run, &mut parts);
                    let close = lexer::skip_squote(src, i + 1).unwrap_or(end);
                    let content_end = quoted_content_end(src, i + 1, close, b'\'');
                    parts.push(Part::single_quoted(src[i + 1..content_end].to_vec()));
                    i = close;
                }
                b'"' => {
                    flush_run(&mut run, &mut parts);
                    let mut inner: Vec<Part> = Vec::new();
                    let after = self.parse_dquote(&mut inner, i + 1, end);
                    parts.push(Part::double_quoted(inner));
                    i = after;
                }
                b'\\' => {
                    if i + 1 < end {
                        if src[i + 1] == b'\n' {
                            // A line continuation contributes nothing.
                            i += 2;
                        } else {
                            flush_run(&mut run, &mut parts);
                            parts.push(Part::quoted(vec![src[i + 1]]));
                            i += 2;
                        }
                    } else {
                        run.push(b'\\');
                        i += 1;
                    }
                }
                b'$' => {
                    flush_run(&mut run, &mut parts);
                    i = self.parse_dollar(&mut parts, i, end, false);
                }
                b'`' => {
                    flush_run(&mut run, &mut parts);
                    let close = lexer::skip_backtick(src, i + 1).unwrap_or(end);
                    let content_end = quoted_content_end(src, i + 1, close, b'`');
                    self.validate_sub(i + 1..content_end);
                    let source = unescape_backticks(&src[i + 1..content_end]);
                    let mut sub = CommandSub::backtick(source);
                    sub.span = i..close;
                    parts.push(Part::CmdSub(sub));
                    i = close;
                }
                b'<' | b'>' if i == span.start && src.get(i + 1) == Some(&b'(') => {
                    flush_run(&mut run, &mut parts);
                    let rp = lexer::region_end(src, i + 2, self.depth).unwrap_or(end);
                    let full_end = (rp + 1).min(end);
                    let content_end = rp.min(end);
                    self.validate_sub((i + 2)..content_end);
                    let sub = ProcessSub {
                        source: src[(i + 2).min(end)..content_end].to_vec(),
                        dir: if src[i] == b'<' {
                            ProcSubDir::Input
                        } else {
                            ProcSubDir::Output
                        },
                        span: i..full_end,
                    };
                    parts.push(Part::ProcSub(sub));
                    self.posix(
                        i..full_end,
                        "process substitution is a bash extension",
                        "POSIX has no `<(...)`; use a temporary file or a pipe",
                    );
                    i = full_end;
                }
                _ => {
                    run.push(src[i]);
                    i += 1;
                }
            }
        }
        flush_run(&mut run, &mut parts);
        Word { parts, span }
    }

    /// Parse the inside of a `"…"` group; `from` is just past the opening
    /// quote. Returns the position past the closing quote.
    fn parse_dquote(&mut self, parts: &mut Vec<Part>, from: usize, end: usize) -> usize {
        let src: &'a [u8] = self.src;
        let mut i = from;
        let mut run: Vec<u8> = Vec::new();
        while i < end {
            match src[i] {
                b'"' => {
                    flush_run(&mut run, parts);
                    return i + 1;
                }
                b'\\' => {
                    if i + 1 >= end {
                        run.push(b'\\');
                        i += 1;
                        continue;
                    }
                    let next = src[i + 1];
                    match next {
                        b'\n' => i += 2,
                        b'$' | b'`' | b'"' | b'\\' => {
                            flush_run(&mut run, parts);
                            parts.push(Part::quoted(vec![next]));
                            i += 2;
                        }
                        // Any other escaped byte keeps its backslash.
                        _ => {
                            run.push(b'\\');
                            run.push(next);
                            i += 2;
                        }
                    }
                }
                b'$' => {
                    flush_run(&mut run, parts);
                    i = self.parse_dollar(parts, i, end, true);
                }
                b'`' => {
                    flush_run(&mut run, parts);
                    let close = lexer::skip_backtick(src, i + 1).unwrap_or(end);
                    let content_end = quoted_content_end(src, i + 1, close, b'`');
                    self.validate_sub(i + 1..content_end);
                    let source = unescape_backticks(&src[i + 1..content_end]);
                    let mut sub = CommandSub::backtick(source);
                    sub.span = i..close;
                    parts.push(Part::CmdSub(sub));
                    i = close;
                }
                _ => {
                    run.push(src[i]);
                    i += 1;
                }
            }
        }
        flush_run(&mut run, parts);
        end
    }

    /// Parse one `$` construct, pushing the part(s) it produces.
    fn parse_dollar(
        &mut self,
        parts: &mut Vec<Part>,
        i: usize,
        end: usize,
        in_dquote: bool,
    ) -> usize {
        let src: &'a [u8] = self.src;
        match src.get(i + 1).copied() {
            Some(b'\'') if !in_dquote => {
                let close = lexer::skip_ansic(src, i + 2).unwrap_or(end);
                let content_end = quoted_content_end(src, i + 2, close, b'\'');
                parts.push(Part::ansi_c(decode_ansi_c(&src[i + 2..content_end])));
                self.posix(
                    i..close,
                    "`$'…'` quoting is a bash extension",
                    "POSIX: use single quotes, or `printf` for escapes",
                );
                close
            }
            Some(b'"') if !in_dquote => {
                let mut inner: Vec<Part> = Vec::new();
                let after = self.parse_dquote(&mut inner, i + 2, end);
                parts.push(Part::double_quoted(inner));
                after
            }
            Some(b'(') => {
                if src.get(i + 2) == Some(&b'(') {
                    match lexer::scan_arith(src, i + 3, self.depth, &mut Vec::new()) {
                        Some(close) => {
                            let expr_end = close.saturating_sub(2).max(i + 3);
                            let mut arith = Arithmetic::new(src[i + 3..expr_end].to_vec());
                            arith.span = i..close;
                            parts.push(Part::Arith(arith));
                            close
                        }
                        None => {
                            self.mark_open_here(i, UnterminatedKind::Arith);
                            push_literal(parts, &src[i..end]);
                            end
                        }
                    }
                } else {
                    match lexer::region_end(src, i + 2, self.depth) {
                        Some(rp) => {
                            let content_end = rp.min(end);
                            self.validate_sub((i + 2)..content_end);
                            let mut sub = CommandSub::dollar_paren(
                                src[(i + 2).min(end)..content_end].to_vec(),
                            );
                            sub.span = i..rp + 1;
                            parts.push(Part::CmdSub(sub));
                            rp + 1
                        }
                        None => {
                            self.mark_open_here(i, UnterminatedKind::CmdSub);
                            push_literal(parts, &src[i..end]);
                            end
                        }
                    }
                }
            }
            Some(b'{') => match lexer::skip_param(src, i + 2, self.depth, &mut Vec::new()) {
                Some(close) => {
                    let exp = self.parse_param_exp(i..close, (i + 2)..(close - 1));
                    parts.push(Part::Param(exp));
                    close
                }
                None => {
                    self.mark_open_here(i, UnterminatedKind::ParamExp);
                    push_literal(parts, &src[i..end]);
                    end
                }
            },
            Some(c) => {
                let (name, len) = unbraced_param_name(src, i + 1, end);
                match name {
                    Some(name) => {
                        let exp = ParamExp::value(name, i..(i + 1 + len));
                        parts.push(Part::Param(exp));
                        i + 1 + len
                    }
                    None => {
                        let _ = c;
                        push_literal(parts, b"$");
                        i + 1
                    }
                }
            }
            None => {
                push_literal(parts, b"$");
                end
            }
        }
    }

    /// Record that a substitution ran to end of input (defensive: the lexer
    /// normally catches this first and marks the word unterminated).
    fn mark_open_here(&mut self, opener: usize, kind: UnterminatedKind) {
        if self.sh.incomplete.is_none() {
            self.sh.incomplete = Some(Incomplete {
                open: opener..opener + 1,
                expected: vec![expect_for_unterminated(kind)],
            });
        }
    }

    /// Parse `${…}` into the shared [`ParamExp`] shape.
    ///
    /// This is the mapping `bsh-lang` depends on: unsupported operators must
    /// arrive as the matching [`UnsupportedOp`], never as a silently partial
    /// parse, and `${x:off:len}` offsets are pre-evaluated only when they are
    /// pure integer literals.
    fn parse_param_exp(&mut self, span: Span, inner: Span) -> ParamExp {
        let src: &'a [u8] = self.src;
        let text = &src[inner.clone()];
        let mut i = 0usize;

        // `${#}` is `$#`, not the length operator.
        if text == b"#" {
            return ParamExp::value(ParamName::Count, span);
        }

        let mut length = false;
        if text.first() == Some(&b'#') && text.len() > 1 {
            length = true;
            i = 1;
        }

        let mut indirect = false;
        if text.get(i) == Some(&b'!') {
            indirect = true;
            i += 1;
        }

        let name = match text.get(i).copied() {
            None => ParamName::Var(Vec::new()),
            Some(b'@') => {
                i += 1;
                ParamName::At
            }
            Some(b'*') => {
                i += 1;
                ParamName::Star
            }
            Some(b'#') => {
                i += 1;
                ParamName::Count
            }
            Some(b'?') | Some(b'-') | Some(b'$') | Some(b'!') => {
                let c = text[i];
                i += 1;
                ParamName::Var(vec![c])
            }
            Some(c) if c.is_ascii_digit() => {
                let start = i;
                while i < text.len() && text[i].is_ascii_digit() {
                    i += 1;
                }
                let n: usize = std::str::from_utf8(&text[start..i])
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                if n == 0 {
                    ParamName::Var(b"0".to_vec())
                } else {
                    ParamName::Positional(n)
                }
            }
            Some(c) if lexer::is_name_start(c) => {
                let start = i;
                i += 1;
                while i < text.len() && lexer::is_name(text[i]) {
                    i += 1;
                }
                ParamName::Var(text[start..i].to_vec())
            }
            Some(_) => ParamName::Var(Vec::new()),
        };

        // `${a[…]}`, `${#a[…]}`, `${!a[@]}`: the subscript is parsed as a
        // word because it is expanded before it is interpreted, and only then
        // arithmetically evaluated (indexed) or used as a key (associative).
        let mut name = name;
        if i < text.len() && text[i] == b'[' {
            let Some(close) = matching_bracket(text, i, text.len()) else {
                return ParamExp {
                    name,
                    op: ParamOp::Unsupported(UnsupportedOp::ArraySubscript),
                    span,
                };
            };
            let subscript = &text[i + 1..close];
            name = match name {
                ParamName::Var(base) if subscript == b"@" || subscript == b"*" => {
                    ParamName::elements(base, subscript == b"*")
                }
                ParamName::Var(base) => ParamName::element(
                    base,
                    self.parse_word_span(inner.start + i + 1..inner.start + close),
                ),
                other => {
                    // `${@[0]}` and friends are not a thing.
                    return ParamExp {
                        name: other,
                        op: ParamOp::Unsupported(UnsupportedOp::ArraySubscript),
                        span,
                    };
                }
            };
            i = close + 1;
            if text.get(i) == Some(&b'[') {
                let d = Diagnostic::error(
                    codes::ARRAY_SUBSCRIPT,
                    span.clone(),
                    "only one array subscript is allowed",
                );
                self.push_error(d);
                return ParamExp {
                    name,
                    op: ParamOp::Unsupported(UnsupportedOp::ArraySubscript),
                    span,
                };
            }
        }
        if length {
            if i < text.len() {
                // `${#a[0]/x/y}` is not `${#a[0]}` followed by an operator.
                let d = Diagnostic::error(
                    codes::ARRAY_SUBSCRIPT,
                    span.clone(),
                    "a length with an array subscript cannot take another operator",
                );
                self.push_error(d);
                return ParamExp {
                    name,
                    op: ParamOp::Unsupported(UnsupportedOp::Other),
                    span,
                };
            }
            return ParamExp {
                name,
                op: ParamOp::Length,
                span,
            };
        }
        if indirect {
            let op = if i >= text.len() {
                match &name {
                    ParamName::Elements { .. } => ParamOp::Indices,
                    ParamName::Element { .. } | ParamName::Var(_) => ParamOp::Indirect,
                    _ => ParamOp::Unsupported(UnsupportedOp::Indirect),
                }
            } else {
                ParamOp::Unsupported(UnsupportedOp::Indirect)
            };
            return ParamExp { name, op, span };
        }
        if i >= text.len() {
            return ParamExp {
                name,
                op: ParamOp::Value,
                span,
            };
        }

        let rest = &text[i..];
        let op_start = inner.start + i;
        let list_param = matches!(name, ParamName::At | ParamName::Star);
        let array_param = matches!(name, ParamName::Elements { .. });

        let op = if rest.starts_with(b":-") || rest.starts_with(b"-") {
            let colon = rest[0] == b':';
            let len = if colon { 2 } else { 1 };
            ParamOp::UseDefault {
                word: self.parse_word_span((op_start + len)..inner.end),
                colon,
            }
        } else if rest.starts_with(b":+") || rest.starts_with(b"+") {
            let colon = rest[0] == b':';
            let len = if colon { 2 } else { 1 };
            ParamOp::UseAlternate {
                word: self.parse_word_span((op_start + len)..inner.end),
                colon,
            }
        } else if rest.starts_with(b":=") || rest.starts_with(b"=") {
            let colon = rest[0] == b':';
            let len = if colon { 2 } else { 1 };
            ParamOp::AssignDefault {
                word: self.parse_word_span((op_start + len)..inner.end),
                colon,
            }
        } else if rest.starts_with(b":?") || rest.starts_with(b"?") {
            let colon = rest[0] == b':';
            let len = if colon { 2 } else { 1 };
            let word_span = (op_start + len)..inner.end;
            let word = if word_span.is_empty() {
                None
            } else {
                Some(self.parse_word_span(word_span))
            };
            ParamOp::Error { word, colon }
        } else if rest.starts_with(b"##") {
            ParamOp::RemovePrefix {
                pattern: self.parse_word_span((op_start + 2)..inner.end),
                longest: true,
            }
        } else if rest.starts_with(b"#") {
            ParamOp::RemovePrefix {
                pattern: self.parse_word_span((op_start + 1)..inner.end),
                longest: false,
            }
        } else if rest.starts_with(b"%%") {
            ParamOp::RemoveSuffix {
                pattern: self.parse_word_span((op_start + 2)..inner.end),
                longest: true,
            }
        } else if rest.starts_with(b"%") {
            ParamOp::RemoveSuffix {
                pattern: self.parse_word_span((op_start + 1)..inner.end),
                longest: false,
            }
        } else if rest.starts_with(b"/") {
            self.parse_replace_op(op_start, inner.end)
        } else if rest.starts_with(b"@") {
            // `${x@P}` and friends. A letter this crate does not know stays
            // unsupported rather than being guessed at: the error names the
            // operator, and no letter is silently ignored.
            match rest.get(1).copied().and_then(Transform::from_letter) {
                Some(transform) => ParamOp::Transform(transform),
                None => ParamOp::Unsupported(UnsupportedOp::Transform),
            }
        } else if rest.starts_with(b"^^") || rest.starts_with(b",,") {
            let how = if rest.starts_with(b"^^") {
                CaseHow::Upper
            } else {
                CaseHow::Lower
            };
            // Case conversion applies element-wise to `$@`/`a[@]`; the
            // expansion layer already vectorises it.
            self.parse_case_op(how, op_start + 2, inner.end)
        } else if rest.starts_with(b"^") || rest.starts_with(b",") {
            let how = if rest.starts_with(b"^") {
                CaseHow::UpperFirst
            } else {
                CaseHow::LowerFirst
            };
            self.parse_case_op(how, op_start + 1, inner.end)
        } else if rest.starts_with(b":") {
            // Array and positional-list slices evaluate their bounds
            // arithmetically, so the bounds stay words; a scalar slice is
            // pre-evaluated only when it is a pure integer literal.
            if list_param || array_param {
                self.array_slice_op(&src[(op_start + 1)..inner.end], op_start + 1)
            } else {
                substring_op(&src[(op_start + 1)..inner.end])
            }
        } else {
            ParamOp::Unsupported(UnsupportedOp::Other)
        };
        ParamExp { name, op, span }
    }

    /// `${x^pat}`, `${x^^pat}`, `${x,pat}`, `${x,,pat}`.
    ///
    /// The pattern is a word: `${x^^[a-c]}` matches a character class and
    /// `${x^^"$p"}` compares literally. With no pattern written, `pattern` is
    /// `None`, which bash treats as "every character".
    fn parse_case_op(&mut self, how: CaseHow, start: usize, end: usize) -> ParamOp {
        let pattern = if start < end {
            Some(self.parse_word_span(start..end))
        } else {
            None
        };
        ParamOp::Case { how, pattern }
    }

    /// `${x/pat/rep}`, `${x//pat/rep}`, `${x/#pat/rep}`, `${x/%pat/rep}`.
    ///
    /// The pattern ends at the first **unquoted** `/`, which is why the body is
    /// parsed as a word first: a `/` inside `'…'`, `"…"`, a backslash escape
    /// or a nested `${…}` (bash: `${v/${w/a/b}/X}`) is not a separator. Two
    /// bash quirks live here:
    ///
    /// * a single `/` followed by `#` or `%` anchors the match, and only an
    ///   unquoted one — `${x/'#'/y}` replaces a literal `#`;
    /// * the global form may not split at its first byte, so `${x////c}`
    ///   replaces `/` with `c` while `${x/'/'/c}` does the same with a quoted
    ///   pattern.
    fn parse_replace_op(&mut self, op_start: usize, end: usize) -> ParamOp {
        let src: &'a [u8] = self.src;
        let mut body_start = op_start + 1;
        let mut mode = ReplaceMode::First;
        match src.get(body_start) {
            Some(b'/') => {
                mode = ReplaceMode::All;
                body_start += 1;
            }
            Some(b'#') => {
                mode = ReplaceMode::Prefix;
                body_start += 1;
            }
            Some(b'%') => {
                mode = ReplaceMode::Suffix;
                body_start += 1;
            }
            _ => {}
        }
        let mut word = self.parse_word_span(body_start..end);
        if mode == ReplaceMode::First {
            if let Some(rest) = word.strip_leading_unquoted(b'#') {
                word = rest;
                mode = ReplaceMode::Prefix;
            } else if let Some(rest) = word.strip_leading_unquoted(b'%') {
                word = rest;
                mode = ReplaceMode::Suffix;
            }
        }
        let skip_first = mode == ReplaceMode::All && word.starts_with_unquoted(b'/');
        let (pattern, replacement) = match word.split_unquoted(b'/', skip_first) {
            Some((pattern, replacement)) => (pattern, replacement),
            None => (word, Word::new(Vec::new()).with_span(end..end)),
        };
        ParamOp::Replace {
            pattern,
            replacement,
            mode,
        }
    }

    /// Parse a substitution body only to validate and to collect its
    /// diagnostics. `bsh-lang` owns the source text and re-parses it when the
    /// runtime executes it, so the nodes are not retained here.
    fn validate_sub(&mut self, span: Span) {
        if span.start >= span.end {
            return;
        }
        if self.depth + 1 > self.opts.max_depth {
            let d = Diagnostic::error(codes::DEPTH, span, "nesting is too deep to parse")
                .with_help("simplify the command or raise the limit");
            self.push_error(d);
            return;
        }
        let src: &'a [u8] = self.src;
        let lexed = lexer::lex_full(&src[span.start..span.end]);
        let mut tokens = lexed.tokens;
        for t in &mut tokens {
            t.span.start += span.start;
            t.span.end += span.start;
        }
        let mut heredocs = lexed.heredocs;
        for h in &mut heredocs {
            shift_span(&mut h.op, span.start);
            shift_span(&mut h.delimiter, span.start);
            shift_span(&mut h.body, span.start);
        }
        let mut unterminated = lexed.unterminated;
        for u in &mut unterminated {
            shift_span(&mut u.span, span.start);
            shift_span(&mut u.opener, span.start);
        }
        let mut inner_sh = Shared::default();
        {
            let mut sub = Parser {
                src,
                toks: tokens,
                pos: 0,
                opts: self.opts,
                depth: self.depth + 1,
                sh: &mut inner_sh,
                expects: Vec::new(),
                open: Vec::new(),
                pending_heredocs: 0,
                next_heredoc: 0,
                lexed_heredocs: heredocs,
                unterminated,
            };
            let _ = sub.parse_list(Stop::Top);
        }
        for d in std::mem::take(&mut inner_sh.errors) {
            self.push_error(d);
        }
        if self.sh.incomplete.is_none() {
            if let Some(inc) = inner_sh.incomplete.take() {
                self.sh.incomplete = Some(inc);
                // The body's slot is not the outer cursor's slot.
                self.sh.incomplete_local = false;
            }
        }
    }

    // ------------------------------------------------------ compound forms

    fn parse_if(&mut self) -> NodeId {
        let start_tok = self.peek_sig().expect("caller checked");
        let start = start_tok.span.start;
        self.advance_exact();
        self.open.push(start_tok.span.clone());
        let mut branches: Vec<IfBranch> = Vec::new();
        let mut else_body = None;
        loop {
            let cond = self.parse_list(Stop::IfBody);
            self.skip_newlines();
            if self.at_end() {
                // `then` may follow only once the condition list has ended at
                // a separator; without one the list slot is still open.
                if self.list_ends_with_separator(cond) {
                    self.record_expect_only(Expect::Then);
                }
                break;
            }
            if !self.eat_keyword(b"then", Expect::Then) {
                self.label_last(start_tok.span.clone(), "`if` opened here");
                self.recover_to_keyword(&[b"fi", b"elif", b"else"]);
                break;
            }
            let body = self.parse_list(Stop::IfBody);
            branches.push(IfBranch { cond, body });
            self.skip_newlines();
            if self.is_keyword(b"elif") {
                self.advance_exact();
                continue;
            }
            if self.is_keyword(b"else") {
                self.advance_exact();
                else_body = Some(self.parse_list(Stop::IfBody));
                self.skip_newlines();
                break;
            }
            if self.is_keyword(b"fi") {
                break;
            }
            if self.at_end() {
                if self.list_ends_with_separator(body) {
                    self.record_expect_only(Expect::Fi);
                }
                break;
            }
            self.expected_current(Expect::Fi);
            self.label_last(start_tok.span.clone(), "`if` opened here");
            self.recover_to_keyword(&[b"fi"]);
            break;
        }
        if self.is_keyword(b"fi") {
            self.advance_exact();
        } else if !self.at_end() {
            self.expected_current(Expect::Fi);
            self.label_last(start_tok.span.clone(), "`if` opened here");
            self.recover_to_keyword(&[b"fi"]);
            if self.is_keyword(b"fi") {
                self.advance_exact();
            }
        }
        let end = self.prev_end();
        self.open.pop();
        self.new_node(
            NodeKind::If {
                branches,
                else_body,
            },
            start..end,
        )
    }

    fn eat_keyword(&mut self, word: &'static [u8], e: Expect) -> bool {
        match self.take_expect(e) {
            None => false,
            Some(t) => {
                if self.text_of(&t) != word {
                    self.expected(e, &t);
                    return false;
                }
                if t.kind == TokenKind::Keyword {
                    return true;
                }
                // The word is right but the lexer saw it as an ordinary word,
                // which means the separator before it is missing:
                // `if true then`, `for f do`. Accept it so that parsing
                // continues, but say exactly what is wrong.
                let d = Diagnostic::error(
                    codes::UNEXPECTED,
                    t.span.clone(),
                    format!(
                        "expected `;` or a newline before `{}`",
                        String::from_utf8_lossy(word)
                    ),
                )
                .with_help("a reserved word ends the preceding command list");
                self.push_error(d);
                true
            }
        }
    }

    fn parse_while(&mut self, until: bool) -> NodeId {
        let start_tok = self.peek_sig().expect("caller checked");
        let start = start_tok.span.start;
        self.advance_exact();
        self.open.push(start_tok.span.clone());
        let cond = self.parse_list(Stop::LoopBody);
        self.skip_newlines();
        if self.at_end() {
            // Nothing more can arrive. Report `do` when the condition ended at
            // a separator, and do not invent the body or `done` slot.
            if self.list_ends_with_separator(cond) {
                self.record_expect_only(Expect::Do);
            }
            let at = self.prev_end();
            let body = self.empty_node(at..at);
            let end = self.prev_end();
            self.open.pop();
            return self.new_node(NodeKind::While { until, cond, body }, start..end);
        }
        let have_do = self.eat_keyword(b"do", Expect::Do);
        if !have_do {
            self.label_last(start_tok.span.clone(), "loop opened here");
            self.recover_to_keyword(&[b"done"]);
        }
        let body = self.parse_list(Stop::LoopBody);
        self.skip_newlines();
        if self.at_end() {
            if self.list_ends_with_separator(body) {
                self.record_expect_only(Expect::Done);
            }
        } else if !self.eat_keyword(b"done", Expect::Done) {
            self.label_last(start_tok.span.clone(), "loop opened here");
            self.recover_to_keyword(&[b"done"]);
            if self.is_keyword(b"done") {
                self.advance_exact();
            }
        }
        let end = self.prev_end();
        self.open.pop();
        self.new_node(NodeKind::While { until, cond, body }, start..end)
    }

    fn parse_for(&mut self) -> NodeId {
        let start_tok = self.peek_sig().expect("caller checked");
        let start = start_tok.span.start;
        self.advance_exact();
        self.open.push(start_tok.span.clone());

        if self.peek_kind() == Some(TokenKind::ArithCommand) {
            let arith = self.peek_sig().unwrap();
            self.advance_exact();
            self.posix(
                arith.span.clone(),
                "`for (( … ))` is a bash extension",
                "POSIX: use a `while` loop with `$(( ))`",
            );
            self.skip_trivia();
            if self.is_operator(b";") {
                self.advance_exact();
            }
            self.skip_newlines();
            if self.at_end() {
                self.record_expect_only(Expect::Do);
                let at = self.prev_end();
                let body = self.empty_node(at..at);
                let end = self.prev_end();
                self.open.pop();
                let header = (arith.span.start + 2)..arith.span.end.saturating_sub(2);
                return self.new_node(NodeKind::ForArith { header, body }, start..end);
            }
            if !self.eat_keyword(b"do", Expect::Do) {
                self.recover_to_keyword(&[b"done"]);
            }
            let body = self.parse_list(Stop::LoopBody);
            self.skip_newlines();
            if self.at_end() {
                if self.list_ends_with_separator(body) {
                    self.record_expect_only(Expect::Done);
                }
            } else if !self.eat_keyword(b"done", Expect::Done) {
                self.recover_to_keyword(&[b"done"]);
                if self.is_keyword(b"done") {
                    self.advance_exact();
                }
            }
            let end = self.prev_end();
            self.open.pop();
            let header = (arith.span.start + 2)..arith.span.end.saturating_sub(2);
            return self.new_node(NodeKind::ForArith { header, body }, start..end);
        }

        let var = match self.take_literal_word(Expect::ForList) {
            Some(w) => w,
            None => {
                self.open.pop();
                return self.empty_node(start..self.prev_end());
            }
        };
        if !is_valid_name(self.text_of_span(&var.span)) {
            let d = Diagnostic::error(
                codes::UNEXPECTED,
                var.span.clone(),
                "the `for` variable must be a valid name",
            )
            .with_help("names start with a letter or `_` and contain only letters, digits and `_`");
            self.push_error(d);
        }

        self.skip_trivia();
        let mut in_list = None;
        let mut eof_in_list = false;
        if self.is_text_keyword(b"in") {
            self.advance_exact();
            let mut words = Vec::new();
            loop {
                self.skip_trivia();
                if self.at_end() {
                    self.record_expect_only(Expect::ForList);
                    eof_in_list = true;
                    break;
                }
                let is_end = match self.peek_sig() {
                    Some(t) => match t.kind {
                        TokenKind::Newline => true,
                        TokenKind::Operator => {
                            let text = self.text_of(&t);
                            text == b";" || text == b")"
                        }
                        TokenKind::Keyword => self.text_of(&t) == b"do",
                        _ => false,
                    },
                    None => true,
                };
                if is_end {
                    break;
                }
                match self.take_literal_word(Expect::ForList) {
                    Some(w) => words.push(w),
                    None => break,
                }
            }
            in_list = Some(words);
        }

        if eof_in_list {
            // The word list itself is still open: no separator has ended it,
            // so `do` here would be the word `do`, not the keyword.
            let at = self.prev_end();
            let body = self.empty_node(at..at);
            let end = self.prev_end();
            self.open.pop();
            return self.new_node(NodeKind::For { var, in_list, body }, start..end);
        }

        self.skip_trivia();
        let mut have_do = false;
        match self.peek_sig() {
            Some(t) if t.kind == TokenKind::Newline => {
                self.skip_newlines();
            }
            Some(t) if t.kind == TokenKind::Operator && self.text_of(&t) == b";" => {
                self.advance_exact();
                self.skip_newlines();
            }
            Some(t) if self.text_of(&t) == b"do" => {
                // `for x do` is valid when there is no `in` list (bash and
                // POSIX both accept it); with an `in` list the grammar needs
                // a separator first.
                if in_list.is_some() {
                    let d = Diagnostic::error(
                        codes::UNEXPECTED,
                        t.span.clone(),
                        "expected `;` or a newline before `do`",
                    )
                    .with_help("write `for x in …; do … done`");
                    self.push_error(d);
                    self.label_last(start_tok.span.clone(), "`for` opened here");
                }
                self.advance_exact();
                have_do = true;
            }
            None => {}
            Some(_) => {
                self.expected_current(Expect::Do);
                self.recover_to_keyword(&[b"do", b"done"]);
            }
        }

        if !have_do && self.at_end() {
            // The header ended at a separator (or there was no `in` list).
            if in_list.is_some() {
                self.record_expect_only(Expect::Do);
            } else {
                self.record_expect_only(Expect::In);
                self.record_expect_only(Expect::Do);
            }
        }
        if self.at_end() && !have_do {
            let at = self.prev_end();
            let body = self.empty_node(at..at);
            let end = self.prev_end();
            self.open.pop();
            return self.new_node(NodeKind::For { var, in_list, body }, start..end);
        }

        if !have_do && !self.eat_keyword(b"do", Expect::Do) {
            self.recover_to_keyword(&[b"done"]);
        }
        let body = self.parse_list(Stop::LoopBody);
        self.skip_newlines();
        if self.at_end() {
            if self.list_ends_with_separator(body) {
                self.record_expect_only(Expect::Done);
            }
        } else if !self.eat_keyword(b"done", Expect::Done) {
            self.recover_to_keyword(&[b"done"]);
            if self.is_keyword(b"done") {
                self.advance_exact();
            }
        }
        let end = self.prev_end();
        self.open.pop();
        self.new_node(NodeKind::For { var, in_list, body }, start..end)
    }

    fn text_of_span(&self, span: &Span) -> &'a [u8] {
        let src: &'a [u8] = self.src;
        &src[span.clone()]
    }

    fn parse_case(&mut self) -> NodeId {
        let start_tok = self.peek_sig().expect("caller checked");
        let start = start_tok.span.start;
        self.advance_exact();
        self.open.push(start_tok.span.clone());
        let word = match self.take_literal_word(Expect::Word) {
            Some(w) => w,
            None => {
                self.open.pop();
                return self.empty_node(start..self.prev_end());
            }
        };
        self.skip_newlines();
        let have_in = self.eat_text_keyword(b"in", Expect::In);
        if !have_in {
            self.label_last(start_tok.span.clone(), "`case` opened here");
            self.recover_to_keyword(&[b"esac"]);
        }
        self.skip_newlines();
        if !have_in && self.at_end() {
            // `in` itself is what is missing; a pattern slot would be a lie.
            let arms = Vec::new();
            let end = self.prev_end();
            self.open.pop();
            return self.new_node(NodeKind::Case { word, arms }, start..end);
        }
        let mut arms: Vec<CaseArm> = Vec::new();
        loop {
            self.skip_newlines();
            if self.at_end() {
                self.record_expect_only(Expect::CasePattern);
                break;
            }
            if self.is_text_keyword(b"esac") {
                break;
            }
            if self.is_operator(b"(") {
                self.advance_exact();
            }
            let mut patterns: Vec<Word> = Vec::new();
            loop {
                self.skip_trivia();
                match self.take_literal_word(Expect::CasePattern) {
                    Some(w) => patterns.push(w),
                    None => {
                        if !self.at_end() {
                            self.unexpected_current();
                            self.advance_exact();
                            self.recover();
                        }
                        break;
                    }
                }
                self.skip_trivia();
                if self.is_operator(b"|") {
                    self.advance_exact();
                    continue;
                }
                break;
            }
            if !self.eat_operator(b")") {
                if self.at_end() {
                    self.record_expect_only(Expect::Operator);
                } else {
                    self.expected_current(Expect::Operator);
                    self.recover();
                }
            }
            let body = self.parse_list(Stop::CaseArm);
            let term = self.take_case_term();
            let term = match term {
                Some(t) => t,
                None => {
                    arms.push(CaseArm {
                        patterns,
                        body,
                        term: CaseTerm::None,
                    });
                    break;
                }
            };
            arms.push(CaseArm {
                patterns,
                body,
                term,
            });
        }
        let term_allows_esac = arms.last().is_some_and(|arm| match arm.term {
            CaseTerm::None => self.list_allows_keyword(arm.body),
            _ => true,
        });
        if self.is_text_keyword(b"esac") {
            self.advance_exact();
        } else if self.at_end() {
            // `esac` closes a case that has at least one arm, once that arm is
            // at a boundary. With no arms the pattern slot stays primary.
            if !arms.is_empty() && term_allows_esac {
                self.record_expect_only(Expect::Esac);
            }
        } else {
            self.expected_current(Expect::Esac);
            self.recover_to_keyword(&[b"esac"]);
            if self.is_keyword(b"esac") {
                self.advance_exact();
            }
        }
        let end = self.prev_end();
        self.open.pop();
        self.new_node(NodeKind::Case { word, arms }, start..end)
    }

    fn take_case_term(&mut self) -> Option<CaseTerm> {
        self.skip_trivia();
        let t = self.toks.get(self.pos).cloned()?;
        if t.kind != TokenKind::Operator {
            return None;
        }
        let term = match self.text_of(&t) {
            b";;" => CaseTerm::Break,
            b";&" => CaseTerm::FallThrough,
            b";;&" => CaseTerm::Continue,
            _ => return None,
        };
        if term != CaseTerm::Break {
            self.posix(
                t.span.clone(),
                "`;&` and `;;&` are bash extensions",
                "POSIX `case` only has `;;`",
            );
        }
        self.advance_exact();
        Some(term)
    }

    fn parse_brace_group(&mut self) -> NodeId {
        let start_tok = self.peek_sig().expect("caller checked");
        let start = start_tok.span.start;
        self.advance_exact();
        self.open.push(start_tok.span.clone());
        let body = self.parse_list(Stop::Brace);
        let empty = match &self.sh.b.nodes[body as usize].kind {
            NodeKind::List { items } => items.is_empty(),
            _ => false,
        };
        self.skip_trivia();
        if self.is_operator(b"}") {
            if empty {
                let brace = self.peek_sig().unwrap();
                let d = Diagnostic::error(
                    codes::UNEXPECTED,
                    brace.span.clone(),
                    "`{ }` needs a command before the closing brace",
                )
                .with_help("write `{ :; }` for a do-nothing group");
                self.push_error(d);
                self.label_last(start_tok.span.clone(), "`{` opened here");
            }
            self.advance_exact();
        } else if self.at_end() {
            // `}` is a reserved word only at a list boundary: `{ echo ` needs
            // a word, not a brace.
            if self.list_ends_with_separator(body) {
                self.record_expect_only(Expect::Operator);
            }
        } else {
            self.expected_current(Expect::Operator);
            self.recover();
        }
        let end = self.prev_end();
        self.open.pop();
        let node = self.new_node(NodeKind::BraceGroup { body }, start..end);
        self.parse_trailing_redirects(node)
    }

    fn parse_subshell(&mut self) -> NodeId {
        let start_tok = self.peek_sig().expect("caller checked");
        let start = start_tok.span.start;
        self.advance_exact();
        self.open.push(start_tok.span.clone());
        let body = self.parse_list(Stop::Paren);
        let empty = match &self.sh.b.nodes[body as usize].kind {
            NodeKind::List { items } => items.is_empty(),
            _ => false,
        };
        self.skip_trivia();
        if self.is_operator(b")") {
            if empty {
                let close = self.peek_sig().unwrap();
                let d = Diagnostic::error(
                    codes::UNEXPECTED,
                    close.span.clone(),
                    "`()` needs a command",
                )
                .with_help("write `( : )` for a subshell that does nothing");
                self.push_error(d);
            }
            self.advance_exact();
        } else if self.at_end() {
            if self.list_ends_with_separator(body) {
                self.record_expect_only(Expect::Operator);
            }
        } else {
            self.expected_current(Expect::Operator);
            self.recover();
        }
        let end = self.prev_end();
        self.open.pop();
        let node = self.new_node(NodeKind::Subshell { body }, start..end);
        self.parse_trailing_redirects(node)
    }

    fn parse_arith_command(&mut self) -> NodeId {
        let t = self.peek_sig().expect("caller checked");
        self.advance_exact();
        let open = t.span.start;
        let expr = (open + 2)..t.span.end.saturating_sub(2);
        let arith_open = self
            .unterminated
            .iter()
            .find(|u| u.opener.start == open && u.kind == UnterminatedKind::ArithCommand)
            .map(|u| u.opener.clone());
        if let Some(arith_open) = arith_open {
            self.record_site(Expect::ArithExpr, t.span.clone());
            self.incomplete_at(arith_open);
        }
        self.posix(
            t.span.clone(),
            "`((` starts an arithmetic command in bash",
            "POSIX: write `( (` for a nested subshell",
        );
        let node = self.new_node(NodeKind::ArithCommand { expr }, t.span.clone());
        self.parse_trailing_redirects(node)
    }

    fn parse_test(&mut self) -> NodeId {
        let start_tok = self.peek_sig().expect("caller checked");
        let start = start_tok.span.start;
        self.advance_exact();
        self.posix(
            start_tok.span.clone(),
            "`[[ … ]]` is a bash conditional",
            "POSIX: use `[` … `]` or `test`",
        );
        self.open.push(start_tok.span.clone());
        let body_start = self.prev_end();
        let mut body_end = body_start;
        // The position of the last `[[ … ]]` operator seen. Reporting it lets
        // a consumer offer operator completions without counting unbalanced
        // `[[` tokens, and tells it which operator the cursor follows.
        let mut last_op: Option<Span> = None;
        loop {
            self.skip_trivia();
            let Some(t) = self.toks.get(self.pos).cloned() else {
                // Inside an unclosed `[[ … ]]` the cursor is in the operator
                // slot; in POSIX mode the construct is rejected instead.
                let expect = if self.opts.posix_mode {
                    Expect::Operator
                } else {
                    Expect::TestOp
                };
                let at = self.src.len();
                let span = last_op.clone().unwrap_or(at..at);
                self.record_site(expect, span);
                self.incomplete_at(start_tok.span.clone());
                break;
            };
            if t.kind == TokenKind::Newline {
                self.advance_exact();
                continue;
            }
            let text = self.text_of(&t);
            if text == b"]]" {
                body_end = t.span.start;
                self.advance_exact();
                break;
            }
            if is_test_operator(text) {
                last_op = Some(t.span.clone());
            }
            body_end = t.span.end;
            self.advance_exact();
        }
        let end = self.prev_end();
        self.open.pop();
        self.new_node(
            NodeKind::TestCommand {
                body: body_start..body_end,
            },
            start..end,
        )
    }

    fn parse_trailing_redirects(&mut self, body: NodeId) -> NodeId {
        let mut redirects: Vec<Redirect> = Vec::new();
        loop {
            self.skip_trivia();
            let Some(t) = self.peek_sig() else {
                break;
            };
            if t.kind != TokenKind::Redirect {
                break;
            }
            redirects.push(self.parse_redirect(&t));
        }
        if redirects.is_empty() {
            return body;
        }
        let start = self.sh.b.nodes[body as usize].span.start;
        let end = self.prev_end();
        self.new_node(NodeKind::Redirected { body, redirects }, start..end)
    }
}

/// Append accumulated unquoted literal bytes as one part.
fn flush_run(run: &mut Vec<u8>, parts: &mut Vec<Part>) {
    if run.is_empty() {
        return;
    }
    parts.push(Part::Lit(Lit::new(LitKind::Unquoted, std::mem::take(run))));
}

/// Append literal bytes as an unquoted literal.
fn push_literal(parts: &mut Vec<Part>, bytes: &[u8]) {
    if !bytes.is_empty() {
        parts.push(Part::Lit(Lit::new(LitKind::Unquoted, bytes.to_vec())));
    }
}

/// The end of a quoted construct's *content*: `close` is just past the
/// closing delimiter when the construct was terminated, and end-of-input
/// otherwise.
fn quoted_content_end(src: &[u8], from: usize, close: usize, quote: u8) -> usize {
    if close > from && src.get(close - 1) == Some(&quote) {
        close - 1
    } else {
        close
    }
}

/// Where a word-initial tilde prefix ends, if this is one.
///
/// `~`, `~/…`, `~user` and `~user/…` qualify; `~$x`, `~"…"` and a `~` in the
/// middle of a word do not.
fn tilde_prefix_end(src: &[u8], start: usize, end: usize) -> Option<usize> {
    let mut i = start + 1;
    while i < end {
        let b = src[i];
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-') {
            i += 1;
        } else {
            break;
        }
    }
    if i < end && src[i] != b'/' {
        return None;
    }
    Some(i)
}

/// Map `$` + one token to a parameter name, returning the bytes consumed.
fn unbraced_param_name(src: &[u8], from: usize, end: usize) -> (Option<ParamName>, usize) {
    let Some(&c) = src.get(from) else {
        return (None, 0);
    };
    if c.is_ascii_digit() {
        let name = if c == b'0' {
            ParamName::Var(b"0".to_vec())
        } else {
            ParamName::Positional((c - b'0') as usize)
        };
        return (Some(name), 1);
    }
    if lexer::is_name_start(c) {
        let mut i = from + 1;
        while i < end && lexer::is_name(src[i]) {
            i += 1;
        }
        return (Some(ParamName::Var(src[from..i].to_vec())), i - from);
    }
    let name = match c {
        b'@' => ParamName::At,
        b'*' => ParamName::Star,
        b'#' => ParamName::Count,
        b'?' | b'-' | b'$' | b'!' => ParamName::Var(vec![c]),
        _ => return (None, 0),
    };
    (Some(name), 1)
}

/// Pre-evaluate `${x:off:len}` only for pure integer literals; anything else
/// is reported as unsupported rather than guessed (`bsh-lang` does not parse
/// arithmetic there).
fn substring_op(text: &[u8]) -> ParamOp {
    let (offset_text, length_text) = match text.iter().position(|&b| b == b':') {
        Some(i) => (&text[..i], Some(&text[i + 1..])),
        None => (text, None),
    };
    let offset = if is_blank(offset_text) {
        // `${x::2}` means offset 0, but `${x:}` is a bad substitution.
        if length_text.is_none() {
            return ParamOp::Unsupported(UnsupportedOp::Other);
        }
        0
    } else {
        match parse_int(offset_text) {
            Some(n) => n,
            None => return ParamOp::Unsupported(UnsupportedOp::ArithmeticSubstring),
        }
    };
    let length = match length_text {
        None => None,
        Some(t) if is_blank(t) => Some(0),
        Some(t) => match parse_int(t) {
            Some(n) => Some(n),
            None => return ParamOp::Unsupported(UnsupportedOp::ArithmeticSubstring),
        },
    };
    ParamOp::Substring { offset, length }
}

/// `${a[@]:offset:length}` / `${@:offset:length}`: the bounds stay words
/// because bash evaluates them arithmetically when the expansion runs.
impl Parser<'_, '_> {
    /// `${a[@]:offset:length}` / `${@:offset:length}`: the bounds stay words
    /// because bash evaluates them arithmetically when the expansion runs.
    fn array_slice_op(&mut self, text: &[u8], start: usize) -> ParamOp {
        let (offset_text, length_text) = match split_top_level_colon(text) {
            Some(index) => (&text[..index], Some(&text[index + 1..])),
            None => (text, None),
        };
        let offset_end = start + offset_text.len();
        ParamOp::ArraySlice {
            offset: self.parse_word_span(start..offset_end),
            length: length_text
                .map(|text| self.parse_word_span(offset_end + 1..offset_end + 1 + text.len())),
        }
    }
}

/// Split at the first `:` that is not inside brackets or quotes.
fn split_top_level_colon(text: &[u8]) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote: Option<u8> = None;
    for (index, &byte) in text.iter().enumerate() {
        match quote {
            Some(q) => {
                if byte == q {
                    quote = None;
                }
            }
            None => match byte {
                b'\'' | b'"' => quote = Some(byte),
                b'[' | b'(' => depth += 1,
                b']' | b')' => depth = depth.saturating_sub(1),
                b':' if depth == 0 => return Some(index),
                _ => {}
            },
        }
    }
    None
}

fn is_blank(text: &[u8]) -> bool {
    text.iter().all(|b| matches!(b, b' ' | b'\t' | b'\n'))
}

fn parse_int(text: &[u8]) -> Option<i64> {
    let mut start = 0;
    let mut end = text.len();
    while start < end && matches!(text[start], b' ' | b'\t' | b'\n') {
        start += 1;
    }
    while end > start && matches!(text[end - 1], b' ' | b'\t' | b'\n') {
        end -= 1;
    }
    let text = &text[start..end];
    let (negative, digits) = match text.first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    if digits.is_empty() || !digits.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut value: i64 = 0;
    for &b in digits {
        value = value.checked_mul(10)?.checked_add((b - b'0') as i64)?;
    }
    Some(if negative { -value } else { value })
}

/// Remove the extra backslash escape layer of a legacy `` `…` `` substitution.
fn unescape_backticks(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            match bytes.get(i + 1) {
                Some(b'$') | Some(b'`') | Some(b'\\') => {
                    out.push(bytes[i + 1]);
                    i += 2;
                }
                Some(b'\n') => i += 2,
                _ => {
                    out.push(b'\\');
                    i += 1;
                }
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// Decode `$'…'` escapes. `bsh-lang` does not re-parse them.
fn decode_ansi_c(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let Some(&esc) = bytes.get(i + 1) else {
            out.push(b'\\');
            break;
        };
        i += 2;
        match esc {
            b'a' => out.push(0x07),
            b'b' => out.push(0x08),
            b'e' | b'E' => out.push(0x1b),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(0x0b),
            b'\\' => out.push(b'\\'),
            b'\'' => out.push(b'\''),
            b'"' => out.push(b'"'),
            b'?' => out.push(b'?'),
            b'\n' => {}
            b'x' => {
                let mut value: u32 = 0;
                let mut digits = 0;
                while digits < 2 {
                    match bytes.get(i).and_then(|b| hex_value(*b)) {
                        Some(v) => {
                            value = value * 16 + v as u32;
                            i += 1;
                            digits += 1;
                        }
                        None => break,
                    }
                }
                if digits == 0 {
                    out.push(b'\\');
                    out.push(b'x');
                } else {
                    out.push(value as u8);
                }
            }
            b'u' | b'U' => {
                let max = if esc == b'u' { 4 } else { 8 };
                let mut value: u32 = 0;
                let mut digits = 0;
                while digits < max {
                    match bytes.get(i).and_then(|b| hex_value(*b)) {
                        Some(v) => {
                            value = value.saturating_mul(16).saturating_add(v as u32);
                            i += 1;
                            digits += 1;
                        }
                        None => break,
                    }
                }
                if digits == 0 {
                    out.push(b'\\');
                    out.push(esc);
                } else if let Some(ch) = char::from_u32(value) {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
            }
            b'0'..=b'7' => {
                let mut value: u32 = (esc - b'0') as u32;
                let mut digits = 1;
                while digits < 3 {
                    match bytes.get(i) {
                        Some(b) if (b'0'..=b'7').contains(b) => {
                            value = value * 8 + (b - b'0') as u32;
                            i += 1;
                            digits += 1;
                        }
                        _ => break,
                    }
                }
                out.push(value as u8);
            }
            b'c' => match bytes.get(i) {
                Some(&b) => {
                    i += 1;
                    let upper = b.to_ascii_uppercase();
                    out.push(upper & 0x1f);
                }
                None => {
                    out.push(b'\\');
                    out.push(b'c');
                }
            },
            other => {
                out.push(b'\\');
                out.push(other);
            }
        }
    }
    out
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn shift_span(span: &mut Span, by: usize) {
    span.start += by;
    span.end += by;
}

/// The operators of a `[[ … ]]` conditional, used only to locate the operator
/// slot for completion. Evaluation belongs to the language layer.
fn is_test_operator(text: &[u8]) -> bool {
    matches!(
        text,
        b"=="
            | b"="
            | b"!="
            | b"=~"
            | b"<"
            | b">"
            | b"!"
            | b"&&"
            | b"||"
            | b"("
            | b")"
            | b"-a"
            | b"-b"
            | b"-c"
            | b"-d"
            | b"-e"
            | b"-ef"
            | b"-eq"
            | b"-f"
            | b"-g"
            | b"-G"
            | b"-ge"
            | b"-gt"
            | b"-h"
            | b"-k"
            | b"-le"
            | b"-lt"
            | b"-L"
            | b"-n"
            | b"-N"
            | b"-ne"
            | b"-nt"
            | b"-o"
            | b"-O"
            | b"-ot"
            | b"-p"
            | b"-r"
            | b"-R"
            | b"-s"
            | b"-S"
            | b"-t"
            | b"-u"
            | b"-v"
            | b"-w"
            | b"-x"
            | b"-z"
    )
}

/// The tokens the parser may look past freely. Newlines are deliberately not
/// included: a newline is a command separator, not layout.
fn is_ignorable(kind: TokenKind) -> bool {
    matches!(kind, TokenKind::Whitespace | TokenKind::Comment)
}

fn is_reserved_stop(text: &[u8]) -> bool {
    matches!(
        text,
        b"then" | b"elif" | b"else" | b"fi" | b"do" | b"done" | b"esac"
    )
}

/// Whether a word span has the shape of an assignment, without building one.
///
/// Kept separate from [`Parser::assignment_at`] so the caller can decide
/// whether to consume the token first: `name=(…)` needs the following `(`,
/// while a plain command word must not be consumed by a failed attempt.
fn assignment_span_ok(src: &[u8], span: &Span) -> bool {
    let Some(eq) = find_assignment_eq(src, span) else {
        return false;
    };
    let mut name_end = eq;
    if src.get(eq.wrapping_sub(1)) == Some(&b'+') {
        name_end = name_end.saturating_sub(1);
    }
    if let Some(open) = src[span.start..name_end].iter().position(|&b| b == b'[') {
        let open = span.start + open;
        if memrchr_byte(src, b']', open + 1, name_end).is_none() {
            return false;
        }
        name_end = open;
    }
    name_end > span.start && is_valid_name(&src[span.start..name_end])
}

fn memrchr_byte(src: &[u8], needle: u8, from: usize, to: usize) -> Option<usize> {
    if to <= from {
        return None;
    }
    src[from..to]
        .iter()
        .rposition(|&b| b == needle)
        .map(|p| p + from)
}

fn show_token(text: &[u8], kind: TokenKind) -> String {
    match kind {
        TokenKind::Newline => "newline".to_string(),
        _ if text.is_empty() => "end of file".to_string(),
        _ => {
            let s = String::from_utf8_lossy(text);
            let shown: String = if s.chars().count() > 24 {
                let mut t: String = s.chars().take(21).collect();
                t.push('…');
                t
            } else {
                s.into_owned()
            };
            format!("`{shown}`")
        }
    }
}

fn orphan_help(text: &[u8], _kind: TokenKind) -> Option<String> {
    // Text-based: a quoted `'fi'` is not the reserved word, and its raw text
    // still includes the quotes, so this cannot misfire on an argument.
    let help = match text {
        b"fi" => "there is no `if` waiting for `fi`",
        b"done" => "there is no loop waiting for `done`",
        b"esac" => "there is no `case` waiting for `esac`",
        b"then" => "there is no `if` waiting for `then`",
        b"elif" => "there is no `if` waiting for `elif`",
        b"else" => "there is no `if` waiting for `else`",
        b"do" => "there is no loop waiting for `do`",
        b"in" => "`in` is only valid in a `for` or `case`",
        b")" => "there is no `(` waiting for `)`",
        b"}" => "there is no `{` waiting for `}`",
        b"|" => "a pipeline cannot start with `|`",
        b"|&" => "a pipeline cannot start with `|&`",
        b"&&" => "a pipeline cannot start with `&&`",
        b"||" => "a pipeline cannot start with `||`",
        b";;" => "`;;` only separates `case` arms",
        b";&" => "`;&` only separates `case` arms",
        b";;&" => "`;;&` only separates `case` arms",
        b";" => "there is no command before `;`",
        _ => return None,
    };
    Some(help.to_string())
}

/// Split an array-literal element into `([subscript]=|+=[)value`.
///
/// Returns the subscript's byte span (inside the brackets), whether the
/// element was written with `+=`, and the value's span. `None` means the
/// element is a bare value. Quote-aware, because `['a]b']=v` is one key.
fn array_element_spans(src: &[u8], span: Span) -> Option<(Span, bool, Span)> {
    if span.start >= span.end || src[span.start] != b'[' {
        return None;
    }
    let close = matching_bracket(src, span.start, span.end)?;
    let mut after = close + 1;
    let append = src.get(after) == Some(&b'+');
    if append {
        after += 1;
    }
    if src.get(after) != Some(&b'=') {
        return None;
    }
    let value_start = after + 1;
    Some((
        span.start + 1..close,
        append,
        value_start..span.end.max(value_start),
    ))
}

/// The index of the `]` matching the `[` at `open`, skipping quoted text and
/// nested brackets.
fn matching_bracket(src: &[u8], open: usize, end: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote: Option<u8> = None;
    let mut index = open;
    while index < end {
        let byte = src[index];
        match quote {
            Some(q) => {
                if byte == q {
                    quote = None;
                } else if byte == b'\\' && q == b'"' {
                    index += 1;
                }
            }
            None => match byte {
                b'\'' | b'"' => quote = Some(byte),
                b'\\' => index += 1,
                b'[' => depth += 1,
                b']' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(index);
                    }
                }
                _ => {}
            },
        }
        index += 1;
    }
    None
}
