//! Completion: candidates, context and ranking.
//!
//! Not implemented. The contract (DESIGN.md §6.4, §6.5): the parser's
//! expectation at the cursor selects the provider set, and ranking is
//! two-tier — typed prefix always outranks a fuzzy match, so `git ch<TAB>` can
//! never lose to noise. `nucleo-matcher` (no threadpool, unlike the high-level
//! `nucleo` crate) is the intended fuzzy scorer.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use bsh_syntax::Expect;

/// What kind of thing a candidate is; drives the icon and the fallback order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandKind {
    /// A path, matched by component prefix.
    Path,
    /// A command from the `PATH` index.
    Command,
    /// A variable or parameter operator.
    Variable,
    /// A command-line option.
    Option,
    /// Anything else.
    Other,
}

/// One thing the user could insert.
#[derive(Clone, Debug)]
pub struct Candidate {
    /// Bytes to insert; authoritative, and not necessarily UTF-8.
    pub insert: Vec<u8>,
    /// UTF-8 text to show, which may be a lossy projection of `insert`.
    pub display: String,
    /// A description column, e.g. "branch, 3 days ago".
    pub desc: Option<String>,
    /// What kind of candidate this is.
    pub kind: CandKind,
    /// Frequency-times-recency weight.
    pub frecency: f32,
}

/// Where in the grammar the cursor is, and what has been typed there.
#[derive(Clone, Debug)]
pub struct CompletionCtx {
    /// The parser's expectation at the cursor.
    pub expect: Expect,
    /// Bytes already typed in the word being completed.
    pub prefix: Vec<u8>,
}
