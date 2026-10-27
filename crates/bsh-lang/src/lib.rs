//! Words, quoting and expansion for ßhell.
//!
//! This crate is where roughly 60% of shell conformance bugs live
//! (DESIGN.md §7): the word model, the five-stage expansion pipeline, IFS
//! field splitting, `$@`/`$*`, and quote removal. It deliberately depends on
//! neither `bsh-syntax` nor `bsh-runtime`; the front end constructs a
//! [`Word`], a variable source implements [`Vars`], and the runtime supplies
//! command substitution, arithmetic and globbing through [`Shell`].
//!
//! # The word model
//!
//! A [`Word`] is a sequence of [`Part`]s (DESIGN.md §4.2). Quoting belongs to
//! the part, not the word: [`Part`]'s module documentation explains why
//! `"$@"`, `${x:-"$@"}` and `$*` cannot be represented with a per-word flag.
//!
//! Expansion needs two independent properties per piece of text:
//!
//! * **quoted** — inert for pathname expansion;
//! * **an expansion result** — subject to field splitting.
//!
//! Literal source text is unquoted but still not split: POSIX XCU 2.6.5
//! applies field splitting only to the results of parameter expansion,
//! command substitution and arithmetic expansion. `IFS=:; printf '%s\n' a:b`
//! is one field; `x=a:b; printf '%s\n' $x` is two. Text inside a parameter
//! expansion's replacement word counts as part of that expansion's result, so
//! `${x:-a:b}` is two fields as well. [`LitKind`] and [`Chunk`] encode this.
//!
//! # The five stages
//!
//! POSIX XCU 2.6 fixes the order, and each stage is a separate documented
//! function so `bsh explain` can show intermediate results:
//!
//! 1. [`expand_tilde`] — tilde expansion.
//! 2. [`expand_parameters`] — parameter, command and arithmetic expansion,
//!    left to right. Command substitution and arithmetic are [`Shell`]
//!    callbacks.
//! 3. [`split_fields`] — IFS field splitting.
//! 4. [`expand_pathnames`] — pathname expansion; the callback seam, with a
//!    passthrough stub that leaves the field unchanged.
//! 5. [`remove_quotes`] — quote removal.
//!
//! [`expand_word`] runs all five. [`Mode::Assignment`] runs only the stages
//! an assignment value needs (no splitting, no globbing).
//!
//! # Seams
//!
//! Command substitution and arithmetic evaluation are [`Shell`] callbacks, and
//! pathname expansion calls [`Shell::pathname`]; none of them is implemented
//! here. [`Passthrough`] is the documented stub that leaves those expansions
//! unchanged (`$(cmd)` becomes its source text, a glob match is `None`), so the
//! pipeline is usable — and testable — before the runtime exists. It is not a
//! shell and must never be used as one.
//!
//! # Deliberate gaps
//!
//! Operators that need pattern matching or re-parse their argument as code —
//! `${x#pat}`, `${x%pat}`, `${x/pat/rep}`, `${!x}` on a non-array, `${x@Q}`,
//! `${x^^pat}` — return [`ErrorKind::Unsupported`] instead of doing something
//! subtly wrong. `${@^^}` is likewise unsupported rather than approximated.
//! Byte-oriented `LC_ALL=C` semantics are used for `${#x}` and `${x:o:l}`.
//!
//! Arrays are part of the model: [`ParamName::Element`] is `${a[i]}` and
//! [`ParamName::Elements`] is `${a[@]}`/`${a[*]}`; [`Vars::array`] exposes the
//! storage. List slicing (`${a[@]:o:l}`, `${@:o:l}`) evaluates its bounds
//! through [`Shell::arithmetic_value`].
//!
//! # Bytes, never `str`
//!
//! Words, fields and argv are `Vec<u8>`/`&[u8]` everywhere. Filenames are
//! arbitrary non-NUL bytes and `$'\xff'` exists (DESIGN.md §4.2).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod expand;
mod host;
mod ifs;
mod quote;
mod vars;
mod word;

pub use bsh_diag::Span;
pub use expand::{
    Chunk, Error, ErrorKind, Expanded, Field, Mode, codes, expand_assignment, expand_parameters,
    expand_pathnames, expand_tilde, expand_word, expand_word_with, expand_words, remove_quotes,
    split_fields,
};
pub use host::{EmptyVars, Passthrough, Shell};
pub use ifs::Ifs;
pub use quote::{QuoteCtx, quote};
pub use vars::{ArrayKind, ArrayValue, SetError, Vars};
pub use word::{
    Arithmetic, CaseHow, CmdSubStyle, CommandSub, DoubleQ, Lit, LitKind, ParamExp, ParamName,
    ParamOp, Part, ProcSubDir, ProcessSub, TildeSpec, UnsupportedOp, Word,
};

#[cfg(test)]
mod fixture;
#[cfg(test)]
mod tests;
