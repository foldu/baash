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
//! * `alias` — [`AliasTable`], the textual substitutions `alias` defines. The
//!   substitution itself is the VM's, because it re-enters the parser.
//! * `command` — [`CommandKind`]/[`CommandInfo`]/[`ResolveQuery`], the
//!   vocabulary `type`, `command -v` and `builtin` share with the executor.
//! * `errors` — state errors and the POSIX §2.8.1 consequence table.
//!
//! Shell text is bytes throughout: names and values are `Vec<u8>`, never
//! `String`. Decoding to UTF-8 happens only for display and diagnostics.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod alias;
mod builtins;
mod command;
pub mod complete;
mod errors;
mod opts;
mod traps;
mod vars;

#[cfg(test)]
mod tests;

pub use alias::AliasTable;
pub use builtins::{
    AssignPersistence, BUILTINS, BuiltinDef, Handler, Tier, builtin_names, declaration_scope,
    declare_array_from_flag, lookup as lookup_builtin, names_in, run_builtin, set_option_table,
    stubs_in, tier_of,
};
pub use command::{CommandInfo, CommandKind, ResolveQuery};
pub use errors::{Action, Error, ShellError, codes, consequence, display as display_bytes};
pub use opts::{ALL_FLAGS, Flag, NamedOption, OptionKind, Opts};
pub use traps::{
    SIGNALS, TrapAction, TrapKey, TrapTable, signal_number, signal_short_name, signal_table,
};
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
    /// Registered traps, including the `EXIT`/`ERR`/`DEBUG`/`RETURN`
    /// pseudo-signals. Signal *dispositions* are the VM's business: this table
    /// only records what `trap` was asked to do.
    pub traps: TrapTable,
    /// Alias definitions. They are inherited by a forked subshell (bash copies
    /// them) and, like every other piece of shell state, not by `exec`.
    pub aliases: AliasTable,
    /// Programmable-completion specs: what `complete` defines. Kept in
    /// definition order like [`AliasTable`], because `complete -p` prints in
    /// that order.
    pub completions: complete::CompletionTable,
    /// The completion being generated, if any: the state `compopt` edits.
    ///
    /// Deliberately *not* part of [`ShellState::completions`]: a `compopt`
    /// inside a completion function changes the run in progress, and only the
    /// engine that finishes the completion decides what to save.
    pub completion: complete::CompletionRun,
    /// Where `getopts` is in the argument it is scanning.
    ///
    /// `OPTIND` names the argument and this holds the character offset inside
    /// a cluster (`-ab`: `OPTIND=1`, offset 1 after `a`). It is not a shell
    /// variable, so it cannot be read or written by a script; when `OPTIND`
    /// changes the offset is discarded, which is what makes `OPTIND=1` a
    /// reset.
    getopts: GetoptsState,
    error: Option<ShellError>,
    exit_request: Option<Status>,
}

/// The `getopts` scan position inside a clustered argument.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct GetoptsState {
    /// The `OPTIND` this offset belongs to.
    pub(crate) optind: i64,
    /// How many bytes of that argument have been consumed.
    pub(crate) offset: usize,
    /// `Vars`'s `OPTIND` write counter when the offset was recorded.
    ///
    /// A script resets `getopts` by assigning `OPTIND`, and the *assignment*
    /// is the signal, not the value: `OPTIND=1` twice re-parses the same
    /// clustered argument, which a value comparison cannot see.
    pub(crate) generation: u64,
}

impl ShellState {
    /// State with `opts` and an empty variable store.
    pub fn new(opts: Opts) -> Self {
        Self {
            opts,
            vars: Vars::default(),
            traps: TrapTable::default(),
            aliases: AliasTable::new(),
            completions: complete::CompletionTable::default(),
            completion: complete::CompletionRun::default(),
            getopts: GetoptsState::default(),
            error: None,
            exit_request: None,
        }
    }

    /// The `getopts` scan position: `OPTIND`, the offset inside that argument,
    /// and the `OPTIND` write counter the offset was recorded at.
    pub(crate) fn getopts_position(&self) -> (i64, usize, u64) {
        (
            self.getopts.optind,
            self.getopts.offset,
            self.getopts.generation,
        )
    }

    /// Record the `getopts` scan position.
    pub(crate) fn set_getopts_position(&mut self, optind: i64, offset: usize, generation: u64) {
        self.getopts = GetoptsState {
            optind,
            offset,
            generation,
        };
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

    /// Apply a structured array-literal operand (`name=(…)`) of a declaration
    /// builtin, e.g. `readonly -a a=(1 2)`.
    ///
    /// The parser keeps array literals out of the ordinary word model (they
    /// are not field-split, and `[i]=v` elements are structural), so their
    /// elements live in a VM side channel. `declare`/`local` already consume
    /// it; `readonly`/`export` reach the runtime as a plain operand whose
    /// textual value would be `(1 2)`, which is why this seam exists.
    ///
    /// `shape` is the `-a`/`-A` request, and `scope` is where a new variable
    /// is created. `None` means the operand is not an array literal for this
    /// command. A failure has already been explained by the host; the runtime
    /// only needs to report it.
    fn apply_array_literal_operand(
        &mut self,
        operand: &[u8],
        shape: Option<ArrayShape>,
        scope: CreateScope,
    ) -> Option<Result<(), String>> {
        let _ = (operand, shape, scope);
        None
    }

    /// The exit status the shell had when it entered its `EXIT` trap.
    ///
    /// bash's `exit` with no argument inside the EXIT trap reuses the status
    /// the shell was already exiting with (`trap 'true; exit' EXIT; exit 3`
    /// exits 3, not 0), which is why the handler cannot just look at `$?`.
    /// `None` means "not running the EXIT trap".
    fn trap_saved_status(&self) -> Option<Status> {
        None
    }

    /// Whether `src` parses as shell code.
    ///
    /// `trap` validates its action string when it is registered, so
    /// `trap 'echo <' EXIT` reports the error immediately rather than at exit
    /// (and returns non-zero, which is what the suite expects). Parsing lives
    /// in `bsh-syntax`, which this crate does not depend on, so the VM answers
    /// through this seam. The default accepts everything.
    fn check_syntax(&mut self, src: &[u8]) -> bool {
        let _ = src;
        true
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

    /// Every place `name` resolves to, in the shell's search order.
    ///
    /// This is the *executor's* resolution, not a second opinion: the VM
    /// implements it once and uses it for both command execution and this
    /// query, so `type` cannot disagree with what running the name would do.
    /// `query.files_only` asks for executable files only (`type -p`/`-P`);
    /// `query.classify` prepends aliases and reserved words, which the
    /// executor never sees because they are handled before it.
    ///
    /// The default returns nothing, which makes `type` report "not found" on a
    /// host with no search order.
    fn resolve_command(&self, name: &[u8], query: ResolveQuery) -> Vec<CommandInfo> {
        let _ = (name, query);
        Vec::new()
    }

    /// Run `argv` as a built-in only, bypassing functions, aliases and `PATH`.
    ///
    /// This is `builtin NAME …`. `None` means `argv[0]` is not a built-in at
    /// all, which the caller reports; a built-in that fails returns its status.
    fn run_builtin_direct(&mut self, argv: &[Vec<u8>]) -> Option<Status> {
        let _ = argv;
        None
    }

    /// Run `argv` bypassing functions and aliases but keeping built-ins and
    /// `PATH`, as `command NAME …` does.
    ///
    /// `default_path` is `command -p`: search the standard utility path
    /// instead of `$PATH`. A special built-in run this way records its failure
    /// as `via_command`, because POSIX §2.8.1 note 1 exempts `command`.
    fn run_command_direct(&mut self, argv: &[Vec<u8>], default_path: bool) -> Status {
        let _ = (argv, default_path);
        Status::NOT_FOUND
    }

    /// Remove the shell function `name` (`unset -f`). Returns whether it
    /// existed; the function table lives in the executor, not in this crate.
    fn unset_function(&mut self, name: &[u8]) -> bool {
        let _ = name;
        false
    }

    /// The text `type` prints after `name is a shell function`.
    ///
    /// A function's body is the source it was parsed from, which only the
    /// executor keeps, so the caller asks for it by name. `None` means the
    /// host cannot render it and `type` prints the one-line form.
    fn function_source(&self, name: &[u8]) -> Option<Vec<u8>> {
        let _ = name;
        None
    }

    /// The shell as a candidate source for `compgen`.
    ///
    /// The candidates are shell state — variables, functions, aliases,
    /// built-ins, `PATH` — so the trait is implemented where that state lives
    /// and this crate never grows a second copy of it. `None` means the host
    /// cannot enumerate candidates, which makes `compgen` fail rather than
    /// offer a wrong list.
    fn completion_source(&mut self) -> Option<&mut dyn complete::CompletionSource> {
        None
    }

    /// Split and expand one `compgen -W` / `complete -W` word list.
    ///
    /// The default treats the list as blank-separated literal words: no
    /// expansion, no quoting, no `IFS`. That is honest for a host with no
    /// parser and is what the built-in unit tests use; the VM overrides it with
    /// the real quote-aware split and word expansion.
    fn completion_words(&mut self, wordlist: &[u8]) -> complete::WordList {
        let words = wordlist
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|word| !word.is_empty())
            .map(<[u8]>::to_vec)
            .collect();
        complete::WordList::Words(words)
    }

    /// Whether a `-W` word list parses.
    ///
    /// `complete -W '${' name` must be rejected when it is *defined*, not when
    /// a completion later runs (the suite pins status 2), so the syntax check
    /// has to happen without expanding anything. The default accepts every
    /// list, which matches a host without a parser.
    fn check_completion_words(&mut self, wordlist: &[u8]) -> bool {
        let _ = wordlist;
        true
    }

    /// Whether `name` is a shell function.
    ///
    /// `complete -F missing` is a usage error when it is defined, so the
    /// built-in asks before it saves the spec. The default knows no functions.
    fn is_function(&self, name: &[u8]) -> bool {
        let _ = name;
        false
    }

    /// Run shell function `name` with `argv` as its positional parameters.
    ///
    /// This is how a completion function is called: in the current shell, so
    /// the `COMPREPLY` it assigns is readable afterwards. `None` means there is
    /// no such function. The caller owns the completion variables
    /// (`COMP_LINE`, `COMP_WORDS`, …), because they are ordinary variables.
    fn call_function(&mut self, name: &[u8], argv: &[Vec<u8>]) -> Option<Status> {
        let _ = (name, argv);
        None
    }

    /// Run `argv` as a command and return its standard output and status.
    ///
    /// This is `compgen -C` / `complete -C`: the command's output *is* the
    /// candidate list, so it has to be captured rather than printed. `None`
    /// means the host cannot capture, which the built-in reports instead of
    /// letting the output escape unfiltered.
    fn capture_command(&mut self, argv: &[Vec<u8>]) -> Option<(Vec<u8>, Status)> {
        let _ = argv;
        None
    }
}
