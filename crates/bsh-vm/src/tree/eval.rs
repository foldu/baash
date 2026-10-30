//! The AST walk: lists, and-or chains, pipelines, compound commands and
//! subshells.
//!
//! Every arm returns [`Unwind`], and the *only* place `errexit` fires is
//! [`Interp::eval_and_or`] — after a whole and-or list, for the syntactically
//! last operand, only if it ran, and only when the region is not an ignored
//! context (DESIGN.md §4.7). Keeping that in one function is the whole point
//! of modelling control flow as unwinding instead of `Result` at every node.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::Arc;

use bsh_diag::Diagnostic;
use bsh_lang::Shell as _;
use bsh_runtime::{ShellHost, Status};
use bsh_syntax::{CaseTerm, NodeId, NodeKind, PipeOp, Separator};

use super::{Ctx, Function, Interp, ParsedChunk, Unwind, traps};

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
    /// Evaluate one node, keeping `$?`, `$LINENO` and `$PIPESTATUS` current.
    pub(crate) fn eval_node(&mut self, chunk: &Arc<ParsedChunk>, id: NodeId, ctx: Ctx) -> Unwind {
        // The innermost node visited while a command's words expand is that
        // command, so `$LINENO` is its line. Compound nodes set the line of
        // their keyword first; the simple commands inside overwrite it.
        let node = chunk.ast.node(id);
        self.set_line(&chunk.source, node.span.start);
        // A command that is not a pipeline is a one-stage one for
        // `$PIPESTATUS` (`false; echo ${PIPESTATUS[@]}` reports `1`).
        // bash records only the commands themselves: `if`, `for`, `while`,
        // `{ … }` and `case` leave the array alone, so the last command
        // *inside* them is what a later `echo` sees. A subshell is a command
        // in the parent; a function definition is not. `Pipeline` is excluded
        // because `eval_pipeline` records the per-stage statuses, and negation
        // must not replace them.
        let command = matches!(
            node.kind,
            NodeKind::Simple(_)
                | NodeKind::TestCommand { .. }
                | NodeKind::ArithCommand { .. }
                | NodeKind::Subshell { .. }
        );
        let result = self.eval_kind(chunk, id, ctx);
        self.set_status(result.status());
        if command {
            self.set_pipeline_status(&[result.status()]);
        }
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
                        let pid = child.pid();
                        // No job table: the pid alone is the handle `$!`,
                        // `wait` and `kill` use.
                        self.background.insert(pid, None);
                        self.state.vars.set_last_bg_pid(Some(pid));
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

        // `$PIPESTATUS` is the per-stage status of this pipeline. A single
        // command is a one-stage pipeline, which is what makes `false;
        // echo ${PIPESTATUS[@]}` report `1`. The word expansions that ask for
        // it run before this assignments, so they still see the previous
        // pipeline — exactly bash's rule.
        let mut stage_statuses = Vec::new();
        let status = match stages {
            [] => Status::SUCCESS,
            // `!` is an ignore context for the pipeline, so a compound command
            // under it must not exit the shell: `! { false; }` continues.
            [only] => match self.eval_node(chunk, *only, operand_ctx(ctx, !negated)) {
                Unwind::Status(status) => {
                    stage_statuses.push(status);
                    status
                }
                other => return other,
            },
            _ => match self.run_pipeline(chunk, stages, links, ctx) {
                Ok(statuses) => {
                    // Traps run after the whole foreground pipeline has been
                    // reaped, never between two of its stages.
                    if let Some(exit) = self.after_foreground_wait() {
                        return Unwind::Exit(exit);
                    }
                    let status = combine_pipeline(&statuses, pipefail);
                    stage_statuses = statuses;
                    status
                }
                Err(message) => {
                    self.report(crate::codes::INTERNAL, 0..0, message);
                    stage_statuses.push(Status::FAILURE);
                    Status::FAILURE
                }
            },
        };
        if !stage_statuses.is_empty() {
            self.set_pipeline_status(&stage_statuses);
        }
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
    /// Run the stages of a pipeline.
    ///
    /// External commands are spawned and *not* waited for here, so the stages
    /// overlap as they did under `fork`; everything else runs in this process.
    /// A non-final stage that turns out to be shell code cannot write into a
    /// pipe — its writer is this process, which would block against its own
    /// read end — so [`Interp::buffer_pipeline_output`] diverts its output to an
    /// unlinked temporary file and the next stage reads that instead.
    fn run_pipeline(
        &mut self,
        chunk: &Arc<ParsedChunk>,
        stages: &[NodeId],
        links: &[PipeOp],
        ctx: Ctx,
    ) -> Result<Vec<Status>, String> {
        let count = stages.len();
        let mut statuses = vec![Status::SUCCESS; count];
        let mut spawned: Vec<Vec<bsh_sys::Child>> = vec![Vec::new(); count];
        // The descriptor the next stage reads as its standard input.
        let mut input: Option<OwnedFd> = None;
        // A nested pipeline must not inherit the outer stage's armed buffer.
        let outer_buffer = self.pipeline_buffer.take();

        for (index, stage) in stages.iter().enumerate() {
            let last = index + 1 == count;
            let stderr_to_stdout = links
                .get(index)
                .is_some_and(|link| *link == PipeOp::PipeStderr);

            let pipe = if last {
                None
            } else {
                Some(
                    bsh_sys::Pipe::new()
                        .map_err(|error| format!("cannot create a pipe: {error}"))?,
                )
            };
            let output = pipe.as_ref().map(bsh_sys::Pipe::write_fd);

            // Wire the descriptors in this process: a child inherits them at
            // `spawn` and shell code writes to them directly.
            // `None` means "this stage does not touch that descriptor", so the
            // restore must leave it alone: closing an untouched 0/1/2 would
            // hand the number to the next `open`.
            let saved_stdin = input.as_ref().map(|fd| save_and_replace(0, fd.as_raw_fd()));
            let saved_stdout = output.map(|fd| save_and_replace(1, fd));
            let saved_stderr = match output {
                Some(fd) if stderr_to_stdout => Some(save_and_replace(2, fd)),
                _ => None,
            };

            // Only a non-final stage needs the swap. A compound command never
            // reaches `invoke`, so it is armed here as well.
            self.pipeline_buffer = (!last).then(|| traps::PipelineBuffer { file: None });
            if self.pipeline_buffer.is_some()
                && !matches!(node_kind(chunk, *stage), NodeKind::Simple(_))
            {
                self.buffer_pipeline_output();
            }

            // Only a stage that *is* an external command may overlap with the
            // others; a compound stage runs here and waits for its own
            // children, so `$?` inside it means what it says.
            self.defer_next_spawn =
                !last && matches!(node_kind(chunk, *stage), NodeKind::Simple(_));
            let mark = self.pending_children.len();
            // Every stage but the last is a subshell in bash: it sees the
            // parent's state and leaves nothing behind, and `exit` inside it
            // ends the stage rather than the shell. The last stage runs in the
            // current shell (lastpipe, always on here), which is what makes
            // `cmd | while read …` useful.
            let scope = (!last).then(|| self.open_scope());
            // `ERR` belongs to the pipeline, not to a stage (see
            // `err_trap_suppressed`).
            self.err_trap_suppressed += 1;
            let unwind = self.eval_node(chunk, *stage, ctx);
            self.err_trap_suppressed -= 1;
            statuses[index] = unwind.status();
            if let Some(scope) = scope {
                self.close_scope(scope);
            }
            self.defer_next_spawn = false;
            spawned[index].extend(self.pending_children.drain(mark..));

            let buffered_size = self
                .pipeline_buffer
                .as_ref()
                .and_then(traps::PipelineBuffer::size);

            if let Some(saved) = saved_stdout {
                restore_saved(1, saved);
            }
            if let Some(saved) = saved_stderr {
                restore_saved(2, saved);
            }
            if let Some(saved) = saved_stdin {
                restore_saved(0, saved);
            }

            let buffered = self.pipeline_buffer.take().and_then(|buffer| buffer.file);
            if let Some(size) = buffered_size {
                if size > MAX_BUFFERED_STAGE {
                    let _ = bsh_sys::close(0);
                    let _ = bsh_sys::close(1);
                    return Err(format!(
                        "pipeline stage buffered {size} bytes of output, more than the {MAX_BUFFERED_STAGE} byte limit for shell code in a pipeline"
                    ));
                }
            }
            if let Some(pipe) = pipe {
                // The parent must not hold the write end, or the reader never
                // sees end of file. Destructuring drops each end exactly once:
                // an explicit `close` plus the `OwnedFd` drop would be an
                // io-safety violation.
                let bsh_sys::Pipe { read, write } = pipe;
                drop(write);
                input = match buffered {
                    Some(file) => {
                        let _ = bsh_sys::lseek(
                            std::os::fd::AsRawFd::as_raw_fd(&file),
                            0,
                            bsh_sys::seek::SET,
                        );
                        Some(file)
                    }
                    None => Some(read),
                };
            }
        }
        self.pipeline_buffer = outer_buffer;

        // Drop our copy of the last stage's input before waiting: an earlier
        // writer blocked on a full pipe only learns that nobody is reading when
        // the read end goes away, and `cat /dev/urandom | sleep 0.1` needs the
        // writer to die of `SIGPIPE`.
        drop(input);

        // Children were spawned in pipeline order, so waiting for each stage's
        // group in turn keeps `$PIPESTATUS` aligned with the stages.
        for (index, group) in spawned.into_iter().enumerate() {
            if group.is_empty() {
                continue;
            }
            let pids: Vec<i32> = group.iter().map(bsh_sys::Child::pid).collect();
            let outcomes = self
                .wait_foreground_children(&pids)
                .map_err(|error| format!("cannot wait for a pipeline stage: {error}"))?;
            if let Some(outcome) = outcomes.last() {
                statuses[index] = Status(outcome.exit_status());
            }
        }
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
            // The header is re-evaluated on every iteration, and `$LINENO`
            // inside it is the header's line even after the body moved the
            // cursor (bash agrees; `for (( i = 0; i < $LINENO; i++ ))` runs
            // until `i` reaches the header's line).
            self.set_line(&chunk.source, header.start);
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
            self.set_line(&chunk.source, header.start);
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
                    // The dialect is read per arm: a `shopt` inside an earlier
                    // arm's body takes effect on the arms after it, as in bash.
                    let flags = self.pattern_flags();
                    if crate::pattern::matches(&field, &subject.bytes, flags) {
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
        // In process, not in a child: the body reads and writes the same shell
        // state, and the scope restores what it changed (`tree/scope.rs`).
        let status = self.run_in_scope(chunk, body, ctx);
        self.set_status(status);
        self.sync_specials();
        if let Some(exit) = self.after_foreground_wait() {
            return Unwind::Exit(exit);
        }
        Unwind::Status(status)
    }

    #[allow(dead_code)]
    fn eval_subshell_forked(&mut self, chunk: &Arc<ParsedChunk>, body: NodeId, ctx: Ctx) -> Unwind {
        match self.fork_shell(chunk, body, ctx) {
            Ok(child) => {
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

    /// Run a `$(…)` body in a scope, capture its stdout and strip trailing
    /// newlines (POSIX XCU 2.6.3).
    ///
    /// The capture target is an unlinked temporary file, not a pipe. The writer
    /// is now this process, so a pipe would deadlock once the body produced
    /// more than its capacity; the same reasoning as `tree/redir.rs` gives for
    /// here-documents.
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

        let capture = bsh_sys::create_unlinked_temp(0)
            .map_err(|error| format!("cannot create a capture file: {error}"))?;
        let scope = self.open_scope();
        // The body writes to the file through descriptor 1, exactly as it would
        // through a pipe: built-ins write with `write_fd`, and an external
        // command inherits the descriptor through its spawn plan.
        let saved_stdout = bsh_sys::dup(1).ok();
        if let Err(error) = bsh_sys::dup2(capture.as_raw_fd(), 1) {
            self.close_scope(scope);
            return Err(format!("cannot redirect a command substitution: {error}"));
        }
        let unwind = self.eval_node(&chunk, chunk.ast.root, Ctx::default());
        let status = unwind.status();
        match saved_stdout {
            Some(saved) => {
                let _ = bsh_sys::dup2(saved.as_raw_fd(), 1);
            }
            // No descriptor 1 on entry (a script with it closed): leave it
            // closed rather than inventing one.
            None => {
                let _ = bsh_sys::close(1);
            }
        }
        self.close_scope(scope);
        // A trap that arrived while the substitution ran runs at the next
        // command boundary; the exit request (if any) is recorded in the state.
        let _ = self.after_foreground_wait();

        let mut bytes = Vec::new();
        {
            use std::io::{Read, Seek};
            let mut file = std::fs::File::from(capture);
            file.seek(std::io::SeekFrom::Start(0))
                .map_err(|error| format!("cannot rewind a command substitution: {error}"))?;
            file.read_to_end(&mut bytes)
                .map_err(|error| format!("cannot read a command substitution: {error}"))?;
        }
        while bytes.last() == Some(&b'\n') {
            bytes.pop();
        }
        Ok((bytes, status))
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

/// The largest amount of output a non-final pipeline stage may buffer.
///
/// A stage that is shell code runs in this process, so it cannot write into a
/// pipe without the shell blocking on its own read end; its output goes to a
/// temporary file instead. That is bounded so an unbounded producer (`while
/// true; do echo x; done | head -1`) fails with a diagnostic rather than
/// filling the disk.
const MAX_BUFFERED_STAGE: u64 = 64 * 1024 * 1024;

/// Point `target` at `source`, returning the descriptor `target` used to name.
///
/// `None` means `target` was closed on entry, so the restore closes it again
/// rather than pointing it somewhere invented.
fn save_and_replace(target: RawFd, source: RawFd) -> Option<OwnedFd> {
    let saved = bsh_sys::dup(target).ok();
    if bsh_sys::dup2(source, target).is_err() {
        return saved;
    }
    saved
}

/// Put a descriptor back the way [`save_and_replace`] found it.
fn restore_saved(target: RawFd, saved: Option<OwnedFd>) {
    match saved {
        Some(fd) => {
            let _ = bsh_sys::dup2(fd.as_raw_fd(), target);
        }
        None => {
            let _ = bsh_sys::close(target);
        }
    }
}

/// The kind of a node, for callers that must know before evaluating it.
fn node_kind(chunk: &ParsedChunk, node: NodeId) -> NodeKind {
    chunk.ast.node(node).kind.clone()
}

impl Interp {
    /// Divert a pipeline stage's output to a temporary file.
    ///
    /// Called at the dispatch site when a non-final stage turns out to be shell
    /// code: a pipe would deadlock, because the writer and the reader would
    /// both be this process. A no-op unless a pipeline stage armed the switch.
    pub(crate) fn buffer_pipeline_output(&mut self) {
        let Some(buffer) = self.pipeline_buffer.as_mut() else {
            return;
        };
        if buffer.file.is_some() {
            return;
        }
        let Ok(file) = bsh_sys::create_unlinked_temp(0) else {
            return;
        };
        if bsh_sys::dup2(file.as_raw_fd(), 1).is_ok() {
            buffer.file = Some(file);
        }
    }
}
