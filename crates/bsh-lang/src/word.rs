//! The word model: literal pieces, quoted groups and expansion nodes.
//!
//! A [`Word`] is a flat sequence of [`Part`]s plus a source span. Quoting is
//! carried by the parts, and the front end is expected to have already
//! interpreted the source syntax: a `'…'` literal holds its bytes without the
//! quotes, a `"…"` group is a [`Part::DoubleQ`] around its parts, and a
//! backslash-escaped byte is a [`LitKind::Quoted`] literal.
//!
//! # Why quotedness cannot be a property of the word
//!
//! DESIGN.md §4.2 makes this a type rather than a comment, because a per-word
//! `quoted: bool` gets all three of these wrong:
//!
//! * `"$@"` with two positional parameters must produce *two* fields, while
//!   the rest of the word is one piece of quoted text glued to the first and
//!   last of them (`x"$@"y` → `xa`, `by`). A word-level flag cannot express
//!   "these parts are quoted, but that expansion still splits the word".
//! * `${x:-"$@"}` has an unquoted outer expansion whose replacement word
//!   contains an explicitly quoted `$@`. Which one is quoted decides whether
//!   the parameters are field-split; the outer word is not quoted at all.
//! * `$*` means different things quoted and unquoted (`a b c` versus three
//!   fields), and inside a larger word only the `$*` part carries that
//!   meaning: `a"$*"b` is one field, `a$*b` may be several.
//!
//! # Two independent properties
//!
//! Expansion needs two flags per piece of text, not one:
//!
//! * **quoted** — the bytes are inert for pathname expansion (`*` is a star).
//! * **an expansion result** — the bytes are subject to field splitting.
//!
//! They are not the same thing: *literal source text* is not field-split even
//! when it is unquoted. `IFS=:; printf '<%s>' a:b` prints one field, because
//! POSIX XCU 2.6.5 applies field splitting only to "the results of parameter
//! expansion, command substitution, and arithmetic expansion that do not
//! occur within double quotes". The same colon arriving from `$x` *is* split.
//! The literal is still glob-active, so `a*` matches files.
//!
//! [`LitKind`] records which kind of literal the front end produced, and the
//! expansion pipeline derives the two flags from it ([`Part::Lit`] is never
//! splittable; [`LitKind::Quoted`] is not glob-active either). Inside the
//! replacement word of a parameter expansion the unquoted literal text *is*
//! part of the expansion result and therefore splittable — see
//! [`crate::expand_parameters`].

use bsh_diag::Span;

/// A shell word before expansion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Word {
    /// The parts, in source order.
    pub parts: Vec<Part>,
    /// Where the word came from, for diagnostics.
    pub span: Span,
}

impl Word {
    /// A word with an empty span.
    pub fn new(parts: Vec<Part>) -> Self {
        Self { parts, span: 0..0 }
    }

    /// An empty word: expands to nothing at all.
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// A single unquoted literal word.
    pub fn lit(bytes: impl Into<Vec<u8>>) -> Self {
        Self::new(vec![Part::lit(bytes)])
    }

    /// Attach a source span.
    #[must_use]
    pub fn with_span(mut self, span: Span) -> Self {
        self.span = span;
        self
    }

    /// Append a part.
    pub fn push(&mut self, part: Part) {
        self.parts.push(part);
    }

    /// Whether the word has no parts (and therefore expands to nothing).
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }
}

impl Default for Word {
    fn default() -> Self {
        Self::empty()
    }
}

/// One piece of a word.
///
/// The variant list mirrors DESIGN.md §4.2. `ProcSub` is included because the
/// front end must be able to represent `<(...)`; expansion reports it as
/// unsupported until the runtime supplies a `/dev/fd/N` provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    /// Literal bytes, with the quoting that produced them.
    Lit(Lit),
    /// A `"…"` group: every part inside is quoted (DESIGN.md §4.2).
    DoubleQ(DoubleQ),
    /// A parameter expansion, `${x}` or `$x`.
    Param(ParamExp),
    /// A command substitution, `$(…)` or `` `…` ``.
    CmdSub(CommandSub),
    /// An arithmetic expansion, `$((…))`.
    Arith(Arithmetic),
    /// A bash process substitution, `<(…)` or `>(…)`.
    ProcSub(ProcessSub),
    /// A word-initial tilde. The front end decides where tilde expansion is
    /// permitted; the pipeline only expands the `Tilde` parts it is given
    /// (plus a leading `~` in an unquoted literal, as a convenience).
    Tilde(TildeSpec),
}

impl Part {
    /// Unquoted literal bytes.
    pub fn lit(bytes: impl Into<Vec<u8>>) -> Self {
        Self::Lit(Lit::unquoted(bytes))
    }

    /// Quoted literal bytes (a backslash escape, or the result of an
    /// expansion): inert for both field splitting and pathname expansion.
    pub fn quoted(bytes: impl Into<Vec<u8>>) -> Self {
        Self::Lit(Lit::quoted(bytes))
    }

    /// A `'…'` literal.
    pub fn single_quoted(bytes: impl Into<Vec<u8>>) -> Self {
        Self::Lit(Lit::single_quoted(bytes))
    }

    /// A `$'…'` literal.
    pub fn ansi_c(bytes: impl Into<Vec<u8>>) -> Self {
        Self::Lit(Lit::ansi_c(bytes))
    }

    /// A `"…"` group around `parts`.
    pub fn double_quoted(parts: Vec<Part>) -> Self {
        Self::DoubleQ(DoubleQ { parts })
    }
}

/// Literal bytes together with the source quoting that produced them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lit {
    /// Which quoting form produced the bytes.
    pub kind: LitKind,
    /// The bytes, with source quoting already removed.
    pub bytes: Vec<u8>,
}

impl Lit {
    /// Construct a literal.
    pub fn new(kind: LitKind, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            kind,
            bytes: bytes.into(),
        }
    }

    /// Unquoted source text: glob-active, but never field-split.
    pub fn unquoted(bytes: impl Into<Vec<u8>>) -> Self {
        Self::new(LitKind::Unquoted, bytes)
    }

    /// Text that is quoted although it was not written with quotes, e.g. a
    /// backslash-escaped byte or a tilde expansion result.
    pub fn quoted(bytes: impl Into<Vec<u8>>) -> Self {
        Self::new(LitKind::Quoted, bytes)
    }

    /// A `'…'` literal.
    pub fn single_quoted(bytes: impl Into<Vec<u8>>) -> Self {
        Self::new(LitKind::SingleQuoted, bytes)
    }

    /// A `$'…'` literal, with escapes already decoded.
    pub fn ansi_c(bytes: impl Into<Vec<u8>>) -> Self {
        Self::new(LitKind::AnsiC, bytes)
    }
}

/// How a [`Lit`] was quoted in the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LitKind {
    /// Source text with no quoting: glob metacharacters are active; it is not
    /// an expansion result, so it is never field-split (POSIX XCU 2.6.5).
    Unquoted,
    /// Quoted by a backslash or produced by tilde expansion: inert for field
    /// splitting and for pathname expansion.
    Quoted,
    /// `'…'`: fully literal.
    SingleQuoted,
    /// `$'…'`: ANSI-C quoted, escapes already decoded.
    AnsiC,
}

impl LitKind {
    /// Whether this literal is inert for field splitting and pattern matching.
    pub fn is_quoted(self) -> bool {
        !matches!(self, LitKind::Unquoted)
    }
}

/// A `"…"` group.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DoubleQ {
    /// The parts inside the quotes.
    pub parts: Vec<Part>,
}

/// A parameter expansion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamExp {
    /// Which parameter is expanded.
    pub name: ParamName,
    /// Which operator, if any, applies.
    pub op: ParamOp,
    /// Span of the whole `${…}` or `$x`, for diagnostics.
    pub span: Span,
}

impl ParamExp {
    /// Construct a parameter expansion.
    pub fn new(name: ParamName, op: ParamOp, span: Span) -> Self {
        Self { name, op, span }
    }

    /// A plain expansion with no operator.
    pub fn value(name: ParamName, span: Span) -> Self {
        Self::new(name, ParamOp::Value, span)
    }
}

/// The parameter named inside `${…}` or after `$`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParamName {
    /// A variable name, looked up with [`crate::Vars::get`]. Special
    /// parameters other than `#`, `@` and `*` (`?`, `-`, `$`, `!`, `0`)
    /// also arrive here, because the runtime owns their values.
    Var(Vec<u8>),
    /// `$1`, `$2`, … (`$0` is passed as `Var(b"0")`).
    Positional(usize),
    /// `$@`.
    At,
    /// `$*`.
    Star,
    /// `$#`, computed from [`crate::Vars::positional`].
    Count,
}

impl ParamName {
    /// A variable name.
    pub fn var(name: impl Into<Vec<u8>>) -> Self {
        Self::Var(name.into())
    }

    /// A positional parameter. `n` is one-based; zero is not a positional
    /// parameter (`$0` is [`ParamName::Var`] holding `0`).
    pub fn positional(n: usize) -> Self {
        Self::Positional(n)
    }

    /// The spelling used in diagnostics, without the leading `$`.
    pub fn display_bytes(&self) -> Vec<u8> {
        match self {
            ParamName::Var(name) => name.clone(),
            ParamName::Positional(n) => n.to_string().into_bytes(),
            ParamName::At => vec![b'@'],
            ParamName::Star => vec![b'*'],
            ParamName::Count => vec![b'#'],
        }
    }
}

/// The operator applied to a parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParamOp {
    /// No operator: substitute the value.
    Value,
    /// `${x:-word}` (`colon`) or `${x-word}`.
    UseDefault {
        /// The replacement word.
        word: Word,
        /// Whether a null value also triggers the replacement.
        colon: bool,
    },
    /// `${x:+word}` (`colon`) or `${x+word}`: substitute `word` only when the
    /// parameter is set (and, with `colon`, non-null).
    UseAlternate {
        /// The replacement word.
        word: Word,
        /// Whether a null value suppresses the replacement.
        colon: bool,
    },
    /// `${x:=word}` (`colon`) or `${x=word}`: assign when the parameter is
    /// unset (or null) and substitute the assigned value.
    AssignDefault {
        /// The replacement word.
        word: Word,
        /// Whether a null value also triggers the assignment.
        colon: bool,
    },
    /// `${x:?word}` (`colon`) or `${x?word}`: fail when the parameter is
    /// unset (or null).
    Error {
        /// The message word; `None` for the shell's default message.
        word: Option<Word>,
        /// Whether a null value also triggers the error.
        colon: bool,
    },
    /// `${#x}`: the length in bytes (POSIX counts characters; ßhell is
    /// byte-oriented, i.e. `LC_ALL=C` semantics — see the crate docs).
    Length,
    /// `${x:offset}` or `${x:offset:length}`. Negative offsets count from the
    /// end; units are bytes.
    Substring {
        /// The offset; negative counts from the end of the value.
        offset: i64,
        /// The length; negative means "up to this many bytes from the end".
        length: Option<i64>,
    },
    /// `${x^^}`: ASCII upper-case the whole value.
    Upper,
    /// `${x,,}`: ASCII lower-case the whole value.
    Lower,
    /// `${x^}`: ASCII upper-case the first byte.
    UpperFirst,
    /// `${x,}`: ASCII lower-case the first byte.
    LowerFirst,
    /// An operator this crate deliberately does not implement.
    Unsupported(UnsupportedOp),
}

impl ParamOp {
    /// The replacement word, if this operator has one.
    pub fn word(&self) -> Option<&Word> {
        match self {
            ParamOp::UseDefault { word, .. }
            | ParamOp::UseAlternate { word, .. }
            | ParamOp::AssignDefault { word, .. } => Some(word),
            ParamOp::Error { word, .. } => word.as_ref(),
            _ => None,
        }
    }

    /// Mutable access to the replacement word, used by tilde expansion.
    pub fn word_mut(&mut self) -> Option<&mut Word> {
        match self {
            ParamOp::UseDefault { word, .. }
            | ParamOp::UseAlternate { word, .. }
            | ParamOp::AssignDefault { word, .. } => Some(word),
            ParamOp::Error { word, .. } => word.as_mut(),
            _ => None,
        }
    }
}

/// Bash operators that need pattern matching or re-parsing, left out of scope
/// on purpose: expansion returns [`crate::ErrorKind::Unsupported`] rather than
/// doing something subtly wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsupportedOp {
    /// `${x#pat}`, `${x##pat}`.
    RemovePrefix,
    /// `${x%pat}`, `${x%%pat}`.
    RemoveSuffix,
    /// `${x/pat/rep}` and friends.
    Replace,
    /// `${!x}`, `${!prefix*}`: the argument is a variable *name*, which would
    /// mean evaluating data as code.
    Indirect,
    /// `${x@Q}` and the other transformations.
    Transform,
    /// `${x^^pat}`, `${x,,pat}`: case conversion driven by a pattern.
    CasePattern,
    /// `${@^^}` / `${*^^}`: case conversion of a parameter list.
    CaseOfList,
    /// `${@:offset:length}`: substring of a parameter list.
    SubstringOfList,
    /// `${x[subscript]}`: arrays are not part of this crate yet.
    ArraySubscript,
    /// `<(…)`, `>(…)`: the runtime has to supply the file descriptor.
    ProcessSubstitution,
    /// An offset or length that is a general arithmetic expression.
    ArithmeticSubstring,
    /// An operator this crate's parser could not classify.
    Other,
}

impl UnsupportedOp {
    /// A short, stable description for diagnostics.
    pub fn description(self) -> &'static str {
        match self {
            UnsupportedOp::RemovePrefix => "prefix removal (`${x#pat}`)",
            UnsupportedOp::RemoveSuffix => "suffix removal (`${x%pat}`)",
            UnsupportedOp::Replace => "pattern substitution (`${x/pat/rep}`)",
            UnsupportedOp::Indirect => "indirect expansion (`${!x}`)",
            UnsupportedOp::Transform => "parameter transformation (`${x@Q}`)",
            UnsupportedOp::CasePattern => "pattern case conversion",
            UnsupportedOp::CaseOfList => "case conversion of `$@`/`$*`",
            UnsupportedOp::SubstringOfList => "substring of `$@`/`$*`",
            UnsupportedOp::ArraySubscript => "array subscript",
            UnsupportedOp::ProcessSubstitution => "process substitution",
            UnsupportedOp::ArithmeticSubstring => "arithmetic offset in a substring",
            UnsupportedOp::Other => "this expansion form",
        }
    }
}

/// A command substitution.
///
/// `source` is the *inner* command text, already unescaped for the backtick
/// form. The front end owns the syntax; the runtime executes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSub {
    /// The command text between the delimiters.
    pub source: Vec<u8>,
    /// Which syntax produced it.
    pub style: CmdSubStyle,
    /// Where it came from.
    pub span: Span,
}

impl CommandSub {
    /// A `$(…)` substitution.
    pub fn dollar_paren(source: impl Into<Vec<u8>>) -> Self {
        Self {
            source: source.into(),
            style: CmdSubStyle::DollarParen,
            span: 0..0,
        }
    }

    /// A `` `…` `` substitution.
    pub fn backtick(source: impl Into<Vec<u8>>) -> Self {
        Self {
            source: source.into(),
            style: CmdSubStyle::Backtick,
            span: 0..0,
        }
    }
}

/// Which syntax produced a command substitution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmdSubStyle {
    /// `$(…)`.
    DollarParen,
    /// `` `…` ``.
    Backtick,
}

/// An arithmetic expansion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arithmetic {
    /// The expression between `$((` and `))`.
    pub source: Vec<u8>,
    /// Where it came from.
    pub span: Span,
}

impl Arithmetic {
    /// Construct an arithmetic expansion.
    pub fn new(source: impl Into<Vec<u8>>) -> Self {
        Self {
            source: source.into(),
            span: 0..0,
        }
    }
}

/// A bash process substitution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessSub {
    /// The command text between the delimiters.
    pub source: Vec<u8>,
    /// `<(` or `>(`.
    pub dir: ProcSubDir,
    /// Where it came from.
    pub span: Span,
}

/// The direction of a process substitution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcSubDir {
    /// `<(…)`: the command writes, the word is a readable path.
    Input,
    /// `>(…)`: the word is a writable path.
    Output,
}

/// A tilde prefix, `~` or `~user`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TildeSpec {
    /// The login name, empty for a bare `~`; `+` and `-` are `$PWD`/`$OLDPWD`.
    pub user: Vec<u8>,
}

impl TildeSpec {
    /// A tilde for `user` (empty means the current user's home directory).
    pub fn new(user: impl Into<Vec<u8>>) -> Self {
        Self { user: user.into() }
    }

    /// The source spelling of the prefix, without the expansion.
    pub fn source_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.user.len() + 1);
        out.push(b'~');
        out.extend_from_slice(&self.user);
        out
    }
}
