//! Subshell scopes: the in-process replacement for `fork`.
//!
//! `( … )`, `$( … )` and the last stage of a pipeline run *in this process*.
//! They see the parent's shell state on entry and must leave none of their own
//! behind, which is what a scope is: a snapshot taken before the body runs and
//! reinstated after it finishes.
//!
//! # What a scope covers
//!
//! * [`ShellState`] — options, variables (globals, arrays, namerefs,
//!   attributes), positional parameters, traps, aliases, completion specs and
//!   the `getopts` cursor. Cloned on entry, restored on exit, so a `cd`
//!   followed by a variable assignment inside `( … )` changes nothing outside.
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
//! A subshell in bash is a copy-on-write clone of the shell process. Here the
//! body runs on the same thread with the same process id, so `$$` is the
//! parent's (bash keeps `$$` too, and gives the subshell's own pid in
//! `$BASHPID`), signals are delivered once rather than twice, and a trap set
//! inside the body is undone by the restore instead of dying with a process.

use std::collections::BTreeMap;
use std::os::fd::RawFd;

use bsh_runtime::ShellState;

use super::Interp;

/// Shell state captured at the start of a scope.
///
/// Held by the caller between [`Interp::open_scope`] and
/// [`Interp::close_scope`]; dropping it without closing leaks nothing but
/// leaves the snapshot behind, so the two calls are always paired.
pub(crate) struct Scope {
    state: ShellState,
    read_buffers: BTreeMap<RawFd, Vec<u8>>,
    path_cache: BTreeMap<Vec<u8>, Vec<u8>>,
    pipeline_status: BTreeMap<i64, Vec<u8>>,
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
        let state = self.state.clone();
        self.state.history = history;

        // POSIX: a subshell starts with every trap at its default action
        // (`trap` inside `( … )` prints nothing, as in dash). Ignored signals
        // stay ignored. This clears the table the body *sees*; `state` keeps
        // the parent's table, which the close puts back.
        self.state.traps.clear_for_subshell();
        // The dispositions follow from the table, so they are reset for the
        // body and reapplied by `sync_traps` on the way out.
        self.signals = super::traps::SignalState::default();
        let _ = bsh_sys::replace_signal_mask(&bsh_sys::SignalSet::empty());

        self.scope_depth += 1;

        Scope {
            state,
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
        self.state = scope.state;
        self.read_buffers = scope.read_buffers;
        self.path_cache = scope.path_cache;
        self.pipeline_status = scope.pipeline_status;
        if let Some(cwd) = scope.cwd {
            // Best effort: the directory may have been removed while the body
            // ran, in which case the shell keeps its current directory (bash
            // has the same failure mode for `cd -` to a deleted directory).
            let _ = bsh_sys::chdir(&cwd);
        }
        // Restored traps must reach the process, not only the table.
        self.sync_traps();
    }

    /// Run `body` as a subshell and report its status.
    ///
    /// `exit` inside the body ends the body, not the shell.
    pub(crate) fn run_in_scope(
        &mut self,
        chunk: &std::sync::Arc<super::ParsedChunk>,
        body: bsh_syntax::NodeId,
        ctx: super::Ctx,
    ) -> bsh_runtime::Status {
        let scope = self.open_scope();
        let status = self.eval_node(chunk, body, ctx).status();
        self.close_scope(scope);
        status
    }
}

/// The working directory as shell bytes, so it can be restored exactly.
fn current_dir_bytes() -> Option<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;

    std::env::current_dir()
        .ok()
        .map(|path| path.as_os_str().as_bytes().to_vec())
}
