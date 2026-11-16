//! Trap delivery: the signal side of `baash-runtime`'s trap table.
//!
//! The table itself (what to run, in bash's listing order, with re-input-safe
//! rendering) lives in `baash-runtime`; this module is the executor's half:
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

use baash_sys::AsRawFd;
use std::sync::Arc;

use baash_runtime::{ShellHost, Status, TrapAction, TrapKey};

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
    reader: Option<Arc<baash_sys::SignalFd>>,
    /// The signals blocked for that reader (and what its mask was built for).
    blocked: baash_sys::SignalSet,
    /// Signals whose disposition this shell set to `SIG_IGN`.
    ignored: Vec<i32>,
    /// The trap-table generation this state was built from.
    ///
    /// `None` means "never applied", which is deliberately distinct from "the
    /// table has generation 0": an interactive shell with no traps at all still
    /// needs its `SIGINT`/`SIGQUIT` block installed and a reader open, and a
    /// subshell body starts from a state that must be rebuilt even though the
    /// restored table's generation is unchanged.
    generation: Option<u64>,
}

impl Interp {
    /// Make the process's signal dispositions match the trap table.
    ///
    /// Idempotent and cheap when nothing changed. Called at command
    /// boundaries, so a `trap` registered by one command is in force before
    /// the next one runs.
    pub(crate) fn sync_traps(&mut self) {
        let generation = self.state.traps.generation();
        if self.signals.generation == Some(generation) {
            return;
        }
        self.signals.generation = Some(generation);

        let caught = self.state.traps.caught_signals();
        let ignored = self.state.traps.ignored_signals();

        // Block first: a signal whose trap was just registered must not be
        // able to kill the shell in the window before the mask changes.
        let mut want = baash_sys::SignalSet::empty();
        for signal in &caught {
            want.insert(*signal);
        }
        // An interactive shell keeps the terminal's own signals blocked for its
        // whole life: a command runs with the terminal in cooked mode, so the
        // `Ctrl-C` that is a keystroke at the prompt is a `SIGINT` for the
        // shell's own process group while a builtin runs
        // (`keep_the_session_through_ctrl_c` in `baash` installs the mask
        // before the first command). They are part of the *wanted* set here, so
        // that this function's own bookkeeping cannot take them away again:
        // `trap 'x' INT` records `SIGINT` as blocked by the trap table, and a
        // later `trap - INT` would otherwise unblock it and put the session
        // back at the default action.
        if self.is_interactive() {
            want.insert(baash_sys::SIGINT);
            want.insert(baash_sys::SIGQUIT);
        }
        if want != self.signals.blocked || (self.signals.reader.is_none() && !want.is_empty()) {
            if let Ok(current) = baash_sys::SignalSet::current() {
                let mut target = current;
                for signal in self.signals.blocked.iter() {
                    if !want.contains(signal) {
                        target.remove(signal);
                    }
                }
                for signal in want.iter() {
                    target.insert(signal);
                }
                let _ = baash_sys::replace_signal_mask(&target);
            }
            // A fresh descriptor per mask change keeps `SignalFd` immutable in
            // the interpreter; pending signals live in the process, not the
            // descriptor, so nothing is lost by closing the old one. The set is
            // `want`, not `caught`, because an interactive shell's blocked
            // `SIGINT` has to be read (and dropped) at a boundary rather than
            // left pending for a trap registered much later.
            self.signals.reader = if want.is_empty() {
                None
            } else {
                baash_sys::SignalFd::new(&want).ok().map(Arc::new)
            };
            self.signals.blocked = want;
        }

        // `SIG_IGN` for ignored traps, and the default action back for signals
        // that are no longer ignored.
        for signal in &ignored {
            if !self.signals.ignored.contains(signal) {
                let _ = baash_sys::set_disposition(*signal, baash_sys::Disposition::Ignore);
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
            let _ = baash_sys::set_disposition(signal, baash_sys::Disposition::Default);
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
    pub(crate) fn child_reset_signals(&self) -> baash_sys::SignalSet {
        let mut set = baash_sys::SignalSet::of(&[baash_sys::SIGPIPE]);
        for signal in self.state.traps.caught_signals() {
            set.insert(signal);
        }
        set
    }

    /// What a forked child must forget.
    ///
    /// POSIX §2.11: a subshell starts with traps cleared, except that an
    /// ignored signal stays ignored (it is a process disposition, not a trap).
    /// The child also unblocks the signals the parent blocked for its
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
    ///
    /// An untrapped `SIGINT` in an interactive shell is not dropped here: it
    /// sets [`Interp::interrupt_pending`], and the evaluator turns that into
    /// [`super::Unwind::Interrupt`] at the next safe point. That is the half of
    /// bash's `Ctrl-C` promise the block alone never delivered — the session
    /// survived, but the in-process command ran to completion.
    pub(crate) fn run_pending_signal_traps(&mut self) -> Option<Status> {
        self.sync_traps();
        let signals = self.drain_signals(&[]);
        for signal in signals {
            let action = self.state.traps.signal_action(signal).cloned();
            let Some(TrapAction::Command(code)) = action else {
                if self.interrupts_command(signal) {
                    self.interrupt_pending = true;
                }
                continue;
            };
            let outcome = self.run_trap_code(&code);
            if let Some(status) = outcome.exit {
                return Some(status);
            }
        }
        None
    }

    /// Whether draining `signal` at a boundary must abort the running command
    /// rather than be dropped.
    ///
    /// Only an untrapped `INT` in an interactive shell: bash's handler abandons
    /// the command, while a script's `SIGINT` keeps the default action and never
    /// reaches this poll. `trap '' INT` is `SIG_IGN`, so the kernel normally
    /// discards the signal — but one generated in the window before the
    /// disposition was installed still reaches the reader, and the *table*, not
    /// the kernel, is the contract: an ignored signal is dropped here exactly as
    /// `SIG_IGN` would have dropped it. A handler has already run by the time
    /// this is asked, and a handler resumes the command, which is the `Command`
    /// half excluded here.
    fn interrupts_command(&self, signal: i32) -> bool {
        signal == baash_sys::SIGINT
            && self.interactive
            && !self.in_trap
            && !matches!(
                self.state.traps.signal_action(signal),
                Some(TrapAction::Command(_)) | Some(TrapAction::Ignore)
            )
    }

    /// The interactive reader's end-of-line housekeeping.
    ///
    /// A `Ctrl-C` that arrived after the line's last command boundary belongs
    /// to no command: it is dropped here rather than left pending, which would
    /// abort the *next* line the user types. The latch is cleared at the same
    /// point, because the abort it stood for has already been reported as
    /// status 130 by [`Interp::run_named`].
    pub fn finish_interactive_line(&mut self) {
        self.interrupt_pending = false;
        self.sync_traps();
        for signal in self.drain_signals(&[]) {
            // A handler still runs: `kill -USR1` at the prompt, or a trapped
            // `INT` that arrived after the last boundary, is the user's
            // business and not this line's. An `exit` in it is a request like
            // any other.
            if let Some(TrapAction::Command(code)) = self.state.traps.signal_action(signal).cloned()
                && let Some(status) = self.run_trap_code(&code).exit
            {
                self.state.request_exit(status);
            }
            // An untrapped `INT`/`QUIT` here belongs to no command: it is
            // dropped rather than left pending, where it would abort a line the
            // user has not typed yet.
        }
    }

    /// Enter an in-process subshell's signal state, returning the parent's.
    ///
    /// The body starts with the parent's traps cleared, so its mask must be
    /// rebuilt from that table rather than inherited — but an interactive
    /// shell keeps `SIGINT`/`SIGQUIT` blocked even here, because there is no
    /// child process to take them: unblocking them would hand the next `Ctrl-C`
    /// the default action and end the session the whole design exists to keep.
    /// A body that never reaches a command boundary therefore still cannot be
    /// killed by the terminal.
    pub(crate) fn enter_scope_signals(&mut self) -> SignalState {
        let saved = std::mem::take(&mut self.signals);
        // A body still waiting on its first command boundary must not sit on
        // an unblocked terminal signal; `sync_traps` re-derives the rest.
        let mut bare = baash_sys::SignalSet::empty();
        if self.interactive {
            bare.insert(baash_sys::SIGINT);
            bare.insert(baash_sys::SIGQUIT);
        }
        let _ = baash_sys::replace_signal_mask(&bare);
        saved
    }

    /// Put the parent's signal state back after a subshell body.
    ///
    /// The body emptied the process mask and may have set `SIG_IGN` for a
    /// signal the parent does not ignore. Restoring the *table* is not enough:
    /// `sync_traps` is generation-driven, and the parent's table can be exactly
    /// the one it had before, so the state is rebuilt from `None` and the
    /// body's ignore list is kept just long enough for the stale-disposition
    /// sweep to undo it. Without this, `x=$(echo hi)` in a startup file left
    /// `SIGINT` unblocked for the rest of the session, and the next `Ctrl-C`
    /// during an in-process loop killed the shell.
    pub(crate) fn leave_scope_signals(&mut self, saved: SignalState) {
        let body_ignored = std::mem::take(&mut self.signals.ignored);
        let body_blocked = self.signals.blocked;
        self.signals = saved;
        self.signals.blocked = body_blocked;
        self.signals.ignored = body_ignored;
        // `None` forces the generation check; a cleared reader forces the mask
        // syscall even when the wanted set is unchanged, because the body set
        // the mask to something that is not it.
        self.signals.generation = None;
        self.signals.reader = None;
        self.sync_traps();
    }

    /// After a foreground child has been reaped: discard `INT`/`QUIT` (bash
    /// ignores them while it waits) and run any other trap that arrived.
    pub(crate) fn after_foreground_wait(&mut self) -> Option<Status> {
        self.sync_traps();
        let signals = self.drain_signals(&[baash_sys::SIGINT, baash_sys::SIGQUIT]);
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
    pub(crate) fn ignore_int_quit(&self) -> Option<baash_sys::SignalGuard> {
        let trapped = self.state.traps.caught_signals();
        let mut ignore = Vec::new();
        for signal in [baash_sys::SIGINT, baash_sys::SIGQUIT] {
            if !trapped.contains(&signal) {
                ignore.push(signal);
            }
        }
        if ignore.is_empty() {
            return None;
        }
        baash_sys::ignore_signals(&ignore).ok()
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
        // The handler is its own chunk, so without this its `$LINENO` would be
        // line 1 of the handler text; bash reports the line of the command the
        // trap is running for. The shell's own line state is restored
        // afterwards, so a handler cannot shift it.
        let line_base = self.line_number();
        let saved_source = self.line_source.clone();
        let saved_offset = self.line_offset;
        let saved_number = self.line_number;
        self.line_base = Some(line_base.max(0) as u32);
        self.in_trap = true;
        self.run_named("<trap>", code);
        self.in_trap = false;
        self.line_base = None;
        self.line_source = saved_source;
        self.line_offset = saved_offset;
        self.line_number = saved_number;
        let text = self.line_number.to_string();
        if self.lineno != text.as_bytes() {
            self.lineno = text.into_bytes();
        }
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
        if self.in_trap || self.debug_suppressed > 0 {
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
        if self.in_trap || self.err_trap_suppressed > 0 {
            return None;
        }
        self.err_trap_reported = true;
        self.set_status(failing);
        self.sync_specials();
        let exit = self.run_trap_code(&code).exit;
        // The handler is a list of commands, and running them clears the
        // record like any other command would; the *failure* it was reporting
        // is still the same one, so a compound wrapping this command must not
        // report it a second time.
        self.err_trap_reported = true;
        exit
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

/// Where a non-final pipeline stage's output actually went.
///
/// `file` stays `None` while the stage is expected to spawn an external
/// command, whose writer is a different process and can use the pipe. It is
/// filled when the stage turns out to be shell code: descriptor 1 has been
/// moved to an unlinked temporary file and the next stage must read that
/// instead of the pipe.
#[derive(Debug)]
pub(crate) struct PipelineBuffer {
    /// The capture file, once the swap happened.
    pub(crate) file: Option<baash_sys::OwnedFd>,
}

impl PipelineBuffer {
    /// Bytes buffered so far, if the swap happened.
    pub(crate) fn size(&self) -> Option<u64> {
        let file = self.file.as_ref()?;
        let len = baash_sys::lseek(file.as_raw_fd(), 0, baash_sys::seek::END).ok()?;
        u64::try_from(len).ok()
    }
}

/// The name a `wait`/`kill` diagnostic should print for itself.
pub(crate) fn builtin_name(argv: &[Vec<u8>], fallback: &str) -> String {
    argv.first()
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .unwrap_or_else(|| fallback.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use baash_runtime::TrapKey;

    /// The one decision the command-boundary poll makes: which drained signal
    /// is an abort, and which is a trap, an ignore, or a script's business.
    #[test]
    fn only_an_untrapped_int_in_an_interactive_shell_interrupts() {
        let mut interactive = Interp::new(true);
        assert!(interactive.interrupts_command(baash_sys::SIGINT));
        // `USR1` with no trap is a trap the shell simply has no handler for;
        // it is dropped, and the command keeps running.
        assert!(!interactive.interrupts_command(baash_sys::SIGTERM));

        interactive
            .state
            .traps
            .set(TrapKey::Signal(baash_sys::SIGINT), TrapAction::Ignore);
        assert!(!interactive.interrupts_command(baash_sys::SIGINT));

        interactive.state.traps.set(
            TrapKey::Signal(baash_sys::SIGINT),
            TrapAction::Command(b"echo caught".to_vec()),
        );
        assert!(!interactive.interrupts_command(baash_sys::SIGINT));

        // A script's `SIGINT` has the default action; this poll never sees it.
        assert!(!Interp::new(false).interrupts_command(baash_sys::SIGINT));
    }

    /// The regression the `Option<u64>` generation fixes: an interactive shell
    /// with no traps at all still has to install its `SIGINT`/`SIGQUIT` block
    /// and open the reader. With a plain generation counter the first
    /// `sync_traps` returned early (generation 0 == generation 0), so a later
    /// scope that emptied the mask left the shell killable.
    #[cfg(unix)]
    #[test]
    fn an_interactive_shell_blocks_int_even_with_no_traps() {
        // Start from a known mask and put it back even if an assertion fails.
        let guard = baash_sys::SignalSet::empty().set().expect("clear the mask");
        let mut shell = Interp::new(true);
        shell.sync_traps();
        let blocked = baash_sys::SignalSet::current().expect("read the mask");
        assert!(blocked.contains(baash_sys::SIGINT));
        assert!(blocked.contains(baash_sys::SIGQUIT));

        // A scope empties the mask for its body; the interactive block must
        // survive the body and the restore.
        let saved = shell.enter_scope_signals();
        let body = baash_sys::SignalSet::current().expect("read the mask");
        assert!(body.contains(baash_sys::SIGINT));
        shell.leave_scope_signals(saved);
        let restored = baash_sys::SignalSet::current().expect("read the mask");
        assert!(restored.contains(baash_sys::SIGINT));
        assert!(restored.contains(baash_sys::SIGQUIT));
        drop(guard);
    }

    /// A non-interactive shell leaves the mask alone: a script's `SIGINT` must
    /// keep the default action, or the process would have to run forever.
    #[cfg(unix)]
    #[test]
    fn a_script_keeps_the_default_signal_disposition() {
        let guard = baash_sys::SignalSet::empty().set().expect("clear the mask");
        let mut shell = Interp::new(false);
        shell.sync_traps();
        assert!(baash_sys::SignalSet::current().expect("read").is_empty());
        drop(guard);
    }
}
