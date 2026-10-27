//! Static providers: parameter operators, `[[ … ]]` operators, keywords and
//! `case` patterns.
//!
//! These are pure data (`sources::PARAM_OPS`, `sources::TEST_OPS`,
//! `sources::keywords_for`) behind the same [`Provider`] trait as the
//! filesystem-backed sources, so the engine treats every source identically and
//! the editor can replace one without special cases.

use bsh_lang::QuoteCtx;
use bsh_syntax::Expect;

use crate::candidate::{CandKind, Candidate};
use crate::context::Ctx;
use crate::sources::{self, Provider, ProviderId};

/// Parameter-expansion operators: `${x<TAB>` → `:-`, `#`, `//`, …
///
/// The completion is inserted *after* the `${` because [`Ctx::replace`] starts
/// there, so an operator is offered as the name already typed plus the
/// operator: `${x` → `x:-`. That keeps one replacement span for the whole
/// word, which is what the editor needs.
#[derive(Clone, Copy, Debug, Default)]
pub struct ParamOpProvider;

impl Provider for ParamOpProvider {
    fn id(&self) -> ProviderId {
        ProviderId::ParamOp
    }

    fn applies(&self, ctx: &Ctx) -> bool {
        ctx.param
            && ctx.quote != QuoteCtx::SingleQuoted
            && !ctx.prefix.is_empty()
            && is_name_prefix(&ctx.prefix)
    }

    fn candidates(&self, ctx: &Ctx) -> Vec<Candidate> {
        sources::PARAM_OPS
            .iter()
            .map(|op| {
                let mut insert = ctx.prefix.clone();
                insert.extend_from_slice(op.insert);
                Candidate::source(insert, CandKind::Operator).with_desc(op.desc)
            })
            .collect()
    }
}

/// `[[ … ]]` operators.
#[derive(Clone, Copy, Debug, Default)]
pub struct TestOpProvider;

impl Provider for TestOpProvider {
    fn id(&self) -> ProviderId {
        ProviderId::TestOp
    }

    fn applies(&self, ctx: &Ctx) -> bool {
        ctx.expect == Expect::TestOp && !ctx.param && ctx.quote == QuoteCtx::Unquoted
    }

    fn candidates(&self, _ctx: &Ctx) -> Vec<Candidate> {
        sources::TEST_OPS
            .iter()
            .map(|op| {
                Candidate::source(op.insert, CandKind::Operator)
                    .with_desc(op.desc)
                    .appending_space()
            })
            .collect()
    }
}

/// Shell keywords for the contexts whose only sensible completion is a keyword.
#[derive(Clone, Copy, Debug, Default)]
pub struct KeywordProvider;

impl Provider for KeywordProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Keyword
    }

    fn applies(&self, ctx: &Ctx) -> bool {
        ctx.quote == QuoteCtx::Unquoted && !sources::keywords_for(ctx.expect).is_empty()
    }

    fn candidates(&self, ctx: &Ctx) -> Vec<Candidate> {
        sources::keywords_for(ctx.expect)
            .iter()
            .map(|keyword| {
                Candidate::source(*keyword, CandKind::Keyword)
                    .appending_space()
                    .with_desc("keyword")
            })
            .collect()
    }
}

/// `case` pattern completion.
///
/// The catch-all `*)` is always offered. Tags from the enclosing function's
/// other arms would need the function body, which completion does not have; the
/// remaining tags are supplied by the caller (`bsh-repl` can pass the arms it
/// has already seen) and default to none.
pub struct CaseTagProvider {
    tags: Vec<Vec<u8>>,
}

impl CaseTagProvider {
    /// A provider offering only the catch-all pattern.
    pub fn new() -> Self {
        Self { tags: Vec::new() }
    }

    /// Add patterns that should be offered alongside `*)`.
    #[must_use]
    pub fn with_tags(mut self, tags: Vec<Vec<u8>>) -> Self {
        self.tags = tags;
        self
    }
}

impl Default for CaseTagProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for CaseTagProvider {
    fn id(&self) -> ProviderId {
        ProviderId::CaseTag
    }

    fn applies(&self, ctx: &Ctx) -> bool {
        ctx.expect == Expect::CasePattern && ctx.quote == QuoteCtx::Unquoted
    }

    fn candidates(&self, _ctx: &Ctx) -> Vec<Candidate> {
        let mut candidates = vec![
            Candidate::source(sources::CATCH_ALL_PATTERN, CandKind::Operator)
                .with_desc("match anything"),
        ];
        candidates.extend(
            self.tags.iter().map(|tag| {
                Candidate::source(tag.clone(), CandKind::Operator).with_desc("case tag")
            }),
        );
        candidates
    }
}

/// Whether `bytes` could be a parameter name so far.
fn is_name_prefix(bytes: &[u8]) -> bool {
    let mut iter = bytes.iter();
    let Some(first) = iter.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || *first == b'_')
        && iter.all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_prefixes() {
        assert!(is_name_prefix(b"x"));
        assert!(is_name_prefix(b"_a1"));
        assert!(!is_name_prefix(b""));
        assert!(!is_name_prefix(b"1x"));
        assert!(!is_name_prefix(b"x:"));
    }
}
