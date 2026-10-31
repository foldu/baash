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
pub(crate) const DEFAULT_PATH: &[u8] = b"/usr/local/bin:/usr/bin:/bin";

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
pub(crate) const KEYWORDS: &[&[u8]] = &[
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
                // `a[]=x` has an *empty literal* subscript. Bash rejects it
                // (`a[]: bad array subscript`) and leaves the array alone,
                // while `a[""]=x` is element zero because the quoted empty
                // word is arithmetic zero. The word's part list is what tells
                // the two apart before expansion erases the difference.
                if subscript.parts.is_empty() {
                    self.report(
                        crate::codes::ASSIGNMENT,
                        assignment.span.clone(),
                        format!("{}[]: bad array subscript", String::from_utf8_lossy(&name)),
                    );
                    if argv.is_empty() {
                        return Unwind::Status(Status::FAILURE);
                    }
                    continue;
                }
                let subscript = match expand_assignment(subscript, self) {
                    Ok(text) => text,
                    Err(error) => return self.expansion_failed(error),
                };
                if !argv.is_empty() {
                    // An element assignment is not an environment binding:
                    // there is no `NAME=value` to hand a child. Report it and
                    // drop the assignment, but still run the command — bash
                    // does the same (with status 0), and the suite pins that
                    // for a word like `a[1 + 2]=` that the parser accepted as
                    // an assignment.
                    self.report(
                        crate::codes::ASSIGNMENT,
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

        // A circular nameref read is fatal: expansion cannot return an error,
        // so `Vars::scalar` recorded it. Stop before running anything with a
        // value that never resolved.
        if self.nameref_cycle.take() {
            self.report(
                bsh_runtime::codes::NAMEREF_CYCLE,
                span.clone(),
                "circular name reference",
            );
            return Unwind::Exit(Status::FAILURE);
        }

        // `DEBUG` runs before the command, after its words have been expanded
        // (bash sets `BASH_COMMAND` from the expanded text). A `DEBUG` handler
        // that fails is fatal only under `errexit`.
        if let Some(exit) = self.run_debug_trap() {
            return Unwind::Exit(exit);
        }

        // 3. `set -x` prints the expanded command before it runs; with no
        //    command word each assignment is its own traced command, which is
        //    also the order the assignments are applied in.
        if self.state.opts.xtrace {
            let raw: Vec<Vec<u8>> = array_literals
                .iter()
                .map(|literal| literal.raw.clone())
                .collect();
            if argv.is_empty() {
                for action in &actions {
                    match action {
                        AssignmentAction::Scalar {
                            name,
                            value,
                            append,
                        } => {
                            let mut word = name.clone();
                            if *append {
                                word.push(b'+');
                            }
                            word.push(b'=');
                            word.extend_from_slice(value);
                            super::xtrace::trace(self, &[word], &[]);
                        }
                        // An array literal is source text; its raw form is
                        // what the reference shell traces.
                        AssignmentAction::Array { name, .. } => {
                            if let Some(literal) =
                                array_literals.iter().find(|literal| &literal.name == name)
                            {
                                super::xtrace::trace(
                                    self,
                                    std::slice::from_ref(&literal.raw),
                                    std::slice::from_ref(&literal.raw),
                                );
                            }
                        }
                        AssignmentAction::Element { .. } => {}
                    }
                }
            } else {
                super::xtrace::trace(self, &argv, &raw);
            }
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
        let unwind = match self.resolve(&name) {
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
            None => {
                #[cfg(feature = "builtin-utils")]
                if let Some(unwind) =
                    self.run_embedded(chunk, &command.redirects, &assign_views, &argv)
                {
                    unwind
                } else {
                    self.command_not_found(&name)
                }
                #[cfg(not(feature = "builtin-utils"))]
                self.command_not_found(&name)
            }
        };
        // `$_` is the last argument of the command that just ran. Expansion
        // has already happened, so a `$_` inside the command saw the previous
        // value; only later commands see this one.
        if let Some(last) = argv.last() {
            let _ = self.state.vars.assign_scalar(b"_", last);
        }
        unwind
    }

    /// Run an embedded coreutils implementation because resolution found
    /// nothing.
    ///
    /// This is deliberately the last resort: `PATH` is searched first, so an
    /// external utility always wins and the feature only changes what happens
    /// when the search fails. A name with a slash in it is a path, not a
    /// utility, and is left to the ordinary "command not found" path.
    ///
    /// The utility runs in this process and writes to descriptor 1 (or 2), so
    /// the redirections are applied shell-side exactly like a built-in's, and
    /// a prefix assignment is scoped to the call.
    #[cfg(feature = "builtin-utils")]
    fn run_embedded(
        &mut self,
        chunk: &Arc<ParsedChunk>,
        redirects: &[Redirect],
        assigns: &[(&[u8], &[u8])],
        argv: &[Vec<u8>],
    ) -> Option<Unwind> {
        let name = argv.first()?;
        if name.contains(&b'/') || !bsh_utils::contains(name) {
            return None;
        }
        let saved = match self.apply_redirects(chunk, redirects) {
            Ok(saved) => saved,
            Err(()) => return Some(Unwind::Status(Status::FAILURE)),
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
                return Some(
                    self.apply_consequence(ShellError::VariableAssignment, Status::FAILURE),
                );
            }
        };
        let status = Status(bsh_utils::run(argv).unwrap_or(Status::NOT_FOUND.0));
        let _ = self.state.vars.finish_prefix_assignments(applied);
        self.restore_fds(saved);
        Some(Unwind::Status(status))
    }

    /// Run one resolved command.
    ///
    /// `direct` marks a command reached through `builtin`/`command`: POSIX
    /// §2.8.1 note 1 says a special built-in that fails there must not exit the
    /// shell, so its error class is rewritten before the consequence table runs.
    fn invoke(&mut self, invocation: Invocation<'_>, resolution: Resolution) -> Unwind {
        // A non-final pipeline stage whose output is a pipe must not write from
        // this process: the shell would block against its own read end. Only an
        // external command gets to keep the pipe, so everything else diverts
        // its output to a temporary file before it writes a byte.
        if !matches!(resolution, Resolution::External(_)) {
            if self.background_spawn {
                // `echo hi &` would have to run the built-in concurrently with
                // the shell; refuse rather than pretend it is asynchronous.
                self.background_spawn = false;
                self.report(
                    bsh_lang::codes::CAPABILITY,
                    0..0,
                    format!(
                        "&: {} is not an external command, and shell code cannot run in the background",
                        String::from_utf8_lossy(invocation.argv.first().map_or(b"?", Vec::as_slice))
                    ),
                );
                return Unwind::Status(Status::FAILURE);
            }
            self.buffer_pipeline_output();
        }
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
            let before = argv.len();
            if assignment_argument {
                // The operand of a declaration builtin arrives as one
                // `name=value` word, but only the *value* is expanded as an
                // assignment: `declare x=~/p` expands that tilde while
                // `declare "x=~/p"` keeps it, and `declare a[1]=*` must not
                // glob the subscript. So the name keeps its source text and
                // the value goes through the assignment pipeline on its own.
                match assignment_parts(source, word) {
                    Some((name, value)) => {
                        let value = expand_assignment(&value, self)?;
                        let mut bytes = name.to_vec();
                        bytes.extend_from_slice(&value);
                        argv.push(bytes);
                    }
                    None => argv.extend(expand_word_with(word, self, Mode::Assignment)?),
                }
            } else {
                argv.extend(expand_word_with(word, self, Mode::Command)?);
            }
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
        if let Err(failure) = self.apply_assignments(actions) {
            self.restore_fds(saved);
            return match failure {
                // A readonly write is a variable-assignment error, which POSIX
                // §2.8.1 makes fatal for a non-interactive shell.
                AssignmentFailure::Readonly(message) => {
                    self.report(crate::codes::ASSIGNMENT, 0..0, message);
                    self.state.record_error(ShellError::VariableAssignment);
                    self.apply_consequence(ShellError::VariableAssignment, Status::FAILURE)
                }
                // A refused operand (invalid name, bad nameref subscript) is
                // reported and the shell keeps running with status 1.
                AssignmentFailure::Refused(message) => {
                    self.report(crate::codes::ASSIGNMENT, 0..0, message);
                    Unwind::Status(Status::FAILURE)
                }
                AssignmentFailure::Fatal(message) => {
                    self.report(crate::codes::ASSIGNMENT, 0..0, message);
                    Unwind::Exit(Status::FAILURE)
                }
                AssignmentFailure::Expansion(message) => {
                    self.report(crate::codes::ARITHMETIC, 0..0, message);
                    self.abort_expansion()
                }
            };
        }
        self.restore_fds(saved);
        // Array elements are expanded during `apply_assignments`, so a command
        // substitution inside one contributes its status just like a scalar
        // assignment's does.
        let status = self.last_cmdsub.take().unwrap_or(status);
        Unwind::Status(status)
    }

    /// Apply expanded assignment words in order.
    fn apply_assignments(&mut self, actions: &[AssignmentAction]) -> Result<(), AssignmentFailure> {
        for action in actions {
            match action {
                AssignmentAction::Scalar {
                    name,
                    value,
                    append,
                } => {
                    // `declare -i n; n=3+4` evaluates the value; `n+=2*3`
                    // adds the evaluated word to the current integer.
                    if self.state.vars.target_has_integer(name) {
                        let evaluated = match crate::arith::eval(value, self) {
                            Ok(number) => number,
                            Err(message) => {
                                return Err(AssignmentFailure::Expansion(message));
                            }
                        };
                        let number = if *append {
                            let current = self
                                .state
                                .vars
                                .scalar(name)
                                .and_then(|text| {
                                    std::str::from_utf8(text).ok()?.trim().parse::<i64>().ok()
                                })
                                .unwrap_or(0);
                            current.wrapping_add(evaluated)
                        } else {
                            evaluated
                        };
                        self.state
                            .vars
                            .assign_scalar(name, number.to_string().as_bytes())
                            .map_err(AssignmentFailure::from_error)?;
                    } else {
                        let result = if *append {
                            self.state.vars.append(name, value)
                        } else {
                            self.state.vars.assign_scalar(name, value)
                        };
                        result.map_err(AssignmentFailure::from_error)?;
                    }
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
                    // A nameref holds a name, not a value: assigning it an
                    // array would have to replace the reference, and bash's
                    // answer (drop the attribute, write the target) is not a
                    // contract worth reproducing. Reject and stop the shell.
                    if self
                        .state
                        .vars
                        .lookup(name)
                        .is_some_and(|var| var.attrs.nameref)
                    {
                        self.state.request_exit(Status::FAILURE);
                        return Err(AssignmentFailure::Fatal(format!(
                            "{}: cannot assign array to a nameref",
                            String::from_utf8_lossy(name)
                        )));
                    }
                    self.assign_array_literal(name, elements, *append, None)
                        .map_err(AssignmentFailure::Readonly)?;
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
    ) -> Result<(), AssignmentFailure> {
        match self.state.vars.shape(name) {
            Some(ArrayShape::Associative) => {
                let result = if append {
                    self.state.vars.append_assoc(name, subscript, value)
                } else {
                    self.state.vars.set_assoc(name, subscript, value)
                };
                result.map_err(|error| AssignmentFailure::Refused(error.message()))
            }
            _ => {
                let index =
                    crate::arith::eval(subscript, self).map_err(AssignmentFailure::Expansion)?;
                let result = if append {
                    self.state.vars.append_indexed(name, index, value)
                } else {
                    self.state.vars.set_indexed(name, index, value)
                };
                result.map_err(|error| AssignmentFailure::Refused(error.message()))
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
                // A regular built-in with no body sits where `PATH` would, and
                // the stub diagnostic is what a *missing* utility gets. The
                // embedded implementation is the last resort: `PATH` was
                // already searched (the body is only reached when it found
                // nothing), and the redirections and prefix assignments are
                // applied around this call like any built-in's.
                #[cfg(feature = "builtin-utils")]
                BuiltinBody::Runtime(def)
                    if !def.is_implemented() && bsh_utils::contains(def.name.as_bytes()) =>
                {
                    Status(bsh_utils::run(argv).unwrap_or(Status::NOT_FOUND.0))
                }
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

    pub(crate) fn call_function(
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
                if self.background_spawn {
                    self.background_spawn = false;
                    let pid = child.pid();
                    // No job table: the pid alone is the handle `$!`, `wait`
                    // and `kill` use.
                    self.background.insert(pid, None);
                    self.state.vars.set_last_bg_pid(Some(pid));
                    return Unwind::Status(Status::SUCCESS);
                }
                if self.defer_next_spawn {
                    // This stage *is* the command: the next stage starts before
                    // it is waited for, so the stages overlap. `run_pipeline`
                    // collects the status.
                    self.defer_next_spawn = false;
                    self.pending_children.push(child);
                    return Unwind::Status(Status::SUCCESS);
                }
                let pid = child.pid();
                let outcomes = self.wait_foreground_children(&[pid]);
                match outcomes {
                    Ok(outcomes) => {
                        let status = outcomes
                            .last()
                            .map(|outcome| Status(outcome.exit_status()))
                            .unwrap_or(Status::FAILURE);
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
            Err(error) if error.raw_os_error() == Some(bsh_sys::ENOEXEC) => {
                // A file with no `#!` is a shell script by convention. Every
                // shell interprets it itself rather than failing; we already
                // have an interpreter here, so no `/bin/sh` is spawned.
                self.run_without_shebang(chunk, path, redirects, assigns, argv)
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

    /// Run an executable file that has no `#!` line as a shell script.
    fn run_without_shebang(
        &mut self,
        chunk: &Arc<ParsedChunk>,
        path: &[u8],
        redirects: &[Redirect],
        assigns: &[(&[u8], &[u8])],
        argv: &[Vec<u8>],
    ) -> Unwind {
        let bytes = match std::fs::read(bsh_sys::os::path_from_bytes(path)) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.report(
                    crate::codes::EXEC,
                    0..0,
                    format!("{}: {error}", String::from_utf8_lossy(path)),
                );
                return Unwind::Status(Status::NOT_EXECUTABLE);
            }
        };
        let scope = self.open_scope();
        let saved = match self.apply_redirects(chunk, redirects) {
            Ok(saved) => saved,
            Err(()) => {
                self.close_scope(scope);
                return Unwind::Status(Status::FAILURE);
            }
        };
        let applied = self
            .state
            .vars
            .apply_prefix_assignments(assigns, bsh_runtime::Tier::Regular)
            .ok();
        self.set_arg0(path.to_vec());
        self.set_positional(argv[1..].to_vec());
        let name = String::from_utf8_lossy(path).into_owned();
        let status = self.run_named(&name, &bytes);
        if let Some(applied) = applied {
            let _ = self.state.vars.finish_prefix_assignments(applied);
        }
        self.restore_fds(saved);
        self.close_scope(scope);
        Unwind::Status(status)
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

        // `exec`'s own options end at `--` or the first word that is not one
        // of them. `-a NAME` sets the child's `argv[0]` without changing which
        // program is executed (bash's only exec option).
        let mut index = 1;
        let mut child_argv0: Option<Vec<u8>> = None;
        while let Some(argument) = argv.get(index) {
            match argument.as_slice() {
                b"--" => {
                    index += 1;
                    break;
                }
                b"-a" => {
                    let Some(name) = argv.get(index + 1) else {
                        let _ = self.state.vars.finish_prefix_assignments(applied);
                        let _ = self.write_err(b"bsh: exec: -a: option requires an argument\n");
                        return Unwind::Status(Status::FAILURE);
                    };
                    child_argv0 = Some(name.clone());
                    index += 2;
                }
                option if option.starts_with(b"-a") && option.len() > 2 => {
                    child_argv0 = Some(option[2..].to_vec());
                    index += 1;
                }
                _ => break,
            }
        }

        if index >= argv.len() {
            let result = self.apply_redirects_committed(chunk, redirects);
            let _ = self.state.vars.finish_prefix_assignments(applied);
            return match result {
                Ok(()) => Unwind::Status(Status::SUCCESS),
                Err(()) => Unwind::Status(Status::FAILURE),
            };
        }

        let Some(program) = self.search_path(&argv[index]) else {
            let _ = self.state.vars.finish_prefix_assignments(applied);
            let mut message = b"bsh: exec: ".to_vec();
            message.extend_from_slice(&argv[index]);
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
        // The child's `argv[0]` is the program as written, unless `-a` named
        // another; the rest of the words follow unchanged.
        let mut child_argv: Vec<&[u8]> = argv[index..].iter().map(Vec::as_slice).collect();
        if let Some(name) = &child_argv0 {
            child_argv[0] = name;
        }
        let mut plan = match SpawnPlan::new_with_env(&program, child_argv.iter().copied(), &envp) {
            Ok(plan) => plan,
            Err(error) => {
                let _ = self.state.vars.finish_prefix_assignments(applied);
                self.report(crate::codes::EXEC, 0..0, error.to_string());
                return Unwind::Status(Status::NOT_EXECUTABLE);
            }
        };
        plan.actions = actions;
        plan.sigmask = Some(bsh_sys::SignalSet::empty());
        plan.reset_signals = self.child_reset_signals();
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
    pub(crate) fn function_index(&self, name: &[u8]) -> Option<usize> {
        self.functions
            .iter()
            .rposition(|function| function.name == name)
    }

    /// Resolve a command word for execution: the first candidate, and a
    /// `PATH` hit is remembered in the hash table.
    fn resolve(&mut self, name: &[u8]) -> Option<Resolution> {
        // A name with a slash is a path, not a search: even when the file is
        // not executable the shell has to *try* it, because the failure is
        // EACCES (status 126) rather than "command not found" (127). The
        // permission check `type`/`command -v` do still applies to them.
        if name.contains(&b'/') && std::fs::metadata(bsh_sys::os::path_from_bytes(name)).is_ok() {
            return Some(Resolution::External(name.to_vec()));
        }
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
        // `waitpid(-1)` is process-wide. With no tracked child, a drain could
        // steal a child belonging to another shell in the same process (the
        // integration tests run several interpreters in threads), so do not
        // call it at all. Foreground waits reap their own pid directly.
        if self.background.is_empty() {
            return;
        }
        while let Some((pid, outcome)) = bsh_sys::wait_any_nohang() {
            if let Some(slot) = self.background.get_mut(&pid) {
                *slot = Some(Status(outcome.exit_status()));
            }
        }
    }
}

/// How an assignment word failed.
pub(crate) enum AssignmentFailure {
    /// A variable the shell cannot write (`readonly`): fatal for a
    /// non-interactive shell.
    Readonly(String),
    /// A word that does not name a writable location (bad subscript, nameref
    /// element target): status 1, keep going.
    Refused(String),
    /// An operation the shell refuses outright: stop with status 1.
    Fatal(String),
    /// An arithmetic error while resolving an assignment target or an
    /// integer value. bash treats it as an *expansion* failure, not an
    /// assignment one: the current line is abandoned (and a non-interactive
    /// shell exits) instead of running the rest of the command.
    Expansion(String),
}

impl AssignmentFailure {
    /// The message for the caller that only reports.
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Readonly(message)
            | Self::Refused(message)
            | Self::Fatal(message)
            | Self::Expansion(message) => message,
        }
    }

    fn from_error(error: bsh_runtime::Error) -> Self {
        match error {
            bsh_runtime::Error::Readonly { .. } => Self::Readonly(error.message()),
            bsh_runtime::Error::NamerefCycle { .. } | bsh_runtime::Error::NamerefSelf { .. } => {
                Self::Fatal(error.message())
            }
            other => Self::Refused(other.message()),
        }
    }
}

enum BuiltinBody<'a> {
    Vm(&'static super::builtins::VmBuiltin),
    Runtime(&'a bsh_runtime::BuiltinDef),
}

/// The `name=` text and the value word of a declaration operand.
///
/// `None` when the `=` is quoted (then the word is not an assignment at all,
/// and the whole word is expanded as one value) or when the parts cannot be
/// split. The name is returned as source bytes because a declaration's name is
/// never expanded — `declare a[1]=x` keeps the literal subscript.
fn assignment_parts<'a>(
    source: &'a [u8],
    word: &bsh_lang::Word,
) -> Option<(&'a [u8], bsh_lang::Word)> {
    let start = bsh_syntax::assignment_value_start(source, &word.span)?;
    let (_, value) = word.split_unquoted(b'=', false)?;
    Some((&source[word.span.start..start], value))
}

/// Whether `path` names a regular file with an execute bit set.
pub(crate) fn executable(path: &[u8]) -> bool {
    let path = bsh_sys::os::path_from_bytes(path);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        // There is no execute bit; a regular file is the test. PATHEXT-based
        // resolution is the gap `docs/portability.md` §9 records.
        path.metadata().map(|meta| meta.is_file()).unwrap_or(false)
    }
}
