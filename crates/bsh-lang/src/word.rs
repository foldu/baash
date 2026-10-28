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

    /// Whether the word's first byte is `byte` and arrived unquoted.
    ///
    /// `${x//pat/rep}` uses this for bash's rule that a global substitution's
    /// pattern may start with `/` (`${x////c}` replaces `/` with `c`), which a
    /// quoted `/` never triggers.
    pub fn starts_with_unquoted(&self, byte: u8) -> bool {
        matches!(self.parts.first(), Some(Part::Lit(lit))
            if lit.kind == LitKind::Unquoted && lit.bytes.first() == Some(&byte))
    }

    /// This word without a leading unquoted `byte`, when it starts with one.
    ///
    /// `${x/#pat/rep}` and `${x/%pat/rep}` are told apart from a pattern that
    /// begins with a literal `#`/`%` this way: only an *unquoted* one is an
    /// anchor.
    #[must_use]
    pub fn strip_leading_unquoted(&self, byte: u8) -> Option<Word> {
        let Some(Part::Lit(lit)) = self.parts.first() else {
            return None;
        };
        if lit.kind != LitKind::Unquoted || lit.bytes.first() != Some(&byte) {
            return None;
        }
        let mut parts = self.parts.clone();
        if lit.bytes.len() == 1 {
            parts.remove(0);
        } else {
            parts[0] = Part::Lit(Lit::new(lit.kind, lit.bytes[1..].to_vec()));
        }
        Some(Word {
            parts,
            span: self.span.clone(),
        })
    }

    /// Split at the first unquoted `sep` byte, dropping the separator.
    ///
    /// Only literal text is searched: a `sep` inside `'…'`, `"…"`, a
    /// backslash escape or a nested `$…` construct is invisible, which is what
    /// bash's `${x/pat/rep}` scanner does (`${v/${w/a/b}/X}` has no separator
    /// inside the nested expansion). `skip_first` ignores a separator at the
    /// word's very first byte — bash's rule for the global form, where
    /// `${x////c}` is "replace `/` with `c`".
    ///
    /// Both halves keep this word's span; the parts inside keep theirs, so a
    /// diagnostic from a nested expansion still points at the nested text.
    pub fn split_unquoted(&self, sep: u8, skip_first: bool) -> Option<(Word, Word)> {
        let mut at_start = true;
        for (index, part) in self.parts.iter().enumerate() {
            let Part::Lit(lit) = part else {
                at_start = false;
                continue;
            };
            if lit.kind != LitKind::Unquoted {
                at_start = false;
                continue;
            }
            let from = usize::from(skip_first && at_start);
            at_start = false;
            let Some(tail) = lit.bytes.get(from..) else {
                continue;
            };
            let Some(offset) = tail.iter().position(|&b| b == sep) else {
                continue;
            };
            let at = from + offset;
            let mut before: Vec<Part> = self.parts[..index].to_vec();
            if at > 0 {
                before.push(Part::Lit(Lit::new(lit.kind, lit.bytes[..at].to_vec())));
            }
            let mut after: Vec<Part> = Vec::new();
            if at + 1 < lit.bytes.len() {
                after.push(Part::Lit(Lit::new(lit.kind, lit.bytes[at + 1..].to_vec())));
            }
            after.extend_from_slice(&self.parts[index + 1..]);
            return Some((
                Word {
                    parts: before,
                    span: self.span.clone(),
                },
                Word {
                    parts: after,
                    span: self.span.clone(),
                },
            ));
        }
        None
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
    ///
    /// For an array, `${a}` is bash's `${a[0]}` (see [`crate::Vars::array`]),
    /// so the bare name still addresses the array's element zero.
    Var(Vec<u8>),
    /// `${a[subscript]}`: one element. The subscript is a word because it is
    /// expanded before it is used, and for an indexed array it is then
    /// evaluated as an arithmetic expression (`${a[i+1]}`). For an
    /// associative array the expanded bytes are the key, unchanged.
    Element {
        /// The array's variable name.
        name: Vec<u8>,
        /// The subscript as written, without the brackets.
        subscript: Word,
    },
    /// `${a[@]}` (`star = false`) or `${a[*]}` (`star = true`): every element.
    ///
    /// `[@]` and `[*]` differ only when the expansion is quoted: `[@]` yields
    /// one field per element (none when the array is empty), `[*]` joins the
    /// elements with the first character of `IFS`.
    Elements {
        /// The array's variable name.
        name: Vec<u8>,
        /// `true` for `[*]`, `false` for `[@]`.
        star: bool,
    },
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

    /// `${name[subscript]}`.
    pub fn element(name: impl Into<Vec<u8>>, subscript: Word) -> Self {
        Self::Element {
            name: name.into(),
            subscript,
        }
    }

    /// `${name[@]}` or `${name[*]}`.
    pub fn elements(name: impl Into<Vec<u8>>, star: bool) -> Self {
        Self::Elements {
            name: name.into(),
            star,
        }
    }

    /// A positional parameter. `n` is one-based; zero is not a positional
    /// parameter (`$0` is [`ParamName::Var`] holding `0`).
    pub fn positional(n: usize) -> Self {
        Self::Positional(n)
    }

    /// The variable name this expansion addresses, if it addresses one.
    ///
    /// A plain parameter is its own name; an array element or element list
    /// names the array. Positional and special parameters return `None`
    /// because they are not assignable variables.
    pub fn variable_name(&self) -> Option<&[u8]> {
        match self {
            ParamName::Var(name) => Some(name),
            ParamName::Element { name, .. } | ParamName::Elements { name, .. } => Some(name),
            _ => None,
        }
    }

    /// The spelling used in diagnostics, without the leading `$`.
    pub fn display_bytes(&self) -> Vec<u8> {
        match self {
            ParamName::Var(name) => name.clone(),
            ParamName::Element { name, subscript } => {
                let mut out = name.clone();
                out.push(b'[');
                out.extend_from_slice(&subscript_text(subscript));
                out.push(b']');
                out
            }
            ParamName::Elements { name, star } => {
                let mut out = name.clone();
                out.extend_from_slice(if *star { b"[*]" } else { b"[@]" });
                out
            }
            ParamName::Positional(n) => n.to_string().into_bytes(),
            ParamName::At => vec![b'@'],
            ParamName::Star => vec![b'*'],
            ParamName::Count => vec![b'#'],
        }
    }
}

/// Best-effort source spelling of a subscript, for messages only. Quoting is
/// already interpreted, so the bytes are the literal text the front end saw.
fn subscript_text(word: &Word) -> Vec<u8> {
    let mut out = Vec::new();
    for part in &word.parts {
        match part {
            Part::Lit(lit) => out.extend_from_slice(&lit.bytes),
            Part::Param(exp) => {
                out.push(b'$');
                out.extend_from_slice(&exp.name.display_bytes());
            }
            _ => out.push(b'?'),
        }
    }
    out
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
    /// `${a[@]:offset:length}` and `${a[*]:offset:length}`: a contiguous
    /// slice of an array's element list (after holes are skipped).
    ///
    /// The bounds are words rather than integers because bash evaluates them
    /// arithmetically (`${a[@]:i-4:2}`), and the subscript-arithmetic seam
    /// already exists; expansion expands the word and evaluates the result.
    ArraySlice {
        /// The offset expression; negative counts from the end of the list.
        offset: Word,
        /// The length expression; negative is an error, as bash reports.
        length: Option<Word>,
    },
    /// `${x#pat}` (`longest` false) or `${x##pat}` (`longest` true): remove
    /// the shortest or longest matching prefix.
    ///
    /// The pattern is a [`Word`] because its quoting decides which
    /// metacharacters are literal: `${x#"$y"}` compares `$y` literally while
    /// `${x#$y}` matches it as a pattern.
    RemovePrefix {
        /// The pattern text, quoting included.
        pattern: Word,
        /// `true` for `##`: strip the longest match.
        longest: bool,
    },
    /// `${x%pat}` (`longest` false) or `${x%%pat}` (`longest` true): remove
    /// the shortest or longest matching suffix.
    RemoveSuffix {
        /// The pattern text, quoting included.
        pattern: Word,
        /// `true` for `%%`: strip the longest match.
        longest: bool,
    },
    /// `${x/pat/rep}`, `${x//pat/rep}`, `${x/#pat/rep}` or `${x/%pat/rep}`:
    /// replace a pattern match.
    Replace {
        /// The pattern text, quoting included.
        pattern: Word,
        /// The replacement; for `/[#%]` forms it is the text after the second
        /// unquoted `/`, and it is empty when that separator is absent.
        replacement: Word,
        /// Which occurrences are replaced.
        mode: ReplaceMode,
    },
    /// `${x^pat}`, `${x^^pat}`, `${x,pat}`, `${x,,pat}`: ASCII case conversion
    /// of the characters that match `pattern`.
    ///
    /// "Character" is a byte: ßhell matches in the C locale, the same unit
    /// `${#x}` counts and `bsh-pattern` uses with `utf8: false`. A non-ASCII
    /// byte is never converted, because `[A-Z]`/`[a-z]` are the only case
    /// mappings the shell applies (bash in `LC_ALL=C` does the same).
    Case {
        /// The direction and reach of the conversion.
        how: CaseHow,
        /// The pattern matched against each character; `None` when the source
        /// wrote none, which matches every character (`^`/`,` still apply to
        /// the first one only).
        pattern: Option<Word>,
    },
    /// `${!a[@]}` / `${!a[*]}`: the array's indices (or keys), in the order
    /// [`crate::Vars::array`] reports them. `${!x}` on a plain variable stays
    /// [`ParamOp::Unsupported`] with [`UnsupportedOp::Indirect`].
    Indices,
    /// `${!a[subscript]}`: the element's value is a variable *name*, which is
    /// then expanded (`v=hello; a=(v); ${!a[0]}` is `hello`).
    ///
    /// Only the element form is implemented: `${!x}` would re-parse arbitrary
    /// data as a name, which stays [`ParamOp::Unsupported`].
    Indirect,
    /// `${x@P}`: expand the value as a prompt string.
    ///
    /// The front end classifies every `@` transformation generically as
    /// [`ParamOp::Unsupported`] with [`UnsupportedOp::Transform`], because the
    /// operator's letter is source text the word model does not copy; see
    /// [`crate::Shell::transform_operator`] for how the letter is recovered at
    /// expansion time. A front end that knows the letter can produce this
    /// variant directly.
    Transform(Transform),
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

    /// Visit the pattern and replacement words of the pattern operators, in
    /// source order.
    ///
    /// Tilde expansion walks these: bash expands a leading `~` in a pattern
    /// (`HOME=/h; x=/h/foo; "${x#~}"` is `/foo`) and in a replacement
    /// (`"${v/b/~/}"` is `a/h/c`), and it does so *inside double quotes*, where
    /// `"${x:-~}"` stays literal. That is why these are expanded with the
    /// quote context switched off while [`ParamOp::word_mut`] keeps it.
    pub fn visit_pattern_words_mut(&mut self, visit: &mut impl FnMut(&mut Word)) {
        match self {
            ParamOp::RemovePrefix { pattern, .. } | ParamOp::RemoveSuffix { pattern, .. } => {
                visit(pattern);
            }
            ParamOp::Replace {
                pattern,
                replacement,
                ..
            } => {
                visit(pattern);
                visit(replacement);
            }
            ParamOp::Case {
                pattern: Some(pattern),
                ..
            } => visit(pattern),
            _ => {}
        }
    }
}

/// Which occurrences `${x/pat/rep}` replaces.
///
/// The variant is decided by the parser from the operator's spelling, so
/// expansion never re-reads source text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplaceMode {
    /// `${x/pat/rep}`: the leftmost longest match, replaced once.
    First,
    /// `${x//pat/rep}`: every match, left to right.
    All,
    /// `${x/#pat/rep}`: a match anchored at the start of the value.
    Prefix,
    /// `${x/%pat/rep}`: a match anchored at the end of the value.
    Suffix,
}

/// The direction and reach of a [`ParamOp::Case`] conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseHow {
    /// `${x^pat}`: upper-case the first character when it matches.
    UpperFirst,
    /// `${x^^pat}`: upper-case every matching character.
    Upper,
    /// `${x,pat}`: lower-case the first character when it matches.
    LowerFirst,
    /// `${x,,pat}`: lower-case every matching character.
    Lower,
}

impl CaseHow {
    /// Whether only the value's first character is considered.
    pub fn first_only(self) -> bool {
        matches!(self, CaseHow::UpperFirst | CaseHow::LowerFirst)
    }

    /// Whether matching characters are upper-cased rather than lower-cased.
    pub fn to_upper(self) -> bool {
        matches!(self, CaseHow::Upper | CaseHow::UpperFirst)
    }
}

/// The `${x@…}` transformations this crate implements.
///
/// bash has several (`@Q`, `@a`, `@u`, `@U`, `@L`, `@A`, `@K`, `@k`, `@E`,
/// `@P`); only prompt expansion is in scope here, and every other letter stays
/// [`UnsupportedOp::Transform`] rather than being guessed at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transform {
    /// `${x@P}`: expand the value with the prompt escape rules.
    Prompt,
    /// `${x@Q}`: quote the value so it re-reads as one word.
    Quote,
    /// `${x@U}`: upper-case every character.
    Upper,
    /// `${x@u}`: upper-case the first character.
    UpperFirst,
    /// `${x@L}`: lower-case every character.
    Lower,
    /// `${x@l}`: lower-case the first character.
    LowerFirst,
}

impl Transform {
    /// The letter that selects this transformation in source.
    pub fn letter(self) -> u8 {
        match self {
            Transform::Prompt => b'P',
            Transform::Quote => b'Q',
            Transform::Upper => b'U',
            Transform::UpperFirst => b'u',
            Transform::Lower => b'L',
            Transform::LowerFirst => b'l',
        }
    }

    /// The transformation `letter` selects, if this crate implements it.
    pub fn from_letter(letter: u8) -> Option<Self> {
        match letter {
            b'P' => Some(Transform::Prompt),
            b'Q' => Some(Transform::Quote),
            b'U' => Some(Transform::Upper),
            b'u' => Some(Transform::UpperFirst),
            b'L' => Some(Transform::Lower),
            b'l' => Some(Transform::LowerFirst),
            _ => None,
        }
    }
}

/// Bash operators that need pattern matching or re-parsing, left out of scope
/// on purpose: expansion returns [`crate::ErrorKind::Unsupported`] rather than
/// doing something subtly wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsupportedOp {
    /// `${!x}`, `${!prefix*}`: the argument is a variable *name*, which would
    /// mean evaluating data as code.
    Indirect,
    /// `${x@Q}` and the other transformations.
    Transform,
    /// `${@:offset:length}`: substring of a parameter list.
    SubstringOfList,
    /// A subscript form this crate could not use (for example a second
    /// subscript, or one on a special parameter). Kept so the parser can
    /// report the exact surface form it saw.
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
            UnsupportedOp::Indirect => "indirect expansion (`${!x}`)",
            UnsupportedOp::Transform => "parameter transformation (`${x@Q}`)",
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
