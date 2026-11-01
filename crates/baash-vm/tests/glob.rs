//! Pathname expansion on top of the shared pattern engine.
//!
//! These pin the contract `baash_lang::Shell::pathname` relies on: sorted
//! matches, per-byte quoting, `None` for "nothing matched" so the caller can
//! leave the word alone, and the `shopt` policies (`dotglob`, `globstar`,
//! `globskipdots`) that are the caller's rather than the matcher's.

use std::path::PathBuf;

use baash_vm::glob::{Options, expand, has_meta};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("baash-glob-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Expand `pattern` with nothing quoted.
fn expand_plain(pattern: &str, options: Options) -> Option<Vec<String>> {
    let quoted = vec![false; pattern.len()];
    expand(pattern.as_bytes(), &quoted, &options).map(|matches| {
        matches
            .iter()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .collect()
    })
}

#[test]
fn has_meta_honours_the_quoting_mask() {
    assert!(has_meta(b"a*c", &[false; 3], false));
    assert!(!has_meta(b"a*c", &[false, true, false], false));
    assert!(!has_meta(b"abc", &[false; 3], false));
    assert!(has_meta(b"[ab]", &[false; 4], false));
    // Extglob operators only count when the dialect allows them.
    assert!(has_meta(b"@(a|b)", &[false; 6], true));
    assert!(!has_meta(b"@(a|b)", &[false; 6], false));
    assert!(!has_meta(b"@(a|b)", &[true; 6], true));
}

#[test]
fn expand_lists_sorted_matches() {
    let dir = temp_dir("expand");
    for name in ["b.txt", "a.txt", "c.md", ".hidden"] {
        std::fs::write(dir.join(name), "").unwrap();
    }
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub/deep.txt"), "").unwrap();

    let found =
        expand_plain(&format!("{}/*.txt", dir.display()), Options::default()).expect("matches");
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found[0].ends_with("a.txt"), "{found:?}");
    assert!(found[1].ends_with("b.txt"), "{found:?}");

    // A wildcard does not match a leading dot, and `*/*` descends one level.
    let found = expand_plain(&format!("{}/*", dir.display()), Options::default()).expect("matches");
    assert_eq!(found.len(), 4, "{found:?}");
    let found =
        expand_plain(&format!("{}/*/*.txt", dir.display()), Options::default()).expect("matches");
    assert_eq!(found.len(), 1);
    assert!(found[0].ends_with("sub/deep.txt"), "{found:?}");

    // No match is `None`, never an empty vector: the caller leaves the word.
    assert_eq!(
        expand_plain(&format!("{}/*.none", dir.display()), Options::default()),
        None
    );
    // A literal component that does not exist is not a match either.
    assert_eq!(
        expand_plain(
            &format!("{}/missing/*.txt", dir.display()),
            Options::default()
        ),
        None
    );
    assert_eq!(expand_plain("plain.txt", Options::default()), None);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn brackets_expand_and_posix_classes_work() {
    let dir = temp_dir("brackets");
    for name in ["a.txt", "b.txt", "c.md", "e.E", "foo.-"] {
        std::fs::write(dir.join(name), "").unwrap();
    }
    let found =
        expand_plain(&format!("{}/[ab].txt", dir.display()), Options::default()).expect("matches");
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found[0].ends_with("a.txt"), "{found:?}");
    assert!(found[1].ends_with("b.txt"), "{found:?}");

    // The placeholder matcher had no POSIX classes; the shared one does.
    let found = expand_plain(
        &format!("{}/*.[[:punct:]E]", dir.display()),
        Options::default(),
    )
    .expect("matches");
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found[0].ends_with("e.E"), "{found:?}");
    assert!(found[1].ends_with("foo.-"), "{found:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn quoted_metacharacters_are_literal_in_a_pathname() {
    let dir = temp_dir("quoted");
    std::fs::write(dir.join("a*b"), "").unwrap();
    std::fs::write(dir.join("axb"), "").unwrap();
    // `a\*b`: the `*` was quoted, so it is a literal star in the pattern.
    let pattern = format!("{}/a*b", dir.display());
    let bytes = pattern.as_bytes();
    let quoted: Vec<bool> = bytes.iter().map(|&byte| byte == b'*').collect();
    let found = expand(bytes, &quoted, &Options::default()).expect("matches");
    let found: Vec<String> = found
        .iter()
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .collect();
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].ends_with("a*b"), "{found:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn dotglob_and_globskipdots_are_caller_policy() {
    let dir = temp_dir("dots");
    std::fs::write(dir.join(".hidden"), "").unwrap();
    std::fs::write(dir.join("shown"), "").unwrap();

    let found = expand_plain(&format!("{}/*", dir.display()), Options::default()).expect("matches");
    assert_eq!(found.len(), 1, "{found:?}");

    // `dotglob`: `*` also matches a leading dot.
    let found = expand_plain(
        &format!("{}/*", dir.display()),
        Options {
            dotglob: true,
            ..Options::default()
        },
    )
    .expect("matches");
    assert_eq!(found.len(), 2, "{found:?}");

    // `.*` never yields `.`/`..` while `globskipdots` is on (bash 5.2+ default).
    let found =
        expand_plain(&format!("{}/.*", dir.display()), Options::default()).expect("matches");
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].ends_with(".hidden"), "{found:?}");

    // Off, it does.
    let found = expand_plain(
        &format!("{}/.*", dir.display()),
        Options {
            globskipdots: false,
            ..Options::default()
        },
    )
    .expect("matches");
    assert!(found.iter().any(|path| path.ends_with("/.")), "{found:?}");
    assert!(found.iter().any(|path| path.ends_with("/..")), "{found:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn globstar_recurses_only_as_a_whole_component() {
    let dir = temp_dir("globstar");
    std::fs::create_dir_all(dir.join("c/subdir")).unwrap();
    std::fs::create_dir_all(dir.join("directory")).unwrap();
    for name in [
        "leaf.md",
        "c/leaf.md",
        "c/subdir/leaf.md",
        "directory/leaf.md",
    ] {
        std::fs::write(dir.join(name), "").unwrap();
    }

    // Off, `**` is `*`: it cannot cross a `/`.
    let found =
        expand_plain(&format!("{}/**/*.md", dir.display()), Options::default()).expect("matches");
    assert_eq!(found.len(), 2, "{found:?}");

    let globstar = Options {
        globstar: true,
        ..Options::default()
    };
    let found = expand_plain(&format!("{}/**/*.md", dir.display()), globstar).expect("matches");
    assert_eq!(found.len(), 4, "{found:?}");

    // The same tree seen by two `**` components must not duplicate.
    let found = expand_plain(&format!("{}/**/**/*.md", dir.display()), globstar).expect("matches");
    assert_eq!(found.len(), 4, "{found:?}");

    // `d**` is not a globstar component: it is `*` next to other bytes.
    let found = expand_plain(&format!("{}/d**/*.md", dir.display()), globstar).expect("matches");
    assert_eq!(found.len(), 1, "{found:?}");
    let found = expand_plain(&format!("{}/**y/*.md", dir.display()), globstar).expect("matches");
    assert_eq!(found.len(), 1, "{found:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn expand_keeps_a_literal_prefix_as_written() {
    let dir = temp_dir("prefix");
    std::fs::write(dir.join("one.txt"), "").unwrap();
    // An absolute pattern keeps its leading `/`.
    let found =
        expand_plain(&format!("{}/*.txt", dir.display()), Options::default()).expect("matches");
    assert!(
        found[0].starts_with('/'),
        "absolute patterns stay absolute: {found:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
