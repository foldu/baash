//! WTF-8: the byte encoding of Windows' UTF-16 names.
//!
//! The shell's text is bytes on purpose (`docs/portability.md`), and Unix
//! names *are* bytes. Windows names are UTF-16, and that encoding is wider
//! than UTF-8 in one specific way: it can hold an unpaired surrogate, which no
//! Unicode scalar encodes. NTFS permits such names, and the Win32 API creates
//! them happily — so a shell that converts with `to_string_lossy` silently
//! renames them to U+FFFD, and then fails to open or delete the file it was
//! asked about.
//!
//! [WTF-8] is the encoding that closes that gap: UTF-8 extended so the three
//! bytes `ED A0 80` through `ED BF BF` are a lone surrogate code point. It is
//! a superset of UTF-8, and the encoding is specified and frozen, so the bytes
//! can be stored — in a variable, in `PWD`, in the history table — and read
//! back later without a version of this crate having to agree with itself.
//!
//! Two rules keep the codec honest:
//!
//! * a *pair* of surrogates is not two three-byte sequences; it is combined
//!   into the four-byte encoding of the scalar they spell, exactly as UTF-8
//!   would. [`encode`] combines, and [`decode`] therefore never has to guess
//!   whether two adjacent lone surrogates were meant as a pair;
//! * bytes that are neither UTF-8 nor that one extension cannot name anything
//!   on Windows at all, so [`decode`] turns them into U+FFFD rather than
//!   inventing an escape. That is the one place it is lossy, and it is the
//!   direction the OS cannot be lied to about.
//!
//! [WTF-8]: https://simonsapin.github.io/wtf-8/

/// U+FFFD, which [`decode`] substitutes for bytes that name nothing.
const REPLACEMENT: u16 = 0xFFFD;

/// Encode UTF-16 code units as WTF-8 bytes.
///
/// A well-formed surrogate pair becomes one four-byte sequence; a lone
/// surrogate becomes its three-byte form; everything else is ordinary UTF-8.
/// The encoding is lossless: [`decode`] returns the same units.
pub fn encode(units: impl IntoIterator<Item = u16>) -> Vec<u8> {
    let mut units = units.into_iter().peekable();
    let mut out = Vec::new();
    while let Some(unit) = units.next() {
        if (0xD800..=0xDBFF).contains(&unit)
            && let Some(&next) = units.peek()
            && (0xDC00..=0xDFFF).contains(&next)
        {
            units.next();
            // The scalar a pair spells: `0x10000 + (high - D800) << 10 |
            // (low - DC00)`, spelled with the four-byte form.
            let high = u32::from(unit) - 0xD800;
            let low = u32::from(next) - 0xDC00;
            push_scalar(&mut out, 0x10000 + (high << 10) + low);
        } else {
            // Non-surrogates and lone surrogates are both BMP code points, and
            // the one-, two- and three-byte forms cover the whole BMP.
            push_scalar(&mut out, u32::from(unit));
        }
    }
    out
}

/// Decode WTF-8 bytes into UTF-16 code units.
///
/// Well-formed WTF-8 round-trips exactly. A byte sequence that is not WTF-8 —
/// an overlong form, a truncated sequence, a stray continuation byte, a scalar
/// above U+10FFFF — becomes U+FFFD, one byte at a time, so the result always
/// fits in a Windows name.
pub fn decode(bytes: &[u8]) -> Vec<u16> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match code_point_at(bytes, index) {
            Some((length, code_point)) => {
                if code_point >= 0x1_0000 {
                    // Four-byte forms are the astral planes: spell them as the
                    // surrogate pair Windows stores.
                    let scalar = code_point - 0x1_0000;
                    out.push(0xD800 + (scalar >> 10) as u16);
                    out.push(0xDC00 + (scalar & 0x3FF) as u16);
                } else {
                    out.push(code_point as u16);
                }
                index += length;
            }
            None => {
                out.push(REPLACEMENT);
                index += 1;
            }
        }
    }
    out
}

/// Append the WTF-8 spelling of `code_point`.
///
/// Surrogate code points take the three-byte branch with everything else in
/// the BMP: that is exactly the WTF-8 extension, so there is no special case.
fn push_scalar(out: &mut Vec<u8>, code_point: u32) {
    match code_point {
        0x0000..=0x007F => out.push(code_point as u8),
        0x0080..=0x07FF => {
            out.push(0xC0 | (code_point >> 6) as u8);
            out.push(0x80 | (code_point & 0x3F) as u8);
        }
        0x0800..=0xFFFF => {
            out.push(0xE0 | (code_point >> 12) as u8);
            out.push(0x80 | ((code_point >> 6) & 0x3F) as u8);
            out.push(0x80 | (code_point & 0x3F) as u8);
        }
        _ => {
            out.push(0xF0 | (code_point >> 18) as u8);
            out.push(0x80 | ((code_point >> 12) & 0x3F) as u8);
            out.push(0x80 | ((code_point >> 6) & 0x3F) as u8);
            out.push(0x80 | (code_point & 0x3F) as u8);
        }
    }
}

/// The code point starting at `at`, and how many bytes spell it.
///
/// This is UTF-8's decoder plus the surrogate range: `ED A0 80`–`ED BF BF`
/// decode as the code points they are, where strict UTF-8 would reject them.
/// Overlong forms and truncation are rejected, so there is exactly one encoding
/// of every WTF-8 string.
fn code_point_at(bytes: &[u8], at: usize) -> Option<(usize, u32)> {
    let lead = bytes[at];
    let (length, minimum) = match lead {
        0x00..=0x7F => return Some((1, u32::from(lead))),
        0xC2..=0xDF => (2, 0x80),
        0xE0..=0xEF => (3, 0x0800),
        0xF0..=0xF4 => (4, 0x1_0000),
        // `0xC0`/`0xC1` are always overlong, `0xF5`.. cannot occur in Unicode,
        // and a continuation byte cannot start a sequence.
        _ => return None,
    };
    let slice = bytes.get(at..at + length)?;
    let mut code_point = u32::from(lead & (0x7F >> length));
    for &byte in &slice[1..] {
        if byte & 0xC0 != 0x80 {
            return None;
        }
        code_point = (code_point << 6) | u32::from(byte & 0x3F);
    }
    if code_point < minimum || code_point > 0x10_FFFF {
        return None;
    }
    Some((length, code_point))
}

#[cfg(test)]
mod tests {
    use super::{decode, encode};

    #[test]
    fn ascii_and_bmp_are_utf8() {
        for text in [
            "",
            "a",
            "hello",
            "Grüße",
            "日本",
            "\u{7F}\u{80}\u{7FF}\u{800}",
        ] {
            let units: Vec<u16> = text.encode_utf16().collect();
            assert_eq!(encode(units.iter().copied()), text.as_bytes(), "{text:?}");
            assert_eq!(decode(text.as_bytes()), units, "{text:?}");
        }
    }

    #[test]
    fn astral_planes_use_the_four_byte_form() {
        let text = "😀";
        let units: Vec<u16> = text.encode_utf16().collect();
        assert_eq!(units, [0xD83D, 0xDE00]);
        assert_eq!(encode(units.iter().copied()), text.as_bytes());
        assert_eq!(decode(text.as_bytes()), units);
    }

    #[test]
    fn a_lone_surrogate_is_its_three_byte_form() {
        // The four boundaries of the surrogate range: the two that can open a
        // pair and the two that can close one.
        for (unit, expected) in [
            (0xD800u16, [0xED, 0xA0, 0x80]),
            (0xDBFF, [0xED, 0xAF, 0xBF]),
            (0xDC00, [0xED, 0xB0, 0x80]),
            (0xDFFF, [0xED, 0xBF, 0xBF]),
        ] {
            assert_eq!(encode([unit]), expected, "{unit:04X}");
            assert_eq!(decode(&expected), [unit], "{unit:04X}");
        }
    }

    #[test]
    fn every_code_unit_round_trips() {
        // A lone surrogate, an ASCII byte, a BMP character and a pair all have
        // to come back as the units they were, and a single unit can never be
        // mistaken for part of its neighbour.
        for unit in 0..=0xFFFFu16 {
            let bytes = encode([unit]);
            assert_eq!(decode(&bytes), [unit], "U+{unit:04X}");
        }
    }

    #[test]
    fn pairs_are_combined_and_never_split() {
        // Two adjacent units, including every interesting crossing: pair,
        // lone-high + lone-low is indistinguishable from a pair (which is why
        // encoding combines them), and a high followed by a non-low stays two
        // independent units.
        for (high, low) in [
            (0xD800u16, 0xDC00u16),
            (0xD83D, 0xDE00),
            (0xDBFF, 0xDFFF),
            (0xD800, 0xD800),
            (0xDC00, 0xDC00),
            (0xD800, 0x0041),
            (0x0041, 0xDC00),
        ] {
            let bytes = encode([high, low]);
            assert_eq!(decode(&bytes), [high, low], "{high:04X} {low:04X}");
        }
        // The pair spelling is the standard four-byte form, not two surrogates.
        assert_eq!(encode([0xD83D, 0xDE00]), "😀".as_bytes());
    }

    #[test]
    fn text_mixing_surrogates_and_characters_round_trips() {
        let units = [0x0041u16, 0xD800, 0x20AC, 0xDC00, 0xD83D, 0xDE00];
        let bytes = encode(units);
        assert_eq!(decode(&bytes), units);
        // The pair in the middle is one four-byte sequence, the lone high and
        // lone low are three bytes each: check the lengths, so a codec that
        // silently split the pair would fail here.
        assert_eq!(bytes.len(), 1 + 3 + 3 + 3 + 4);
    }

    #[test]
    fn bytes_that_name_nothing_become_the_replacement_character() {
        // 0xFF is not UTF-8 and not the surrogate form.
        assert_eq!(decode(&[0xFF]), [0xFFFD]);
        // A continuation byte cannot start a sequence.
        assert_eq!(decode(&[0x80]), [0xFFFD]);
        // An overlong form: `C0 AF` spells `/` but must not be accepted.
        assert_eq!(
            decode(&[b'a', 0xC0, 0xAF, b'b']),
            [0x61, 0xFFFD, 0xFFFD, 0x62]
        );
        // A truncated three-byte sequence.
        assert_eq!(decode(&[0xE2, 0x82]), [0xFFFD, 0xFFFD]);
        // A scalar above U+10FFFF.
        assert_eq!(decode(&[0xF4, 0x90, 0x80, 0x80]), [0xFFFD; 4]);
        // A four-byte spelling of a surrogate is not WTF-8 either: surrogates
        // have exactly one form.
        assert_eq!(decode(&[0xF0, 0x8D, 0xA0, 0x80]), [0xFFFD; 4]);
    }

    #[test]
    fn the_decoder_never_panics_on_short_input() {
        for byte in 0..=u8::MAX {
            let _ = decode(&[byte]);
        }
        for first in 0..=u8::MAX {
            for second in 0..=u8::MAX {
                let _ = decode(&[first, second]);
            }
        }
    }

    #[test]
    fn the_decoder_accepts_every_three_byte_form_wtf8_allows() {
        // Every `ED A0 80`–`ED BF BF` is a surrogate and must come back as
        // that unit, not as U+FFFD.
        for unit in 0xD800..=0xDFFFu16 {
            let bytes = encode([unit]);
            assert_eq!(bytes.len(), 3);
            assert_eq!(bytes[0], 0xED, "{unit:04X}");
            assert!(
                (0xA0..=0xBF).contains(&bytes[1]),
                "{unit:04X}: {:02X}",
                bytes[1]
            );
            assert_eq!(decode(&bytes), [unit]);
        }
    }
}
