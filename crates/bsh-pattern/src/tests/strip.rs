//! `prefix_end` and `suffix_start`: the four parameter-strip operators.
//!
//! The offsets are byte offsets. The comments give the bash expression each
//! row models; every number here was also checked against bash 5.3.20 by
//! comparing `${v#pat}`, `${v##pat}`, `${v%pat}` and `${v%%pat}`.

use crate::{Flags, prefix_end, suffix_start};

const OFF: Flags = Flags {
    extglob: false,
    nocasematch: false,
    pathname: false,
};
const EXT: Flags = Flags {
    extglob: true,
    nocasematch: false,
    pathname: false,
};

/// `short` is `${v#pat}` / `${v%pat}`, `long` is `##` / `%%`.
fn prefix(flags: Flags, pattern: &[u8], text: &[u8], short: Option<usize>, long: Option<usize>) {
    assert_eq!(
        prefix_end(pattern, text, false, flags),
        short,
        "shortest prefix: pattern={pattern:02x?} text={text:02x?}"
    );
    assert_eq!(
        prefix_end(pattern, text, true, flags),
        long,
        "longest prefix: pattern={pattern:02x?} text={text:02x?}"
    );
}

fn suffix(flags: Flags, pattern: &[u8], text: &[u8], short: Option<usize>, long: Option<usize>) {
    assert_eq!(
        suffix_start(pattern, text, false, flags),
        short,
        "shortest suffix: pattern={pattern:02x?} text={text:02x?}"
    );
    assert_eq!(
        suffix_start(pattern, text, true, flags),
        long,
        "longest suffix: pattern={pattern:02x?} text={text:02x?}"
    );
}

#[test]
fn strip_constants() {
    // ${v#?} / ${v##?} / ${v%[[:alpha:]]} / ...
    prefix(OFF, b"?", b"aabbccdd", Some(1), Some(1));
    suffix(OFF, b"?", b"aabbccdd", Some(7), Some(7));
    prefix(OFF, b"[[:alpha:]]", b"aabbccdd", Some(1), Some(1));
    suffix(OFF, b"[[:alpha:]]", b"aabbccdd", Some(7), Some(7));

    // ${v#aabbccdd} removes the whole string; ${v%aabbccdd} removes all of it.
    prefix(OFF, b"aabbccdd", b"aabbccdd", Some(8), Some(8));
    suffix(OFF, b"aabbccdd", b"aabbccdd", Some(0), Some(0));

    // ${v#} and ${v%} are no-ops.
    prefix(OFF, b"", b"aabbccdd", Some(0), Some(0));
    suffix(OFF, b"", b"aabbccdd", Some(8), Some(8));
}

#[test]
fn strip_glob_prefix() {
    // ${v#*b} = bccdd (remove 3), ${v##*b} = ccdd (remove 4).
    prefix(OFF, b"*b", b"aabbccdd", Some(3), Some(4));
    // ${v#a*b} has the same two ends.
    prefix(OFF, b"a*b", b"aabbccdd", Some(3), Some(4));
    // ${v#*b*} = bccdd (remove 3), ${v##*b*} = "" (remove 8).
    prefix(OFF, b"*b*", b"aabbccdd", Some(3), Some(8));
    // ${v#*} = v, ${v##*} = "".
    prefix(OFF, b"*", b"aabbccdd", Some(0), Some(8));
    // A `*` that matches empty at the front.
    prefix(OFF, b"*a*b", b"aabbccdd", Some(3), Some(4));
}

#[test]
fn strip_glob_suffix() {
    // ${v%c*} = aabbc (suffix starts at 5), ${v%%c*} = aabb (starts at 4).
    suffix(OFF, b"c*", b"aabbccdd", Some(5), Some(4));
    // ${v%*b*} = aab (starts at 3), ${v%%*b*} = "" (starts at 0).
    suffix(OFF, b"*b*", b"aabbccdd", Some(3), Some(0));
    // ${v%*} = v, ${v%%*} = "".
    suffix(OFF, b"*", b"aabbccdd", Some(8), Some(0));
}

#[test]
fn strip_no_match() {
    prefix(OFF, b"z*", b"aabbccdd", None, None);
    suffix(OFF, b"*z", b"aabbccdd", None, None);
    prefix(OFF, b"abcd", b"abc", None, None);
    suffix(OFF, b"abcd", b"abc", None, None);
    prefix(OFF, b"a", b"", None, None);
    suffix(OFF, b"a", b"", None, None);
    // The pattern matches *inside* the text but not as a prefix/suffix.
    prefix(OFF, b"b*", b"ab", None, None);
    suffix(OFF, b"*a", b"ab", None, None);
}

#[test]
fn strip_empty_text() {
    prefix(OFF, b"*", b"", Some(0), Some(0));
    suffix(OFF, b"*", b"", Some(0), Some(0));
    prefix(OFF, b"", b"", Some(0), Some(0));
    suffix(OFF, b"", b"", Some(0), Some(0));
}

#[test]
fn strip_extglob_alternation() {
    // v=abc: ${v#@(a|ab)} = bc (1), ${v##@(a|ab)} = c (2).
    prefix(EXT, b"@(a|ab)", b"abc", Some(1), Some(2));
    // ${v%@(c|bc)} = ab (start 2), ${v%%@(c|bc)} = a (start 1).
    suffix(EXT, b"@(c|bc)", b"abc", Some(2), Some(1));
    // ${v#@(a|ab)} has no suffix match at all.
    suffix(EXT, b"@(a|ab)", b"abc", None, None);
    // ${v%@(c|bc)} has no prefix match at all.
    prefix(EXT, b"@(c|bc)", b"abc", None, None);
}

#[test]
fn strip_extglob_repetition() {
    // v=abc: ${v#*(a|b)} = abc (0), ${v##*(a|b)} = c (2).
    prefix(EXT, b"*(a|b)", b"abc", Some(0), Some(2));
    // The only matching suffix is the empty one: both forms leave v alone.
    suffix(EXT, b"*(a|b)", b"abc", Some(3), Some(3));
    // ${v#+(a|)} = abc (0, the empty alternative), ${v##+(a|)} = bc (1).
    prefix(EXT, b"+(a|)", b"abc", Some(0), Some(1));
}

#[test]
fn strip_extglob_negation() {
    // v=abc: ${v#!(a)*} = abc (0), ${v##!(a)*} = "" (3).
    prefix(EXT, b"!(a)*", b"abc", Some(0), Some(3));
    // ${v%!(a)*} = abc (3), ${v%%!(a)*} = "" (0).
    suffix(EXT, b"!(a)*", b"abc", Some(3), Some(0));
    // !(!(a)) is exactly `a`: prefix only, suffix never.
    prefix(EXT, b"!(!(a))", b"abc", Some(1), Some(1));
    suffix(EXT, b"!(!(a))", b"abc", None, None);
    // The rest of the pattern constrains the negated span.
    prefix(EXT, b"a!(b)c", b"abc", None, None);
    suffix(EXT, b"a!(b)c", b"abc", None, None);
    // On "abbc" it does match the whole string.
    prefix(EXT, b"a!(b)c", b"abbc", Some(4), Some(4));
    suffix(EXT, b"a!(b)c", b"abbc", Some(0), Some(0));
}

#[test]
fn strip_nocasematch() {
    // The engine folds when asked. bash 5.3.20 does *not* honour
    // `shopt -s nocasematch` for `${x#pat}`/`${x%pat}` (only for `${x/a/b}`),
    // so the shell's strip consumer must pass `nocasematch: false`.
    let nocase = Flags {
        nocasematch: true,
        ..Flags::default()
    };
    prefix(nocase, b"*B", b"aabbccdd", Some(3), Some(4));
    suffix(nocase, b"C*", b"aabbccdd", Some(5), Some(4));
    // Without the flag these do not match.
    prefix(OFF, b"*B", b"aabbccdd", None, None);
    suffix(OFF, b"C*", b"aabbccdd", None, None);
}

#[test]
fn prefix_and_suffix_offsets_are_character_boundaries() {
    // "μ-" is 2 + 1 bytes.
    let text = "μ-".as_bytes();
    prefix(OFF, b"?", text, Some(2), Some(2));
    prefix(OFF, b"??", text, Some(3), Some(3));
    // The suffix of one character starts at byte 2.
    suffix(OFF, b"?", text, Some(2), Some(2));
    suffix(OFF, b"??", text, Some(0), Some(0));
    // "-μ": the last unit starts at byte 1.
    suffix(OFF, b"?", "-μ".as_bytes(), Some(1), Some(1));
}
