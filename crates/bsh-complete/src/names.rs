//! Variable, history, function, alias and builtin providers.
//!
//! Two of the six provider sources are defined here as traits rather than as
//! concrete data, because their data lives above this crate: the variable store
//! belongs to `bsh-runtime` and the history ring to `bsh-history`, and
//! `bsh-complete` must not depend on either (`DESIGN.md` §4.1). The binary
//! implements [`VarNames`], [`History`] and [`ShellNames`] over those and hands
//! them in as `Arc`s.

use std::sync::Arc;

use bsh_lang::QuoteCtx;
use bsh_syntax::{Expect, TokenKind, lex_full};

use crate::candidate::{CandKind, Candidate};
use crate::context::Ctx;
use crate::sources::{self, Provider, ProviderId};

/// The names of every variable the shell knows.
///
/// Only names are needed: completion never reads a value, so a provider cannot
/// accidentally trigger an expansion or expose a secret through a menu.
pub trait VarNames: Send + Sync {
    /// Every variable name, without the `$`.
    fn names(&self) -> Vec<Vec<u8>>;
}

/// The shell's history, most recent entry first.
pub trait History: Send + Sync {
    /// Raw history lines, most recent first.
    fn entries(&self) -> Vec<Vec<u8>>;
}

/// Shell functions and aliases.
///
/// Split from [`VarNames`] because a runtime may have variables without
/// functions (a POSIX profile) or functions without an interactive history.
pub trait ShellNames: Send + Sync {
    /// Function names.
    fn functions(&self) -> Vec<Vec<u8>> {
        Vec::new()
    }

    /// Alias names.
    fn aliases(&self) -> Vec<Vec<u8>> {
        Vec::new()
    }
}

/// Variables and parameter names.
pub struct VarProvider {
    names: Arc<dyn VarNames>,
}

impl VarProvider {
    /// Wrap a variable-name source.
    pub fn new(names: Arc<dyn VarNames>) -> Self {
        Self { names }
    }

    /// Every variable name, without the `$`.
    ///
    /// `complete -A variable` inserts the bare name, which is the opposite of
    /// what the interactive `$…` completion does, so the engine needs the list
    /// itself and not this provider's decorated candidates.
    pub fn names(&self) -> Vec<Vec<u8>> {
        self.names.names()
    }
}

impl Provider for VarProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Variable
    }

    fn applies(&self, ctx: &Ctx) -> bool {
        if ctx.quote == QuoteCtx::SingleQuoted || ctx.heredoc {
            return false;
        }
        ctx.arith
            || (ctx.expect == Expect::ParamOp && ctx.param)
            || ctx.expect == Expect::ForList
            || ctx.prefix.starts_with(b"$")
    }

    fn candidates(&self, ctx: &Ctx) -> Vec<Candidate> {
        let names = self.names.names();
        let bare = ctx.param || ctx.arith;
        names
            .into_iter()
            .map(|name| {
                if ctx.param {
                    // Inside `${…}` the insertion point is already past the
                    // `$` and `{`. Bash closes the expansion when the name is
                    // complete (`${HO<TAB>` → `${HOME}`), which is also the
                    // only way the inserted word re-parses.
                    let display = format!("${{{}}}", String::from_utf8_lossy(&name));
                    let mut insert = name;
                    insert.push(b'}');
                    Candidate::source(insert, CandKind::Variable)
                        .with_desc("variable")
                        .with_display(display)
                        .appending_space()
                } else if bare {
                    // Inside `$((…))` arithmetic takes the name without a `$`,
                    // and there is no brace to close.
                    Candidate::source(name, CandKind::Variable).with_desc("variable")
                } else {
                    let mut insert = Vec::with_capacity(name.len() + 1);
                    insert.push(b'$');
                    insert.extend_from_slice(&name);
                    Candidate::source(insert, CandKind::Variable)
                        .with_desc("variable")
                        .appending_space()
                }
            })
            .collect()
    }
}

/// Functions, aliases and builtins: the non-`PATH` half of command position.
pub struct FunctionProvider {
    names: Arc<dyn ShellNames>,
}

impl FunctionProvider {
    /// Wrap a function/alias source.
    pub fn new(names: Arc<dyn ShellNames>) -> Self {
        Self { names }
    }
}

impl Provider for FunctionProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Function
    }

    fn applies(&self, ctx: &Ctx) -> bool {
        sources::named_command_slot(ctx)
    }

    fn candidates(&self, _ctx: &Ctx) -> Vec<Candidate> {
        self.names
            .functions()
            .into_iter()
            .map(|name| Candidate::new(name, CandKind::Function).with_desc("function"))
            .collect()
    }
}

/// Aliases.
pub struct AliasProvider {
    names: Arc<dyn ShellNames>,
}

impl AliasProvider {
    /// Wrap a function/alias source.
    pub fn new(names: Arc<dyn ShellNames>) -> Self {
        Self { names }
    }
}

impl Provider for AliasProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Alias
    }

    fn applies(&self, ctx: &Ctx) -> bool {
        sources::named_command_slot(ctx)
    }

    fn candidates(&self, _ctx: &Ctx) -> Vec<Candidate> {
        self.names
            .aliases()
            .into_iter()
            .map(|name| Candidate::new(name, CandKind::Alias).with_desc("alias"))
            .collect()
    }
}

/// Builtins, from [`sources::BUILTINS`].
#[derive(Clone, Copy, Debug, Default)]
pub struct BuiltinProvider;

impl Provider for BuiltinProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Builtin
    }

    fn applies(&self, ctx: &Ctx) -> bool {
        sources::named_command_slot(ctx)
    }

    fn candidates(&self, _ctx: &Ctx) -> Vec<Candidate> {
        // Literal, not source: a builtin name is a command word like any other,
        // and a literal can be inserted inside an open quote. The only name
        // that gains an escape is `[` (as `\[`, which still runs `[`).
        sources::BUILTINS
            .iter()
            .map(|name| Candidate::new(*name, CandKind::Builtin).with_desc("builtin"))
            .collect()
    }
}

/// The most history entries consulted for one completion.
const HISTORY_ENTRY_LIMIT: usize = 2_000;

/// Words from history, weighted by recency.
pub struct HistoryProvider {
    history: Arc<dyn History>,
}

impl HistoryProvider {
    /// Wrap a history source.
    pub fn new(history: Arc<dyn History>) -> Self {
        Self { history }
    }
}

impl Provider for HistoryProvider {
    fn id(&self) -> ProviderId {
        ProviderId::History
    }

    fn applies(&self, ctx: &Ctx) -> bool {
        ctx.quote == QuoteCtx::Unquoted
            && !ctx.param
            && matches!(ctx.expect, Expect::Command | Expect::ForList)
    }

    fn candidates(&self, ctx: &Ctx) -> Vec<Candidate> {
        let mut candidates = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (rank, entry) in self
            .history
            .entries()
            .into_iter()
            .take(HISTORY_ENTRY_LIMIT)
            .enumerate()
        {
            let frecency = 1.0 / (1.0 + rank as f32);
            let words = history_words(&entry, ctx.expect);
            for word in words {
                if word.is_empty() || !seen.insert(word.clone()) {
                    continue;
                }
                candidates.push(
                    Candidate::source(word, CandKind::History)
                        .with_desc("history")
                        .with_frecency(frecency),
                );
            }
        }
        candidates
    }
}

/// The words of a history line that are useful for `expect`.
fn history_words(entry: &[u8], expect: Expect) -> Vec<Vec<u8>> {
    let lexed = lex_full(entry);
    let mut words = Vec::new();
    for token in &lexed.tokens {
        if token.kind.is_trivia() {
            continue;
        }
        let text = token.text(entry);
        match token.kind {
            TokenKind::Word | TokenKind::WordUnterminated | TokenKind::Keyword => {
                words.push(text.to_vec());
                if expect == Expect::Command {
                    // Command position wants the command word only; the rest
                    // of the line is not a completion candidate for it.
                    break;
                }
            }
            _ => continue,
        }
    }
    words
}
