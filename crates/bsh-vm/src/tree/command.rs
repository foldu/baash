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

/// One place a command name resolved to, in search order.
///
/// This is the single description of "what would this name run?", produced by
/// [`Interp::each_candidate`]. The executor takes the first entry; `type`,
/// `command -v` and `builtin` ask the same function through
/// `ShellHost::resolve_command`, so the two can never disagree.
pub(crate) enum Resolution {
    /// A shell alias: text substituted at command position before parsing.
    Alias(Vec<u8>),
    /// A reserved word. Only a classification ever yields this; the parser
    /// consumes keywords before the executor sees a command word.
    Keyword,
    /// A shell function, by index into the function table.
    Function(usize),
    /// A body in the runtime's builtin registry.
    Builtin(&'static bsh_runtime::BuiltinDef, Tier),
    /// An executor-side built-in (see `builtins.rs`).
    Vm(&'static super::builtins::VmBuiltin, Tier),
    /// An external program, already found through `PATH`.
    External(Vec<u8>),
}

/// Where the builtin registry puts a name, before `PATH` is consulted.
#[derive(Clone, Copy)]
enum Position {
    /// A POSIX special built-in: searched before functions.
    Special(&'static bsh_runtime::BuiltinDef),
    /// A POSIX intrinsic utility: searched after functions.
    Intrinsic(&'static bsh_runtime::BuiltinDef),
    /// A regular/extension built-in whose body is the runtime's.
    Body(&'static bsh_runtime::BuiltinDef),
    /// A regular/extension built-in whose body is the executor's.
    VmBody(&'static super::builtins::VmBuiltin, Tier),
    /// A regular/extension stub: `PATH` decides, the stub is the fallback.
    PathFirst(&'static bsh_runtime::BuiltinDef),
    /// Not a builtin at all.
    NotBuiltin,
}

/// Everything one resolved command needs, apart from its body.
///
/// Bundling these keeps [`Interp::invoke`] and [`Interp::run_builtin`] to a
/// readable arity, and makes the one flag that changes *semantics* — `direct`,
/// meaning "reached through `builtin`/`command`" — explicit at the call site
/// instead of a bare trailing boolean.
struct Invocation<'a> {
    chunk: &'a Arc<ParsedChunk>,
    redirects: &'a [Redirect],
    assigns: &'a [(&'a [u8], &'a [u8])],
    argv: &'a [Vec<u8>],
    ctx: Ctx,
    /// Reached through `builtin`/`command`: POSIX §2.8.1 note 1 says a special
    /// built-in that fails there must not exit the shell.
    direct: bool,
}

/// Reserved words, for `type`/`command -v` only.
///
/// `bsh-syntax` owns the *lexer's* table and it answers a different question:
/// it over-classifies (`in`, `[[`, `]]` are coloured as keywords) and has no
/// entry for `{`/`}`, which are operators there. This list is bash's `type`
/// answer — `type -t '{'` is `keyword` — so it is spelled here rather than
/// borrowed wrongly. It is a classification table; nothing dispatches on it.
const KEYWORDS: &[&[u8]] = &[
    b"!",
    b"[[",
    b"]]",
    b"{",
    b"}",
    b"case",
    b"coproc",
    b"do",
    b"done",
    b"elif",
    b"else",
    b"esac",
    b"fi",
    b"for",
    b"function",
    b"if",
    b"in",
    b"select",
    b"then",
    b"time",
    b"until",
    b"while",
];

/// Whether `name` is a reserved word `type` reports.
fn is_keyword(name: &[u8]) -> bool {
    KEYWORDS.contains(&name)
}

impl Interp {
    pub(crate) fn exec_simple(
        &mut self,
        chunk: &Arc<ParsedChunk>,
        command: &SimpleCommand,
        span: &bsh_diag::Span,
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

        // Alias substitution happens at command position *before* anything is
        // expanded or resolved, because an alias value is shell text that may
        // change the shape of the command (`echo hi | wc -c`).
        if let Some(unwind) = self.exec_alias_command(chunk, span, ctx) {
            return unwind;
        }

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
            Some(resolution) => self.invoke(
                Invocation {
                    chunk,
                    redirects: &command.redirects,
                    assigns: &assign_views,
                    argv: &argv,
                    ctx,
                    direct: false,
                },
                resolution,
            ),
            None => self.command_not_found(&name),
        }
    }

    /// Run one resolved command.
    ///
    /// `direct` marks a command reached through `builtin`/`command`: POSIX
    /// §2.8.1 note 1 says a special built-in that fails there must not exit the
    /// shell, so its error class is rewritten before the consequence table runs.
    fn invoke(&mut self, invocation: Invocation<'_>, resolution: Resolution) -> Unwind {
        match resolution {
            Resolution::Vm(builtin, tier) => {
                self.run_builtin(&invocation, tier, BuiltinBody::Vm(builtin))
            }
            Resolution::Builtin(def, tier) => {
                self.run_builtin(&invocation, tier, BuiltinBody::Runtime(def))
            }
            Resolution::Function(index) => self.call_function(
                invocation.chunk,
                index,
                invocation.redirects,
                invocation.assigns,
                invocation.argv,
                invocation.ctx,
            ),
            Resolution::External(path) => self.run_external(
                invocation.chunk,
                &path,
                invocation.redirects,
                invocation.assigns,
                invocation.argv,
            ),
            // Neither can reach here: alias substitution happens before
            // parsing and keywords are consumed by the parser.
            Resolution::Alias(_) | Resolution::Keyword => self.command_not_found(
                invocation
                    .argv
                    .first()
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
            ),
        }
    }

    /// The `command not found` diagnostic and its POSIX §2.8.1 class.
    pub(crate) fn command_not_found(&mut self, name: &[u8]) -> Unwind {
        let mut message = b"bsh: ".to_vec();
        message.extend_from_slice(name);
        message.extend_from_slice(b": command not found\n");
        let _ = self.write_err(&message);
        self.state.record_error(ShellError::CommandNotFound);
        self.apply_consequence(ShellError::CommandNotFound, Status::NOT_FOUND)
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
        // True until the word that *names* the command has been seen. It is a
        // live question because `builtin`/`command` wrap another command word:
        // `command readonly c=$x` is a declaration just like `readonly c=$x`,
        // and its operands are assignment words that must not be split.
        let mut awaiting_command = true;
        for (index, word) in command.words.iter().enumerate() {
            let assignment_argument =
                index > 0 && declaration && bsh_syntax::is_assignment_word(source, &word.span);
            let mode = if assignment_argument {
                Mode::Assignment
            } else {
                Mode::Command
            };
            let before = argv.len();
            argv.extend(expand_word_with(word, self, mode)?);
            if !awaiting_command || argv.len() == before {
                continue;
            }
            let expanded = argv[before].as_slice();
            if matches!(expanded, b"builtin" | b"command") {
                // The next word names the command again.
                continue;
            }
            if index > 0 && expanded.len() > 1 && expanded[0] == b'-' {
                // A flag of the wrapper (`command -p export x=1`).
                continue;
            }
            declaration = matches!(
                expanded,
                b"declare" | b"typeset" | b"local" | b"export" | b"readonly"
            );
            awaiting_command = false;
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
        invocation: &Invocation<'_>,
        tier: Tier,
        body: BuiltinBody<'_>,
    ) -> Unwind {
        let Invocation {
            chunk,
            redirects,
            assigns,
            argv,
            direct,
            ..
        } = *invocation;
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
        if let Some(mut error) = self.state.take_error() {
            if direct {
                // POSIX §2.8.1 note 1: a special built-in reached through
                // `command` does not take the shell down with it. `builtin` is
                // the same escape hatch by construction — "run the built-in
                // itself" — so it gets the same treatment.
                error = error.reached_via_wrapper();
            }
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

    /// The executor's search order, as one walk.
    ///
    /// This is the *only* implementation of command resolution. Execution
    /// takes the first candidate it yields; `type`, `command -v`/`-V` and
    /// `bash`'s `builtin` ask for the same walk through `ShellHost`, so a
    /// built-in's claim about what a name means cannot drift from what running
    /// the name does.
    ///
    /// `f` returns false to stop the walk.
    pub(crate) fn each_candidate(
        &self,
        name: &[u8],
        query: bsh_runtime::ResolveQuery,
        f: &mut dyn FnMut(Resolution) -> bool,
    ) {
        // Aliases and keywords are not part of the executor's order — alias
        // substitution happens before parsing and the parser consumes
        // keywords — but they come first in a *classification*.
        if query.classify && !query.files_only {
            if let Some(value) = self.state.aliases.get(name)
                && (!f(Resolution::Alias(value.to_vec())) || !query.all)
            {
                return;
            }
            if is_keyword(name) && (!f(Resolution::Keyword) || !query.all) {
                return;
            }
        }

        let mut path_done = false;
        if !query.files_only {
            let position = self.position(name);
            if let Position::Special(def) = position
                && (!f(self.registry_body(def)) || !query.all)
            {
                return;
            }
            if !query.skip_functions
                && let Some(index) = self.function_index(name)
                && (!f(Resolution::Function(index)) || !query.all)
            {
                return;
            }
            match position {
                Position::Special(_) | Position::NotBuiltin => {}
                Position::Intrinsic(def) => {
                    if !f(self.registry_body(def)) || !query.all {
                        return;
                    }
                }
                Position::Body(def) => {
                    if !f(Resolution::Builtin(def, def.tier)) || !query.all {
                        return;
                    }
                }
                Position::VmBody(vm, tier) => {
                    if !f(Resolution::Vm(vm, tier)) || !query.all {
                        return;
                    }
                }
                Position::PathFirst(def) => {
                    // A regular/extension built-in with no body "sits where
                    // `PATH` would": the program wins and the stub is only the
                    // fallback that reports "not implemented".
                    path_done = true;
                    let hits = self.path_hits(name, query);
                    if hits.is_empty() {
                        if !f(Resolution::Builtin(def, def.tier)) || !query.all {
                            return;
                        }
                    } else {
                        for hit in hits {
                            if !f(Resolution::External(hit)) {
                                return;
                            }
                        }
                        if !query.all {
                            return;
                        }
                    }
                }
            }
        }
        if !path_done {
            for hit in self.path_hits(name, query) {
                if !f(Resolution::External(hit)) {
                    return;
                }
            }
        }
    }

    /// What the builtin registry says about `name`, excluding `PATH`.
    fn position(&self, name: &[u8]) -> Position {
        let Some(def) = lookup_builtin(name) else {
            return Position::NotBuiltin;
        };
        match def.tier {
            Tier::Special => Position::Special(def),
            Tier::Intrinsic => Position::Intrinsic(def),
            _ => {
                if def.is_implemented() {
                    Position::Body(def)
                } else if let Some(vm) = super::builtins::lookup(name) {
                    Position::VmBody(vm, def.tier)
                } else {
                    Position::PathFirst(def)
                }
            }
        }
    }

    /// The body a `Special`/`Intrinsic` entry contributes.
    ///
    /// A runtime body wins; the executor's body runs only while the registry
    /// entry has none, which is what keeps the ownership boundary honest.
    pub(crate) fn registry_body(&self, def: &'static bsh_runtime::BuiltinDef) -> Resolution {
        match super::builtins::lookup(def.name.as_bytes()) {
            Some(vm) if !def.is_implemented() => Resolution::Vm(vm, def.tier),
            _ => Resolution::Builtin(def, def.tier),
        }
    }

    /// The newest definition of `name`, if any.
    fn function_index(&self, name: &[u8]) -> Option<usize> {
        self.functions
            .iter()
            .rposition(|function| function.name == name)
    }

    /// Resolve a command word for execution: the first candidate, and a
    /// `PATH` hit is remembered in the hash table.
    fn resolve(&mut self, name: &[u8]) -> Option<Resolution> {
        let mut found = None;
        self.each_candidate(
            name,
            bsh_runtime::ResolveQuery::EXECUTION,
            &mut |candidate| {
                found = Some(candidate);
                false
            },
        );
        if let Some(Resolution::External(path)) = &found
            && !name.contains(&b'/')
        {
            // bash's hash table: a command found through `PATH` is remembered
            // until `hash -r`, so a program that appears earlier in `PATH`
            // later does not take over. `hash` exists to make that visible.
            self.path_cache.insert(name.to_vec(), path.clone());
        }
        found
    }

    /// One `PATH` hit, for `hash`'s "remember this name" search.
    pub(crate) fn path_hits_for_hash(
        &self,
        name: &[u8],
        query: bsh_runtime::ResolveQuery,
    ) -> Option<Vec<u8>> {
        self.path_hits(name, query).into_iter().next()
    }

    /// Every `PATH` hit for `name`, in `PATH` order.
    ///
    /// A name containing `/` is used as written and never searched. The hash
    /// table answers first unless the caller asked for a fresh search
    /// (`type -P`) or for every hit (`-a`).
    fn path_hits(&self, name: &[u8], query: bsh_runtime::ResolveQuery) -> Vec<Vec<u8>> {
        if name.contains(&b'/') {
            return executable(name)
                .then(|| name.to_vec())
                .into_iter()
                .collect();
        }
        if !query.fresh
            && !query.all
            && let Some(path) = self.path_cache.get(name)
        {
            return vec![path.clone()];
        }
        let path = match query.default_path {
            true => DEFAULT_PATH.to_vec(),
            false => self
                .state
                .vars
                .scalar(b"PATH")
                .map(<[u8]>::to_vec)
                .unwrap_or_else(|| DEFAULT_PATH.to_vec()),
        };
        let mut hits = Vec::new();
        for directory in path.split(|&byte| byte == b':') {
            let mut candidate = if directory.is_empty() {
                b".".to_vec()
            } else {
                directory.to_vec()
            };
            candidate.push(b'/');
            candidate.extend_from_slice(name);
            if executable(&candidate) {
                hits.push(candidate);
                if !query.all {
                    break;
                }
            }
        }
        hits
    }

    /// `PATH` search for one program, as `exec`, `.`/`source` and pipelines
    /// need it. The hash table is consulted, so `hash -r` matters here too.
    pub(crate) fn search_path(&self, name: &[u8]) -> Option<Vec<u8>> {
        self.path_hits(name, bsh_runtime::ResolveQuery::EXECUTION)
            .into_iter()
            .next()
    }

    /// One candidate described for `type`/`command -V`.
    pub(crate) fn candidate_info(&self, candidate: Resolution) -> bsh_runtime::CommandInfo {
        use bsh_runtime::{CommandInfo, CommandKind};
        match candidate {
            Resolution::Alias(value) => CommandInfo {
                kind: CommandKind::Alias,
                target: value,
            },
            Resolution::Keyword => CommandInfo::plain(CommandKind::Keyword),
            Resolution::Function(index) => {
                let name = self
                    .functions
                    .get(index)
                    .map(|function| function.name.clone())
                    .unwrap_or_default();
                CommandInfo {
                    kind: CommandKind::Function,
                    target: self.function_body(&name).unwrap_or_default(),
                }
            }
            Resolution::Builtin(def, _tier) => CommandInfo::plain(CommandKind::Builtin(def.tier)),
            Resolution::Vm(_, tier) => CommandInfo::plain(CommandKind::Builtin(tier)),
            Resolution::External(path) => CommandInfo {
                kind: CommandKind::External,
                target: path,
            },
        }
    }

    /// Run one resolved command with no redirections and no prefix
    /// assignments, folding the unwind back into a status.
    ///
    /// This is the execution half of the `builtin`/`command` seams: the inner
    /// command is not a separate shell command, so an unwind it produces
    /// (`exit` inside `builtin exit`) is re-published rather than returned.
    pub(crate) fn run_direct(&mut self, resolution: Resolution, argv: &[Vec<u8>]) -> Status {
        let chunk = self.empty_chunk.clone();
        let unwind = self.invoke(
            Invocation {
                chunk: &chunk,
                redirects: &[],
                assigns: &[],
                argv,
                ctx: Ctx::default(),
                direct: true,
            },
            resolution,
        );
        match unwind {
            Unwind::Status(status) => status,
            Unwind::Exit(status) => {
                self.state.request_exit(status);
                status
            }
            // `break`/`continue`/`return` from a direct built-in still has to
            // reach its handler in the enclosing loop or function.
            other => {
                self.pending = Some(other);
                Status::SUCCESS
            }
        }
    }

    /// The text `type` prints for a function: the body as it was written.
    pub(crate) fn function_body(&self, name: &[u8]) -> Option<Vec<u8>> {
        let index = self.function_index(name)?;
        let function = self.functions.get(index)?;
        let span = function.chunk.ast.node(function.body).span.clone();
        Some(function.chunk.source.bytes()[span].to_vec())
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
