//! The execution engine.
//!
//! This crate has two backends, deliberately, and the order matters
//! (DESIGN.md §5.7):
//!
//! * [`tree`] — the **reference backend**. A tree-walking evaluator over the
//!   `bsh-syntax` AST that owns the semantics: word expansion wiring, the
//!   command search order, redirections, pipelines, compound commands,
//!   unwinding (`break`/`continue`/`return`/`exit`/`errexit`) and the POSIX
//!   §2.8.1 consequence table. It ships first and stays forever: the bytecode
//!   backend is differential-tested against it, so a disagreement between the
//!   two is a bug in the compiler, not a second opinion about the language.
//! * [`Vm`] — the bytecode interpreter loop. Not written yet; the settled
//!   shapes live in `bsh-compile` and the loop lands next (DESIGN.md §5.9).
//!   The evaluator is the oracle that makes that safe to do.
//!
//! # The per-command sequence is fixed
//!
//! `docs/contracts.md` records it and the evaluator implements it literally:
//!
//! ```text
//! resolve the command's tier
//! → Vars::apply_prefix_assignments(assigns, tier)
//! → run the command
//! → Vars::finish_prefix_assignments()
//! → ShellState::take_error() → consequence(error, interactive)
//! → ShellState::take_exit_request()
//! ```
//!
//! # Bytes, never `str`
//!
//! Words, argv, paths and variable values are `Vec<u8>`. Conversion to `OsStr`
//! happens at the syscall boundary and nowhere else, because filenames are
//! arbitrary non-NUL bytes (DESIGN.md §4.2).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use bsh_compile::Chunk;

pub mod arith;
pub mod glob;
mod pattern;
pub mod tree;

pub use tree::{Interp, TrapKind, Unwind};

/// Stable diagnostic codes emitted by the evaluator.
///
/// `docs/contracts.md` gives `bsh-vm` the 2xx range for the POSIX §2.8.1
/// consequence layer. Codes are stable; message text is not.
pub mod codes {
    use bsh_diag::ErrCode;

    /// The command name resolved to nothing.
    pub const COMMAND_NOT_FOUND: ErrCode = ErrCode::new(201);
    /// A redirection could not be set up.
    pub const REDIRECTION: ErrCode = ErrCode::new(202);
    /// Arithmetic evaluation failed.
    pub const ARITHMETIC: ErrCode = ErrCode::new(203);
    /// A `[[ … ]]` conditional could not be parsed or evaluated.
    pub const CONDITIONAL: ErrCode = ErrCode::new(204);
    /// `exec` failed.
    pub const EXEC: ErrCode = ErrCode::new(205);
    /// A shell feature this wave does not implement (see `tree`'s
    /// `# Deferred` section).
    pub const DEFERRED: ErrCode = ErrCode::new(206);
    /// A variable assignment failed outside a built-in (for example a readonly
    /// write in a bare `x=1`).
    pub const ASSIGNMENT: ErrCode = ErrCode::new(207);
    /// `break`/`continue`/`return` used where there is nothing to unwind to.
    pub const CONTROL_FLOW: ErrCode = ErrCode::new(208);
    /// An internal invariant was violated; always a bug in this crate.
    pub const INTERNAL: ErrCode = ErrCode::new(299);
}

/// Interpreter state for the bytecode backend.
///
/// Not implemented: the plan (DESIGN.md §5.9) is a plain
/// `loop { match code[pc] { … } }` on stable Rust — no threaded dispatch, no
/// `musttail`, no unsafe, because in a shell almost every statement ends in a
/// syscall and dispatch is a small term. The escape hatch, if it ever shows up
/// in a profile, is one closure per op. Use [`tree::Interp`] until then.
#[derive(Debug, Default)]
pub struct Vm {
    /// The chunk being executed.
    pub chunk: Chunk,
    /// Program counter.
    pub pc: usize,
}
