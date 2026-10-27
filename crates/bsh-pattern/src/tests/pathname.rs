//! `Flags::pathname`: `*`, `?` and brackets must not cross `/`.
//!
//! This is the flag pathname expansion uses. The glob layer splits a path into
//! components and matches each component, but the flag is still needed for
//! `*/*` and for callers that match a whole path at once. Dotfile rules
//! (`dotglob`), `globstar`, `nullglob` and sorting are deliberately not part of
//! this crate.

use crate::{Flags, Pattern, matches, prefix_end, suffix_start};

fn path(pattern: &[u8], text: &[u8]) -> bool {
    matches(
        pattern,
        text,
        Flags {
            pathname: true,
            ..Flags::default()
        },
    )
}

#[test]
fn star_cannot_cross_a_slash() {
    assert!(path(b"*", b"a"));
    assert!(!path(b"*", b"a/b"));
    assert!(!path(b"*", b"/"));
    assert!(path(b"*", b""));
    assert!(path(b"*/*", b"a/b"));
    assert!(!path(b"*/*", b"a/b/c"));
    assert!(path(b"*/*/*", b"a/b/c"));
    assert!(path(b"a/*", b"a/b"));
    assert!(!path(b"a/*", b"a/b/c"));
    assert!(path(b"a*", b"abc"));
    assert!(!path(b"a*", b"ab/c"));
}

#[test]
fn question_cannot_match_a_slash() {
    assert!(path(b"?", b"a"));
    assert!(!path(b"?", b"/"));
    assert!(!path(b"a?b", b"a/b"));
    assert!(path(b"a?b", b"axb"));
    assert!(path(b"?/b", b"a/b"));
    assert!(!path(b"?/b", b"aa/b"));
    // One code point, not one byte, and still not `/`.
    assert!(path(b"?", "μ".as_bytes()));
    assert!(path(b"?/?", "μ/€".as_bytes()));
}

#[test]
fn brackets_cannot_match_a_slash() {
    assert!(path(b"[ab]", b"a"));
    assert!(!path(b"[ab]", b"/"));
    assert!(!path(b"[a/]", b"/"));
    assert!(!path(b"[!a]", b"/"));
    assert!(!path(b"[^a]", b"/"));
    assert!(!path(b"a[/]b", b"a/b"));
    assert!(path(b"a/b", b"a/b"));
}

#[test]
fn string_mode_is_the_contrast() {
    // The same patterns with pathname off.
    assert!(matches(b"*", b"a/b", Flags::default()));
    assert!(matches(b"?", b"/", Flags::default()));
    assert!(matches(b"[a/]", b"/", Flags::default()));
    assert!(matches(b"a?b", b"a/b", Flags::default()));
}

#[test]
fn extglob_groups_respect_slashes_through_their_own_constructs() {
    let ext = Flags {
        extglob: true,
        pathname: true,
        ..Flags::default()
    };
    assert!(matches(b"@(a|b)", b"a", ext));
    assert!(!matches(b"@(a|b)", b"a/b", ext));
    assert!(matches(b"@(a|*)/b", b"a/b", ext));
    assert!(!matches(b"@(a|*)/b", b"a/c/b", ext));
    // A group's `*` still refuses `/`, so this cannot match two components.
    assert!(!matches(b"@(*)", b"a/b", ext));
    // Repetition of a character group cannot jump over a slash.
    assert!(matches(b"*(a|b)", b"abba", ext));
    assert!(!matches(b"*(a|b)", b"ab/ba", ext));
}

#[test]
fn pathname_combines_with_nocasematch() {
    let flags = Flags {
        nocasematch: true,
        pathname: true,
        ..Flags::default()
    };
    assert!(matches(b"*.TXT", b"a.txt", flags));
    assert!(!matches(b"*.TXT", b"a.txt/b", flags));
    assert!(matches(b"[A-Z]*", b"abc", flags));
    assert!(!matches(b"[A-Z]*", b"a/b", flags));
}

#[test]
fn compiled_pattern_matches_components() {
    let pattern = Pattern::new(
        b"*.txt",
        Flags {
            pathname: true,
            ..Flags::default()
        },
    );
    for name in [&b"a.txt"[..], b"b.txt", b"a.txt.bak", b"dir/a.txt"] {
        assert_eq!(
            pattern.matches(name),
            !name.contains(&b'/') && name.ends_with(b".txt"),
            "name={name:02x?}"
        );
    }
}

#[test]
fn strip_operators_respect_pathname_components() {
    let path = Flags {
        pathname: true,
        ..Flags::default()
    };
    // `*` cannot cross the slash, so it can remove at most "a".
    assert_eq!(prefix_end(b"*", b"a/b", false, path), Some(0));
    assert_eq!(prefix_end(b"*", b"a/b", true, path), Some(1));
    assert_eq!(prefix_end(b"a/*", b"a/b/c", false, path), Some(2));
    assert_eq!(prefix_end(b"a/*", b"a/b/c", true, path), Some(3));
    // A suffix must also stay inside one component.
    assert_eq!(suffix_start(b"*", b"a/b", false, path), Some(3));
    assert_eq!(suffix_start(b"*", b"a/b", true, path), Some(2));
    assert_eq!(suffix_start(b"*/c", b"a/b/c", false, path), Some(3));
    assert_eq!(suffix_start(b"*/c", b"a/b/c", true, path), Some(2));
    // Explicit slashes still match.
    assert_eq!(prefix_end(b"a/b", b"a/bc", false, path), Some(3));
    assert_eq!(suffix_start(b"b/c", b"ab/c", false, path), Some(1));
}
