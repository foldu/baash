//! Bracket expressions: the literal-`]`, literal-`-`, escape and range rules.
//!
//! Every expectation was checked against bash 5.3.20 unless the comment says
//! otherwise. These are the corners a hand-written parser gets wrong.

use crate::{Flags, matches};

fn hit(pattern: &[u8], text: &[u8]) -> bool {
    matches(pattern, text, Flags::default())
}

fn miss(pattern: &[u8], text: &[u8]) {
    assert!(
        !hit(pattern, text),
        "expected no match: pattern={:?} text={:?}",
        String::from_utf8_lossy(pattern),
        String::from_utf8_lossy(text)
    );
}

fn yes(pattern: &[u8], text: &[u8]) {
    assert!(
        hit(pattern, text),
        "expected a match: pattern={:?} text={:?}",
        String::from_utf8_lossy(pattern),
        String::from_utf8_lossy(text)
    );
}

#[test]
fn closing_bracket_first_is_literal() {
    yes(b"[]]", b"]");
    miss(b"[]]", b"[");
    miss(b"[]]", b"a");
    // `[!]]` negates the singleton `]`.
    miss(b"[!]]", b"]");
    yes(b"[!]]", b"a");
    yes(b"[^]]", b"a");
    miss(b"[^]]", b"]");
    // `[]a]` contains `]` and `a`.
    yes(b"[]a]", b"]");
    yes(b"[]a]", b"a");
    // `[]-a]` parses as the range `]`..`a`, so `-` (0x2d) is *below* it.
    yes(b"[]-a]", b"]");
    yes(b"[]-a]", b"a");
    miss(b"[]-a]", b"-");
    // `[]` is not a closed bracket at all: both bytes are literal.
    yes(b"[]", b"[]");
    miss(b"[]", b"[");
    miss(b"[]", b"]");
}

#[test]
fn dash_is_literal_first_or_last() {
    yes(b"[a-]", b"-");
    yes(b"[a-]", b"a");
    miss(b"[a-]", b"b");
    yes(b"[-a]", b"-");
    yes(b"[-a]", b"a");
    miss(b"[-a]", b"b");
    // A range with a descending endpoint matches nothing.
    miss(b"[z-a]", b"z");
    miss(b"[z-a]", b"a");
    miss(b"[z-a]", b"m");
    // Two dashes: `[a-b-c]` is a-b, `-`, c.
    yes(b"[a-b-c]", b"a");
    yes(b"[a-b-c]", b"b");
    yes(b"[a-b-c]", b"-");
    yes(b"[a-b-c]", b"c");
    miss(b"[a-b-c]", b"d");
    // `-` used as a range endpoint.
    yes(b"[--0]", b".");
    yes(b"[--0]", b"/");
    miss(b"[--0]", b",");
    yes(b"[--0]", b"0");
    miss(b"[--0]", b"1");
}

#[test]
fn unterminated_bracket_is_a_literal() {
    yes(b"[abc", b"[abc");
    miss(b"[abc", b"a");
    miss(b"[abc", b"[ab");
    yes(b"[", b"[");
    miss(b"[", b"a");
    yes(b"[a", b"[a");
    // A bracket whose only possible `]` is the first byte is also unterminated.
    yes(b"[!", b"[!");
}

#[test]
fn escapes_inside_brackets() {
    // `\]` is a member, and the next `]` closes.
    yes(b"[\\]]", b"]");
    miss(b"[\\]]", b"\\");
    // `[\]a]` contains `]` and `a`.
    yes(b"[\\]a]", b"]");
    yes(b"[\\]a]", b"a");
    // `[a\]b]` contains a, ], b.
    yes(b"[a\\]b]", b"a");
    yes(b"[a\\]b]", b"]");
    yes(b"[a\\]b]", b"b");
    // `\-` is a literal dash, not a range separator.
    yes(b"[a\\-z]", b"a");
    yes(b"[a\\-z]", b"-");
    yes(b"[a\\-z]", b"z");
    miss(b"[a\\-z]", b"m");
    // `[[]` contains `[`.
    yes(b"[[]", b"[");
    miss(b"[[]", b"]");
    yes(b"[a[]", b"[");
    yes(b"[a[]", b"a");
}

#[test]
fn bracket_literals_in_a_sequence() {
    yes(b"a[bc]d", b"abd");
    yes(b"a[bc]d", b"acd");
    miss(b"a[bc]d", b"ad");
    miss(b"a[bc]d", b"abcd");
    yes(b"*[0-9]", b"file7");
    miss(b"*[0-9]", b"filex");
    // Default byte mode: a bracket is one byte, so a two-byte character is
    // not one bracket match. `unicode.rs` pins the UTF-8-mode versions.
    miss("[é]".as_bytes(), "é".as_bytes());
    miss("[é]".as_bytes(), "e".as_bytes());
    miss("é".as_bytes(), "e".as_bytes());
    miss("[à-ÿ]".as_bytes(), "é".as_bytes());
    miss("[à-ÿ]".as_bytes(), "a".as_bytes());
    miss("[!é]".as_bytes(), "é".as_bytes());
    yes("[!é]".as_bytes(), b"x");
}

#[test]
fn unknown_class_matches_nothing() {
    // bash 5.3.20: `[[:bogus:]]` matches none of these.
    for text in [&b"a"[..], b"[", b":", b"]", b"[:bogus:]"] {
        miss(b"[[:bogus:]]", text);
    }
    // Negating a never-matching item accepts everything, as bash does.
    yes(b"[![:bogus:]]", b"a");
    yes(b"[![:bogus:]]", b"[");
}

#[test]
fn bracket_and_pathname() {
    let path = Flags {
        pathname: true,
        ..Flags::default()
    };
    // A bracket never matches `/`, even when it lists it: POSIX says a slash
    // in the pattern is matched only by a slash in the string, so the `/` has
    // to be written outside the brackets.
    assert!(!matches(b"[a/]", b"/", path));
    assert!(matches(b"[a/]", b"a", path));
    assert!(!matches(b"[/]", b"/", path));
    // A negated bracket also cannot match `/`.
    assert!(!matches(b"[!a]", b"/", path));
    assert!(!matches(b"[^a]", b"/", path));
    // A literal `/` still matches.
    assert!(matches(b"/", b"/", path));
    assert!(matches(b"a/b", b"a/b", path));
}

#[test]
fn bracket_with_nocasematch() {
    let nocase = Flags {
        nocasematch: true,
        ..Flags::default()
    };
    assert!(matches(b"[a-c]", b"B", nocase));
    assert!(matches(b"[A-C]", b"b", nocase));
    assert!(!matches(b"[a-c]", b"D", nocase));
    // Folding range endpoints makes `[A-z]` behave as `[a-z]`: `_` (0x5f) is
    // between `A` and `z` in byte order but not in the folded range.
    assert!(matches(b"[A-z]", b"a", nocase));
    assert!(!matches(b"[A-z]", b"_", nocase));
}
