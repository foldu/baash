//! `Regex`: the POSIX ERE engine behind `[[ x =~ re ]]` and `BASH_REMATCH`.
//!
//! The expectations here double as the contract for the VM: byte offsets,
//! leftmost-longest selection, capture slots (including non-participating
//! groups), and the documented NUL/group-count limits.

use bsh_sys::regex::MAX_GROUPS;
use bsh_sys::{Regex, RegexError};

/// Compile a pattern that must be valid.
fn compile(pattern: &[u8]) -> Regex {
    Regex::new(pattern).unwrap_or_else(|error| {
        panic!(
            "pattern {:?} should compile: {error}",
            String::from_utf8_lossy(pattern)
        )
    })
}

#[test]
fn a_literal_matches_and_a_literal_does_not() {
    let re = compile(b"abc");
    assert_eq!(re.captures(b"xxabcyy"), Some(vec![Some((2, 5))]));
    assert_eq!(re.captures(b"ab"), None);
    assert_eq!(re.captures(b"ABC"), None, "matching is case-sensitive");
}

#[test]
fn the_search_is_not_anchored() {
    // `regexec` searches: this is what makes `[[ x =~ b ]]` find the middle
    // of a word and `[[ x =~ ^b ]]` not.
    let re = compile(b"b");
    assert_eq!(re.captures(b"abc"), Some(vec![Some((1, 2))]));
}

#[test]
fn caret_and_dollar_anchor_the_whole_subject() {
    let anchored = compile(b"^abc$");
    assert_eq!(anchored.captures(b"abc"), Some(vec![Some((0, 3))]));
    assert_eq!(anchored.captures(b"xabc"), None);
    assert_eq!(anchored.captures(b"abcx"), None);

    // `^` alone matches the empty string at the start; `$` at the end.
    assert_eq!(compile(b"^").captures(b"abc"), Some(vec![Some((0, 0))]));
    assert_eq!(compile(b"$").captures(b"abc"), Some(vec![Some((3, 3))]));
    assert_eq!(compile(b"^$").captures(b""), Some(vec![Some((0, 0))]));
}

#[test]
fn reg_newline_is_not_set() {
    // bash does not pass `REG_NEWLINE`, so `^` does not match after a newline,
    // `.` does match a newline, and a negated class matches a newline. If this
    // ever changes, the shell's `=~` would silently disagree with bash on
    // multi-line values.
    assert_eq!(compile(b"^b").captures(b"a\nb"), None);
    assert_eq!(compile(b"a.b").captures(b"a\nb"), Some(vec![Some((0, 3))]));
    assert_eq!(compile(b"a$").captures(b"a\n"), None);
    assert_eq!(compile(b"[^x]").captures(b"\n"), Some(vec![Some((0, 1))]));
}

#[test]
fn alternation_and_quantifiers() {
    assert_eq!(
        compile(b"cat|dog").captures(b"hotdog"),
        Some(vec![Some((3, 6))])
    );
    assert_eq!(compile(b"ab+").captures(b"abbb"), Some(vec![Some((0, 4))]));
    assert_eq!(compile(b"ab+").captures(b"a"), None);
    assert_eq!(
        compile(b"colou?r").captures(b"color"),
        Some(vec![Some((0, 5))])
    );
    assert_eq!(
        compile(b"^a{2,3}$").captures(b"aa"),
        Some(vec![Some((0, 2))])
    );
    assert_eq!(
        compile(b"^a{2,3}$").captures(b"aaa"),
        Some(vec![Some((0, 3))])
    );
    assert_eq!(compile(b"^a{2,3}$").captures(b"a"), None);
    assert_eq!(compile(b"^a{2,3}$").captures(b"aaaa"), None);
}

#[test]
fn bracket_ranges_and_posix_classes() {
    assert_eq!(
        compile(b"^[a-z]+$").captures(b"hello"),
        Some(vec![Some((0, 5))])
    );
    assert_eq!(compile(b"^[a-z]+$").captures(b"Hello"), None);
    assert_eq!(
        compile(b"^[[:digit:]]+$").captures(b"12345"),
        Some(vec![Some((0, 5))])
    );
    assert_eq!(compile(b"^[[:digit:]]+$").captures(b"12a"), None);
    // A range and a class in the same expression.
    assert_eq!(
        compile(b"^[0-9[:lower:]]+$").captures(b"a1b2"),
        Some(vec![Some((0, 4))])
    );
}

#[test]
fn captures_report_groups_and_non_participating_groups() {
    let re = compile(b"(a)(b)?(c)");
    assert_eq!(re.group_count(), 3);
    // The vector always has one slot per group plus the whole match, even
    // when a group in the middle did not participate.
    assert_eq!(
        re.captures(b"ac"),
        Some(vec![Some((0, 2)), Some((0, 1)), None, Some((1, 2))])
    );
    assert_eq!(
        re.captures(b"abc"),
        Some(vec![Some((0, 3)), Some((0, 1)), Some((1, 2)), Some((2, 3))])
    );

    // Nested groups are numbered by opening parenthesis.
    let nested = compile(b"(a(b)c)");
    assert_eq!(nested.group_count(), 2);
    assert_eq!(
        nested.captures(b"abc"),
        Some(vec![Some((0, 3)), Some((0, 3)), Some((1, 2))])
    );

    // A group in an alternative that did not match is `None`, but the slot
    // after it is still reported.
    let alternatives = compile(b"(a)|(b)");
    assert_eq!(alternatives.group_count(), 2);
    assert_eq!(
        alternatives.captures(b"b"),
        Some(vec![Some((0, 1)), None, Some((0, 1))])
    );
}

#[test]
fn posix_selection_is_leftmost_longest() {
    // A leftmost-first engine would take `a` here; POSIX takes the longest
    // alternative at the leftmost position. This is the property that made a
    // Rust regex crate unusable for `=~`.
    assert_eq!(compile(b"a|ab").captures(b"ab"), Some(vec![Some((0, 2))]));
}

#[test]
fn a_backreference_matches_like_bash() {
    // glibc accepts backreferences in ERE mode (a GNU extension; POSIX leaves
    // them undefined for ERE). bash inherits that, so `=~` must too.
    let re = compile(b"(a)\\1");
    assert_eq!(re.captures(b"aa"), Some(vec![Some((0, 2)), Some((0, 1))]));
    assert_eq!(re.captures(b"ab"), None);
}

#[test]
fn nocasematch_folds_case_through_reg_icase() {
    // `shopt -s nocasematch` is `REG_ICASE`, and the conformance suite checks
    // `[[ a =~ A ]]` with it. Doing this by lowercasing the pattern would
    // change bracket ranges, so the engine has to do the folding.
    assert_eq!(compile(b"abc").captures(b"ABC"), None);
    let re = Regex::new_icase(b"abc").expect("valid pattern");
    assert_eq!(re.captures(b"ABC"), Some(vec![Some((0, 3))]));
    let range = Regex::new_icase(b"^[a-z]+$").expect("valid pattern");
    assert_eq!(range.captures(b"ABC"), Some(vec![Some((0, 3))]));
    // The group count and limits are unaffected by the flag.
    assert_eq!(range.group_count(), 0);
}

#[test]
fn a_compile_failure_carries_a_non_empty_message() {
    let error = Regex::new(b"(a").expect_err("`(a` is not a valid ERE");
    match &error {
        RegexError::Posix { .. } => {}
        other => panic!("expected a POSIX error, got {other:?}"),
    }
    assert!(error.code().is_some(), "the REG_* code is preserved");
    assert!(
        !error.message().is_empty(),
        "a user typo needs a message, not just a code"
    );
    assert!(!error.to_string().is_empty());

    let class = Regex::new(b"[[:notaclass:]]").expect_err("unknown class");
    assert!(!class.message().is_empty());
}

#[test]
fn nul_in_the_pattern_is_rejected() {
    // `regcomp` takes a C string. Truncating would compile a different
    // pattern, so the only honest answer is an error.
    assert_eq!(Regex::new(b"a\0b").unwrap_err(), RegexError::NulInPattern);
    assert_eq!(Regex::new(b"\0").unwrap_err(), RegexError::NulInPattern);
    assert_eq!(
        RegexError::NulInPattern.message(),
        "embedded NUL byte in a regular expression"
    );
}

#[test]
fn nul_in_the_text_gives_up() {
    // `regexec` cannot represent such a subject either. `captures` gives up
    // (None, like a non-match) and `try_captures` says why.
    let re = compile(b"a");
    assert_eq!(re.captures(b"x\0a"), None);
    assert_eq!(re.captures(b"\0a"), None);
    assert_eq!(re.captures(b"a\0"), None);
    assert_eq!(re.try_captures(b"x\0a"), Err(RegexError::NulInText));
    // The same subject without the NUL still matches, so the rejection is
    // about representation, not about the pattern.
    assert_eq!(re.captures(b"xa"), Some(vec![Some((1, 2))]));
}

#[test]
fn capture_offsets_are_bytes_not_characters() {
    // "h\u{e9}llo" is six bytes: h(0), the two bytes of é(1,2), l(3), l(4),
    // o(5). The character index of `llo` would be 2; the byte offset is 3.
    let re = compile(b"llo");
    assert_eq!(re.captures("héllo".as_bytes()), Some(vec![Some((3, 6))]));
}

#[test]
fn groups_beyond_the_limit_are_truncated() {
    // The pattern compiles and matches; only capture slots are bounded.
    let groups = MAX_GROUPS + 6;
    let pattern = "(a)".repeat(groups);
    let text = "a".repeat(groups);
    let re = compile(pattern.as_bytes());
    assert_eq!(re.group_count(), groups);
    let captures = re.captures(text.as_bytes()).expect("must match");
    assert_eq!(captures.len(), MAX_GROUPS);
    assert!(
        captures.iter().all(Option::is_some),
        "every reported group participated"
    );
}

#[test]
fn an_empty_match_is_reported_at_its_position() {
    let re = compile(b"x*");
    assert_eq!(re.captures(b"abc"), Some(vec![Some((0, 0))]));
}

// The loop *is* the test: it exercises compile/match/drop, including the
// failed-compile path that must never reach `regfree`.
#[allow(clippy::regex_creation_in_loops)]
#[test]
fn compiling_and_dropping_many_regexes_is_stable() {
    // No leak checker here; the point is that `Drop`'s `regfree` path is
    // exercised repeatedly, including for failed compilations, which own
    // nothing and must not reach `regfree`.
    for i in 0..10_000 {
        let pattern = format!("^(x{})(y)?(z*)$", i);
        let re = compile(pattern.as_bytes());
        assert_eq!(re.group_count(), 3);
        let text = format!("x{}z", i);
        assert_eq!(
            re.captures(text.as_bytes()),
            Some(vec![
                Some((0, text.len())),
                Some((0, 1 + i.to_string().len())),
                None,
                Some((1 + i.to_string().len(), text.len())),
            ])
        );
        if i % 16 == 0 {
            assert!(Regex::new(b"a{1").is_err(), "failed compiles own nothing");
        }
    }
}
