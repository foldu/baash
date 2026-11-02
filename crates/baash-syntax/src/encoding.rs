//! How a script's bytes become the text the parser reads.
//!
//! A script written on Windows is not the script Linux sees: an editor may
//! prefix a byte order mark, and a text file's line endings are `\r\n`. Both
//! turn ordinary input into a syntax error that looks like a shell bug — the
//! `for` of a UTF-8-BOM script is the word `\u{feff}for`, so the `do` that
//! follows it is "unexpected", and a CRLF script finds `do\r` where it wanted
//! `do`.
//!
//! So the encoding is settled once, before parsing, in one place. This is
//! deliberately *not* in the lexer: the lexer's job is to be lossless about
//! the bytes it is given, and a shell that silently rewrites a string it was
//! asked to parse would be lying to `--parse-only` and to every caller that
//! reuses the tree.
//!
//! What is *not* changed: a lone `\r` stays a byte of the line (it is a
//! character in a word, as it is for bash), and nothing after the mark is
//! reinterpreted — a UTF-8 file stays UTF-8, byte for byte.

use std::borrow::Cow;

/// A byte order mark, and the encoding it announces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mark {
    Utf8,
    Utf16Le,
    Utf16Be,
    Utf32Le,
    Utf32Be,
}

/// The encoding a byte order mark announces, longest mark first.
///
/// `FF FE 00 00` is a UTF-32LE mark and not a UTF-16LE one followed by a NUL,
/// so the four-byte marks are checked before the two-byte ones that share
/// their first half.
fn mark(bytes: &[u8]) -> Option<(Mark, usize)> {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        Some((Mark::Utf8, 3))
    } else if bytes.starts_with(&[0xFF, 0xFE, 0x00, 0x00]) {
        Some((Mark::Utf32Le, 4))
    } else if bytes.starts_with(&[0x00, 0x00, 0xFE, 0xFF]) {
        Some((Mark::Utf32Be, 4))
    } else if bytes.starts_with(&[0xFF, 0xFE]) {
        Some((Mark::Utf16Le, 2))
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        Some((Mark::Utf16Be, 2))
    } else {
        None
    }
}

/// Decode a script into the text the parser reads.
///
/// * a byte order mark is consumed, and UTF-16 or UTF-32 text is transcoded to
///   UTF-8, lossily — an unpaired surrogate becomes `U+FFFD`, because a script
///   that parses with one odd character is better than one that does not parse
///   at all, and the replacement is visible in the echoed output;
/// * `\r\n` becomes `\n`.
///
/// Text that is already plain UTF-8 with LF endings is returned borrowed.
pub fn decode(bytes: &[u8]) -> Cow<'_, [u8]> {
    let text = match mark(bytes) {
        None => Cow::Borrowed(bytes),
        Some((Mark::Utf8, skip)) => Cow::Borrowed(&bytes[skip..]),
        Some((Mark::Utf16Le, skip)) => Cow::Owned(utf16(&bytes[skip..], u16::from_le_bytes)),
        Some((Mark::Utf16Be, skip)) => Cow::Owned(utf16(&bytes[skip..], u16::from_be_bytes)),
        Some((Mark::Utf32Le, skip)) => Cow::Owned(utf32(&bytes[skip..], u32::from_le_bytes)),
        Some((Mark::Utf32Be, skip)) => Cow::Owned(utf32(&bytes[skip..], u32::from_be_bytes)),
    };
    normalize_newlines(text)
}

/// Transcode little- or big-endian UTF-16 to UTF-8, lossily.
///
/// A byte left over from an odd length is dropped: it cannot be half of a
/// character, and the rest of the script is still worth running.
fn utf16(bytes: &[u8], word: fn([u8; 2]) -> u16) -> Vec<u8> {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| word([pair[0], pair[1]]))
        .collect();
    String::from_utf16_lossy(&units).into_bytes()
}

/// Transcode little- or big-endian UTF-32 to UTF-8, lossily.
///
/// A `u32` that is not a scalar value becomes `U+FFFD`, as in [`utf16`].
fn utf32(bytes: &[u8], word: fn([u8; 4]) -> u32) -> Vec<u8> {
    let mut text = String::with_capacity(bytes.len() / 2);
    for quad in bytes.chunks_exact(4) {
        let scalar = word([quad[0], quad[1], quad[2], quad[3]]);
        text.push(char::from_u32(scalar).unwrap_or(char::REPLACEMENT_CHARACTER));
    }
    text.into_bytes()
}

/// Replace every `\r\n` with `\n`.
///
/// Windows line endings are the only thing this recognises: a bare `\r` is a
/// character, which is what bash does with it too (a script that separates
/// lines with a lone CR is a Mac OS 9 artifact, and treating it as a newline
/// would change what `printf '\r'` prints).
fn normalize_newlines(text: Cow<'_, [u8]>) -> Cow<'_, [u8]> {
    if !text.windows(2).any(|pair| pair == b"\r\n") {
        return text;
    }
    let mut out = Vec::with_capacity(text.len());
    let mut rest = text.as_ref();
    while let Some(index) = rest.windows(2).position(|pair| pair == b"\r\n") {
        out.extend_from_slice(&rest[..index]);
        out.push(b'\n');
        rest = &rest[index + 2..];
    }
    out.extend_from_slice(rest);
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_utf8_is_borrowed_unchanged() {
        let script = b"for i in 1 2; do echo $i; done\n";
        assert!(matches!(decode(script), Cow::Borrowed(_)));
        assert_eq!(decode(script).as_ref(), script);
    }

    #[test]
    fn a_utf8_bom_is_dropped() {
        assert_eq!(
            decode(b"\xEF\xBB\xBFecho hi\n").as_ref(),
            b"echo hi\n",
            "Notepad's UTF-8 is a BOM and then the script"
        );
    }

    #[test]
    fn a_utf16le_script_is_transcoded() {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "echo hi\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode(&bytes).as_ref(), b"echo hi\n");
    }

    #[test]
    fn a_utf16be_script_is_transcoded() {
        let mut bytes = vec![0xFE, 0xFF];
        for unit in "echo hi\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_be_bytes());
        }
        assert_eq!(decode(&bytes).as_ref(), b"echo hi\n");
    }

    #[test]
    fn a_utf32_bom_is_not_mistaken_for_utf16() {
        // `FF FE 00 00` starts with the UTF-16LE mark; only the longer check
        // keeps the NULs out of the text.
        let mut bytes = vec![0xFF, 0xFE, 0x00, 0x00];
        for scalar in "echo hi\n".chars() {
            bytes.extend_from_slice(&(scalar as u32).to_le_bytes());
        }
        assert_eq!(decode(&bytes).as_ref(), b"echo hi\n");
    }

    #[test]
    fn an_unpaired_surrogate_becomes_a_replacement() {
        let bytes = [0xFF, 0xFE, 0x00, 0xD8, b'x', 0];
        assert_eq!(decode(&bytes).as_ref(), "\u{FFFD}x".as_bytes());
    }

    #[test]
    fn crlf_becomes_lf() {
        assert_eq!(
            decode(b"for i in 1 2; do\r\n\techo $i\r\ndone\r\n").as_ref(),
            b"for i in 1 2; do\n\techo $i\ndone\n"
        );
    }

    #[test]
    fn a_lone_cr_is_a_character() {
        assert_eq!(decode(b"printf 'a\rb'").as_ref(), b"printf 'a\rb'");
    }

    #[test]
    fn a_bom_and_crlf_are_both_settled() {
        assert_eq!(
            decode(b"\xEF\xBB\xBFecho hi\r\n").as_ref(),
            b"echo hi\n",
            "PowerShell 5 writes both at once"
        );
    }
}
