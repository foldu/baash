//! Trap delivery: the signal side of `bsh-runtime`'s trap table.
//!
//! The table itself (what to run, in bash's listing order, with re-input-safe
//! rendering) lives in `bsh-runtime`; this module is the executor's half:
//!
//! * **Caught signals are blocked, not handled.** DESIGN.md §4.5: a trap runs
//!   at a command boundary, never inside a built-in and never in a signal
//!   handler. So every signal with a `Command` trap is blocked in the shell
//!   thread and read from a `signalfd` at safe points; no native handler can
//!   touch interpreter state, and a signal that arrives while a foreground
//!   child runs simply stays pending until the child is reaped.
//! * **Ignored signals are `SIG_IGN`, not blocked.** That disposition survives
//!   `exec`, which is exactly what `trap '' INT` promises for children.
//! * **A trap never re-enters itself.** `in_trap` guards every entry point.
//! * **`$?` is preserved** across a handler unless the handler exits, and the
//!   EXIT trap's status is the shell's pending status unless the handler calls
//!   `exit`.
//!
//! The trap kinds have different execution points: `EXIT` when the shell
//! finishes (including a parse error, an interrupt, and `exit`), `ERR` where
//! `errexit` would fire, `DEBUG` before each simple command, `RETURN` when a
//! sourced script returns (and from a function only under `-o functrace`,
//! which is bash's inheritance rule).

use std::collections::BTreeMap;
use std::sync::Arc;

use bsh_runtime::{Status, TrapAction, TrapKey};

use super::Interp;

/// What running one handler produced.
struct TrapOutcome {
    /// `Some` when the handler called `exit`: the shell must unwind.
    exit: Option<Status>,
    /// The handler's own final status, which most trap kinds discard.
    status: Status,
}

/// The signal dispositions this shell currently maintains.
///
/// Rebuilt from the trap table whenever its generation changes; comparing the
/// generation is cheaper than diffing, and the table is the only source of
/// truth.
#[derive(Clone, Debug, Default)]
pub(crate) struct SignalState {
    /// The `signalfd` reader for the caught signals, once there is one.
    reader: Option<Arc<bsh_sys::SignalFd>>,
    /// The signals blocked for that reader (and what its mask was built for).
    blocked: bsh_sys::SignalSet,
    /// Signals whose disposition this shell set to `SIG_IGN`.
    ignored: Vec<i32>,
    /// The trap-table generation this state was built from.
    generation: u64,
}

impl Interp {
    /// Make the process's signal dispositions match the trap table.
    ///
    /// Idempotent and cheap when nothing changed. Called at command
    /// boundaries, so a `trap` registered by one command is in force before
    /// the next one runs.
    pub(crate) fn sync_traps(&mut self) {
        let generation = self.state.traps.generation();
        if generation == self.signals.generation {
            return;
        }
        self.signals.generation = generation;

        let caught = self.state.traps.caught_signals();
        let ignored = self.state.traps.ignored_signals();

        // Block first: a signal whose trap was just registered must not be
        // able to kill the shell in the window before the mask changes.
        let mut want = bsh_sys::SignalSet::empty();
        for signal in &caught {
            want.insert(*signal);
        }
        if want != self.signals.blocked || (self.signals.reader.is_none() && !caught.is_empty()) {
            if let Ok(current) = bsh_sys::SignalSet::current() {
                let mut target = current;
                for signal in self.signals.blocked.iter() {
                    if !want.contains(signal) {
                        target.remove(signal);
                    }
                }
                for signal in want.iter() {
                    target.insert(signal);
                }
                let _ = bsh_sys::replace_signal_mask(&target);
            }
            self.signals.blocked = want;
            // A fresh descriptor per mask change keeps `SignalFd` immutable in
            // the interpreter; pending signals live in the process, not the
            // descriptor, so nothing is lost by closing the old one.
            self.signals.reader = if caught.is_empty() {
                None
            } else {
                bsh_sys::SignalFd::new(&want).ok().map(Arc::new)
            };
        }

        // `SIG_IGN` for ignored traps, and the default action back for signals
        // that are no longer ignored.
        for signal in &ignored {
            if !self.signals.ignored.contains(signal) {
                let _ = bsh_sys::set_disposition(*signal, bsh_sys::Disposition::Ignore);
                self.signals.ignored.push(*signal);
            }
        }
        let stale: Vec<i32> = self
            .signals
            .ignored
            .iter()
            .copied()
            .filter(|signal| !ignored.contains(signal))
            .collect();
        for signal in stale {
            let _ = bsh_sys::set_disposition(signal, bsh_sys::Disposition::Default);
            self.signals
                .ignored
                .retain(|candidate| *candidate != signal);
        }
    }

    /// The `reset_signals` set for a spawned child.
    ///
    /// `exec` resets *caught* signals to their default action, so a child must
    /// not inherit a trap. Ignored signals are deliberately absent: `SIG_IGN`
    /// is the one disposition that survives `exec`, which is what `trap ''`
    /// means.
    pub(crate) fn child_reset_signals(&self) -> bsh_sys::SignalSet {
        let mut set = bsh_sys::SignalSet::of(&[bsh_sys::SIGPIPE]);
        for signal in self.state.traps.caught_signals() {
            set.insert(signal);
        }
        // The shell ignores these three for its own job control. `exec`
        // preserves `SIG_IGN`, so without an explicit reset a `Ctrl-Z` could
        // never stop a command. A `trap '' TSTP` still wins.
        let ignored = self.state.traps.ignored_signals();
        for signal in &self.ignored_job_signals {
            if !ignored.contains(signal) {
                set.insert(*signal);
            }
        }
        set
    }

    /// What a forked child must forget.
    ///
    /// POSIX §2.11: a subshell starts with traps cleared, except that an
    /// ignored signal stays ignored (it is a process disposition, not a trap).
    /// The child also unblocks the signals the parent blocked for its
    /// `signalfd`, so `kill -INT` on a subshell behaves like it does in bash.
    pub(crate) fn enter_child(&mut self) {
        self.state.traps.clear_for_subshell();
        self.signals = SignalState::default();
        let ignored = self.state.traps.ignored_signals();
        // The shell itself ignores `SIGTTOU`/`SIGTTIN`/`SIGTSTP`; a forked
        // child must be stopable again (unless the user made it ignored).
        for signal in self.ignored_job_signals.clone() {
            if !ignored.contains(&signal) {
                let _ = bsh_sys::set_disposition(signal, bsh_sys::Disposition::Default);
            }
        }
        let _ = bsh_sys::replace_signal_mask(&bsh_sys::SignalSet::empty());
        self.in_trap = false;
        self.exiting = false;
        self.run_depth = 0;
        self.background.clear();
        self.job_table = super::jobs::JobTable::default();
        self.read_buffers.clear();
        // A subshell is not an interactive job-control shell: it must not
        // create process groups or claim the terminal for its own children.
        self.job_control = false;
        self.shell_pgid = 0;
        self.ignored_job_signals.clear();
    }

    /// Read every pending signal, dropping the ones in `discard`.
    ///
    /// `SIGINT`/`SIGQUIT` are discarded after a foreground wait: bash ignores
    /// those two while it waits, so a `kill -INT <shell>` during `sleep` never
    /// runs an `INT` trap (the suite pins that difference against `USR1`).
    pub(crate) fn drain_signals(&mut self, discard: &[i32]) -> Vec<i32> {
        let Some(reader) = self.signals.reader.clone() else {
            return Vec::new();
        };
        let mut signals = Vec::new();
        while let Ok(Some(event)) = reader.read() {
            if !discard.contains(&event.signal) {
                signals.push(event.signal);
            }
        }
        signals.sort_unstable();
        signals.dedup();
        signals
    }

    /// Run the traps for the signals that have arrived. Returns an exit status
    /// when one of them called `exit`.
    pub(crate) fn run_pending_signal_traps(&mut self) -> Option<Status> {
        self.sync_traps();
        let signals = self.drain_signals(&[]);
        for signal in signals {
            let action = self.state.traps.signal_action(signal).cloned();
            let Some(TrapAction::Command(code)) = action else {
                continue;
            };
            let outcome = self.run_trap_code(&code);
            if let Some(status) = outcome.exit {
                return Some(status);
            }
        }
        None
    }

    /// After a foreground child has been reaped: discard `INT`/`QUIT` (bash
    /// ignores them while it waits) and run any other trap that arrived.
    pub(crate) fn after_foreground_wait(&mut self) -> Option<Status> {
        self.sync_traps();
        let signals = self.drain_signals(&[bsh_sys::SIGINT, bsh_sys::SIGQUIT]);
        for signal in signals {
            let action = self.state.traps.signal_action(signal).cloned();
            let Some(TrapAction::Command(code)) = action else {
                continue;
            };
            let outcome = self.run_trap_code(&code);
            if let Some(status) = outcome.exit {
                return Some(status);
            }
        }
        None
    }

    /// Ignore `SIGINT`/`SIGQUIT` for the duration of a foreground wait, except
    /// for the ones a trap has already blocked (they arrive via the reader).
    pub(crate) fn ignore_int_quit(&self) -> Option<bsh_sys::SignalGuard> {
        let trapped = self.state.traps.caught_signals();
        let mut ignore = Vec::new();
        for signal in [bsh_sys::SIGINT, bsh_sys::SIGQUIT] {
            if !trapped.contains(&signal) {
                ignore.push(signal);
            }
        }
        if ignore.is_empty() {
            return None;
        }
        bsh_sys::ignore_signals(&ignore).ok()
    }

    /// Run one handler body, with the no-re-entry and `$?` rules applied.
    fn run_trap_code(&mut self, code: &[u8]) -> TrapOutcome {
        if self.in_trap {
            return TrapOutcome {
                exit: None,
                status: Status::SUCCESS,
            };
        }
        let saved = self.state.vars.last_status();
        self.in_trap = true;
        self.run_named("<trap>", code);
        self.in_trap = false;
        let status = self.state.vars.last_status();
        let exit = self.state.take_exit_request();
        // A handler's own status never becomes `$?` unless it called `exit`
        // ("exit codes for traps are isolated").
        self.set_status(saved);
        self.sync_specials();
        TrapOutcome { exit, status }
    }

    /// `DEBUG`: before each simple command.
    ///
    /// The handler's status is ignored, *except* under `errexit`, where a
    /// failing DEBUG trap exits the shell — that is the one observable
    /// difference the suite draws between the two DEBUG cases.
    pub(crate) fn run_debug_trap(&mut self) -> Option<Status> {
        let Some(TrapAction::Command(code)) = self.state.traps.get(TrapKey::Debug).cloned() else {
            return None;
        };
        if self.in_trap {
            return None;
        }
        let outcome = self.run_trap_code(&code);
        if let Some(status) = outcome.exit {
            return Some(status);
        }
        if !outcome.status.is_success() && self.state.opts.errexit {
            return Some(outcome.status);
        }
        None
    }

    /// `ERR`: where `errexit` would fire, whether or not it is enabled.
    pub(crate) fn run_err_trap(&mut self, failing: Status) -> Option<Status> {
        let Some(TrapAction::Command(code)) = self.state.traps.get(TrapKey::Err).cloned() else {
            return None;
        };
        if self.in_trap {
            return None;
        }
        self.set_status(failing);
        self.sync_specials();
        self.run_trap_code(&code).exit
    }

    /// Take the traps a function call must not inherit.
    ///
    /// bash's rule: `ERR` reaches a function only under `-o errtrace`, and
    /// `DEBUG`/`RETURN` only under `-o functrace`. Returns what was removed so
    /// the caller can put it back when the function returns.
    pub(crate) fn suspend_function_traps(&mut self) -> Vec<(TrapKey, TrapAction)> {
        let errtrace = self.state.opts.errtrace;
        let functrace = self.state.opts.functrace;
        let mut suspended = Vec::new();
        for (key, inherited) in [
            (TrapKey::Err, errtrace),
            (TrapKey::Debug, functrace),
            (TrapKey::Return, functrace),
        ] {
            if inherited {
                continue;
            }
            if let Some(action) = self.state.traps.get(key).cloned() {
                self.state.traps.remove(key);
                suspended.push((key, action));
            }
        }
        suspended
    }

    /// Put back what [`Interp::suspend_function_traps`] removed.
    pub(crate) fn restore_function_traps(&mut self, suspended: Vec<(TrapKey, TrapAction)>) {
        for (key, action) in suspended {
            self.state.traps.set(key, action);
        }
    }

    /// `RETURN`: when a sourced script returns, and (under `-o functrace`)
    /// when a function returns.
    pub(crate) fn run_return_trap(&mut self) -> Option<Status> {
        let Some(TrapAction::Command(code)) = self.state.traps.get(TrapKey::Return).cloned() else {
            return None;
        };
        if self.in_trap {
            return None;
        }
        self.run_trap_code(&code).exit
    }

    /// The shell is finishing: run the signal traps that are still pending,
    /// then `EXIT`, and report the status the process must exit with.
    ///
    /// `$?` inside `EXIT` is the pending status, and the handler's own status
    /// does not replace it unless the handler calls `exit`.
    pub(crate) fn finish(&mut self, status: Status) -> Status {
        self.exiting = true;
        self.set_status(status);
        self.sync_specials();
        let pending = self.run_pending_signal_traps().unwrap_or(status);
        self.set_status(pending);
        self.sync_specials();

        // The status is captured in `pending`; a request left over from the
        // `exit` that got us here must not abort the handler's first command.
        let _ = self.state.take_exit_request();
        let action = self.state.traps.get(TrapKey::Exit).cloned();
        let Some(TrapAction::Command(code)) = action else {
            return pending;
        };
        // Clear before running: a trap must not run twice, and the handler is
        // allowed to register a new EXIT trap for a nested exit.
        self.state.traps.remove(TrapKey::Exit);
        // A bare `exit` inside the handler reuses `pending` (bash keeps a
        // saved exit value for exactly this), and the handler's own status is
        // otherwise ignored.
        self.exit_trap_status = Some(pending);
        let exit = self.run_trap_code(&code).exit;
        self.exit_trap_status = None;
        exit.unwrap_or(pending)
    }
}

impl Drop for Interp {
    fn drop(&mut self) {
        // A non-interactive shell runs its EXIT trap at the end of the
        // top-level chunk, where `exit` inside the handler can still change the
        // status the process returns. An interactive shell instead exits when
        // its editor loop ends, which the VM cannot observe, so the trap runs
        // here. (An `exit 42` inside the trap then cannot change the process
        // status; bash's REPL has no such wrinkle.)
        if self.interactive && !self.exiting {
            let status = self.status();
            self.finish(status);
        }
    }
}

/// A child process the shell started with `&`, and its status once reaped.
///
/// `None` means "still running" (or at least not reaped yet); the status is
/// kept after the child is reaped so `wait PID` can still report it.
pub(crate) type Background = BTreeMap<i32, Option<Status>>;

/// The name a `wait`/`kill` diagnostic should print for itself.
pub(crate) fn builtin_name(argv: &[Vec<u8>], fallback: &str) -> String {
    argv.first()
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .unwrap_or_else(|| fallback.to_owned())
}
