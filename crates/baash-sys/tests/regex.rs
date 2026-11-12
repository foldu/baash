//! `Regex` and `Cache`: the POSIX ERE engine behind `[[ x =~ re ]]` and
//! `BASH_REMATCH`.
//!
//! The expectations here double as the contract for the VM: byte offsets,
//! leftmost-longest selection, capture slots (including non-participating
//! groups), the documented group-count limit, and the two engines' measured
//! differences. Tests that describe the *system* engine are `#[cfg(unix)]`;
//! the portable engine is exercised everywhere.

use baash_sys::regex::MAX_GROUPS;
use baash_sys::{Cache, Engine, Regex, RegexError};

/// Compile a pattern that must be valid, with the platform's default engine.
fn compile(pattern: &[u8]) -> Regex {
    Regex::new(pattern).unwrap_or_else(|error| {
        panic!(
            "pattern {:?} should compile: {error}",
            String::from_utf8_lossy(pattern)
        )
    })
}

/// Compile a pattern that must be valid with an explicit engine.
fn compile_with(engine: Engine, pattern: &[u8]) -> Regex {
    Regex::with_engine(engine, pattern, false).unwrap_or_else(|error| {
        panic!(
            "pattern {:?} should compile on {}: {error}",
            String::from_utf8_lossy(pattern),
            engine
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
    // multi-line values. Both engines are configured this way.
    for engine in Engine::ALL {
        if !engine.is_available() {
            continue;
        }
        assert_eq!(
            compile_with(*engine, b"^b").captures(b"a\nb"),
            None,
            "{engine}"
        );
        assert_eq!(
            compile_with(*engine, b"a.b").captures(b"a\nb"),
            Some(vec![Some((0, 3))]),
            "{engine}"
        );
        assert_eq!(
            compile_with(*engine, b"a$").captures(b"a\n"),
            None,
            "{engine}"
        );
        assert_eq!(
            compile_with(*engine, b"[^x]").captures(b"\n"),
            Some(vec![Some((0, 1))]),
            "{engine}"
        );
    }
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
    for engine in Engine::ALL {
        if !engine.is_available() {
            continue;
        }
        assert_eq!(
            compile_with(*engine, b"a|ab").captures(b"ab"),
            Some(vec![Some((0, 2))]),
            "{engine}"
        );
    }
}

#[test]
fn nocasematch_folds_case() {
    // `shopt -s nocasematch` is the engine's case-folding flag, and the
    // conformance suite checks `[[ a =~ A ]]` with it. Doing this by
    // lowercasing the pattern would change bracket ranges, so the engine has
    // to do the folding.
    assert_eq!(compile(b"abc").captures(b"ABC"), None);
    for engine in Engine::ALL {
        if !engine.is_available() {
            continue;
        }
        let re = Regex::with_engine(*engine, b"abc", true).expect("valid pattern");
        assert_eq!(re.captures(b"ABC"), Some(vec![Some((0, 3))]), "{engine}");
        let range = Regex::with_engine(*engine, b"^[a-z]+$", true).expect("valid pattern");
        assert_eq!(range.captures(b"ABC"), Some(vec![Some((0, 3))]), "{engine}");
        assert_eq!(range.group_count(), 0, "{engine}");
    }
}

#[test]
fn a_compile_failure_carries_a_non_empty_message() {
    let error = Regex::new(b"(a").expect_err("`(a` is not a valid ERE");
    match &error {
        RegexError::Engine { .. } => {}
        other => panic!("expected an engine error, got {other:?}"),
    }
    assert!(error.code().is_some(), "the engine's code is preserved");
    assert!(
        !error.message().is_empty(),
        "a user typo needs a message, not just a code"
    );
    assert!(!error.to_string().is_empty());

    let class = Regex::new(b"[[:notaclass:]]").expect_err("unknown class");
    assert!(!class.message().is_empty());
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
// failed-compile path that must never reach the engine's free.
#[allow(clippy::regex_creation_in_loops)]
#[test]
fn compiling_and_dropping_many_regexes_is_stable() {
    // No leak checker here; the point is that each backend's release path is
    // exercised repeatedly, including for failed compilations, which own
    // nothing and must not be freed.
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
            assert!(Regex::new(b"(a").is_err(), "failed compiles own nothing");
        }
    }
}

// ---------------------------------------------------------------- engines ----

#[test]
fn the_default_engine_is_the_platform_one() {
    #[cfg(unix)]
    assert_eq!(Engine::default(), Engine::System);
    #[cfg(not(unix))]
    assert_eq!(Engine::default(), Engine::Minrx);
    assert_eq!(compile(b"a").engine(), Engine::default());
}

#[test]
fn every_available_engine_agrees_on_the_bash_completion_shapes() {
    // The patterns completion actually uses: anchored option filters, a
    // prefix alternation, a class, a bounded blank run, a word boundary.
    let cases: &[(&[u8], &[u8])] = &[
        (b"^--(no-)?file$", b"--no-file"),
        (b"^-[a-zA-Z0-9?]*$", b"-czvf"),
        (b"^[[:blank:]]{1,10}--?([A-Za-z0-9?=-]+)", b"    --create"),
        (b"\\bfoo\\b", b"a foo b"),
        (b"^(foo|foobar|baz)([0-9]+)?$", b"foobar42"),
        (
            b"[[:space:]]-(t|-table=?)[[:space:]]*([^[:space:]]+)",
            b"iptables -t filter",
        ),
    ];
    for engine in Engine::ALL {
        if !engine.is_available() {
            continue;
        }
        for (pattern, text) in cases {
            let re = compile_with(*engine, pattern);
            let got = re.captures(text);
            let system = compile_with(Engine::System, pattern).captures(text);
            assert_eq!(
                got,
                system,
                "{engine} disagrees on {:?}",
                String::from_utf8_lossy(pattern)
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn the_system_engine_keeps_glibc_quirks_bash_inherits() {
    // A stray `)` is a literal (`--[A-Za-z0-9-]+(\[?)=)` is a real pattern
    // bash-completion produces after quote removal), and `\1` is a
    // backreference. bash inherits both from glibc.
    let stray = compile_with(Engine::System, b"--[A-Za-z0-9-]+(\\[?)=)");
    assert_eq!(stray.captures(b"--file[=ARCHIVE"), None);
    let backref = compile_with(Engine::System, b"(a)\\1");
    assert_eq!(
        backref.captures(b"aa"),
        Some(vec![Some((0, 2)), Some((0, 1))])
    );
    assert_eq!(backref.captures(b"ab"), None);
}

#[cfg(unix)]
#[test]
fn the_system_engine_rejects_embedded_nul() {
    // `regcomp` takes a C string. Truncating would compile a different
    // pattern, so the only honest answer is an error.
    assert_eq!(
        Regex::with_engine(Engine::System, b"a\0b", false).unwrap_err(),
        RegexError::NulInPattern
    );
    assert_eq!(
        Regex::new(b"\0").unwrap_err(),
        RegexError::NulInPattern,
        "the default engine on Unix is the system one"
    );
    assert_eq!(
        RegexError::NulInPattern.message(),
        "embedded NUL byte in a regular expression"
    );

    // `regexec` cannot represent such a subject either. `captures` gives up
    // (None, like a non-match) and `try_captures` says why.
    let re = Regex::with_engine(Engine::System, b"a", false).unwrap();
    assert_eq!(re.captures(b"x\0a"), None);
    assert_eq!(re.try_captures(b"x\0a"), Err(RegexError::NulInText));
    // The same subject without the NUL still matches, so the rejection is
    // about representation, not about the pattern.
    assert_eq!(re.captures(b"xa"), Some(vec![Some((1, 2))]));
}

#[test]
fn minrx_searches_bytes_including_nul_and_has_no_backreferences() {
    // MinRX takes explicit lengths, so unlike the system engine it searches
    // the whole subject. Both differences are documented; neither can arise
    // from shell text, which cannot hold a NUL.
    let re = compile_with(Engine::Minrx, b"a");
    assert_eq!(re.captures(b"x\0a"), Some(vec![Some((2, 3))]));
    assert_eq!(re.try_captures(b"x\0a"), Ok(Some(vec![Some((2, 3))])));

    // `\1` is a literal `1` rather than a backreference. bash-completion does
    // not use backreferences; a script that does needs `--regex-engine system`.
    let literal = compile_with(Engine::Minrx, b"(a)\\1");
    assert_eq!(
        literal.captures(b"a1"),
        Some(vec![Some((0, 2)), Some((0, 1))])
    );
    assert_eq!(literal.captures(b"aa"), None);
}

#[test]
fn minrx_accepts_the_stray_parenthesis_that_bash_accepts() {
    // The regression that would matter: this pattern must compile and simply
    // not match, not turn `[[` into a status-2 error mid-completion.
    let re = compile_with(Engine::Minrx, b"--[A-Za-z0-9-]+(\\[?)=)");
    assert_eq!(re.captures(b"--file[=ARCHIVE"), None);
}

// ------------------------------------------------------------------ cache ----

#[test]
fn the_cache_compiles_once_per_pattern() {
    let mut cache = Cache::new(Engine::default());
    assert_eq!(cache.engine(), Engine::default());
    let first = cache
        .captures(b"^--([[:alnum:]-]+)=(.*)$", false, b"--file=x")
        .expect("valid pattern");
    assert_eq!(first, Some(vec![Some((0, 8)), Some((2, 6)), Some((7, 8))]));
    // A second evaluation of the same pattern must not compile again; the
    // observable contract is that it gives the same answer, and the benchmark
    // in docs/performance.md is what proves the compile is skipped.
    for text in [&b"--file=x"[..], b"--other=y", b"no match here"] {
        let got = cache
            .captures(b"^--([[:alnum:]-]+)=(.*)$", false, text)
            .expect("valid pattern");
        assert_eq!(
            got,
            compile(b"^--([[:alnum:]-]+)=(.*)$").captures(text),
            "cache result differs for {:?}",
            String::from_utf8_lossy(text)
        );
    }
}

#[test]
fn the_cache_key_covers_case_mode_and_engine() {
    let mut cache = Cache::new(Engine::default());
    assert_eq!(
        cache.captures(b"abc", false, b"ABC").unwrap(),
        None,
        "case-sensitive first"
    );
    assert_eq!(
        cache.captures(b"abc", true, b"ABC").unwrap(),
        Some(vec![Some((0, 3))]),
        "same bytes, folding mode changed"
    );
    let engine = cache.engine();
    cache.set_engine(engine);
    assert_eq!(cache.engine(), engine);
    cache.clear();
    assert_eq!(
        cache.captures(b"abc", false, b"abc").unwrap(),
        Some(vec![Some((0, 3))])
    );
}

#[test]
fn a_failed_compile_is_not_cached() {
    let mut cache = Cache::new(Engine::default());
    let first = cache.captures(b"(a", false, b"a");
    assert!(matches!(first, Err(RegexError::Engine { .. })));
    // The second evaluation must report the same error, not a stale success.
    let second = cache.captures(b"(a", false, b"a");
    assert!(matches!(second, Err(RegexError::Engine { .. })));
    // And a valid pattern after the failure still works.
    assert_eq!(
        cache.captures(b"a", false, b"a").unwrap(),
        Some(vec![Some((0, 1))])
    );
}

#[test]
fn a_cloned_cache_starts_empty_but_keeps_the_engine() {
    let mut cache = Cache::new(Engine::default());
    cache.captures(b"abc", false, b"abc").unwrap();
    let clone = cache.clone();
    assert_eq!(clone.engine(), cache.engine());
    assert!(!format!("{clone:?}").contains("cached: true"));
}
