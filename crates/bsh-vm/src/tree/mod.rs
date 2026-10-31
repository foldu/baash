//! The tree-walking evaluator: ßhell's execution engine.
//!
//! There is no second engine yet. A bytecode compiler and VM were sequenced to
//! follow this one (`--interp=vm`, differential-tested against it); that plan is
//! deferred until the bash surface is smaller, because a shell statement ends in
//! a word expansion or a syscall and dispatch is a small term next to either.
//! `DESIGN.md` §5 keeps the design; `docs/direction.md` §2 records the deferral.
//!
//! # What is here
//!
//! * [`Interp`] — one object that is a `bsh_runtime::ShellHost`, a
//!   `bsh_lang::Vars` and a `bsh_lang::Shell`. Expansion needs
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
//!   into `bsh-lang` and `bsh-syntax`.
//! * `process` — the single foreground-wait path and the process built-ins
//!   `wait` and `kill`. There is no job control: background children are
//!   tracked by pid so `$!`, `wait` and `kill` work, and the job built-ins
//!   report `BSH-E0302` instead of pretending.
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
//!   parse as command words (`bsh-syntax` documents that); `[[ … =~ … ]]`
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
//!   as `Unsupported` by `bsh-lang` (its documented boundary).
//! * **Process substitution `<(…)`/`>(…)`** — `bsh-lang`'s default callback
//!   errors, and this evaluator does not override it.
//! * **`set -x`/`set -v`, `umask`, `shopt`, `ulimit`** — registry stubs; see
//!   `builtins.rs` for the executor-side set that *is* implemented and why.
//!   `printf`/`test`/`[`/`pwd` are served from `PATH` until `bsh-runtime`
//!   writes their bodies, which is where POSIX puts a regular built-in — which
//!   is also why `type -t [` reports `file` rather than `builtin`.
//! * **`~user`** — `bsh_lang::Shell::home_dir` has a default of `None` and
//!   `bsh-sys` exposes no `getpwnam`, so `~root` is left as written. `~`,
//!   `~+` and `~-` work.
//! * **Command substitution inside arithmetic** — `$(( $(cmd) + 1 ))` reports
//!   a syntax error, because arithmetic evaluation has no access to the
//!   command-substitution callback ([`crate::arith::IntVars`] is deliberately
//!   about integers only).
//! * **`$FUNCNAME`, `$PIPESTATUS`, `$RANDOM`, `$SECONDS`, `$LINENO`** — bash
//!   extensions the runtime does not define yet; `$PIPESTATUS` and `$FUNCNAME`
//!   are arrays, which are deferred above.
//! * **`set -i`/`PS1`** — the CLI marks the shell interactive when stdin is a
//!   terminal and reads lines from it; the prompt, editor and completion are
//!   `bsh-repl`'s job and are not the evaluator's. History recording is split:
//!   the interactive loop calls [`Interp::initialize_history`] and
//!   [`Interp::flush_history`], and the ring itself lives in `bsh-history`.
//! * **SIGPIPE** — Rust's runtime ignores it, so a built-in writing into a
//!   closed pipe sees `EPIPE` and reports status 1 instead of dying by signal.
//!   Children get `SIGPIPE` reset to `SIG_DFL` through `SpawnPlan`.
//! * **In-place `execve`** — `bsh-sys` exposes no way to replace the process
//!   without `unsafe`, so `exec cmd` spawns, waits and exits. Observationally
//!   equivalent except for the pid and for redirect-only `exec`, which is
//!   applied to the shell's own descriptors (that case is exact).
//!
//! # One deliberate deviation: a failing special built-in
//!
//! POSIX §2.8.1 says a non-interactive shell shall exit when a *special*
//! built-in fails, and `bsh_runtime::consequence` encodes exactly that. bash
//! (without `--posix`) continues instead, so `shift 5; echo after` prints
//! `after` in bash and exits 1 here. This follows the landed consequence table
//! and the project's rule of POSIX over bash where the two disagree, exactly as
//! it does for prefix assignments before special built-ins (DESIGN.md §3.3
//! Tier C). It is called out here because it is the one place a bash script
//! may notice.

use std::sync::Arc;

use bsh_diag::{Diagnostic, SourceFile};
use bsh_runtime::{History, Opts, ShellHost, ShellState, Status, consequence};
use bsh_syntax::ArrayElement;
use bsh_syntax::{Ast, NodeId, ParseOpts, ParseOutcome, parse_with};

mod alias;
mod builtins;
mod command;
mod complete;
mod cond;
mod eval;
mod mapfile;
mod process;
mod read;
mod redir;
mod scope;
mod traps;
mod umask;
mod words;

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
}

impl Unwind {
    /// The status this unwind carries, for `$?` and for a caller that has
    /// already decided to absorb it.
    pub fn status(self) -> Status {
        match self {
            Unwind::Status(status) | Unwind::Return(status) | Unwind::Exit(status) => status,
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

/// A defined shell function: the chunk it was parsed from plus its body node.
#[derive(Clone, Debug)]
struct Function {
    name: Vec<u8>,
    chunk: Arc<ParsedChunk>,
    body: NodeId,
}

/// Cached values for the special parameters that `bsh_lang::Vars::get` has to
/// answer by value (`$?`, `$-`, `$$`, `$!`). `$0` is read straight from
/// `Vars::arg0` and `$#` is computed from `positional()`, as the contract says.
#[derive(Clone, Debug, Default)]
struct Specials {
    status: Vec<u8>,
    dash: Vec<u8>,
    pid: Vec<u8>,
    bang: Vec<u8>,
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
    /// `bsh_lang::Vars::get` takes `&self` and answers `Option<&[u8]>`, so it
    /// cannot report the cycle itself. The flag records it and the next
    /// command boundary turns it into a fatal diagnostic, which is what makes
    /// `typeset -n a=b; typeset -n b=a; echo $a` stop instead of printing an
    /// empty string and looking like an unset variable.
    nameref_cycle: std::cell::Cell<bool>,
    /// Bytes a `read` pulled from a descriptor but could not give back.
    ///
    /// Only a descriptor that is not seekable can end up here; on a regular
    /// file the surplus is rewound with `lseek` instead, so no second reader
    /// ever sees a stale buffer.
    read_buffers: std::collections::BTreeMap<bsh_sys::RawFd, Vec<u8>>,
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
    pending_children: Vec<bsh_sys::Child>,
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
/// The *variable* keeps bash's value (the conformance suite reads it), while
/// [`Interp::render_prompt`] substitutes the fish-like default when the value
/// is still this one. A user who sets `PS1` at all — in the environment, an rc
/// file or at the prompt — replaces it and gets exactly their template.
const DEFAULT_PS1: &[u8] = b"\\s-\\v\\$ ";

/// bash's default `PS2` for an interactive shell.
const DEFAULT_PS2: &[u8] = b"> ";

impl Interp {
    /// A fresh shell. `interactive` selects the POSIX §2.8.1 consequences and
    /// the diagnostic renderer.
    pub fn new(interactive: bool) -> Self {
        Self::with_opts(Opts::default(), interactive)
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
            interactive,
            current: Arc::new(SourceFile::new("<none>", &b""[..])),
            functions: Vec::new(),
            specials: Specials::default(),
            last_cmdsub: None,
            pending: None,
            depth: 0,
            array_literals: Vec::new(),
            nameref_cycle: std::cell::Cell::new(false),
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
        };
        // POSIX initializes `OPTIND` to 1 at shell startup, and `getopts`
        // counts from there; a script that never mentions it still needs the
        // value (`echo $OPTIND` is `1`, not empty).
        let _ = interp.state.vars.assign_scalar(b"OPTIND", b"1");
        // `$?`, `$$` and `$-` must answer before the first command runs.
        interp.sync_specials();
        // bash turns history recording on for an interactive shell (the
        // `set -o history` option), before the startup file runs so that file
        // can turn it off. A non-interactive shell records nothing either way.
        if interactive {
            interp.state.opts.history = true;
            // An interactive bash also has default prompt variables, before the
            // environment and the startup file can replace them: measured
            // against 5.3.20, `declare -p PS1` in `bash -i` is
            // `PS1="\\s-\\v\\\$ "` and `PS2="> "`. `${PS1@P}` and the prompt
            // therefore decode the same template, and the conformance case
            // that reads `$PS1` from an interactive shell sees bash's value.
            let _ = interp.state.vars.assign_scalar(b"PS1", DEFAULT_PS1);
            let _ = interp.state.vars.assign_scalar(b"PS2", DEFAULT_PS2);
        }
        interp
    }

    /// The facts `PS1`/`PS2` read while the shell draws a prompt.
    ///
    /// This is [`bsh_lang::Shell::prompt_context`] with the two differences a
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
    pub fn repl_prompt_context(&self) -> bsh_lang::prompt::PromptContext {
        let mut context = <Self as bsh_lang::Shell>::prompt_context(self);
        context.history_number = self.state.history.next_number();
        if let Some((columns, _)) = bsh_sys::window_size_fd(0) {
            context.columns = columns;
        }
        context
    }

    /// Put the last command's status back and refresh `$?`.
    ///
    /// The interactive loop runs `PROMPT_COMMAND` between commands, which
    /// changes the status the way any other command would; bash keeps the
    /// status the user last saw, and this restores it.
    pub fn restore_status(&mut self, status: bsh_runtime::Status) {
        self.state.vars.set_last_status(status);
        self.sync_specials();
    }

    /// Whether the shell is still using its built-in `PS1`.
    ///
    /// The value is bash's default, which is what makes it distinguishable
    /// from a user's template without tracking assignments: an interactive
    /// shell starts with it, and the environment and the startup file
    /// overwrite it when they define `PS1`. The REPL uses this to leave its
    /// legacy `[branch]` segment off the fish-like default, which already
    /// contains the branch.
    pub fn uses_default_prompt(&self) -> bool {
        self.state
            .vars
            .scalar(b"PS1")
            .is_some_and(|value| value == DEFAULT_PS1)
    }

    /// Render a `PS1`/`PS2` template the way the interactive prompt needs it.
    ///
    /// This is the shared expander with the shell as its second-pass host, so
    /// `shopt promptvars` decides whether `$PWD` and `$(date)` are expanded.
    /// The last status is put back afterwards: bash's prompt expansion does
    /// not change `$?` even though it may run commands (measured against
    /// 5.3.20 with `PS1='$(false)$ '`).
    ///
    /// The stock `PS1` is the one exception: it renders as the fish-like
    /// [`bsh_lang::prompt::fish_prompt`] instead of bash's `bsh-0.0$ `. That
    /// prompt is built from data rather than decoded from a template, so a
    /// directory or branch containing `$`, a backslash or an escape-looking
    /// sequence is painted literally and cannot be expanded twice. The
    /// `PS1` *variable* is untouched, so `${PS1@P}` and `$PS1` keep the value
    /// the conformance suite expects.
    pub fn render_prompt(&mut self, template: &[u8]) -> bsh_lang::prompt::ExpandedPrompt {
        let context = self.repl_prompt_context();
        let status = self.state.vars.last_status();
        let rendered = if template == DEFAULT_PS1 {
            let branch = bsh_lang::prompt::git_branch(&context.cwd);
            bsh_lang::prompt::fish_prompt(
                &context.cwd,
                &context.home,
                branch.as_deref(),
                context.job_count,
                status.0,
            )
        } else {
            bsh_lang::prompt::expand_with_variables(template, &context, self)
        };
        self.state.vars.set_last_status(status);
        self.sync_specials();
        rendered
    }

    /// Import the process environment as exported shell variables, the way
    /// every shell starts. Bytes are preserved exactly.
    pub fn import_environment(&mut self) {
        let mut entries: Vec<Vec<u8>> = Vec::new();
        for (name, value) in std::env::vars_os() {
            let mut entry = bsh_sys::os::bytes_from_os(&name);
            entry.push(b'=');
            entry.extend_from_slice(&bsh_sys::os::bytes_from_os(&value));
            entries.push(entry);
        }
        self.state.vars.import_envp(&entries);
    }

    /// Give an interactive shell its history database.
    ///
    /// A terminal REPL passes `persist = true` and gets the user's database
    /// (`bsh_history::default_path`). A piped or `-c` interactive shell gets
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
        let Some(path) = bsh_history::default_path() else {
            return;
        };
        match History::open(&path, bsh_history::RECENT_DEPTH) {
            Ok(history) => self.state.history = history,
            Err(error) => {
                eprintln!("bsh: history: cannot open {}: {error}", path.display());
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
                    bsh_syntax::codes::NEED_MORE_INPUT,
                    open,
                    "unexpected end of file: the input is incomplete",
                ));
                self.current = previous;
                self.set_status(Status(2));
                return Status(2);
            }
            ParseOutcome::Failed(diagnostics) => {
                let previous = std::mem::replace(&mut self.current, source);
                for diagnostic in &diagnostics {
                    self.diagnose(diagnostic);
                }
                self.current = previous;
                self.set_status(Status(2));
                return Status(2);
            }
        };

        let previous = std::mem::replace(&mut self.current, source);
        let unwind = self.eval_node(&chunk, chunk.ast.root, Ctx::default());
        self.current = previous;
        let status = self.absorb(unwind);
        self.set_status(status);
        // Leave the cached specials agreeing with the final state, so a caller
        // that reads `$?` after `run_named` sees this chunk's status.
        self.sync_specials();
        status
    }

    /// Turn a top-level unwind into an exit status.
    ///
    /// An `Exit` is re-requested through `ShellState` rather than merely
    /// converted: `eval 'exit 3'` and `source` run a nested chunk, and the
    /// outer command sequence must still see the exit request. Without this,
    /// `exit` inside `eval` would only end the `eval`.
    fn absorb(&mut self, unwind: Unwind) -> Status {
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
                self.diagnose(&Diagnostic::error(
                    crate::codes::CONTROL_FLOW,
                    0..0,
                    "break/continue is only meaningful inside a loop",
                ));
                Status::SUCCESS
            }
            Unwind::Trap(_) => Status::SUCCESS,
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
    pub(crate) fn sync_specials(&mut self) {
        let status = self.state.vars.last_status().exit_code().to_string();
        let dash = self.state.opts.dash_flags();
        let pid = self.state.vars.shell_pid().to_string();
        let bang = self
            .state
            .vars
            .last_bg_pid()
            .map(|pid| pid.to_string())
            .unwrap_or_default();
        self.specials = Specials {
            status: status.into_bytes(),
            dash,
            pid: pid.into_bytes(),
            bang: bang.into_bytes(),
        };
    }

    /// Point `$LINENO` at `offset` in `source`, the chunk being evaluated.
    ///
    /// Node spans arrive mostly in source order, so the line counter advances
    /// from the previous offset; a jump backwards (a loop body, a function
    /// called twice) rescans from the top rather than reporting a stale line.
    pub(crate) fn set_line(&mut self, source: &Arc<SourceFile>, offset: usize) {
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
        code: bsh_diag::ErrCode,
        span: bsh_diag::Span,
        message: impl Into<String>,
    ) {
        self.diagnose(&Diagnostic::error(code, span, message.into()));
    }

    /// The POSIX §2.8.1 table, applied. Returns the unwind the caller must
    /// honour: `Continue` becomes an ordinary status.
    pub(crate) fn apply_consequence(
        &mut self,
        error: bsh_runtime::ShellError,
        status: Status,
    ) -> Unwind {
        match consequence(&error, self.interactive) {
            bsh_runtime::Action::Continue => Unwind::Status(status),
            bsh_runtime::Action::Exit(code) => Unwind::Exit(code),
        }
    }

    /// Report an expansion error and classify it.
    ///
    /// A failed glob is the one expansion error that must not escalate: with
    /// `shopt -s failglob`, bash fails the command and a script carries on
    /// (`shopt -s failglob; echo *.ZZ; echo status=$?` prints `status=1`),
    /// while a `${x:?}` error does end a non-interactive shell. Splitting the
    /// kind here is what keeps those two apart.
    pub(crate) fn expansion_failed(&mut self, error: bsh_lang::Error) -> Unwind {
        self.diagnose(&error.diagnostic());
        if matches!(error.kind, bsh_lang::ErrorKind::GlobNoMatch { .. }) {
            return Unwind::Status(Status(1));
        }
        self.apply_consequence(bsh_runtime::ShellError::Expansion, Status(1))
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
        if let Some(exit) = self.run_err_trap(status) {
            return Unwind::Exit(exit);
        }
        if !self.state.opts.errexit {
            return Unwind::Status(status);
        }
        Unwind::Exit(status)
    }
}

impl bsh_lang::Vars for Interp {
    fn get(&self, name: &[u8]) -> Option<&[u8]> {
        if name == b"LINENO" {
            return Some(&self.lineno);
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

    fn set(&mut self, name: &[u8], value: Vec<u8>) -> Result<(), bsh_lang::SetError> {
        match self.state.vars.assign_scalar(name, &value) {
            Ok(()) => Ok(()),
            Err(bsh_runtime::Error::InvalidName { .. }) => Err(bsh_lang::SetError::InvalidName),
            Err(_) => Err(bsh_lang::SetError::ReadOnly),
        }
    }

    fn array(&self, name: &[u8]) -> Option<bsh_lang::ArrayValue<'_>> {
        if name == b"PIPESTATUS" {
            return Some(bsh_lang::ArrayValue::Indexed(&self.pipeline_status));
        }
        match self.state.vars.value(name)? {
            bsh_runtime::VarValue::Indexed(elements) => {
                Some(bsh_lang::ArrayValue::Indexed(elements))
            }
            bsh_runtime::VarValue::Assoc(pairs) => {
                Some(bsh_lang::ArrayValue::Assoc(pairs.as_slice()))
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
    if !bsh_sys::is_tty(0) {
        return b"tty".to_vec();
    }
    #[cfg(unix)]
    if let Some(name) = std::fs::read_link("/proc/self/fd/0")
        .ok()
        .and_then(|path| path.file_name().map(bsh_sys::os::bytes_from_os))
    {
        return name;
    }
    b"tty".to_vec()
}

impl bsh_lang::Shell for Interp {
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
    fn prompt_context(&self) -> bsh_lang::prompt::PromptContext {
        let mut context = bsh_lang::prompt::PromptContext::from_env();
        let vars = &self.state.vars;
        if let Some(value) = vars.scalar(b"PWD") {
            context.cwd = value.to_vec();
        }
        if let Some(value) = vars.scalar(b"OLDPWD") {
            context.previous_cwd = value.to_vec();
        }
        if let Some(value) = vars.scalar(b"HOME") {
            context.home = value.to_vec();
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
        context.root = bsh_lang::prompt::is_root();
        context.tty = current_tty();
        context
    }

    fn pattern_flags(&self) -> bsh_pattern::Flags {
        // `utf8` stays at its default (byte units): expansion forces the C
        // locale, the same policy `${#x}` uses. What comes from `shopt` is
        // the pattern *dialect*.
        bsh_pattern::Flags {
            extglob: self.state.opts.extglob,
            nocasematch: self.state.opts.nocasematch,
            pathname: false,
            utf8: self.utf8_locale,
        }
    }

    fn command_substitution(
        &mut self,
        cmd: &bsh_lang::CommandSub,
    ) -> Result<Vec<u8>, bsh_lang::Error> {
        match self.capture_substitution(&cmd.source) {
            Ok((bytes, status)) => {
                self.set_status(status);
                self.last_cmdsub = Some(status);
                self.sync_specials();
                Ok(bytes)
            }
            Err(message) => Err(bsh_lang::Error::new(
                bsh_lang::ErrorKind::Host { message },
                cmd.span.clone(),
            )),
        }
    }

    fn arithmetic(&mut self, expr: &bsh_lang::Arithmetic) -> Result<Vec<u8>, bsh_lang::Error> {
        match crate::arith::eval(&expr.source, self) {
            Ok(value) => Ok(value.to_string().into_bytes()),
            Err(message) => Err(bsh_lang::Error::new(
                bsh_lang::ErrorKind::Host { message },
                expr.span.clone(),
            )),
        }
    }

    fn pathname(
        &mut self,
        pattern: &bsh_lang::Field,
    ) -> Result<Option<Vec<Vec<u8>>>, bsh_lang::Error> {
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
        let globignore = self
            .state
            .vars
            .scalar(b"GLOBIGNORE")
            .is_some_and(|value| !value.is_empty());
        let options = crate::glob::Options {
            flags: bsh_pattern::Flags {
                nocasematch: self.state.opts.nocaseglob,
                ..self.pattern_flags()
            },
            dotglob: self.state.opts.dotglob,
            globstar: self.state.opts.globstar,
            globskipdots: self.state.opts.globskipdots || globignore,
        };
        match crate::glob::expand(&pattern.bytes, &pattern.quoted, &options) {
            Some(matches) => Ok(Some(matches)),
            None if self.state.opts.nullglob => Ok(Some(Vec::new())),
            None if self.state.opts.failglob => Err(bsh_lang::Error::new(
                bsh_lang::ErrorKind::GlobNoMatch {
                    pattern: pattern.bytes.clone(),
                },
                0..0,
            )),
            None => Ok(None),
        }
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
        // An array decays to element zero in arithmetic, just as `${a}` does.
        let bytes = match self.state.vars.value(name) {
            Some(bsh_runtime::VarValue::Indexed(elements)) => elements.get(&0).map(Vec::as_slice),
            Some(bsh_runtime::VarValue::Assoc(pairs)) => pairs
                .iter()
                .find(|(key, _)| key.as_slice() == b"0")
                .map(|(_, value)| value.as_slice()),
            _ => self.state.vars.scalar(name),
        };
        bytes.and_then(crate::arith::parse_int).unwrap_or(0)
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
            Some(bsh_runtime::VarValue::Indexed(elements)) => elements.get(&0).cloned(),
            Some(bsh_runtime::VarValue::Assoc(pairs)) => pairs
                .iter()
                .find(|(key, _)| key.as_slice() == b"0")
                .map(|(_, value)| value.clone()),
            _ => self.state.vars.scalar(name).map(<[u8]>::to_vec),
        }
    }

    fn get_index_text(&mut self, name: &[u8], index: i64) -> Option<Vec<u8>> {
        let value = match self.state.vars.shape(name) {
            Some(bsh_runtime::ArrayShape::Associative) => self
                .state
                .vars
                .element_assoc(name, index.to_string().as_bytes()),
            _ => self.state.vars.element_indexed(name, index),
        };
        value.map(<[u8]>::to_vec)
    }

    fn get_index(&mut self, name: &[u8], index: i64) -> i64 {
        let value = match self.state.vars.shape(name) {
            Some(bsh_runtime::ArrayShape::Associative) => self
                .state
                .vars
                .element_assoc(name, index.to_string().as_bytes()),
            _ => self.state.vars.element_indexed(name, index),
        };
        value.and_then(crate::arith::parse_int).unwrap_or(0)
    }

    fn set_index(&mut self, name: &[u8], index: i64, value: i64) -> Result<(), String> {
        let text = value.to_string();
        match self.state.vars.shape(name) {
            // An associative array's subscript is a string key, even when it
            // looks like a number (`assoc[0]=42`).
            Some(bsh_runtime::ArrayShape::Associative) => self
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
        let word = match bsh_syntax::parse_word(&subscript, &self.parse_opts()) {
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
        let text = match bsh_lang::expand_assignment(&word, self) {
            Ok(text) => text,
            Err(error) => {
                self.diagnose(&error.diagnostic());
                return Some(Status(1));
            }
        };
        let result = match self.state.vars.shape(&name) {
            Some(bsh_runtime::ArrayShape::Associative) => {
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

    fn apply_array_literal_operand(
        &mut self,
        operand: &[u8],
        shape: Option<bsh_runtime::ArrayShape>,
        scope: bsh_runtime::CreateScope,
    ) -> Option<Result<(), String>> {
        let literal = self.take_array_literal(operand)?;
        let variable = literal.name.clone();
        if scope == bsh_runtime::CreateScope::Local
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
            bsh_syntax::parse_bytes(src, &opts),
            bsh_syntax::ParseOutcome::Ok(_)
        )
    }

    fn diagnose(&mut self, diagnostic: &Diagnostic) {
        let source = self.current.clone();
        let rendered = if self.interactive {
            bsh_diag::render_compact(diagnostic, &source)
        } else {
            diagnostic.render_line_with_help(&source)
        };
        let mut line = rendered.into_bytes();
        line.push(b'\n');
        let _ = self.write_err(&line);
    }

    fn resolve_command(
        &self,
        name: &[u8],
        query: bsh_runtime::ResolveQuery,
    ) -> Vec<bsh_runtime::CommandInfo> {
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
        let def = bsh_runtime::lookup_builtin(name)?;
        let resolution = self.registry_body(def);
        Some(self.run_direct(resolution, argv))
    }

    fn run_command_direct(&mut self, argv: &[Vec<u8>], default_path: bool) -> Status {
        let Some(name) = argv.first().cloned() else {
            return Status::SUCCESS;
        };
        let query = bsh_runtime::ResolveQuery {
            skip_functions: true,
            default_path,
            ..bsh_runtime::ResolveQuery::EXECUTION
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

    fn completion_source(&mut self) -> Option<&mut dyn bsh_runtime::complete::CompletionSource> {
        // `complete.rs` implements the trait for the interpreter, so `compgen`
        // asks the shell itself: there is no second name table to disagree
        // with execution.
        Some(self)
    }

    fn completion_words(&mut self, wordlist: &[u8]) -> bsh_runtime::complete::WordList {
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
