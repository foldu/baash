//! Display cells: bytes in, terminal cells out.
//!
//! Shell text is bytes, but a terminal is measured in columns. This module is
//! the one place that turns the former into the latter, so the editor's cursor
//! arithmetic and the renderer agree about how wide a character is.
//!
//! The transform is deliberate about the awkward cases:
//!
//! * a tab becomes four spaces, so a column is a number and not a function of
//!   where the tab started;
//! * a control byte becomes caret notation (`^J`), so a pasted newline cannot
//!   move the terminal's cursor behind the editor's back;
//! * an invalid byte stays itself and counts as one cell, because bytes out is
//!   the rule and the terminal will draw a replacement glyph there anyway;
//! * a zero-width character is kept in the byte stream but consumes no cells.

use std::ops::Range;

use unicode_width::UnicodeWidthChar;

/// How many columns a tab occupies.
pub const TAB_WIDTH: usize = 4;

/// One source character and what it looks like on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    /// The source byte range this segment came from.
    pub src: Range<usize>,
    /// The range in [`Expanded::bytes`] holding the display bytes.
    pub display: Range<usize>,
    /// The number of cells the segment occupies.
    pub width: usize,
}

/// A line transformed for display, with the source offsets mapped onto display
/// offsets and columns.
#[derive(Clone, Debug, Default)]
pub struct Expanded {
    bytes: Vec<u8>,
    segments: Vec<Segment>,
    src_to_column: Vec<usize>,
    width: usize,
}

impl Expanded {
    /// Expand `src` into display bytes and edges.
    pub fn new(src: &[u8]) -> Self {
        let mut expanded = Expanded {
            bytes: Vec::with_capacity(src.len()),
            segments: Vec::with_capacity(src.len()),
            src_to_column: vec![0; src.len() + 1],
            width: 0,
        };
        let mut at = 0;
        while at < src.len() {
            let (bytes, length, width) = display_char(src, at);
            let display_start = expanded.bytes.len();
            expanded.src_to_column[at] = expanded.width;
            expanded.bytes.extend_from_slice(&bytes);
            // Interior offsets of a multi-byte character (which the editor's
            // grapheme-aware movement never lands on) map to the character's
            // start, so a cursor cannot render in the middle of a scalar.
            for inner in (at + 1)..(at + length) {
                expanded.src_to_column[inner] = expanded.width;
            }
            expanded.segments.push(Segment {
                src: at..at + length,
                display: display_start..expanded.bytes.len(),
                width,
            });
            expanded.width += width;
            at += length;
        }
        expanded.src_to_column[src.len()] = expanded.width;
        expanded
    }

    /// The display bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The per-character segments, in source order.
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// Total display width in cells.
    #[cfg(test)]
    pub fn width(&self) -> usize {
        self.width
    }

    /// The display column for a source byte offset.
    #[cfg(test)]
    pub fn column_at(&self, src: usize) -> usize {
        self.src_to_column.get(src).copied().unwrap_or(self.width)
    }
}

/// The display bytes, source length and cell width for the character at `at`.
fn display_char(src: &[u8], at: usize) -> (Vec<u8>, usize, usize) {
    match decode_char(&src[at..]) {
        None => (vec![src[at]], 1, 1),
        Some((character, length)) => {
            if character == '\t' {
                (vec![b' '; TAB_WIDTH], 1, TAB_WIDTH)
            } else if (character as u32) < 0x20 || character == '\u{7f}' {
                (vec![b'^', (character as u32 as u8) ^ 0x40], 1, 2)
            } else if character.is_control() {
                (vec![b'?'], length, 1)
            } else {
                let mut buffer = [0u8; 4];
                let text = character.encode_utf8(&mut buffer);
                let width = UnicodeWidthChar::width(character).unwrap_or(0);
                (text.as_bytes().to_vec(), length, width)
            }
        }
    }
}

/// Decode one UTF-8 scalar from the start of `bytes`.
fn decode_char(bytes: &[u8]) -> Option<(char, usize)> {
    let length = match bytes.first()? {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    };
    let slice = bytes.get(..length)?;
    let text = std::str::from_utf8(slice).ok()?;
    text.chars().next().map(|character| (character, length))
}

/// The display width of `text`, for menu layout.
pub fn width_of(text: &str) -> usize {
    text.chars()
        .map(|character| UnicodeWidthChar::width(character).unwrap_or(0))
        .sum()
}

/// Cut `text` to at most `columns` cells without splitting a character.
pub fn truncate(text: &str, columns: usize) -> String {
    let mut out = String::new();
    let mut width = 0;
    for character in text.chars() {
        let cell = UnicodeWidthChar::width(character).unwrap_or(0);
        if width + cell > columns {
            break;
        }
        out.push(character);
        width += cell;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_one_cell_per_byte() {
        let expanded = Expanded::new(b"ls -l");
        assert_eq!(expanded.bytes(), b"ls -l");
        assert_eq!(expanded.width(), 5);
        assert_eq!(expanded.column_at(0), 0);
        assert_eq!(expanded.column_at(3), 3);
        assert_eq!(expanded.column_at(5), 5);
    }

    #[test]
    fn a_wide_character_takes_two_cells() {
        let expanded = Expanded::new("你好".as_bytes());
        assert_eq!(expanded.width(), 4);
        assert_eq!(expanded.column_at(3), 2, "after the first ideograph");
        assert_eq!(expanded.column_at(6), 4);
    }

    #[test]
    fn a_combining_mark_takes_no_cell() {
        let text = "e\u{301}";
        let expanded = Expanded::new(text.as_bytes());
        assert_eq!(expanded.width(), 1);
        assert_eq!(expanded.column_at(text.len()), 1);
    }

    #[test]
    fn a_tab_is_four_cells() {
        let expanded = Expanded::new(b"a\tb");
        assert_eq!(expanded.bytes(), b"a    b");
        assert_eq!(expanded.width(), 6);
        assert_eq!(expanded.column_at(2), 5);
    }

    #[test]
    fn control_bytes_become_caret_notation() {
        let expanded = Expanded::new(b"a\nb");
        assert_eq!(expanded.bytes(), b"a^Jb");
        assert_eq!(expanded.width(), 4);
        assert!(!expanded.bytes().contains(&b'\n'));
    }

    #[test]
    fn invalid_bytes_survive_and_count_once() {
        let expanded = Expanded::new(b"a\xffb");
        assert_eq!(expanded.bytes(), b"a\xffb");
        assert_eq!(expanded.width(), 3);
    }

    #[test]
    fn truncation_does_not_split_a_character() {
        assert_eq!(truncate("你好", 3), "你");
        assert_eq!(truncate("abc", 2), "ab");
        assert_eq!(truncate("你好", 4), "你好");
    }
}
