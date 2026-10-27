//! The locale switch: byte units (`Flags::utf8 == false`, the default and the
//! C locale) versus UTF-8 character units (`utf8 == true`).
//!
//! In byte mode `?` is one **byte**, bracket ranges compare byte values, and a
//! multi-byte character is several units. In UTF-8 mode `?` is one Unicode
//! scalar value, invalid bytes are one-byte units, and bracket ranges compare
//! scalar values. Both modes go through the same NFA; only unit splitting and
//! range comparison differ.
//!
//! bash behaves the same way: with `LC_ALL=C`, `v=$(printf '\316\274x');
//! ${#v}` is 3 and `${v#?}` strips one byte; with `LC_ALL=C.UTF-8` it is 2 and
//! `${v#?}` strips the whole character.

use crate::{Flags, matches, prefix_end, suffix_start};

/// Byte mode: the default, C locale.
const C: Flags = Flags {
    extglob: false,
    nocasematch: false,
    pathname: false,
    utf8: false,
};
/// UTF-8 mode.
const U: Flags = Flags {
    extglob: false,
    nocasematch: false,
    pathname: false,
    utf8: true,
};

#[test]
fn byte_mode_question_is_one_byte() {
    assert!(!matches(b"?", "μ".as_bytes(), C));
    assert!(matches(b"??", "μ".as_bytes(), C));
    assert!(!matches(b"??", "μx".as_bytes(), C));
    assert!(matches(b"???", "μx".as_bytes(), C));
    assert!(!matches(b"?", "é".as_bytes(), C));
    assert!(matches(b"??", "é".as_bytes(), C));
    // An invalid byte is just a byte.
    assert!(matches(b"?", &[0xFF], C));
    assert!(!matches(b"??", &[0xFF], C));
}

#[test]
fn utf8_mode_question_counts_code_points() {
    assert!(matches(b"?", "μ".as_bytes(), U));
    assert!(matches(b"?", "€".as_bytes(), U));
    assert!(matches(b"?", "😀".as_bytes(), U));
    assert!(!matches(b"??", "μ".as_bytes(), U));
    assert!(matches(b"??", "μa".as_bytes(), U));
    // A combining sequence is two code points, so `?` is not enough.
    assert!(!matches(b"__?__", "__a\u{0300}__".as_bytes(), U));
    assert!(matches(b"__?__", "__a__".as_bytes(), U));
}

#[test]
fn byte_mode_star_splits_bytes() {
    // `*` can stop between the two bytes of `μ`; a literal continuation byte
    // after a star is enough to match the tail.
    assert!(matches(&[b'*', 0xBC], "μ".as_bytes(), C));
    assert!(matches(b"*", "μ€😀".as_bytes(), C));
    // The first byte alone is not the whole character.
    assert!(!matches(&[0xBC], "μ".as_bytes(), C));
}

#[test]
fn utf8_mode_star_does_not_split_a_character() {
    assert!(matches(b"*", "μ€😀".as_bytes(), U));
    assert!(matches("*μ".as_bytes(), "aμ".as_bytes(), U));
    assert!(!matches("*μ".as_bytes(), "aμb".as_bytes(), U));
    // The second byte of `μ`, used as a literal, is not a character boundary.
    assert!(!matches(&[0xBC], "μ".as_bytes(), U));
}

#[test]
fn byte_mode_prefix_suffix_offsets() {
    // "μ-" = CE BC 2D: three bytes, so `?` removes one byte.
    let text = "μ-".as_bytes();
    assert_eq!(prefix_end(b"?", text, false, C), Some(1));
    assert_eq!(prefix_end(b"??", text, false, C), Some(2));
    assert_eq!(prefix_end(b"???", text, false, C), Some(3));
    assert_eq!(suffix_start(b"?", text, false, C), Some(2));
    assert_eq!(suffix_start(b"??", text, false, C), Some(1));
    assert_eq!(suffix_start(b"???", text, false, C), Some(0));
    // "-μ" = 2D CE BC: the last byte starts at 2.
    assert_eq!(suffix_start(b"?", "-μ".as_bytes(), false, C), Some(2));
}

#[test]
fn utf8_mode_prefix_suffix_offsets() {
    // "μ-" = CE BC 2D: two characters, so `?` removes two bytes.
    let text = "μ-".as_bytes();
    assert_eq!(prefix_end(b"?", text, false, U), Some(2));
    assert_eq!(prefix_end(b"??", text, false, U), Some(3));
    assert_eq!(suffix_start(b"?", text, false, U), Some(2));
    assert_eq!(suffix_start(b"??", text, false, U), Some(0));
    assert_eq!(suffix_start(b"?", "-μ".as_bytes(), false, U), Some(1));
    assert_eq!(prefix_end(b"??", "μ".as_bytes(), false, U), None);
}

#[test]
fn byte_mode_brackets_are_bytes() {
    // `[é]` parses as two one-byte members, so it matches either byte alone,
    // not the two-byte character as a whole.
    assert!(!matches("[é]".as_bytes(), "é".as_bytes(), C));
    assert!(matches("[é]".as_bytes(), &[0xC3], C));
    assert!(matches("[é]".as_bytes(), &[0xA9], C));
    // A negated bracket is still a one-byte expression.
    assert!(!matches("[!μ]".as_bytes(), &[0xCE], C));
    assert!(matches("[!μ]".as_bytes(), b"x", C));
    // Byte ranges compare byte values; `\xC0-\xFF` includes 0xFF.
    assert!(matches(b"[\xC0-\xFF]", &[0xFF], C));
    assert!(matches(b"[\xC0-\xFF]", &[0xC3], C));
    assert!(!matches(b"[\xC0-\xFF]", b"a", C));
}

#[test]
fn utf8_mode_brackets_hold_whole_characters() {
    assert!(matches("[μ]".as_bytes(), "μ".as_bytes(), U));
    assert!(!matches("[μ]".as_bytes(), b"a", U));
    assert!(matches(&[b'[', 0xFF, b']'], &[0xFF], U));
    assert!(!matches("[!μ]".as_bytes(), "μ".as_bytes(), U));
    assert!(matches("[!μ]".as_bytes(), "a".as_bytes(), U));
    // Scalar-value ranges include the whole character.
    assert!(matches("[à-ÿ]".as_bytes(), "é".as_bytes(), U));
    assert!(!matches("[à-ÿ]".as_bytes(), b"a", U));
}

#[test]
fn invalid_bytes_are_units_in_both_modes() {
    let ff = &[0xFF][..];
    for flags in [C, U] {
        assert!(matches(b"?", ff, flags));
        assert!(!matches(b"??", ff, flags));
        assert!(matches(b"*", ff, flags));
        assert!(matches(ff, ff, flags));
        assert!(!matches(b"[a-z]", ff, flags));
        assert!(!matches(b"[[:print:]]", ff, flags));
        // Literal bracket membership still works.
        assert!(matches(b"[!a]", ff, flags));
    }
}

#[test]
fn nocasematch_is_ascii_only_in_both_modes() {
    for flags in [
        Flags {
            nocasematch: true,
            ..C
        },
        Flags {
            nocasematch: true,
            ..U
        },
    ] {
        // É (C3 89) and é (C3 A9) differ in a non-ASCII byte; ASCII folding
        // leaves both alone in both modes.
        assert!(!matches("É".as_bytes(), "é".as_bytes(), flags));
        assert!(matches("É".as_bytes(), "É".as_bytes(), flags));
        // ASCII still folds.
        assert!(matches(b"A", b"a", flags));
        assert!(matches(b"a?C", b"Abc", flags));
        assert!(matches(b"[a-z]", b"A", flags));
    }
}

#[test]
fn posix_classes_are_ascii_only() {
    // Documented deviation from glibc in a UTF-8 locale: the classes are
    // locale-independent ASCII, in both modes.
    for flags in [C, U] {
        assert!(!matches(b"[[:alpha:]]", "é".as_bytes(), flags));
        assert!(!matches(b"[[:upper:]]", "É".as_bytes(), flags));
        assert!(!matches(b"[[:print:]]", "é".as_bytes(), flags));
        assert!(matches(b"[[:alpha:]]", "a".as_bytes(), flags));
    }
}
