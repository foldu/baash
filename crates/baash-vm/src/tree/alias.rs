//! Alias substitution: bash's textual rewrite at command position.
//!
//! An alias is *not* a function call. `alias t1='echo hi|wc -c'` makes `t1`
//! stand for those tokens, so the substitution has to happen on the *text*
//! before the command is parsed — a function call could not produce a
//! pipeline, and a word-level rewrite could not produce `for`/`{`/`(`. That is
//! why this module re-lexes the command and re-parses the result, and why the
//! alias table itself lives in `baash-runtime` (state) while the substitution
//! lives here (the parser's front door).
//!
//! The rules, all verified against bash 5.3.20:
//!
//! * **Only command positions are eligible**, plus the word right after an
//!   expansion whose value ended in a blank (`alias hi='echo hi '` makes the
//!   *next* word eligible too).
//! * **An alias never expands into itself.** The guard is a stack of the names
//!   being substituted, so `alias echo='echo foo'` runs `echo foo` and stops.
//! * **The rewritten text is fully expanded.** [`Interp::expand_aliases`]
//!   walks the whole replacement, including nested values, so evaluation of the
//!   result must not expand again; re-entering is what would make
//!   `alias echo='echo foo'` loop.
//! * **A definition takes effect for the next command, not the current line
//!   fragment.** Expansion happens per command at execution time, so
//!   `alias e=echo; e hi` on one line *does* expand — a deliberate, documented
//!   deviation from bash's line-buffered parser, and the same one `baash-vm`
//!   already makes for parsing in general.

use std::sync::Arc;

use baash_diag::SourceFile;
use baash_runtime::{ShellHost, Status};
use baash_syntax::{ParseOutcome, TokenKind};

use super::{Ctx, Interp, ParsedChunk, Unwind};

/// One substituted word: where it was, and what name produced it.
struct Found {
    start: usize,
    end: usize,
    name: Vec<u8>,
}

impl Interp {
    /// Substitute aliases in the command spanning `span` and run the result.
    ///
    /// `None` means "no alias at this command position" — the common case, and
    /// the one that must stay cheap: with no aliases defined this is a single
    /// map lookup.
    pub(crate) fn exec_alias_command(
        &mut self,
        chunk: &Arc<ParsedChunk>,
        span: &baash_diag::Span,
        ctx: Ctx,
    ) -> Option<Unwind> {
        if !self.state.opts.expand_aliases
            || self.state.aliases.is_empty()
            || self.alias_eval_depth > 0
            || span.is_empty()
        {
            return None;
        }
        let text = chunk.source.bytes()[span.clone()].to_vec();
        let mut stack = Vec::new();
        let expanded = self.expand_aliases(&text, &mut stack)?;

        let source = Arc::new(SourceFile::new("<alias>", &expanded[..]));
        let opts = self.parse_opts();
        let ast = match baash_syntax::parse_with(&source, &opts) {
            ParseOutcome::Ok(ast) => ast,
            outcome => {
                // A syntax error introduced by the substitution is reported
                // against the rewritten text, which is where the fault is.
                let previous = std::mem::replace(&mut self.current, source.clone());
                match outcome {
                    ParseOutcome::Failed(diagnostics) => {
                        for diagnostic in &diagnostics {
                            self.diagnose(diagnostic);
                        }
                    }
                    ParseOutcome::NeedMoreInput { open, .. } => {
                        self.diagnose(&baash_diag::Diagnostic::error(
                            baash_syntax::codes::NEED_MORE_INPUT,
                            open,
                            "unexpected end of file: the alias expansion is incomplete",
                        ));
                    }
                    ParseOutcome::Ok(_) => unreachable!("handled above"),
                }
                self.current = previous;
                return Some(Unwind::Status(Status(2)));
            }
        };

        let rewritten = Arc::new(ParsedChunk { ast, source });
        let previous = std::mem::replace(&mut self.current, rewritten.source.clone());
        // The replacement is already fully expanded; a second pass would
        // expand the value's own first word against the alias that produced
        // it, which is exactly the recursion the stack forbids.
        self.alias_eval_depth += 1;
        let unwind = self.eval_node(&rewritten, rewritten.ast.root, ctx);
        self.alias_eval_depth -= 1;
        self.current = previous;
        Some(unwind)
    }

    /// Rewrite `text` until no eligible alias remains.
    ///
    /// Returns `None` when nothing was substituted, so the caller can skip the
    /// re-parse entirely.
    fn expand_aliases(&self, text: &[u8], stack: &mut Vec<Vec<u8>>) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(text.len());
        let mut changed = false;
        let mut rest = text;
        // Eligibility does not restart at a splice: the replacement was
        // expanded on its own, and what follows it depends on how the
        // replacement ended.
        let mut at_command = true;
        let mut next_eligible = false;
        while !rest.is_empty() {
            let Some(found) = self.find_alias(rest, at_command, next_eligible, stack) else {
                out.extend_from_slice(rest);
                break;
            };
            out.extend_from_slice(&rest[..found.start]);
            let value = self
                .state
                .aliases
                .get(&found.name)
                .map(<[u8]>::to_vec)
                .unwrap_or_default();
            stack.push(found.name);
            let expanded_value = self.expand_aliases(&value, stack).unwrap_or(value);
            stack.pop();
            at_command = ends_at_command_position(&expanded_value);
            next_eligible = expanded_value
                .last()
                .is_some_and(|byte| *byte == b' ' || *byte == b'\t');
            out.extend_from_slice(&expanded_value);
            changed = true;
            rest = &rest[found.end..];
        }
        changed.then_some(out)
    }

    /// The first eligible alias name in `text`, if there is one.
    ///
    /// Eligibility is bash's: command position, or the word after an expansion
    /// that ended in a blank. `at_command` and `next_eligible` are the state
    /// carried in from a preceding splice.
    fn find_alias(
        &self,
        text: &[u8],
        at_command: bool,
        next_eligible: bool,
        stack: &[Vec<u8>],
    ) -> Option<Found> {
        let mut at_command = at_command;
        let mut next_eligible = next_eligible;
        let mut expect_target = false;
        for token in baash_syntax::lex(text) {
            match token.kind {
                TokenKind::Whitespace | TokenKind::Comment => {}
                TokenKind::Newline | TokenKind::Operator => {
                    at_command = true;
                    next_eligible = false;
                    expect_target = false;
                }
                TokenKind::Redirect => {
                    // A target written hard against the operator is inside the
                    // token (`>out`); otherwise the next word is the target,
                    // and it is not a command word.
                    expect_target = token.redirect_target(text).is_none();
                    next_eligible = false;
                }
                TokenKind::Keyword => {
                    // The lexer only calls a word a keyword in command
                    // position, and every reserved word that can appear there
                    // is either followed by a command (`if`, `do`, `then`) or
                    // ends one. Treating it as still-command-position is the
                    // safe reading, and it is what makes `do hi` expand in a
                    // loop body.
                    next_eligible = false;
                    expect_target = false;
                }
                TokenKind::Word | TokenKind::WordUnterminated => {
                    if expect_target {
                        expect_target = false;
                        next_eligible = false;
                        continue;
                    }
                    if at_command || next_eligible {
                        let word = token.text(text);
                        if let Some(name) = plain_word(word)
                            && self.state.aliases.get(&name).is_some()
                            && !stack.contains(&name)
                        {
                            return Some(Found {
                                start: token.span.start,
                                end: token.span.end,
                                name,
                            });
                        }
                    }
                    next_eligible = false;
                    at_command = at_command && token.assignment_value_start(text).is_some();
                }
                TokenKind::ArithCommand | TokenKind::HereDocBody | TokenKind::HereDocDelimiter => {
                    at_command = false;
                    next_eligible = false;
                    expect_target = false;
                }
            }
        }
        None
    }
}

/// The bytes of a word that could name an alias.
///
/// An alias name is unquoted, unexpanded text: `'hi'`, `$x` and `hi=x` are
/// never alias references, and `alias --` is a usage error rather than a
/// substitution. Anything that a later expansion phase would transform, or that
/// shell syntax would interpret, disqualifies the word.
fn plain_word(word: &[u8]) -> Option<Vec<u8>> {
    if word.is_empty() {
        return None;
    }
    for (index, &byte) in word.iter().enumerate() {
        let allowed = byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'_' | b'+' | b'.' | b'@' | b'%' | b'^' | b',' | b':' | b'-' | b'/'
            )
            // A tilde is literal after the first byte; at the start it is tilde
            // expansion, so the word is not a name.
            || (byte == b'~' && index > 0);
        if !allowed {
            return None;
        }
    }
    Some(word.to_vec())
}

/// Whether text ending in `value` leaves the next word in a command position.
fn ends_at_command_position(value: &[u8]) -> bool {
    let trimmed = value.trim_ascii_end();
    matches!(
        trimmed.last(),
        Some(b';' | b'&' | b'|' | b'\n' | b'{' | b'(' | b')')
    )
}
