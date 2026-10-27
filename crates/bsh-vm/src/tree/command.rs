//! Simple commands: expansion, the search order, and the per-command sequence.
//!
//! The sequence is fixed by `docs/contracts.md` and implemented literally:
//!
//! ```text
//! resolve the command's tier
//! → Vars::apply_prefix_assignments(assigns, tier)
//! → run the command
//! → Vars::finish_prefix_assignments()
//! → ShellState::take_error() → consequence(error, interactive)
//! → ShellState::take_exit_request()
//! ```
//!
//! Search order (POSIX §2.9.1.4, DESIGN.md §3.3, and `bsh-runtime`'s own
//! documentation): special built-in, function, intrinsic utility, then the
//! `PATH` position — where the *regular* built-ins sit alongside real
//! executables, which is why `printf`/`test`/`[`/`pwd` may be satisfied by a
//! program in `PATH` while `cd` may not.

use std::sync::Arc;

use bsh_lang::{expand_assignment, expand_words};
use bsh_runtime::{ShellError, ShellHost, Status, Tier, lookup_builtin};
use bsh_syntax::{Redirect, SimpleCommand};
use bsh_sys::SpawnPlan;

use super::{Ctx, Interp, ParsedChunk, Unwind};

/// Where the shell finds commands when `PATH` is unset.
const DEFAULT_PATH: &[u8] = b"/usr/local/bin:/usr/bin:/bin";

/// What a command name resolved to.
enum Target {
    /// A body in the runtime's builtin registry.
    Builtin(&'static bsh_runtime::BuiltinDef, Tier),
    /// An executor-side built-in (see `builtins.rs`).
    Vm(&'static super::builtins::VmBuiltin),
    /// A shell function, by index into the function table.
    Function(usize),
    /// An external program, already found through `PATH`.
    External(Vec<u8>),
    /// Nothing at all.
    NotFound,
}

impl Interp {
    pub(crate) fn exec_simple(
        &mut self,
        chunk: &Arc<ParsedChunk>,
        command: &SimpleCommand,
        ctx: Ctx,
    ) -> Unwind {
        self.reap_background();
        // Errors are per-command: an unconsumed class from an earlier command
        // must not be applied to this one.
        self.state.clear_error();
        self.sync_specials();
        if !command.array_args.is_empty() {
            self.report(
                crate::codes::DEFERRED,
                command.array_args[0].span.clone(),
                "array literals are not implemented yet",
            );
            return Unwind::Status(Status(2));
        }

        // 1. The command words.
        self.last_cmdsub = None;
        let argv = match expand_words(&command.words, self) {
            Ok(argv) => argv,
            Err(error) => return self.expansion_failed(error),
        };

        // 2. The prefix assignments, whose values may run command
        //    substitutions of their own.
        let mut assigns: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(command.assignments.len());
        for assignment in &command.assignments {
            let name = chunk.source.bytes()[assignment.name.clone()].to_vec();
            if assignment.index.is_some() || assignment.array.is_some() {
                self.report(
                    crate::codes::DEFERRED,
                    assignment.span.clone(),
                    "array assignment is not implemented yet",
                );
                return Unwind::Status(Status(2));
            }
            let value = match expand_assignment(&assignment.value, self) {
                Ok(value) => value,
                Err(error) => return self.expansion_failed(error),
            };
            let value = if assignment.append {
                let mut joined = self.state.vars.scalar(&name).unwrap_or_default().to_vec();
                joined.extend_from_slice(&value);
                joined
            } else {
                value
            };
            assigns.push((name, value));
        }
        let assignment_status = self.last_cmdsub.take().unwrap_or(Status::SUCCESS);

        // 3. No command word: the assignments persist, the redirections are
        //    scoped to this command (POSIX §2.9.1.4).
        if argv.is_empty() {
            return self.exec_assignments_only(
                chunk,
                &command.redirects,
                &assigns,
                assignment_status,
            );
        }

        let name = argv[0].clone();
        let assign_views: Vec<(&[u8], &[u8])> = assigns
            .iter()
            .map(|(name, value)| (name.as_slice(), value.as_slice()))
            .collect();

        // 4. `exec` needs the redirect actions rather than a save/restore, so
        //    it is dispatched before the generic path.
        if name == b"exec" {
            return self.exec_process(chunk, &command.redirects, &assign_views, &argv);
        }

        match self.resolve(&name) {
            Target::Vm(builtin) => {
                let tier = super::builtins::tier_of(builtin.name);
                self.run_builtin(
                    chunk,
                    &command.redirects,
                    &assign_views,
                    tier,
                    &argv,
                    BuiltinBody::Vm(builtin),
                )
            }
            Target::Builtin(def, tier) => self.run_builtin(
                chunk,
                &command.redirects,
                &assign_views,
                tier,
                &argv,
                BuiltinBody::Runtime(def),
            ),
            Target::Function(index) => {
                self.call_function(chunk, index, &command.redirects, &assign_views, &argv, ctx)
            }
            Target::External(path) => {
                self.run_external(chunk, &path, &command.redirects, &assign_views, &argv)
            }
            Target::NotFound => {
                let mut message = b"bsh: ".to_vec();
                message.extend_from_slice(&name);
                message.extend_from_slice(b": command not found\n");
                let _ = self.write_err(&message);
                self.state.record_error(ShellError::CommandNotFound);
                let status = Status::NOT_FOUND;
                self.apply_consequence(ShellError::CommandNotFound, status)
            }
        }
    }

    /// Commands with only assignments and redirections: `x=1`, `>file`.
    fn exec_assignments_only(
        &mut self,
        chunk: &Arc<ParsedChunk>,
        redirects: &[Redirect],
        assigns: &[(Vec<u8>, Vec<u8>)],
        status: Status,
    ) -> Unwind {
        let saved = match self.apply_redirects(chunk, redirects) {
            Ok(saved) => saved,
            Err(()) => {
                let error = ShellError::Utility {
                    status: Status::FAILURE,
                    redirection: true,
                };
                return self.apply_consequence(error, Status::FAILURE);
            }
        };
        for (name, value) in assigns {
            if let Err(error) = self.state.vars.assign_scalar(name, value) {
                self.report(crate::codes::ASSIGNMENT, 0..0, error.message());
                self.restore_fds(saved);
                self.state.record_error(ShellError::VariableAssignment);
                return self.apply_consequence(ShellError::VariableAssignment, Status::FAILURE);
            }
            if self.state.opts.allexport {
                let _ = self.state.vars.mark_exported(name);
            }
        }
        self.restore_fds(saved);
        Unwind::Status(status)
    }

    /// Apply the sequence around a built-in body.
    fn run_builtin(
        &mut self,
        chunk: &Arc<ParsedChunk>,
        redirects: &[Redirect],
        assigns: &[(&[u8], &[u8])],
        tier: Tier,
        argv: &[Vec<u8>],
        body: BuiltinBody<'_>,
    ) -> Unwind {
        let special = tier == Tier::Special;
        let saved = match self.apply_redirects(chunk, redirects) {
            Ok(saved) => saved,
            Err(()) => {
                let error = if special {
                    ShellError::SpecialBuiltinRedirection
                } else {
                    ShellError::Utility {
                        status: Status::FAILURE,
                        redirection: true,
                    }
                };
                return self.apply_consequence(error, Status::FAILURE);
            }
        };
        let applied = match self.state.vars.apply_prefix_assignments(assigns, tier) {
            Ok(applied) => applied,
            Err(error) => {
                self.restore_fds(saved);
                self.report(crate::codes::ASSIGNMENT, 0..0, error.message());
                self.state.record_error(ShellError::VariableAssignment);
                return self.apply_consequence(ShellError::VariableAssignment, Status::FAILURE);
            }
        };

        let status = {
            let slices: Vec<&[u8]> = argv.iter().map(Vec::as_slice).collect();
            match body {
                BuiltinBody::Vm(builtin) => (builtin.run)(self, argv),
                BuiltinBody::Runtime(def) => def.run(self, &slices),
            }
        };

        let _ = self.state.vars.finish_prefix_assignments(applied);
        self.restore_fds(saved);

        if let Some(unwind) = self.pending.take() {
            return unwind;
        }
        // An explicit exit request wins over the consequence table: `exit 7`
        // returns 7, not the table's status for "a special built-in failed"
        // (POSIX §2.8.1 note 1), because `exit`'s non-zero status is the
        // request itself rather than a failure of the utility.
        if let Some(exit) = self.state.take_exit_request() {
            self.state.clear_error();
            return Unwind::Exit(exit);
        }
        if let Some(error) = self.state.take_error() {
            if !status.is_success() || matches!(error, ShellError::SpecialBuiltin { .. }) {
                let unwind = self.apply_consequence(error, status);
                if let Unwind::Exit(_) = unwind {
                    return unwind;
                }
            }
        }
        Unwind::Status(status)
    }

    fn call_function(
        &mut self,
        chunk: &Arc<ParsedChunk>,
        index: usize,
        redirects: &[Redirect],
        assigns: &[(&[u8], &[u8])],
        argv: &[Vec<u8>],
        ctx: Ctx,
    ) -> Unwind {
        let Some(function) = self.functions.get(index).cloned() else {
            self.report(
                crate::codes::INTERNAL,
                chunk.ast.node(chunk.ast.root).span.clone(),
                "the function table changed under the evaluator",
            );
            return Unwind::Status(Status::FAILURE);
        };
        if self.depth >= super::MAX_FUNCTION_DEPTH {
            self.report(
                crate::codes::CONTROL_FLOW,
                0..0,
                format!(
                    "maximum function nesting depth ({}) exceeded",
                    super::MAX_FUNCTION_DEPTH
                ),
            );
            return Unwind::Status(Status::FAILURE);
        }
        let saved = match self.apply_redirects(chunk, redirects) {
            Ok(saved) => saved,
            Err(()) => {
                self.state.record_error(ShellError::FunctionRedirection);
                return Unwind::Status(Status::FAILURE);
            }
        };
        let applied = match self
            .state
            .vars
            .apply_prefix_assignments(assigns, Tier::Regular)
        {
            Ok(applied) => applied,
            Err(error) => {
                self.restore_fds(saved);
                self.report(crate::codes::ASSIGNMENT, 0..0, error.message());
                self.state.record_error(ShellError::VariableAssignment);
                return self.apply_consequence(ShellError::VariableAssignment, Status::FAILURE);
            }
        };

        self.state.vars.push_frame();
        self.state.vars.set_positional(argv[1..].to_vec());
        self.depth += 1;
        let result = self.eval_node(&function.chunk, function.body, ctx);
        self.depth -= 1;
        let frame = self.state.vars.pop_frame();
        let _ = self.state.vars.finish_prefix_assignments(applied);
        self.restore_fds(saved);
        if let Err(error) = frame {
            self.report(crate::codes::INTERNAL, 0..0, error.message());
        }

        match result {
            Unwind::Return(status) => Unwind::Status(status),
            other => other,
        }
    }

    fn run_external(
        &mut self,
        chunk: &Arc<ParsedChunk>,
        path: &[u8],
        redirects: &[Redirect],
        assigns: &[(&[u8], &[u8])],
        argv: &[Vec<u8>],
    ) -> Unwind {
        // Redirections are expanded and opened *before* the prefix assignments
        // are applied, because POSIX orders redirections before assignments in
        // a simple command (XCU 2.9.1).
        let (actions, keep) = match self.compose_actions(chunk, redirects) {
            Ok(composed) => composed,
            Err(()) => {
                self.state.record_error(ShellError::Utility {
                    status: Status::FAILURE,
                    redirection: true,
                });
                return Unwind::Status(Status::FAILURE);
            }
        };
        let applied = match self
            .state
            .vars
            .apply_prefix_assignments(assigns, Tier::Regular)
        {
            Ok(applied) => applied,
            Err(error) => {
                self.report(crate::codes::ASSIGNMENT, 0..0, error.message());
                self.state.record_error(ShellError::VariableAssignment);
                return self.apply_consequence(ShellError::VariableAssignment, Status::FAILURE);
            }
        };

        let mut envp = self.state.vars.envp();
        for entry in &applied.env {
            // Later entries win, and there must be exactly one entry per name.
            if let Some(equals) = entry.iter().position(|&byte| byte == b'=') {
                let name = &entry[..equals];
                envp.retain(|existing| {
                    existing
                        .iter()
                        .position(|&b| b == b'=')
                        .is_none_or(|at| &existing[..at] != name)
                });
            }
            envp.push(entry.clone());
        }

        let plan = SpawnPlan::new_with_env(path, argv.iter().map(Vec::as_slice), &envp);
        let mut plan = match plan {
            Ok(plan) => plan,
            Err(error) => {
                let _ = self.state.vars.finish_prefix_assignments(applied);
                self.report(
                    crate::codes::EXEC,
                    0..0,
                    format!("{}: {error}", String::from_utf8_lossy(path)),
                );
                return Unwind::Status(self.spawn_status(&error, path));
            }
        };
        plan.actions = actions;
        // The shell blocks nothing it exposes, so the child starts with an
        // empty mask. `SIGPIPE` is reset because Rust's runtime ignores it in
        // the shell, and `exec` preserves `SIG_IGN` — a child that inherits the
        // ignore would never die on a closed pipe (DESIGN.md §4.3, §4.5).
        plan.sigmask = Some(bsh_sys::SignalSet::empty());
        plan.reset_signals = bsh_sys::SignalSet::of(&[bsh_sys::SIGPIPE]);

        let result = bsh_sys::spawn(plan);
        drop(keep);
        let _ = self.state.vars.finish_prefix_assignments(applied);

        match result {
            Ok(child) => match bsh_sys::wait_pid(child.pid(), false) {
                Ok(outcome) => Unwind::Status(Status(outcome.exit_status())),
                Err(error) => {
                    self.report(
                        crate::codes::INTERNAL,
                        0..0,
                        format!("{}: cannot wait: {error}", String::from_utf8_lossy(path)),
                    );
                    Unwind::Status(Status::FAILURE)
                }
            },
            Err(error) => {
                let status = self.spawn_status(&error, path);
                self.report(
                    crate::codes::EXEC,
                    0..0,
                    format!("{}: {error}", String::from_utf8_lossy(path)),
                );
                if status == Status::NOT_FOUND {
                    self.state.record_error(ShellError::CommandNotFound);
                    return self.apply_consequence(ShellError::CommandNotFound, status);
                }
                Unwind::Status(status)
            }
        }
    }

    /// `exec`: replace the process.
    ///
    /// `bsh-sys` exposes no in-place `execve` (it would need `unsafe`), so the
    /// child is spawned, waited for and the shell exits with its status. That
    /// is observationally equivalent except for the pid; `exec` with
    /// *only* redirections is exact, because those are applied to the shell's
    /// own descriptors and not restored.
    fn exec_process(
        &mut self,
        chunk: &Arc<ParsedChunk>,
        redirects: &[Redirect],
        assigns: &[(&[u8], &[u8])],
        argv: &[Vec<u8>],
    ) -> Unwind {
        let applied = match self
            .state
            .vars
            .apply_prefix_assignments(assigns, Tier::Special)
        {
            Ok(applied) => applied,
            Err(error) => {
                self.report(crate::codes::ASSIGNMENT, 0..0, error.message());
                self.state.record_error(ShellError::VariableAssignment);
                return self.apply_consequence(ShellError::VariableAssignment, Status::FAILURE);
            }
        };

        if argv.len() == 1 {
            let result = self.apply_redirects_committed(chunk, redirects);
            let _ = self.state.vars.finish_prefix_assignments(applied);
            return match result {
                Ok(()) => Unwind::Status(Status::SUCCESS),
                Err(()) => Unwind::Status(Status::FAILURE),
            };
        }

        let Some(program) = self.search_path(&argv[1]) else {
            let _ = self.state.vars.finish_prefix_assignments(applied);
            let mut message = b"bsh: exec: ".to_vec();
            message.extend_from_slice(&argv[1]);
            message.extend_from_slice(b": not found\n");
            let _ = self.write_err(&message);
            return Unwind::Status(Status::NOT_FOUND);
        };
        let (actions, keep) = match self.compose_actions(chunk, redirects) {
            Ok(composed) => composed,
            Err(()) => {
                let _ = self.state.vars.finish_prefix_assignments(applied);
                return Unwind::Status(Status::FAILURE);
            }
        };
        let mut envp = self.state.vars.envp();
        for entry in &applied.env {
            envp.push(entry.clone());
        }
        let mut plan =
            match SpawnPlan::new_with_env(&program, argv[1..].iter().map(Vec::as_slice), &envp) {
                Ok(plan) => plan,
                Err(error) => {
                    let _ = self.state.vars.finish_prefix_assignments(applied);
                    self.report(crate::codes::EXEC, 0..0, error.to_string());
                    return Unwind::Status(Status::NOT_EXECUTABLE);
                }
            };
        plan.actions = actions;
        plan.sigmask = Some(bsh_sys::SignalSet::empty());
        plan.reset_signals = bsh_sys::SignalSet::of(&[bsh_sys::SIGPIPE]);
        // In place: on success this never returns. On failure the process image
        // is intact, but the plan's fd actions and signal attributes have
        // already been applied — inherent to exec ordering — so the only sound
        // response is to report and exit, which is what follows. `keep` must
        // stay alive across the call: it holds the descriptors the actions
        // reference.
        let error = bsh_sys::exec(plan);
        drop(keep);
        let _ = self.state.vars.finish_prefix_assignments(applied);
        let status = self.spawn_status(&error, &program);
        self.report(
            crate::codes::EXEC,
            0..0,
            format!("{}: {error}", String::from_utf8_lossy(&program)),
        );
        Unwind::Exit(status)
    }

    /// The `execve` failure table from `docs/contracts.md`.
    fn spawn_status(&self, error: &std::io::Error, _program: &[u8]) -> Status {
        match error.kind() {
            std::io::ErrorKind::NotFound => Status::NOT_FOUND,
            std::io::ErrorKind::PermissionDenied => Status::NOT_EXECUTABLE,
            _ => Status::NOT_EXECUTABLE,
        }
    }

    /// POSIX search order, minus the function and special cases the caller
    /// has already handled.
    fn resolve(&mut self, name: &[u8]) -> Target {
        if let Some(def) = lookup_builtin(name) {
            if def.tier == Tier::Special {
                return match super::builtins::lookup(name) {
                    Some(builtin) if !def.is_implemented() => Target::Vm(builtin),
                    _ => Target::Builtin(def, Tier::Special),
                };
            }
        }
        if let Some(index) = self
            .functions
            .iter()
            .rposition(|function| function.name == name)
        {
            return Target::Function(index);
        }
        if let Some(def) = lookup_builtin(name) {
            if def.tier == Tier::Intrinsic {
                return match super::builtins::lookup(name) {
                    Some(builtin) if !def.is_implemented() => Target::Vm(builtin),
                    _ => Target::Builtin(def, Tier::Intrinsic),
                };
            }
            if def.is_implemented() {
                return Target::Builtin(def, def.tier);
            }
            if def.tier == Tier::Extension {
                return match super::builtins::lookup(name) {
                    Some(builtin) => Target::Vm(builtin),
                    None => Target::Builtin(def, def.tier),
                };
            }
            // A regular built-in "sits where PATH would be". An executor-side
            // body (today only `echo`) shadows the program, as a bash built-in
            // does; otherwise the real program is used, and if there is none
            // the registry reports that the body is not written yet.
            if let Some(builtin) = super::builtins::lookup(name) {
                return Target::Vm(builtin);
            }
            return match self.search_path(name) {
                Some(path) => Target::External(path),
                None => Target::Builtin(def, def.tier),
            };
        }
        match self.search_path(name) {
            Some(path) => Target::External(path),
            None => Target::NotFound,
        }
    }

    /// `PATH` search. A name containing `/` is used as written; `PATH` is only
    /// consulted for bare names. Empty entries mean the current directory.
    pub(crate) fn search_path(&self, name: &[u8]) -> Option<Vec<u8>> {
        if name.contains(&b'/') {
            return executable(name).then(|| name.to_vec());
        }
        let path = self
            .state
            .vars
            .scalar(b"PATH")
            .map(<[u8]>::to_vec)
            .unwrap_or_else(|| DEFAULT_PATH.to_vec());
        for directory in path.split(|&byte| byte == b':') {
            let mut candidate = if directory.is_empty() {
                b".".to_vec()
            } else {
                directory.to_vec()
            };
            candidate.push(b'/');
            candidate.extend_from_slice(name);
            if executable(&candidate) {
                return Some(candidate);
            }
        }
        None
    }

    /// Reap finished background children without blocking. `SIGCHLD` is a
    /// wakeup hint, not a per-child event (DESIGN.md §4.5), so this is a
    /// drain-in-the-loop, not a handler.
    fn reap_background(&mut self) {
        while bsh_sys::wait_any_nohang().is_some() {}
    }
}

enum BuiltinBody<'a> {
    Vm(&'static super::builtins::VmBuiltin),
    Runtime(&'a bsh_runtime::BuiltinDef),
}

/// Whether `path` names a regular file with an execute bit set.
fn executable(path: &[u8]) -> bool {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    let path = std::path::Path::new(std::ffi::OsStr::from_bytes(path));
    path.metadata()
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
