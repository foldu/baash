//! Subshell scopes: the in-process replacement for `fork`.
//!
//! `( … )`, `$( … )` and the last stage of a pipeline run *in this process*.
//! They see the parent's shell state on entry and must leave none of their own
//! behind, which is what a scope is: a snapshot taken before the body runs and
//! reinstated after it finishes.
//!
//! # What a scope covers
//!
//! * The variable store — consciously *not* copied. A scope opens a journal on
//!   it ([`baash_runtime::Vars::begin_scope`]) and the close replays it
//!   backwards, so a body that writes one variable costs one record instead of
//!   a copy of every name. Every command writes `_`, so a copy-on-open made
//!   every `$()` pay for the whole table: 15% of the work in a substitution
//!   loop, and 34% together with the snapshot's drop
//!   (`docs/performance.md`). Debug builds compare the restored store with a
//!   copy taken at open, so a write path that forgets to journal fails a test
//!   instead of leaking state.
//! * The rest of [`ShellState`] — options, traps, aliases, completion specs,
//!   the directory stack and the `getopts` cursor. Cloned on entry and restored
//!   on exit, which measures under 1% of the same loop: they are small, and a
//!   journal for each would be bookkeeping worth more than the copy.
//! * The working directory — the process has one, so it is saved as bytes and
//!   `chdir`-ed back.
//! * Committed descriptors — `exec 3>file` with no command. Ordinary
//!   redirections are already undone per command by [`Interp::restore_fds`];
//!   the committed ones are the only fd state that outlives a command, and
//!   they are journalled on `Interp::committed` so a scope can undo exactly the
//!   ones it caused, in reverse.
//! * The interpreter's own transient state: the `read` surplus buffers, the
//!   `PATH` hash table and `$PIPESTATUS`. The cache is *inherited* (a subshell
//!   starts with the parent's hits) but changes do not leak back.
//!
//! # What it deliberately does not cover
//!
//! * **History.** A scope gets an empty ring. Cloning the real one would copy
//!   every entry on each `$( … )`, and a subshell must not write to it anyway;
//!   the cost of the difference is that `history` inside `$( … )` prints
//!   nothing.
//! * **Background children.** `cmd &` inside a scope spawns a child of *this*
//!   process; its pid stays in the parent's `background` map so `wait` and
//!   `reap_background` can still see it.
//! * **`$?`.** The status is part of [`ShellState`] and is restored with it;
//!   the caller sets the command's status afterwards, as for any command.
//!
//! # Why this is sound without `fork`
//!
//! A subshell in bash is a copy-on-write clone of the shell process: the
//! kernel shares every page and copies the ones written. Here the body runs on
//! the same thread with the same process id — `$$` is the parent's, `$BASHPID`
//! the shell's — signals are delivered once rather than twice, and a trap set
//! inside the body is undone by the closing instead of dying with a process.
//! For the variable store the two are now the same shape: a write copies the
//! one thing written, never the table.

use baash_sys::RawFd;
use std::collections::BTreeMap;

use baash_runtime::ShellState;

use super::Interp;

/// Shell state captured at the start of a scope.
///
/// Held by the caller between [`Interp::open_scope`] and
/// [`Interp::close_scope`]; dropping it without closing leaks nothing but
/// leaves the snapshot behind, so the two calls are always paired.
pub(crate) struct Scope {
    state: ShellState,
    /// The variable store's journal: what the body wrote, to replay backwards.
    vars: baash_runtime::SavedScope,
    /// A copy of the store taken before the body ran.
    ///
    /// Debug builds compare it with what `end_scope` restored: a write path
    /// that forgets to journal leaks state into the parent, and a leak is
    /// silent until a script trips over it.
    #[cfg(debug_assertions)]
    before: baash_runtime::Vars,
    /// The parent's blocked mask, reader and `SIG_IGN` bookkeeping.
    ///
    /// The body runs with its own (empty, then rebuilt) state; this is what
    /// puts the parent's back, including the interactive `SIGINT` block.
    signals: super::traps::SignalState,
    read_buffers: BTreeMap<RawFd, Vec<u8>>,
    path_cache: BTreeMap<Vec<u8>, Vec<u8>>,
    pipeline_status: BTreeMap<i64, Vec<u8>>,
    /// `unset FUNCNAME` and its siblings, which a body inherits but must not
    /// leak back: the flag is shell state, so the snapshot carries it. The
    /// arrays themselves are cache — the frames do not change across the
    /// boundary, so a body that reads one rebuilds the same thing.
    call_vars_cleared: bool,
    /// How many entries `Interp::committed` had when the scope opened.
    committed: usize,
    /// The working directory to return to, as shell bytes.
    cwd: Option<Vec<u8>>,
}

impl Interp {
    /// Capture the shell state a subshell body must not leak into.
    pub(crate) fn open_scope(&mut self) -> Scope {
        // The ring is left behind rather than cloned: see the module docs.
        let history = std::mem::take(&mut self.state.history);
        // The variable store is the expensive part of the state and the only
        // one a body usually touches, so it is *not* copied: the scope records
        // its writes and undoes them (`Vars::begin_scope`). A stand-in keeps
        // the clone below from copying the 71 names the body may never touch.
        let vars = std::mem::take(&mut self.state.vars);
        let state = self.state.clone();
        self.state.history = history;
        self.state.vars = vars;
        #[cfg(debug_assertions)]
        let before = self.state.vars.clone();
        let vars = self.state.vars.begin_scope();

        // POSIX: a subshell starts with every trap at its default action
        // (`trap` inside `( … )` prints nothing, as in dash). Ignored signals
        // stay ignored. This clears the table the body *sees*; `state` keeps
        // the parent's table, which the close puts back.
        self.state.traps.clear_for_subshell();
        // The dispositions follow from the table, so they are reset for the
        // body and reapplied when the scope closes. An interactive shell keeps
        // the terminal's signals blocked inside the body: there is no child
        // process to receive them, so unblocking them would kill the session
        // (`enter_scope_signals`).
        let signals = self.enter_scope_signals();

        self.scope_depth += 1;

        Scope {
            state,
            vars,
            call_vars_cleared: self.call_vars.cleared,
            #[cfg(debug_assertions)]
            before,
            signals,
            read_buffers: std::mem::take(&mut self.read_buffers),
            path_cache: self.path_cache.clone(),
            pipeline_status: self.pipeline_status.clone(),
            committed: self.committed.len(),
            cwd: current_dir_bytes(),
        }
    }

    /// Undo everything the scope's body changed.
    pub(crate) fn close_scope(&mut self, scope: Scope) {
        debug_assert!(self.scope_depth > 0, "scope closed without an opener");
        self.scope_depth = self.scope_depth.saturating_sub(1);
        // Descriptors first: the body may have left one pointing at a file the
        // restored state expects to have its own binding.
        while self.committed.len() > scope.committed {
            if let Some(saved) = self.committed.pop() {
                self.restore_fds(saved);
            }
        }
        // History stays with the shell: the snapshot does not carry it (see
        // `open_scope`), so putting the state back must not take the ring away.
        let history = std::mem::take(&mut self.state.history);
        // Undo the body's variable writes *before* the state goes back: the
        // snapshot holds an empty stand-in where the live store is, so the
        // journal edits the live one and the state swap keeps it.
        let mut vars = std::mem::take(&mut self.state.vars);
        vars.end_scope(scope.vars);
        #[cfg(debug_assertions)]
        assert!(
            vars == scope.before,
            "a subshell scope left variable state behind: a write path is not journalled"
        );
        self.state = scope.state;
        self.state.history = history;
        self.state.vars = vars;
        self.read_buffers = scope.read_buffers;
        self.path_cache = scope.path_cache;
        self.pipeline_status = scope.pipeline_status;
        self.call_vars.cleared = scope.call_vars_cleared;
        if let Some(cwd) = scope.cwd {
            // Best effort: the directory may have been removed while the body
            // ran, in which case the shell keeps its current directory (bash
            // has the same failure mode for `cd -` to a deleted directory).
            let _ = baash_sys::chdir(&cwd);
        }
        // Restored traps must reach the process, not only the table. The mask
        // needs this even when the table did not change: the body emptied it.
        self.leave_scope_signals(scope.signals);
    }

    /// Run `body` as a subshell and report its status.
    ///
    /// `exit` inside the body ends the body, not the shell.
    pub(crate) fn run_in_scope(
        &mut self,
        chunk: &std::sync::Arc<super::ParsedChunk>,
        body: baash_syntax::NodeId,
        ctx: super::Ctx,
    ) -> baash_runtime::Status {
        let scope = self.open_scope();
        let status = self.eval_node(chunk, body, ctx).status();
        self.close_scope(scope);
        status
    }
}

/// The working directory as shell bytes, so it can be restored exactly.
fn current_dir_bytes() -> Option<Vec<u8>> {
    std::env::current_dir()
        .ok()
        .map(baash_sys::os::bytes_from_os)
}
