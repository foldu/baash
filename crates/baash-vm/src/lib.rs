//! The execution engine.
//!
//! One engine, deliberately: [`tree`] is a tree-walking evaluator over the
//! `baash-syntax` AST, and it is the whole implementation — word expansion
//! wiring, the command search order, redirections, pipelines, compound
//! commands, unwinding (`break`/`continue`/`return`/`exit`/`errexit`) and the
//! POSIX §2.8.1 consequence table.
//!
//! A bytecode backend is deferred, not abandoned — the crate is named for one,
//! and `DESIGN.md` §5 keeps the design (`docs/direction.md` §2). Dispatch is not
//! where a shell spends its time: a statement ends in a word expansion or a
//! syscall, and measured interpretation is already 1.0–2.1× bash. A second
//! engine built now would be a permanent agreement to keep two implementations
//! identical. Until it exists, a profile that disagrees with bash is fixed here
//! (cache the command lookup, expand words once).
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
/// `docs/contracts.md` gives `baash-vm` the 2xx range for the POSIX §2.8.1
/// consequence layer. Codes are stable; message text is not.
pub mod codes {
    use baash_diag::ErrCode;

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
    /// An unset variable was read in a `nounset` context (bash's "unbound
    /// variable"), which is fatal for a non-interactive shell.
    pub const UNBOUND: ErrCode = ErrCode::new(209);
    /// An internal invariant was violated; always a bug in this crate.
    pub const INTERNAL: ErrCode = ErrCode::new(299);
}
