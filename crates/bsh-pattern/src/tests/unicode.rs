//! Bytes versus characters: UTF-8 units, invalid bytes and byte offsets.
//!
//! The engine is byte-oriented in its API but character-oriented in its
//! wildcards, which is what bash 5.3 does in a UTF-8 locale: `?` matches `μ`
//! once. Invalid bytes are units of their own, so a non-UTF-8 filename still
//! matches itself and `?` still consumes it as one character. This module pins
//! both halves and every returned offset.

use crate::{Flags, matches, prefix_end, suffix_start};

const OFF: Flags = Flags {
    extglob: false,
    nocasematch: false,
    pathname: false,
};

#[test]
fn question_counts_code_points() {
    assert!(matches(b"?", "μ".as_bytes(), OFF));
    assert!(matches(b"?", "€".as_bytes(), OFF));
    assert!(matches(b"?", "😀".as_bytes(), OFF));
    assert!(!matches(b"??", "μ".as_bytes(), OFF));
    assert!(matches(b"??", "μa".as_bytes(), OFF));
    // A combining sequence is two code points, so `?` is not enough.
    assert!(!matches(b"__?__", "__a\u{0300}__".as_bytes(), OFF));
    assert!(matches(b"__?__", "__a__".as_bytes(), OFF));
}

#[test]
fn star_does_not_split_a_character() {
    // `*` can stop anywhere, but always on a character boundary. A trailing
    // literal that begins with a continuation byte cannot match the middle of
    // a character.
    assert!(matches(b"*", "μ€😀".as_bytes(), OFF));
    assert!(matches("*μ".as_bytes(), "aμ".as_bytes(), OFF));
    assert!(!matches("*μ".as_bytes(), "aμb".as_bytes(), OFF));
    // The second byte of `μ`, used as a literal, is not a character boundary.
    assert!(!matches(&[0xBC], "μ".as_bytes(), OFF));
}

#[test]
fn invalid_bytes_are_single_units() {
    let ff = &[0xFF][..];
    assert!(matches(b"?", ff, OFF));
    assert!(!matches(b"??", ff, OFF));
    assert!(matches(b"*", ff, OFF));
    assert!(matches(ff, ff, OFF));
    assert!(!matches(ff, b"a", OFF));
    assert!(matches(b"a?b", &[b'a', 0xFF, b'b'], OFF));

    // An invalid byte matches no bracket range or POSIX class, but literal
    // bracket membership still works.
    assert!(!matches(b"[a-z]", ff, OFF));
    assert!(!matches(b"[[:print:]]", ff, OFF));
    assert!(matches(b"[!a]", ff, OFF));
}

#[test]
fn prefix_offsets_are_bytes_at_character_boundaries() {
    // "μ-" = CE BC 2D.
    assert_eq!(prefix_end(b"?", "μ-".as_bytes(), false, OFF), Some(2));
    assert_eq!(prefix_end(b"?", "μ-".as_bytes(), true, OFF), Some(2));
    assert_eq!(prefix_end(b"??", "μ-".as_bytes(), false, OFF), Some(3));
    assert_eq!(prefix_end(b"?-", "μ-".as_bytes(), false, OFF), Some(3));
    // "€" is three bytes; "😀" is four.
    assert_eq!(prefix_end(b"?", "€".as_bytes(), false, OFF), Some(3));
    assert_eq!(prefix_end(b"?", "😀".as_bytes(), false, OFF), Some(4));
    assert_eq!(
        prefix_end("é".as_bytes(), "éx".as_bytes(), false, OFF),
        Some(2)
    );
    // An invalid byte is one byte of prefix.
    assert_eq!(prefix_end(b"?", &[0xFF, b'a'], false, OFF), Some(1));
    assert_eq!(prefix_end(b"??", &[0xFF, b'a'], false, OFF), Some(2));
    // `??` cannot match a single character.
    assert_eq!(prefix_end(b"??", "μ".as_bytes(), false, OFF), None);
}

#[test]
fn suffix_offsets_are_bytes_at_character_boundaries() {
    assert_eq!(suffix_start(b"?", "μ-".as_bytes(), false, OFF), Some(2));
    assert_eq!(suffix_start(b"?", "μ-".as_bytes(), true, OFF), Some(2));
    assert_eq!(suffix_start(b"??", "μ-".as_bytes(), false, OFF), Some(0));
    assert_eq!(suffix_start(b"?-", "μ-".as_bytes(), false, OFF), Some(0));
    assert_eq!(suffix_start(b"?", "-μ".as_bytes(), false, OFF), Some(1));
    assert_eq!(suffix_start(b"?", "€".as_bytes(), false, OFF), Some(0));
    assert_eq!(suffix_start(b"?", "😀".as_bytes(), false, OFF), Some(0));
    assert_eq!(
        suffix_start("é".as_bytes(), "xé".as_bytes(), false, OFF),
        Some(1)
    );
    assert_eq!(suffix_start(b"??", "μ".as_bytes(), false, OFF), None);
    assert_eq!(suffix_start(b"?", &[b'a', 0xFF], false, OFF), Some(1));
}

#[test]
fn bracket_expressions_hold_whole_characters() {
    assert!(matches("[μ]".as_bytes(), "μ".as_bytes(), OFF));
    assert!(!matches("[μ]".as_bytes(), b"a", OFF));
    // Any single invalid byte can be a bracket member.
    assert!(matches(&[b'[', 0xFF, b']'], &[0xFF], OFF));
    // Negation is per character, not per byte.
    assert!(!matches("[!μ]".as_bytes(), "μ".as_bytes(), OFF));
    assert!(matches("[!μ]".as_bytes(), "a".as_bytes(), OFF));
}

#[test]
fn nocasematch_does_not_touch_non_ascii() {
    let nocase = Flags {
        nocasematch: true,
        ..Flags::default()
    };
    // É (C3 89) and é (C3 A9) differ in a non-ASCII byte; ASCII folding leaves
    // both alone.
    assert!(!matches("É".as_bytes(), "é".as_bytes(), nocase));
    assert!(matches("É".as_bytes(), "É".as_bytes(), nocase));
    // The same two strings without the flag.
    assert!(!matches("É".as_bytes(), "é".as_bytes(), OFF));
    // ASCII still folds.
    assert!(matches(b"A", b"a", nocase));
    assert!(matches(b"a?C", b"Abc", nocase));
}

#[test]
fn posix_classes_are_ascii_only() {
    // Documented deviation: glibc's `[:alpha:]` accepts `é` in a UTF-8
    // locale; this engine is locale-independent.
    assert!(!matches(b"[[:alpha:]]", "é".as_bytes(), OFF));
    assert!(!matches(b"[[:upper:]]", "É".as_bytes(), OFF));
    assert!(!matches(b"[[:print:]]", "é".as_bytes(), OFF));
}
