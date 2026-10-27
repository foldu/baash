//! Shell state: options, variables, and the host interface the VM calls back
//! into.
//!
//! This crate must not depend on `bsh-vm`, or `eval`, `source` and traps would
//! create a cycle. Re-entrant execution therefore goes through [`ShellHost`],
//! which the VM implements (DESIGN.md §4.1).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// An exit status. Only the low 8 bits reach `$?`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Status(pub i32);

impl Status {
    /// Success.
    pub const SUCCESS: Status = Status(0);
    /// Command not found.
    pub const NOT_FOUND: Status = Status(127);
    /// Found but not executable.
    pub const NOT_EXECUTABLE: Status = Status(126);

    /// Whether this status counts as success.
    pub fn is_success(self) -> bool {
        self.0 == 0
    }

    /// The value `$?` expands to.
    pub fn exit_code(self) -> i32 {
        self.0 & 0xff
    }
}

/// Option bundle. A profile is a named preset, not a code path
/// (DESIGN.md §3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Opts {
    /// Strict mode: reject bash-only nodes after parsing.
    pub posix_mode: bool,
    /// `set -e`.
    pub errexit: bool,
    /// `set -u`.
    pub nounset: bool,
    /// `set -f`.
    pub noglob: bool,
    /// `set -n`.
    pub noexec: bool,
    /// bash `extglob`.
    pub extglob: bool,
    /// bash `dotglob`.
    pub dotglob: bool,
    /// bash `nullglob`.
    pub nullglob: bool,
    /// bash `globstar`.
    pub globstar: bool,
    /// bash `shopt -s lastpipe`.
    pub lastpipe: bool,
    /// bash `nocasematch`.
    pub nocasematch: bool,
    /// `xpg_echo`: `echo` interprets escapes without `-e`.
    pub xpg_echo: bool,
    /// Parse-time alias substitution.
    pub expand_aliases: bool,
    /// History expansion (`!$`). Off by default even in the bash profile.
    pub histexpand: bool,
}

impl Opts {
    /// POSIX profile.
    pub const POSIX: Opts = Opts {
        posix_mode: true,
        errexit: false,
        nounset: false,
        noglob: false,
        noexec: false,
        extglob: false,
        dotglob: false,
        nullglob: false,
        globstar: false,
        lastpipe: false,
        nocasematch: false,
        xpg_echo: false,
        expand_aliases: false,
        histexpand: false,
    };

    /// bash compatibility profile: POSIX plus the extensions scripts expect.
    pub const BASH: Opts = Opts {
        posix_mode: false,
        errexit: false,
        nounset: false,
        noglob: false,
        noexec: false,
        extglob: false,
        dotglob: false,
        nullglob: false,
        globstar: false,
        lastpipe: true,
        nocasematch: false,
        xpg_echo: false,
        expand_aliases: true,
        histexpand: false,
    };
}

impl Default for Opts {
    fn default() -> Self {
        Self::BASH
    }
}

/// Variable attributes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attrs {
    /// Passed to children via the environment.
    pub exported: bool,
    /// Writes are an error.
    pub readonly: bool,
    /// Arithmetic on assignment.
    pub integer: bool,
}

/// A variable's value. Bash arrays are extensions, hence the extra shapes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum VarValue {
    /// Declared but unset.
    #[default]
    Unset,
    /// A scalar.
    Str(Vec<u8>),
    /// An indexed array.
    Indexed(Vec<Vec<u8>>),
    /// An associative array, kept as insertion-ordered pairs.
    Assoc(Vec<(Vec<u8>, Vec<u8>)>),
}

/// A variable: value plus attributes.
#[derive(Clone, Debug, Default)]
pub struct Var {
    /// Current value.
    pub value: VarValue,
    /// Current attributes.
    pub attrs: Attrs,
}

/// What the runtime needs from the executor: the ability to run shell code
/// re-entrantly for `eval`, `.`/`source`, traps and completion functions.
pub trait ShellHost {
    /// Run `src` as shell code in the current environment.
    fn run_source(&mut self, src: &[u8]) -> Status;
}
