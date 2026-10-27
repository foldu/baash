//! The twelve POSIX character classes, over ASCII.
//!
//! `[:alpha:]` and friends are deliberately locale-independent here; see the
//! crate docs. The test table pins both the positive and the negative side of
//! every class, including the boundaries that are easy to get one off
//! (`[:cntrl:]` vs `[:print:]`, `[:blank:]` vs `[:space:]`, `[:graph:]` vs
//! `[:print:]`).

use crate::{Flags, matches};

fn class(name: &str, byte: u8) -> bool {
    let pattern = format!("[[:{name}:]]");
    matches(pattern.as_bytes(), &[byte], Flags::default())
}

fn accepts(name: &str, bytes: &[u8]) {
    for &byte in bytes {
        assert!(class(name, byte), "[:{name}:] should accept {byte:#04x}");
    }
}

fn rejects(name: &str, bytes: &[u8]) {
    for &byte in bytes {
        assert!(!class(name, byte), "[:{name}:] should reject {byte:#04x}");
    }
}

#[test]
fn alpha() {
    accepts("alpha", b"azAZ");
    rejects("alpha", b"09_! \x00\x7f");
    // ASCII-only: the two-byte UTF-8 character `é` is not an ASCII letter.
    assert!(!matches(b"[[:alpha:]]", "é".as_bytes(), Flags::default()));
}

#[test]
fn digit() {
    accepts("digit", b"09");
    rejects("digit", b"aZ_");
}

#[test]
fn space() {
    accepts("space", b" \t\n\r\x0b\x0c");
    rejects("space", b"a0\x00\x7f");
}

#[test]
fn upper_and_lower_are_not_folded_by_nocasematch() {
    accepts("upper", b"AZ");
    rejects("upper", b"az0");
    accepts("lower", b"az");
    rejects("lower", b"AZ0");

    let nocase = Flags {
        nocasematch: true,
        ..Flags::default()
    };
    assert!(matches(b"[[:upper:]]", b"A", nocase));
    assert!(!matches(b"[[:upper:]]", b"a", nocase));
    assert!(matches(b"[[:lower:]]", b"a", nocase));
    assert!(!matches(b"[[:lower:]]", b"A", nocase));
}

#[test]
fn alnum() {
    accepts("alnum", b"aZ09");
    rejects("alnum", b"_! ");
}

#[test]
fn punct() {
    accepts("punct", b"!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~");
    rejects("punct", b"aZ09 \t");
}

#[test]
fn blank_is_space_plus_tab_only() {
    accepts("blank", b" \t");
    rejects("blank", b"\n\r\x0b\x0ca");
}

#[test]
fn xdigit() {
    accepts("xdigit", b"09afAF");
    rejects("xdigit", b"gGzZ");
}

#[test]
fn cntrl() {
    accepts("cntrl", &[0x00, 0x01, 0x09, 0x0a, 0x0d, 0x1f, 0x7f]);
    rejects("cntrl", b" ~a");
    // 0x80 is not ASCII control, and is not valid UTF-8 on its own.
    rejects("cntrl", &[0x80]);
}

#[test]
fn print_and_graph() {
    accepts("print", b" ~");
    rejects("print", &[0x1f, 0x7f]);
    accepts("graph", b"!~");
    rejects("graph", b" ");
    rejects("graph", &[0x7f]);
    // Non-ASCII characters are outside the ASCII classes here.
    assert!(!matches(b"[[:print:]]", "é".as_bytes(), Flags::default()));
    assert!(!matches(b"[[:graph:]]", "é".as_bytes(), Flags::default()));
}

#[test]
fn class_combined_with_literals_and_ranges() {
    assert!(matches(b"[[:digit:]abc]", b"5", Flags::default()));
    assert!(matches(b"[[:digit:]abc]", b"a", Flags::default()));
    assert!(!matches(b"[[:digit:]abc]", b"z", Flags::default()));
    assert!(matches(b"[![:digit:]]", b"a", Flags::default()));
    assert!(!matches(b"[![:digit:]]", b"5", Flags::default()));
    assert!(matches(b"[[:alpha:][:digit:]]", b"7", Flags::default()));
    assert!(matches(b"[a-z[:digit:]]", b"5", Flags::default()));
    assert!(matches(b"[a-z[:digit:]]", b"a", Flags::default()));
}

#[test]
fn an_unterminated_class_bracket_is_literal() {
    // The whole bracket is unterminated, so the first `[` is literal. This is
    // a malformed pattern; the useful property is that it does not panic.
    assert!(!matches(b"[[:alpha:]", b"a", Flags::default()));
    assert!(!matches(b"[[:alpha:]", b"[", Flags::default()));
}
