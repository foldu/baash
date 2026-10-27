//! Redirections, applied left to right.
//!
//! Order is observable and the list is never treated as a set (DESIGN.md §4.4):
//! `2>&1 >f` is not `>f 2>&1`. Both application strategies therefore consume
//! one ordered `Vec<Step>`:
//!
//! * **built-ins, compound commands, function calls** — save the target
//!   descriptor, `dup2` the new one over it, run, restore in reverse;
//! * **external commands** — compose `bsh_sys::FdAction`s for the child, so the
//!   redirection happens after `fork`/`spawn` and the parent never holds the
//!   file open for the command's lifetime.
//!
//! Redirect targets are opened *in the parent* even for the child path: a
//! failure is then a diagnostic and a status 1 rather than a mysterious
//! spawn error, and `FdAction::Dup2` already clears `FD_CLOEXEC` on the target.
//!
//! Here-documents are written to an unlinked temporary file
//! (`bsh_sys::create_unlinked_temp`, DESIGN.md §4.4): a pipe would deadlock
//! when the reader never drains a large body.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd, RawFd};
use std::sync::Arc;

use bsh_lang::{Mode, expand_word_with};
use bsh_syntax::{HereDoc, Redirect, RedirectOp};

use super::{Interp, ParsedChunk};

/// One ordered descriptor change.
enum Step {
    /// Open `path` onto `to`, created/truncated/append per the operator.
    Open {
        path: Vec<u8>,
        flags: i32,
        to: RawFd,
    },
    /// `dup2(from, to)`.
    Dup { from: RawFd, to: RawFd },
    /// Close `to` (`n>&-`).
    Close { to: RawFd },
    /// A here-document or here-string, already written and rewound.
    Temp { file: File, to: RawFd },
}

/// Descriptors saved so a built-in's redirections can be undone, in the order
/// they were applied.
pub(crate) struct SavedFds {
    entries: Vec<(RawFd, Option<RawFd>)>,
}

impl Interp {
    /// Apply redirections to the shell's own descriptors (built-ins, compound
    /// commands, function calls), remembering how to undo them.
    pub(crate) fn apply_redirects(
        &mut self,
        chunk: &Arc<ParsedChunk>,
        redirects: &[Redirect],
    ) -> Result<SavedFds, ()> {
        let steps = self.redirect_steps(chunk, redirects)?;
        let targets: Vec<RawFd> = steps.iter().map(step_target).collect();
        let mut saved = SavedFds {
            entries: Vec::with_capacity(steps.len()),
        };
        for (index, step) in steps.into_iter().enumerate() {
            let target = targets[index];
            let previous = save_descriptor(target, &targets);
            saved.entries.push((target, previous));
            if let Err(error) = apply_step(step) {
                // Undo what this command already changed, then report.
                self.restore_fds(saved);
                self.report(
                    crate::codes::REDIRECTION,
                    0..0,
                    format!("redirection failed: {error}"),
                );
                return Err(());
            }
        }
        Ok(saved)
    }

    /// Apply redirections and *keep* them. This is `exec` with no command
    /// (`exec 3>file`), where the point is that the shell's own descriptor
    /// table changes permanently.
    pub(crate) fn apply_redirects_committed(
        &mut self,
        chunk: &Arc<ParsedChunk>,
        redirects: &[Redirect],
    ) -> Result<(), ()> {
        for step in self.redirect_steps(chunk, redirects)? {
            if let Err(error) = apply_step(step) {
                self.report(
                    crate::codes::REDIRECTION,
                    0..0,
                    format!("redirection failed: {error}"),
                );
                return Err(());
            }
        }
        Ok(())
    }

    /// Restore descriptors saved by [`Interp::apply_redirects`], newest first.
    pub(crate) fn restore_fds(&mut self, saved: SavedFds) {
        for (target, previous) in saved.entries.into_iter().rev() {
            match previous {
                Some(saved) => {
                    let _ = bsh_sys::dup2(saved, target);
                    let _ = bsh_sys::close(saved);
                }
                None => {
                    let _ = bsh_sys::close(target);
                }
            }
        }
    }

    /// Compose the child's ordered actions and the descriptors it needs. The
    /// caller keeps the returned descriptors alive until `spawn` returns.
    pub(crate) fn compose_actions(
        &mut self,
        chunk: &Arc<ParsedChunk>,
        redirects: &[Redirect],
    ) -> Result<(Vec<bsh_sys::FdAction>, Vec<OwnedFd>), ()> {
        let mut actions = Vec::new();
        let mut keep = Vec::new();
        for step in self.redirect_steps(chunk, redirects)? {
            match step {
                Step::Open { path, flags, to } => {
                    let file = match bsh_sys::open_cloexec(&path, flags, 0o666) {
                        Ok(file) => file,
                        Err(error) => {
                            self.report(
                                crate::codes::REDIRECTION,
                                0..0,
                                format!("{}: {error}", String::from_utf8_lossy(&path)),
                            );
                            return Err(());
                        }
                    };
                    actions.push(bsh_sys::FdAction::dup2(file.as_raw_fd(), to));
                    keep.push(file);
                }
                Step::Dup { from, to } => actions.push(bsh_sys::FdAction::dup2(from, to)),
                Step::Close { to } => actions.push(bsh_sys::FdAction::close(to)),
                Step::Temp { file, to } => {
                    actions.push(bsh_sys::FdAction::dup2(file.as_raw_fd(), to));
                    keep.push(OwnedFd::from(file));
                }
            }
        }
        Ok((actions, keep))
    }

    /// Turn the AST's redirection list into ordered steps, expanding targets.
    fn redirect_steps(
        &mut self,
        chunk: &Arc<ParsedChunk>,
        redirects: &[Redirect],
    ) -> Result<Vec<Step>, ()> {
        let mut steps = Vec::with_capacity(redirects.len());
        for redirect in redirects {
            let target = default_fd(redirect.op, redirect.fd);
            match redirect.op {
                RedirectOp::HereDoc | RedirectOp::HereDocStrip => {
                    let Some(index) = redirect.heredoc else {
                        self.report(
                            crate::codes::REDIRECTION,
                            redirect.span.clone(),
                            "here-document without a body",
                        );
                        return Err(());
                    };
                    let Some(heredoc) = chunk.ast.heredocs.get(index) else {
                        self.report(
                            crate::codes::REDIRECTION,
                            redirect.span.clone(),
                            "here-document without a body",
                        );
                        return Err(());
                    };
                    let body = self.heredoc_body(chunk, heredoc)?;
                    steps.push(Step::Temp {
                        file: self.temp_file(&body, &redirect.span)?,
                        to: target,
                    });
                }
                RedirectOp::HereString => {
                    let Some(word) = &redirect.target else {
                        self.report(
                            crate::codes::REDIRECTION,
                            redirect.span.clone(),
                            "here-string without a word",
                        );
                        return Err(());
                    };
                    let mut body = self.scalar_word(word)?;
                    body.push(b'\n');
                    steps.push(Step::Temp {
                        file: self.temp_file(&body, &redirect.span)?,
                        to: target,
                    });
                }
                RedirectOp::DupRead | RedirectOp::DupWrite => {
                    let Some(target_word) = &redirect.target else {
                        self.report(
                            crate::codes::REDIRECTION,
                            redirect.span.clone(),
                            "redirection without a target",
                        );
                        return Err(());
                    };
                    let word = self.scalar_word(target_word)?;
                    if word == b"-" {
                        steps.push(Step::Close { to: target });
                        continue;
                    }
                    if let Some(fd) = descriptor(&word) {
                        steps.push(Step::Dup {
                            from: fd,
                            to: target,
                        });
                        continue;
                    }
                    // bash's obsolete `>&file` means `&>file`; `<&file` is an
                    // error because there is nothing to duplicate.
                    if redirect.op == RedirectOp::DupWrite {
                        steps.push(open_step(&word, RedirectOp::Write, 1)?);
                        steps.push(Step::Dup { from: 1, to: 2 });
                        continue;
                    }
                    self.report(
                        crate::codes::REDIRECTION,
                        redirect.span.clone(),
                        format!("{}: ambiguous redirect", String::from_utf8_lossy(&word)),
                    );
                    return Err(());
                }
                RedirectOp::Both | RedirectOp::BothAppend => {
                    let op = if redirect.op == RedirectOp::Both {
                        RedirectOp::Write
                    } else {
                        RedirectOp::Append
                    };
                    let path = self.redirect_path(redirect)?;
                    steps.push(open_step(&path, op, 1)?);
                    steps.push(Step::Dup { from: 1, to: 2 });
                }
                op => {
                    let path = self.redirect_path(redirect)?;
                    steps.push(open_step(&path, op, target)?);
                }
            }
        }
        Ok(steps)
    }

    /// Expand a redirection's target to exactly one path.
    fn redirect_path(&mut self, redirect: &Redirect) -> Result<Vec<u8>, ()> {
        let Some(word) = &redirect.target else {
            self.report(
                crate::codes::REDIRECTION,
                redirect.span.clone(),
                "redirection without a target",
            );
            return Err(());
        };
        let fields = match expand_word_with(word, self, Mode::Command) {
            Ok(fields) => fields,
            Err(error) => {
                self.expansion_failed(error);
                return Err(());
            }
        };
        match fields.len() {
            1 => Ok(fields.into_iter().next().unwrap_or_default()),
            _ => {
                self.report(
                    crate::codes::REDIRECTION,
                    redirect.span.clone(),
                    "ambiguous redirect",
                );
                Err(())
            }
        }
    }

    /// Expand a word as a scalar (no splitting, no globbing) — the rule for
    /// here-strings and for `n>&m`.
    fn scalar_word(&mut self, word: &bsh_lang::Word) -> Result<Vec<u8>, ()> {
        match expand_word_with(word, self, Mode::Assignment) {
            Ok(mut fields) => Ok(fields.pop().unwrap_or_default()),
            Err(error) => {
                self.expansion_failed(error);
                Err(())
            }
        }
    }

    /// The body of a here-document, expanded unless its delimiter was quoted.
    fn heredoc_body(&mut self, chunk: &Arc<ParsedChunk>, heredoc: &HereDoc) -> Result<Vec<u8>, ()> {
        let mut body = chunk.source.bytes()[heredoc.body.clone()].to_vec();
        if heredoc.strip_tabs {
            body = strip_leading_tabs(&body);
        }
        if heredoc.quoted {
            return Ok(body);
        }
        match self.expand_heredoc_body(&body) {
            Ok(body) => Ok(body),
            Err(error) => {
                self.expansion_failed(error);
                Err(())
            }
        }
    }

    fn temp_file(&mut self, body: &[u8], span: &bsh_diag::Span) -> Result<File, ()> {
        let file = match bsh_sys::create_unlinked_temp(body.len()) {
            Ok(file) => file,
            Err(error) => {
                self.report(
                    crate::codes::REDIRECTION,
                    span.clone(),
                    format!("cannot create a here-document: {error}"),
                );
                return Err(());
            }
        };
        let mut file = File::from(file);
        if let Err(error) = file
            .write_all(body)
            .and_then(|()| file.seek(SeekFrom::Start(0)))
        {
            self.report(
                crate::codes::REDIRECTION,
                span.clone(),
                format!("cannot write a here-document: {error}"),
            );
            return Err(());
        }
        Ok(file)
    }
}

/// The redirection's target descriptor when it was not written explicitly.
fn default_fd(op: RedirectOp, explicit: Option<u32>) -> RawFd {
    if let Some(fd) = explicit {
        return fd as RawFd;
    }
    match op {
        RedirectOp::Read
        | RedirectOp::ReadWrite
        | RedirectOp::HereDoc
        | RedirectOp::HereDocStrip
        | RedirectOp::HereString
        | RedirectOp::DupRead => 0,
        _ => 1,
    }
}

/// Build an `Open` step for `path` under `op`, onto `to`.
fn open_step(path: &[u8], op: RedirectOp, to: RawFd) -> Result<Step, ()> {
    let (flags, _) = match op {
        RedirectOp::Read => (libc::O_RDONLY, ()),
        RedirectOp::Write | RedirectOp::Clobber | RedirectOp::Both => {
            (libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC, ())
        }
        RedirectOp::Append | RedirectOp::BothAppend => {
            (libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND, ())
        }
        RedirectOp::ReadWrite => (libc::O_RDWR | libc::O_CREAT, ()),
        _ => {
            return Err(());
        }
    };
    Ok(Step::Open {
        path: path.to_vec(),
        flags,
        to,
    })
}

/// Apply one step to the current process.
fn apply_step(step: Step) -> std::io::Result<()> {
    match step {
        Step::Open { path, flags, to } => {
            let file = bsh_sys::open_cloexec(&path, flags, 0o666)?;
            adopt(file, to)
        }
        Step::Dup { from, to } => bsh_sys::dup2(from, to),
        Step::Close { to } => {
            if bsh_sys::fd_is_open(to) {
                bsh_sys::close(to)?;
            }
            Ok(())
        }
        Step::Temp { file, to } => adopt(OwnedFd::from(file), to),
    }
}

/// Move `file` onto `to`. When the descriptor already *is* `to` (a free
/// low-numbered descriptor, as `exec 3>file` produces), ownership transfers to
/// the target and must not be closed with the `OwnedFd`.
fn adopt(file: OwnedFd, to: RawFd) -> std::io::Result<()> {
    let fd = file.as_raw_fd();
    if fd == to {
        let _ = file.into_raw_fd();
        return Ok(());
    }
    let result = bsh_sys::dup2(fd, to);
    drop(file);
    result
}

fn step_target(step: &Step) -> RawFd {
    match step {
        Step::Open { to, .. }
        | Step::Dup { to, .. }
        | Step::Close { to }
        | Step::Temp { to, .. } => *to,
    }
}

/// Duplicate `target` to a spare descriptor, or `None` when it was closed.
fn save_descriptor(target: RawFd, reserved: &[RawFd]) -> Option<RawFd> {
    if !bsh_sys::fd_is_open(target) {
        return None;
    }
    for candidate in 20..=255 {
        if reserved.contains(&candidate) || bsh_sys::fd_is_open(candidate) {
            continue;
        }
        if bsh_sys::dup2(target, candidate).is_ok() {
            return Some(candidate);
        }
        return None;
    }
    None
}

/// A plain descriptor number, as `n>&m` requires.
fn descriptor(word: &[u8]) -> Option<RawFd> {
    if word.is_empty() || !word.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(word).ok()?.parse::<RawFd>().ok()
}

/// `<<-` removes leading tabs from every line of the body.
fn strip_leading_tabs(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut at_line_start = true;
    for &byte in body {
        if at_line_start && byte == b'\t' {
            continue;
        }
        at_line_start = byte == b'\n';
        out.push(byte);
    }
    out
}
