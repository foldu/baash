//! POSIX extended regular expressions, through the platform's own engine.
//!
//! `[[ x =~ re ]]` has to agree with bash, and bash hands the pattern to
//! `regcomp`/`regexec` with `REG_EXTENDED` (DESIGN.md §4.8). A Rust regex
//! crate is leftmost-first rather than POSIX, so it disagrees with bash on
//! real patterns: leftmost-longest alternation, POSIX bracket classes,
//! backreferences and interval expressions. The conformance suite is the
//! specification, so this module binds the same engine bash uses instead of
//! imitating it.
//!
//! # What is compiled
//!
//! [`Regex::new`] compiles with `REG_EXTENDED` and nothing else. In
//! particular **`REG_NEWLINE` is not set**, because bash does not set it:
//! `^`/`$` match only at the ends of the whole subject, `.` matches a newline,
//! and a negated bracket expression matches a newline. A caller that wants
//! line-oriented matching must split the subject first.
//!
//! # Bytes, not characters
//!
//! Patterns and subjects are `&[u8]`. Capture offsets are byte offsets into
//! the subject even when the text is UTF-8, because the shell is
//! byte-oriented and `BASH_REMATCH` slices bytes. Under a multibyte locale `.`
//! and bracket expressions match whole characters, but the offsets stay byte
//! offsets.
//!
//! # Locale
//!
//! `regcomp`/`regexec` use the process locale: POSIX classes such as
//! `[[:alpha:]]`, collating symbols and multibyte decoding all come from
//! `LC_CTYPE`/`LC_COLLATE`. A Rust process starts in the `C` locale even when
//! the environment says otherwise, so the shell must call `setlocale(3)`
//! before compiling a pattern if it wants bash's behaviour on non-ASCII text.
//! This module never changes process state.
//!
//! # Limits
//!
//! * The engine takes C strings, so an embedded NUL cannot be expressed.
//!   [`Regex::new`] rejects a pattern containing NUL with
//!   [`Error::NulInPattern`]; a search of text containing NUL gives up
//!   ([`Regex::captures`] returns `None`, [`Regex::try_captures`] returns
//!   [`Error::NulInText`]). Truncating either would silently match something
//!   the caller did not ask for.
//! * At most [`MAX_GROUPS`] capture slots are filled: the whole match plus 63
//!   parenthesised subexpressions. bash keeps every group (a 70-group pattern
//!   produced 71 `BASH_REMATCH` elements), so patterns above the limit are
//!   truncated here; [`Regex::group_count`] still reports the true count
//!   scanned from the pattern source.
//!
//! # Engine quirks
//!
//! Delegating exists to inherit bash's quirks rather than invent our own.
//! Measured on glibc (Linux x86-64, `C` locale):
//!
//! * `[[:alpha:]]` is ASCII-only in the `C` locale and decodes multibyte
//!   characters in a UTF-8 locale; the answer depends on `setlocale`.
//! * `\b` **is** a word boundary: glibc implements it as a GNU extension to
//!   ERE. This is easy to get wrong in the other direction — POSIX ERE does
//!   not define `\b`, but glibc does not treat it as a literal `b`, and bash
//!   inherits the extension. Another libc (BSD/macOS) may differ.
//! * Backreferences such as `(a)\1` match in ERE mode, also a GNU extension;
//!   POSIX leaves backreferences undefined for ERE.
//! * Redundant quantifiers (`a**`) compile, and a stray `)` is a literal.
//!
//! # `unsafe`
//!
//! This is the one module of `bsh-sys` that owns an opaque C allocation. The
//! invariant that keeps it sound is stated on [`Regex`]: a `Regex` exists only
//! after a successful `regcomp`, and `Drop` calls `regfree` exactly once.

use std::fmt;
use std::mem::MaybeUninit;

use crate::bytes::cstring;

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

/// A compiled POSIX extended regular expression.
///
/// # Invariant
///
/// `compiled` is an initialised `regex_t` that `regcomp` accepted and that
/// `regfree` has not yet been called on. [`Regex::new`] is the only
/// constructor: it either returns a `Regex` carrying that invariant or an
/// error, in which case nothing is owned and `regfree` is never called.
/// `Regex` is deliberately not `Clone`/`Copy`, so `Drop` — the only place
/// that frees — runs exactly once per successful compilation.
///
/// The compiled form holds raw pointers, so `Regex` is neither `Send` nor
/// `Sync`, which matches its use: one shell thread, one pattern, matched
/// repeatedly.
pub struct Regex {
    /// The live compiled form. See the type-level invariant.
    compiled: libc::regex_t,
    /// Parenthesised subexpressions scanned from the pattern source.
    groups: usize,
}

impl Regex {
    /// Compile `pattern` as a POSIX extended regular expression.
    ///
    /// This is `regcomp` with `REG_EXTENDED` only; see the module docs for why
    /// `REG_NEWLINE` is not set. A pattern containing an embedded NUL cannot be
    /// passed to the C engine and is rejected with [`Error::NulInPattern`]
    /// rather than truncated.
    ///
    /// A pattern with more than `MAX_GROUPS - 1` parenthesised subexpressions
    /// still compiles; [`Regex::captures`] reports the first [`MAX_GROUPS`]
    /// slots and the rest are dropped (see [`Regex::group_count`]).
    pub fn new(pattern: &[u8]) -> Result<Self, Error> {
        // Count before compiling: the scan is pure, so nothing can leak even
        // if it were to panic (it cannot).
        let groups = count_subexpressions(pattern);
        let pattern = cstring(pattern).map_err(|_| Error::NulInPattern)?;

        let mut compiled = MaybeUninit::<libc::regex_t>::uninit();
        // SAFETY: `pattern` is NUL-terminated and lives for the call. On
        // success `regcomp` initialises the storage behind `compiled`; on
        // failure the storage stays uninitialised and is never turned into a
        // `Regex`, so `regfree` cannot observe it.
        let code =
            unsafe { libc::regcomp(compiled.as_mut_ptr(), pattern.as_ptr(), libc::REG_EXTENDED) };
        if code != 0 {
            return Err(Error::Posix { code });
        }
        // SAFETY: `regcomp` returned 0, so it initialised the value. Moving it
        // out of `MaybeUninit` transfers ownership to the `Regex`, whose
        // invariant it now satisfies.
        let compiled = unsafe { compiled.assume_init() };
        Ok(Self { compiled, groups })
    }

    /// The number of parenthesised subexpressions in the pattern.
    ///
    /// This is the count bash exposes as `re_nsub`. [`Regex::captures`]
    /// returns `min(group_count() + 1, MAX_GROUPS)` entries: the whole match
    /// plus one slot per group. POSIX provides no way to read `re_nsub` back
    /// out of a `regex_t`, so the count is scanned from the pattern source
    /// (an unescaped `(` outside a bracket expression); the pattern must have
    /// compiled for the scan to be used.
    pub fn group_count(&self) -> usize {
        self.groups
    }

    /// Match anywhere in `text`, returning the capture ranges in bytes.
    ///
    /// `regexec` searches, so this is not anchored: `"b"` matches `"abc"` at
    /// `(1, 2)`. Index 0 is the whole match. A group that did not participate
    /// in the match is `None`; unused slots are not reported at all, so the
    /// vector has `min(group_count() + 1, MAX_GROUPS)` entries.
    ///
    /// Returns `None` when there is no match, when `text` contains an embedded
    /// NUL (the C engine cannot represent the subject; see
    /// [`Regex::try_captures`] for the error), or when `regexec` reports a
    /// `REG_*` failure of its own. Use [`Regex::try_captures`] to distinguish
    /// "did not match" from "could not search".
    pub fn captures(&self, text: &[u8]) -> Option<CaptureRanges> {
        self.try_captures(text).ok().flatten()
    }

    /// [`Regex::captures`], but a failure to run the search is an error.
    ///
    /// `Ok(None)` means the pattern did not match, which is not an error in
    /// POSIX. `Err(Error::NulInText)` means the subject contains an embedded
    /// NUL, and `Err(Error::Posix { .. })` is a `REG_*` failure from
    /// `regexec` (for example `REG_ESPACE` when memory runs out).
    pub fn try_captures(&self, text: &[u8]) -> Result<Option<CaptureRanges>, Error> {
        let text = cstring(text).map_err(|_| Error::NulInText)?;
        let mut matches = vec![
            libc::regmatch_t {
                rm_so: -1,
                rm_eo: -1,
            };
            self.slots()
        ];
        // SAFETY: `text` is NUL-terminated for the call; `matches` has
        // `self.slots()` initialised entries and `regexec` writes at most that
        // many; neither pointer is retained after the call.
        let code = unsafe {
            libc::regexec(
                &self.compiled,
                text.as_ptr(),
                matches.len(),
                matches.as_mut_ptr(),
                0,
            )
        };
        if code == libc::REG_NOMATCH {
            return Ok(None);
        }
        if code != 0 {
            return Err(Error::Posix { code });
        }
        Ok(Some(
            matches
                .into_iter()
                .map(|matched| {
                    // POSIX leaves unused entries at -1. A broken engine could
                    // also report an inverted range; treat both as "did not
                    // participate" rather than handing a caller an offset that
                    // would panic when used to slice.
                    if matched.rm_so < 0 || matched.rm_eo < matched.rm_so {
                        None
                    } else {
                        Some((matched.rm_so as usize, matched.rm_eo as usize))
                    }
                })
                .collect(),
        ))
    }

    /// The number of `regmatch_t` slots to request from `regexec`.
    fn slots(&self) -> usize {
        self.groups.saturating_add(1).min(MAX_GROUPS)
    }
}

impl Drop for Regex {
    fn drop(&mut self) {
        // SAFETY: the type-level invariant says `compiled` was initialised by
        // a successful `regcomp` and has not been freed. `drop` runs at most
        // once for a value, and `Regex` cannot be copied, so this `regfree`
        // is the exactly-once release of that allocation. Values from a failed
        // `regcomp` never became a `Regex` and are never passed here.
        unsafe { libc::regfree(&mut self.compiled) };
    }
}

impl fmt::Debug for Regex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Regex")
            .field("group_count", &self.groups)
            .finish_non_exhaustive()
    }
}

/// Why compiling a pattern or running a search failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The pattern contains an embedded NUL byte.
    ///
    /// `regcomp` takes a C string, so the pattern cannot be expressed at all.
    /// A shell must reject it rather than silently compiling a prefix.
    NulInPattern,
    /// The text to search contains an embedded NUL byte.
    ///
    /// `regexec` takes a C string, so searching would silently stop at the
    /// NUL. [`Regex::captures`] gives up (`None`); [`Regex::try_captures`]
    /// reports this error.
    NulInText,
    /// The platform engine returned a `REG_*` code.
    Posix {
        /// The `REG_*` code returned by `regcomp` or `regexec`.
        code: libc::c_int,
    },
}

impl Error {
    /// The raw `REG_*` code, or `None` for the embedded-NUL variants.
    pub fn code(&self) -> Option<libc::c_int> {
        match self {
            Self::Posix { code } => Some(*code),
            Self::NulInPattern | Self::NulInText => None,
        }
    }

    /// A human-readable message, never empty.
    pub fn message(&self) -> &str {
        match self {
            Self::NulInPattern => "embedded NUL byte in a regular expression",
            Self::NulInText => "embedded NUL byte in the text searched for a regular expression",
            Self::Posix { .. } => "regular expression error",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for Error {}

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
/// expression is unterminated, which only happens for patterns `regcomp`
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

