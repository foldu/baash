//! AST to bytecode.
//!
//! Not implemented. The shapes below are the settled part (DESIGN.md §5.2,
//! §5.3, §5.9); the compiler itself lands after the AST interpreter, so the
//! two can be differential-tested against each other (§5.7).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use bsh_diag::Span;

/// Where an unwinding handler resumes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandlerKind {
    /// `set -e` reaches the top of the current command list.
    Errexit,
    /// `trap … ERR`.
    TrapErr,
    /// `break`/`continue` to the Nth enclosing loop.
    Loop(u8),
    /// `return` out of a function frame.
    Function,
    /// `exit`, after which the `EXIT` trap runs.
    Exit,
}

/// Argument words for a command, resolved at compile time when possible.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Words {
    /// Every word was literal after quote removal: no `$`, no unquoted glob
    /// metacharacter, no `~`. The argv is prebuilt and needs no expansion.
    Static(Vec<Vec<u8>>),
    /// Expansion is required; the id indexes the chunk's word table.
    Dynamic(u32),
}

/// How a command name resolved when the chunk was compiled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandRef {
    /// A special builtin: assignments persist, failures are fatal.
    Special(u16),
    /// An intrinsic utility: a function may override it, `PATH` may not.
    Intrinsic(u16),
    /// A `PATH` hit, valid while the `PATH` epoch is unchanged.
    Resolved {
        /// The resolved executable path.
        path: Vec<u8>,
        /// `PATH` epoch this resolution is valid for; `hash -r` bumps it.
        epoch: u32,
    },
    /// A function, valid while the function table epoch is unchanged.
    Function {
        /// Index into the function table.
        id: u32,
        /// Function table epoch; redefining any function bumps it.
        epoch: u32,
    },
    /// Not statically knowable: run the full dynamic search.
    Search,
}

/// A bytecode instruction. Words are operands, never instructions
/// (DESIGN.md §5.2).
#[derive(Clone, Debug)]
pub enum Op {
    /// Execute one simple command.
    ExecSimple {
        /// Argument words.
        words: Words,
        /// Redirections, applied left to right.
        redirs: u32,
        /// How the name was resolved.
        command: CommandRef,
        /// Source span, for `xtrace` and diagnostics.
        span: Span,
    },
    /// Execute a pipeline.
    ExecPipeline {
        /// Number of stages.
        stages: u8,
        /// Whether the pipeline was prefixed with `!`.
        negated: bool,
    },
    /// Populate a loop variable and enter the body.
    ForEach {
        /// Loop variable name.
        var: u32,
        /// The word list to iterate.
        words: Words,
    },
    /// A compiled command list.
    Jump(u32),
    /// `return`.
    Return,
    /// `break` N levels.
    Break(u8),
    /// `continue` N levels.
    Continue(u8),
    /// `exit`.
    Exit,
    /// Install an unwinding handler.
    PushHandler {
        /// What it catches.
        kind: HandlerKind,
        /// Where it resumes.
        target: u32,
    },
    /// Remove the innermost handler.
    PopHandler,
}

/// A compiled chunk: code plus the side tables it references.
#[derive(Clone, Debug, Default)]
pub struct Chunk {
    /// Instructions.
    pub code: Vec<Op>,
    /// The source this chunk was compiled from, owned so diagnostics can be
    /// rendered later (DESIGN.md §4.11).
    pub source: Option<std::sync::Arc<[u8]>>,
}
