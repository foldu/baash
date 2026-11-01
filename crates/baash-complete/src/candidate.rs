//! Candidates: one thing the user could insert, plus how it must be inserted.

use std::ops::Range;

/// A byte range into the line being completed.
pub type Span = Range<usize>;

/// What kind of thing a candidate is.
///
/// The kind drives the menu's icon/colour and the editor's fallback order. It
/// is not used for ranking: ranking is by [`Candidate::tier`] and
/// [`Candidate::frecency`], so a path never outranks a command just because of
/// its kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CandKind {
    /// A path, matched by component prefix.
    Path,
    /// A command from the `PATH` index.
    Command,
    /// A shell function.
    Function,
    /// A shell builtin.
    Builtin,
    /// A shell alias.
    Alias,
    /// A variable or parameter operator.
    Variable,
    /// A completion that is a `[[ … ]]` or parameter-expansion operator.
    Operator,
    /// A shell keyword, e.g. `then` or `esac`.
    Keyword,
    /// A word taken from history.
    History,
    /// A command-line option.
    ///
    /// Reserved: no provider produces options yet (`DESIGN.md` §6.6 item 5).
    Option,
    /// Anything else.
    Other,
}

/// How [`Candidate::insert`] must be placed in the line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Insertion {
    /// `insert` is the argument *value*; it is escaped for the quote context
    /// by [`baash_lang::quote`] before insertion.
    Literal,
    /// `insert` is already shell source — `$FOO`, `:-`, `*)`, or a raw word
    /// taken from history — and is inserted verbatim.
    ///
    /// Source candidates are never offered inside single quotes, where their
    /// syntax would be inert, and they are not re-escaped: escaping `$FOO`
    /// inside `"…"` would turn a parameter expansion into a literal dollar.
    Source,
}

/// One thing the user could insert.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    /// Bytes to insert; authoritative, and not necessarily UTF-8.
    ///
    /// For a directory this ends with `/`; for a [`Insertion::Source`]
    /// candidate it is shell syntax.
    pub insert: Vec<u8>,
    /// UTF-8 text to show, which may be a lossy projection of `insert`.
    pub display: String,
    /// A description column, e.g. "branch, 3 days ago".
    pub desc: Option<String>,
    /// What kind of candidate this is.
    pub kind: CandKind,
    /// Frequency-times-recency weight, used to order candidates *within* a
    /// ranking tier. Must be finite.
    pub frecency: f32,
    /// Ranking tier, filled in by [`crate::rank`]: `0` exact-case prefix,
    /// `1` caseless prefix, `2` fuzzy. A lower tier always sorts first.
    pub tier: u8,
    /// Fuzzy score from `nucleo-matcher`; `0` for prefix matches.
    pub score: u16,
    /// How the bytes must be inserted.
    pub insertion: Insertion,
    /// Whether a unique completion may append a space.
    ///
    /// Directories set this to `false` because they already end with `/`.
    pub append_space: bool,
}

impl Candidate {
    /// A literal candidate: `insert` is an argument value and is escaped for
    /// the quote context on insertion.
    pub fn new(insert: impl Into<Vec<u8>>, kind: CandKind) -> Self {
        let insert = insert.into();
        Self {
            display: String::from_utf8_lossy(&insert).into_owned(),
            insert,
            desc: None,
            kind,
            frecency: 0.0,
            tier: 0,
            score: 0,
            insertion: Insertion::Literal,
            append_space: true,
        }
    }

    /// A candidate whose `insert` is shell source (see [`Insertion::Source`]).
    ///
    /// Source candidates never append a trailing space unless asked to.
    pub fn source(insert: impl Into<Vec<u8>>, kind: CandKind) -> Self {
        let mut candidate = Self::new(insert, kind);
        candidate.insertion = Insertion::Source;
        candidate.append_space = false;
        candidate
    }

    /// Set the description column.
    #[must_use]
    pub fn with_desc(mut self, desc: impl Into<String>) -> Self {
        self.desc = Some(desc.into());
        self
    }

    /// Set the frecency weight.
    #[must_use]
    pub fn with_frecency(mut self, frecency: f32) -> Self {
        self.frecency = frecency;
        self
    }

    /// Allow a unique match to append a space.
    #[must_use]
    pub fn appending_space(mut self) -> Self {
        self.append_space = true;
        self
    }

    /// Override the display text.
    #[must_use]
    pub fn with_display(mut self, display: impl Into<String>) -> Self {
        self.display = display.into();
        self
    }
}

/// Remove duplicate `insert` bytes, keeping the first occurrence.
///
/// The first occurrence wins because providers are consulted in the order the
/// expectation table lists them: a `PATH` command shadows a same-named alias,
/// and a builtin shadows a same-named path.
pub fn dedupe(candidates: &mut Vec<Candidate>) {
    let mut seen = std::collections::HashSet::with_capacity(candidates.len());
    candidates.retain(|candidate| seen.insert(candidate.insert.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_candidate_is_a_literal() {
        let c = Candidate::new("a b", CandKind::Path);
        assert_eq!(c.display, "a b");
        assert_eq!(c.insertion, Insertion::Literal);
        assert!(c.append_space);
    }

    #[test]
    fn display_is_a_lossy_projection() {
        let c = Candidate::new(b"bad\xff".to_vec(), CandKind::Path);
        assert_eq!(c.insert, b"bad\xff");
        assert_eq!(c.display, "bad\u{fffd}");
    }

    #[test]
    fn source_candidates_do_not_append_by_default() {
        let c = Candidate::source("$HOME", CandKind::Variable);
        assert_eq!(c.insertion, Insertion::Source);
        assert!(!c.append_space);
    }

    #[test]
    fn dedupe_keeps_the_first() {
        let mut c = vec![
            Candidate::new("ls", CandKind::Command),
            Candidate::new("ls", CandKind::Path),
        ];
        dedupe(&mut c);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].kind, CandKind::Command);
    }
}
