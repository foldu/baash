//! The AST walk: lists, and-or chains, pipelines, compound commands and
//! subshells.
//!
//! Every arm returns [`Unwind`], and the *only* place `errexit` fires is
//! [`Interp::eval_and_or`] — after a whole and-or list, for the syntactically
//! last operand, only if it ran, and only when the region is not an ignored
//! context (DESIGN.md §4.7). Keeping that in one function is the whole point
//! of modelling control flow as unwinding instead of `Result` at every node.

use std::sync::Arc;

use bsh_diag::Diagnostic;
use bsh_runtime::{ShellHost, Status};
use bsh_syntax::{CaseTerm, NodeId, NodeKind, PipeOp, Separator};

use super::{Ctx, Function, Interp, ParsedChunk, Unwind};

/// Which trap a [`Unwind::Trap`] is asking for.
///
/// Nothing constructs a `Trap` any more: traps are dispatched from the trap
/// table at command boundaries (`traps.rs`), which is where they belong, and
/// the variant stays as the unwinding channel of last resort rather than being
/// deleted from a public enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrapKind {
    /// `trap … EXIT`, run by the top level before it returns.
    Exit,
    /// `trap … ERR`, run when `errexit` would fire.
    Err,
    /// `trap … DEBUG`, run before each command.
    Debug,
    /// `trap … RETURN`, run on function-frame exit.
    Return,
}

impl Interp {
    /// Evaluate one node, keeping `$?` up to date.
    pub(crate) fn eval_node(&mut self, chunk: &Arc<ParsedChunk>, id: NodeId, ctx: Ctx) -> Unwind {
        let result = self.eval_kind(chunk, id, ctx);
        self.set_status(result.status());
        result
    }

    fn eval_kind(&mut self, chunk: &Arc<ParsedChunk>, id: NodeId, ctx: Ctx) -> Unwind {
        let node = chunk.ast.node(id);
        match &node.kind {
            NodeKind::Empty => Unwind::Status(Status::SUCCESS),
            NodeKind::List { items } => self.eval_list(chunk, items, ctx),
            NodeKind::AndOr { first, rest } => self.eval_and_or(chunk, *first, rest, ctx),
            NodeKind::Pipeline {
                negated,
                stages,
                links,
            } => self.eval_pipeline(chunk, *negated, stages, links, ctx),
            NodeKind::Simple(command) => {
                let span = node.span.clone();
                self.exec_simple(chunk, command, &span, ctx)
            }
            NodeKind::If {
                branches,
                else_body,
            } => {
                for branch in branches {
                    let condition = self.eval_node(chunk, branch.cond, ignored());
                    if condition.is_unwinding() {
                        return condition;
                    }
                    if condition.status().is_success() {
                        return self.eval_node(chunk, branch.body, ctx);
                    }
                }
                match else_body {
                    Some(body) => self.eval_node(chunk, *body, ctx),
                    None => Unwind::Status(Status::SUCCESS),
                }
            }
            NodeKind::For { var, in_list, body } => {
                self.eval_for(chunk, var, in_list.as_deref(), *body, ctx)
            }
            NodeKind::ForArith { header, body } => {
                self.sync_specials();
                self.eval_for_arith(chunk, header.clone(), *body, ctx)
            }
            NodeKind::While { until, cond, body } => {
                self.eval_while(chunk, *until, *cond, *body, ctx)
            }
            NodeKind::Case { word, arms } => {
                self.sync_specials();
                self.eval_case(chunk, word, arms, ctx)
            }
            NodeKind::BraceGroup { body } => self.eval_node(chunk, *body, ctx),
            NodeKind::Subshell { body } => self.eval_subshell(chunk, *body, ctx),
            NodeKind::Function { name, body, .. } => {
                let Some(name) = literal_name(chunk, name) else {
                    self.report(
                        crate::codes::DEFERRED,
                        node.span.clone(),
                        "function names must be literal",
                    );
                    return Unwind::Status(Status(2));
                };
                self.functions.retain(|function| function.name != name);
                self.functions.push(Function {
                    name,
                    chunk: chunk.clone(),
                    body: *body,
                });
                Unwind::Status(Status::SUCCESS)
            }
            NodeKind::Redirected { body, redirects } => {
                let saved = match self.apply_redirects(chunk, redirects) {
                    Ok(saved) => saved,
                    Err(()) => return Unwind::Status(Status(1)),
                };
                let result = self.eval_node(chunk, *body, ctx);
                self.restore_fds(saved);
                result
            }
            NodeKind::ArithCommand { expr } => {
                self.sync_specials();
                let text = &chunk.source.bytes()[expr.clone()];
                match crate::arith::eval(text, self) {
                    Ok(0) => Unwind::Status(Status::FAILURE),
                    Ok(_) => Unwind::Status(Status::SUCCESS),
                    Err(message) => {
                        self.report(
                            crate::codes::ARITHMETIC,
                            expr.clone(),
                            format!("arithmetic: {message}"),
                        );
                        Unwind::Status(Status::FAILURE)
                    }
                }
            }
            NodeKind::TestCommand { body } => {
                self.sync_specials();
                let status = super::cond::evaluate(self, chunk, body.clone());
                Unwind::Status(status)
            }
            // `time` is a bash keyword the parser keeps as a prefix. Timing is
            // deferred; the pipeline still runs, unmeasured.
            NodeKind::Timed { body } => self.eval_node(chunk, *body, ctx),
        }
    }

    fn eval_list(
        &mut self,
        chunk: &Arc<ParsedChunk>,
        items: &[bsh_syntax::ListItem],
        ctx: Ctx,
    ) -> Unwind {
        let mut last = Status::SUCCESS;
        for item in items {
            if item.sep == Some(Separator::Amp) {
                match self.fork_shell(chunk, item.node, Ctx::default()) {
                    Ok(child) => {
                        self.track_background(child.pid());
                        self.state.vars.set_last_bg_pid(Some(child.pid()));
                        last = Status::SUCCESS;
                    }
                    Err(error) => {
                        self.report(
                            crate::codes::INTERNAL,
                            0..0,
                            format!("cannot start a background job: {error}"),
                        );
                        last = Status::FAILURE;
                    }
                }
                continue;
            }
            match self.eval_node(chunk, item.node, ctx) {
                Unwind::Status(status) => last = status,
                other => return other,
            }
        }
        Unwind::Status(last)
    }

    fn eval_and_or(
        &mut self,
        chunk: &Arc<ParsedChunk>,
        first: NodeId,
        rest: &[bsh_syntax::AndOrLink],
        ctx: Ctx,
    ) -> Unwind {
        // POSIX's ignore-context model exempts all but the *last* command of an
        // `&&`/`||` list — syntactically last, not merely the last one that ran
        // (`false && true` does not exit). The exemption is inherited by
        // everything the operand runs, which is why the flag is passed down
        // instead of only suppressing the check here: `{ false; } && true`
        // continues, while a brace group as the *last* operand still exits.
        let chain_last = rest.last().map_or(first, |link| link.node);
        let mut status = match self.eval_node(chunk, first, operand_ctx(ctx, first == chain_last)) {
            Unwind::Status(status) => status,
            other => return other,
        };
        let mut executed = first;
        for link in rest {
            let run = match link.op {
                bsh_syntax::AndOrOp::And => status.is_success(),
                bsh_syntax::AndOrOp::Or => !status.is_success(),
            };
            if !run {
                continue;
            }
            status =
                match self.eval_node(chunk, link.node, operand_ctx(ctx, link.node == chain_last)) {
                    Unwind::Status(status) => status,
                    other => return other,
                };
            executed = link.node;
        }
        if executed != chain_last {
            return Unwind::Status(status);
        }
        let negated = matches!(
            chunk.ast.node(chain_last).kind,
            NodeKind::Pipeline { negated: true, .. }
        );
        self.check_errexit(status, ctx, negated)
    }

    fn eval_pipeline(
        &mut self,
        chunk: &Arc<ParsedChunk>,
        negated: bool,
        stages: &[NodeId],
        links: &[PipeOp],
        ctx: Ctx,
    ) -> Unwind {
        // POSIX 2024 §2.9.2 and DESIGN.md §2: `pipefail` is sampled when the
        // pipeline STARTS, not when its status is computed. A `set -o pipefail`
        // in a later stage, or the fact that the option changed while the
        // pipeline ran, must not change how this pipeline is judged.
        let pipefail = self.state.opts.pipefail;

        let status = match stages {
            [] => Status::SUCCESS,
            // `!` is an ignore context for the pipeline, so a compound command
            // under it must not exit the shell: `! { false; }` continues.
            [only] => match self.eval_node(chunk, *only, operand_ctx(ctx, !negated)) {
                Unwind::Status(status) => status,
                other => return other,
            },
            _ => match self.run_pipeline(chunk, stages, links, ctx) {
                Ok(statuses) => {
                    // Traps run after the whole foreground pipeline has been
                    // reaped, never between two of its stages.
                    if let Some(exit) = self.after_foreground_wait() {
                        return Unwind::Exit(exit);
                    }
                    combine_pipeline(&statuses, pipefail)
                }
                Err(message) => {
                    self.report(crate::codes::INTERNAL, 0..0, message);
                    Status::FAILURE
                }
            },
        };
        let status = if negated {
            if status.is_success() {
                Status::FAILURE
            } else {
                Status::SUCCESS
            }
        } else {
            status
        };
        Unwind::Status(status)
    }

    /// Run every stage in its own process and collect their statuses in
    /// order. The parent owns no pipe descriptors once this returns, so a
    /// stage that exits early cannot keep a sibling blocked on a write.
    fn run_pipeline(
        &mut self,
        chunk: &Arc<ParsedChunk>,
        stages: &[NodeId],
        links: &[PipeOp],
        ctx: Ctx,
    ) -> Result<Vec<Status>, String> {
        let mut pipes = Vec::new();
        for _ in 1..stages.len() {
            pipes.push(
                bsh_sys::Pipe::new().map_err(|error| format!("cannot create a pipe: {error}"))?,
            );
        }
        // Raw numbers, so the children can close every end they did not use.
        let ends: Vec<(i32, i32)> = pipes
            .iter()
            .map(|pipe| (pipe.read_fd(), pipe.write_fd()))
            .collect();

        let mut children = Vec::with_capacity(stages.len());
        for (index, stage) in stages.iter().enumerate() {
            let stdin = if index == 0 {
                None
            } else {
                Some(ends[index - 1].0)
            };
            let stdout = ends.get(index).map(|end| end.1);
            let stderr_to_stdout = links
                .get(index)
                .is_some_and(|link| *link == PipeOp::PipeStderr);
            let ends = ends.clone();
            let result = bsh_sys::fork_child(|| {
                self.enter_child();
                if let Some(fd) = stdin {
                    bsh_sys::dup2(fd, 0)?;
                }
                if let Some(fd) = stdout {
                    bsh_sys::dup2(fd, 1)?;
                    if stderr_to_stdout {
                        bsh_sys::dup2(fd, 2)?;
                    }
                }
                // Every pipe end the child inherited must go, or the reader
                // never sees EOF.
                for (read, write) in ends {
                    let _ = bsh_sys::close(read);
                    let _ = bsh_sys::close(write);
                }
                let unwind = self.eval_node(chunk, *stage, ctx);
                bsh_sys::exit_child(unwind.status().exit_code());
            });
            match result {
                Ok(child) => children.push(child),
                Err(error) => {
                    for child in &children {
                        let _ = bsh_sys::wait_pid(child.pid(), false);
                    }
                    drop(pipes);
                    return Err(format!("cannot start a pipeline stage: {error}"));
                }
            }
        }
        drop(pipes);

        // The stages are already forked, so ignoring `SIGINT`/`SIGQUIT` here
        // protects only the shell while it reaps them.
        let guard = self.ignore_int_quit();
        let mut statuses = Vec::with_capacity(children.len());
        for child in children {
            match bsh_sys::wait_pid(child.pid(), false) {
                Ok(outcome) => statuses.push(Status(outcome.exit_status())),
                Err(error) => {
                    drop(guard);
                    return Err(format!("cannot wait for a pipeline stage: {error}"));
                }
            }
        }
        drop(guard);
        Ok(statuses)
    }

    fn eval_for(
        &mut self,
        chunk: &Arc<ParsedChunk>,
        var: &bsh_lang::Word,
        in_list: Option<&[bsh_lang::Word]>,
        body: NodeId,
        ctx: Ctx,
    ) -> Unwind {
        let Some(name) = literal_name(chunk, var) else {
            self.report(
                crate::codes::DEFERRED,
                var.span.clone(),
                "the `for` variable must be a literal name",
            );
            return Unwind::Status(Status(2));
        };
        let words: Vec<Vec<u8>> = match in_list {
            // `for x; do` iterates the positional parameters.
            None => self.state.vars.positional().argv.clone(),
            Some(words) => match bsh_lang::expand_words(words, self) {
                Ok(words) => words,
                Err(error) => return self.expansion_failed(error),
            },
        };

        let mut last = Status::SUCCESS;
        for value in words {
            if let Err(error) = self.state.vars.assign_scalar(&name, &value) {
                self.report(crate::codes::ASSIGNMENT, var.span.clone(), error.message());
                return Unwind::Status(Status::FAILURE);
            }
            match self.eval_node(chunk, body, ctx) {
                Unwind::Status(status) => last = status,
                Unwind::Break(1) => return Unwind::Status(Status::SUCCESS),
                Unwind::Break(count) => return Unwind::Break(count - 1),
                Unwind::Continue(1) => continue,
                Unwind::Continue(count) => return Unwind::Continue(count - 1),
                other => return other,
            }
        }
        Unwind::Status(last)
    }

    fn eval_for_arith(
        &mut self,
        chunk: &Arc<ParsedChunk>,
        header: bsh_diag::Span,
        body: NodeId,
        ctx: Ctx,
    ) -> Unwind {
        let text = &chunk.source.bytes()[header.clone()];
        let parts: Vec<&[u8]> = text.split(|&b| b == b';').collect();
        let init = parts.first().copied().unwrap_or_default();
        let condition = parts.get(1).copied().unwrap_or_default();
        let update = parts.get(2).copied().unwrap_or_default();
        if let Err(message) = crate::arith::eval(init, self) {
            self.report(
                crate::codes::ARITHMETIC,
                header.clone(),
                format!("arithmetic: {message}"),
            );
            return Unwind::Status(Status::FAILURE);
        }
        let mut last = Status::SUCCESS;
        loop {
            if !condition.trim_ascii().is_empty() {
                match crate::arith::eval(condition, self) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(message) => {
                        self.report(
                            crate::codes::ARITHMETIC,
                            header.clone(),
                            format!("arithmetic: {message}"),
                        );
                        return Unwind::Status(Status::FAILURE);
                    }
                }
            }
            match self.eval_node(chunk, body, ctx) {
                Unwind::Status(status) => last = status,
                Unwind::Break(1) => return Unwind::Status(Status::SUCCESS),
                Unwind::Break(count) => return Unwind::Break(count - 1),
                Unwind::Continue(1) => {}
                Unwind::Continue(count) => return Unwind::Continue(count - 1),
                other => return other,
            }
            if let Err(message) = crate::arith::eval(update, self) {
                self.report(
                    crate::codes::ARITHMETIC,
                    header.clone(),
                    format!("arithmetic: {message}"),
                );
                return Unwind::Status(Status::FAILURE);
            }
        }
        Unwind::Status(last)
    }

    fn eval_while(
        &mut self,
        chunk: &Arc<ParsedChunk>,
        until: bool,
        condition: NodeId,
        body: NodeId,
        ctx: Ctx,
    ) -> Unwind {
        let mut last = Status::SUCCESS;
        loop {
            let result = self.eval_node(chunk, condition, ignored());
            if result.is_unwinding() {
                return result;
            }
            let truth = if until {
                !result.status().is_success()
            } else {
                result.status().is_success()
            };
            if !truth {
                break;
            }
            match self.eval_node(chunk, body, ctx) {
                Unwind::Status(status) => last = status,
                Unwind::Break(1) => return Unwind::Status(Status::SUCCESS),
                Unwind::Break(count) => return Unwind::Break(count - 1),
                Unwind::Continue(1) => continue,
                Unwind::Continue(count) => return Unwind::Continue(count - 1),
                other => return other,
            }
        }
        Unwind::Status(last)
    }

    fn eval_case(
        &mut self,
        chunk: &Arc<ParsedChunk>,
        word: &bsh_lang::Word,
        arms: &[bsh_syntax::CaseArm],
        ctx: Ctx,
    ) -> Unwind {
        let subject = match self.expand_operand(word) {
            Ok(field) => field,
            Err(error) => return self.expansion_failed(error),
        };
        let mut status = Status::SUCCESS;
        let mut fallthrough = false;
        for arm in arms {
            let mut matched = fallthrough;
            fallthrough = false;
            if !matched {
                for pattern in &arm.patterns {
                    let field = match self.expand_operand(pattern) {
                        Ok(field) => field,
                        Err(error) => return self.expansion_failed(error),
                    };
                    let pattern = crate::glob::Pattern::new(&field.bytes, Some(&field.quoted));
                    if crate::glob::matches(&pattern, &subject.bytes) {
                        matched = true;
                        break;
                    }
                }
            }
            if !matched {
                continue;
            }
            match self.eval_node(chunk, arm.body, ctx) {
                Unwind::Status(arm_status) => status = arm_status,
                other => return other,
            }
            match arm.term {
                CaseTerm::Break | CaseTerm::None => return Unwind::Status(status),
                CaseTerm::FallThrough => fallthrough = true,
                CaseTerm::Continue => {}
            }
        }
        Unwind::Status(status)
    }

    fn eval_subshell(&mut self, chunk: &Arc<ParsedChunk>, body: NodeId, ctx: Ctx) -> Unwind {
        match self.fork_shell(chunk, body, ctx) {
            Ok(child) => {
                let guard = self.ignore_int_quit();
                let waited = child.wait(false);
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
                            format!("cannot wait for a subshell: {error}"),
                        );
                        Unwind::Status(Status::FAILURE)
                    }
                }
            }
            Err(error) => {
                self.report(
                    crate::codes::INTERNAL,
                    0..0,
                    format!("cannot start a subshell: {error}"),
                );
                Unwind::Status(Status::FAILURE)
            }
        }
    }

    /// Run a `$(…)` body in a child, capture its stdout and strip trailing
    /// newlines (POSIX XCU 2.6.3).
    pub(crate) fn capture_substitution(&mut self, src: &[u8]) -> Result<(Vec<u8>, Status), String> {
        let source = Arc::new(bsh_diag::SourceFile::new("<command substitution>", src));
        let opts = self.parse_opts();
        let chunk = match bsh_syntax::parse_with(&source, &opts) {
            bsh_syntax::ParseOutcome::Ok(ast) => Arc::new(ParsedChunk {
                ast,
                source: source.clone(),
            }),
            bsh_syntax::ParseOutcome::NeedMoreInput { open, .. } => {
                let previous = std::mem::replace(&mut self.current, source);
                self.diagnose(&Diagnostic::error(
                    bsh_syntax::codes::NEED_MORE_INPUT,
                    open,
                    "unexpected end of file in a command substitution",
                ));
                self.current = previous;
                return Ok((Vec::new(), Status(2)));
            }
            bsh_syntax::ParseOutcome::Failed(diagnostics) => {
                let previous = std::mem::replace(&mut self.current, source);
                for diagnostic in &diagnostics {
                    self.diagnose(diagnostic);
                }
                self.current = previous;
                return Ok((Vec::new(), Status(2)));
            }
        };

        let pipe =
            bsh_sys::Pipe::new().map_err(|error| format!("cannot create a pipe: {error}"))?;
        let read = pipe.read_fd();
        let write = pipe.write_fd();
        let child = bsh_sys::fork_child(|| {
            self.enter_child();
            bsh_sys::dup2(write, 1)?;
            let _ = bsh_sys::close(write);
            let _ = bsh_sys::close(read);
            let unwind = self.eval_node(&chunk, chunk.ast.root, Ctx::default());
            bsh_sys::exit_child(unwind.status().exit_code());
        })
        .map_err(|error| format!("cannot start a command substitution: {error}"))?;

        drop(pipe.write);
        // Reading the child's output is a foreground wait as far as signals
        // are concerned: `SIGINT`/`SIGQUIT` must not kill the shell here.
        let guard = self.ignore_int_quit();
        let mut bytes = Vec::new();
        {
            use std::io::Read;
            let mut file = std::fs::File::from(pipe.read);
            file.read_to_end(&mut bytes)
                .map_err(|error| format!("cannot read a command substitution: {error}"))?;
        }
        let outcome = bsh_sys::wait_pid(child.pid(), false)
            .map_err(|error| format!("cannot wait for a command substitution: {error}"))?;
        drop(guard);
        // A trap that arrived while the substitution ran runs at the next
        // command boundary; the exit request (if any) is recorded in the state.
        let _ = self.after_foreground_wait();
        while bytes.last() == Some(&b'\n') {
            bytes.pop();
        }
        Ok((bytes, Status(outcome.exit_status())))
    }
}

/// A context where `errexit` is suppressed (a condition, or a non-final
/// `&&`/`||` operand).
fn ignored() -> Ctx {
    Ctx {
        ignore_errexit: true,
    }
}

/// The context for an `&&`/`||` operand: exempt unless it is the final one,
/// and exempt anyway if the enclosing region is.
fn operand_ctx(ctx: Ctx, is_final: bool) -> Ctx {
    Ctx {
        ignore_errexit: ctx.ignore_errexit || !is_final,
    }
}

/// Fold pipeline statuses: the last stage, or under `pipefail` the last
/// non-zero one, read left to right.
fn combine_pipeline(statuses: &[Status], pipefail: bool) -> Status {
    let last = statuses.last().copied().unwrap_or(Status::SUCCESS);
    if !pipefail {
        return last;
    }
    statuses
        .iter()
        .copied()
        .find(|status| !status.is_success())
        .unwrap_or(Status::SUCCESS)
}

/// The bytes of a word that must be a literal name (`for x`, `f()`, `case`).
fn literal_name(chunk: &Arc<ParsedChunk>, word: &bsh_lang::Word) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for part in &word.parts {
        match part {
            bsh_lang::Part::Lit(lit) => out.extend_from_slice(&lit.bytes),
            _ => return None,
        }
    }
    let _ = chunk;
    (!out.is_empty()).then_some(out)
}
