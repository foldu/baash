//! The placeholder glob engine's own tests.
//!
//! These pin the shape the shared pattern engine (DESIGN.md §4.8) will have to
//! keep: per-byte quoting, no pathname semantics in `matches`, sorted matches,
//! and `None` for "nothing matched" so the caller can leave the word alone.

use std::path::PathBuf;

use bsh_vm::glob::{Pattern, expand, matches};

fn hit(pattern: &str, text: &str) -> bool {
    matches(&Pattern::new(pattern.as_bytes(), None), text.as_bytes())
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bsh-glob-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn literals_and_wildcards() {
    assert!(hit("abc", "abc"));
    assert!(!hit("abc", "abd"));
    assert!(!hit("abc", "ab"));
    assert!(hit("a*c", "abc"));
    assert!(hit("a*c", "ac"), "`*` matches the empty string");
    assert!(hit("a*c", "axyzc"));
    assert!(hit("*", ""));
    assert!(hit("*", "anything"));
    assert!(hit("a?c", "abc"));
    assert!(!hit("a?c", "ac"));
    assert!(hit("*abc*", "xxabcyy"));
    assert!(hit("a*b*c", "aXbYc"));
    assert!(!hit("a*b*c", "aXbY"));
    assert!(hit("", ""));
    assert!(!hit("", "x"));
}

#[test]
fn brackets_ranges_and_negation() {
    assert!(hit("[abc]", "b"));
    assert!(!hit("[abc]", "d"));
    assert!(hit("[a-c]x", "bx"));
    assert!(!hit("[a-c]x", "dx"));
    assert!(hit("[!abc]", "d"));
    assert!(!hit("[!abc]", "a"));
    assert!(hit("[^abc]", "d"));
    assert!(hit("[]]", "]"), "a `]` first is literal");
    assert!(hit("[a-]", "-"));
    // An unterminated bracket is a literal `[`.
    assert!(hit("[abc", "[abc"));
    assert!(hit("[a-z]*[0-9]", "zz9"));
}

#[test]
fn quoted_metacharacters_are_inert() {
    let pattern = b"a*c";
    let quoted = [false, true, false];
    let pattern = Pattern::new(pattern, Some(&quoted));
    assert!(matches(&pattern, b"a*c"), "a quoted `*` matches itself");
    assert!(!matches(&pattern, b"abc"), "a quoted `*` is not a wildcard");
    // A fully quoted field has no active metacharacter at all.
    assert!(!Pattern::new(b"a*c", Some(&[true, true, true])).has_meta());
    assert!(Pattern::new(b"a*c", None).has_meta());
    assert!(!Pattern::new(b"abc", None).has_meta());
}

#[test]
fn star_backtracking_finds_the_match() {
    // The classic case that a greedy matcher without backtracking gets wrong.
    assert!(hit("*a*a*a*b", "aaaaaaab"));
    assert!(!hit("*a*a*a*b", "aaaaaaa"));
    assert!(hit("*x*", "yxyxyx"));
}

#[test]
fn expand_lists_sorted_matches() {
    let dir = temp_dir("expand");
    for name in ["b.txt", "a.txt", "c.md", ".hidden"] {
        std::fs::write(dir.join(name), "").unwrap();
    }
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub/deep.txt"), "").unwrap();

    let pattern = format!("{}/*.txt", dir.display());
    let found = expand(&Pattern::new(pattern.as_bytes(), None)).expect("matches");
    let found: Vec<String> = found
        .iter()
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .collect();
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found[0].ends_with("a.txt"), "{found:?}");
    assert!(found[1].ends_with("b.txt"), "{found:?}");

    // A wildcard does not match a leading dot, and `*/*` descends one level.
    let pattern = format!("{}/*", dir.display());
    let found = expand(&Pattern::new(pattern.as_bytes(), None)).expect("matches");
    assert_eq!(found.len(), 4, "{found:?}");
    let pattern = format!("{}/*/*.txt", dir.display());
    let found = expand(&Pattern::new(pattern.as_bytes(), None)).expect("matches");
    assert_eq!(found.len(), 1);
    assert!(
        String::from_utf8_lossy(&found[0]).ends_with("sub/deep.txt"),
        "{found:?}"
    );

    // No match is `None`, never an empty vector: the caller leaves the word.
    let pattern = format!("{}/*.none", dir.display());
    assert_eq!(expand(&Pattern::new(pattern.as_bytes(), None)), None);

    // A literal component that does not exist is not a match either.
    let pattern = format!("{}/missing/*.txt", dir.display());
    assert_eq!(expand(&Pattern::new(pattern.as_bytes(), None)), None);

    // Nothing to glob: `None`, and the caller keeps the word as written.
    assert_eq!(expand(&Pattern::new(pattern.as_bytes(), None)), None);
    assert_eq!(expand(&Pattern::new(b"plain.txt", None)), None);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn expand_keeps_a_literal_prefix_as_written() {
    let dir = temp_dir("prefix");
    std::fs::write(dir.join("one.txt"), "").unwrap();
    // An absolute pattern keeps its leading `/`.
    let pattern = format!("{}/*.txt", dir.display());
    let found = expand(&Pattern::new(pattern.as_bytes(), None)).expect("matches");
    assert!(
        String::from_utf8_lossy(&found[0]).starts_with('/'),
        "absolute patterns stay absolute"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
