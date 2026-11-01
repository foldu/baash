//! The core (pattern, text, expected) table.
//!
//! Expectations here were checked against bash 5.3.20 where the behaviour is
//! non-obvious; the comments say so where it matters. Non-ASCII literals are
//! spelled with `str` because `b"μ"` is not a byte string literal.

use crate::{Flags, matches};

const OFF: Flags = Flags {
    extglob: false,
    nocasematch: false,
    pathname: false,
    utf8: false,
};
const EXT: Flags = Flags {
    extglob: true,
    nocasematch: false,
    pathname: false,
    utf8: false,
};

fn check(flags: Flags, cases: &[(&[u8], &[u8], bool)]) {
    for &(pattern, text, expected) in cases {
        assert_eq!(
            matches(pattern, text, flags),
            expected,
            "pattern={pattern:02x?} text={text:02x?} extglob={} nocase={} pathname={}",
            flags.extglob,
            flags.nocasematch,
            flags.pathname
        );
    }
}

#[test]
fn empty_pattern_and_subject() {
    check(
        OFF,
        &[
            (b"", b"", true),
            (b"", b"a", false),
            (b"", b"*", false),
            (b"a", b"", false),
            (b"*", b"", true),
            (b"**", b"", true),
            (b"?", b"", false),
            (b"[a]", b"", false),
            (b"a*", b"", false),
        ],
    );
}

#[test]
fn stars_match_anything_including_empty() {
    check(
        OFF,
        &[
            (b"*", b"a", true),
            (b"*", b"abc", true),
            (b"*", b"/", true), // string mode: `*` crosses `/`
            (b"*", "μ".as_bytes(), true),
            (b"*", &[0xFF], true),
            (b"**", b"abc", true),
            (b"a*", b"a", true),
            (b"a*", b"abc", true),
            (b"a*", b"ba", false),
            (b"*a", b"a", true),
            (b"*a", b"ba", true),
            (b"*a", b"ab", false),
            (b"*a*", b"a", true),
            (b"*a*", b"xay", true),
            (b"*a*", b"xy", false),
            (b"a*b*c", b"abc", true),
            (b"a*b*c", b"aXbYc", true),
            (b"a*b*c", b"ac", false),
            (b"a*b*c", b"ab", false),
        ],
    );
}

#[test]
fn question_matches_exactly_one_character() {
    check(
        OFF,
        &[
            (b"?", b"a", true),
            (b"?", b"", false),
            (b"?", b"ab", false),
            (b"?", b"/", true),
            (b"a?c", b"abc", true),
            (b"a?c", b"ac", false),
            (b"a?c", b"abbc", false),
            // Byte mode (the default): each byte of `μ` is one unit.
            // `unicode.rs` pins the UTF-8-mode expectations.
            (b"?", "μ".as_bytes(), false),
            (b"??", "μ".as_bytes(), true),
            (b"??", "μa".as_bytes(), false),
            (b"a?c", "aμc".as_bytes(), false),
            // An invalid byte is a character of its own.
            (b"?", &[0xFF], true),
            (b"??", &[0xFF], false),
        ],
    );
}

#[test]
fn literals_are_literal() {
    check(
        OFF,
        &[
            (b"abc", b"abc", true),
            (b"abc", b"ab", false),
            (b"abc", b"abcd", false),
            (b"ABC", b"abc", false),
            (b"a.c", b"abc", false), // `.` is not a wildcard here
            (b"a.c", b"a.c", true),
            (b"a|b", b"a|b", true), // top-level `|` is literal even with extglob
            (b"a|b", b"a", false),
            ("é".as_bytes(), "é".as_bytes(), true),
            ("é".as_bytes(), "e".as_bytes(), false),
            (&[0xFF], &[0xFF], true),
            (&[0xFF], &[0xFE], false),
        ],
    );
}

#[test]
fn escaped_metacharacters_are_literal() {
    check(
        OFF,
        &[
            (b"\\*", b"*", true),
            (b"\\*", b"a", false),
            (b"\\?", b"?", true),
            (b"\\?", b"a", false),
            (b"\\[", b"[", true),
            (b"\\]", b"]", true),
            (b"\\|", b"|", true),
            (b"\\a", b"a", true),
            (b"\\\\", b"\\", true),
            (b"a\\*b", b"a*b", true),
            (b"a\\*b", b"axb", false),
            (b"\\\\*", b"\\anything", true),
            // A trailing backslash is itself literal.
            (b"a\\", b"a\\", true),
            (b"a\\", b"a", false),
            // Escaping a multi-byte character leaves it literal.
            ("\\μ".as_bytes(), "μ".as_bytes(), true),
        ],
    );
}

#[test]
fn extglob_off_means_operators_are_literal() {
    check(
        OFF,
        &[
            (b"@(a|b)", b"@(a|b)", true),
            (b"@(a|b)", b"a", false),
            (b"@(a|b)", b"b", false),
            // `?(` is `?` followed by a literal `(`.
            (b"?(a)", b"x(a)", true),
            (b"?(a)", b"a", false),
            // `*(` is `*` followed by a literal `(`.
            (b"*(a)", b"xx(a)", true),
            (b"*(a)", b"aaa", false),
            (b"+(a)", b"+(a)", true),
            (b"+(a)", b"(a)", false),
            (b"!(a)", b"!(a)", true),
            (b"!(a)", b"a", false),
        ],
    );
}

#[test]
fn extglob_on_changes_the_same_patterns() {
    check(
        EXT,
        &[
            (b"@(a|b)", b"a", true),
            (b"@(a|b)", b"b", true),
            (b"@(a|b)", b"@(a|b)", false),
            (b"?(a)", b"", true),
            (b"?(a)", b"a", true),
            (b"?(a)", b"aa", false),
            (b"?(a)", b"x(a)", false),
            (b"*(a)", b"", true),
            (b"*(a)", b"aaa", true),
            (b"*(a)", b"b", false),
            (b"+(a)", b"", false),
            (b"+(a)", b"a", true),
            (b"+(a)", b"aaa", true),
            (b"+(a)", b"aaab", false),
            (b"!(a)", b"", true),
            (b"!(a)", b"a", false),
            (b"!(a)", b"aa", true),
            (b"!(a)", b"b", true),
        ],
    );
}

#[test]
fn extglob_alternatives() {
    check(
        EXT,
        &[
            (b"@()", b"", true),
            (b"@()", b"x", false),
            (b"@(||)", b"", true),
            (b"@(||)", b"|", false),
            (b"@(a|)", b"a", true),
            (b"@(a|)", b"", true),
            (b"@(a|)", b"b", false),
            (b"?(a|b)", b"", true),
            (b"?(a|b)", b"a", true),
            (b"?(a|b)", b"b", true),
            (b"?(a|b)", b"ab", false),
            (b"+(a|b)", b"", false),
            (b"+(a|b)", b"abba", true),
            (b"*(a|b)", b"", true),
            (b"*(a|b)", b"abba", true),
            (b"*(a|b)", b"abc", false),
            (b"+(a|)", b"", true),
            (b"+(a|)", b"aaa", true),
            (b"*(a|)", b"b", false),
            (b"*(a|)", b"aaa", true),
        ],
    );
}

#[test]
fn extglob_nesting_and_adjacency() {
    check(
        EXT,
        &[
            (b"@(a|b)@(c|d)", b"ac", true),
            (b"@(a|b)@(c|d)", b"bd", true),
            (b"@(a|b)@(c|d)", b"ab", false),
            (b"@(--|++)@(help|verbose)", b"--help", true),
            (b"@(--|++)@(help|verbose)", b"++verbose", true),
            (b"@(--|++)@(help|verbose)", b"--nope", false),
            (
                b"--@(help|verbose|no-@(long|short)-option)",
                b"--no-long-option",
                true,
            ),
            (
                b"--@(help|verbose|no-@(long|short)-option)",
                b"--help",
                true,
            ),
            (
                b"--@(help|verbose|no-@(long|short)-option)",
                b"--oops",
                false,
            ),
            (b"@(a|ab)b", b"ab", true),
            (b"@(a|ab)b", b"abb", true),
            (b"@(a|ab)b", b"abbb", false),
            // A group can match the empty string inside a repetition.
            (b"x*()y", b"xy", true),
            (b"x*()y", b"xzy", false),
            (b"x@()y", b"xy", true),
        ],
    );
}

#[test]
fn extglob_repetition_with_globs() {
    check(
        EXT,
        &[
            (b"+(foo)", b"foo", true),
            (b"+(foo)", b"foofoo", true),
            (b"+(foo)", b"foofoo_", false),
            (b"*(foo)", b"", true),
            (b"*(foo)", b"foofoo", true),
            (b"*(foo*)", b"foofoo_foo__foo___", true),
            (b"*(foo*)", b"Xoofoo_foo__foo___", false),
            (b"*(a*)", b"aaa", true),
            // `a*` needs an `a`, so `+` needs at least one.
            (b"+(a*)", b"", false),
        ],
    );
}

#[test]
fn empty_group_after_star_is_the_language_not_the_bash_bug() {
    // bash 5.3.20 fails `[[ abc == *@() ]]` although `@()` matches empty and
    // `@()*`/`x*@()y` work. This crate implements the compositional meaning;
    // see the crate docs. These expectations are deliberately *not* bash's.
    check(
        EXT,
        &[
            (b"*@()", b"", true),
            (b"*@()", b"abc", true),
            (b"@()*", b"abc", true),
            (b"x*@()y", b"xy", true),
            (b"*+(|)", b"abc", true),
            (b"*@()@()", b"abc", true),
        ],
    );
}

#[test]
fn malformed_extglob_falls_back_to_literals() {
    check(
        EXT,
        &[
            (b"@(a", b"@(a", true),
            (b"@(a", b"a", false),
            (b"@(", b"@(", true),
            // The first group is real; the unterminated second group is
            // literal, so this is `a` followed by `@(b`.
            (b"@(a)@(b", b"a@(b", true),
            (b"@(a)@(b", b"@(a)@(b", false),
            // `?*?(` is Any, Star, then a literal `?` and `(` here. bash
            // treats the truncated group as matching nothing; see docs.
            (b"?*?(", b"?*?(", true),
            (b"?*?(", b"", false),
        ],
    );
}

#[test]
fn negation_is_a_span_complement() {
    check(
        EXT,
        &[
            (b"!(a*)", b"", true), // `a*` requires an `a`
            (b"!(a*)", b"b", true),
            (b"!(a*)", b"a", false),
            (b"!(a*)", b"ab", false),
            (b"!(a*)", b"aaa", false),
            (b"!(a|b)", b"", true),
            (b"!(a|b)", b"a", false),
            (b"!(a|b)", b"ab", true),
            (b"!(a|b)", b"c", true),
            (b"!()", b"", false),
            (b"!()", b"x", true),
            (b"!(|)", b"", false),
            (b"!(|)", b"x", true),
            // Complement is an involution: !(!(a)) is exactly `a`.
            (b"!(!(a))", b"a", true),
            (b"!(!(a))", b"", false),
            (b"!(!(a))", b"aa", false),
            (b"!(@(a|b))", b"ab", true),
            (b"!(@(a|b))", b"a", false),
            // The rest of the pattern decides where the negated span ends.
            (b"a!(b)c", b"ac", true),
            (b"a!(b)c", b"abc", false),
            (b"a!(b)c", b"axc", true),
            (b"a!(b)c", b"abbc", true),
        ],
    );
}

#[test]
fn negation_with_trailing_star() {
    check(
        EXT,
        &[
            (b"!(a)*", b"a", true),
            (b"!(a)*", b"", true),
            (b"!(a)*", b"b", true),
            (b"!(a)*", b"ab", true),
            // The `*` can absorb what the negation does not.
            (b"!(a|ab)b", b"abb", false),
        ],
    );
}

#[test]
fn pathname_star_and_question_refuse_slash() {
    let path = Flags {
        pathname: true,
        ..Flags::default()
    };
    check(
        path,
        &[
            (b"*", b"a", true),
            (b"*", b"a/b", false),
            (b"*", b"", true),
            (b"*/*", b"a/b", true),
            (b"*/*", b"a/b/c", false),
            (b"a?b", b"a/b", false),
            (b"a?b", b"axb", true),
            (b"a[b/c]d", b"a/b/d", false),
            (b"a/b", b"a/b", true),
            (b"*/b", b"a/b", true),
            (b"a/*", b"a/b/c", false),
            (b"?/b", b"a/b", true),
            // Byte mode: `?` matches only the first byte of `μ`.
            (b"?/b", "μ/b".as_bytes(), false),
        ],
    );
}

#[test]
fn nocasematch_folds_ascii() {
    let nocase = Flags {
        nocasematch: true,
        ..Flags::default()
    };
    check(
        nocase,
        &[
            (b"ABC", b"abc", true),
            (b"abc", b"ABC", true),
            (b"aBc", b"AbC", true),
            (b"ABC", b"abd", false),
            (b"a*C", b"Abc", true),
            (b"?", b"A", true),
            (b"[]a]", b"A", true),
            (b"[a-z]", b"A", true),
            (b"[A-Z]", b"a", true),
            (b"[!a]", b"A", false),
            (b"[^a]", b"A", false),
            (b"[!a]", b"B", true),
            // ASCII folding only: the two-byte UTF-8 spellings differ in
            // their second byte and are left alone.
            ("É".as_bytes(), "é".as_bytes(), false),
            ("é".as_bytes(), "é".as_bytes(), true),
            // `[:upper:]` is not folded; bash keeps the class test on the raw
            // character (verified on bash 5.3.20).
            (b"[[:upper:]]", b"A", true),
            (b"[[:upper:]]", b"a", false),
            (b"[[:lower:]]", b"A", false),
            (b"[[:lower:]]", b"a", true),
        ],
    );
}

#[test]
fn pattern_object_and_free_functions_agree() {
    use crate::Pattern;
    for flags in [OFF, EXT] {
        let pattern = Pattern::new(b"@(a|ab)c", flags);
        assert_eq!(pattern.flags(), flags);
        for text in [&b"ac"[..], b"abc", b"c", b"aabc"] {
            assert_eq!(
                pattern.matches(text),
                matches(b"@(a|ab)c", text, flags),
                "flags={flags:?} text={text:02x?}"
            );
            for longest in [false, true] {
                assert_eq!(
                    pattern.prefix_end(text, longest),
                    crate::prefix_end(b"@(a|ab)c", text, longest, flags)
                );
                assert_eq!(
                    pattern.suffix_start(text, longest),
                    crate::suffix_start(b"@(a|ab)c", text, longest, flags)
                );
            }
        }
    }
}

#[test]
fn control_and_nul_bytes_are_ordinary() {
    // Shell strings cannot contain NUL in practice, but the API is bytes and
    // the matcher must not treat any byte as special.
    check(
        OFF,
        &[
            (&[0], &[0], true),
            (&[0], &[1], false),
            (b"?", &[0], true),
            (b"*", &[0], true),
            (b"a\x00b", b"a\x00b", true),
            (b"[a\x00]", &[0], true),
            (b"?", b"\n", true),
            (b"*", b"\n", true),
            (b"a?b", b"a\nb", true),
        ],
    );
}
