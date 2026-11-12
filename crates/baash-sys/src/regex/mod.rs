//! POSIX extended regular expressions for `[[ x =~ re ]]`, with a selectable
//! engine.
//!
//! The conformance target is bash, and bash hands the pattern to the platform's
//! `regcomp`/`regexec` (DESIGN.md §4.8). A Rust regex crate is leftmost-first
//! rather than leftmost-longest, so it disagrees with bash on real patterns:
//! alternation, POSIX bracket classes, backreferences and interval
//! expressions. The conformance suite is the specification.
//!
//! # Engines
//!
//! [`Engine::System`] is the platform's own engine — glibc's
//! `regcomp`/`regexec` on Linux, which is literally what bash links. It is the
//! fidelity reference: it accepts bash-completion's patterns as they are,
//! including the quirks (a stray `)` is a literal, `\b` is a word boundary,
//! glibc's capture tie-break), and it is the default on Unix.
//!
//! [`Engine::Minrx`] is [MinRX](https://github.com/michaeljhaertel/minrx), a
//! small BSD-licensed POSIX ERE engine vendored by `minrx-sys`. It is the
//! engine on Windows, where there is no `regcomp`, and an opt-in alternative on
//! Unix. It parses the pattern in one pass into a flat Thompson program, so it
//! compiles several times faster than glibc (`docs/performance.md`), at the
//! cost of three measured differences from bash: no backreferences (`\1` is a
//! literal `1`), `{,n}` is rejected rather than accepted as an interval, and
//! nested/repeated groups take MinRX's capture tie-break rather than glibc's.
//! Everything bash-completion actually uses was measured to agree.
//!
//! The engine is chosen per [`Regex`] (and per [`Cache`]) rather than by a
//! global, so a test or a second interpreter can use a different one in the
//! same process.
//!
//! # What is compiled
//!
//! [`Regex::new`] compiles with POSIX extended syntax and nothing else; in
//! particular **line-oriented matching is never enabled**, because bash does
//! not enable it: `^`/`$` match only at the ends of the whole subject, `.`
//! matches a newline, and a negated bracket expression matches a newline. A
//! caller that wants line-oriented matching must split the subject first.
//! Case folding (`shopt -s nocasematch`) is a compile-time option.
//!
//! # Bytes, not characters
//!
//! Patterns and subjects are `&[u8]`. Capture offsets are byte offsets into
//! the subject because the shell is byte-oriented and `BASH_REMATCH` slices
//! bytes. The shell never calls `setlocale(3)`, so both engines run in the `C`
//! locale: the system engine matches byte by byte, and MinRX takes its `Byte`
//! encoding path rather than converting to wide characters.
//!
//! # NUL bytes
//!
//! The system engine takes C strings, so a pattern or subject containing an
//! embedded NUL cannot be expressed ([`RegexError::NulInPattern`] and
//! [`RegexError::NulInText`]). MinRX takes explicit lengths and searches the
//! whole subject. Shell text cannot contain NUL, so this only matters to a
//! caller that hands these functions raw bytes.
//!
//! # Limits
//!
//! At most [`MAX_GROUPS`] capture slots are filled: the whole match plus 63
//! parenthesised subexpressions. bash keeps every group (a 70-group pattern
//! produced 71 `BASH_REMATCH` elements), so patterns above the limit are
//! truncated here; [`Regex::group_count`] still reports the true count scanned
//! from the pattern source.

use std::fmt;

/// The capture ranges of one match, in bytes.
///
/// Index 0 is the whole match; index `n` is the `n`-th parenthesised
/// subexpression, numbered by opening parenthesis. `None` means the group did
/// not participate in this match, while a slot that is absent from the vector
/// altogether is a group that does not exist (or one dropped by
/// [`MAX_GROUPS`]).
pub type CaptureRanges = Vec<Option<(usize, usize)>>;

/// The largest number of capture slots a match reports.
///
/// Slot 0 is the whole match, so this is the whole match plus 63 parenthesised
/// subexpressions. bash has no such small limit (it reported 71 slots for 70
/// groups); the fixed bound keeps the wrapper's per-match allocation
/// predictable. A pattern with more groups still compiles and matches — only
/// the extra captures are unavailable — so a shell must not treat the limit as
/// a compile error.
pub const MAX_GROUPS: usize = 64;

/// The largest pattern, in bytes, that will be compiled.
///
/// A pattern this large is not something a person types: it is generated, and
/// almost always by a bug (a loop that appends to a pattern, a substitution
/// that never terminates). Compiling it costs the engine an unbounded amount of
/// memory and time, and the shell would then memoise it, so the refusal is a
/// bound on both the compile and the cache. bash has no such limit; this is a
/// deliberate divergence, and the diagnostic names the limit so a script that
/// really does need a huge generated regex can be told what it hit.
pub const MAX_PATTERN: usize = 64 * 1024;

/// Which regular-expression engine a [`Regex`] or [`Cache`] uses.
///
/// The engine is part of the value, not process-global state: two interpreters
/// in one process may use different engines.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Engine {
    /// The platform's `regcomp`/`regexec`: glibc on Linux, which is what bash
    /// uses. Unavailable on Windows, which has no `regcomp`.
    System,
    /// MinRX, the vendored portable engine. Available everywhere; the only
    /// engine on Windows.
    Minrx,
}

impl Engine {
    /// Every engine, in the order `--help` lists them.
    pub const ALL: &'static [Engine] = &[Engine::System, Engine::Minrx];

    /// The name used by `--regex-engine`, `BAASH_REGEX_ENGINE` and diagnostics.
    pub fn name(self) -> &'static str {
        match self {
            Engine::System => "system",
            Engine::Minrx => "minrx",
        }
    }

    /// Parse a name, accepting `glibc` as an alias for [`Engine::System`].
    ///
    /// The alias exists because `glibc` is what the engine is on the platform
    /// the shell is developed on; `system` is the name that stays true on a
    /// BSD or macOS host, so it is the one printed everywhere.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "system" | "glibc" => Some(Engine::System),
            "minrx" => Some(Engine::Minrx),
            _ => None,
        }
    }

    /// Whether this engine can compile a pattern on this platform.
    pub fn is_available(self) -> bool {
        match self {
            Engine::System => cfg!(unix),
            Engine::Minrx => true,
        }
    }
}

impl Default for Engine {
    /// The platform's engine where there is one (maximal bash compatibility),
    /// MinRX where there is not (Windows).
    fn default() -> Self {
        if cfg!(unix) {
            Engine::System
        } else {
            Engine::Minrx
        }
    }
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

mod minrx;
#[cfg(unix)]
mod system;

/// The compiled form, one variant per engine.
enum Backend {
    #[cfg(unix)]
    System(system::Raw),
    Minrx(minrx::Raw),
}

impl Backend {
    fn compile(engine: Engine, pattern: &[u8], icase: bool) -> Result<Self, RegexError> {
        match engine {
            #[cfg(unix)]
            Engine::System => Ok(Self::System(system::Raw::compile(pattern, icase)?)),
            #[cfg(not(unix))]
            Engine::System => Err(RegexError::Unavailable { engine }),
            Engine::Minrx => Ok(Self::Minrx(minrx::Raw::compile(pattern, icase)?)),
        }
    }

    fn exec(&self, text: &[u8], slots: usize) -> Result<Option<CaptureRanges>, RegexError> {
        match self {
            #[cfg(unix)]
            Self::System(raw) => raw.exec(text, slots),
            Self::Minrx(raw) => raw.exec(text, slots),
        }
    }
}

/// A compiled POSIX extended regular expression.
///
/// The compiled form is owned by the engine's backend, which frees it exactly
/// once, on drop. A `Regex` is deliberately not `Clone`/`Copy`; it is used on
/// one shell thread and matched repeatedly. Matching takes `&self` — the
/// wrappers keep per-search state on the stack or in an allocation the backend
/// owns, never in a way that could alias across threads.
pub struct Regex {
    backend: Backend,
    engine: Engine,
    /// Parenthesised subexpressions scanned from the pattern source.
    groups: usize,
}

impl Regex {
    /// Compile `pattern` with the platform's default engine
    /// ([`Engine::default`]).
    ///
    /// # Errors
    ///
    /// A pattern the engine rejects becomes [`RegexError::Engine`] carrying the
    /// engine's own message, which is what bash shows the user. The system
    /// engine also rejects an embedded NUL with [`RegexError::NulInPattern`]
    /// rather than truncating the pattern.
    ///
    /// A pattern with more than `MAX_GROUPS - 1` parenthesised subexpressions
    /// still compiles; [`Regex::captures`] reports the first [`MAX_GROUPS`]
    /// slots and the rest are dropped (see [`Regex::group_count`]).
    pub fn new(pattern: &[u8]) -> Result<Self, RegexError> {
        Self::with_engine(Engine::default(), pattern, false)
    }

    /// Compile `pattern` with the default engine and case folding
    /// (`shopt -s nocasematch`).
    ///
    /// Folding is the engine's; a caller must not try to get the same effect
    /// by lowercasing the pattern and the subject, because that changes
    /// bracket expressions (an engine also folds `[[:upper:]]` and
    /// `[[:lower:]]` under case-insensitivity).
    pub fn new_icase(pattern: &[u8]) -> Result<Self, RegexError> {
        Self::with_engine(Engine::default(), pattern, true)
    }

    /// Compile `pattern` with an explicit engine and case mode.
    ///
    /// # Errors
    ///
    /// [`RegexError::Unavailable`] when the engine does not exist on this
    /// platform (the system engine on Windows), otherwise as [`Regex::new`].
    pub fn with_engine(engine: Engine, pattern: &[u8], icase: bool) -> Result<Self, RegexError> {
        if !engine.is_available() {
            return Err(RegexError::Unavailable { engine });
        }
        if pattern.len() > MAX_PATTERN {
            return Err(RegexError::TooLarge { len: pattern.len() });
        }
        // The scan is pure, so nothing can leak even if it were to panic (it
        // cannot), and a pattern that fails to compile never reports a count.
        let groups = count_subexpressions(pattern);
        let backend = Backend::compile(engine, pattern, icase)?;
        Ok(Self {
            backend,
            engine,
            groups,
        })
    }

    /// The engine this pattern was compiled with.
    pub fn engine(&self) -> Engine {
        self.engine
    }

    /// The number of parenthesised subexpressions in the pattern.
    ///
    /// This is the count bash exposes as `re_nsub`. [`Regex::captures`]
    /// returns `min(group_count() + 1, MAX_GROUPS)` entries: the whole match
    /// plus one slot per group. POSIX provides no way to read `re_nsub` back
    /// out of a compiled expression, so the count is scanned from the pattern
    /// source (an unescaped `(` outside a bracket expression); the pattern
    /// must have compiled for the scan to be used.
    pub fn group_count(&self) -> usize {
        self.groups
    }

    /// Match anywhere in `text`, returning the capture ranges in bytes.
    ///
    /// The search is not anchored: `"b"` matches `"abc"` at `(1, 2)`. Index 0
    /// is the whole match. A group that did not participate in the match is
    /// `None`; unused slots are not reported at all, so the vector has
    /// `min(group_count() + 1, MAX_GROUPS)` entries.
    ///
    /// Returns `None` when there is no match, when `text` cannot be searched
    /// at all (an embedded NUL on the system engine; see [`Regex::try_captures`]
    /// for the error), or when the engine reports a failure of its own. Use
    /// [`Regex::try_captures`] to distinguish "did not match" from "could not
    /// search".
    pub fn captures(&self, text: &[u8]) -> Option<CaptureRanges> {
        self.try_captures(text).ok().flatten()
    }

    /// [`Regex::captures`], but a failure to run the search is an error.
    ///
    /// `Ok(None)` means the pattern did not match, which is not an error in
    /// POSIX. `Err(RegexError::NulInText)` means the subject contains an
    /// embedded NUL and the engine takes C strings; `Err(RegexError::Engine)`
    /// is a failure from the engine's search (for example running out of
    /// memory).
    pub fn try_captures(&self, text: &[u8]) -> Result<Option<CaptureRanges>, RegexError> {
        self.backend.exec(text, self.slots())
    }

    /// The number of capture slots to request from the engine.
    fn slots(&self) -> usize {
        self.groups.saturating_add(1).min(MAX_GROUPS)
    }
}

impl fmt::Debug for Regex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Regex")
            .field("engine", &self.engine)
            .field("group_count", &self.groups)
            .finish_non_exhaustive()
    }
}

/// The last few compiled patterns, reused across evaluations.
///
/// `[[ x =~ re ]]` in a loop re-evaluates the same pattern on every iteration,
/// and compiling is the dominant cost — glibc's `regcomp` is 8.7 µs on an
/// 84-byte completion pattern, and even MinRX's 1.8 µs is more than the match
/// it feeds. bash has the same problem (its own `=~` costs 37.9 µs on that
/// pattern), which is why a loop over `=~` is slow everywhere.
///
/// A small memo keyed on the pattern bytes and the case mode removes the
/// compile from the loop; measured, it takes that glibc evaluation from 29 µs
/// to 3.6 µs (`docs/performance.md`). Sixteen slots, least-recently-used,
/// because the second common shape is a loop alternating between the patterns
/// a completion function tests, and one entry misses every iteration there.
/// The entries are kept in recency order and scanned newest-first, so the
/// common single-pattern loop compares once; a hash map would compute a hash
/// of the whole pattern on every call, which is dearer than that one
/// comparison, so this stays a hand-rolled array rather than an LRU crate.
///
/// Compile errors are deliberately **not** cached: the shell reports the
/// engine's message at the point of use, and a failed compile is cheap next to
/// the diagnostics it produces.
///
/// The cache is not observable: cloning it starts empty, and no shell variable
/// or output depends on whether a compilation happened.
pub struct Cache {
    engine: Engine,
    /// Least recently used first, most recently used last.
    entries: Vec<Entry>,
}

/// How many compiled patterns a cache keeps.
///
/// Sixteen covers the measured shapes (one pattern in a loop, two alternating,
/// and the patterns one completion function tests) without letting a
/// pathological script pin arbitrary memory: each entry is a pattern plus one
/// compiled form.
const CACHE_SLOTS: usize = 16;

struct Entry {
    pattern: Vec<u8>,
    icase: bool,
    regex: Regex,
}

impl Cache {
    /// An empty cache that compiles with `engine`.
    pub fn new(engine: Engine) -> Self {
        Self {
            engine,
            entries: Vec::new(),
        }
    }

    /// The engine new patterns are compiled with.
    pub fn engine(&self) -> Engine {
        self.engine
    }

    /// Switch engines, discarding the memo if the engine changed.
    ///
    /// A compiled pattern belongs to the engine that produced it, so it cannot
    /// be reused across a switch; after this call the next evaluation compiles
    /// again with the new engine.
    pub fn set_engine(&mut self, engine: Engine) {
        if self.engine != engine {
            self.engine = engine;
            self.clear();
        }
    }

    /// Drop the memo, keeping the engine.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Compile `pattern` if it is not cached, then search `text`.
    ///
    /// The returned captures are owned, so the caller may hold them while
    /// using the cache again.
    pub fn captures(
        &mut self,
        pattern: &[u8],
        icase: bool,
        text: &[u8],
    ) -> Result<Option<CaptureRanges>, RegexError> {
        // Newest first: the pattern a loop repeats is the last one inserted.
        if let Some(index) = self
            .entries
            .iter()
            .rposition(|entry| entry.icase == icase && entry.pattern == pattern)
        {
            // Refresh its position, keeping the vector in recency order.
            let entry = self.entries.remove(index);
            self.entries.push(entry);
            return self
                .entries
                .last()
                .expect("just pushed")
                .regex
                .try_captures(text);
        }
        let regex = Regex::with_engine(self.engine, pattern, icase)?;
        if self.entries.len() == CACHE_SLOTS {
            // The least recently used entry is the first one.
            self.entries.remove(0);
        }
        self.entries.push(Entry {
            pattern: pattern.to_vec(),
            icase,
            regex,
        });
        self.entries
            .last()
            .expect("just pushed")
            .regex
            .try_captures(text)
    }
}

impl Clone for Cache {
    /// A clone starts empty: the memo is an optimisation, never state.
    fn clone(&self) -> Self {
        Self::new(self.engine)
    }
}

impl fmt::Debug for Cache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cache")
            .field("engine", &self.engine)
            .field("cached", &self.entries.len())
            .finish()
    }
}

impl Default for Cache {
    fn default() -> Self {
        Self::new(Engine::default())
    }
}

/// Why compiling a pattern or running a search failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegexError {
    /// The pattern contains an embedded NUL byte.
    ///
    /// The system engine takes a C string, so the pattern cannot be expressed
    /// at all. A shell must reject it rather than silently compiling a prefix.
    NulInPattern,
    /// The text to search contains an embedded NUL byte.
    ///
    /// The system engine takes a C string, so searching would silently stop at
    /// the NUL. [`Regex::captures`] gives up (`None`); [`Regex::try_captures`]
    /// reports this error.
    NulInText,
    /// The engine returned an error code.
    ///
    /// `message` is the engine's own text, which is what bash shows the user
    /// (for example `Unmatched ( or \(`), so compiling a mistyped pattern
    /// produces an explanation and not just a number. The meaning of `code`
    /// is the engine's.
    Engine {
        /// The engine's error code.
        code: libc::c_int,
        /// The engine's message, lossily decoded as UTF-8.
        message: String,
    },
    /// The requested engine does not exist on this platform.
    ///
    /// The system engine is Unix-only; the binary rejects the name before it
    /// gets here, so this is the defensive result of calling
    /// [`Regex::with_engine`] directly.
    Unavailable {
        /// The engine that was asked for.
        engine: Engine,
    },
    /// The pattern is larger than [`MAX_PATTERN`].
    ///
    /// `len` is the pattern's length in bytes, which is what a diagnostic
    /// needs; the limit itself is the constant, so it cannot go stale here.
    TooLarge {
        /// The pattern's length in bytes.
        len: usize,
    },
}

impl RegexError {
    /// The raw engine error code, or `None` when there is no engine code.
    pub fn code(&self) -> Option<libc::c_int> {
        match self {
            Self::Engine { code, .. } => Some(*code),
            Self::NulInPattern
            | Self::NulInText
            | Self::Unavailable { .. }
            | Self::TooLarge { .. } => None,
        }
    }

    /// A human-readable message, never empty.
    ///
    /// For [`RegexError::Engine`] this is the engine's text in the current
    /// locale; error text is subject to translation, so classify on
    /// [`RegexError::code`] rather than on this string.
    pub fn message(&self) -> &str {
        match self {
            Self::NulInPattern => "embedded NUL byte in a regular expression",
            Self::NulInText => "embedded NUL byte in the text searched for a regular expression",
            Self::Engine { message, .. } => message,
            Self::Unavailable { .. } => {
                "the requested regular-expression engine is not available on this platform"
            }
            Self::TooLarge { .. } => "regular expression is too large (64 KiB limit)",
        }
    }
}

impl fmt::Display for RegexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for RegexError {}

/// Count the parenthesised subexpressions in an ERE pattern.
///
/// POSIX ERE has no non-capturing groups, so this is simply the number of
/// unescaped `(` outside a bracket expression. Backslash escapes the next
/// byte there, while inside a bracket expression it is an ordinary member
/// (glibc agrees: `[a\]b]` is the class `{a, \}` followed by a literal `b]`,
/// not an escaped `]`).
///
/// A malformed pattern can make this count "wrong", but such a pattern never
/// compiles, so `Regex` never uses the count. The bracket scanner only needs
/// to find the *end* of a bracket expression: `[. .]`, `[= =]` and `[: :]`
/// items close with `.]]`, `=]]` and `:]]`, and a `]` in first position is a
/// literal member.
fn count_subexpressions(pattern: &[u8]) -> usize {
    let mut groups = 0;
    let mut index = 0;
    while index < pattern.len() {
        match pattern[index] {
            // Skip the escaped byte (and, if the backslash is trailing, fall
            // out of the loop).
            b'\\' => index += 2,
            b'[' => index = after_bracket_expression(pattern, index),
            b'(' => {
                groups += 1;
                index += 1;
            }
            _ => index += 1,
        }
    }
    groups
}

/// The index just past the bracket expression starting at `start`.
///
/// `start` is known to point at `[`. Returns `pattern.len()` when the
/// expression is unterminated, which only happens for patterns the engine
/// rejects.
fn after_bracket_expression(pattern: &[u8], start: usize) -> usize {
    let mut index = start + 1;
    if index < pattern.len() && pattern[index] == b'^' {
        index += 1;
    }
    // A `]` immediately after `[` or `[^` is a member, not the terminator.
    if index < pattern.len() && pattern[index] == b']' {
        index += 1;
    }
    while index < pattern.len() {
        match pattern[index] {
            b']' => return index + 1,
            b'[' if index + 1 < pattern.len() => {
                let kind = pattern[index + 1];
                if matches!(kind, b':' | b'.' | b'=') {
                    // `[: :]`, `[. .]` and `[= =]` are single items: their
                    // inner `]` does not terminate the bracket expression.
                    let mut close = index + 2;
                    while close + 1 < pattern.len()
                        && !(pattern[close] == kind && pattern[close + 1] == b']')
                    {
                        close += 1;
                    }
                    if close + 1 < pattern.len() {
                        index = close + 2;
                        continue;
                    }
                    return pattern.len();
                }
                index += 1;
            }
            _ => index += 1,
        }
    }
    pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_groups_outside_bracket_expressions() {
        // The expected counts were cross-checked against glibc's public
        // `re_nsub` field with a C probe; they follow from the POSIX ERE
        // grammar, not from this implementation.
        assert_eq!(count_subexpressions(b""), 0);
        assert_eq!(count_subexpressions(b"(a)"), 1);
        assert_eq!(count_subexpressions(b"(a)(b)?(c)"), 3);
        assert_eq!(count_subexpressions(b"(a(b)c)"), 2);
        assert_eq!(count_subexpressions(b"[(]"), 0);
        assert_eq!(count_subexpressions(b"[)]"), 0);
        assert_eq!(count_subexpressions(b"[]]"), 0);
        assert_eq!(count_subexpressions(b"[^]]"), 0);
        assert_eq!(count_subexpressions(b"[a(b]"), 0);
        assert_eq!(count_subexpressions(b"[[]"), 0);
        assert_eq!(count_subexpressions(b"[[:alpha:]]"), 0);
        assert_eq!(count_subexpressions(b"[[:alpha:](]"), 0);
        assert_eq!(count_subexpressions(b"[[.a.]]"), 0);
        assert_eq!(count_subexpressions(b"[[.a.](]"), 0);
        assert_eq!(count_subexpressions(b"[[=a=]]"), 0);
        // A backslash is a literal member inside a bracket expression.
        assert_eq!(count_subexpressions(b"[a\\]b]"), 0);
        assert_eq!(count_subexpressions(b"\\[(a)"), 1);
        assert_eq!(count_subexpressions(b"\\(a\\)"), 0);
        assert_eq!(count_subexpressions(b"\\\\(a)"), 1);
        assert_eq!(count_subexpressions(b"[[:alpha:]]+(x)"), 1);
        assert_eq!(count_subexpressions(b"[[:alpha:]]+(x)(y)"), 2);
    }

    #[test]
    fn a_failed_compile_still_has_a_message() {
        // Every backend must explain a user typo rather than return an empty
        // string; the exact wording is the engine's.
        let error = Regex::new(b"(a").expect_err("`(a` must not compile");
        assert!(!error.message().is_empty());
        assert!(!error.to_string().is_empty());
        assert!(!format!("{error:?}").is_empty());
        assert!(error.code().is_some());
    }

    #[test]
    fn engine_names_round_trip() {
        for engine in Engine::ALL {
            assert_eq!(Engine::from_name(engine.name()), Some(*engine));
        }
        assert_eq!(Engine::from_name("glibc"), Some(Engine::System));
        assert_eq!(Engine::from_name("pcre"), None);
    }

    #[test]
    fn an_unavailable_engine_is_an_error_not_a_panic() {
        for engine in Engine::ALL {
            if engine.is_available() {
                continue;
            }
            let error = Regex::with_engine(*engine, b"a", false)
                .expect_err("an unavailable engine must not compile");
            assert!(matches!(error, RegexError::Unavailable { .. }));
            assert!(!error.message().is_empty());
        }
    }

    #[test]
    fn a_cache_clone_starts_empty() {
        let mut cache = Cache::new(Engine::Minrx);
        assert!(cache.captures(b"a", false, b"a").unwrap().is_some());
        let clone = cache.clone();
        assert_eq!(clone.engine(), Engine::Minrx);
        assert!(!format!("{clone:?}").contains("cached: true"));
    }
}
