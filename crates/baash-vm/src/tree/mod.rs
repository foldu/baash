//! The tree-walking evaluator: baash's execution engine.
//!
//! There is no second engine yet. A bytecode compiler and VM were sequenced to
//! follow this one (`--interp=vm`, differential-tested against it); that plan is
//! deferred until the bash surface is smaller, because a shell statement ends in
//! a word expansion or a syscall and dispatch is a small term next to either.
//! `DESIGN.md` §5 keeps the design; `docs/direction.md` §2 records the deferral.
//!
//! # What is here
//!
//! * [`Interp`] — one object that is a `baash_runtime::ShellHost`, a
//!   `baash_lang::Vars` and a `baash_lang::Shell`. Expansion needs
//!   `&mut S where S: Vars + Shell`, and re-entrant execution (`eval`,
//!   `source`, `$(…)`) needs `ShellHost`, so all three traits live on the one
//!   type that owns the state.
//! * [`Unwind`] — shell control flow as *one* explicit enum returned up the
//!   tree, never a `Result` at every node (DESIGN.md §5.3). That keeps
//!   `errexit`, `break n`, `return` and `exit` in one place instead of smeared
//!   over forty AST arms.
//! * `eval` — the AST walk; `command` — simple commands, search order and the
//!   per-command sequence; `redir` — redirections, left to right; `builtins` —
//!   executor-side built-ins; `cond` — `[[ … ]]`; `words` — the seams back
//!   into `baash-lang` and `baash-syntax`.
//! * `process` — the single foreground-wait path and the process built-ins
//!   `wait` and `kill`. There is no job control: background children are
//!   tracked by pid so `$!`, `wait` and `kill` work, and the job built-ins
//!   report `BAASH-E0302` instead of pretending.
//!
//! # `errexit`
//!
//! DESIGN.md §4.7: the POSIX text does not determine the behaviour, so one
//! algorithm is implemented and documented — the *ignore-context* model.
//! `errexit` never fires in the condition of `if`/`while`/`until`, in a
//! left-hand `&&`/`||` operand, under `!`, or in a non-final pipeline stage.
//! It fires after a whole and-or list whose last executed pipeline failed.
//! The private `Ctx::ignore_errexit` flag carries "this region is exempt".
//!
//! # Deferred
//!
//! Deliberately not implemented in this wave, so the next one inherits an
//! honest list rather than a surprise:
//!
//! * **`select`, `time`, `coproc`, `[[ x =~ re ]]`** — `select` and `time`
//!   parse as command words (`baash-syntax` documents that); `[[ … =~ … ]]`
//!   reports a deferred-feature diagnostic rather than a wrong answer.
//! * **`shopt -u expand_aliases`** — the flag is read at command position but
//!   only `shopt` could change it, and `shopt` is a stub.
//! * **`set -u`** — arrays themselves are implemented (storage, expansion,
//!   literals, element assignment, `declare`/`local`/`unset`), but `nounset` is
//!   not implemented at all, so a missing element under `set -u` behaves as
//!   empty rather than as an error.
//! * **Alias substitution is per command, not per input line.** `alias e=echo`
//!   followed by `e hi` on the *same* line expands here and does not in bash,
//!   whose parser buffers a whole line before executing anything. The
//!   substitution itself (including the "never into itself" rule and the
//!   trailing-blank rule) matches bash; see `alias.rs`.
//! * **Structural alias values** — `alias L='{'` and `alias f='for i in …; do`
//!   — expand only when the rest of the value completes a command *within the
//!   same simple command's text*. bash re-lexes the rest of the line, so a
//!   value that needs the following `; done` fails here with a syntax error.
//! * **`mapfile -C`/`-c`** (the per-record callback) is not implemented; the
//!   rest of `mapfile`/`readarray` is.
//! * **A shebang-less executable script** — POSIX says the shell retries an
//!   `ENOEXEC` file as a shell script; today it is status 126. That is what
//!   blocks the `$REPO_ROOT/spec/testdata/*.sh` trap cases together with
//!   `command -v`.
//! * **Assignment-before-command ordering** — the values of a simple command's
//!   prefix assignments are all expanded before any is applied, so
//!   `FOO=1 BAR=$FOO cmd` does not see `FOO` in `BAR`. bash applies each
//!   assignment before expanding the next.
//! * **`${x#pat}`, `${x%pat}`, `${x/pat/rep}`, `${!x}`, `${x@Q}`** — reported
//!   as `Unsupported` by `baash-lang` (its documented boundary).
//! * **Process substitution `<(…)`/`>(…)`** — `baash-lang`'s default callback
//!   errors, and this evaluator does not override it.
//! * **`set -x`/`set -v`, `umask`, `shopt`, `ulimit`** — registry stubs; see
//!   `builtins.rs` for the executor-side set that *is* implemented and why.
//!   `printf`/`test`/`[`/`pwd` are served from `PATH` until `baash-runtime`
//!   writes their bodies, which is where POSIX puts a regular built-in — which
//!   is also why `type -t [` reports `file` rather than `builtin`.
//! * **`~+N`/`~-N`** — bash's directory-stack forms of `~`, which this shell
//!   does not model; `~`, `~user`, `~+` and `~-` work.
//! * **Command substitution inside arithmetic** — `$(( $(cmd) + 1 ))` reports
//!   a syntax error, because arithmetic evaluation has no access to the
//!   command-substitution callback ([`crate::arith::IntVars`] is deliberately
//!   about integers only).
//! * **`$FUNCNAME`, `$PIPESTATUS`, `$RANDOM`, `$SECONDS`, `$LINENO`** — bash
//!   extensions the runtime does not define yet; `$PIPESTATUS` and `$FUNCNAME`
//!   are arrays, which are deferred above.
//! * **`set -i`/`PS1`** — the CLI marks the shell interactive when stdin is a
//!   terminal and reads lines from it; the prompt, editor and completion are
//!   `baash-repl`'s job and are not the evaluator's. History recording is split:
//!   the interactive loop calls [`Interp::initialize_history`] and
//!   [`Interp::flush_history`], and the ring itself lives in `baash-history`.
//! * **SIGPIPE** — Rust's runtime ignores it, so a built-in writing into a
//!   closed pipe sees `EPIPE` and reports status 1 instead of dying by signal.
//!   Children get `SIGPIPE` reset to `SIG_DFL` through `SpawnPlan`.
//! * **In-place `execve`** — `baash-sys` exposes no way to replace the process
//!   without `unsafe`, so `exec cmd` spawns, waits and exits. Observationally
//!   equivalent except for the pid and for redirect-only `exec`, which is
//!   applied to the shell's own descriptors (that case is exact).
//!
//! # One deliberate deviation: a failing special built-in
//!
//! POSIX §2.8.1 says a non-interactive shell shall exit when a *special*
//! built-in fails, and `baash_runtime::consequence` encodes exactly that. bash
//! (without `--posix`) continues instead, so `shift 5; echo after` prints
//! `after` in bash and exits 1 here. This follows the landed consequence table
//! and the project's rule of POSIX over bash where the two disagree, exactly as
//! it does for prefix assignments before special built-ins (DESIGN.md §3.3
//! Tier C). It is called out here because it is the one place a bash script
//! may notice.

use std::sync::Arc;

use baash_diag::{Diagnostic, SourceFile};
use baash_runtime::{History, Opts, ShellHost, ShellState, Status, consequence};
use baash_syntax::ArrayElement;
use baash_syntax::{Ast, NodeId, ParseOpts, ParseOutcome, parse_with};

/// bash's `$OSTYPE` for the host this binary was built for.
pub(crate) const OSTYPE: &str = if cfg!(target_os = "linux") {
    "linux-gnu"
} else if cfg!(target_os = "macos") {
    "darwin"
} else if cfg!(windows) {
    "msys"
} else {
    "unknown"
};

mod alias;
mod builtins;
mod command;
mod complete;
mod cond;
mod dirs;
mod eval;
mod limits;
mod mapfile;
mod process;
mod read;
mod redir;
mod scope;
mod traps;
mod umask;
mod words;
mod xtrace;

pub use eval::TrapKind;

/// Why evaluation stopped short of a plain status.
///
/// Shell control flow *is* exception semantics (DESIGN.md §5.3), so it is one
/// enum returned up the tree rather than a `Result` at every node: `set -e`,
/// `break n`, `continue n`, `return n` and `exit n` are all "unwind to a
/// handler", and the handlers live in exactly one place each (the loop, the
/// function call, the top level).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unwind {
    /// Ordinary completion with a status.
    Status(Status),
    /// `break n`: unwind to the nth enclosing loop. `n` is never 0.
    Break(u8),
    /// `continue n`: resume the nth enclosing loop.
    Continue(u8),
    /// `return n`: unwind to the enclosing function frame.
    Return(Status),
    /// `exit n`: unwind everything, then exit the shell.
    Exit(Status),
    /// A trap to run at the next safe point. Traps are executed directly at
    /// command boundaries, so nothing constructs this today; the variant is
    /// kept so the enum does not have to change shape later.
    Trap(TrapKind),
    /// Abandon the rest of the current input line.
    ///
    /// An interactive shell does not exit on an expansion error (`set -u`,
    /// `${x:?}`); it abandons the line and reads the next one. Only
    /// `eval_list` handles this, by skipping the items that begin on the line
    /// that failed.
    Discard(Status),
}

impl Unwind {
    /// The status this unwind carries, for `$?` and for a caller that has
    /// already decided to absorb it.
    pub fn status(self) -> Status {
        match self {
            Unwind::Status(status)
            | Unwind::Return(status)
            | Unwind::Exit(status)
            | Unwind::Discard(status) => status,
            Unwind::Break(_) | Unwind::Continue(_) | Unwind::Trap(_) => Status::SUCCESS,
        }
    }

    /// Whether this unwind still has to reach a handler.
    pub fn is_unwinding(self) -> bool {
        !matches!(self, Unwind::Status(_))
    }
}

/// A single piece of parsed source, kept alive because a function body's node
/// ids and every diagnostic span index it.
#[derive(Debug)]
pub(crate) struct ParsedChunk {
    pub(crate) ast: Ast,
    pub(crate) source: Arc<SourceFile>,
}

/// One frame of bash's call stack.
///
/// `FUNCNAME[i]` is the *name* of frame `i` (`main` for a script file,
/// `source` for a sourced file), `BASH_SOURCE[i]` is the file that frame came
/// from, and `BASH_LINENO[i]` is the line in the caller that started it. The
/// three arrays are emitted innermost-first, which is why the stack is stored
/// the other way round.
#[derive(Clone, Debug)]
pub(crate) struct CallFrame {
    /// `FUNCNAME[i]`: the function name, `source` or `main`.
    pub(crate) name: Vec<u8>,
    /// `BASH_SOURCE[i]`: the file the frame runs from.
    pub(crate) source: Vec<u8>,
    /// `BASH_LINENO[i]`: the caller's line that started this frame.
    pub(crate) line: usize,
}

/// A defined shell function: the chunk it was parsed from plus its body node.
#[derive(Clone, Debug)]
struct Function {
    name: Vec<u8>,
    chunk: Arc<ParsedChunk>,
    body: NodeId,
    /// `BASH_SOURCE` for frames of this function: the *definition* file.
    ///
    /// Stored rather than recomputed per call, because every call needs the
    /// bytes (`push_call` copies them into the frame and `refresh_call_vars`
    /// into the array) and the definition does not move.
    source: Vec<u8>,
}

/// Cached values for the special parameters that `baash_lang::Vars::get` has to
/// answer by value (`$?`, `$-`, `$$`, `$!`). `$0` is read straight from
/// `Vars::arg0` and `$#` is computed from `positional()`, as the contract says.
#[derive(Clone, Debug, Default)]
struct Specials {
    status: Vec<u8>,
    dash: Vec<u8>,
    pid: Vec<u8>,
    bang: Vec<u8>,
    /// `$SECONDS`, materialised once per command. A dynamic variable: the
    /// value exists only to be read, so it is not written into the variable
    /// table (`Vars::get` falls through to here).
    seconds: Vec<u8>,
    /// `$RANDOM`, same treatment.
    random: Vec<u8>,
}

/// Write `value` in decimal into `dest`, reusing its buffer.
///
/// `to_string()` allocates and the per-command specials are on the hot path of
/// every loop, so the number is formatted into a stack buffer and copied: two
/// digits on average, no allocation after the first command.
fn write_decimal(dest: &mut Vec<u8>, value: i64) {
    let mut digits = [0u8; 20];
    let mut at = digits.len();
    let negative = value < 0;
    let mut magnitude = value.unsigned_abs();
    loop {
        at -= 1;
        digits[at] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    if negative {
        at -= 1;
        digits[at] = b'-';
    }
    dest.clear();
    dest.extend_from_slice(&digits[at..]);
}

/// Where an `errexit` check is allowed to fire.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Ctx {
    /// True inside a condition, a left-hand `&&`/`||` operand, `!`'s pipeline
    /// or a non-final pipeline stage: regions where a failure is not fatal.
    pub(crate) ignore_errexit: bool,
}

/// The evaluator.
///
/// Not `Clone`: it owns the descriptor journal and the child bookkeeping, so a
/// copy would restore the same descriptors twice. A subshell is a
/// [`scope`](self::scope) over one interpreter, not a second interpreter.
#[derive(Debug)]
pub struct Interp {
    state: ShellState,
    /// LRU memo for `[[ =~ ]]`, so a loop does not recompile per evaluation
    /// (`docs/performance.md`). Its engine is the shell's engine, set from the
    /// command line before the first parse.
    regex_cache: baash_sys::regex::Cache,
    interactive: bool,
    /// Whether the locale counts characters rather than bytes.
    ///
    /// Off by default, and set only by [`Interp::detect_locale`], so a
    /// constructed shell never depends on the environment of whatever process
    /// built it — a test that silently changed behaviour with `LC_ALL` would
    /// be worse than no test.
    utf8_locale: bool,
    /// The source of the chunk being evaluated, for diagnostics.
    current: Arc<SourceFile>,
    functions: Vec<Function>,
    specials: Specials,
    /// Status of the last command substitution performed while expanding:
    /// `x=$(false)` must leave `$?` at 1.
    last_cmdsub: Option<Status>,
    /// The option set `SHELLOPTS` and `BASHOPTS` were last rendered from.
    ///
    /// Both are derived variables, so they are rebuilt whenever an option
    /// changes — but they were being rebuilt after *every* command, which put a
    /// pair of sorted, allocated strings on the hot path of a loop
    /// (`((i++))` 20k went from 15 ms to 40 ms, `docs/performance.md`). The
    /// fingerprint makes "did anything change" one comparison over the option
    /// bits.
    option_lists: Option<baash_runtime::Opts>,
    /// Set by a built-in that unwinds (`break`, `continue`, `return`), read by
    /// the command sequence after the built-in returns.
    pending: Option<Unwind>,
    /// Function-call nesting, so runaway recursion is diagnosed instead of
    /// overflowing the stack.
    depth: usize,
    /// Array literals in the command being executed, for the declaration
    /// built-ins (`local a=(1 2)`, `declare -A m=([k]=v)`).
    ///
    /// `exec_simple` fills this immediately before dispatching the built-in;
    /// `bi_declare` drains it. The literal is structural so it cannot be
    /// confused with the textual placeholder `argv` carries.
    array_literals: Vec<ArrayLiteral>,
    /// Set when expansion read a variable whose nameref chain is circular.
    ///
    /// `baash_lang::Vars::get` takes `&self` and answers `Option<&[u8]>`, so it
    /// cannot report the cycle itself. The flag records it and the next
    /// command boundary turns it into a fatal diagnostic, which is what makes
    /// `typeset -n a=b; typeset -n b=a; echo $a` stop instead of printing an
    /// empty string and looking like an unset variable.
    nameref_cycle: std::cell::Cell<bool>,
    /// State for `$RANDOM`, refreshed once per command in `sync_specials`.
    random: std::cell::Cell<u64>,
    /// `$SECONDS`' counter. It ticks once per command rather than reading the
    /// clock, which is what it did when the value lived in the variable table.
    seconds: u64,
    /// `$-` and the option set it was built from.
    ///
    /// Rebuilding `$-` walks the whole option table, and the option set changes
    /// far more rarely than commands run. The set is compared as a value:
    /// `Opts` is a plain `bool` bundle, so that is a memcmp rather than a walk
    /// of the table computing a fingerprint.
    dash: Option<(baash_runtime::Opts, Vec<u8>)>,
    /// Bytes a `read` pulled from a descriptor but could not give back.
    ///
    /// Only a descriptor that is not seekable can end up here; on a regular
    /// file the surplus is rewound with `lseek` instead, so no second reader
    /// ever sees a stale buffer.
    read_buffers: std::collections::BTreeMap<baash_sys::RawFd, Vec<u8>>,
    /// The signal dispositions the trap table implies (see `traps.rs`).
    signals: traps::SignalState,
    /// `run_named` nesting. The EXIT trap runs when this returns to zero, so
    /// `eval`, `source` and `$(…)` never trigger it early.
    run_depth: usize,
    /// True while a trap handler runs: a trap never re-enters itself.
    in_trap: bool,
    /// True once the shell has started finishing, so the EXIT trap runs once.
    exiting: bool,
    /// Children started with `&`, and the status of the ones already reaped
    /// (so `wait PID` can report a status after the fact).
    background: traps::Background,
    /// Armed by [`Interp::run_pipeline`] for a stage whose output is a pipe.
    ///
    /// A pipe is right for an external command, whose writer is a different
    /// process. A built-in or compound command would write from *this* process
    /// and deadlock once the pipe filled, so the dispatch site moves descriptor
    /// 1 to an unlinked temporary file before the stage writes anything and
    /// records it for the next stage.
    pipeline_buffer: Option<traps::PipelineBuffer>,
    /// Children spawned by pipeline stages that have not been waited for.
    pending_children: Vec<baash_sys::Child>,
    /// Set while the command being run is a `&` list item.
    ///
    /// Only an external command may go to the background: it is a process of
    /// its own, so the shell can keep going. Shell code would have to run
    /// concurrently with the shell in the same process, which is what `fork`
    /// was for, so it is rejected instead of being run in the foreground and
    /// called asynchronous.
    background_spawn: bool,
    /// Non-zero while a pipeline stage is being evaluated.
    ///
    /// A stage is a subshell, and bash does not fire the `ERR` trap inside one:
    /// the trap belongs to the pipeline, which fires once from the combined
    /// status at the next command boundary. Without this, `{ false; } | { false; }`
    /// fires twice — once per stage — where bash fires once.
    err_trap_suppressed: usize,
    /// Set for the top-level command of a pipeline stage that is nothing but
    /// an external command: it spawns and hands its child to `pending_children`
    /// instead of waiting, so the stages overlap.
    ///
    /// Consumed by the first spawn, so a *compound* stage's own children are
    /// waited for normally — inside a stage, `$?` after an external command is
    /// that command's status, as in a subshell.
    defer_next_spawn: bool,
    /// Whether the `ERR` trap has already run for the command that is
    /// finishing. bash fires it once per failing *command*: a `for`, `case` or
    /// brace group whose status is a child's failure does not fire it again,
    /// which is what `err_trap_reported` (reset as each command starts)
    /// encodes.
    err_trap_reported: bool,
    /// Set while a pipeline stage runs, whose `DEBUG` trap already fired in
    /// the parent: bash fires exactly one per stage, before the stage's
    /// redirections and before its own (subshell) trap table exists, so the
    /// commands inside a compound stage must not fire it again.
    debug_suppressed: usize,
    /// Set when a `DEBUG` handler asked to leave the shell while a pipeline
    /// was starting; `eval_pipeline` turns it into the unwind.
    debug_exit_request: Option<Status>,
    /// While a trap handler runs: the line number that handler's first line
    /// stands for, so `$LINENO` inside it names the command being trapped.
    line_base: Option<u32>,
    /// bash's call stack, which `FUNCNAME`, `BASH_SOURCE` and `BASH_LINENO`
    /// report (see [`CallFrame`]); innermost last.
    pub(crate) calls: Vec<CallFrame>,
    /// How many loops enclose the code being evaluated.
    ///
    /// `break` and `continue` that reach a list with no enclosing loop are a
    /// warning in bash, not an unwind: the list keeps running and the status
    /// is zero. `eval.rs` brackets each loop's condition and body with
    /// [`Interp::eval_in_loop`], which is what keeps a `break` inside a loop
    /// propagating while a top-level one is absorbed.
    loop_depth: usize,
    /// How many subshell scopes are open around the command being run.
    ///
    /// Zero at the top level. A committed redirection is journalled only while
    /// this is non-zero, because only a scope can undo one: at the top level the
    /// change is permanent, and keeping the saved descriptor would leak a
    /// descriptor per `exec >file`.
    scope_depth: usize,
    /// Redirections that `exec` committed to the shell's own descriptor table,
    /// newest last.
    ///
    /// A committed redirection is the only fd state that outlives a command, so
    /// this is what a subshell scope has to undo when its body ends
    /// (`tree/scope.rs`). Ordinary redirections are restored per command and
    /// never reach this list.
    committed: Vec<redir::SavedFds>,
    /// The pending exit status while the `EXIT` trap runs, so a bare `exit`
    /// inside it keeps that status (bash's rule).
    exit_trap_status: Option<Status>,
    /// Set when arithmetic read an unset variable under `nounset`.
    ///
    /// Arithmetic reads have no error channel of their own (an unset name is
    /// zero, POSIX XCU 2.6.4), so the read records the name here and the entry
    /// point that started the evaluation turns it into bash's "unbound
    /// variable" failure — fatal for a non-interactive shell.
    nounset_arith: Option<Vec<u8>>,
    /// `PATH` hits remembered by command name: bash's hash table.
    ///
    /// `hash` lists and edits it, `hash -r` empties it, and a remembered path
    /// is used without touching the filesystem again — which is why a program
    /// that appears earlier in `PATH` later does not take over until the table
    /// is cleared, and why a deleted program still resolves and then fails to
    /// spawn.
    path_cache: std::collections::BTreeMap<Vec<u8>, Vec<u8>>,
    /// A parsed empty chunk.
    ///
    /// `builtin`/`command` run a command that has no redirections and no
    /// prefix assignments of its own, but the shared invocation path takes a
    /// chunk (a redirection needs one for its spans and here-documents). One
    /// parsed empty chunk, built once, is cheaper and clearer than an
    /// `Option` threaded through every call site.
    empty_chunk: Arc<ParsedChunk>,
    /// True while the command produced by an alias substitution is being
    /// evaluated.
    ///
    /// The replacement text is fully expanded before it is parsed, so a second
    /// expansion pass over it would expand the value's first word against the
    /// alias that produced it — the recursion the expansion stack forbids.
    alias_eval_depth: usize,
    /// `$PIPESTATUS`: the exit status of every stage of the last pipeline, and
    /// of the last simple command (which is a one-stage pipeline). Empty at
    /// startup, as in bash.
    pipeline_status: std::collections::BTreeMap<i64, Vec<u8>>,
    /// `$LINENO`: the 1-based line of the command being evaluated, and the
    /// source file it belongs to. The offset cursor makes advancing through a
    /// chunk amortized O(1) instead of a scan from byte zero per node.
    line_source: Arc<SourceFile>,
    line_offset: usize,
    line_number: u32,
    lineno: Vec<u8>,
    /// The span of the node being evaluated, so a diagnostic raised by a
    /// builtin points at the command rather than at the start of the file.
    ///
    /// Builtins get `argv`, not a span; `0..0` in a diagnostic means "no
    /// location", and this is the best one the interpreter knows. A sourced
    /// file therefore reports the offending line instead of `file:1:1`.
    current_span: baash_diag::Span,
}

/// One `name=(…)` argument in a declaration command.
#[derive(Clone, Debug)]
pub(crate) struct ArrayLiteral {
    /// The variable name before `=`.
    pub(crate) name: Vec<u8>,
    /// The raw `name=(…)` text as written, which is the placeholder argument
    /// the built-in sees.
    pub(crate) raw: Vec<u8>,
    /// The parsed elements, in source order.
    pub(crate) elements: Vec<ArrayElement>,
    /// Whether this was `name+=(…)`.
    pub(crate) append: bool,
}

/// The recursion limit for shell functions. bash has none and eventually
/// crashes; a diagnostic is better than a crash.
const MAX_FUNCTION_DEPTH: usize = 512;

/// bash's default `PS1` for an interactive shell, as a value rather than the
/// rendered text: `\s-\v\$ ` is what `declare -p PS1` shows and what
/// `${PS1@P}` decodes. Measured against 5.3.20.
///
/// The *variable* keeps bash's value (the conformance suite reads it): the
/// interactive loop recognises this exact value as "the user has not set a
/// prompt" and paints the stock tide-like line itself
/// (`crates/baash/src/prompt_state.rs`). A user who sets `PS1` at all — in the
/// environment, an rc file or at the prompt — replaces it and gets exactly
/// their template through [`Interp::render_prompt`].
const DEFAULT_PS1: &[u8] = b"\\s-\\v\\$ ";

/// bash's default `PS2` for an interactive shell.
const DEFAULT_PS2: &[u8] = b"> ";

impl Interp {
    /// A fresh shell. `interactive` selects the POSIX §2.8.1 consequences and
    /// the diagnostic renderer.
    pub fn new(interactive: bool) -> Self {
        Self::with_opts(Opts::default(), interactive)
    }

    /// Choose the engine `[[ =~ ]]` compiles with.
    ///
    /// The interpreter starts on the platform default
    /// ([`baash_sys::Engine::default`]: the system engine on Unix, MinRX on
    /// Windows). The binary overrides it once, before the first parse, from
    /// `--regex-engine` or `BAASH_REGEX_ENGINE`; the caller is responsible for
    /// checking [`baash_sys::Engine::is_available`] first.
    pub fn set_regex_engine(&mut self, engine: baash_sys::Engine) {
        self.regex_cache.set_engine(engine);
    }

    /// Take the character-width model from the environment.
    ///
    /// POSIX precedence is `LC_ALL` > `LC_CTYPE` > `LANG`; a value naming
    /// UTF-8 (in any case) means characters, anything else means bytes. This is
    /// deliberately *not* done in [`Interp::new`]: a shell built by a test must
    /// not change behaviour with the developer's locale, and the binary is the
    /// only place that knows it is running for a user.
    pub fn detect_locale(&mut self) {
        self.utf8_locale = locale_is_utf8(
            std::env::var_os("LC_ALL")
                .or_else(|| std::env::var_os("LC_CTYPE"))
                .or_else(|| std::env::var_os("LANG"))
                .as_deref(),
        );
    }

    /// A fresh shell with explicit options (a profile preset, per the runtime
    /// contract: never an exhaustive `Opts` literal).
    pub fn with_opts(opts: Opts, interactive: bool) -> Self {
        let empty_source = Arc::new(SourceFile::new("<direct>", &b""[..]));
        let empty_ast = match parse_with(&empty_source, &ParseOpts::DEFAULT) {
            ParseOutcome::Ok(ast) => ast,
            // Empty input always parses; a failure here would be a bug in the
            // front end, not something a shell run can recover from.
            _ => panic!("the empty chunk must parse"),
        };
        let empty_chunk = Arc::new(ParsedChunk {
            ast: empty_ast,
            source: empty_source,
        });
        let mut interp = Self {
            state: ShellState::new(opts),
            regex_cache: baash_sys::regex::Cache::default(),
            interactive,
            current: Arc::new(SourceFile::new("<none>", &b""[..])),
            functions: Vec::new(),
            specials: Specials::default(),
            last_cmdsub: None,
            option_lists: None,
            pending: None,
            depth: 0,
            calls: Vec::new(),
            line_base: None,
            err_trap_reported: false,
            debug_suppressed: 0,
            nounset_arith: None,
            debug_exit_request: None,
            array_literals: Vec::new(),
            nameref_cycle: std::cell::Cell::new(false),
            random: std::cell::Cell::new(0x9E37_79B9_7F4A_7C15),
            seconds: 0,
            dash: None,
            read_buffers: std::collections::BTreeMap::new(),
            signals: traps::SignalState::default(),
            run_depth: 0,
            in_trap: false,
            exiting: false,
            background: traps::Background::new(),
            pipeline_buffer: None,
            pending_children: Vec::new(),
            background_spawn: false,
            err_trap_suppressed: 0,
            defer_next_spawn: false,
            loop_depth: 0,
            scope_depth: 0,
            committed: Vec::new(),
            exit_trap_status: None,
            path_cache: std::collections::BTreeMap::new(),
            empty_chunk,
            alias_eval_depth: 0,
            pipeline_status: std::collections::BTreeMap::new(),
            utf8_locale: false,
            line_source: Arc::new(SourceFile::new("<none>", &b""[..])),
            line_offset: 0,
            line_number: 1,
            lineno: b"1".to_vec(),
            current_span: 0..0,
        };
        // POSIX initializes `OPTIND` to 1 at shell startup, and `getopts`
        // counts from there; a script that never mentions it still needs the
        // value (`echo $OPTIND` is `1`, not empty).
        let _ = interp.state.vars.assign_scalar(b"OPTIND", b"1");
        // `PS4` has a default in bash, not merely a fallback in the tracer: a
        // script that never sets it still traces with `+ `, while `unset PS4`
        // makes the prefix empty. The environment import can replace it.
        let _ = interp.state.vars.assign_scalar(b"PS4", b"+ ");
        // `SECONDS` is *not* seeded: it is a dynamic variable, so the value
        // lives in `Specials` and `Vars::get` answers for it (a seed here would
        // shadow the ticking value for the life of the shell).
        // POSIX's default field separators, and the option list a script can
        // read (`SHELLOPTS` is non-empty in every real shell).
        let _ = interp.state.vars.assign_scalar(b"IFS", b" \t\n");
        // Both option lists are derived from the table (`Opts::shellopts`),
        // and both are readonly for scripts; `refresh_option_lists` keeps them
        // current after every command.
        let shellopts = interp.state.opts.shellopts();
        let _ = interp.state.vars.assign_scalar(b"SHELLOPTS", &shellopts);
        let bashopts = interp.state.opts.bashopts();
        let _ = interp.state.vars.assign_scalar(b"BASHOPTS", &bashopts);
        let _ = interp.state.vars.mark_readonly(b"SHELLOPTS");
        let _ = interp.state.vars.mark_readonly(b"BASHOPTS");
        // Read-only identity variables bash provides: the environment cannot
        // override them, and a script cannot assign them.
        if let Some(uid) = baash_sys::os::own_uid() {
            let uid = uid.to_string().into_bytes();
            let _ = interp.state.vars.assign_scalar(b"UID", &uid);
            let _ = interp.state.vars.assign_scalar(b"EUID", &uid);
            let _ = interp.state.vars.mark_readonly(b"UID");
            let _ = interp.state.vars.mark_readonly(b"EUID");
        }
        // Unset rather than zero when the platform cannot answer: `$PPID` is
        // either the parent's id or not there at all.
        if let Some(ppid) = baash_sys::parent_pid() {
            let ppid = ppid.to_string().into_bytes();
            let _ = interp.state.vars.assign_scalar(b"PPID", &ppid);
            let _ = interp.state.vars.mark_readonly(b"PPID");
        }
        let _ = interp
            .state
            .vars
            .assign_scalar(b"HOSTNAME", &baash_sys::hostname());
        let _ = interp
            .state
            .vars
            .assign_scalar(b"OSTYPE", crate::tree::OSTYPE.as_bytes());
        // `$?`, `$$` and `$-` must answer before the first command runs.
        interp.sync_specials();
        // bash turns history recording on for an interactive shell (the
        // `set -o history` option), before the startup file runs so that file
        // can turn it off. A non-interactive shell records nothing either way.
        if interactive {
            interp.state.opts.history = true;
            // bash starts an interactive shell in emacs editing mode, and the
            // startup file can change it (`set -o vi`). The option is what
            // `test -o emacs` and `set -o` report, so it is set here rather
            // than implied by the line editor.
            interp.state.opts.emacs = true;
            // An interactive bash also has default prompt variables, before the
            // environment and the startup file can replace them: measured
            // against 5.3.20, `declare -p PS1` in `bash -i` is
            // `PS1="\\s-\\v\\\$ "` and `PS2="> "`. `${PS1@P}` and the prompt
            // therefore decode the same template, and the conformance case
            // that reads `$PS1` from an interactive shell sees bash's value.
            let _ = interp.state.vars.assign_scalar(b"PS1", DEFAULT_PS1);
            let _ = interp.state.vars.assign_scalar(b"PS2", DEFAULT_PS2);
            // `emacs` and `history` were turned on just above; the option lists
            // must include them before the first prompt is drawn.
            interp.refresh_option_lists();
        }
        interp
    }

    /// The facts `PS1`/`PS2` read while the shell draws a prompt.
    ///
    /// This is [`baash_lang::Shell::prompt_context`] with the two differences a
    /// prompt has and `${x@P}` does not:
    ///
    /// * `\!` is the number the line being edited *will* get — the ring's next
    ///   number, because the line has not been recorded yet — while `${x@P}`
    ///   runs with the line already in the ring and reports its number.
    /// * `\l` and the terminal width come from the terminal on standard
    ///   input, which is the one the editor paints on.
    ///
    /// The background-child count behind `\j`, `$PWD`, `$HOME` and the
    /// user/host names come from the shell's own state, not the process
    /// environment.
    pub fn repl_prompt_context(&self) -> baash_lang::prompt::PromptContext {
        let mut context = <Self as baash_lang::Shell>::prompt_context(self);
        context.history_number = self.state.history.next_number();
        if let Some((columns, _)) = baash_sys::window_size_fd(0) {
            context.columns = columns;
        }
        context
    }

    /// Put the last command's status back and refresh `$?`.
    ///
    /// The interactive loop runs `PROMPT_COMMAND` between commands, which
    /// changes the status the way any other command would; bash keeps the
    /// status the user last saw, and this restores it.
    pub fn restore_status(&mut self, status: baash_runtime::Status) {
        self.state.vars.set_last_status(status);
        self.sync_specials();
    }

    /// Whether the shell is still using its built-in `PS1`.
    ///
    /// The value is bash's default, which is what makes it distinguishable
    /// from a user's template without tracking assignments: an interactive
    /// shell starts with it, and the environment and the startup file
    /// overwrite it when they define `PS1`. The REPL uses this to choose the
    /// stock tide-like prompt (with its live git facts) instead of rendering
    /// the template, and to leave its legacy `[branch]` suffix off that
    /// prompt, which already contains the branch.
    pub fn uses_default_prompt(&self) -> bool {
        self.state
            .vars
            .scalar(b"PS1")
            .is_some_and(|value| value == DEFAULT_PS1)
    }

    /// Render a user-defined `PS1`/`PS2` template the way the interactive
    /// prompt needs it.
    ///
    /// This is the shared expander with the shell as its second-pass host, so
    /// `shopt promptvars` decides whether `$PWD` and `$(date)` are expanded.
    /// The last status is put back afterwards: bash's prompt expansion does
    /// not change `$?` even though it may run commands (measured against
    /// 5.3.20 with `PS1='$(false)$ '`).
    ///
    /// The **stock** prompt is not rendered here. It used to be, but its
    /// counters need a `git status` and this method runs on the prompt path;
    /// the binary now composes it from a worker's cached facts
    /// (`crates/baash/src/prompt_state.rs`, `docs/prompt-watch.md`). The
    /// `PS1` *variable* is untouched either way, so `${PS1@P}` and `$PS1`
    /// keep the value the conformance suite expects.
    pub fn render_prompt(&mut self, template: &[u8]) -> baash_lang::prompt::ExpandedPrompt {
        let context = self.repl_prompt_context();
        let status = self.state.vars.last_status();
        let rendered = baash_lang::prompt::expand_with_variables(template, &context, self);
        self.state.vars.set_last_status(status);
        self.sync_specials();
        rendered
    }

    /// Import the process environment as exported shell variables, the way
    /// every shell starts. Bytes are preserved exactly.
    pub fn import_environment(&mut self) {
        let mut entries: Vec<Vec<u8>> = Vec::new();
        for (name, value) in std::env::vars_os() {
            let mut entry = baash_sys::os::bytes_from_os(&name);
            entry.push(b'=');
            entry.extend_from_slice(&baash_sys::os::bytes_from_os(&value));
            entries.push(entry);
        }
        // Windows environment variables are case-insensitive and keep
        // whatever case the parent used, so the names every script asks for
        // arrive as `Path` and `Pathext`. Shell variables are case-sensitive:
        // without this `$PATH` is unset, the shell falls back to a POSIX
        // default that names no directory on this machine, and no command is
        // ever found.
        #[cfg(windows)]
        for entry in &mut entries {
            let Some(name_end) = entry.iter().position(|&byte| byte == b'=') else {
                continue;
            };
            let wanted = [b"PATH".as_slice(), b"PATHEXT"]
                .into_iter()
                .find(|wanted| entry[..name_end].eq_ignore_ascii_case(wanted));
            if let Some(wanted) = wanted {
                entry[..wanted.len()].copy_from_slice(wanted);
            }
        }
        self.state.vars.import_envp(&entries);
        // bash applies exported option lists from the environment: that is how
        // `export SHELLOPTS` hands tracing to a child shell. The imported
        // values are then recomputed, because this shell may have started with
        // different defaults.
        for (name, shopt) in [(b"SHELLOPTS".as_slice(), false), (b"BASHOPTS", true)] {
            let Some(value) = self.state.vars.scalar(name).map(<[u8]>::to_vec) else {
                continue;
            };
            for option in value.split(|&byte| byte == b':') {
                if option.is_empty() {
                    continue;
                }
                let _ = if shopt {
                    Opts::set_shopt_by_name(&mut self.state.opts, option, true)
                } else {
                    self.state.opts.set_by_name(option, true)
                };
            }
        }
        self.refresh_option_lists();
    }

    /// Give an interactive shell its history database.
    ///
    /// A terminal REPL passes `persist = true` and gets the user's database
    /// (`baash_history::default_path`). A piped or `-c` interactive shell gets
    /// none: an interactive conformance run must not depend on, or write to,
    /// the user's data directory.
    ///
    /// A database that cannot be opened is reported once and the shell keeps
    /// its in-memory ring, because losing recall is better than refusing to
    /// start.
    pub fn initialize_history(&mut self, persist: bool) {
        if !self.interactive || !persist {
            return;
        }
        let Some(path) = baash_history::default_path() else {
            return;
        };
        match History::open(&path, baash_history::RECENT_DEPTH) {
            Ok(history) => self.state.history = history,
            Err(error) => {
                eprintln!("baash: history: cannot open {}: {error}", path.display());
            }
        }
    }

    /// Wait for every queued history write to reach the database.
    ///
    /// The editor calls this on exit; the `-i -c` and piped-interactive paths
    /// call it here, because no editor exists to hold the ring.
    pub fn flush_history(&mut self) {
        self.state.history.flush();
    }

    /// Set `$0`.
    pub fn set_arg0(&mut self, arg0: impl Into<Vec<u8>>) {
        self.state.vars.set_arg0(arg0.into());
    }

    /// Set the positional parameters (`$1`, `$2`, …).
    pub fn set_positional(&mut self, argv: Vec<Vec<u8>>) {
        self.state.vars.set_positional(argv);
    }

    /// `$?`.
    pub fn status(&self) -> Status {
        self.state.vars.last_status()
    }

    /// Parse and run one chunk, reporting diagnostics against `name`.
    ///
    /// This is what `-c`, a script file, a `$(…)` body, `eval` and `source`
    /// all funnel through, so there is exactly one place where "a chunk of
    /// shell text becomes commands" happens.
    ///
    /// The outermost call on a non-interactive shell is also where the EXIT
    /// trap runs: it is inside the chunk's unwind, so `exit 42` in the handler
    /// still decides the process's status. Nested calls (`eval`, `source`,
    /// a command substitution) never reach that point.
    pub fn run_named(&mut self, name: &str, src: &[u8]) -> Status {
        self.run_depth += 1;
        let status = self.run_named_inner(name, src);
        self.run_depth -= 1;
        if self.run_depth == 0 && !self.interactive && !self.exiting {
            return self.finish(status);
        }
        status
    }

    fn run_named_inner(&mut self, name: &str, src: &[u8]) -> Status {
        let unwind = self.run_chunk_unwind(name, src);
        let status = self.absorb(unwind);
        self.set_status(status);
        // Leave the cached specials agreeing with the final state, so a caller
        // that reads `$?` after `run_named` sees this chunk's status.
        self.sync_specials();
        status
    }

    /// Push one call frame and republish the introspection arrays.
    ///
    /// `FUNCNAME`/`BASH_SOURCE`/`BASH_LINENO` are ordinary indexed arrays in
    /// bash (writable, and *unset* at top level, which is why `set -u` makes
    /// `${FUNCNAME}` an error there), so they are rewritten from the stack
    /// rather than answered through a hook: `Vars::get` returns a borrow and
    /// these values are synthesised.
    pub fn push_call(&mut self, name: &[u8], source: &[u8], line: usize) {
        self.calls.push(CallFrame {
            name: name.to_vec(),
            source: source.to_vec(),
            line,
        });
        self.refresh_call_vars();
    }

    /// Drop the innermost frame.
    pub fn pop_call(&mut self) {
        self.calls.pop();
        self.refresh_call_vars();
    }

    fn refresh_call_vars(&mut self) {
        if self.calls.is_empty() {
            for name in [b"FUNCNAME".as_slice(), b"BASH_SOURCE", b"BASH_LINENO"] {
                let _ = self.state.vars.unset(name);
            }
            return;
        }
        // `replace_indexed` installs the whole array per name: one table
        // operation instead of `declare_shape` + `clear_array` + one
        // `set_indexed` per element, each of which scanned the variable table.
        // The three arrays are the entire cost of a function call
        // (`docs/performance.md`).
        let mut names: Vec<Vec<u8>> = Vec::with_capacity(self.calls.len());
        let mut sources: Vec<Vec<u8>> = Vec::with_capacity(self.calls.len());
        let mut lines: Vec<Vec<u8>> = Vec::with_capacity(self.calls.len());
        for frame in self.calls.iter().rev() {
            names.push(frame.name.clone());
            sources.push(frame.source.clone());
            lines.push(frame.line.to_string().into_bytes());
        }
        let _ = self.state.vars.replace_indexed(b"FUNCNAME", names);
        let _ = self.state.vars.replace_indexed(b"BASH_SOURCE", sources);
        let _ = self.state.vars.replace_indexed(b"BASH_LINENO", lines);
    }

    /// Parse and evaluate one chunk, returning the unwind *unabsorbed*.
    ///
    /// `eval` and `source` need the raw unwind: their text behaves as if it
    /// appeared at the call site, so a `return` in `eval` returns from the
    /// enclosing function and a `break` in either leaves the caller's loop.
    /// Only the top-level entry point absorbs.
    pub(crate) fn run_chunk_unwind(&mut self, name: &str, src: &[u8]) -> Unwind {
        let source = Arc::new(SourceFile::new(name, src));
        let opts = self.parse_opts();
        let chunk = match parse_with(&source, &opts) {
            ParseOutcome::Ok(ast) => Arc::new(ParsedChunk {
                ast,
                source: source.clone(),
            }),
            ParseOutcome::NeedMoreInput { open, .. } => {
                let previous = std::mem::replace(&mut self.current, source);
                self.diagnose(&Diagnostic::error(
                    baash_syntax::codes::NEED_MORE_INPUT,
                    open,
                    "unexpected end of file: the input is incomplete",
                ));
                self.current = previous;
                self.set_status(Status(2));
                return Unwind::Status(Status(2));
            }
            ParseOutcome::Failed(diagnostics) => {
                let previous = std::mem::replace(&mut self.current, source);
                for diagnostic in &diagnostics {
                    self.diagnose(diagnostic);
                }
                self.current = previous;
                self.set_status(Status(2));
                return Unwind::Status(Status(2));
            }
        };

        // bash rejects a malformed `[[ … ]]` while parsing, before any command
        // in the chunk runs; the evaluator parses conditionals lazily, so the
        // pass is done here, at the one entry point every chunk goes through.
        if let Some((span, message)) = self.validate_conditionals(&chunk) {
            // The chunk's own source: `self.current` is only switched to it
            // below, and a diagnostic rendered against the previous file has
            // no line to point at.
            self.report_in(&chunk.source, crate::codes::CONDITIONAL, span, message);
            self.set_status(Status(2));
            return Unwind::Status(Status(2));
        }
        let previous = std::mem::replace(&mut self.current, source);
        let unwind = self.eval_node(&chunk, chunk.ast.root, Ctx::default());
        self.current = previous;
        unwind
    }

    /// Parse every `[[ … ]]` body in `chunk`, returning the first failure.
    ///
    /// `extglob` is tried both ways: the chunk can turn it on or off before the
    /// conditional runs, and this pass cannot know which. Accepting either
    /// setting keeps a valid conditional from being rejected; only a body that
    /// fails under both is a syntactic error.
    fn validate_conditionals(
        &mut self,
        chunk: &Arc<ParsedChunk>,
    ) -> Option<(baash_diag::Span, String)> {
        let extglob = self.state.opts.extglob;
        let mut stack = vec![chunk.ast.root];
        while let Some(id) = stack.pop() {
            if let baash_syntax::NodeKind::TestCommand { body } = &chunk.ast.node(id).kind {
                let body = body.clone();
                let mut last = None;
                let mut accepted = false;
                for attempt in [extglob, !extglob] {
                    match cond::validate(self, chunk, body.clone(), attempt) {
                        Ok(()) => {
                            accepted = true;
                            break;
                        }
                        Err(error) => last = Some(error),
                    }
                }
                if !accepted {
                    return last;
                }
            }
            stack.extend(chunk.ast.children(id));
        }
        None
    }

    /// Turn a top-level unwind into an exit status.
    ///
    /// An `Exit` is re-requested through `ShellState` rather than merely
    /// converted: `eval 'exit 3'` and `source` run a nested chunk, and the
    /// outer command sequence must still see the exit request. Without this,
    /// `exit` inside `eval` would only end the `eval`.
    pub(crate) fn absorb(&mut self, unwind: Unwind) -> Status {
        match unwind {
            Unwind::Status(status) => status,
            Unwind::Exit(status) => {
                self.state.request_exit(status);
                status
            }
            // `return` outside a function exits (or returns from) the current
            // script, which is what bash does for a script's top level.
            Unwind::Return(status) => status,
            Unwind::Break(_) | Unwind::Continue(_) => {
                // A warning, not an error: bash prints one and carries on.
                self.warn_control_flow("break/continue is only meaningful inside a loop");
                Status::SUCCESS
            }
            Unwind::Trap(_) | Unwind::Discard(_) => Status::SUCCESS,
        }
    }

    /// The parse profile, derived from the shell's own options.
    fn parse_opts(&self) -> ParseOpts {
        ParseOpts {
            posix_mode: self.state.opts.posix_mode,
            extglob: self.state.opts.extglob,
            ..ParseOpts::DEFAULT
        }
    }

    /// Take the array literal whose raw text is `operand`, if this command
    /// has one. Declaration built-ins call this for each operand.
    pub(crate) fn take_array_literal(&mut self, operand: &[u8]) -> Option<ArrayLiteral> {
        let index = self
            .array_literals
            .iter()
            .position(|literal| literal.raw == operand)?;
        Some(self.array_literals.remove(index))
    }

    pub(crate) fn set_status(&mut self, status: Status) {
        self.state.vars.set_last_status(status);
    }

    /// Refresh the cached special parameters. Called before expansion, because
    /// `Vars::get` cannot allocate and `$?` must be current.
    /// Recompute `SHELLOPTS` and `BASHOPTS` from the live option table.
    ///
    /// Both are derived variables: bash recomputes them whenever an option
    /// changes, and a script cannot assign them. The write has to bypass the
    /// readonly attribute they carry, which is why it goes through
    /// [`baash_runtime::Vars::refresh_derived`].
    fn refresh_option_lists(&mut self) {
        if self.option_lists == Some(self.state.opts) {
            return;
        }
        self.option_lists = Some(self.state.opts);
        let shellopts = self.state.opts.shellopts();
        let _ = self.state.vars.refresh_derived(b"SHELLOPTS", &shellopts);
        let bashopts = self.state.opts.bashopts();
        let _ = self.state.vars.refresh_derived(b"BASHOPTS", &bashopts);
    }

    pub(crate) fn sync_specials(&mut self) {
        self.refresh_option_lists();
        write_decimal(
            &mut self.specials.status,
            i64::from(self.state.vars.last_status().exit_code()),
        );
        // `$RANDOM` is a fresh value per command: bash gives a new one on
        // every read, but one per command is enough for `$RANDOM` to look
        // random. `$SECONDS` counts from shell start; a per-command tick is
        // close enough to the second-resolution value bash reports.
        //
        // Both are dynamic variables, so they are materialised here and read
        // back through `Vars::get`, which knows their names. Writing them into
        // the variable table instead cost a name lookup and a copy per
        // command, and nothing but a read ever wanted the value.
        self.seconds = self.seconds.saturating_add(1);
        write_decimal(
            &mut self.specials.seconds,
            i64::try_from(self.seconds).unwrap_or(i64::MAX),
        );
        let random = self
            .random
            .get()
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.random.set(random);
        write_decimal(
            &mut self.specials.random,
            i64::from(((random >> 33) % 32768) as u32),
        );
        // `-i` has no `set -o` row (it is an invocation mode, not a toggle),
        // so `$-` has to add it here.
        let stale = !matches!(&self.dash, Some((cached, _)) if *cached == self.state.opts);
        if stale {
            let mut flags = self.state.opts.dash_flags();
            if self.interactive && !flags.contains(&b'i') {
                flags.push(b'i');
            }
            self.dash = Some((self.state.opts, flags));
        }
        let (_, flags) = self.dash.as_ref().expect("just built");
        self.specials.dash.clear();
        self.specials.dash.extend_from_slice(flags);
        if self.specials.pid.is_empty() {
            write_decimal(
                &mut self.specials.pid,
                i64::from(self.state.vars.shell_pid()),
            );
        }
        self.specials.bang.clear();
        if let Some(pid) = self.state.vars.last_bg_pid() {
            write_decimal(&mut self.specials.bang, i64::from(pid));
        }
    }

    /// Point `$LINENO` at `offset` in `source`, the chunk being evaluated.
    ///
    /// Node spans arrive mostly in source order, so the line counter advances
    /// from the previous offset; a jump backwards (a loop body, a function
    /// called twice) rescans from the top rather than reporting a stale line.
    pub(crate) fn set_line(&mut self, source: &Arc<SourceFile>, offset: usize) {
        if let Some(base) = self.line_base {
            // A trap handler is a chunk of its own, but bash reports the line
            // of the command it is running *for*: `trap 'echo $LINENO' DEBUG`
            // sees the next command's line, not line 1 of the handler text.
            let end = offset.min(source.bytes().len());
            let newlines = source.bytes()[..end]
                .iter()
                .filter(|&&byte| byte == b'\n')
                .count();
            self.line_number = base.saturating_add(newlines as u32);
            self.line_offset = end;
            let text = self.line_number.to_string();
            if self.lineno != text.as_bytes() {
                self.lineno = text.into_bytes();
            }
            return;
        }
        if !Arc::ptr_eq(&self.line_source, source) {
            self.line_source = source.clone();
            self.line_offset = 0;
            self.line_number = 1;
        }
        if offset < self.line_offset {
            self.line_offset = 0;
            self.line_number = 1;
        }
        let bytes = source.bytes();
        let end = offset.min(bytes.len());
        if end > self.line_offset {
            let newlines = bytes[self.line_offset..end]
                .iter()
                .filter(|&&byte| byte == b'\n')
                .count() as u32;
            self.line_number += newlines;
            self.line_offset = end;
        }
        let text = self.line_number.to_string();
        if self.lineno != text.as_bytes() {
            self.lineno = text.into_bytes();
        }
    }

    /// `$LINENO` as an integer, for arithmetic.
    pub(crate) fn line_number(&self) -> i64 {
        i64::from(self.line_number)
    }

    /// Note an unset arithmetic read, when `nounset` is on.
    fn note_nounset(&mut self, name: &[u8]) {
        if self.state.opts.nounset && self.nounset_arith.is_none() {
            self.nounset_arith = Some(name.to_vec());
        }
    }

    /// Turn a recorded `nounset` read into bash's fatal "unbound variable".
    ///
    /// A non-interactive shell exits 1 (`set -u; (( x++ ))` ends the script),
    /// an interactive one abandons the line.
    pub(crate) fn arith_nounset(&mut self) -> Option<Unwind> {
        let name = self.nounset_arith.take()?;
        let message = format!("{}: unbound variable", String::from_utf8_lossy(&name));
        let len = message.len();
        self.report(crate::codes::UNBOUND, len - 17..len, message);
        Some(self.abort_expansion())
    }

    /// Record one pipeline's per-stage statuses for `$PIPESTATUS`.
    pub(crate) fn set_pipeline_status(&mut self, statuses: &[Status]) {
        self.pipeline_status.clear();
        for (index, status) in statuses.iter().enumerate() {
            self.pipeline_status
                .insert(index as i64, status.exit_code().to_string().into_bytes());
        }
    }

    /// Report a diagnostic at `span`, with an evaluator code (2xx).
    pub(crate) fn report(
        &mut self,
        code: baash_diag::ErrCode,
        span: baash_diag::Span,
        message: impl Into<String>,
    ) {
        self.diagnose(&Diagnostic::error(code, span, message.into()));
    }

    /// Report a diagnostic against an explicit source, with a span into it.
    pub(crate) fn report_in(
        &mut self,
        source: &Arc<SourceFile>,
        code: baash_diag::ErrCode,
        span: baash_diag::Span,
        message: impl Into<String>,
    ) {
        self.diagnose_in(source, &Diagnostic::error(code, span, message.into()));
    }

    /// Render and print a diagnostic against an explicit source.
    ///
    /// `[[ … ]]` runs its own sub-parser over a chunk's bytes while
    /// `self.current` may still be some other file, and an error reported
    /// against the wrong source has no line to point at: the caller that knows
    /// which file a span belongs to names it here.
    pub(crate) fn diagnose_in(&mut self, source: &Arc<SourceFile>, diagnostic: &Diagnostic) {
        let rendered = if self.interactive {
            baash_diag::render_compact(diagnostic, source)
        } else {
            diagnostic.render_line_with_help(source)
        };
        let mut line = rendered.into_bytes();
        line.push(b'\n');
        let _ = self.write_err(&line);
    }

    /// Print a non-fatal control-flow warning.
    ///
    /// bash warns, with a `bash: line N:` prefix, when `break`/`continue`
    /// leaves no loop or `return` leaves no function or sourced file; the
    /// command itself is not an error.
    pub(crate) fn warn_control_flow(&mut self, message: &str) {
        self.diagnose(&Diagnostic::warning(
            crate::codes::CONTROL_FLOW,
            0..0,
            message.to_owned(),
        ));
    }

    /// The POSIX §2.8.1 table, applied. Returns the unwind the caller must
    /// honour: `Continue` becomes an ordinary status.
    pub(crate) fn apply_consequence(
        &mut self,
        error: baash_runtime::ShellError,
        status: Status,
    ) -> Unwind {
        match consequence(&error, self.interactive) {
            baash_runtime::Action::Continue => Unwind::Status(status),
            baash_runtime::Action::Exit(code) => Unwind::Exit(code),
        }
    }

    /// Report an expansion error and classify it.
    ///
    /// A failed glob is the one expansion error that must not escalate: with
    /// `shopt -s failglob`, bash fails the command and a script carries on
    /// (`shopt -s failglob; echo *.ZZ; echo status=$?` prints `status=1`),
    /// while a `${x:?}` error does end a non-interactive shell. Splitting the
    /// kind here is what keeps those two apart.
    pub(crate) fn expansion_failed(&mut self, error: baash_lang::Error) -> Unwind {
        self.diagnose(&error.diagnostic());
        // bash fails the *command* for these, and a script continues: `a='bad
        // name'; echo ${!a}; echo status=$?` prints `status=1` rather than
        // ending the shell, and so does `${x:}`.
        if matches!(
            error.kind,
            baash_lang::ErrorKind::GlobNoMatch { .. }
                | baash_lang::ErrorKind::BadSubstitution { .. }
        ) {
            return Unwind::Status(Status(1));
        }
        self.abort_expansion()
    }

    /// The consequence of an expansion error, once it has been reported.
    ///
    /// bash does not exit an interactive shell on one: it abandons the line
    /// (`set -u; echo a; echo $x; echo b` prints `a` and runs the *next* line)
    /// and keeps the status of the failure. A non-interactive shell treats it
    /// as fatal (POSIX XCU 2.8.1).
    pub(crate) fn abort_expansion(&mut self) -> Unwind {
        if self.interactive {
            self.state.clear_error();
            return Unwind::Discard(Status(1));
        }
        self.apply_consequence(baash_runtime::ShellError::Expansion, Status(1))
    }

    /// `errexit`, applied at the one place a whole and-or list finishes.
    ///
    /// The `ERR` trap fires here too, whether or not `errexit` is on: the
    /// ignore-context rules are the same for both (DESIGN.md §4.7), which is
    /// why they share this one call site.
    pub(crate) fn check_errexit(&mut self, status: Status, ctx: Ctx, negated: bool) -> Unwind {
        if status.is_success() || ctx.ignore_errexit || negated {
            return Unwind::Status(status);
        }
        if !self.err_trap_reported
            && let Some(exit) = self.run_err_trap(status)
        {
            return Unwind::Exit(exit);
        }
        if !self.state.opts.errexit {
            return Unwind::Status(status);
        }
        Unwind::Exit(status)
    }
}

impl baash_lang::Vars for Interp {
    fn get(&self, name: &[u8]) -> Option<&[u8]> {
        if name == b"LINENO" {
            return Some(&self.lineno);
        }
        // `$SECONDS`, `$RANDOM` and `$_` are dynamic: their bytes are refreshed
        // once per command and live beside the other special parameters, unless
        // the script assigned the name itself, in which case the variable table
        // wins (it is what `unset` and `declare` act on).
        if name == b"SECONDS" && self.state.vars.scalar(name).is_none() {
            return Some(&self.specials.seconds);
        }
        if name == b"RANDOM" && self.state.vars.scalar(name).is_none() {
            return Some(&self.specials.random);
        }
        // `$PIPESTATUS` is its element zero, like any array's scalar form.
        if name == b"PIPESTATUS" {
            return self.pipeline_status.get(&0).map(Vec::as_slice);
        }
        if let [special] = name {
            match *special {
                b'?' => return Some(&self.specials.status),
                b'-' => return Some(&self.specials.dash),
                b'$' => return Some(&self.specials.pid),
                b'!' => return Some(&self.specials.bang),
                b'0' => return Some(self.state.vars.arg0()),
                // `$#` is computed from `positional()`, per the contract.
                _ => {}
            }
        }
        let value = self.state.vars.scalar(name);
        if value.is_none() && self.state.vars.is_nameref_cycle(name) {
            self.nameref_cycle.set(true);
        }
        value
    }

    fn positional(&self) -> &[Vec<u8>] {
        &self.state.vars.positional().argv
    }

    fn nounset(&self) -> bool {
        self.state.opts.nounset
    }

    fn utf8(&self) -> bool {
        self.utf8_locale
    }

    fn set(&mut self, name: &[u8], value: Vec<u8>) -> Result<(), baash_lang::SetError> {
        match self.state.vars.assign_scalar(name, &value) {
            Ok(()) => Ok(()),
            Err(baash_runtime::Error::InvalidName { .. }) => Err(baash_lang::SetError::InvalidName),
            Err(_) => Err(baash_lang::SetError::ReadOnly),
        }
    }

    fn attributes(&self, name: &[u8]) -> Vec<u8> {
        let Some(var) = self.state.vars.lookup(name) else {
            return Vec::new();
        };
        let mut letters = Vec::new();
        match &var.value {
            baash_runtime::VarValue::Indexed(_) => letters.push(b'a'),
            baash_runtime::VarValue::Assoc(_) => letters.push(b'A'),
            _ => {}
        }
        if var.attrs.exported {
            letters.push(b'x');
        }
        if var.attrs.readonly {
            letters.push(b'r');
        }
        if var.attrs.integer {
            letters.push(b'i');
        }
        if var.attrs.nameref {
            letters.push(b'n');
        }
        letters.sort();
        letters
    }

    fn names(&self) -> Vec<Vec<u8>> {
        self.state
            .vars
            .visible()
            .into_iter()
            .map(|(name, _)| name.to_vec())
            .collect()
    }

    fn array(&self, name: &[u8]) -> Option<baash_lang::ArrayValue<'_>> {
        if name == b"PIPESTATUS" {
            return Some(baash_lang::ArrayValue::Indexed(&self.pipeline_status));
        }
        match self.state.vars.value(name)? {
            baash_runtime::VarValue::Indexed(elements) => {
                Some(baash_lang::ArrayValue::Indexed(elements))
            }
            baash_runtime::VarValue::Assoc(pairs) => {
                Some(baash_lang::ArrayValue::Assoc(pairs.as_slice()))
            }
            _ => None,
        }
    }
}

/// The `\l` value: the basename of the terminal device on standard input.
///
/// Measured against bash 5.3.20 on a pty: `PS1='\l '` shows `0` for
/// `/dev/pts/0` — the *basename*, not the whole device path — and `tty` when
/// standard input is not a terminal. `/proc/self/fd/0` is the std-only way to
/// ask which device it is.
fn current_tty() -> Vec<u8> {
    if !baash_sys::is_tty(0) {
        return b"tty".to_vec();
    }
    #[cfg(unix)]
    if let Some(name) = std::fs::read_link("/proc/self/fd/0")
        .ok()
        .and_then(|path| path.file_name().map(baash_sys::os::bytes_from_os))
    {
        return name;
    }
    b"tty".to_vec()
}

impl baash_lang::Shell for Interp {
    fn home_dir(&mut self, user: &[u8]) -> Option<Vec<u8>> {
        // `~user` asks the platform's user database. A bare `~` uses `$HOME`
        // first and only lands here when it is unset, in which case bash asks
        // the same database for the user running the shell — not for the
        // empty login name. An unknown login stays literal.
        if user.is_empty() {
            return baash_sys::os::own_home();
        }
        baash_sys::os::user_home(user)
    }

    fn brace_expansion(&self) -> bool {
        // `set -B`/`set +B`, and `set -o braceexpand`. The other stages are
        // not switchable, which is why this is the pipeline's only option.
        self.state.opts.braceexpand
    }

    /// The facts `${x@P}` and the `PS1` renderer read.
    ///
    /// The default resolves user, host, cwd and time from the environment; a
    /// shell adds what only it knows — the *logical* `PWD` (which can differ
    /// from `std::env::current_dir()` after `cd` through a symlink), the
    /// command counter behind `\#`, and the background-child count behind
    /// `\j` (there is no job table).
    fn prompt_context(&self) -> baash_lang::prompt::PromptContext {
        let mut context = baash_lang::prompt::PromptContext::from_env();
        let vars = &self.state.vars;
        if let Some(value) = vars.scalar(b"PWD") {
            context.cwd = value.to_vec();
        }
        if let Some(value) = vars.scalar(b"OLDPWD") {
            context.previous_cwd = value.to_vec();
        }
        if let Some(value) = vars.scalar(b"HOME") {
            context.home = value.to_vec();
        } else if let Some(home) = baash_sys::dirs::fallback_home() {
            // A native Windows process starts without `HOME`; the profile
            // directory is what `\w` and `~` should abbreviate to there.
            context.home = home;
        }
        if let Some(value) = vars.scalar(b"USER") {
            context.user = value.to_vec();
        }
        if let Some(value) = vars.scalar(b"HOSTNAME") {
            context.host = value.to_vec();
        }
        // No job table: `\j` counts the children this shell still owns.
        context.job_count = self.background.len() as u32;
        // `\!`: bash's `${x@P}` reports the number of the line already in the
        // ring, not the next one the prompt will show (`docs/contracts.md`).
        context.history_number = self.state.history.next_number().saturating_sub(1);
        // `\#`: the prompt for line N and `${x@P}` inside line N both report
        // N + 1, so one counter serves both (see [`ShellState::command_number`]).
        context.command_number = self.state.command_number.saturating_add(1);
        context.prompt_vars = self.state.opts.promptvars;
        context.last_failed = self.state.vars.last_status().exit_code() != 0;
        context.root = baash_lang::prompt::is_root();
        context.tty = current_tty();
        context
    }

    fn pattern_flags(&self) -> baash_pattern::Flags {
        // `utf8` stays at its default (byte units): expansion forces the C
        // locale, the same policy `${#x}` uses. What comes from `shopt` is
        // the pattern *dialect*.
        baash_pattern::Flags {
            extglob: self.state.opts.extglob,
            nocasematch: self.state.opts.nocasematch,
            pathname: false,
            utf8: self.utf8_locale,
        }
    }

    fn command_substitution(
        &mut self,
        cmd: &baash_lang::CommandSub,
    ) -> Result<Vec<u8>, baash_lang::Error> {
        match self.capture_substitution(&cmd.source) {
            Ok((bytes, status)) => {
                self.set_status(status);
                self.last_cmdsub = Some(status);
                self.sync_specials();
                Ok(bytes)
            }
            Err(message) => Err(baash_lang::Error::new(
                baash_lang::ErrorKind::Host { message },
                cmd.span.clone(),
            )),
        }
    }

    fn arithmetic(&mut self, expr: &baash_lang::Arithmetic) -> Result<Vec<u8>, baash_lang::Error> {
        let source = self.expand_arithmetic(&expr.source, &expr.span)?;
        let outcome = crate::arith::eval(&source, self);
        if let Some(name) = self.nounset_arith.take() {
            // bash's "unbound variable": the expansion error path makes it
            // fatal for a script, `exit 1`.
            return Err(baash_lang::Error::new(
                baash_lang::ErrorKind::Host {
                    message: format!("{}: unbound variable", String::from_utf8_lossy(&name)),
                },
                expr.span.clone(),
            ));
        }
        match outcome {
            Ok(value) => Ok(value.to_string().into_bytes()),
            Err(message) => Err(baash_lang::Error::new(
                baash_lang::ErrorKind::Host { message },
                expr.span.clone(),
            )),
        }
    }

    fn pathname(
        &mut self,
        pattern: &baash_lang::Field,
    ) -> Result<Option<Vec<Vec<u8>>>, baash_lang::Error> {
        if self.state.opts.noglob {
            return Ok(None);
        }
        // `nocaseglob`, not `nocasematch`, folds case in pathnames; the rest
        // of the dialect (`extglob`) comes from the shared flags.
        //
        // bash documents one part of `GLOBIGNORE` that is not a match filter:
        // when it is set, `.` and `..` are always ignored. Full `GLOBIGNORE`
        // filtering is not implemented, but honouring that rule keeps
        // `shopt -u globskipdots` from reporting them when the variable is set.
        let globignore: Vec<Vec<u8>> = self
            .state
            .vars
            .scalar(b"GLOBIGNORE")
            .map(split_globignore)
            .unwrap_or_default();
        let ignore_set = !globignore.is_empty();
        let flags = baash_pattern::Flags {
            nocasematch: self.state.opts.nocaseglob,
            ..self.pattern_flags()
        };
        let options = crate::glob::Options {
            flags,
            // Setting `GLOBIGNORE` implies `dotglob` in bash: a leading dot
            // stops hiding a file from `*`, while `.`/`..` stay skipped.
            dotglob: self.state.opts.dotglob || ignore_set,
            globstar: self.state.opts.globstar,
            globskipdots: self.state.opts.globskipdots || ignore_set,
        };
        let expand = |pattern: &baash_lang::Field| {
            crate::glob::expand(&pattern.bytes, &pattern.quoted, &options, &self.state.vfs)
        };
        // `GLOBIGNORE` is a match filter, not another pattern: a file is kept
        // only when no list entry matches it. Every entry is matched against
        // the whole generated pathname with `fnmatch` (no `FNM_PATHNAME`, but
        // `*` still stops at `/`), so `*.txt` hides `one.txt` and leaves
        // `foo/two.txt` alone while `src/*` can hide a file in a directory.
        let kept = |matches: Vec<Vec<u8>>| -> Vec<Vec<u8>> {
            if globignore.is_empty() {
                return matches;
            }
            // An entry without `/` only hides a match in the directory being
            // expanded, so `*.txt` cannot hide `foo/two.txt`; an entry that
            // names a directory (`src/*`) is matched against the whole path.
            let compiled: Vec<_> = globignore
                .iter()
                .map(|pattern| {
                    (
                        baash_pattern::compiled(pattern, flags),
                        !pattern.contains(&b'/'),
                    )
                })
                .collect();
            matches
                .into_iter()
                .filter(|path| {
                    let nested = path.contains(&b'/');
                    !compiled.iter().any(|(pattern, name_only)| {
                        !(*name_only && nested) && pattern.matches(path)
                    })
                })
                .collect()
        };
        match expand(pattern) {
            Some(matches) => {
                let matches = kept(matches);
                if matches.is_empty() && ignore_set {
                    // Every candidate was ignored: this is a non-match, not a
                    // word with no fields.
                    return if self.state.opts.nullglob {
                        Ok(Some(Vec::new()))
                    } else if self.state.opts.failglob {
                        Err(baash_lang::Error::new(
                            baash_lang::ErrorKind::GlobNoMatch {
                                pattern: pattern.bytes.clone(),
                            },
                            0..0,
                        ))
                    } else {
                        Ok(None)
                    };
                }
                Ok(Some(matches))
            }
            None if self.state.opts.nullglob => Ok(Some(Vec::new())),
            None if self.state.opts.failglob => Err(baash_lang::Error::new(
                baash_lang::ErrorKind::GlobNoMatch {
                    pattern: pattern.bytes.clone(),
                },
                0..0,
            )),
            None => Ok(None),
        }
    }
}

impl Interp {
    /// Expand the text between `$((` and `))` before evaluating it.
    ///
    /// bash runs the ordinary word stages over that text first (parameter
    /// expansion, command substitution, backticks, quote removal), which is
    /// what makes `$(( ${j:-5} + 1 ))`, `$(( $(echo 1) + 2 ))`,
    /// `$((1 + $((2 + 3)) + 4))` and `` $((`echo 1` + 2)) `` work. The text is
    /// re-read as one double-quoted word, so a space never splits it and the
    /// same [`baash_lang`] pipeline does the expansion. `'1'` therefore stays a
    /// syntax error, as bash reports it.
    fn expand_arithmetic(
        &mut self,
        source: &[u8],
        span: &baash_diag::Span,
    ) -> Result<Vec<u8>, baash_lang::Error> {
        if !source.iter().any(|&byte| byte == b'$' || byte == b'`') {
            return Ok(source.to_vec());
        }
        let mut quoted = Vec::with_capacity(source.len() + 4);
        quoted.push(b'"');
        for &byte in source {
            if byte == b'"' {
                quoted.push(b'\\');
            }
            quoted.push(byte);
        }
        quoted.push(b'"');
        let opts = baash_syntax::ParseOpts::default();
        let Ok(word) = baash_syntax::parse_word(&quoted, &opts) else {
            // A text the word parser rejects is left alone: arithmetic will
            // report the syntax error itself.
            return Ok(source.to_vec());
        };
        let expanded = baash_lang::expand_assignment(&word, self)?;
        let _ = span;
        Ok(expanded)
    }
}

impl crate::arith::IntVars for Interp {
    fn get(&mut self, name: &[u8]) -> i64 {
        // `LINENO` is a shell parameter, not a variable: `(( x = LINENO ))`
        // must read the line the arithmetic is on.
        if name == b"LINENO" {
            return self.line_number();
        }
        // A name of digits is a positional parameter, not a variable:
        // `$(( $1 + 1 ))` reads `$1`.
        if !name.is_empty() && name.iter().all(u8::is_ascii_digit) && name[0] != b'0' {
            let index: usize = std::str::from_utf8(name)
                .ok()
                .and_then(|text| text.parse().ok())
                .unwrap_or(0);
            // `Positional::get` is one-based: `get(0)` is `None` because `$0`
            // is not a positional parameter.
            return self
                .state
                .vars
                .positional()
                .get(index)
                .and_then(crate::arith::parse_int)
                .unwrap_or(0);
        }
        // `$SECONDS` and `$RANDOM` are dynamic variables, so they are not in
        // the variable table; arithmetic reads them from the same place
        // `Vars::get` does.
        if name == b"SECONDS" && self.state.vars.value(name).is_none() {
            return crate::arith::parse_int(&self.specials.seconds).unwrap_or(0);
        }
        if name == b"RANDOM" && self.state.vars.value(name).is_none() {
            return crate::arith::parse_int(&self.specials.random).unwrap_or(0);
        }
        // An array decays to element zero in arithmetic, just as `${a}` does.
        let bytes = match self.state.vars.value(name) {
            Some(baash_runtime::VarValue::Indexed(elements)) => elements.get(&0).map(Vec::as_slice),
            Some(baash_runtime::VarValue::Assoc(pairs)) => pairs
                .iter()
                .find(|(key, _)| key.as_slice() == b"0")
                .map(|(_, value)| value.as_slice()),
            _ => self.state.vars.scalar(name),
        };
        match bytes {
            Some(bytes) => crate::arith::parse_int(bytes).unwrap_or(0),
            None => {
                self.note_nounset(name);
                0
            }
        }
    }

    fn set(&mut self, name: &[u8], value: i64) -> Result<(), String> {
        self.state
            .vars
            .assign_scalar(name, value.to_string().as_bytes())
            .map_err(|error| error.message())
    }

    fn get_text(&mut self, name: &[u8]) -> Option<Vec<u8>> {
        if !name.is_empty() && name.iter().all(u8::is_ascii_digit) && name[0] != b'0' {
            let index: usize = std::str::from_utf8(name)
                .ok()
                .and_then(|text| text.parse().ok())
                .unwrap_or(0);
            return self.state.vars.positional().get(index).map(<[u8]>::to_vec);
        }
        match self.state.vars.value(name) {
            Some(baash_runtime::VarValue::Indexed(elements)) => elements.get(&0).cloned(),
            Some(baash_runtime::VarValue::Assoc(pairs)) => pairs
                .iter()
                .find(|(key, _)| key.as_slice() == b"0")
                .map(|(_, value)| value.clone()),
            _ => self.state.vars.scalar(name).map(<[u8]>::to_vec),
        }
    }

    fn get_index_text(&mut self, name: &[u8], index: i64) -> Option<Vec<u8>> {
        let value = match self.state.vars.shape(name) {
            Some(baash_runtime::ArrayShape::Associative) => self
                .state
                .vars
                .element_assoc(name, index.to_string().as_bytes()),
            _ => self.state.vars.element_indexed(name, index),
        };
        value.map(<[u8]>::to_vec)
    }

    fn get_index(&mut self, name: &[u8], index: i64) -> i64 {
        let value = match self.state.vars.shape(name) {
            Some(baash_runtime::ArrayShape::Associative) => self
                .state
                .vars
                .element_assoc(name, index.to_string().as_bytes()),
            _ => self.state.vars.element_indexed(name, index),
        };
        match value {
            Some(value) => crate::arith::parse_int(value).unwrap_or(0),
            None => {
                self.note_nounset(name);
                0
            }
        }
    }

    fn set_index(&mut self, name: &[u8], index: i64, value: i64) -> Result<(), String> {
        let text = value.to_string();
        match self.state.vars.shape(name) {
            // An associative array's subscript is a string key, even when it
            // looks like a number (`assoc[0]=42`).
            Some(baash_runtime::ArrayShape::Associative) => self
                .state
                .vars
                .set_assoc(name, index.to_string().as_bytes(), text.as_bytes())
                .map_err(|error| error.message()),
            _ => self
                .state
                .vars
                .set_indexed(name, index, text.as_bytes())
                .map_err(|error| error.message()),
        }
    }
}

impl ShellHost for Interp {
    fn state(&mut self) -> &mut ShellState {
        &mut self.state
    }

    fn run_source(&mut self, src: &[u8]) -> Status {
        self.run_named("<eval>", src)
    }

    fn write_out(&mut self, bytes: &[u8]) -> Status {
        use std::io::Write;
        let mut stdout = std::io::stdout().lock();
        match stdout.write_all(bytes).and_then(|()| stdout.flush()) {
            Ok(()) => Status::SUCCESS,
            // EPIPE is the reader going away, not the shell failing. Rust
            // ignores SIGPIPE, so the write returns an error instead of
            // killing the shell.
            Err(_) => Status(1),
        }
    }

    fn write_err(&mut self, bytes: &[u8]) -> Status {
        use std::io::Write;
        let mut stderr = std::io::stderr().lock();
        match stderr.write_all(bytes).and_then(|()| stderr.flush()) {
            Ok(()) => Status::SUCCESS,
            Err(_) => Status(1),
        }
    }

    fn is_interactive(&self) -> bool {
        self.interactive
    }

    fn unset_array_element(&mut self, operand: &[u8]) -> Option<Status> {
        // `name[subscript]`: `unset` re-parses the operand as an assignment
        // word, so the subscript can contain expansion and, for an
        // associative array, is a string key rather than arithmetic.
        let open = operand.iter().position(|&byte| byte == b'[')?;
        if !operand.ends_with(b"]") {
            // `unset 'a[1'` is not an element operand. Bash treats the
            // malformed text as a name that matches nothing and succeeds.
            return Some(Status::SUCCESS);
        }
        let name = operand[..open].to_vec();
        let subscript = operand[open + 1..operand.len() - 1].to_vec();
        // `unset 'a[]'` is a no-op in bash 5.3.20, for both shapes. It is not
        // an empty arithmetic subscript: the quoted spelling `unset 'a[""]'`
        // is the one that addresses element zero.
        if subscript.is_empty() {
            return Some(Status::SUCCESS);
        }
        // `unset 'a[@]'`/`unset 'a[*]'` is not an element operation at all:
        // bash clears an indexed array (keeping its shape), leaves an
        // associative array alone, rejects a scalar and ignores an absent
        // name. Compare the raw text so a *quoted* key `a["@"]` still means the
        // literal key.
        if subscript == b"@" || subscript == b"*" {
            let result = self.state.vars.unset_all_elements(&name);
            return Some(match result {
                Ok(()) => Status::SUCCESS,
                Err(error) => {
                    self.report(crate::codes::ASSIGNMENT, 0..0, error.message());
                    Status(1)
                }
            });
        }
        let word = match baash_syntax::parse_word(&subscript, &self.parse_opts()) {
            Ok(word) => word,
            // `a[1 + 2]` and `a[a b]` are one assignment subscript but more
            // than one lexer token, which `parse_word` refuses. Re-parse them
            // through the assignment grammar, which stitches the span back
            // together exactly as it does for `a[1 + 2]=v`.
            Err(_) => match self.parse_subscript_word(&subscript) {
                Some(word) => word,
                None => {
                    self.report(
                        crate::codes::ASSIGNMENT,
                        0..0,
                        format!("{}: bad array subscript", String::from_utf8_lossy(&name)),
                    );
                    return Some(Status(1));
                }
            },
        };
        let text = match baash_lang::expand_assignment(&word, self) {
            Ok(text) => text,
            Err(error) => {
                self.diagnose(&error.diagnostic());
                return Some(Status(1));
            }
        };
        let result = match self.state.vars.shape(&name) {
            Some(baash_runtime::ArrayShape::Associative) => {
                self.state.vars.unset_element_assoc(&name, &text)
            }
            _ => match crate::arith::eval(&text, self) {
                Ok(index) => self.state.vars.unset_element_indexed(&name, index),
                Err(message) => {
                    self.report(crate::codes::ARITHMETIC, 0..0, message);
                    return Some(Status(1));
                }
            },
        };
        Some(match result {
            Ok(()) => Status::SUCCESS,
            Err(error) => {
                self.report(crate::codes::ASSIGNMENT, 0..0, error.message());
                Status(1)
            }
        })
    }

    fn assign_variable_element(
        &mut self,
        name: &[u8],
        subscript: &[u8],
        value: &[u8],
    ) -> Result<(), String> {
        // `printf -v 'a[subscript]'` assigns like `a[subscript]=value`: the
        // subscript is expanded as an assignment word and then resolved by the
        // element-assignment path, which knows about shapes and namerefs.
        let text = self
            .expand_subscript(subscript)
            .map_err(|()| format!("{}: bad array subscript", String::from_utf8_lossy(name)))?;
        self.assign_element(name, &text, false, value)
            .map_err(|failure| failure.message().to_string())
    }

    fn variable_is_set(&mut self, operand: &[u8]) -> Option<bool> {
        // `test -v name` / `test -v 'name[subscript]'`. A bare name follows
        // bash's array rules: an indexed array asks about element 0, an
        // associative array about key "0".
        let Some(open) = operand.iter().position(|&byte| byte == b'[') else {
            let shape = self.state.vars.shape(operand);
            return Some(match shape {
                Some(baash_runtime::ArrayShape::Indexed) => {
                    self.state.vars.element_indexed(operand, 0).is_some()
                }
                Some(baash_runtime::ArrayShape::Associative) => {
                    self.state.vars.element_assoc(operand, b"0").is_some()
                }
                None => self.state.vars.is_set(operand),
            });
        };
        if !operand.ends_with(b"]") {
            return Some(false);
        }
        let name = &operand[..open];
        let subscript = &operand[open + 1..operand.len() - 1];
        if subscript.is_empty() {
            return Some(false);
        }
        let text = match self.expand_subscript(subscript) {
            Ok(text) => text,
            Err(()) => return Some(false),
        };
        let all = text.as_slice() == b"@" || text.as_slice() == b"*";
        match self.state.vars.shape(name) {
            Some(baash_runtime::ArrayShape::Associative) => {
                // bash answers false for `h[@]`/`h[*]` on an associative array
                // unless the key is literally `@`/`*`, which this spelling does
                // not ask for.
                if all {
                    return Some(false);
                }
                Some(self.state.vars.element_assoc(name, &text).is_some())
            }
            _ => {
                if all {
                    return Some(self.state.vars.array_len(name) > 0);
                }
                match crate::arith::eval(&text, self) {
                    Ok(index) => Some(self.state.vars.element_indexed(name, index).is_some()),
                    Err(_) => Some(false),
                }
            }
        }
    }

    fn apply_array_literal_operand(
        &mut self,
        operand: &[u8],
        shape: Option<baash_runtime::ArrayShape>,
        scope: baash_runtime::CreateScope,
    ) -> Option<Result<(), String>> {
        let literal = self.take_array_literal(operand)?;
        let variable = literal.name.clone();
        if scope == baash_runtime::CreateScope::Local
            && let Err(error) = self.state.vars.declare_local(&variable)
        {
            return Some(Err(error.message()));
        }
        if let Some(shape) = shape
            && let Err(error) = self.state.vars.declare_shape(&variable, shape)
        {
            return Some(Err(error.message()));
        }
        Some(self.assign_array_literal(&variable, &literal.elements, literal.append, shape))
    }

    fn trap_saved_status(&self) -> Option<Status> {
        self.exit_trap_status
    }

    fn check_syntax(&mut self, src: &[u8]) -> bool {
        // `trap` validates its action when it is registered, so the error is
        // reported immediately rather than at exit.
        let opts = self.parse_opts();
        matches!(
            baash_syntax::parse_bytes(src, &opts),
            baash_syntax::ParseOutcome::Ok(_)
        )
    }

    fn diagnose(&mut self, diagnostic: &Diagnostic) {
        let source = self.current.clone();
        // `0..0` is the diagnostic's way of saying "no location": a builtin
        // knows its arguments, not where they were written, so point at the
        // command being evaluated. Without this a sourced file reported every
        // builtin error as `file:1:1`.
        if diagnostic.span.start == 0
            && diagnostic.span.end == 0
            && self.current_span.end > self.current_span.start
        {
            let mut located = diagnostic.clone();
            located.span = self.current_span.clone();
            self.diagnose_in(&source, &located);
            return;
        }
        self.diagnose_in(&source, diagnostic);
    }

    fn resolve_command(
        &self,
        name: &[u8],
        query: baash_runtime::ResolveQuery,
    ) -> Vec<baash_runtime::CommandInfo> {
        let mut out = Vec::new();
        self.each_candidate(name, query, &mut |candidate| {
            out.push(self.candidate_info(candidate));
            query.all
        });
        out
    }

    fn run_builtin_direct(&mut self, argv: &[Vec<u8>]) -> Option<Status> {
        // `builtin NAME …`: the registry only, no function, alias or `PATH`
        // lookup. A name that is not a builtin at all is the caller's to
        // report, so `None` rather than a status.
        let name = argv.first()?;
        let def = baash_runtime::lookup_builtin(name)?;
        let resolution = self.registry_body(def);
        Some(self.run_direct(resolution, argv))
    }

    fn run_command_direct(&mut self, argv: &[Vec<u8>], default_path: bool) -> Status {
        let Some(name) = argv.first().cloned() else {
            return Status::SUCCESS;
        };
        let query = baash_runtime::ResolveQuery {
            skip_functions: true,
            default_path,
            ..baash_runtime::ResolveQuery::EXECUTION
        };
        let mut found = None;
        self.each_candidate(&name, query, &mut |candidate| {
            found = Some(candidate);
            false
        });
        let Some(resolution) = found else {
            return self.command_not_found(&name).status();
        };
        if let command::Resolution::External(path) = &resolution
            && !name.contains(&b'/')
        {
            self.path_cache.insert(name.clone(), path.clone());
        }
        self.run_direct(resolution, argv)
    }

    fn unset_function(&mut self, name: &[u8]) -> bool {
        let before = self.functions.len();
        self.functions.retain(|function| function.name != name);
        self.functions.len() != before
    }

    fn function_source(&self, name: &[u8]) -> Option<Vec<u8>> {
        self.function_body(name)
    }

    fn completion_source(&mut self) -> Option<&mut dyn baash_runtime::complete::CompletionSource> {
        // `complete.rs` implements the trait for the interpreter, so `compgen`
        // asks the shell itself: there is no second name table to disagree
        // with execution.
        Some(self)
    }

    fn completion_words(&mut self, wordlist: &[u8]) -> baash_runtime::complete::WordList {
        self.completion_wordlist(wordlist)
    }

    fn check_completion_words(&mut self, wordlist: &[u8]) -> bool {
        self.completion_wordlist_parses(wordlist)
    }

    fn is_function(&self, name: &[u8]) -> bool {
        self.function_index(name).is_some()
    }

    fn call_function(&mut self, name: &[u8], argv: &[Vec<u8>]) -> Option<Status> {
        self.completion_call(name, argv)
    }

    fn call_function_captured(
        &mut self,
        name: &[u8],
        argv: &[Vec<u8>],
    ) -> Option<(Vec<u8>, Status)> {
        self.completion_call_captured(name, argv)
    }

    fn capture_command(&mut self, argv: &[Vec<u8>]) -> Option<(Vec<u8>, Status)> {
        self.completion_capture(argv)
    }
}

/// Whether a locale name means UTF-8.
///
/// Free-standing and pure, so it can be tested without touching the process
/// environment: `LC_ALL=C.UTF-8`, `en_US.utf8` and `de_DE.UTF-8@euro` are
/// UTF-8, while `C`, `POSIX` and an empty value are not.
fn locale_is_utf8(locale: Option<&std::ffi::OsStr>) -> bool {
    let Some(locale) = locale else {
        return false;
    };
    let text = locale.to_string_lossy().to_ascii_lowercase();
    text.contains("utf-8") || text.contains("utf8")
}

/// Split `GLOBIGNORE` on `:`, but not on a colon inside a bracket expression
/// (`[[:alnum:]]` is one pattern, not three).
fn split_globignore(value: &[u8]) -> Vec<Vec<u8>> {
    let mut patterns = Vec::new();
    let mut current = Vec::new();
    let mut depth = 0usize;
    for &byte in value {
        match byte {
            b'[' => depth += 1,
            b']' => depth = depth.saturating_sub(1),
            b':' if depth == 0 => {
                if !current.is_empty() {
                    patterns.push(std::mem::take(&mut current));
                }
                continue;
            }
            _ => {}
        }
        current.push(byte);
    }
    if !current.is_empty() {
        patterns.push(current);
    }
    patterns
}

#[cfg(test)]
mod locale_tests {
    use super::locale_is_utf8;
    use std::ffi::OsStr;

    #[test]
    fn recognises_utf8_locale_names() {
        for utf8 in [
            "C.UTF-8",
            "c.utf8",
            "en_US.UTF-8",
            "en_US.utf8",
            "de_DE.UTF-8@euro",
        ] {
            assert!(
                locale_is_utf8(Some(OsStr::new(utf8))),
                "{utf8} should mean characters"
            );
        }
        for bytes in ["C", "POSIX", "", "en_US.ISO-8859-1"] {
            assert!(
                !locale_is_utf8(Some(OsStr::new(bytes))),
                "{bytes} should mean bytes"
            );
        }
        assert!(!locale_is_utf8(None), "an unset locale means bytes");
    }
}
