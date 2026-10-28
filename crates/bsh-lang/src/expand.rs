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
use bsh_pattern::Flags;

use crate::host::Shell;
use crate::ifs::Ifs;
use crate::vars::{ArrayKind, ArrayValue, SetError};
use crate::word::{
    CaseHow, DoubleQ, Lit, LitKind, ParamExp, ParamName, ParamOp, Part, ReplaceMode, TildeSpec,
    UnsupportedOp, Word,
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
    /// A glob matched nothing under `shopt -s failglob`.
    pub const GLOB_NO_MATCH: ErrCode = ErrCode::new(307);
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
    /// A glob with `shopt -s failglob` matched nothing.
    ///
    /// Distinct from [`ErrorKind::Host`] because the shell must not exit on it:
    /// bash fails the *command* and a script continues.
    GlobNoMatch {
        /// The pattern that matched nothing.
        pattern: Vec<u8>,
    },
    /// A [`Shell`] callback failed (command substitution, arithmetic, globbing).
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
            // A glob that matched nothing under `failglob` is its own code:
            // the shell reports it and fails the command, rather than exiting.
            ErrorKind::GlobNoMatch { .. } => codes::GLOB_NO_MATCH,
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
            ErrorKind::GlobNoMatch { pattern } => {
                write!(f, "no match: {}", text(pattern))
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
    ///
    /// With `extglob` on, the group openers are metacharacters too: `@(a)` is
    /// a pattern even though it contains no `*`, and failing to say so leaves
    /// it unexpanded, which is a whole class of silently-wrong results rather
    /// than an error.
    pub fn has_active_glob(&self) -> bool {
        self.has_active_glob_with(false)
    }

    /// [`Field::has_active_glob`], with the shell's `extglob` setting.
    pub fn has_active_glob_with(&self, extglob: bool) -> bool {
        self.bytes
            .iter()
            .zip(&self.quoted)
            .enumerate()
            .any(|(i, (&b, &q))| {
                if q {
                    return false;
                }
                match b {
                    b'*' | b'?' | b'[' => true,
                    b'@' | b'+' | b'!' => {
                        extglob && self.bytes.get(i + 1) == Some(&b'(') && !self.quoted[i + 1]
                    }
                    _ => false,
                }
            })
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
    expand_tilde_word(&mut word, false, false, shell);
    Ok(word)
}

/// Tilde expansion for an assignment value.
///
/// In an assignment POSIX expands a tilde at the start of the value *and*
/// after every unquoted `:` (that is how `PATH=~:~/bin` works), which a
/// command word does not do. `~` and `~user` are both recognised; an unknown
/// login name is left as written.
pub fn expand_assignment_tilde<S: Shell>(word: &Word, shell: &mut S) -> Result<Word, Error> {
    let mut word = word.clone();
    expand_tilde_word(&mut word, false, true, shell);
    Ok(word)
}

fn expand_tilde_word<S: Shell>(word: &mut Word, quoted: bool, assignment: bool, shell: &mut S) {
    if !quoted {
        if assignment {
            expand_assignment_tildes(word, shell);
        } else {
            expand_leading_tilde(word, shell);
        }
    }
    for part in &mut word.parts {
        expand_tilde_part(part, quoted, assignment, shell);
    }
}

fn expand_tilde_part<S: Shell>(part: &mut Part, quoted: bool, assignment: bool, shell: &mut S) {
    match part {
        Part::DoubleQ(DoubleQ { parts }) => {
            for part in parts {
                expand_tilde_part(part, true, assignment, shell);
            }
        }
        Part::Param(ParamExp { op, .. }) => {
            // A replacement word keeps the enclosing quote context: bash leaves
            // `"${x:-~}"` literal.
            if let Some(word) = op.word_mut() {
                expand_tilde_word(word, quoted, assignment, shell);
            }
            // A pattern or replacement of a pattern operator does not: bash
            // expands `"${x#~}"` to `/foo` and `"${v/b/~/}"` to `a/h/c`, so
            // these sub-words are expanded as if they were unquoted.
            op.visit_pattern_words_mut(&mut |word| {
                expand_tilde_word(word, false, assignment, shell)
            });
        }
        _ => {}
    }
}

/// Expand `~`/`~user` at the start of the value and after each unquoted `:`.
fn expand_assignment_tildes<S: Shell>(word: &mut Word, shell: &mut S) {
    let mut at_segment_start = true;
    for part in &mut word.parts {
        if let Part::Tilde(spec) = part {
            let replacement = if at_segment_start {
                resolve_tilde(&spec.user, shell)
            } else {
                None
            };
            *part = match replacement {
                Some(home) => Part::Lit(Lit::quoted(home)),
                None => Part::Lit(Lit::unquoted(spec.source_bytes())),
            };
            at_segment_start = false;
            continue;
        }
        let Part::Lit(lit) = part else {
            at_segment_start = false;
            continue;
        };
        if lit.kind != LitKind::Unquoted {
            // Quoted text is never a tilde prefix, and it ends the segment.
            at_segment_start = false;
            continue;
        }
        let bytes = std::mem::take(&mut lit.bytes);
        let mut out = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            if at_segment_start && bytes[index] == b'~' {
                let end = bytes[index + 1..]
                    .iter()
                    .position(|&byte| byte == b'/' || byte == b':')
                    .map_or(bytes.len(), |offset| index + 1 + offset);
                let user = &bytes[index + 1..end];
                if let Some(home) = resolve_tilde(user, shell) {
                    out.extend_from_slice(&home);
                    index = end;
                    at_segment_start = false;
                    continue;
                }
            }
            out.push(bytes[index]);
            at_segment_start = bytes[index] == b':';
            index += 1;
        }
        lit.bytes = out;
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
    /// The scalar value of `name`.
    ///
    /// Arrays decay to element zero: bash's `${a}` is `${a[0]}` for both
    /// shapes (an associative array looks up the key `0`). That is what makes
    /// `b="${a[@]}"` and `echo $a` behave like bash rather than like an
    /// unset variable.
    fn value(&self, name: &ParamName) -> Option<Vec<u8>> {
        match name {
            ParamName::Var(name) => match self.shell.get(name) {
                Some(value) => Some(value.to_vec()),
                None => self.shell.array(name).and_then(|array| match array {
                    ArrayValue::Indexed(elements) => elements.get(&0).cloned(),
                    ArrayValue::Assoc(pairs) => pairs
                        .iter()
                        .find(|(key, _)| key.as_slice() == b"0")
                        .map(|(_, value)| value.clone()),
                }),
            },
            ParamName::Positional(n) => {
                if *n == 0 {
                    self.shell.get(b"0").map(<[u8]>::to_vec)
                } else {
                    self.shell.positional().get(n - 1).cloned()
                }
            }
            ParamName::Count => Some(self.shell.positional().len().to_string().into_bytes()),
            ParamName::At | ParamName::Star => None,
            ParamName::Element { .. } | ParamName::Elements { .. } => None,
        }
    }

    fn is_set(&mut self, name: &ParamName) -> bool {
        match name {
            ParamName::At | ParamName::Star => !self.shell.positional().is_empty(),
            ParamName::Element { name, subscript } => read_element(self, name, subscript).is_some(),
            ParamName::Elements { name, .. } => self
                .shell
                .array(name)
                .map(|array| !array.is_empty())
                .unwrap_or_else(|| self.shell.get(name).is_some()),
            _ => self.value(name).is_some(),
        }
    }

    fn is_nonnull(&mut self, name: &ParamName) -> bool {
        match name {
            ParamName::At | ParamName::Star => !self.shell.positional().is_empty(),
            ParamName::Element { name, subscript } => {
                read_element(self, name, subscript).is_some_and(|value| !value.is_empty())
            }
            ParamName::Elements { name, .. } => {
                // bash's `${a[@]:-word}` rule: a one-element list behaves like
                // a scalar for the null test, while a longer list is non-null
                // as a list (`a=("" ""); "${a[@]:-x}"` yields two empties).
                match self.shell.array(name) {
                    Some(array) => match array.len() {
                        0 => false,
                        1 => array
                            .values()
                            .first()
                            .is_some_and(|value| !value.is_empty()),
                        _ => true,
                    },
                    None => self.shell.get(name).is_some_and(|value| !value.is_empty()),
                }
            }
            _ => self.value(name).is_some_and(|value| !value.is_empty()),
        }
    }
}

/// `subscript` inside `${a[subscript]}` for a *read*.
///
/// The word is expanded first, then interpreted by the array's shape: an
/// associative array uses the bytes as the key, an indexed one (and a scalar,
/// which behaves like a one-element indexed array) evaluates them
/// arithmetically. The arithmetic callback is the same one `$((…))` uses, so
/// `${a[i+1]}` and `$((i+1))` cannot disagree.
fn read_element<S: Shell>(ctx: &mut Ctx<'_, S>, name: &[u8], subscript: &Word) -> Option<Vec<u8>> {
    let kind = ctx.shell.array(name).map(|array| array.kind());
    let text = expand_word_to_scalar(subscript, false, ctx).ok()?;
    match kind {
        Some(ArrayKind::Associative) => ctx
            .shell
            .array(name)
            .and_then(|array| array.get(&text))
            .map(<[u8]>::to_vec),
        Some(ArrayKind::Indexed) => {
            let index = arithmetic(&text, subscript, ctx).ok()?;
            let key = index.to_string().into_bytes();
            ctx.shell
                .array(name)
                .and_then(|array| array.get(&key))
                .map(<[u8]>::to_vec)
        }
        // No shape: bash treats the subscript arithmetically and a scalar is
        // an indexed array with one element.
        None => {
            let index = arithmetic(&text, subscript, ctx).ok()?;
            if index == 0 {
                ctx.shell.get(name).map(<[u8]>::to_vec)
            } else {
                None
            }
        }
    }
}

/// Evaluate subscript text as arithmetic, through the runtime's evaluator.
fn arithmetic<S: Shell>(text: &[u8], source: &Word, ctx: &mut Ctx<'_, S>) -> Result<i64, Error> {
    let expr = crate::word::Arithmetic {
        source: text.to_vec(),
        span: source.span.clone(),
    };
    ctx.shell.arithmetic_value(&expr)
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
    // POSIX `set -u`: every operator that *consumes* a value must fail on an
    // unset parameter. The operators that supply their own value (`-`, `:-`,
    // `+`, `:+`, `=`, `:=`, `?`, `:?`) exist precisely to handle "unset", so
    // they are exempt; `$@`/`$*`/`$#` are never unset.
    if ctx.shell.nounset()
        && needs_set_value(&exp.op)
        && enumerable(&exp.name)
        && !ctx.is_set(&exp.name)
    {
        return Err(Error::new(
            ErrorKind::ParameterUnset {
                name: exp.name.display_bytes(),
                message: None,
            },
            span,
        ));
    }
    match &exp.op {
        ParamOp::Unsupported(op) => Err(Error::unsupported(*op, span)),
        ParamOp::Length => {
            let len = match &exp.name {
                ParamName::At | ParamName::Star => ctx.shell.positional().len(),
                ParamName::Elements { name, .. } => {
                    match ctx.shell.array(name) {
                        Some(array) => array.len(),
                        // A scalar is one element; an unset name is none.
                        None => usize::from(ctx.shell.get(name).is_some()),
                    }
                }
                ParamName::Element { name, subscript } => read_element(ctx, name, subscript)
                    .map_or(0, |value| char_count(&value, ctx.shell.utf8())),
                name => ctx
                    .value(name)
                    .map_or(0, |value| char_count(&value, ctx.shell.utf8())),
            };
            out.push(len.to_string().into_bytes(), quoted, !quoted);
            Ok(())
        }
        ParamOp::Indices => {
            let ParamName::Elements { name, star } = &exp.name else {
                // `${!x}` re-parses a variable's *value* as a name; that stays
                // out of scope (it is an eval by another name).
                return Err(Error::unsupported(UnsupportedOp::Indirect, span));
            };
            let indices = array_indices(ctx, name);
            emit_values(ctx, &indices, *star, quoted, out);
            Ok(())
        }
        ParamOp::Indirect => {
            let target = match &exp.name {
                ParamName::Element { name, subscript } => {
                    read_element(ctx, name, subscript).unwrap_or_default()
                }
                // `${!a}` on an array is `${!a[0]}`; on a plain variable it
                // would re-parse the value as a name, which stays out of
                // scope.
                ParamName::Var(name) if ctx.shell.array(name).is_some() => {
                    ctx.value(&exp.name).unwrap_or_default()
                }
                _ => return Err(Error::unsupported(UnsupportedOp::Indirect, span)),
            };
            substitute(&ParamName::Var(target), quoted, ctx, out);
            Ok(())
        }
        ParamOp::ArraySlice { offset, length } => {
            let (source, star) = match &exp.name {
                ParamName::Elements { name, star } => (array_values(ctx, name), *star),
                ParamName::At | ParamName::Star => (
                    ctx.shell.positional().to_vec(),
                    matches!(&exp.name, ParamName::Star),
                ),
                _ => return Err(Error::unsupported(UnsupportedOp::SubstringOfList, span)),
            };
            let bounds = slice_bounds(offset, length.as_ref(), ctx)?;
            let values = slice_list(&source, bounds, span)?;
            emit_values(ctx, &values, star, quoted, out);
            Ok(())
        }
        ParamOp::Substring { offset, length } => {
            if matches!(&exp.name, ParamName::At | ParamName::Star) {
                // bash slices the parameter *list* here; slicing a joined
                // string would be quietly wrong.
                let values: Vec<Vec<u8>> = ctx.shell.positional().to_vec();
                let values = slice_list(&values, (*offset, *length), span)?;
                let star = matches!(&exp.name, ParamName::Star);
                emit_values(ctx, &values, star, quoted, out);
                return Ok(());
            }
            if let ParamName::Element { name, subscript } = &exp.name {
                let value = read_element(ctx, name, subscript).unwrap_or_default();
                let slice = substring(&value, *offset, *length, ctx.shell.utf8(), span)?;
                out.push(slice.to_vec(), quoted, !quoted);
                return Ok(());
            }
            if let ParamName::Elements { name, star } = &exp.name {
                let values = array_values(ctx, name);
                let values = slice_list(&values, (*offset, *length), span)?;
                emit_values(ctx, &values, *star, quoted, out);
                return Ok(());
            }
            let Some(value) = ctx.value(&exp.name) else {
                out.push(Vec::new(), quoted, !quoted);
                return Ok(());
            };
            let slice = substring(&value, *offset, *length, ctx.shell.utf8(), span)?;
            out.push(slice.to_vec(), quoted, !quoted);
            Ok(())
        }
        ParamOp::RemovePrefix { pattern, longest } => {
            let prefix = true;
            strip_parameter(exp, pattern, *longest, prefix, quoted, ctx, out)
        }
        ParamOp::RemoveSuffix { pattern, longest } => {
            let prefix = false;
            strip_parameter(exp, pattern, *longest, prefix, quoted, ctx, out)
        }
        ParamOp::Replace {
            pattern,
            replacement,
            mode,
        } => {
            let pattern = expand_pattern(pattern, ctx)?;
            let replacement = expand_word_to_scalar(replacement, quoted, ctx)?;
            let flags = match_flags(ctx);
            let mode = *mode;
            let mut replace = |value: &mut Vec<u8>| {
                *value = replace_value(value, &pattern, &replacement, mode, flags);
            };
            map_parameter(ctx, &exp.name, quoted, out, &mut replace);
            Ok(())
        }
        ParamOp::Case { how, pattern } => {
            let pattern = match pattern {
                Some(word) => Some(expand_pattern(word, ctx)?),
                None => None,
            };
            let flags = match_flags(ctx);
            let how = *how;
            let mut convert = |value: &mut Vec<u8>| {
                convert_case(value, how, pattern.as_deref(), flags);
            };
            map_parameter(ctx, &exp.name, quoted, out, &mut convert);
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

/// Whether an unset `name` is an error under `set -u` once the operator is
/// known to read its value.
///
/// `$@`, `$*` and any `name[@]`/`name[*]` enumerate rather than read: bash
/// treats an empty or unset one as no fields, not as an unbound variable
/// (`set -u; unset a; echo "${a[@]}"` succeeds). A single element like
/// `${a[5]}` *is* an error, which is why this is separate from the operator
/// test.
fn enumerable(name: &ParamName) -> bool {
    !matches!(
        name,
        ParamName::At | ParamName::Star | ParamName::Count | ParamName::Elements { .. }
    )
}

/// Whether `op` reads the parameter's value, making an unset parameter an
/// error under `set -u`.
fn needs_set_value(op: &ParamOp) -> bool {
    !matches!(
        op,
        ParamOp::UseDefault { .. }
            | ParamOp::UseAlternate { .. }
            | ParamOp::AssignDefault { .. }
            | ParamOp::Error { .. }
    )
}

/// The elements of `name` in iteration order. A scalar is a one-element list;
/// an unset name is empty.
fn array_values<S: Shell>(ctx: &Ctx<'_, S>, name: &[u8]) -> Vec<Vec<u8>> {
    match ctx.shell.array(name) {
        Some(array) => array.values().into_iter().map(<[u8]>::to_vec).collect(),
        None => ctx
            .shell
            .get(name)
            .map(|value| vec![value.to_vec()])
            .unwrap_or_default(),
    }
}

/// The subscripts of `name`: decimal indices for an indexed array, keys for an
/// associative one, both in iteration order.
fn array_indices<S: Shell>(ctx: &Ctx<'_, S>, name: &[u8]) -> Vec<Vec<u8>> {
    match ctx.shell.array(name) {
        Some(array) => array.indices(),
        // A scalar has the single subscript 0; an unset name has none.
        None => {
            if ctx.shell.get(name).is_some() {
                vec![b"0".to_vec()]
            } else {
                Vec::new()
            }
        }
    }
}

/// Evaluate `${a[@]:offset:length}`'s bounds. Both are words, expanded and
/// then evaluated arithmetically through the runtime.
fn slice_bounds<S: Shell>(
    offset: &Word,
    length: Option<&Word>,
    ctx: &mut Ctx<'_, S>,
) -> Result<(i64, Option<i64>), Error> {
    let offset_text = expand_word_to_scalar(offset, false, ctx)?;
    let offset = arithmetic(&offset_text, offset, ctx)?;
    let length = match length {
        None => None,
        Some(word) => {
            let text = expand_word_to_scalar(word, false, ctx)?;
            Some(arithmetic(&text, word, ctx)?)
        }
    };
    Ok((offset, length))
}

/// `values[offset..offset+length]`, with bash's list slicing rules.
///
/// A negative offset counts from the end; an offset before the start or past
/// the end yields nothing; a negative length is an error bash reports as
/// `substring expression < 0`.
fn slice_list(
    values: &[Vec<u8>],
    (offset, length): (i64, Option<i64>),
    span: Span,
) -> Result<Vec<Vec<u8>>, Error> {
    let len = values.len() as i64;
    let start = if offset < 0 { len + offset } else { offset };
    if start < 0 || start > len {
        return Ok(Vec::new());
    }
    let end = match length {
        None => len,
        Some(length) if length >= 0 => (start + length).min(len),
        Some(length) => {
            return Err(Error::new(
                ErrorKind::SubstringRange { value: length },
                span,
            ));
        }
    };
    Ok(values[start as usize..end as usize].to_vec())
}

/// Substitute the value of `name` as an expansion result.
fn substitute<S: Shell>(name: &ParamName, quoted: bool, ctx: &mut Ctx<'_, S>, out: &mut Expanded) {
    match name {
        ParamName::At => emit_at(ctx, quoted, out),
        ParamName::Star => emit_star(ctx, quoted, out),
        ParamName::Elements { name, star } => {
            let values = array_values(ctx, name);
            emit_values(ctx, &values, *star, quoted, out);
        }
        ParamName::Element { name, subscript } => {
            let value = read_element(ctx, name, subscript).unwrap_or_default();
            out.push(value, quoted, !quoted);
        }
        _ => {
            let value = ctx.value(name).unwrap_or_default();
            out.push(value, quoted, !quoted);
        }
    }
}

/// Emit a list expansion (`$@`/`$*` or `${a[@]}`/`${a[*]}`).
///
/// `star` selects the `[*]` joining rule; quoted `[@]` produces one field per
/// value and *zero* fields for an empty list. [`Expanded::break_field`] is the
/// same field-boundary mechanism quoted `$@` uses, so arrays cannot drift from
/// positional parameters.
fn emit_values<S: Shell>(
    ctx: &Ctx<'_, S>,
    values: &[Vec<u8>],
    star: bool,
    quoted: bool,
    out: &mut Expanded,
) {
    let join = |separator: Option<u8>| {
        let mut joined = Vec::new();
        for (index, value) in values.iter().enumerate() {
            if index > 0 {
                if let Some(separator) = separator {
                    joined.push(separator);
                }
            }
            joined.extend_from_slice(value);
        }
        joined
    };

    if quoted {
        if star {
            // `"${a[*]}"` is one field even when the array is empty (bash:
            // `"$*"` behaves the same way).
            let separator = Ifs::from_vars(&*ctx.shell).join_separator();
            out.push(join(separator), true, false);
        } else {
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    out.break_field();
                }
                out.push(value.clone(), true, false);
            }
        }
        return;
    }

    match ctx.mode {
        // A scalar context cannot carry field boundaries; bash joins `$@`
        // with spaces in assignments, and arrays follow the equal rule.
        Mode::Assignment => out.push(join(Some(b' ')), false, true),
        Mode::Command => match Ifs::from_vars(&*ctx.shell).join_separator() {
            Some(separator) => out.push(join(Some(separator)), false, true),
            None => {
                // IFS is set but empty: no separator, so each value is its
                // own expansion result.
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        out.break_field();
                    }
                    out.push(value.clone(), false, true);
                }
            }
        },
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

/// Expand a replacement word as a scalar context: no field splitting, no
/// pathname expansion, and `$@` joined as an assignment value.
///
/// The chunks are kept rather than collapsed because pattern expansion needs
/// the per-chunk quoting flags; [`Expanded::into_scalar`] is the string view.
fn expand_word_to_chunks<S: Shell>(
    word: &Word,
    quoted: bool,
    ctx: &mut Ctx<'_, S>,
) -> Result<Expanded, Error> {
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
    Ok(expanded)
}

fn expand_word_to_scalar<S: Shell>(
    word: &Word,
    quoted: bool,
    ctx: &mut Ctx<'_, S>,
) -> Result<Vec<u8>, Error> {
    Ok(expand_word_to_chunks(word, quoted, ctx)?.into_scalar())
}

/// The flags every pattern operator in this module matches with.
///
/// [`Flags::utf8`] is forced off, not taken from the runtime: the shell's unit
/// policy is bytes (the C locale), the same policy `${#x}` counts with and what
/// the conformance harness (`LC_ALL=C`) sees. A pattern operator that measured
/// characters while `${#x}` counted bytes would disagree with itself.
fn match_flags<S: Shell>(ctx: &Ctx<'_, S>) -> Flags {
    Flags {
        utf8: false,
        ..ctx.shell.pattern_flags()
    }
}

/// Flags for the four strip operators.
///
/// bash 5.3.20 ignores `shopt -s nocasematch` for `${x#pat}`/`${x%pat}` even
/// though it honours it for `${x/pat/rep}`, so caseless matching is forced off
/// here rather than inherited from the runtime's options.
fn strip_flags<S: Shell>(ctx: &Ctx<'_, S>) -> Flags {
    Flags {
        nocasematch: false,
        ..match_flags(ctx)
    }
}

/// `${x#pat}` and `${x%pat}`, including the array and `$@` forms.
#[allow(clippy::too_many_arguments)]
fn strip_parameter<S: Shell>(
    exp: &ParamExp,
    pattern: &Word,
    longest: bool,
    prefix: bool,
    quoted: bool,
    ctx: &mut Ctx<'_, S>,
    out: &mut Expanded,
) -> Result<(), Error> {
    let pattern = expand_pattern(pattern, ctx)?;
    let flags = strip_flags(ctx);
    let mut strip = |value: &mut Vec<u8>| strip_value(value, &pattern, longest, prefix, flags);
    map_parameter(ctx, &exp.name, quoted, out, &mut strip);
    Ok(())
}

/// Apply `transform` to every value the expansion names, then emit the result.
///
/// bash vectorises these operators over `$@`, `$*` and `name[@]`/`name[*]`
/// (`${a[@]#x}`, `${@%a}`, `${a[@]/x/y}`): each element is transformed and the
/// usual list rules decide the fields, so `"${a[@]#x}"` keeps one field per
/// element while `"${a[*]#x}"` joins them. A scalar is one value.
fn map_parameter<S: Shell>(
    ctx: &mut Ctx<'_, S>,
    name: &ParamName,
    quoted: bool,
    out: &mut Expanded,
    transform: &mut impl FnMut(&mut Vec<u8>),
) {
    match name {
        ParamName::At | ParamName::Star => {
            let mut values: Vec<Vec<u8>> = ctx.shell.positional().to_vec();
            for value in &mut values {
                transform(value);
            }
            emit_values(ctx, &values, matches!(name, ParamName::Star), quoted, out);
        }
        ParamName::Elements { name, star } => {
            let mut values = array_values(ctx, name);
            for value in &mut values {
                transform(value);
            }
            emit_values(ctx, &values, *star, quoted, out);
        }
        ParamName::Element { name, subscript } => {
            let mut value = read_element(ctx, name, subscript).unwrap_or_default();
            transform(&mut value);
            out.push(value, quoted, !quoted);
        }
        _ => {
            let mut value = ctx.value(name).unwrap_or_default();
            transform(&mut value);
            out.push(value, quoted, !quoted);
        }
    }
}

/// `${x#pat}` / `${x%pat}` on one value.
///
/// An empty pattern leaves the value alone, which is bash's rule for `${x##}`
/// and `${x##""}`: the four operators never "strip everything" because there
/// was nothing to match.
fn strip_value(value: &mut Vec<u8>, pattern: &[u8], longest: bool, prefix: bool, flags: Flags) {
    if pattern.is_empty() {
        return;
    }
    let pattern = bsh_pattern::Pattern::new(pattern, flags);
    if prefix {
        if let Some(end) = pattern.prefix_end(value, longest) {
            value.drain(..end);
        }
    } else if let Some(start) = pattern.suffix_start(value, longest) {
        value.truncate(start);
    }
}

/// Expand a pattern word to the bytes handed to `bsh-pattern`.
///
/// `bsh-pattern` deliberately has no quote mask, so quoted bytes become
/// backslash escapes: a quoted `*` reaches the matcher as `\*` and is literal,
/// and a quoted backslash becomes `\\`. Unquoted bytes — including unquoted
/// expansions — stay active, which is exactly bash's split:
/// `p='a*'; ${x##$p}` matches a pattern while `${x##"$p"}` compares text.
///
/// The enclosing double quotes do *not* quote the pattern: bash expands the
/// argument of `"${x##$p}"` exactly like the unquoted form, and only quoting
/// written inside the pattern counts. That is why this expands with
/// `quoted: false` even when the expansion itself is inside `"…"`.
fn expand_pattern<S: Shell>(word: &Word, ctx: &mut Ctx<'_, S>) -> Result<Vec<u8>, Error> {
    let expanded = expand_word_to_chunks(word, false, ctx)?;
    let mut out = Vec::new();
    let mut first = true;
    for chunk in expanded.chunks {
        if chunk.break_before && !first {
            // A `$@` field boundary cannot survive in a pattern; a space is
            // what the scalar forms of `$@` produce too.
            out.push(b' ');
        }
        first = false;
        if chunk.quoted {
            for byte in chunk.bytes {
                out.push(b'\\');
                out.push(byte);
            }
        } else {
            out.extend_from_slice(&chunk.bytes);
        }
    }
    Ok(out)
}

/// One `${x/pat/rep}` pass over one value.
///
/// bash scans left to right and takes the **longest** match at each position.
/// An empty match is never replaced — the byte is copied and the scan advances
/// one byte — which is why `${x//""/X}` and `${x/x*/X}` leave the value alone.
/// The empty *pattern* is the one exception, and only for the anchored forms:
/// `${x/#/X}` prepends and `${x/%/X}` appends.
fn replace_value(
    text: &[u8],
    pattern: &[u8],
    replacement: &[u8],
    mode: ReplaceMode,
    flags: Flags,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + replacement.len());
    if pattern.is_empty() {
        match mode {
            ReplaceMode::Prefix => {
                out.extend_from_slice(replacement);
                out.extend_from_slice(text);
            }
            ReplaceMode::Suffix => {
                out.extend_from_slice(text);
                out.extend_from_slice(replacement);
            }
            ReplaceMode::First | ReplaceMode::All => out.extend_from_slice(text),
        }
        return out;
    }
    // Compile once per value: the scanning loop asks for a prefix match at
    // every byte, and recompiling there would cost a parse per position.
    let pattern = bsh_pattern::Pattern::new(pattern, flags);
    match mode {
        ReplaceMode::Prefix => match pattern.prefix_end(text, true) {
            // An empty match is not a replacement, as in the unanchored loop.
            Some(end) if end > 0 => {
                out.extend_from_slice(replacement);
                out.extend_from_slice(&text[end..]);
            }
            _ => out.extend_from_slice(text),
        },
        ReplaceMode::Suffix => match pattern.suffix_start(text, true) {
            Some(start) if start < text.len() => {
                out.extend_from_slice(&text[..start]);
                out.extend_from_slice(replacement);
            }
            _ => out.extend_from_slice(text),
        },
        ReplaceMode::First | ReplaceMode::All => {
            let global = mode == ReplaceMode::All;
            let mut index = 0;
            while index < text.len() {
                match pattern.prefix_end(&text[index..], true) {
                    Some(len) if len > 0 => {
                        out.extend_from_slice(replacement);
                        index += len;
                        if !global {
                            out.extend_from_slice(&text[index..]);
                            return out;
                        }
                    }
                    _ => {
                        out.push(text[index]);
                        index += 1;
                    }
                }
            }
        }
    }
    out
}

/// `${x^^pat}` and friends: ASCII case conversion of the matching bytes.
///
/// bash matches the pattern against **one character at a time**, not against
/// the rest of the string (verified against 5.3.20: `${x^^*[bB]}` upper-cases
/// only the `b`s), so a pattern can never consume more than the byte it is
/// tested on. With byte units a multi-byte character is several characters,
/// and no byte above 0x7f has an ASCII case mapping, so non-ASCII text is
/// never changed — `μ` stays `μ` exactly as bash's `LC_ALL=C` leaves it.
fn convert_case(value: &mut [u8], how: CaseHow, pattern: Option<&[u8]>, flags: Flags) {
    let compiled = pattern.map(|pattern| bsh_pattern::Pattern::new(pattern, flags));
    for (index, byte) in value.iter_mut().enumerate() {
        if index > 0 && how.first_only() {
            break;
        }
        let matched = match &compiled {
            None => true,
            Some(pattern) => pattern.matches(&[*byte]),
        };
        if matched {
            *byte = if how.to_upper() {
                byte.to_ascii_uppercase()
            } else {
                byte.to_ascii_lowercase()
            };
        }
    }
}

/// `${x:offset:length}` on a byte string.
///
/// Byte units, i.e. `LC_ALL=C` semantics. bash reports a negative *length*
/// that reaches past the start as `substring expression < 0`, and an offset
/// that reaches past the start simply yields nothing; both are reproduced.
fn substring(
    value: &[u8],
    offset: i64,
    length: Option<i64>,
    utf8: bool,
    span: Span,
) -> Result<&[u8], Error> {
    // In a UTF-8 locale the offset and length count characters; in the C
    // locale they count bytes. Both paths share the arithmetic, which is why
    // the only difference is the unit table.
    let starts = if utf8 {
        unit_starts(value)
    } else {
        (0..=value.len()).collect()
    };
    let len = starts.len() as i64 - 1;
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
    Ok(&value[starts[start as usize]..starts[end as usize]])
}

/// Byte offset of every character boundary, plus `value.len()`.
fn unit_starts(value: &[u8]) -> Vec<usize> {
    let mut starts = Vec::with_capacity(value.len() + 1);
    let mut index = 0;
    while index < value.len() {
        starts.push(index);
        index += bsh_pattern::char_len(value, index);
    }
    starts.push(value.len());
    starts
}

/// Characters in `value`: code points in a UTF-8 locale, bytes otherwise.
fn char_count(value: &[u8], utf8: bool) -> usize {
    if !utf8 {
        return value.len();
    }
    let mut count = 0;
    let mut index = 0;
    while index < value.len() {
        index += bsh_pattern::char_len(value, index);
        count += 1;
    }
    count
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
    let extglob = shell.pattern_flags().extglob;
    let mut out = Vec::with_capacity(fields.len());
    for field in fields {
        if !field.has_active_glob_with(extglob) {
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
    let word = match mode {
        Mode::Command => expand_tilde(word, shell)?,
        Mode::Assignment => expand_assignment_tilde(word, shell)?,
    };
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
