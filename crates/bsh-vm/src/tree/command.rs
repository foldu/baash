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

use bsh_lang::{Mode, expand_assignment, expand_word, expand_word_with};
use bsh_runtime::{ArrayShape, ShellError, ShellHost, Status, Tier, lookup_builtin};
use bsh_syntax::{ArrayElement, Redirect, SimpleCommand};
use bsh_sys::SpawnPlan;

use super::{ArrayLiteral, Ctx, Interp, ParsedChunk, Unwind};

/// One assignment word of a simple command, after its values have been
/// expanded but before it has been applied.
///
/// Array assignments cannot go through the scalar `(name, value)` path: the
/// literal's elements are expanded in one phase and written in a second, and
/// an element assignment needs the array's shape to know whether the
/// subscript is an index or a key.
enum AssignmentAction {
    /// `name=value` / `name+=value`.
    Scalar {
        name: Vec<u8>,
        value: Vec<u8>,
        append: bool,
    },
    /// `name[subscript]=value` / `name[subscript]+=value`; the subscript has
    /// been expanded but not yet evaluated.
    Element {
        name: Vec<u8>,
        subscript: Vec<u8>,
        value: Vec<u8>,
        append: bool,
    },
    /// `name=( … )` / `name+=( … )`.
    Array {
        name: Vec<u8>,
        elements: Vec<ArrayElement>,
        append: bool,
    },
}

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
        // A command boundary is a trap safe point (DESIGN.md §4.5): signals
        // that arrived during the previous command run now, before this one
        // starts, so a trap can never interrupt a built-in half-way.
        self.sync_traps();
        if let Some(exit) = self.run_pending_signal_traps() {
            return Unwind::Exit(exit);
        }
        if let Some(exit) = self.state.take_exit_request() {
            return Unwind::Exit(exit);
        }
        self.reap_background();
        // Errors are per-command: an unconsumed class from an earlier command
        // must not be applied to this one.
        self.state.clear_error();
        self.sync_specials();
        self.array_literals.clear();

        // Array-literal arguments (`local a=(1 2)`) keep the structured
        // elements; the `argv` entry is a quoted placeholder holding the raw
        // text, which is how the declaration built-in recognises the operand.
        let mut array_literals: Vec<ArrayLiteral> = Vec::with_capacity(command.array_args.len());
        for argument in &command.array_args {
            let raw = chunk.source.bytes()[command.words[argument.word].span.clone()].to_vec();
            let name = chunk.source.bytes()[argument.name.clone()].to_vec();
            array_literals.push(ArrayLiteral {
                name,
                raw,
                elements: argument.elements.clone(),
                append: argument.append,
            });
        }

        // 1. The command words.
        self.last_cmdsub = None;
        let argv = match self.expand_command_words(chunk, command) {
            Ok(argv) => argv,
            Err(error) => return self.expansion_failed(error),
        };

        // 2. The assignments, whose values may run command substitutions of
        //    their own. Everything is expanded in source order first; array
        //    literals defer writing to `apply_assignments` because bash
        //    expands every element before it modifies the array.
        let mut actions: Vec<AssignmentAction> = Vec::with_capacity(command.assignments.len());
        for assignment in &command.assignments {
            let name = chunk.source.bytes()[assignment.name.clone()].to_vec();
            if let Some(elements) = &assignment.array {
                if !argv.is_empty() {
                    self.report(
                        crate::codes::DEFERRED,
                        assignment.span.clone(),
                        "an array assignment cannot be part of a command's environment",
                    );
                    return Unwind::Status(Status(2));
                }
                actions.push(AssignmentAction::Array {
                    name,
                    elements: elements.clone(),
                    append: assignment.append,
                });
                continue;
            }
            let value = match expand_assignment(&assignment.value, self) {
                Ok(value) => value,
                Err(error) => return self.expansion_failed(error),
            };
            if let Some(subscript) = &assignment.index {
                let subscript = match expand_assignment(subscript, self) {
                    Ok(text) => text,
                    Err(error) => return self.expansion_failed(error),
                };
                if !argv.is_empty() {
                    // bash reports `a[0]': not a valid identifier` and runs
                    // the command without applying it.
                    self.report(
                        crate::codes::DEFERRED,
                        assignment.span.clone(),
                        format!("{}: not a valid identifier", String::from_utf8_lossy(&name)),
                    );
                    continue;
                }
                actions.push(AssignmentAction::Element {
                    name,
                    subscript,
                    value,
                    append: assignment.append,
                });
            } else {
                actions.push(AssignmentAction::Scalar {
                    name,
                    value,
                    append: assignment.append,
                });
            }
        }
        let assignment_status = self.last_cmdsub.take().unwrap_or(Status::SUCCESS);

        // `DEBUG` runs before the command, after its words have been expanded
        // (bash sets `BASH_COMMAND` from the expanded text). A `DEBUG` handler
        // that fails is fatal only under `errexit`.
        if let Some(exit) = self.run_debug_trap() {
            return Unwind::Exit(exit);
        }

        // 3. No command word: the assignments persist, the redirections are
        //    scoped to this command (POSIX §2.9.1.4).
        if argv.is_empty() {
            return self.exec_assignments_only(
                chunk,
                &command.redirects,
                &actions,
                assignment_status,
            );
        }

        // Prefix assignments are scalars by definition: an array literal is
        // not an environment binding (bash refuses one), and an element
        // assignment is rejected above.
        let mut assigns: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(actions.len());
        for action in &actions {
            if let AssignmentAction::Scalar {
                name,
                value,
                append,
            } = action
            {
                let value = if *append {
                    let mut joined = self.state.vars.scalar(name).unwrap_or_default().to_vec();
                    joined.extend_from_slice(value);
                    joined
                } else {
                    value.clone()
                };
                assigns.push((name.clone(), value));
            }
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

        self.array_literals = array_literals;
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

    /// Expand a simple command's words.
    ///
    /// Every word is a command word except the arguments of the declaration
    /// built-ins that look like `name=value`: those are expanded as assignment
    /// values, so `export x=$v` does not field-split or glob `$v` (POSIX
    /// XCU 2.9.1.1 treats them as assignment words even though they are
    /// arguments).
    fn expand_command_words(
        &mut self,
        chunk: &Arc<ParsedChunk>,
        command: &SimpleCommand,
    ) -> Result<Vec<Vec<u8>>, bsh_lang::Error> {
        let source = chunk.source.bytes();
        let mut argv: Vec<Vec<u8>> = Vec::new();
        let mut declaration = false;
        for (index, word) in command.words.iter().enumerate() {
            let assignment_argument =
                index > 0 && declaration && bsh_syntax::is_assignment_word(source, &word.span);
            let mode = if assignment_argument {
                Mode::Assignment
            } else {
                Mode::Command
            };
            argv.extend(expand_word_with(word, self, mode)?);
            if index == 0 {
                declaration = argv.first().is_some_and(|name| {
                    matches!(
                        name.as_slice(),
                        b"declare" | b"typeset" | b"local" | b"export" | b"readonly"
                    )
                });
            }
        }
        Ok(argv)
    }

    /// Commands with only assignments and redirections: `x=1`, `>file`.
    fn exec_assignments_only(
        &mut self,
        chunk: &Arc<ParsedChunk>,
        redirects: &[Redirect],
        actions: &[AssignmentAction],
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
        if let Err(message) = self.apply_assignments(actions) {
            self.report(crate::codes::ASSIGNMENT, 0..0, message);
            self.restore_fds(saved);
            self.state.record_error(ShellError::VariableAssignment);
            return self.apply_consequence(ShellError::VariableAssignment, Status::FAILURE);
        }
        self.restore_fds(saved);
        // Array elements are expanded during `apply_assignments`, so a command
        // substitution inside one contributes its status just like a scalar
        // assignment's does.
        let status = self.last_cmdsub.take().unwrap_or(status);
        Unwind::Status(status)
    }

    /// Apply expanded assignment words in order.
    fn apply_assignments(&mut self, actions: &[AssignmentAction]) -> Result<(), String> {
        for action in actions {
            match action {
                AssignmentAction::Scalar {
                    name,
                    value,
                    append,
                } => {
                    let result = if *append {
                        self.state.vars.append(name, value)
                    } else {
                        self.state.vars.assign_scalar(name, value)
                    };
                    result.map_err(|error| error.message())?;
                    if self.state.opts.allexport {
                        let _ = self.state.vars.mark_exported(name);
                    }
                }
                AssignmentAction::Element {
                    name,
                    subscript,
                    value,
                    append,
                } => {
                    self.assign_element(name, subscript, *append, value)?;
                }
                AssignmentAction::Array {
                    name,
                    elements,
                    append,
                } => {
                    self.assign_array_literal(name, elements, *append, None)?;
                }
            }
        }
        Ok(())
    }

    /// `a[sub]=v` / `a[sub]+=v`.
    ///
    /// The subscript's meaning depends on the target's shape: an associative
    /// array uses the expanded text as the key, everything else evaluates it
    /// arithmetically through the same evaluator `$((…))` uses.
    pub(crate) fn assign_element(
        &mut self,
        name: &[u8],
        subscript: &[u8],
        append: bool,
        value: &[u8],
    ) -> Result<(), String> {
        match self.state.vars.shape(name) {
            Some(ArrayShape::Associative) => {
                let result = if append {
                    self.state.vars.append_assoc(name, subscript, value)
                } else {
                    self.state.vars.set_assoc(name, subscript, value)
                };
                result.map_err(|error| error.message())
            }
            _ => {
                let index = crate::arith::eval(subscript, self)?;
                let result = if append {
                    self.state.vars.append_indexed(name, index, value)
                } else {
                    self.state.vars.set_indexed(name, index, value)
                };
                result.map_err(|error| error.message())
            }
        }
    }

    /// `a=( … )` / `a+=( … )`.
    ///
    /// Two phases on purpose, because bash expands every element before it
    /// touches the array: `a=(old; a+=("${a[@]}"))` must copy the old
    /// elements. Explicit subscripts are evaluated in the second phase, so
    /// `a=([0]=x [a[0]]=y)` sees the element written just before it.
    pub(crate) fn assign_array_literal(
        &mut self,
        name: &[u8],
        elements: &[ArrayElement],
        append: bool,
        shape: Option<ArrayShape>,
    ) -> Result<(), String> {
        let shape = shape
            .or_else(|| self.state.vars.shape(name))
            .unwrap_or(ArrayShape::Indexed);

        // Phase 1: expand values (and explicit subscripts) without writing.
        struct Prepared {
            subscript: Option<(Vec<u8>, bool)>,
            values: Vec<Vec<u8>>,
        }
        let mut prepared: Vec<Prepared> = Vec::with_capacity(elements.len());
        for element in elements {
            match &element.index {
                Some(index) => {
                    let subscript = expand_assignment(&index.subscript, self)
                        .map_err(|error| error.to_string())?;
                    let value = expand_assignment(&element.value, self)
                        .map_err(|error| error.to_string())?;
                    prepared.push(Prepared {
                        subscript: Some((subscript, index.append)),
                        values: vec![value],
                    });
                }
                None => {
                    // A bare element is an ordinary command word: it splits
                    // and globs, which is how `a=(*.c)` becomes several
                    // elements.
                    let values =
                        expand_word(&element.value, self).map_err(|error| error.to_string())?;
                    prepared.push(Prepared {
                        subscript: None,
                        values,
                    });
                }
            }
        }

        if !append {
            self.state
                .vars
                .clear_array(name, shape)
                .map_err(|error| error.message())?;
        } else if self.state.vars.shape(name).is_none() {
            // `a+=(…)` on a scalar keeps the scalar as element zero, which is
            // bash's promotion rule.
            if let Some(existing) = self.state.vars.scalar(name).map(<[u8]>::to_vec) {
                self.state
                    .vars
                    .declare_shape(name, ArrayShape::Indexed)
                    .map_err(|error| error.message())?;
                self.state
                    .vars
                    .set_indexed(name, 0, &existing)
                    .map_err(|error| error.message())?;
            } else {
                self.state
                    .vars
                    .declare_shape(name, shape)
                    .map_err(|error| error.message())?;
            }
        }

        match shape {
            ArrayShape::Indexed => {
                let mut next = if append {
                    self.state
                        .vars
                        .max_index(name)
                        .map_or(0, |index| index.saturating_add(1))
                } else {
                    0
                };
                for element in &prepared {
                    match &element.subscript {
                        Some((text, element_append)) => {
                            let index = crate::arith::eval(text, self)?;
                            let value = &element.values[0];
                            let result = if *element_append {
                                self.state.vars.append_indexed(name, index, value)
                            } else {
                                self.state.vars.set_indexed(name, index, value)
                            };
                            result.map_err(|error| error.message())?;
                            next = index.saturating_add(1);
                        }
                        None => {
                            for value in &element.values {
                                self.state
                                    .vars
                                    .set_indexed(name, next, value)
                                    .map_err(|error| error.message())?;
                                next = next.saturating_add(1);
                            }
                        }
                    }
                }
            }
            ArrayShape::Associative => {
                let has_subscripts = prepared.iter().any(|element| element.subscript.is_some());
                let has_bare = prepared.iter().any(|element| element.subscript.is_none());
                if has_subscripts && has_bare {
                    return Err(format!(
                        "{}: must use subscript when assigning associative array",
                        String::from_utf8_lossy(name)
                    ));
                }
                if has_subscripts {
                    for element in &prepared {
                        let Some((key, element_append)) = &element.subscript else {
                            continue;
                        };
                        let value = &element.values[0];
                        let result = if *element_append {
                            self.state.vars.append_assoc(name, key, value)
                        } else {
                            self.state.vars.set_assoc(name, key, value)
                        };
                        result.map_err(|error| error.message())?;
                    }
                } else {
                    // bash 5.1's `assoc=(key value …)` alternation.
                    let mut pending: Option<Vec<u8>> = None;
                    for element in &prepared {
                        for value in &element.values {
                            match pending.take() {
                                None => pending = Some(value.clone()),
                                Some(key) => {
                                    self.state
                                        .vars
                                        .set_assoc(name, &key, value)
                                        .map_err(|error| error.message())?;
                                }
                            }
                        }
                    }
                    if let Some(key) = pending {
                        self.state
                            .vars
                            .set_assoc(name, &key, b"")
                            .map_err(|error| error.message())?;
                    }
                }
            }
        }
        Ok(())
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
        // A `trap` built-in changes the table; make the dispositions agree
        // before the next command can be interrupted.
        self.sync_traps();

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

        // The DEBUG/ERR/RETURN traps are not inherited by functions unless the
        // matching `-o` option is on (bash's rule). Save the inherited ones,
        // take them out for the call, and put them back afterwards.
        let inherited = self.suspend_function_traps();
        self.state.vars.push_frame();
        self.state.vars.set_positional(argv[1..].to_vec());
        self.depth += 1;
        let result = self.eval_node(&function.chunk, function.body, ctx);
        // `RETURN` runs with the function's frame still in scope.
        if let Some(exit) = self.run_return_trap() {
            self.depth -= 1;
            let _ = self.state.vars.pop_frame();
            self.restore_function_traps(inherited);
            let _ = self.state.vars.finish_prefix_assignments(applied);
            self.restore_fds(saved);
            return Unwind::Exit(exit);
        }
        self.depth -= 1;
        let frame = self.state.vars.pop_frame();
        self.restore_function_traps(inherited);
        let _ = self.state.vars.finish_prefix_assignments(applied);
        self.restore_fds(saved);
        // A `trap` built-in changes the table; make the dispositions agree
        // before the next command can be interrupted.
        self.sync_traps();
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
        plan.reset_signals = self.child_reset_signals();

        let result = bsh_sys::spawn(plan);
        drop(keep);
        let _ = self.state.vars.finish_prefix_assignments(applied);

        match result {
            Ok(child) => {
                // bash ignores `SIGINT`/`SIGQUIT` delivered to the shell while
                // it waits for a foreground job; the job has its own copy.
                let guard = self.ignore_int_quit();
                let waited = bsh_sys::wait_pid(child.pid(), false);
                drop(guard);
                match waited {
                    Ok(outcome) => {
                        let status = Status(outcome.exit_status());
                        self.set_status(status);
                        self.sync_specials();
                        if let Some(exit) = self.after_foreground_wait() {
                            return Unwind::Exit(exit);
                        }
                        Unwind::Status(status)
                    }
                    Err(error) => {
                        self.report(
                            crate::codes::INTERNAL,
                            0..0,
                            format!("{}: cannot wait: {error}", String::from_utf8_lossy(path)),
                        );
                        Unwind::Status(Status::FAILURE)
                    }
                }
            }
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

    /// Reap finished children without blocking. `SIGCHLD` is a wakeup hint,
    /// not a per-child event (DESIGN.md §4.5), so this is a drain-in-the-loop,
    /// not a handler. Background statuses are kept: `wait PID` may be asked
    /// for a child that finished several commands ago.
    pub(crate) fn reap_background(&mut self) {
        while let Some((pid, outcome)) = bsh_sys::wait_any_nohang() {
            if let Some(slot) = self.background.get_mut(&pid) {
                *slot = Some(Status(outcome.exit_status()));
            }
        }
    }

    /// Register a child started with `&` so `wait` and `%N` can find it.
    pub(crate) fn track_background(&mut self, pid: i32) {
        self.background.insert(pid, None);
        self.jobs.retain(|candidate| *candidate != pid);
        self.jobs.push(pid);
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
