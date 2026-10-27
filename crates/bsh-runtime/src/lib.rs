//! Shell state: options, variables, the builtin registry and the host
//! interface the VM calls back into.
//!
//! This crate must not depend on `bsh-vm`, or `eval`, `source` and traps would
//! create a cycle. Everything the VM needs to mutate — options, variables,
//! positional parameters, the builtin table — lives here, and re-entrant
//! execution goes through [`ShellHost`], which the VM implements
//! (DESIGN.md §4.1).
//!
//! # What lives where
//!
//! * `opts` — [`Opts`] and [`Flag`], the bidirectional name table that
//!   `set -o`/`shopt` drive (DESIGN.md §3.2). A profile is a preset value.
//! * `vars` — [`Vars`]: the global scope, frame stack, positional
//!   parameters, special parameters and the `envp` encoding (DESIGN.md §4.6).
//! * `builtins` — [`Tier`] and [`BUILTINS`], with POSIX 2024's special,
//!   intrinsic and regular tiers plus bash extensions.
//! * `errors` — state errors and the POSIX §2.8.1 consequence table.
//!
//! Shell text is bytes throughout: names and values are `Vec<u8>`, never
//! `String`. Decoding to UTF-8 happens only for display and diagnostics.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod builtins;
mod errors;
mod opts;
mod vars;

#[cfg(test)]
mod tests;

pub use builtins::{
    AssignPersistence, BUILTINS, BuiltinDef, Handler, Tier, builtin_names, declaration_scope,
    declare_array_from_flag, lookup as lookup_builtin, names_in, run_builtin, set_option_table,
    stubs_in, tier_of,
};
pub use errors::{Action, Error, ShellError, codes, consequence, display as display_bytes};
pub use opts::{ALL_FLAGS, Flag, NamedOption, OptionKind, Opts};
pub use vars::{
    ArrayShape, Attrs, CreateScope, FrameId, FrameKind, Positional, PrefixAssignments, Var,
    VarValue, Vars,
};

/// An exit status. Only the low 8 bits reach `$?`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Status(pub i32);

impl Status {
    /// Success.
    pub const SUCCESS: Status = Status(0);
    /// A generic failure, `1`.
    pub const FAILURE: Status = Status(1);
    /// Usage error, `2` (the status bash uses for a bad builtin invocation).
    pub const USAGE: Status = Status(2);
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

/// The mutable state the VM owns and hands to builtins.
///
/// It bundles options and variables so that [`ShellHost::state`] is one
/// method, and carries the two pieces of command control that must survive a
/// builtin call: the last error class (for POSIX §2.8.1) and a pending `exit`
/// request. The VM reads both after every builtin.
#[derive(Clone, Debug, Default)]
pub struct ShellState {
    /// Shell options.
    pub opts: Opts,
    /// Variables, scopes and positional parameters.
    pub vars: Vars,
    error: Option<ShellError>,
    exit_request: Option<Status>,
}

impl ShellState {
    /// State with `opts` and an empty variable store.
    pub fn new(opts: Opts) -> Self {
        Self {
            opts,
            vars: Vars::default(),
            error: None,
            exit_request: None,
        }
    }

    /// The error class recorded by the most recent failing operation, if any.
    pub fn error(&self) -> Option<ShellError> {
        self.error
    }

    /// Record an error class. Later classes replace earlier ones; the VM takes
    /// it after each command.
    pub fn record_error(&mut self, error: ShellError) {
        self.error = Some(error);
    }

    /// Take the recorded error class.
    pub fn take_error(&mut self) -> Option<ShellError> {
        self.error.take()
    }

    /// Clear the recorded error class without looking at it.
    pub fn clear_error(&mut self) {
        self.error = None;
    }

    /// Ask the VM to unwind the shell (`exit`). The status is unmasked; `$?`
    /// masks it.
    pub fn request_exit(&mut self, status: Status) {
        self.exit_request = Some(status);
    }

    /// The pending exit status, if `exit` (or a fatal error) was requested.
    pub fn exit_requested(&self) -> Option<Status> {
        self.exit_request
    }

    /// Take the pending exit request.
    pub fn take_exit_request(&mut self) -> Option<Status> {
        self.exit_request.take()
    }
}

/// What the runtime needs from the executor: the ability to run shell code
/// re-entrantly for `eval`, `.`/`source`, traps and completion functions, and
/// the byte sinks a builtin writes to.
///
/// The VM implements this trait. Keeping I/O here rather than in this crate
/// means `echo`, `printf`, `export -p` and `set -o` all go through the VM's
/// descriptor table and can be captured by tests.
pub trait ShellHost {
    /// Mutable shell state. Builtins use this for variables and options.
    fn state(&mut self) -> &mut ShellState;

    /// Run `src` as shell code in the current environment.
    fn run_source(&mut self, src: &[u8]) -> Status;

    /// Write raw bytes to standard output. Returns the write's status so a
    /// builtin can report `EPIPE`.
    fn write_out(&mut self, bytes: &[u8]) -> Status;

    /// Write raw bytes to standard error.
    fn write_err(&mut self, bytes: &[u8]) -> Status;

    /// Whether the shell is interactive. Defaults to false; the VM overrides
    /// it for `-i`.
    fn is_interactive(&self) -> bool {
        false
    }

    /// Remove one array element for `unset 'name[subscript]'`.
    ///
    /// `unset` re-parses its operand as an assignment word, so the subscript
    /// may contain expansion (`unset 'a[$i]'`) and the array may be
    /// associative (a string key) or indexed (arithmetic). That is word work,
    /// which lives in the VM; the storage lives in this crate. `None` means
    /// the host does not implement it, and the runtime keeps its "not
    /// implemented yet" diagnostic.
    fn unset_array_element(&mut self, operand: &[u8]) -> Option<Status> {
        let _ = operand;
        None
    }

    /// Report a diagnostic.
    ///
    /// The default renders one greppable line on standard error, which is the
    /// right shape at a prompt; the VM overrides this to use `bsh-diag`'s
    /// renderer and the chunk's source spans. Diagnostics are data: a handler
    /// never prints directly.
    fn diagnose(&mut self, diagnostic: &bsh_diag::Diagnostic) {
        let mut line = Vec::new();
        line.extend_from_slice(b"bsh: ");
        line.extend_from_slice(diagnostic.message.as_bytes());
        line.extend_from_slice(b" [");
        line.extend_from_slice(diagnostic.code.to_string().as_bytes());
        line.extend_from_slice(b"]\n");
        let _ = self.write_err(&line);
    }
}
