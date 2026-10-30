//! The execution engine.
//!
//! One engine, deliberately: [`tree`] is a tree-walking evaluator over the
//! `bsh-syntax` AST, and it is the whole implementation — word expansion
//! wiring, the command search order, redirections, pipelines, compound
//! commands, unwinding (`break`/`continue`/`return`/`exit`/`errexit`) and the
//! POSIX §2.8.1 consequence table.
//!
//! A bytecode backend was planned as a second engine to differential-test
//! against this one. That plan is dropped (`docs/direction.md` §2): dispatch
//! was never the cost — a shell statement ends in a syscall or a word
//! expansion — and a second engine is a permanent agreement to maintain. If a
//! profile ever disagrees, the fix is inside this crate (cache the command
//! lookup, expand words once), not a second implementation.
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
