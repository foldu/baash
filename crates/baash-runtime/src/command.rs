//! What a command name means, as one shared description.
//!
//! `type`, `command -v`/`-V` and `builtin` all have to answer "what would the
//! shell do with this name?", and the answer has to be the *executor's* answer
//! or the shell lies about itself. So the resolution lives in `baash-vm` — it
//! owns the function table and the `PATH` search — and this module holds the
//! vocabulary it reports through [`crate::ShellHost::resolve_command`].
//!
//! The order is POSIX §2.9.1.4 plus bash's aliases and keywords:
//!
//! ```text
//! alias → keyword → special built-in → function → intrinsic built-in
//!       → regular/extension built-in → PATH
//! ```
//!
//! Aliases and keywords are not part of the executor's own search (alias
//! expansion happens before parsing and the parser consumes keywords), but
//! `type` must still report them, so they are part of the *classification*
//! and never part of what a command word resolves to for execution.

use crate::Tier;

/// The class a name belongs to, in search order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandKind {
    /// A shell alias: text substituted at command position before parsing.
    Alias,
    /// A reserved word (`while`, `for`, `{`, …).
    Keyword,
    /// A shell function.
    Function,
    /// A built-in, in its POSIX 2024 tier.
    Builtin(Tier),
    /// An executable file found through `PATH`.
    External,
}

impl CommandKind {
    /// The word `type -t` prints for this class.
    pub fn type_word(self) -> &'static str {
        match self {
            CommandKind::Alias => "alias",
            CommandKind::Keyword => "keyword",
            CommandKind::Function => "function",
            CommandKind::Builtin(_) => "builtin",
            CommandKind::External => "file",
        }
    }

    /// Whether the name would run an external program.
    pub fn is_external(self) -> bool {
        matches!(self, CommandKind::External)
    }
}

/// One place a name resolved to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandInfo {
    /// What the name is.
    pub kind: CommandKind,
    /// The alias value, the function's body text, or the executable's path.
    /// Empty for keywords and built-ins, where the operand is the name itself.
    pub target: Vec<u8>,
}

impl CommandInfo {
    /// A keyword, function or built-in with no extra payload.
    pub fn plain(kind: CommandKind) -> Self {
        Self {
            kind,
            target: Vec::new(),
        }
    }
}

/// How much of the search order a caller wants walked.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResolveQuery {
    /// Report every match instead of the first (`type -a`).
    pub all: bool,
    /// Ignore shell functions (`type -f`).
    pub skip_functions: bool,
    /// Only executable files, ignoring aliases, keywords, functions and
    /// built-ins (`type -p`, `type -P`).
    pub files_only: bool,
    /// Include aliases and keywords at the front of the order. This is what
    /// turns the executor's search into a *classification* for `type` and
    /// `command -v`.
    pub classify: bool,
    /// Search `_CS_PATH` instead of `$PATH` (`command -p`).
    pub default_path: bool,
    /// Bypass the hashed `PATH` cache (`type -P` forces a fresh search).
    pub fresh: bool,
}

impl ResolveQuery {
    /// The query a plain search uses: the executor's order, first match, no
    /// aliases or keywords (`alias` expansion and keywords are handled before
    /// a command word is resolved).
    pub const EXECUTION: Self = Self {
        all: false,
        skip_functions: false,
        files_only: false,
        classify: false,
        default_path: false,
        fresh: false,
    };

    /// The query `type`/`command -v` use: the full classification.
    pub const CLASSIFY: Self = Self {
        classify: true,
        ..Self::EXECUTION
    };
}
