//! Shell pattern matching: one engine for `case`, `[[ x == pat ]]`, the four
//! parameter-strip operators, `${x/pat/rep}`, pathname expansion and
//! completion filtering.
//!
//! Patterns and subjects are bytes (`&[u8]`), never `String`: shell strings
//! are byte strings `[POSIX]` and a filename need not be UTF-8. There are no
//! dependencies, no I/O and no shell state; the caller owns the flags.
//!
//! ```
//! use bsh_pattern::{Flags, matches, prefix_end, suffix_start};
//!
//! assert!(matches(b"a*c", b"abc", Flags::default()));
//! assert!(!matches(b"a*c", b"abcd", Flags::default()), "matches is anchored");
//!
//! // ${x#*b} and ${x##*b} on "aabb".
//! assert_eq!(prefix_end(b"*b", b"aabb", false, Flags::default()), Some(3));
//! assert_eq!(prefix_end(b"*b", b"aabb", true, Flags::default()), Some(4));
//!
//! // ${x%c*} and ${x%%c*} on "aabbccdd".
//! assert_eq!(suffix_start(b"c*", b"aabbccdd", false, Flags::default()), Some(5));
//! assert_eq!(suffix_start(b"c*", b"aabbccdd", true, Flags::default()), Some(4));
//! ```
//!
//! # Surface syntax
//!
//! When [`Flags::extglob`] is off, `(`, `)`, `|`, `@`, `+` and `!` are
//! ordinary characters and a backslash escapes the next character. On:
//!
//! | operator | meaning |
//! |---|---|
//! | `?(a\|b)` | zero or one alternative |
//! | `*(a\|b)` | zero or more repetitions |
//! | `+(a\|b)` | one or more repetitions |
//! | `@(a\|b)` | exactly one alternative |
//! | `!(a\|b)` | any span that is not one alternative |
//!
//! Groups nest and alternatives may be empty (`@()`, `@(a|)`, `@(||)`).
//! `!(A)` is a *span complement*, not a negative lookahead: for a fixed start
//! it may end at any position `q` for which `A` does not match exactly
//! `text[start..q]`. The rest of the pattern then continues from `q`. That is
//! what makes `[[ ac == a!(b)c ]]` true and `[[ abc == a!(b)c ]]` false.
//!
//! # Characters, flags and offsets
//!
//! `?`, `*` and bracket expressions work on *character units*: one well-formed
//! UTF-8 scalar value, or one invalid byte. `?` therefore matches `μ` once
//! (leaving a valid string), while each invalid byte is still matched exactly.
//! This follows bash 5.3 in a UTF-8 locale. There is deliberately no `LC_ALL=C`
//! mode: the shell has no locale model yet, and a byte-granular `?` would split
//! a valid character. Returned offsets are always byte offsets at a character
//! boundary.
//!
//! [`Flags`] has three orthogonal switches:
//!
//! * `extglob` enables the five operators above. Off, `?(…)` is `?` followed
//!   by a literal `(` — exactly bash's parsing rule.
//! * `nocasematch` ASCII-folds literals, bracket characters and *range
//!   endpoints* (so `[A-z]` behaves as `[a-z]`). Non-ASCII bytes are unchanged,
//!   because ASCII folding is the only kind that cannot silently mangle a
//!   non-UTF-8 filename. POSIX classes are **not** folded: `[[:upper:]]`
//!   still means uppercase, matching bash 5.3.20.
//! * `pathname` makes `*`, `?` and bracket expressions refuse `/`, for
//!   pathname expansion. Dotfile rules (`dotglob`) are the caller's job, as is
//!   splitting a path into components. `!(…)` is not slash-aware, matching
//!   glibc `fnmatch(3)` with `FNM_PATHNAME|FNM_EXTMATCH`; a component never
//!   contains `/`, so this only matters if a caller passes a whole path.
//!
//! # Deliberate semantics
//!
//! These are the places where a byte-oriented matcher has to make a call:
//!
//! * **POSIX classes are ASCII.** `[:alpha:]` does not match `é`, unlike glibc
//!   in a UTF-8 locale. They are locale-independent on purpose; a shell that
//!   behaves the same everywhere is worth more than one that matches the host
//!   locale's idea of `isalpha`.
//! * **Ranges compare Unicode scalar values**, not collation weights. glibc
//!   uses the locale's collation order; `[à-ÿ]` matching `é` works here, but a
//!   locale-specific ordering would not necessarily. Invalid bytes match no
//!   range, though they match literal bracket characters.
//! * **An unterminated `[` is a literal `[`.** Bash does this; `[abc` matches
//!   the five bytes `[abc`.
//! * **A trailing backslash is a literal backslash.**
//! * **An unterminated extglob group degrades to literal text**: with extglob
//!   on, `@(a` matches `@(a`. Bash's behavior for malformed groups varies
//!   (`@(a` is literal, but a trailing `?(` after a `*` can behave like the
//!   truncated prefix); this crate always uses the literal fallback. Malformed
//!   patterns are not worth an error path, and a `case` pattern must never
//!   fail to compile.
//! * **An unknown `[:name:]` class matches nothing**, which is what bash does
//!   for `[[:bogus:]]` (it does not match `[`, `:` or the name).
//! * **Only `[:class:]` bracket syntax is implemented.** Collating symbols
//!   (`[[.ch.]]`) and equivalence classes (`[[=a=]]`) are treated as ordinary
//!   bracket members, so their bytes match literally. They are locale features
//!   with no locale-independent meaning; nothing in the shell's pattern
//!   consumers needs them.
//! * **Brace expansion is not here.** `{a,b}` and `{1..3}` are syntax, expanded
//!   before pattern matching; they are not part of this crate.
//! * **Empty alternatives are real**: `@()` matches the empty string, `!()`
//!   matches every non-empty string, and `!(|)` behaves like `!()`.
//! * **Extglob nesting is capped at 128 groups.** The parser, compiler and
//!   `!` evaluator recurse on nesting, and patterns can come from variables;
//!   deeper groups degrade to literal text rather than risk a stack overflow.
//! * **Bash's empty-group-after-`*` bug is not reproduced.** bash 5.3.20 fails
//!   `[[ abc == *@() ]]` even though the group matches the empty string, while
//!   `@()*`, `x*@()y` and `*[a]@()` all work. The same inconsistency affects
//!   `*+(|)`. Language composition says the group just matches empty, so here
//!   `*@()` behaves like `*`; emulating the bug would make the matcher depend
//!   on where a group happens to sit.
//! * **Quoting is not part of this API.** `case` and `[[ … ]]` have already
//!   removed quotes before matching; a glob caller that has a per-byte quoting
//!   mask must turn quoted metacharacters into backslash escapes (and a quoted
//!   backslash into `\\`) before calling. An escaped metacharacter is literal,
//!   so this is exact and costs one pass over the pattern.
//!
//! # Algorithm and complexity
//!
//! The pattern is parsed into a small tree of literals, `?`, `*`, brackets and
//! extglob groups, then compiled to a Thompson-style NFA with explicit
//! `Split`/`Star` epsilon edges. Matching simulates a *set of NFA states* for
//! each character position instead of backtracking; `prefix_end` simply
//! records every position at which the accept state is active and takes the
//! first (shortest) or last (longest). Suffix matching reverses the parsed
//! pattern and walks the text from the end, which is exact because reversal
//! commutes with alternation, repetition and complement; offsets are mapped
//! back to the original string.
//!
//! `!(A)` is the one construct that is not a plain NFA edge. At each position
//! where it is reached, the sub-automaton `A` is simulated over the rest of
//! the text, the accepted ends are memoised per (sub-program, position), and
//! the continuation is scheduled at every *other* end. This is why the
//! algorithm is polynomial even for nested negations.
//!
//! For `n` text characters and `m` NFA instructions: `O(n·m)` time and
//! `O(n·m / 64)` words for the pending frontier without `!(…)`. With `!(…)`
//! the worst case is `O(n²·m)` time and `O(n²·m)` bits of memoised end sets,
//! still polynomial; the memo is per sub-program and start position. There is
//! no exponential path: 30 `a*` groups against 200 `a`s is roughly 200 × 90
//! state transitions, while a backtracking matcher would consider 200³⁰
//! splits. The crate's tests include that case under a watchdog.
//!
//! # Wiring notes
//!
//! * `case` and `[[ x == pat ]]`: [`matches()`], with `pathname: false`. One
//!   bash surprise: `[[ ]]` matches its right-hand side with extglob semantics
//!   **even when `shopt -u extglob`** (`[[ cc == @(cc) ]]` is true), while
//!   `case` parses extglob syntax only with the option on. To reproduce bash,
//!   pass `extglob: true` for `[[ == ]]` unconditionally.
//! * `${x#pat}` / `${x##pat}`: [`prefix_end`] with `longest` false / true.
//! * `${x%pat}` / `${x%%pat}`: [`suffix_start`] with `longest` false / true.
//!   Bash 5.3.20 ignores `shopt -s nocasematch` for these four operators
//!   (`v=AbC; ${v#a}` stays `AbC`) even though it honours it for
//!   `${v/a/X}`. Pass `nocasematch: false` here to match bash.
//! * `${x/pat/rep}`, `${x//pat/rep}`, `${x/#pat/rep}`, `${x/%pat/rep}`:
//!   [`prefix_end`] with `longest: true` at the candidate position — call it
//!   on `&text[i..]` and add `i` to the result (bash uses the longest match at
//!   the leftmost position). Empty matches need the substitution loop's own
//!   policy: bash leaves `x` untouched for `${x//""/X}`, prepends for
//!   `${x/#/X}` and appends for `${x/%/X}`.
//! * Pathname expansion: compile one [`Pattern`] and call
//!   [`Pattern::matches`] for each directory entry with `pathname: true`.
//! * `shopt -s extglob` maps to `extglob` for `case`, `${x#pat}` and
//!   `${x/pat/rep}`; `[[ ]]` needs it unconditionally (see above).
//! * `shopt -s nocasematch` maps to `nocasematch` for `case`, `[[ ]]` and
//!   `${x/pat/rep}`; **not** for `${x#pat}`/`${x%pat}` and not for pathname
//!   expansion. Pathname expansion uses `shopt -s nocaseglob`; wire that
//!   option into `nocasematch` for the glob caller only.
//! * `globstar`, `dotglob`, `nullglob` and sorting are glob-layer features,
//!   not pattern features.
//!
//! [`POSIX`]: https://pubs.opengroup.org/onlinepubs/9799919799/

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod ast;
mod nfa;
mod units;

#[cfg(test)]
mod tests;

/// Matching options.
///
/// All three are off by default: no extglob, case-sensitive, and no pathname
/// semantics. (This is not the same as bash's `[[ == ]]`, which enables extglob
/// matching regardless of `shopt`; see the crate-level wiring notes.)
#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq)]
pub struct Flags {
    /// Enable `?(…)`, `*(…)`, `+(…)`, `@(…)` and `!(…)`.
    pub extglob: bool,
    /// Fold ASCII case in literals, bracket characters and range endpoints.
    pub nocasematch: bool,
    /// Make `*`, `?` and bracket expressions refuse `/`.
    pub pathname: bool,
}

/// A parsed and compiled pattern, reusable across subjects.
///
/// [`matches()`], [`prefix_end`] and [`suffix_start`] compile on every call. A
/// glob loop should construct a `Pattern` once and match every directory entry
/// against it. Suffix matching needs a reversed program; it is compiled on the
/// first suffix query and then reused, so a pattern only used for prefix
/// matching pays nothing for it.
#[derive(Clone, Debug)]
pub struct Pattern {
    flags: Flags,
    /// The parsed top-level sequence, kept so `suffix_start` can reverse it.
    alts: Vec<ast::Elem>,
    forward: nfa::Program,
    /// Lazily compiled `reverse(alts)`.
    reverse: std::sync::OnceLock<nfa::Program>,
}

impl Pattern {
    /// Parse and compile `pattern`.
    ///
    /// Parsing is total: malformed input becomes literal text, it never panics
    /// and never returns an error. See the crate docs for the exact fallbacks.
    pub fn new(pattern: &[u8], flags: Flags) -> Self {
        let alts = ast::parse(pattern, flags.extglob);
        let forward = nfa::compile(&alts);
        Self {
            flags,
            alts,
            forward,
            reverse: std::sync::OnceLock::new(),
        }
    }

    /// The flags this pattern was compiled with.
    pub fn flags(&self) -> Flags {
        self.flags
    }

    /// Match the whole of `text`, anchored at both ends.
    pub fn matches(&self, text: &[u8]) -> bool {
        let text = units::Text::forward(text);
        let n = text.len();
        nfa::run(&self.forward, &text, self.flags)[n]
    }

    /// Match a prefix of `text`; return the byte offset just past it.
    ///
    /// With `longest` false this is `${x#pat}`; with true, `${x##pat}`.
    /// `Some(0)` means the pattern matched the empty prefix. `None` means no
    /// prefix matched.
    pub fn prefix_end(&self, text: &[u8], longest: bool) -> Option<usize> {
        let text = units::Text::forward(text);
        let ends = nfa::run(&self.forward, &text, self.flags);
        pick(&ends[..], text.len(), longest).map(|q| text.offset(q))
    }

    /// Match a suffix of `text`; return the byte offset where it starts.
    ///
    /// With `longest` false this is `${x%pat}`; with true, `${x%%pat}`.
    /// `Some(text.len())` means the pattern matched the empty suffix. `None`
    /// means no suffix matched.
    pub fn suffix_start(&self, text: &[u8], longest: bool) -> Option<usize> {
        let program = self
            .reverse
            .get_or_init(|| nfa::compile(&ast::reverse(&self.alts)));
        let text = units::Text::reverse(text);
        let ends = nfa::run(program, &text, self.flags);
        pick(&ends[..], text.len(), longest).map(|q| text.offset(q))
    }
}

/// Whether `pattern` matches all of `text`.
///
/// Anchored at both ends, like `case` and `[[ x == pat ]]` without an implicit
/// `*`. See [`Pattern`] to reuse a compiled pattern.
pub fn matches(pattern: &[u8], text: &[u8], flags: Flags) -> bool {
    Pattern::new(pattern, flags).matches(text)
}

/// Match a prefix of `text` and return the byte offset just past it.
///
/// `longest` false is `${x#pat}`, true is `${x##pat}`. See [`Pattern::prefix_end`].
pub fn prefix_end(pattern: &[u8], text: &[u8], longest: bool, flags: Flags) -> Option<usize> {
    Pattern::new(pattern, flags).prefix_end(text, longest)
}

/// Match a suffix of `text` and return the byte offset where it starts.
///
/// `longest` false is `${x%pat}`, true is `${x%%pat}`. See [`Pattern::suffix_start`].
pub fn suffix_start(pattern: &[u8], text: &[u8], longest: bool, flags: Flags) -> Option<usize> {
    Pattern::new(pattern, flags).suffix_start(text, longest)
}

/// First or last accepting position. Positions are ordered by construction.
fn pick(ends: &[bool], n: usize, longest: bool) -> Option<usize> {
    if longest {
        (0..=n).rev().find(|&q| ends[q])
    } else {
        (0..=n).find(|&q| ends[q])
    }
}
