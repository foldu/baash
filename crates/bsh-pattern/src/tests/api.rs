//! The public API surface: defaults, compiled-pattern reuse and the free
//! functions staying in lockstep with the methods.

use crate::{Flags, Pattern, matches, prefix_end, suffix_start};

#[test]
fn default_flags_are_all_off() {
    let flags = Flags::default();
    assert!(!flags.extglob);
    assert!(!flags.nocasematch);
    assert!(!flags.pathname);
    assert_eq!(
        flags,
        Flags {
            extglob: false,
            nocasematch: false,
            pathname: false
        }
    );
    let copied = flags;
    assert_eq!(copied, flags);
}

#[test]
fn flags_are_copy_and_independent() {
    let ext = Flags {
        extglob: true,
        ..Flags::default()
    };
    let mut path = ext;
    path.pathname = true;
    assert!(ext.extglob);
    assert!(!ext.pathname, "copying must not mutate the original");
    assert!(path.extglob);
    assert!(path.pathname);
}

#[test]
fn compiled_pattern_reuse_and_flags_accessor() {
    let flags = Flags {
        extglob: true,
        ..Flags::default()
    };
    let pattern = Pattern::new(b"@(foo|bar)*", flags);
    assert_eq!(pattern.flags(), flags);
    assert!(pattern.matches(b"foo.txt"));
    assert!(pattern.matches(b"bar"));
    assert!(pattern.matches(b"barbaz"));
    assert!(!pattern.matches(b"baz"));
    assert!(!pattern.matches(b"fo"));
    assert_eq!(pattern.prefix_end(b"foo.txt", false), Some(3));
    assert_eq!(pattern.prefix_end(b"foo.txt", true), Some(7));
    // Only the whole string is a suffix that starts with `foo`/`bar`.
    assert_eq!(pattern.suffix_start(b"foo.txt", false), Some(0));
    assert_eq!(pattern.suffix_start(b"foo.txt", true), Some(0));
}

#[test]
fn free_functions_match_pattern_methods() {
    let cases: &[(&[u8], &[u8])] = &[
        (b"", b""),
        (b"*", b""),
        (b"a*b", b"axxb"),
        (b"[a-c]*", b"bcd"),
        (b"@(a|ab)c", b"abc"),
        (b"!(*.o)", b"a.c"),
        (b"!(*.o)", b"a.o"),
        ("a?c".as_bytes(), "aμc".as_bytes()),
        (b"*/*", b"a/b"),
        (b"[[:digit:]]*", b"1"),
    ];
    for extglob in [false, true] {
        for pathname in [false, true] {
            for nocasematch in [false, true] {
                let flags = Flags {
                    extglob,
                    nocasematch,
                    pathname,
                };
                for &(p, t) in cases {
                    let compiled = Pattern::new(p, flags);
                    assert_eq!(compiled.matches(t), matches(p, t, flags));
                    for longest in [false, true] {
                        assert_eq!(
                            compiled.prefix_end(t, longest),
                            prefix_end(p, t, longest, flags),
                            "prefix p={p:02x?} t={t:02x?} flags={flags:?} longest={longest}"
                        );
                        assert_eq!(
                            compiled.suffix_start(t, longest),
                            suffix_start(p, t, longest, flags),
                            "suffix p={p:02x?} t={t:02x?} flags={flags:?} longest={longest}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn empty_pattern_object_is_usable() {
    let pattern = Pattern::new(b"", Flags::default());
    assert!(pattern.matches(b""));
    assert!(!pattern.matches(b"x"));
    assert_eq!(pattern.prefix_end(b"abc", false), Some(0));
    assert_eq!(pattern.suffix_start(b"abc", false), Some(3));
}

#[test]
fn malformed_patterns_never_panic() {
    // No parser failure path: every one of these is a normal pattern.
    let patterns: &[&[u8]] = &[
        b"[",
        b"[!",
        b"[]",
        b"\\",
        b"@(",
        b"@(a",
        b"@(a|",
        b"!((",
        b"*(*(",
        b"[[[:alpha:]]",
        b"[(]",
        b"\\[",
        b"@(a\\|b)",
        b"[a-",
        b"[a\\",
        b"|||",
        b")))",
        b"??(((",
    ];
    for extglob in [false, true] {
        let flags = Flags {
            extglob,
            ..Flags::default()
        };
        for &pattern in patterns {
            for text in [&b""[..], b"a", b"(", b"[", b"|||"] {
                let _ = matches(pattern, text, flags);
                let _ = prefix_end(pattern, text, false, flags);
                let _ = suffix_start(pattern, text, true, flags);
            }
        }
    }
}

#[test]
fn deep_nesting_is_bounded_not_a_stack_overflow() {
    // The parser, compiler and `!` evaluator are recursive in nesting depth;
    // a shell variable can supply an arbitrarily deep pattern. Beyond the
    // documented cap the group is literal text, and nothing panics.
    let depth = 400;
    let mut pattern = String::new();
    for _ in 0..depth {
        pattern.push_str("@(");
    }
    pattern.push('a');
    for _ in 0..depth {
        pattern.push(')');
    }
    let flags = Flags {
        extglob: true,
        ..Flags::default()
    };
    let _ = matches(pattern.as_bytes(), b"a", flags);
    let _ = prefix_end(pattern.as_bytes(), b"a", true, flags);
    let _ = suffix_start(pattern.as_bytes(), b"a", true, flags);

    let mut negated = String::new();
    for _ in 0..depth {
        negated.push_str("!(");
    }
    negated.push('a');
    for _ in 0..depth {
        negated.push(')');
    }
    let _ = matches(negated.as_bytes(), b"a", flags);
}
