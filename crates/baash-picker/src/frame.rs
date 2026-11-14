//! One screen of the picker, as terminal bytes.
//!
//! The picker owns the whole screen, so a frame is not a repaint of part of it:
//! it is the complete picture, painted from the top-left corner. That is why
//! there is no line arithmetic here — no "move up to where the prompt was", no
//! remembering how tall the last frame was. Home the cursor, paint every row,
//! put the cursor back on the query. Every row ends with erase-to-end-of-line,
//! so a shorter row cannot leave the tail of a longer one behind.
//!
//! Two rows are not the list: the query at the top and the count pinned to the
//! bottom, the way fzf draws them. What is left goes to the candidates, and a
//! terminal reported as one row tall gets the query alone — painting a list
//! into a screen of unknown height is how a frame ends up scrolling its own
//! first row away.

use std::io::Write as _;

use unicode_width::UnicodeWidthChar;

use crate::pick::Picker;

/// `ESC [ ? 1049 h`: switch to the alternate screen and save the cursor.
///
/// The alternate screen is the whole point of a full-screen picker: the shell's
/// scrollback and the command line under the picker are untouched while it is
/// up, and the terminal puts them back when it goes away. Nothing the picker
/// paints can therefore end up in a transcript or a scrollback buffer.
pub const ENTER_SCREEN: &[u8] = b"\x1b[?1049h";

/// `ESC [ ? 1049 l`: back to the primary screen, cursor restored.
pub const LEAVE_SCREEN: &[u8] = b"\x1b[?1049l";

/// Home the cursor.
const HOME: &[u8] = b"\x1b[H";

/// Erase from the cursor to the end of the row.
const ERASE: &[u8] = b"\x1b[K";

/// Bold on and off, for the characters the query matched.
const BOLD: &[u8] = b"\x1b[1m";
const BOLD_OFF: &[u8] = b"\x1b[22m";

/// Reverse video, for the selected row.
const REVERSE: &[u8] = b"\x1b[7m";

/// Dim, for the prompt and the count.
const DIM: &[u8] = b"\x1b[2m";

/// SGR reset.
const RESET: &[u8] = b"\x1b[0m";

impl Picker {
    /// Paint one whole screen and leave the cursor on the query row.
    ///
    /// `columns` and `rows` are the terminal's size; a size it will not report
    /// is the caller's to substitute, because only the caller knows which
    /// fallback the rest of its rendering uses.
    pub fn frame(&mut self, columns: u16, rows: u16, color: bool) -> Vec<u8> {
        frame(self, columns, rows, color)
    }
}

/// The body of [`Picker::frame`], split out so the state and the painting have
/// separate borrows to work with.
fn frame(picker: &mut Picker, columns: u16, rows: u16, color: bool) -> Vec<u8> {
    let columns = usize::from(columns.max(1));
    let rows = usize::from(rows.max(1));
    // The count is pinned to the last row, so it is the first thing to go when
    // the terminal is one row tall.
    let list_height = rows.saturating_sub(2);
    // PageUp/PageDown move by what was last painted, so the caller does not
    // have to tell the state what the terminal looked like.
    picker.page = list_height.max(1);

    let mut out = Vec::with_capacity(columns * rows + 64);
    out.extend_from_slice(HOME);

    let prompt = truncate(&String::from_utf8_lossy(&picker.prompt), columns);
    let tail = query_tail(picker, columns.saturating_sub(width_of(&prompt)));
    paint_query(&mut out, &prompt, &tail, color);

    let (first, count) = window(picker, list_height);
    for index in first..(first + count) {
        out.extend_from_slice(b"\r\n");
        // The positions are computed for the rows that are painted and no
        // others: a list of a hundred thousand candidates costs the visible
        // twenty, not the list.
        let positions = picker.positions(index);
        let line = picker.hits[index].index;
        paint_row(
            &mut out,
            &picker.lines[line].display,
            &positions,
            index == picker.selected,
            columns,
            color,
        );
    }

    if rows >= 2 {
        let _ = write!(out, "\x1b[{rows};1H");
        paint_count(&mut out, picker, columns, color);
    }

    // The cursor belongs on the query row, after the last byte of the query —
    // the same place the line editor would leave it. The column is clamped on
    // the last cell: a cursor one past the right edge would sit on the next
    // row, or wrap.
    let column = (width_of(&prompt) + width_of(&tail)).min(columns.saturating_sub(1));
    let _ = write!(out, "\x1b[1;{}H", column + 1);
    out
}

/// Which matches are on screen, and how many.
///
/// The window follows the selection and moves only as far as it must, so
/// walking down a long list scrolls one row at a time instead of jumping.
fn window(picker: &mut Picker, height: usize) -> (usize, usize) {
    let count = picker.hits.len();
    let visible = height.min(count);
    if visible == 0 {
        picker.offset = 0;
        return (0, 0);
    }
    if picker.selected < picker.offset {
        picker.offset = picker.selected;
    } else if picker.selected >= picker.offset + visible {
        picker.offset = picker.selected + 1 - visible;
    }
    picker.offset = picker.offset.min(count - visible);
    (picker.offset, visible)
}

/// The query row: the prompt, then as much of the query's tail as fits.
///
/// The tail and not the head: the cursor sits at the end of the query, so a
/// query longer than the terminal has to scroll the way the line editor's does
/// rather than push the cursor off the screen.
fn paint_query(out: &mut Vec<u8>, prompt: &str, tail: &str, color: bool) {
    if color && !prompt.is_empty() {
        out.extend_from_slice(DIM);
    }
    out.extend_from_slice(prompt.as_bytes());
    if color && !prompt.is_empty() {
        out.extend_from_slice(RESET);
    }
    out.extend_from_slice(tail.as_bytes());
    out.extend_from_slice(ERASE);
}

/// The part of the query that fits in `width` cells, counted from its end.
fn query_tail(picker: &Picker, width: usize) -> String {
    let query = display_of(&picker.query);
    if width_of(&query) <= width {
        return query;
    }
    // Keep the last characters that fit. A wide character at the edge is
    // dropped rather than half painted.
    let mut kept: Vec<char> = Vec::new();
    let mut used = 0;
    for character in query.chars().rev() {
        let cell = cell_width(character);
        if used + cell > width {
            break;
        }
        used += cell;
        kept.push(character);
    }
    kept.iter().rev().collect()
}

/// One list row: the marker, then the line with its matched characters bold.
///
/// Matched characters are bold rather than a colour: the picker inherits the
/// user's palette, and bold is the emphasis every terminal can show. In a
/// reversed (selected) row bold still applies, which is why match-off is `22` —
/// `SGR 0` would cancel the reverse video too.
fn paint_row(
    out: &mut Vec<u8>,
    text: &str,
    positions: &[u32],
    selected: bool,
    columns: usize,
    color: bool,
) {
    let marker = truncate(if selected { "> " } else { "  " }, columns);
    if color && selected {
        out.extend_from_slice(REVERSE);
    }
    out.extend_from_slice(marker.as_bytes());
    let width = columns.saturating_sub(width_of(&marker));
    let mut used = 0;
    let mut bold = false;
    for (index, character) in text.chars().enumerate() {
        let cell = cell_width(character);
        if used + cell > width {
            break;
        }
        used += cell;
        let matched = positions.binary_search(&(index as u32)).is_ok();
        if color && matched != bold {
            out.extend_from_slice(if matched { BOLD } else { BOLD_OFF });
            bold = matched;
        }
        let mut buffer = [0u8; 4];
        out.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
    }
    if color && (bold || selected) {
        out.extend_from_slice(RESET);
    }
    out.extend_from_slice(ERASE);
}

/// The bottom row: how many candidates match, out of how many there are.
fn paint_count(out: &mut Vec<u8>, picker: &Picker, columns: usize, color: bool) {
    let text = if picker.matches() == 0 {
        String::from("  no match")
    } else {
        format!("  {}/{}", picker.matches(), picker.len())
    };
    let text = truncate(&text, columns);
    if color {
        out.extend_from_slice(DIM);
    }
    out.extend_from_slice(text.as_bytes());
    if color {
        out.extend_from_slice(RESET);
    }
    out.extend_from_slice(ERASE);
}

/// The screen form of `bytes`: one row, whatever control bytes it contains.
///
/// The same transform [`Picker`] uses for a candidate's display string, because
/// a query byte from a paste can be a control character too.
fn display_of(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for character in String::from_utf8_lossy(bytes).chars() {
        match character {
            '\n' | '\r' => out.push(' '),
            '\t' => out.push_str("    "),
            other if other.is_control() => out.push(' '),
            other => out.push(other),
        }
    }
    out
}

/// How many cells a character takes; a zero-width character still occupies the
/// bytes it is written in, so it is drawn and counted as zero.
fn cell_width(character: char) -> usize {
    UnicodeWidthChar::width(character).unwrap_or(0)
}

/// The width of `text` in cells.
fn width_of(text: &str) -> usize {
    text.chars().map(cell_width).sum()
}

/// The longest prefix of `text` that fits in `columns` cells.
fn truncate(text: &str, columns: usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut used = 0;
    for character in text.chars() {
        let cell = cell_width(character);
        if used + cell > columns {
            break;
        }
        used += cell;
        out.push(character);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Action, Picker};

    fn picker(lines: &[&str]) -> Picker {
        Picker::new(
            b"history> ",
            lines.iter().map(|line| line.as_bytes().to_vec()).collect(),
        )
    }

    /// The frame as text, row by row, with every escape sequence removed.
    ///
    /// Rows are separated by the `\r\n` the painter writes *and* by an
    /// absolute cursor move, which is how the count row is pinned to the
    /// bottom of the screen.
    fn rows_of(frame: &[u8], rows: usize) -> Vec<String> {
        let text = String::from_utf8_lossy(frame);
        let mut out = Vec::new();
        let mut row = String::new();
        let mut escape: Vec<char> = Vec::new();
        for character in text.chars() {
            if !escape.is_empty() {
                escape.push(character);
                if character.is_ascii_alphabetic() {
                    // A cursor-position sequence starts a new row.
                    if character == 'H' && !row.is_empty() {
                        out.push(std::mem::take(&mut row));
                    }
                    escape.clear();
                }
                continue;
            }
            match character {
                '\u{1b}' => escape.push(character),
                '\r' => {}
                '\n' => out.push(std::mem::take(&mut row)),
                other => row.push(other),
            }
        }
        if !row.is_empty() {
            out.push(row);
        }
        out.truncate(rows);
        out
    }

    fn inserted(text: &str) -> Action {
        Action::Insert(text.as_bytes().to_vec())
    }

    #[test]
    fn the_layout_is_the_query_then_the_matches_then_the_count() {
        let mut picker = picker(&["alpha", "beta"]);
        let frame = picker.frame(20, 6, false);
        assert!(frame.starts_with(HOME));
        let rows = rows_of(&frame, 6);
        assert_eq!(rows[0], "history> ");
        assert_eq!(rows[1], "> alpha");
        assert_eq!(rows[2], "  beta");
        assert_eq!(rows[3], "  2/2");
        assert_eq!(rows.len(), 4, "a short list paints only its own rows");
    }

    #[test]
    fn the_cursor_lands_after_the_query() {
        let mut picker = picker(&["alpha"]);
        picker.act(inserted("al"));
        let frame = picker.frame(40, 10, false);
        let text = String::from_utf8_lossy(&frame);
        assert!(
            text.ends_with("\x1b[1;12H"),
            "the 9-cell prompt plus a 2-cell query puts the cursor in column 12: {text:?}"
        );
    }

    #[test]
    fn matched_characters_are_bold_in_colour_and_absent_without() {
        let mut picker = picker(&["git checkout"]);
        picker.act(inserted("gco"));
        let colour = picker.frame(30, 6, true);
        let text = String::from_utf8_lossy(&colour);
        assert!(text.contains("\x1b[7m"), "the selected row is reversed");
        assert!(text.contains("\x1b[1m"), "matched characters are bold");
        let plain = String::from_utf8_lossy(&picker.frame(30, 6, false)).into_owned();
        for sgr in ["\x1b[1m", "\x1b[7m", "\x1b[2m", "\x1b[22m"] {
            assert!(!plain.contains(sgr), "no SGR without colour: {plain:?}");
        }
    }

    #[test]
    fn a_long_selection_scrolls_the_window_one_row_at_a_time() {
        let lines: Vec<String> = (0..10).map(|index| format!("line {index}")).collect();
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let mut picker = picker(&lines);
        // Five rows: the query, three candidates and the count.
        let rows = rows_of(&picker.frame(20, 5, false), 5);
        assert_eq!(rows[0], "history> ");
        assert_eq!(rows[1], "> line 0");
        assert_eq!(rows[2], "  line 1");
        assert_eq!(rows[3], "  line 2");
        assert_eq!(rows[4], "  10/10");

        for _ in 0..3 {
            picker.act(Action::Down);
        }
        let rows = rows_of(&picker.frame(20, 5, false), 5);
        assert_eq!(rows[1], "  line 1", "the window scrolled by one row");
        assert_eq!(rows[2], "  line 2");
        assert_eq!(rows[3], "> line 3");
    }

    #[test]
    fn a_row_is_never_wider_than_the_terminal() {
        let long = "x".repeat(100);
        let mut picker = picker(&[long.as_str()]);
        for columns in [1u16, 2, 3, 10, 80] {
            let frame = picker.frame(columns, 6, false);
            for row in rows_of(&frame, 6) {
                assert!(
                    width_of(&row) <= usize::from(columns),
                    "{row:?} is wider than {columns}"
                );
            }
        }
    }

    #[test]
    fn a_one_row_terminal_paints_only_the_query() {
        let mut picker = picker(&["alpha"]);
        let rows = rows_of(&picker.frame(20, 1, false), 1);
        assert_eq!(rows, vec!["history> ".to_string()]);
    }

    #[test]
    fn a_query_longer_than_the_terminal_keeps_its_tail() {
        let mut picker = picker(&["alpha"]);
        picker.act(inserted("0123456789abcdef"));
        // Twelve columns and a nine-cell prompt leave three cells of query,
        // and they are the *last* three: the cursor sits at the end.
        let rows = rows_of(&picker.frame(12, 6, false), 6);
        assert_eq!(rows[0], "history> def", "the tail of the query is kept");
    }

    #[test]
    fn a_paging_action_uses_the_height_that_was_painted() {
        let lines: Vec<String> = (0..20).map(|index| format!("line {index}")).collect();
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let mut picker = picker(&lines);
        // Five rows: query, three candidates, count.
        picker.frame(20, 5, false);
        picker.act(Action::PageDown);
        assert_eq!(picker.selection(), 3);
        picker.act(Action::PageUp);
        assert_eq!(picker.selection(), 0);
    }

    #[test]
    fn the_count_says_when_nothing_matches() {
        let mut picker = picker(&["alpha"]);
        picker.act(inserted("zzz"));
        let rows = rows_of(&picker.frame(20, 6, false), 6);
        assert_eq!(rows[0], "history> zzz");
        assert_eq!(rows[1], "  no match");
        assert_eq!(rows.len(), 2);
    }
}
