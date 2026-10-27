//! `${x^pat}`, `${x^^pat}`, `${x,pat}`, `${x,,pat}`: case conversion.
//!
//! bash matches the pattern against one character at a time, and in `LC_ALL=C`
//! a character is a byte with only ASCII case mappings. Every expected value
//! was checked against bash 5.3.20 under `LC_ALL=C`.

use crate::fixture::TestShell;

use super::{assert_argv, expand_one, expand_one_bytes, values};

fn one(src: &str, shell: &mut TestShell) -> Vec<Vec<u8>> {
    expand_one(src, shell)
}

#[test]
fn case_operators_match_one_character_at_a_time() {
    let mut shell = TestShell::new().var(b"x", b"abcABC123");
    assert_argv(&one("\"${x^^}\"", &mut shell), &["ABCABC123"], "${x^^}");
    assert_argv(
        &one("\"${x^^[a-c]}\"", &mut shell),
        &["ABCABC123"],
        "${x^^[a-c]}",
    );
    // bash matches the pattern against one character, not against the rest of
    // the value: `*[bB]` matches a single `b`/`B`.
    assert_argv(
        &one("\"${x^^*[bB]}\"", &mut shell),
        &["aBcABC123"],
        "${x^^*[bB]}",
    );
    // A pattern longer than one character can never match.
    assert_argv(
        &one("\"${x^^?b?}\"", &mut shell),
        &["abcABC123"],
        "${x^^?b?}",
    );
    // `^` and `,` only consider the first character.
    let mut shell = TestShell::new().var(b"x", b"abc");
    assert_argv(&one("\"${x^[a]}\"", &mut shell), &["Abc"], "${x^[a]}");
    let mut shell = TestShell::new().var(b"x", b"bcd");
    assert_argv(
        &one("\"${x^[a]}\"", &mut shell),
        &["bcd"],
        "${x^[a]} no match",
    );
    let mut shell = TestShell::new().var(b"x", b"1abc");
    assert_argv(&one("\"${x^}\"", &mut shell), &["1abc"], "${x^} on a digit");
    // An explicitly empty pattern matches no character; an absent one matches
    // every character.
    let mut shell = TestShell::new().var(b"x", b"ab");
    assert_argv(&one("\"${x^^}\"", &mut shell), &["AB"], "${x^^}");
    assert_argv(&one("\"${x^^\"\"}\"", &mut shell), &["ab"], "${x^^\"\"}");
}

#[test]
fn case_operators_are_ascii_and_honour_quoting() {
    // Only ASCII has a case mapping: the two bytes of `μ` are left alone, and
    // `?` matches one byte at a time because the shell is in the C locale.
    let mut shell = TestShell::new().var(b"x", "μabc".as_bytes());
    assert_eq!(
        expand_one_bytes(b"\"${x^^}\"", &mut shell),
        values(&["μABC".as_bytes()])
    );
    // A quoted `?` is literal, so only the `?` itself is considered.
    let mut shell = TestShell::new().var(b"x", b"a?b").var(b"p", b"?");
    assert_argv(&one("\"${x^^\"$p\"}\"", &mut shell), &["a?b"], "quoted `?`");
    // An unquoted expansion is a pattern: `?` matches every character.
    let mut shell = TestShell::new().var(b"x", b"a?b").var(b"p", b"?");
    assert_argv(&one("\"${x^^$p}\"", &mut shell), &["A?B"], "unquoted `?`");
}
