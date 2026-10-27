//! Developer tooling: `fmt`, `lint`, `explain`.
//!
//! Not implemented. This crate exists now so the binary can grow subcommands
//! without restructuring, and so the tooling has a home that is allowed to
//! depend on rendering (DESIGN.md §4.1, §9).
//!
//! Planned:
//!
//! * `lint --posix` — reject bash-only nodes from the AST, with spans and a
//!   fix-it. Strictly better than `checkbashisms`, which is a pile of regexes.
//! * `fmt` — pretty-print from the lossless AST.
//! * `explain` — print every expansion stage and the resulting argv, the
//!   feature no existing shell has.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
