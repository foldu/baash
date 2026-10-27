//! Extended globs, in one place: the five operators, nesting, escaping and the
//! ways extglob interacts with the other flags.
//!
//! `tables.rs` carries the broad (pattern, text, expected) table; this module
//! is the focused extglob suite, including the cases that are easy to get
//! subtly wrong: `!` as a span complement, empty alternatives, and a group that
//! matches empty inside a repetition.

use crate::{Flags, matches, prefix_end, suffix_start};

const EXT: Flags = Flags {
    extglob: true,
    nocasematch: false,
    pathname: false,
};
const OFF: Flags = Flags {
    extglob: false,
    nocasematch: false,
    pathname: false,
};

fn yes(pattern: &[u8], text: &[u8]) {
    assert!(
        matches(pattern, text, EXT),
        "expected extglob match: pattern={:?} text={:?}",
        String::from_utf8_lossy(pattern),
        String::from_utf8_lossy(text)
    );
}

fn no(pattern: &[u8], text: &[u8]) {
    assert!(
        !matches(pattern, text, EXT),
        "expected extglob non-match: pattern={:?} text={:?}",
        String::from_utf8_lossy(pattern),
        String::from_utf8_lossy(text)
    );
}

#[test]
fn every_operator() {
    // ?: zero or one
    yes(b"?()", b"");
    yes(b"?(a)", b"");
    yes(b"?(a)", b"a");
    no(b"?(a)", b"aa");
    // *: zero or more
    yes(b"*()", b"");
    yes(b"*(a)", b"");
    yes(b"*(a)", b"aaaa");
    no(b"*(a)", b"ab");
    // +: one or more; one occurrence of an empty group is still a match.
    yes(b"+()", b"");
    yes(b"+(a)", b"a");
    yes(b"+(a)", b"aaaa");
    no(b"+(a)", b"");
    // @: exactly one; `@()` is the one empty alternative.
    yes(b"@()", b"");
    no(b"@()", b"x");
    yes(b"@(a|b)", b"a");
    yes(b"@(a|b)", b"b");
    no(b"@(a|b)", b"ab");
    // !: a span complement
    yes(b"!(a)", b"");
    yes(b"!(a)", b"b");
    yes(b"!(a)", b"aa");
    no(b"!(a)", b"a");
    // `!!(a)` is `!` followed by `!(a)`, not double negation.
    no(b"!!(a)", b"");
    no(b"!!(a)", b"a");
    yes(b"!!(a)", b"!b");
}

#[test]
fn empty_alternatives() {
    yes(b"@()", b"");
    no(b"@()", b"x");
    yes(b"@(||)", b"");
    no(b"@(||)", b"|");
    yes(b"@(a|)", b"");
    yes(b"@(a|)", b"a");
    yes(b"@(a||b)", b"b");
    // `!` complements the exact set of alternatives, including the empty one.
    no(b"!()", b"");
    yes(b"!()", b"x");
    no(b"!(|)", b"");
    yes(b"!(|)", b"x");
}

#[test]
fn nested_groups() {
    yes(b"@(a|@(b|c))", b"a");
    yes(b"@(a|@(b|c))", b"c");
    no(b"@(a|@(b|c))", b"d");
    yes(b"!(@(a|b))", b"");
    yes(b"!(@(a|b))", b"c");
    no(b"!(@(a|b))", b"a");
    no(b"!(@(a|b))", b"b");
    // Complement is an involution.
    yes(b"!(!(a))", b"a");
    no(b"!(!(a))", b"");
    no(b"!(!(a))", b"aa");
    // A group inside a group's alternative.
    yes(b"@(x|y@(1|2))", b"x");
    yes(b"@(x|y@(1|2))", b"y2");
    no(b"@(x|y@(1|2))", b"y3");
}

#[test]
fn escaping_inside_groups() {
    yes(b"@(a\\|b)", b"a|b");
    no(b"@(a\\|b)", b"a");
    no(b"@(a\\|b)", b"b");
    yes(b"@(a\\)b)", b"a)b");
    no(b"@(a\\)b)", b"ab");
    yes(b"@(a\\(b)", b"a(b");
    // Escaped extglob operators are literal, even with the flag on.
    yes(b"\\@(a)", b"@(a)");
    no(b"\\@(a)", b"a");
    yes(b"\\!(x)", b"!(x)");
    // An escaped backslash then a real group.
    yes(b"\\\\@(a)", b"\\a");
}

#[test]
fn negation_is_anchored_to_its_span() {
    // `!(a)` in the middle: the rest of the pattern decides the span.
    yes(b"a!(b)c", b"ac");
    yes(b"a!(b)c", b"axc");
    yes(b"a!(b)c", b"abbc");
    no(b"a!(b)c", b"abc");
    no(b"a!(b)c", b"aXcY");
    // The negated span can be empty.
    yes(b"!(a)b", b"b");
    yes(b"!(a)b", b"xb");
    no(b"!(a)b", b"ab");
    // A trailing star can absorb what the negation rejects.
    yes(b"!(a)*", b"a");
    yes(b"!(a)*", b"abc");
    // `!` is not a lookahead: it must consume the characters.
    no(b"!(a)a", b"aa");
    yes(b"!(a)a", b"ba");
}

#[test]
fn repetition_and_empty_matches() {
    // `*(a|)` is `a*`: the empty alternative lets it stop anywhere.
    yes(b"*(a|)", b"");
    yes(b"*(a|)", b"aaa");
    no(b"*(a|)", b"b");
    // `+(a|)` requires one occurrence, but the empty alternative counts.
    yes(b"+(a|)", b"");
    no(b"+(a|)", b"b");
    // A group that matches empty inside a star must not loop forever.
    // `*()` repeats a group that can only match empty, so it matches "".
    yes(b"*()", b"");
    no(b"*()", b"abc");
    yes(b"*(a*)", b"aaa");
    yes(b"*(foo*)", b"foofoo_foo__foo___");
    no(b"*(foo*)", b"Xoofoo_foo__foo___");
    // `?(…)` is one occurrence at most, not a star.
    no(b"?(a|b)", b"ab");
    yes(b"?(a|b)", b"a");
    yes(b"?(a|b)", b"");
}

#[test]
fn extglob_requires_the_flag() {
    // Without the flag the operators are ordinary characters.
    assert!(matches(b"@(a|b)", b"@(a|b)", OFF));
    assert!(!matches(b"@(a|b)", b"a", OFF));
    assert!(matches(b"?(a)", b"x(a)", OFF));
    assert!(!matches(b"?(a)", b"a", OFF));
    assert!(matches(b"*(a)", b"xx(a)", OFF));
    assert!(!matches(b"*(a)", b"aaa", OFF));
    assert!(matches(b"+(a)", b"+(a)", OFF));
    assert!(!matches(b"+(a)", b"a", OFF));
    assert!(matches(b"!(a)", b"!(a)", OFF));
    assert!(!matches(b"!(a)", b"a", OFF));
}

#[test]
fn extglob_with_nocasematch() {
    let flags = Flags {
        extglob: true,
        nocasematch: true,
        pathname: false,
    };
    assert!(matches(b"@(FOO|BAR)", b"bar", flags));
    assert!(matches(b"!(foo)", b"BAR", flags)); // complement of folded "foo"
    assert!(!matches(b"!(foo)", b"FOO", flags));
    assert!(matches(b"+(ab)", b"ABab", flags));
    assert!(!matches(b"+(ab)", b"ABc", flags));
}

#[test]
fn extglob_with_pathname() {
    let flags = Flags {
        extglob: true,
        nocasematch: false,
        pathname: true,
    };
    assert!(matches(b"@(a|b)/c", b"a/c", flags));
    assert!(!matches(b"@(a|b)/c", b"a/b/c", flags));
    assert!(matches(b"@(a|*)/c", b"x/c", flags));
    assert!(!matches(b"@(*)/c", b"x/y/c", flags));
    assert!(matches(b"*(a|b)", b"abba", flags));
    assert!(!matches(b"*(a|b)", b"ab/ba", flags));
}

#[test]
fn extglob_on_multibyte_characters() {
    // The operators are ASCII, but their operands are characters.
    yes("@(μ|é)".as_bytes(), "é".as_bytes());
    yes("@(μ|é)".as_bytes(), "μ".as_bytes());
    no("@(μ|é)".as_bytes(), "e".as_bytes());
    yes("?(μ)".as_bytes(), "μ".as_bytes());
    yes("?(μ)".as_bytes(), b"");
    no("?(μ)".as_bytes(), "μμ".as_bytes());
    yes("+(μ)".as_bytes(), "μμμ".as_bytes());
    // `?` inside a group still means one character.
    yes(b"@(?|??)", "μ".as_bytes());
    no(b"@(?|??)", "μμμ".as_bytes());
    // Negation is over characters.
    no("!(μ)".as_bytes(), "μ".as_bytes());
    yes("!(μ)".as_bytes(), "é".as_bytes());
}

#[test]
fn extglob_in_strip_operators() {
    // The group is part of the pattern; shortest/longest still choose a
    // matching prefix or suffix.
    assert_eq!(prefix_end(b"@(a|ab)", b"abc", false, EXT), Some(1));
    assert_eq!(prefix_end(b"@(a|ab)", b"abc", true, EXT), Some(2));
    assert_eq!(suffix_start(b"@(c|bc)", b"abc", false, EXT), Some(2));
    assert_eq!(suffix_start(b"@(c|bc)", b"abc", true, EXT), Some(1));
    assert_eq!(prefix_end(b"!(a)*", b"abc", false, EXT), Some(0));
    assert_eq!(prefix_end(b"!(a)*", b"abc", true, EXT), Some(3));
    assert_eq!(suffix_start(b"!(a)*", b"abc", false, EXT), Some(3));
    assert_eq!(suffix_start(b"!(a)*", b"abc", true, EXT), Some(0));
}
