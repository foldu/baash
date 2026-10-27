//! Character units for pattern matching.
//!
//! Shell strings are bytes, but bash (in a UTF-8 locale) treats `?`, `*` and
//! bracket expressions as operating on characters, not bytes: `?` matches
//! `μ`. See the crate docs for the exact contract. A unit is one well-formed
//! UTF-8 scalar value, or one invalid byte. Invalid bytes are therefore never
//! silently merged into a neighbouring character and non-UTF-8 filenames still
//! match byte for byte.

/// Byte length of the UTF-8 character starting at `i`.
///
/// Returns `1` for a continuation byte, an overlong/surrogate/out-of-range
/// sequence, or a truncated sequence: such a byte stands alone. The caller
/// must ensure `i < bytes.len()`.
pub(crate) fn char_len(bytes: &[u8], i: usize) -> usize {
    let b0 = bytes[i];
    let n = match b0 {
        0x00..=0x7F => 1,
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return 1,
    };
    if i + n > bytes.len() {
        return 1;
    }
    if n == 1 {
        return 1;
    }
    let continuation = |k: usize| (0x80..=0xBF).contains(&bytes[i + k]);
    let first_ok = match b0 {
        0xC2..=0xDF => continuation(1),
        // E0: no overlong; ED: no surrogate half.
        0xE0 => (0xA0..=0xBF).contains(&bytes[i + 1]),
        0xED => (0x80..=0x9F).contains(&bytes[i + 1]),
        // F0: no overlong; F4: no scalar above U+10FFFF.
        0xF0 => (0x90..=0xBF).contains(&bytes[i + 1]),
        0xF4 => (0x80..=0x8F).contains(&bytes[i + 1]),
        _ => continuation(1),
    };
    if !first_ok {
        return 1;
    }
    for k in 2..n {
        if !continuation(k) {
            return 1;
        }
    }
    n
}

/// The scalar value of a unit that is exactly one well-formed character.
///
/// `None` for invalid bytes and for slices that are not exactly one character.
pub(crate) fn scalar(unit: &[u8]) -> Option<char> {
    let s = std::str::from_utf8(unit).ok()?;
    let mut chars = s.chars();
    let c = chars.next()?;
    chars.next().is_none().then_some(c)
}

/// The same scalar value with ASCII case folded, for `nocasematch` ranges.
///
/// Folding is per byte and only affects ASCII, so the result stays valid
/// UTF-8 and non-ASCII characters are unchanged. A unit is at most four bytes.
pub(crate) fn scalar_folded(unit: &[u8]) -> Option<char> {
    if unit.len() > 4 {
        return None;
    }
    let mut buf = [0u8; 4];
    for (dst, src) in buf.iter_mut().zip(unit) {
        *dst = src.to_ascii_lowercase();
    }
    scalar(&buf[..unit.len()])
}

/// A text viewed as a sequence of character units, in either direction.
///
/// Forward positions are byte offsets into the string. A reversed text has the
/// same units in reverse order, so position `i` means "`i` units from the
/// end"; [`Text::offset`] maps that back to the original byte offset. Reversal
/// is how suffix matching (`${x%pat}`) reuses the forward engine.
pub(crate) struct Text<'a> {
    bytes: &'a [u8],
    /// Byte offsets of every unit boundary; length is `len() + 1`.
    bounds: Vec<usize>,
    reverse: bool,
}

impl<'a> Text<'a> {
    /// A text read from the start.
    pub(crate) fn forward(bytes: &'a [u8]) -> Self {
        Self::new(bytes, false)
    }

    /// A text read from the end, unit by unit.
    pub(crate) fn reverse(bytes: &'a [u8]) -> Self {
        Self::new(bytes, true)
    }

    fn new(bytes: &'a [u8], reverse: bool) -> Self {
        let mut bounds = Vec::with_capacity(bytes.len() + 1);
        bounds.push(0);
        let mut i = 0;
        while i < bytes.len() {
            i += char_len(bytes, i);
            bounds.push(i);
        }
        Self {
            bytes,
            bounds,
            reverse,
        }
    }

    /// Number of character units.
    pub(crate) fn len(&self) -> usize {
        self.bounds.len() - 1
    }

    /// Unit `i`. `i` must be less than [`Text::len`].
    pub(crate) fn unit(&self, i: usize) -> &'a [u8] {
        let k = if self.reverse { self.len() - 1 - i } else { i };
        &self.bytes[self.bounds[k]..self.bounds[k + 1]]
    }

    /// Original byte offset of boundary `i` in this text's direction.
    pub(crate) fn offset(&self, i: usize) -> usize {
        if self.reverse {
            self.bounds[self.len() - i]
        } else {
            self.bounds[i]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_lengths() {
        assert_eq!(char_len(b"a", 0), 1);
        assert_eq!(char_len("μ".as_bytes(), 0), 2);
        assert_eq!(char_len("€".as_bytes(), 0), 3);
        assert_eq!(char_len("😀".as_bytes(), 0), 4);
    }

    #[test]
    fn invalid_bytes_are_single_units() {
        // Lone continuation, overlong, surrogate half, above U+10FFFF and
        // truncated sequences all stand alone.
        for bytes in [
            &[0x80u8][..],
            &[0xC0, 0xAF],
            &[0xED, 0xA0, 0x80],
            &[0xF4, 0x90, 0x80, 0x80],
            &[0xE2, 0x82],
            &[0xFF],
        ] {
            assert_eq!(char_len(bytes, 0), 1, "{bytes:02x?}");
        }
    }

    #[test]
    fn invalid_first_byte_does_not_consume_valid_tail() {
        // E2 82 AC is €; dropping the last byte makes the whole prefix invalid,
        // so E2 and 82 each stand alone and AC is a stray continuation.
        let bytes = &[0xE2, 0x82, 0xFF][..];
        assert_eq!(char_len(bytes, 0), 1);
        assert_eq!(char_len(bytes, 1), 1);
        assert_eq!(char_len(bytes, 2), 1);
    }

    #[test]
    fn offsets_forward_and_reverse() {
        let bytes = "aμ".as_bytes(); // a = 1 byte, μ = 2 bytes
        let f = Text::forward(bytes);
        assert_eq!(f.len(), 2);
        assert_eq!(f.unit(0), b"a");
        assert_eq!(f.unit(1), "μ".as_bytes());
        assert_eq!(f.offset(0), 0);
        assert_eq!(f.offset(2), 3);

        let r = Text::reverse(bytes);
        assert_eq!(r.len(), 2);
        assert_eq!(r.unit(0), "μ".as_bytes());
        assert_eq!(r.unit(1), b"a");
        assert_eq!(r.offset(0), 3);
        assert_eq!(r.offset(1), 1);
        assert_eq!(r.offset(2), 0);
    }

    #[test]
    fn scalars() {
        assert_eq!(scalar(b"a"), Some('a'));
        assert_eq!(scalar("μ".as_bytes()), Some('μ'));
        assert_eq!(scalar(b""), None);
        assert_eq!(scalar(b"ab"), None);
        assert_eq!(scalar(&[0xFF]), None);
        assert_eq!(scalar_folded(b"A"), Some('a'));
        assert_eq!(scalar_folded("É".as_bytes()), Some('É'));
    }
}
