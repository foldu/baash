//! Where the cursor is: the word, the quote context, and the grammar's
//! expectation.
//!
//! Everything here comes from the front end. [`baash_syntax::lex_full`] supplies
//! the token spans, [`baash_syntax::open_constructs_at`] the nesting stack of
//! open quotes and substitutions at the cursor, and
//! [`baash_syntax::expectations_at`] the production the cursor is in. The
//! completer never splits a line on whitespace, never guesses a word boundary,
//! and never re-enters a substitution body by hand.
//!
//! # How the context is derived
//!
//! 1. Ask the lexer for the chain of open constructs at the cursor, innermost
//!    first. `echo "$(ls ` yields `[CmdSub, DoubleQuote]`, so the innermost
//!    entry names the shell-code region the cursor is in and its
//!    `content_start` says where that region begins — no per-kind opener
//!    widths are encoded here.
//! 2. Lex that region and take the word token under the cursor. A redirection
//!    token carries its target when no space separates them;
//!    [`baash_syntax::Token::redirect_op_len`] splits them.
//! 3. Ask [`expectations_at`] at the *cursor*. The parser answers for the slot
//!    the cursor is in, including `[[ -` → `TestOp` and `echo > fi` →
//!    `RedirectTarget`, and reports the innermost open construct for a cursor
//!    inside a quote or substitution.
//! 4. Classify the region's tokens into simple-command roles
//!    ([`baash_syntax::classify_tokens`]) to find the command word and the
//!    argument index; the roles come from the grammar, not from a keyword
//!    table here.
//!
//! # Fallbacks that remain
//!
//! * A word containing *closed* quotes before the cursor (for example
//!   `"a b"c`) is completed as one opaque word: the whole word is replaced and
//!   the raw text (quotes included) is matched. The parser exposes only open
//!   constructs at the cursor, which is exactly what insertion needs; quoting
//!   *inside* a closed prefix of the same word is not tracked.
//! * A reserved word that the lexer cannot classify (the `in` of a `for` list
//!   is lexed as a plain word because it does not sit in command position) is
//!   not a command boundary. The expectation already selects the right
//!   providers for that slot, so the command word found there is unused.

use baash_lang::QuoteCtx;
use baash_syntax::{
    Expect, ParseOpts, Token, TokenKind, TokenRole, UnterminatedKind, classify_tokens,
    expectations_at, lex_full, open_constructs_at,
};

use crate::candidate::Span;

/// Where in the grammar the cursor is, and what has been typed there.
///
/// Built by [`context`]; every provider reads it, and the editor receives the
/// parts it needs through [`crate::Menu`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ctx {
    /// The expectation the completer acts on: the innermost slot the cursor is
    /// in, i.e. the last entry of [`Ctx::expects`].
    pub expect: Expect,
    /// Every expectation [`expectations_at`] reported, most specific first.
    ///
    /// A compound command's own keyword comes before the generic command slot
    /// (`if true; ` → `[Then, Command]`), so the first entry is the headline
    /// and the last is the slot the cursor is actually in.
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
    /// The whole line for top-level code; the content of the innermost
    /// substitution when the cursor is inside one.
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
    let chain = open_constructs_at(line, cursor);
    let innermost = chain.first();

    // The shell-code region the cursor is in: the content of the innermost
    // substitution, or the whole line.
    let code_start = chain
        .iter()
        .find(|open| {
            matches!(
                open.kind,
                UnterminatedKind::CmdSub | UnterminatedKind::Backtick | UnterminatedKind::ProcSub
            )
        })
        .map_or(0, |open| open.content_start);
    let in_construct = innermost.is_some();
    // Only top-level plain code can have text after the cursor; an open
    // construct extends to the end of the line by definition.
    let future = !in_construct && code_start == 0;

    // What the grammar expects at the cursor. The parser answers for the
    // cursor position itself, so a half-typed command word reports `Command`
    // and a half-typed redirect target reports `RedirectTarget`.
    let expects = expectations_at(line, cursor, opts);

    let word_tokens = lex_region(line, code_start, if future { line.len() } else { cursor });
    let hit = word_hit(&word_tokens, line, cursor);

    let (word, mut redirect, assign_start) = match &hit {
        Some(hit) => (hit.span.clone(), hit.redirect, hit.assignment_value),
        None => (cursor..cursor, false, None),
    };
    redirect = redirect || expects.contains(&Expect::RedirectTarget);

    let mut replace_start = word.start;
    if let Some(open) = innermost {
        match open.kind {
            UnterminatedKind::SingleQuote
            | UnterminatedKind::DoubleQuote
            | UnterminatedKind::AnsiCQuote
            | UnterminatedKind::ParamExp
            | UnterminatedKind::Arith
            | UnterminatedKind::ArithCommand => replace_start = open.content_start,
            _ => {}
        }
    }
    // A quotation cannot begin after the cursor, but clamp anyway so that a
    // malformed line can never produce a reversed span.
    replace_start = replace_start.min(cursor);

    // The command the cursor is an argument of comes from the tokens *before*
    // this word.
    let empty_word = word.start == cursor && word.end == cursor;
    let up_to = if empty_word { cursor } else { word.start };
    let command_tokens: Vec<Token> = word_tokens
        .iter()
        .take_while(|token| token.span.end <= up_to)
        .cloned()
        .collect();
    let (command_name, arg_index) = command_info(&command_tokens, line);

    // `FOO=/us<TAB>` completes the value, not the assignment name.
    let mut assign = false;
    if !in_construct && !redirect && arg_index == 0 && command_name.is_none() {
        if let Some(value) = assign_start {
            if value <= cursor {
                replace_start = value;
                assign = value > word.start;
            }
        }
    }

    let mut expect = expects.last().copied().unwrap_or(Expect::Command);
    let quote = quote_context(&chain);
    let param = innermost.is_some_and(|open| open.kind == UnterminatedKind::ParamExp);
    let arith = innermost.is_some_and(|open| {
        matches!(
            open.kind,
            UnterminatedKind::Arith | UnterminatedKind::ArithCommand
        )
    });
    let heredoc = innermost.is_some_and(|open| open.kind == UnterminatedKind::HereDoc);
    if quote != QuoteCtx::Unquoted && !param && !arith {
        expect = Expect::Word;
    }
    if param {
        expect = Expect::ParamOp;
    }
    if arith {
        expect = Expect::ArithExpr;
    }
    if heredoc {
        expect = Expect::HereDocDelimiter;
    }
    // Inside a redirection target's quotes a file name is still what is being
    // completed; inside a substitution in the target, the command slot wins.
    if redirect && expect == Expect::Word {
        expect = Expect::RedirectTarget;
    }
    if assign {
        expect = Expect::Word;
    }

    let prefix = line[replace_start..cursor].to_vec();

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
        param,
        arith,
        heredoc,
        assign,
        code: code_start..cursor,
    }
}

/// The quote context at the cursor.
///
/// A substitution starts a fresh quoting context, so a quote *outside* the
/// innermost substitution does not apply: in `echo "$(ls ` the body is
/// unquoted even though the word is inside `"…"`.
fn quote_context(chain: &[baash_syntax::OpenConstruct]) -> QuoteCtx {
    for open in chain {
        match open.kind {
            UnterminatedKind::CmdSub | UnterminatedKind::Backtick | UnterminatedKind::ProcSub => {
                return QuoteCtx::Unquoted;
            }
            UnterminatedKind::DoubleQuote => return QuoteCtx::DoubleQuoted,
            UnterminatedKind::SingleQuote | UnterminatedKind::AnsiCQuote => {
                return QuoteCtx::SingleQuoted;
            }
            _ => {}
        }
    }
    QuoteCtx::Unquoted
}

/// Lex `line[start..end]`, returning tokens with absolute spans.
fn lex_region(line: &[u8], start: usize, end: usize) -> Vec<Token> {
    let end = end.min(line.len());
    let start = start.min(end);
    let mut tokens = lex_full(&line[start..end]).tokens;
    for token in &mut tokens {
        token.span.start += start;
        token.span.end += start;
    }
    tokens
}

/// A word token under the cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
struct WordHit {
    span: Span,
    redirect: bool,
    /// The offset just past the `=` when the token is a `NAME=value` word.
    assignment_value: Option<usize>,
}

/// Find the word token the cursor is completing.
fn word_hit(tokens: &[Token], line: &[u8], cursor: usize) -> Option<WordHit> {
    // A token that begins exactly at the cursor wins: the cursor is at the
    // start of a word that already has text after it.
    if let Some(token) = tokens
        .iter()
        .find(|token| token.span.start == cursor && !token.kind.is_trivia())
    {
        return hit_of(token, line);
    }
    // Otherwise a token that strictly contains the cursor.
    if let Some(token) = tokens.iter().find(|token| {
        token.span.start < cursor && cursor < token.span.end && !token.kind.is_trivia()
    }) {
        return hit_of(token, line);
    }
    // A token ending at the cursor, but only when nothing (not even
    // whitespace) starts there: at `echo ` the cursor is at a boundary and a
    // new word begins, while at the end of `echo` it is inside the word.
    if tokens.iter().any(|token| token.span.start == cursor) {
        return None;
    }
    let token = tokens
        .iter()
        .rev()
        .find(|token| token.span.end == cursor && !token.kind.is_trivia())?;
    hit_of(token, line)
}

fn hit_of(token: &Token, line: &[u8]) -> Option<WordHit> {
    match token.kind {
        TokenKind::Word | TokenKind::WordUnterminated | TokenKind::Keyword => Some(WordHit {
            span: token.span.clone(),
            redirect: false,
            assignment_value: token.assignment_value_start(line),
        }),
        TokenKind::Redirect => {
            // The operator and a target written hard against it share one
            // token; the lexer owns the split.
            let op = token.redirect_op_len(line)?;
            Some(WordHit {
                span: token.span.start + op..token.span.end,
                redirect: true,
                assignment_value: None,
            })
        }
        _ => None,
    }
}

/// The command word and argument index for the tokens before the completion
/// word.
fn command_info(tokens: &[Token], line: &[u8]) -> (Option<Vec<u8>>, usize) {
    let roles = classify_tokens(tokens, line);
    let start = roles
        .iter()
        .rposition(|role| *role == TokenRole::Boundary)
        .map_or(0, |index| index + 1);

    let mut command: Option<Vec<u8>> = None;
    let mut arguments = 0usize;
    for (role, token) in roles[start..].iter().zip(&tokens[start..]) {
        match role {
            TokenRole::CommandWord => command = Some(token.text(line).to_vec()),
            TokenRole::Argument => arguments += 1,
            _ => {}
        }
    }
    let arg_index = if command.is_some() { arguments + 1 } else { 0 };
    (command, arg_index)
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
    fn redirect_op_lengths_come_from_the_lexer() {
        // The split lives in baash-syntax (gap 4): the completer must not carry
        // its own operator table.
        use baash_syntax::redirect_op_len;
        assert_eq!(redirect_op_len(b">"), Some(1));
        assert_eq!(redirect_op_len(b">>"), Some(2));
        assert_eq!(redirect_op_len(b"2>&1"), Some(3));
        assert_eq!(redirect_op_len(b"&>>"), Some(3));
        assert_eq!(redirect_op_len(b"<<-"), Some(3));
        assert_eq!(redirect_op_len(b"<<<"), Some(3));
        assert_eq!(redirect_op_len(b"<fi"), Some(1));
        assert_eq!(redirect_op_len(b">out"), Some(1));
        assert_eq!(redirect_op_len(b"fi"), None);
    }

    // ------------------------------------------------ parser API, no fallbacks

    #[test]
    fn test_operator_comes_from_the_parser() {
        use baash_syntax::expectations_at;
        // Gap 2: the parser reports `TestOp` inside `[[ … ]]`; the completer no
        // longer counts unbalanced `[[` tokens.
        assert!(
            expectations_at(b"[[ x == ", 9, &ParseOpts::DEFAULT).contains(&Expect::TestOp),
            "the parser must report the operator slot"
        );
        let c = ctx_at_end("[[ x == ");
        assert_eq!(c.expect, Expect::TestOp);
        assert_eq!(c.prefix, b"");
    }

    #[test]
    fn redirect_target_comes_from_the_parser() {
        // Gap 3: a present target is still a target.
        let c = ctx_at_end("echo > fi");
        assert_eq!(c.expect, Expect::RedirectTarget);
        assert!(c.redirect);
        assert!(c.expects.contains(&Expect::RedirectTarget));

        let fused = ctx_at_end("echo 2>&1");
        assert_eq!(fused.expect, Expect::RedirectTarget);
        assert_eq!(fused.word, 8..9);
        assert_eq!(fused.replace, 8..9);
        assert_eq!(fused.prefix, b"1");
    }

    #[test]
    fn nested_constructs_need_no_region_reentry() {
        // Gap 5: the lexer's nesting stack names the region; the completer does
        // not re-enter substitution bodies by hand.
        let c = ctx_at_end("echo \"$(ls ");
        assert_eq!(c.expect, Expect::Word);
        assert_eq!(c.code.start, 8);
        assert_eq!(c.quote, QuoteCtx::Unquoted);
        assert_eq!(c.command_name.as_deref(), Some(&b"ls"[..]));
        assert_eq!(c.arg_index, 1);

        let c = ctx_at_end("echo \"$(l");
        assert_eq!(c.expect, Expect::Command);
        assert_eq!(c.code.start, 8);
        assert_eq!(c.prefix, b"l");

        // `${` inside `"…"` keeps the quote context for insertion.
        let c = ctx_at_end("echo \"${x");
        assert_eq!(c.expect, Expect::ParamOp);
        assert_eq!(c.quote, QuoteCtx::DoubleQuoted);
        assert_eq!(c.replace, 8..9);
    }

    #[test]
    fn closing_keywords_are_reported_alongside_the_slot() {
        // Gap 7: the parser now reports the compound's own keyword. The
        // completer keeps the innermost list slot as the primary expectation,
        // so command completion still works inside the body.
        let c = ctx_at_end("if true; ");
        assert!(c.expects.contains(&Expect::Then));
        assert_eq!(c.expect, Expect::Command);

        let c = ctx_at_end("if true; then echo; ");
        assert!(c.expects.contains(&Expect::Fi));
        assert_eq!(c.expect, Expect::Command);

        let c = ctx_at_end("while true; ");
        assert!(c.expects.contains(&Expect::Do));

        let c = ctx_at_end("for x in a; ");
        assert_eq!(c.expect, Expect::Do);

        let c = ctx_at_end("case x ");
        assert_eq!(c.expect, Expect::In);
    }

    #[test]
    fn unterminated_content_start_drives_the_replacement() {
        // Gap 6: no per-kind opener widths in the completer.
        let c = ctx_at_end("echo $'ab");
        assert_eq!(c.replace, 7..9);
        assert_eq!(c.prefix, b"ab");

        let c = ctx_at_end("echo ${x");
        assert_eq!(c.replace, 7..8);

        let c = ctx_at_end("echo $((x");
        assert_eq!(c.replace, 8..9);
    }

    #[test]
    fn prefix_lossy_is_available_for_display() {
        let c = ctx_at_end("git ch");
        assert_eq!(c.prefix_lossy(), "ch");
    }
}
