//! The line buffer, its cursor, and the kill ring.
//!
//! The buffer is bytes ([`Vec<u8>`]) and the cursor is a byte offset that stays
//! on a character boundary for valid UTF-8. Movement and deletion step over
//! whole scalars so that editing a UTF-8 line cannot produce mojibake; an
//! invalid byte is its own one-byte character.
//!
//! A logical command can span several physical lines (PS2 continuation). The
//! already-submitted lines are kept in the buffer — the parser needs them — but
//! [`Line::start`] marks where the editable continuation begins; the renderer
//! only paints from there, because the earlier lines are scrollback.

use baash_complete::Menu;

/// One direction of the kill ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KillDirection {
    Backward,
    Forward,
}

/// The last piece of text killed, so `Ctrl-Y` can put it back.
#[derive(Clone, Debug, Default)]
struct KillRing {
    text: Vec<u8>,
    direction: Option<KillDirection>,
}

impl KillRing {
    /// Record a kill, merging with the previous one in the same direction.
    ///
    /// Readline accumulates consecutive kills so that `Ctrl-W Ctrl-W Ctrl-Y`
    /// returns both words; a kill in the other direction starts a new entry.
    fn kill(&mut self, text: Vec<u8>, direction: KillDirection) {
        if self.direction == Some(direction) {
            match direction {
                KillDirection::Backward => {
                    let mut merged = text;
                    merged.extend_from_slice(&self.text);
                    self.text = merged;
                }
                KillDirection::Forward => self.text.extend_from_slice(&text),
            }
        } else {
            self.text = text;
        }
        self.direction = Some(direction);
    }

    fn reset(&mut self) {
        self.direction = None;
    }
}

/// The editable line.
#[derive(Debug, Default)]
pub struct Line {
    buffer: Vec<u8>,
    cursor: usize,
    start: usize,
    kill: KillRing,
}

impl Line {
    /// An empty line.
    pub fn new() -> Self {
        Self::default()
    }

    /// Clear everything, including the continuation boundary.
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.start = 0;
        self.kill.reset();
    }

    /// The whole buffer, including any continuation lines.
    pub fn buffer(&self) -> &[u8] {
        &self.buffer
    }

    /// The part the editor currently paints and edits.
    pub fn editable(&self) -> &[u8] {
        &self.buffer[self.start..]
    }

    /// The byte offset the editable region starts at.
    pub fn start(&self) -> usize {
        self.start
    }

    /// Whether the line already has submitted continuation lines.
    pub fn in_continuation(&self) -> bool {
        self.start > 0
    }

    /// The cursor as a byte offset into [`Line::buffer`].
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The cursor as a byte offset into [`Line::editable`].
    pub fn editable_cursor(&self) -> usize {
        self.cursor - self.start
    }

    /// Move the cursor, snapping to a character boundary inside the region.
    pub fn set_cursor(&mut self, at: usize) {
        self.cursor = at.clamp(self.start, self.buffer.len());
        while self.cursor > self.start && !is_boundary(&self.buffer, self.cursor) {
            self.cursor -= 1;
        }
    }

    /// Replace the whole line with `text` and put the cursor at its end.
    pub fn set_text(&mut self, text: &[u8]) {
        self.buffer.clear();
        self.buffer.extend_from_slice(text);
        self.start = 0;
        self.cursor = self.buffer.len();
    }

    /// Take the buffer, leaving an empty line behind.
    pub fn take(&mut self) -> Vec<u8> {
        let text = std::mem::take(&mut self.buffer);
        self.cursor = 0;
        self.start = 0;
        text
    }

    /// Undo any `Ctrl-Y` bookkeeping after a key that is not a kill.
    pub fn kill_reset(&mut self) {
        self.kill.reset();
    }

    /// Insert `bytes` at the cursor.
    pub fn insert(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.buffer
            .splice(self.cursor..self.cursor, bytes.iter().copied());
        self.cursor += bytes.len();
    }

    /// Insert one character at the cursor.
    pub fn insert_char(&mut self, character: char) {
        let mut buffer = [0u8; 4];
        self.insert(character.encode_utf8(&mut buffer).as_bytes());
    }

    /// Append a newline and move the cursor past it. Used by continuation.
    pub fn push_newline(&mut self) {
        self.buffer.push(b'\n');
        self.cursor = self.buffer.len();
    }

    /// Mark the current end as submitted scrollback; edit after it only.
    pub fn commit_prefix(&mut self) {
        self.start = self.buffer.len();
        self.cursor = self.start;
    }

    /// Delete the character before the cursor.
    pub fn backspace(&mut self) -> bool {
        if self.cursor <= self.start {
            return false;
        }
        let from = previous_boundary(&self.buffer, self.cursor, self.start);
        self.buffer.drain(from..self.cursor);
        self.cursor = from;
        true
    }

    /// Delete the character under the cursor.
    pub fn delete(&mut self) -> bool {
        if self.cursor >= self.buffer.len() {
            return false;
        }
        let to = next_boundary(&self.buffer, self.cursor, self.buffer.len());
        self.buffer.drain(self.cursor..to);
        true
    }

    /// Move one character left.
    pub fn move_left(&mut self) -> bool {
        let to = previous_boundary(&self.buffer, self.cursor, self.start);
        let moved = to != self.cursor;
        self.cursor = to;
        moved
    }

    /// Move one character right.
    pub fn move_right(&mut self) -> bool {
        let to = next_boundary(&self.buffer, self.cursor, self.buffer.len());
        let moved = to != self.cursor;
        self.cursor = to;
        moved
    }

    /// Move to the start of the editable region.
    pub fn move_home(&mut self) -> bool {
        let moved = self.cursor != self.start;
        self.cursor = self.start;
        moved
    }

    /// Move to the end of the buffer.
    pub fn move_end(&mut self) -> bool {
        let moved = self.cursor != self.buffer.len();
        self.cursor = self.buffer.len();
        moved
    }

    /// Move to the start of the previous word (Alt-B).
    pub fn move_word_left(&mut self) -> bool {
        let mut at = self.cursor;
        while at > self.start && !is_word(self.buffer[at - 1]) {
            at -= 1;
        }
        while at > self.start && is_word(self.buffer[at - 1]) {
            at -= 1;
        }
        let moved = at != self.cursor;
        self.cursor = at;
        moved
    }

    /// Move past the end of the next word (Alt-F).
    pub fn move_word_right(&mut self) -> bool {
        let length = self.buffer.len();
        let mut at = self.cursor;
        while at < length && !is_word(self.buffer[at]) {
            at += 1;
        }
        while at < length && is_word(self.buffer[at]) {
            at += 1;
        }
        let moved = at != self.cursor;
        self.cursor = at;
        moved
    }

    /// Kill from the start of the editable region to the cursor (Ctrl-U).
    pub fn kill_to_start(&mut self) -> bool {
        if self.cursor <= self.start {
            return false;
        }
        let killed: Vec<u8> = self.buffer.drain(self.start..self.cursor).collect();
        self.cursor = self.start;
        self.kill.kill(killed, KillDirection::Backward);
        true
    }

    /// Kill from the cursor to the end of the buffer (Ctrl-K).
    pub fn kill_to_end(&mut self) -> bool {
        if self.cursor >= self.buffer.len() {
            return false;
        }
        let killed: Vec<u8> = self.buffer.drain(self.cursor..).collect();
        self.kill.kill(killed, KillDirection::Forward);
        true
    }

    /// Kill the whitespace-delimited word before the cursor (Ctrl-W).
    pub fn kill_word(&mut self) -> bool {
        if self.cursor <= self.start {
            return false;
        }
        let mut at = self.cursor;
        while at > self.start && self.buffer[at - 1].is_ascii_whitespace() {
            at -= 1;
        }
        while at > self.start && !self.buffer[at - 1].is_ascii_whitespace() {
            at -= 1;
        }
        let killed: Vec<u8> = self.buffer.drain(at..self.cursor).collect();
        self.cursor = at;
        self.kill.kill(killed, KillDirection::Backward);
        true
    }

    /// Insert the most recently killed text (Ctrl-Y).
    pub fn yank(&mut self) -> bool {
        if self.kill.text.is_empty() {
            return false;
        }
        let text = self.kill.text.clone();
        self.insert(&text);
        true
    }

    /// Swap the characters around the cursor (Ctrl-T).
    pub fn transpose(&mut self) -> bool {
        let length = self.buffer.len();
        if length - self.start < 2 {
            return false;
        }
        let mut cursor = self.cursor;
        if cursor >= length {
            cursor = previous_boundary(&self.buffer, length, self.start);
        } else if cursor == self.start {
            // At the start, readline transposes the first two characters; that
            // is more useful than doing nothing.
            cursor = next_boundary(&self.buffer, self.start, length);
        }
        let left_start = previous_boundary(&self.buffer, cursor, self.start);
        let right_end = next_boundary(&self.buffer, cursor, length);
        if left_start == cursor || cursor == right_end {
            return false;
        }
        let left = self.buffer[left_start..cursor].to_vec();
        let right = self.buffer[cursor..right_end].to_vec();
        self.buffer
            .splice(left_start..right_end, right.into_iter().chain(left));
        self.cursor = right_end;
        true
    }

    /// Apply completion candidate `index` through [`Menu::apply`].
    ///
    /// Insertion quoting belongs to `baash-complete`; the editor must never
    /// splice a candidate itself (`docs/contracts.md`, `baash-complete`).
    ///
    /// A menu whose replacement would reach into submitted scrollback is
    /// refused: those bytes are no longer on screen and cannot be repainted.
    pub fn apply_menu(&mut self, menu: &Menu, index: usize) -> bool {
        if menu.replace.start < self.start || menu.replace.end > self.buffer.len() {
            return false;
        }
        let Some(span) = menu.apply(&mut self.buffer, index) else {
            return false;
        };
        self.cursor = span.end.min(self.buffer.len());
        true
    }

    /// Insert the candidates' unambiguous common prefix, replacing the word.
    ///
    /// This is fish's first `Tab`: nothing is selected, so the bytes spliced in
    /// are exactly the part every candidate agrees on. The quoting is
    /// `baash-complete`'s (`Menu::common_prefix_insertion`), for the same reason
    /// [`Line::apply_menu`] uses `Menu::apply` rather than escaping here.
    ///
    /// Returns `false` when there is no prefix to add or the word starts in
    /// submitted scrollback; the caller then opens the menu instead.
    pub fn apply_menu_prefix(&mut self, menu: &Menu) -> bool {
        if menu.replace.start < self.start || menu.replace.end > self.buffer.len() {
            return false;
        }
        let Some(text) = menu.common_prefix_insertion() else {
            return false;
        };
        let start = menu.replace.start;
        let end = menu.replace.end.min(self.buffer.len()).max(start);
        self.buffer.splice(start..end, text.iter().copied());
        self.cursor = (start + text.len()).min(self.buffer.len());
        true
    }
}

/// Whether byte offset `at` starts a character.
fn is_boundary(bytes: &[u8], at: usize) -> bool {
    at == 0 || at >= bytes.len() || (bytes[at] & 0xc0) != 0x80
}

/// The previous character boundary, never below `floor`.
fn previous_boundary(bytes: &[u8], at: usize, floor: usize) -> usize {
    if at <= floor {
        return floor;
    }
    let mut at = at - 1;
    while at > floor && (bytes[at] & 0xc0) == 0x80 {
        at -= 1;
    }
    at
}

/// The next character boundary, never above `ceil`.
fn next_boundary(bytes: &[u8], at: usize, ceil: usize) -> usize {
    if at >= ceil {
        return ceil;
    }
    let mut at = at + 1;
    while at < ceil && (bytes[at] & 0xc0) == 0x80 {
        at += 1;
    }
    at
}

/// Readline's default word character set: alphanumerics and underscore.
///
/// Bytes above ASCII are word characters so that a UTF-8 word moves as a unit.
fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insertion_moves_the_cursor() {
        let mut line = Line::new();
        line.insert(b"ls -l");
        assert_eq!(line.buffer(), b"ls -l");
        assert_eq!(line.cursor(), 5);
        line.move_left();
        line.insert(b"a");
        assert_eq!(line.buffer(), b"ls -al");
        assert_eq!(line.cursor(), 5);
    }

    #[test]
    fn utf8_moves_and_deletes_by_character() {
        let mut line = Line::new();
        line.insert("é€".as_bytes());
        assert_eq!(line.cursor(), 5);
        assert!(line.backspace());
        assert_eq!(line.buffer(), "é".as_bytes());
        assert!(line.backspace());
        assert_eq!(line.buffer(), b"");
        assert!(!line.backspace());
    }

    #[test]
    fn delete_is_forward() {
        let mut line = Line::new();
        line.insert(b"abc");
        line.move_home();
        assert!(line.delete());
        assert_eq!(line.buffer(), b"bc");
        assert_eq!(line.cursor(), 0);
    }

    #[test]
    fn word_movement() {
        let mut line = Line::new();
        line.insert(b"git commit --amend");
        line.move_home();
        assert!(line.move_word_right());
        assert_eq!(line.cursor(), 3);
        assert!(line.move_word_right());
        assert_eq!(line.cursor(), 10);
        assert!(line.move_word_left());
        assert_eq!(line.cursor(), 4);
    }

    #[test]
    fn kill_and_yank_round_trip() {
        let mut line = Line::new();
        line.insert(b"one two three");
        line.kill_word();
        assert_eq!(line.buffer(), b"one two ");
        line.kill_word();
        assert_eq!(line.buffer(), b"one ");
        line.yank();
        assert_eq!(line.buffer(), b"one two three");
    }

    #[test]
    fn consecutive_kills_accumulate() {
        let mut line = Line::new();
        line.insert(b"a b c");
        line.kill_word();
        line.kill_word();
        line.yank();
        assert_eq!(line.buffer(), b"a b c");
    }

    #[test]
    fn kill_to_start_and_end() {
        let mut line = Line::new();
        line.insert(b"abcdef");
        line.set_cursor(3);
        assert!(line.kill_to_end());
        assert_eq!(line.buffer(), b"abc");
        line.yank();
        assert_eq!(line.buffer(), b"abcdef");
        line.set_cursor(3);
        assert!(line.kill_to_start());
        assert_eq!(line.buffer(), b"def");
        line.yank();
        assert_eq!(line.buffer(), b"abcdef");
    }

    #[test]
    fn transpose_swaps_around_the_cursor() {
        let mut line = Line::new();
        line.insert(b"ab");
        assert!(line.transpose());
        assert_eq!(line.buffer(), b"ba");
        assert_eq!(line.cursor(), 2);

        let mut line = Line::new();
        line.insert(b"abc");
        line.set_cursor(2);
        assert!(line.transpose());
        assert_eq!(line.buffer(), b"acb");
        assert_eq!(line.cursor(), 3);
    }

    #[test]
    fn home_and_end_stop_at_the_continuation_boundary() {
        let mut line = Line::new();
        line.insert(b"if true");
        line.push_newline();
        line.commit_prefix();
        line.insert(b"then echo hi");
        assert_eq!(line.start(), 8);
        assert!(line.move_home());
        assert_eq!(line.cursor(), 8);
        assert!(!line.backspace(), "cannot cross into scrollback");
        line.move_end();
        assert_eq!(line.cursor(), 20);
    }

    #[test]
    fn set_text_resets_the_continuation_boundary() {
        let mut line = Line::new();
        line.insert(b"x");
        line.push_newline();
        line.commit_prefix();
        line.set_text(b"echo hi");
        assert_eq!(line.start(), 0);
        assert_eq!(line.cursor(), 7);
    }
}
