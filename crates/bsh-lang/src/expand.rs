//! The expansion pipeline, in five separately callable stages.
//!
//! POSIX XCU 2.6 fixes the order (and DESIGN.md §4.2 makes it explicit):
//!
//! 1. tilde expansion — [`expand_tilde`]
//! 2. parameter, command and arithmetic expansion, left to right —
//!    [`expand_parameters`]
//! 3. field splitting — [`split_fields`]
//! 4. pathname expansion — [`expand_pathnames`]
//! 5. quote removal — [`remove_quotes`]
//!
//! [`expand_word`] runs all five; [`expand_word_with`] runs them in a chosen
//! [`Mode`]. Each stage is a separate function over inspectable data so a
//! future `bsh explain` can print the intermediate result at every step
//! (DESIGN.md §5.6, §9).
//!
//! Command substitution, arithmetic evaluation and pathname matching are not
//! implemented here; they are the [`crate::Shell`] seam. The pipeline's own
//! work is the bookkeeping: which parts are quoted, which text is an
//! expansion result, IFS splitting and quote removal.
//!
//! # The one non-obvious rule
//!
//! Literal source text is *not* field-split: `IFS=:; printf '<%s>' a:b` is one
//! field, because POSIX XCU 2.6.5 splits only expansion results. But the
//! unquoted text *inside a parameter expansion's replacement word* is part of
//! that expansion's result, so `${x:-a:b}` with the same IFS is two fields.
//! That is why [`expand_parameters`] threads an `in_expansion` flag through
//! the walk while [`crate::Part::Lit`] at the top of a command word is never
//! splittable.

use std::fmt;

use bsh_diag::{Diagnostic, ErrCode, Span};

use crate::host::Shell;
use crate::ifs::Ifs;
use crate::vars::SetError;
use crate::word::{
    DoubleQ, Lit, LitKind, ParamExp, ParamName, ParamOp, Part, TildeSpec, UnsupportedOp, Word,
};

/// Diagnostic codes produced by this crate.
///
/// The numbers are provisional: the error catalog in DESIGN.md §4.10 is not
/// written yet, and the orchestrator owns the final allocation. Codes are
/// stable within this crate and are what `bsh` should match on.
pub mod codes {
    use bsh_diag::ErrCode;

    /// An expansion form this crate leaves out of scope on purpose.
    pub const UNSUPPORTED: ErrCode = ErrCode::new(301);
    /// `${x:?word}` on a null parameter.
    pub const PARAMETER_NULL: ErrCode = ErrCode::new(302);
    /// `${x?word}` on an unset parameter.
    pub const PARAMETER_UNSET: ErrCode = ErrCode::new(303);
    /// `${x:=word}` where the parameter cannot be assigned.
    pub const CANNOT_ASSIGN: ErrCode = ErrCode::new(304);
    /// A negative substring length that runs off the front of the value.
    pub const SUBSTRING_RANGE: ErrCode = ErrCode::new(305);
    /// A runtime callback (command substitution, arithmetic or glob) failed.
    pub const HOST: ErrCode = ErrCode::new(306);
}

/// An expansion failure.
///
/// Expansion is fallible because `${x:?msg}` is a command failure, because
/// `${x:=word}` may be refused by a read-only variable, and because the
/// runtime callbacks can fail. The error carries a [`Span`] so the runtime can
/// render it with the same machinery as syntax errors; [`Error::diagnostic`]
/// builds the [`Diagnostic`] for that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// What went wrong.
    pub kind: ErrorKind,
    /// Where in the source it happened.
    pub span: Span,
}

/// The kind of expansion failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// An operator this crate deliberately does not implement. Nothing was
    /// expanded, so no partial result escapes.
    Unsupported(UnsupportedOp),
    /// `${x:?word}`: the parameter is set but null (or unset).
    ParameterNull {
        /// The parameter name, for the message.
        name: Vec<u8>,
        /// The expanded message word, if one was given.
        message: Option<Vec<u8>>,
    },
    /// `${x?word}`: the parameter is unset.
    ParameterUnset {
        /// The parameter name, for the message.
        name: Vec<u8>,
        /// The expanded message word, if one was given.
        message: Option<Vec<u8>>,
    },
    /// `${x:=word}` tried to assign something that cannot be assigned.
    CannotAssign {
        /// The parameter name, for the message.
        name: Vec<u8>,
    },
    /// A negative substring length reached before the start of the value.
    SubstringRange {
        /// The length expression bash reports in its message.
        value: i64,
    },
    /// A [`Shell`] callback failed.
    Host {
        /// Human-readable detail from the runtime.
        message: String,
    },
}

impl Error {
    /// An error of `kind` at `span`.
    pub fn new(kind: ErrorKind, span: Span) -> Self {
        Self { kind, span }
    }

    /// Report an unsupported operator at `span`.
    pub fn unsupported(op: UnsupportedOp, span: Span) -> Self {
        Self::new(ErrorKind::Unsupported(op), span)
    }

    /// Report `${x:=word}` on an unassignable parameter.
    pub fn cannot_assign(name: Vec<u8>, span: Span) -> Self {
        Self::new(ErrorKind::CannotAssign { name }, span)
    }

    /// The diagnostic code for this error.
    pub fn code(&self) -> ErrCode {
        match &self.kind {
            ErrorKind::Unsupported(_) => codes::UNSUPPORTED,
            ErrorKind::ParameterNull { .. } => codes::PARAMETER_NULL,
            ErrorKind::ParameterUnset { .. } => codes::PARAMETER_UNSET,
            ErrorKind::CannotAssign { .. } => codes::CANNOT_ASSIGN,
            ErrorKind::SubstringRange { .. } => codes::SUBSTRING_RANGE,
            ErrorKind::Host { .. } => codes::HOST,
        }
    }

    /// Build the diagnostic IR for this error.
    pub fn diagnostic(&self) -> Diagnostic {
        Diagnostic::error(self.code(), self.span.clone(), self.to_string())
    }

    /// Map a [`Vars::set`](crate::Vars::set) refusal to an expansion error.
    fn from_set_error(err: SetError, name: &[u8], span: Span) -> Self {
        let _ = err; // ReadOnly and InvalidName share one user-facing outcome.
        Self::cannot_assign(name.to_vec(), span)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn text(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
            String::from_utf8_lossy(bytes)
        }
        match &self.kind {
            ErrorKind::Unsupported(op) => write!(f, "unsupported expansion: {}", op.description()),
            ErrorKind::ParameterNull { name, message } => match message {
                Some(message) => write!(f, "{}: {}", text(name), text(message)),
                None => write!(f, "{}: parameter null or not set", text(name)),
            },
            ErrorKind::ParameterUnset { name, message } => match message {
                Some(message) => write!(f, "{}: {}", text(name), text(message)),
                None => write!(f, "{}: parameter not set", text(name)),
            },
            ErrorKind::CannotAssign { name } => {
                write!(f, "{}: cannot assign in this way", text(name))
            }
            ErrorKind::SubstringRange { value } => {
                write!(f, "{value}: substring expression < 0")
            }
            ErrorKind::Host { message } => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

/// How a word is being expanded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// A command word: all five stages.
    Command,
    /// An assignment value or a `${x:=word}` value: no field splitting and no
    /// pathname expansion (POSIX XCU 2.6), but tilde and parameter expansion
    /// still apply.
    Assignment,
}

/// The result of expansion stages 1 and 2, before field splitting.
///
/// This is deliberately visible: `bsh explain` can print the chunks and their
/// flags, and the splitting stage is a pure function of this value and
/// [`Ifs`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Expanded {
    /// The chunks, in order. A chunk is a run of bytes with uniform flags.
    pub chunks: Vec<Chunk>,
    pending_break: bool,
}

/// A run of expanded bytes with uniform quoting properties.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// The bytes.
    pub bytes: Vec<u8>,
    /// These bytes are an unquoted expansion result and may be split by IFS
    /// (POSIX XCU 2.6.5). Literal source text and quoted text are not
    /// splittable.
    pub splittable: bool,
    /// The bytes are quoted: inert for pathname expansion.
    pub quoted: bool,
    /// A field boundary precedes this chunk, produced by `"$@"` or by an
    /// unquoted `$@` when IFS is empty. A boundary splits fields even without
    /// an IFS character.
    pub break_before: bool,
}

impl Expanded {
    /// An empty expansion.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a chunk. `quoted` and `splittable` are mutually exclusive;
    /// quoted text is never splittable.
    pub fn push(&mut self, bytes: Vec<u8>, quoted: bool, splittable: bool) {
        debug_assert!(!(quoted && splittable));
        let break_before = std::mem::take(&mut self.pending_break);
        self.chunks.push(Chunk {
            bytes,
            splittable: splittable && !quoted,
            quoted,
            break_before,
        });
    }

    /// Start a new field before the next chunk, if there is a current one.
    pub fn break_field(&mut self) {
        if !self.chunks.is_empty() {
            self.pending_break = true;
        }
    }

    /// Whether nothing at all was produced.
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// Collapse to a single string for a scalar context (an assignment value
    /// or a `${x:?word}` message).
    ///
    /// Field boundaries cannot be represented in a scalar, so they become a
    /// space — bash's behaviour for `v="$@"` and `v=$@`: `set -- a b; v="$@"`
    /// sets `v` to `a b`.
    pub fn into_scalar(self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut first = true;
        for chunk in self.chunks {
            if chunk.break_before && !first {
                out.push(b' ');
            }
            out.extend_from_slice(&chunk.bytes);
            first = false;
        }
        out
    }
}

/// A field after splitting and before quote removal.
///
/// `quoted[i]` says whether `bytes[i]` was quoted. Pathname expansion uses it
/// so that only unquoted `*`, `?` and `[` are metacharacters; quote removal
/// then just keeps the bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    /// The bytes.
    pub bytes: Vec<u8>,
    /// Per-byte quoting mask; always the same length as `bytes`.
    pub quoted: Vec<bool>,
}

impl Field {
    /// Construct a field. `quoted.len()` must equal `bytes.len()`.
    pub fn new(bytes: Vec<u8>, quoted: Vec<bool>) -> Self {
        assert_eq!(bytes.len(), quoted.len(), "quoting mask must match bytes");
        Self { bytes, quoted }
    }

    /// An unquoted field.
    pub fn plain(bytes: Vec<u8>) -> Self {
        let len = bytes.len();
        Self {
            bytes,
            quoted: vec![false; len],
        }
    }

    /// A field produced by the runtime's pathname expansion: treated as fully
    /// quoted, so it is never globbed again.
    pub fn matched(bytes: Vec<u8>) -> Self {
        let len = bytes.len();
        Self {
            bytes,
            quoted: vec![true; len],
        }
    }

    /// Whether a glob metacharacter is active in this field.
    pub fn has_active_glob(&self) -> bool {
        self.bytes
            .iter()
            .zip(&self.quoted)
            .any(|(&b, &q)| !q && matches!(b, b'*' | b'?' | b'['))
    }
}

/// Stage 1: tilde expansion (POSIX XCU 2.6.1).
///
/// Replaces every [`Part::Tilde`] the front end emitted with a literal marked
/// [`LitKind::Quoted`], because the result is "treated as if it were quoted":
/// a home directory containing a space is one field, and one containing `[`
/// does not turn into a pattern.
///
/// For convenience a leading `~` in an unquoted literal is recognised too, so
/// a front end that does not track tilde prefixes still gets the common case
/// right. Tildes are expanded inside the replacement words of parameter
/// expansions (bash does: `${x:-~}` is the home directory) unless the
/// expansion is inside double quotes, where `~` is literal.
///
/// `~user` is resolved through [`Shell::home_dir`]; an unknown user leaves the
/// text unchanged (bash's behaviour). `~+` and `~-` use `$PWD`/`$OLDPWD`;
/// bash's directory-stack forms (`~+2`, `~-3`) are left alone.
pub fn expand_tilde<S: Shell>(word: &Word, shell: &mut S) -> Result<Word, Error> {
    let mut word = word.clone();
    expand_tilde_word(&mut word, false, shell);
    Ok(word)
}

fn expand_tilde_word<S: Shell>(word: &mut Word, quoted: bool, shell: &mut S) {
    if !quoted {
        expand_leading_tilde(word, shell);
    }
    for part in &mut word.parts {
        expand_tilde_part(part, quoted, shell);
    }
}

fn expand_tilde_part<S: Shell>(part: &mut Part, quoted: bool, shell: &mut S) {
    match part {
        Part::DoubleQ(DoubleQ { parts }) => {
            for part in parts {
                expand_tilde_part(part, true, shell);
            }
        }
        Part::Param(ParamExp { op, .. }) => {
            if let Some(word) = op.word_mut() {
                expand_tilde_word(word, quoted, shell);
            }
        }
        _ => {}
    }
}

fn expand_leading_tilde<S: Shell>(word: &mut Word, shell: &mut S) {
    enum Leading {
        Tilde(TildeSpec),
        Lit(Vec<u8>),
        Other,
    }
    let leading = match word.parts.first() {
        Some(Part::Tilde(spec)) => Leading::Tilde(spec.clone()),
        Some(Part::Lit(Lit {
            kind: LitKind::Unquoted,
            bytes,
        })) => Leading::Lit(bytes.clone()),
        _ => Leading::Other,
    };
    match leading {
        Leading::Tilde(spec) => {
            if let Some(home) = resolve_tilde(&spec.user, shell) {
                word.parts[0] = Part::Lit(Lit::quoted(home));
            }
        }
        Leading::Lit(bytes) => {
            if bytes.first() != Some(&b'~') {
                return;
            }
            let end = bytes.iter().position(|&b| b == b'/').unwrap_or(bytes.len());
            let user = &bytes[1..end];
            let Some(home) = resolve_tilde(user, shell) else {
                return;
            };
            let rest = bytes[end..].to_vec();
            let mut parts = Vec::with_capacity(word.parts.len() + 1);
            parts.push(Part::Lit(Lit::quoted(home)));
            if !rest.is_empty() {
                parts.push(Part::Lit(Lit::unquoted(rest)));
            }
            parts.extend(word.parts.drain(1..));
            word.parts = parts;
        }
        Leading::Other => {}
    }
}

fn resolve_tilde<S: Shell>(user: &[u8], shell: &mut S) -> Option<Vec<u8>> {
    if user.is_empty() {
        if let Some(home) = shell.get(b"HOME") {
            return Some(home.to_vec());
        }
        return shell.home_dir(b"");
    }
    if user == b"+" {
        return shell.get(b"PWD").map(<[u8]>::to_vec);
    }
    if user == b"-" {
        return shell.get(b"OLDPWD").map(<[u8]>::to_vec);
    }
    // `~+N` / `~-N` / `~N` address bash's directory stack, which this crate
    // does not model; leaving the text alone is visible, not silently wrong.
    let stack_form = user
        .strip_prefix(b"+")
        .or_else(|| user.strip_prefix(b"-"))
        .unwrap_or(user);
    if !stack_form.is_empty() && stack_form.iter().all(u8::is_ascii_digit) {
        return None;
    }
    shell.home_dir(user)
}

/// Stage 2: parameter, command and arithmetic expansion, left to right.
///
/// Walks the parts in source order, so side effects (command substitutions,
/// `${x:=word}` assignments) happen exactly when POSIX says they do: a
/// replacement word that is not used is never expanded.
///
/// Command substitution and arithmetic are delegated to [`Shell`]; their
/// results are unquoted expansion results and are therefore splittable unless
/// the part is inside double quotes. `$@` and `$*` are built here:
///
/// * quoted `$@` produces one field per positional parameter, with a field
///   boundary before each one after the first; zero parameters produce
///   nothing at all, which is how `"$@"` vanishes while `""` does not;
/// * unquoted `$@` and `$*` are identical, as POSIX says they are outside
///   double quotes: the parameters are joined by the first IFS character and
///   the result is split. When IFS is set but empty there is no separator, so
///   each parameter becomes its own field instead;
/// * quoted `$*` joins with the first IFS character into one quoted chunk.
///
/// The one bash quirk this deliberately does *not* reproduce: bash treats an
/// unquoted `$@` inside `${x:-…}` specially (it joins with spaces and, when the
/// first IFS character is not whitespace, suppresses splitting). Here the
/// ordinary unquoted `$@` rules apply; `${x:-"$@"}` — the case DESIGN.md §3.3
/// Tier C pins — behaves exactly like bash and produces one field per
/// parameter.
///
/// IFS is read from `shell` at each `$*` / `$@` expansion and again by
/// [`split_fields`], which is what bash does: in `${IFS:=:}$*` the assignment
/// takes effect before the `$*` is joined, so the join uses the new value.
pub fn expand_parameters<S: Shell>(
    word: &Word,
    shell: &mut S,
    mode: Mode,
) -> Result<Expanded, Error> {
    let mut out = Expanded::new();
    let mut ctx = Ctx { shell, mode };
    expand_parts(&word.parts, false, false, &mut ctx, &mut out)?;
    Ok(out)
}

struct Ctx<'a, S: Shell> {
    shell: &'a mut S,
    mode: Mode,
}

impl<S: Shell> Ctx<'_, S> {
    fn value(&self, name: &ParamName) -> Option<Vec<u8>> {
        match name {
            ParamName::Var(name) => self.shell.get(name).map(<[u8]>::to_vec),
            ParamName::Positional(n) => {
                if *n == 0 {
                    self.shell.get(b"0").map(<[u8]>::to_vec)
                } else {
                    self.shell.positional().get(n - 1).cloned()
                }
            }
            ParamName::Count => Some(self.shell.positional().len().to_string().into_bytes()),
            ParamName::At | ParamName::Star => None,
        }
    }

    fn is_set(&self, name: &ParamName) -> bool {
        match name {
            ParamName::At | ParamName::Star => !self.shell.positional().is_empty(),
            _ => self.value(name).is_some(),
        }
    }

    fn is_nonnull(&self, name: &ParamName) -> bool {
        match name {
            ParamName::At | ParamName::Star => !self.shell.positional().is_empty(),
            _ => self.value(name).is_some_and(|value| !value.is_empty()),
        }
    }
}

fn expand_parts<S: Shell>(
    parts: &[Part],
    quoted: bool,
    in_expansion: bool,
    ctx: &mut Ctx<'_, S>,
    out: &mut Expanded,
) -> Result<(), Error> {
    for part in parts {
        match part {
            Part::Lit(lit) => {
                let quoted = quoted || lit.kind.is_quoted();
                out.push(lit.bytes.clone(), quoted, in_expansion && !quoted);
            }
            Part::DoubleQ(DoubleQ { parts }) => {
                if parts.is_empty() {
                    // `""` is one empty quoted field, unlike `"$@"` with no
                    // parameters, which produces no chunks at all.
                    out.push(Vec::new(), true, false);
                } else {
                    expand_parts(parts, true, in_expansion, ctx, out)?;
                }
            }
            Part::Tilde(spec) => {
                // Left over only if stage 1 was skipped or left it unresolved.
                out.push(spec.source_bytes(), quoted, in_expansion && !quoted);
            }
            Part::Param(exp) => expand_param(exp, quoted, ctx, out)?,
            Part::CmdSub(cmd) => {
                let bytes = ctx.shell.command_substitution(cmd)?;
                out.push(bytes, quoted, !quoted);
            }
            Part::Arith(expr) => {
                let bytes = ctx.shell.arithmetic(expr)?;
                out.push(bytes, quoted, !quoted);
            }
            Part::ProcSub(subs) => {
                // A /dev/fd path: never split, never globbed.
                let bytes = ctx.shell.process_substitution(subs)?;
                out.push(bytes, true, false);
            }
        }
    }
    Ok(())
}

fn expand_param<S: Shell>(
    exp: &ParamExp,
    quoted: bool,
    ctx: &mut Ctx<'_, S>,
    out: &mut Expanded,
) -> Result<(), Error> {
    let span = exp.span.clone();
    match &exp.op {
        ParamOp::Unsupported(op) => Err(Error::unsupported(*op, span)),
        ParamOp::Length => {
            let len = match &exp.name {
                ParamName::At | ParamName::Star => ctx.shell.positional().len(),
                name => ctx.value(name).map_or(0, |value| value.len()),
            };
            out.push(len.to_string().into_bytes(), quoted, !quoted);
            Ok(())
        }
        ParamOp::Substring { offset, length } => {
            if matches!(&exp.name, ParamName::At | ParamName::Star) {
                // bash slices the parameter *list* here; doing it on a joined
                // string would be quietly wrong.
                return Err(Error::unsupported(UnsupportedOp::SubstringOfList, span));
            }
            let Some(value) = ctx.value(&exp.name) else {
                out.push(Vec::new(), quoted, !quoted);
                return Ok(());
            };
            let slice = substring(&value, *offset, *length, span)?;
            out.push(slice.to_vec(), quoted, !quoted);
            Ok(())
        }
        ParamOp::Upper | ParamOp::Lower | ParamOp::UpperFirst | ParamOp::LowerFirst => {
            if matches!(&exp.name, ParamName::At | ParamName::Star) {
                return Err(Error::unsupported(UnsupportedOp::CaseOfList, span));
            }
            let mut value = ctx.value(&exp.name).unwrap_or_default();
            apply_case(&mut value, &exp.op);
            out.push(value, quoted, !quoted);
            Ok(())
        }
        ParamOp::Value => {
            substitute(&exp.name, quoted, ctx, out);
            Ok(())
        }
        ParamOp::UseDefault { word, colon } => {
            let use_word = !ctx.is_set(&exp.name) || (*colon && !ctx.is_nonnull(&exp.name));
            if use_word {
                expand_word_field(word, quoted, ctx, out)
            } else {
                substitute(&exp.name, quoted, ctx, out);
                Ok(())
            }
        }
        ParamOp::UseAlternate { word, colon } => {
            let use_word = ctx.is_set(&exp.name) && (!*colon || ctx.is_nonnull(&exp.name));
            if use_word {
                expand_word_field(word, quoted, ctx, out)
            } else {
                // An empty quoted expansion is still one empty field:
                // `"${x:+}"` with `x` unset is `""`.
                out.push(Vec::new(), quoted, !quoted);
                Ok(())
            }
        }
        ParamOp::AssignDefault { word, colon } => {
            let assign = !ctx.is_set(&exp.name) || (*colon && !ctx.is_nonnull(&exp.name));
            if !assign {
                substitute(&exp.name, quoted, ctx, out);
                return Ok(());
            }
            let name = assignable_name(&exp.name)
                .ok_or_else(|| Error::cannot_assign(exp.name.display_bytes(), span.clone()))?;
            let value = expand_word_to_scalar(word, quoted, ctx)?;
            ctx.shell
                .set(name, value.clone())
                .map_err(|err| Error::from_set_error(err, name, span.clone()))?;
            out.push(value, quoted, !quoted);
            Ok(())
        }
        ParamOp::Error { word, colon } => {
            let fail = !ctx.is_set(&exp.name) || (*colon && !ctx.is_nonnull(&exp.name));
            if !fail {
                substitute(&exp.name, quoted, ctx, out);
                return Ok(());
            }
            let message = match word {
                Some(word) => Some(expand_word_to_scalar(word, quoted, ctx)?),
                None => None,
            };
            let name = exp.name.display_bytes();
            let kind = if *colon {
                ErrorKind::ParameterNull { name, message }
            } else {
                ErrorKind::ParameterUnset { name, message }
            };
            Err(Error::new(kind, span))
        }
    }
}

/// Substitute the value of `name` as an expansion result.
fn substitute<S: Shell>(name: &ParamName, quoted: bool, ctx: &mut Ctx<'_, S>, out: &mut Expanded) {
    match name {
        ParamName::At => emit_at(ctx, quoted, out),
        ParamName::Star => emit_star(ctx, quoted, out),
        _ => {
            let value = ctx.value(name).unwrap_or_default();
            out.push(value, quoted, !quoted);
        }
    }
}

/// `$@`: one field per positional parameter when quoted, otherwise a joined
/// (and therefore splittable) expansion result.
fn emit_at<S: Shell>(ctx: &mut Ctx<'_, S>, quoted: bool, out: &mut Expanded) {
    if quoted {
        for (index, param) in ctx.shell.positional().iter().enumerate() {
            if index > 0 {
                out.break_field();
            }
            out.push(param.clone(), true, false);
        }
        return;
    }
    match ctx.mode {
        // A scalar context cannot carry field boundaries; bash joins with a
        // space for `$@` in assignments regardless of IFS.
        Mode::Assignment => {
            out.push(join_positionals(ctx, Some(b' ')), false, true);
        }
        Mode::Command => {
            let ifs = Ifs::from_vars(&*ctx.shell);
            match ifs.join_separator() {
                Some(separator) => out.push(join_positionals(ctx, Some(separator)), false, true),
                // IFS is set but empty: no separator to join with, so keep
                // the parameters apart with boundaries (bash's behaviour).
                None => emit_unquoted_list(ctx, out),
            }
        }
    }
}

/// `$*`: one joined expansion result when IFS has a character to join with.
/// Quoted it is one field; unquoted it is split like any other expansion
/// result. An empty IFS cannot join, so unquoted `$*` behaves like unquoted
/// `$@` (bash does the same).
fn emit_star<S: Shell>(ctx: &mut Ctx<'_, S>, quoted: bool, out: &mut Expanded) {
    let ifs = Ifs::from_vars(&*ctx.shell);
    let separator = ifs.join_separator();
    if quoted {
        out.push(join_positionals(ctx, separator), true, false);
        return;
    }
    match separator {
        Some(separator) => out.push(join_positionals(ctx, Some(separator)), false, true),
        None => emit_unquoted_list(ctx, out),
    }
}

/// One field boundary per positional parameter, used by unquoted `$@` (and
/// unquoted `$*` with an empty IFS) when there is no separator to join with.
fn emit_unquoted_list<S: Shell>(ctx: &Ctx<'_, S>, out: &mut Expanded) {
    for (index, param) in ctx.shell.positional().iter().enumerate() {
        if index > 0 {
            out.break_field();
        }
        out.push(param.clone(), false, true);
    }
}

fn join_positionals<S: Shell>(ctx: &Ctx<'_, S>, separator: Option<u8>) -> Vec<u8> {
    let mut out = Vec::new();
    for (index, param) in ctx.shell.positional().iter().enumerate() {
        if index > 0 {
            if let Some(separator) = separator {
                out.push(separator);
            }
        }
        out.extend_from_slice(param);
    }
    out
}

fn expand_word_into<S: Shell>(
    word: &Word,
    quoted: bool,
    ctx: &mut Ctx<'_, S>,
    out: &mut Expanded,
) -> Result<(), Error> {
    // Inside a replacement word the unquoted literal text is part of the
    // parameter expansion's result, so it *is* subject to field splitting:
    // `${x:-a:b}` with IFS=: is two fields while `a:b` alone is one.
    expand_parts(&word.parts, quoted, true, ctx, out)
}

/// Expand a replacement word, making sure a quoted empty word still produces
/// one empty field (`"${x:-}"` is `""`, not nothing), while an unquoted empty
/// word produces no field at all.
fn expand_word_field<S: Shell>(
    word: &Word,
    quoted: bool,
    ctx: &mut Ctx<'_, S>,
    out: &mut Expanded,
) -> Result<(), Error> {
    let before = out.chunks.len();
    expand_word_into(word, quoted, ctx, out)?;
    if quoted && out.chunks.len() == before {
        out.push(Vec::new(), true, false);
    }
    Ok(())
}

fn expand_word_to_scalar<S: Shell>(
    word: &Word,
    quoted: bool,
    ctx: &mut Ctx<'_, S>,
) -> Result<Vec<u8>, Error> {
    let mut expanded = Expanded::new();
    {
        // A scalar context: `${x:=word}` and `${x:?word}` never split or glob,
        // and `$@` cannot produce fields, so use the assignment rules.
        let mut scalar = Ctx {
            shell: &mut *ctx.shell,
            mode: Mode::Assignment,
        };
        expand_parts(&word.parts, quoted, true, &mut scalar, &mut expanded)?;
    }
    Ok(expanded.into_scalar())
}

fn apply_case(value: &mut [u8], op: &ParamOp) {
    match op {
        ParamOp::Upper => value.make_ascii_uppercase(),
        ParamOp::Lower => value.make_ascii_lowercase(),
        ParamOp::UpperFirst => {
            if let Some(first) = value.first_mut() {
                *first = first.to_ascii_uppercase();
            }
        }
        ParamOp::LowerFirst => {
            if let Some(first) = value.first_mut() {
                *first = first.to_ascii_lowercase();
            }
        }
        _ => {}
    }
}

/// `${x:offset:length}` on a byte string.
///
/// Byte units, i.e. `LC_ALL=C` semantics. bash reports a negative *length*
/// that reaches past the start as `substring expression < 0`, and an offset
/// that reaches past the start simply yields nothing; both are reproduced.
fn substring(value: &[u8], offset: i64, length: Option<i64>, span: Span) -> Result<&[u8], Error> {
    let len = value.len() as i64;
    let start = if offset < 0 { len + offset } else { offset };
    if start < 0 || start > len {
        return Ok(&[]);
    }
    let end = match length {
        None => len,
        Some(length) if length >= 0 => (start + length).min(len),
        Some(length) => {
            let end = len + length;
            if end < start {
                return Err(Error::new(
                    ErrorKind::SubstringRange { value: length },
                    span,
                ));
            }
            end.min(len)
        }
    };
    Ok(&value[start as usize..end as usize])
}

fn assignable_name(name: &ParamName) -> Option<&[u8]> {
    match name {
        ParamName::Var(name) if is_identifier(name) => Some(name),
        _ => None,
    }
}

fn is_identifier(name: &[u8]) -> bool {
    match name.split_first() {
        Some((first, rest)) => {
            (first.is_ascii_alphabetic() || *first == b'_')
                && rest.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_')
        }
        None => false,
    }
}

/// Stage 3: field splitting (POSIX XCU 2.6.5).
///
/// Consumes the chunks from stage 2 and produces fields. Only bytes marked
/// [`Chunk::splittable`] are examined for IFS characters; literal and quoted
/// bytes are appended verbatim, but they still confirm a pending whitespace
/// delimiter (so `$x"b"` with `x="a "` is two fields, `a` and `b`). The rules
/// themselves are documented on [`Ifs`] and quoted from POSIX XCU 2.6.5.
pub fn split_fields(expanded: &Expanded, ifs: &Ifs) -> Vec<Field> {
    let mut splitter = Splitter::new();
    for chunk in &expanded.chunks {
        if chunk.break_before {
            splitter.break_field();
        }
        if chunk.quoted && chunk.bytes.is_empty() {
            // A quoted empty expansion (`""`, `"$unset"`) is a real field.
            // Pending unquoted whitespace has to become a delimiter first:
            // `$x""` with x='a ' is `a` and ``, not one field.
            splitter.commit_pending();
            splitter.has_quoted = true;
        }
        for &byte in &chunk.bytes {
            if chunk.splittable && ifs.contains(byte) {
                if Ifs::is_whitespace(byte) {
                    splitter.pending = true;
                } else {
                    splitter.pending = false;
                    splitter.finish_field();
                }
            } else {
                splitter.commit_pending();
                splitter.current.push(byte);
                splitter.quoted.push(chunk.quoted);
                if chunk.quoted {
                    splitter.has_quoted = true;
                }
            }
        }
    }
    splitter.finish();
    splitter.fields
}

struct Splitter {
    fields: Vec<Field>,
    current: Vec<u8>,
    quoted: Vec<bool>,
    has_quoted: bool,
    pending: bool,
}

impl Splitter {
    fn new() -> Self {
        Self {
            fields: Vec::new(),
            current: Vec::new(),
            quoted: Vec::new(),
            has_quoted: false,
            pending: false,
        }
    }

    fn has_content(&self) -> bool {
        !self.current.is_empty() || self.has_quoted
    }

    /// Pending IFS whitespace becomes a delimiter only when more content
    /// follows a non-empty field; leading and trailing whitespace is ignored.
    fn commit_pending(&mut self) {
        if std::mem::take(&mut self.pending) && self.has_content() {
            self.finish_field();
        }
    }

    fn finish_field(&mut self) {
        self.pending = false;
        self.fields.push(Field {
            bytes: std::mem::take(&mut self.current),
            quoted: std::mem::take(&mut self.quoted),
        });
        self.has_quoted = false;
    }

    /// A `"$@"` boundary: always a field break, but an empty unquoted field
    /// (an empty positional parameter in unquoted `$@` with empty IFS) is
    /// dropped.
    fn break_field(&mut self) {
        self.pending = false;
        if self.has_content() {
            self.finish_field();
        } else {
            self.current.clear();
            self.quoted.clear();
            self.has_quoted = false;
        }
    }

    fn finish(&mut self) {
        self.pending = false;
        if self.has_content() {
            self.finish_field();
        }
    }
}

/// Stage 4: pathname expansion (POSIX XCU 2.6.6).
///
/// Delegates matching to [`Shell::pathname`]. Fields with no active glob
/// metacharacter are passed through untouched, and a `None` from the callback
/// leaves the field as written (POSIX's default, and bash's without
/// `nullglob`). Matches are marked fully quoted so nothing is globbed twice.
pub fn expand_pathnames<S: Shell>(fields: Vec<Field>, shell: &mut S) -> Result<Vec<Field>, Error> {
    let mut out = Vec::with_capacity(fields.len());
    for field in fields {
        if !field.has_active_glob() {
            out.push(field);
            continue;
        }
        match shell.pathname(&field)? {
            Some(matches) => out.extend(matches.into_iter().map(Field::matched)),
            None => out.push(field),
        }
    }
    Ok(out)
}

/// Stage 5: quote removal (POSIX XCU 2.6.7).
///
/// In a model where the lexer already interpreted quoting, quote removal is
/// the point at which the per-byte quoting mask, which only field splitting
/// and pathname expansion needed, is dropped and argv bytes remain.
pub fn remove_quotes(fields: Vec<Field>) -> Vec<Vec<u8>> {
    fields.into_iter().map(|field| field.bytes).collect()
}

/// Expand one command word: all five stages, in order.
///
/// Tilde expansion, parameter expansion, splitting, pathname expansion and
/// quote removal — exactly the order POSIX XCU 2.6 requires.
pub fn expand_word<S: Shell>(word: &Word, shell: &mut S) -> Result<Vec<Vec<u8>>, Error> {
    expand_word_with(word, shell, Mode::Command)
}

/// Expand one word in the given [`Mode`].
///
/// [`Mode::Assignment`] runs stages 1, 2 and 5 only: an assignment value is
/// neither field-split nor pathname-expanded (POSIX XCU 2.6), so the result
/// is always exactly one string.
pub fn expand_word_with<S: Shell>(
    word: &Word,
    shell: &mut S,
    mode: Mode,
) -> Result<Vec<Vec<u8>>, Error> {
    let word = expand_tilde(word, shell)?;
    let expanded = expand_parameters(&word, shell, mode)?;
    match mode {
        Mode::Command => {
            // Read IFS after expansion: a `${IFS:=…}` in this very word has
            // already happened, and bash splits with the new value.
            let ifs = Ifs::from_vars(&*shell);
            let fields = split_fields(&expanded, &ifs);
            let fields = expand_pathnames(fields, shell)?;
            Ok(remove_quotes(fields))
        }
        Mode::Assignment => Ok(vec![expanded.into_scalar()]),
    }
}

/// Expand a word as an assignment value and return its bytes.
///
/// This is the shape `x=word` and `x+=word` need: tilde expansion, parameter
/// expansion and quote removal, with no field splitting or globbing.
pub fn expand_assignment<S: Shell>(word: &Word, shell: &mut S) -> Result<Vec<u8>, Error> {
    let mut words = expand_word_with(word, shell, Mode::Assignment)?;
    Ok(words.pop().unwrap_or_default())
}

/// Expand a command's words in order, concatenating the resulting argv.
///
/// Each word is expanded independently, so `"$@"` produces its fields in place
/// and a command substitution in one word cannot change the splitting of
/// another.
pub fn expand_words<S: Shell>(words: &[Word], shell: &mut S) -> Result<Vec<Vec<u8>>, Error> {
    let mut argv = Vec::new();
    for word in words {
        argv.extend(expand_word(word, shell)?);
    }
    Ok(argv)
}
