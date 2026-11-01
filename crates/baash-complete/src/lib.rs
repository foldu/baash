//! Completion: candidates, context and ranking.
//!
//! The contract is `DESIGN.md` §6.3–§6.9. Three commitments shape this crate:
//!
//! * **The parser decides the context.** [`context`] takes the line and a
//!   cursor byte offset, asks [`baash_syntax::lex_full`] for the word span and
//!   the quote context, and [`baash_syntax::expectations_at`] for what the
//!   grammar allows there. There is no whitespace splitting and no word-break
//!   heuristic.
//! * **Ranking is two-tier.** Typed-prefix matches always outrank fuzzy ones,
//!   so `git ch<TAB>` can never lose to a fuzzy match on something else
//!   ([`Ranker`]). Fuzzy scoring is `nucleo-matcher` directly — a matcher with
//!   no threadpool — with `prefer_prefix` on and `match_paths()` for paths.
//! * **Shell text is bytes.** [`Candidate::insert`] is authoritative and may be
//!   invalid UTF-8; [`Candidate::display`] is a lossy projection for rendering.
//!   Insertion goes through [`baash_lang::quote`] and never through `String`.
//!
//! # How a completion is produced
//!
//! ```
//! use baash_complete::{Completer, CompletionEngine};
//! let engine = CompletionEngine::new();
//! let menu = engine.complete(b"ls ", 3);
//! // Every candidate knows which span it replaces and how to quote itself.
//! assert_eq!(menu.replace, 3..3);
//! if menu.candidates.first().is_some() {
//!     let bytes = menu.insertion(0).expect("insertion bytes");
//!     let _ = bytes; // splice into the line with `menu.apply`.
//! }
//! ```
//!
//! The pipeline is: [`context`] → the expectation-to-provider table
//! ([`providers_for`]) → providers ([`PathProvider`], [`CommandProvider`], …)
//! → [`Ranker`] → [`Menu`].
//!
//! # What the editor sees
//!
//! [`Menu`] carries the ranked candidates, the full word span, the span to
//! replace, the quote context and a [`Menu::insertion`] helper. `baash-repl` owns
//! rendering, key handling and the line editor; this crate owns the data.
//!
//! # Wiring up state that lives elsewhere
//!
//! Variables, history, functions and aliases are traits here
//! ([`VarNames`], [`History`], [`ShellNames`]) so `baash-runtime` can implement
//! them without a dependency edge pointing back into this crate. They are
//! `Send + Sync` because the full picker may run providers on a worker thread
//! (`DESIGN.md` §6.8); a runtime store with interior mutability should hand
//! over a snapshot or a `Mutex`-backed view.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod candidate;
mod commands;
mod context;
mod engine;
mod menu;
mod names;
mod ops;
mod paths;
mod rank;
mod sources;
mod spec;
mod text;

pub use baash_lang::QuoteCtx;

pub use candidate::{CandKind, Candidate, Insertion, Span};
pub use commands::{CommandIndex, CommandProvider};
pub use context::{Ctx, context};
pub use engine::{Completer, CompletionEngine, Config};
pub use menu::Menu;
pub use names::{
    AliasProvider, BuiltinProvider, FunctionProvider, History, HistoryProvider, ShellNames,
    VarNames, VarProvider,
};
pub use ops::{CaseTagProvider, KeywordProvider, ParamOpProvider, TestOpProvider};
pub use paths::{PathMode, PathProvider};
pub use rank::Ranker;
pub use sources::{Op, PARAM_OPS, PROVIDERS, Provider, ProviderId, TEST_OPS, providers_for};
pub use spec::{
    Action, OPT_BASHDEFAULT, OPT_DEFAULT, OPT_DIRNAMES, OPT_NOSPACE, OPT_PLUSDIRS, Spec, SpecSource,
};

/// The default number of entries one directory may contribute.
pub const ENTRY_CAP: usize = paths::DEFAULT_ENTRY_CAP;
