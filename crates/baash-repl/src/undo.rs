//! The line editor's undo history.
//!
//! `Ctrl-Z` and `Ctrl-_` walk back through the edits of the line being typed;
//! `Ctrl-Shift-Z` walks forward again. An edit is not a keystroke: a run of
//! typed characters is one step, and **one paste is one step** — a paste that
//! was auto-quoted (`paste.rs`) can be taken out again with a single keystroke,
//! which is what makes guessing worth doing. Everything else is one step per
//! edit, so a kill, a completion insertion or a menu choice is its own.
//!
//! The history is snapshots, not an edit log: the line is small, this cannot
//! desynchronise, and it holds for any future edit path without touching it.
//! Two bounds keep a pathological line from eating memory: at most
//! [`MAX_ENTRIES`] steps, and at most [`MAX_BYTES`] of snapshot text in total,
//! dropping the oldest step first.
//!
//! The history belongs to one line: [`Undo::reset`] is called at every prompt,
//! and a `Ctrl-C` or a continuation line never lets an undo reach back into the
//! previous command or into the scrollback.

use crate::line::{Line, Snapshot};

/// The most steps one line keeps.
const MAX_ENTRIES: usize = 64;

/// The most snapshot bytes one line keeps.
///
/// 64 steps of a pasted megabyte would be 64 MB for one prompt; this is a
/// bound a shell can afford, and the oldest step is dropped first.
const MAX_BYTES: usize = 8 * 1024 * 1024;

/// What made an edit, which decides whether it joins the previous step.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum EditKind {
    /// A character or a byte typed at the keyboard: a run of these is one step.
    Typing,
    /// A bracketed paste: always its own step, whatever came before it.
    Paste,
    /// An edit no key typed: a kill, a completion insertion, a menu choice.
    #[default]
    Other,
}

/// Undo and redo state for the line being edited.
#[derive(Debug, Default)]
pub(crate) struct Undo {
    /// States to go back to, oldest first. The last one is the state one
    /// `Ctrl-Z` produces.
    past: Vec<Snapshot>,
    /// States to go forward to after an undo, most recent first.
    future: Vec<Snapshot>,
    /// The line as it stands after the last recorded edit.
    last: Snapshot,
    /// What made that edit, for coalescing.
    last_kind: EditKind,
    /// The total text held in `past`, against [`MAX_BYTES`]. The redo side is
    /// not counted: it is bounded by the same steps and is transient.
    bytes: usize,
}

impl Undo {
    /// Start a fresh history for `line`, forgetting the previous command's.
    pub(crate) fn reset(&mut self, line: &Line) {
        self.past.clear();
        self.future.clear();
        self.last = line.snapshot();
        self.last_kind = EditKind::Other;
        self.bytes = 0;
    }

    /// Note the line's state after an edit of `kind`.
    ///
    /// A key that did not change the text (a cursor movement, an opened menu)
    /// records nothing, so undo walks edits rather than keystrokes.
    pub(crate) fn record(&mut self, line: &Line, kind: EditKind) {
        let current = line.snapshot();
        if current.text() == self.last.text() {
            // A cursor movement is not an edit. The remembered state still
            // follows it, so that an undo puts the cursor back where the edit
            // it takes back began rather than where the user wandered.
            self.last = current;
            return;
        }
        // A run of typed characters is one step: the second character of a run
        // must not push the state the first one already pushed.
        let joins = kind == EditKind::Typing
            && self.last_kind == EditKind::Typing
            && appends(&self.last, &current);
        if !joins {
            self.past.push(self.last.clone());
            self.bytes += self.last.text().len();
        }
        self.future.clear();
        self.last = current;
        self.last_kind = kind;
        self.trim();
    }

    /// Go back one step. Returns whether there was one.
    pub(crate) fn undo(&mut self, line: &mut Line) -> bool {
        let Some(previous) = self.past.pop() else {
            return false;
        };
        self.bytes -= previous.text().len();
        let current = line.snapshot();
        line.restore(&previous);
        self.future.push(current);
        self.last = previous;
        // The next edit starts a step of its own: undoing twice must walk two
        // steps, and typing after an undo must not be folded into the step the
        // undo went back to.
        self.last_kind = EditKind::Other;
        true
    }

    /// Go forward one step. Returns whether there was one.
    pub(crate) fn redo(&mut self, line: &mut Line) -> bool {
        let Some(next) = self.future.pop() else {
            return false;
        };
        let current = line.snapshot();
        line.restore(&next);
        self.bytes += current.text().len();
        self.past.push(current);
        self.last = next;
        self.last_kind = EditKind::Other;
        self.trim();
        true
    }

    /// Drop the oldest steps until the bounds hold again.
    fn trim(&mut self) {
        while self.past.len() > MAX_ENTRIES || (self.bytes > MAX_BYTES && self.past.len() > 1) {
            let dropped = self.past.remove(0);
            self.bytes -= dropped.text().len();
        }
    }
}

/// Whether `after` is `before` with text inserted at the cursor.
///
/// This is what a typed character does, and it is how consecutive characters
/// are recognised as one step. Anything else — a deletion, an insertion
/// somewhere behind the cursor, a menu replacing a span — is a new step.
fn appends(before: &Snapshot, after: &Snapshot) -> bool {
    if after.cursor() <= before.cursor() || before.start() != after.start() {
        return false;
    }
    let head = before.cursor();
    let inserted = after.cursor() - before.cursor();
    let before_text = before.text();
    let after_text = after.text();
    after_text.len() == before_text.len() + inserted
        && after_text[..head] == before_text[..head]
        && after_text[head + inserted..] == before_text[head..]
}

#[cfg(test)]
mod tests {
    use super::{EditKind, Undo};
    use crate::line::Line;

    fn typed(undo: &mut Undo, line: &mut Line, text: &str) {
        for character in text.chars() {
            line.insert_char(character);
            undo.record(line, EditKind::Typing);
        }
    }

    #[test]
    fn a_run_of_typed_characters_is_one_step() {
        let mut line = Line::new();
        let mut undo = Undo::default();
        undo.reset(&line);
        typed(&mut undo, &mut line, "git status");
        assert_eq!(line.buffer(), b"git status");

        assert!(undo.undo(&mut line));
        assert_eq!(line.buffer(), b"", "the whole run goes in one step");
        assert!(undo.redo(&mut line));
        assert_eq!(line.buffer(), b"git status");
    }

    #[test]
    fn a_paste_is_one_step_even_after_typing() {
        let mut line = Line::new();
        let mut undo = Undo::default();
        undo.reset(&line);
        typed(&mut undo, &mut line, "echo ");
        line.insert(b"'a&b'");
        undo.record(&line, EditKind::Paste);

        // The typing and the paste are two steps, not one: the paste must come
        // out first.
        assert!(undo.undo(&mut line));
        assert_eq!(line.buffer(), b"echo ");
        assert!(undo.undo(&mut line));
        assert_eq!(line.buffer(), b"");
        assert!(!undo.undo(&mut line), "nothing before the line started");
    }

    #[test]
    fn a_deletion_is_its_own_step() {
        let mut line = Line::new();
        let mut undo = Undo::default();
        undo.reset(&line);
        typed(&mut undo, &mut line, "abc");
        line.backspace();
        undo.record(&line, EditKind::Other);
        assert_eq!(line.buffer(), b"ab");
        assert!(undo.undo(&mut line));
        assert_eq!(line.buffer(), b"abc");
    }

    #[test]
    fn typing_after_an_undo_starts_a_new_step() {
        let mut line = Line::new();
        let mut undo = Undo::default();
        undo.reset(&line);
        typed(&mut undo, &mut line, "ab");
        assert!(undo.undo(&mut line));
        typed(&mut undo, &mut line, "xy");
        // Undo takes back the typing after the undo, not the whole line.
        assert!(undo.undo(&mut line));
        assert_eq!(line.buffer(), b"");
        assert!(undo.redo(&mut line));
        assert_eq!(line.buffer(), b"xy");
    }

    #[test]
    fn an_edit_that_changes_nothing_records_nothing() {
        let mut line = Line::new();
        let mut undo = Undo::default();
        undo.reset(&line);
        typed(&mut undo, &mut line, "ab");
        line.move_home();
        undo.record(&line, EditKind::Other);
        assert!(undo.undo(&mut line));
        assert_eq!(line.buffer(), b"", "the movement was not a step");
    }

    #[test]
    fn the_history_is_bounded() {
        let mut line = Line::new();
        let mut undo = Undo::default();
        undo.reset(&line);
        for index in 0..(super::MAX_ENTRIES + 10) {
            // Each iteration is its own step: type a character, then a kill.
            line.insert_char(char::from(b'a' + (index % 26) as u8));
            undo.record(&line, EditKind::Other);
        }
        let mut steps = 0;
        while undo.undo(&mut line) {
            steps += 1;
        }
        assert_eq!(steps, super::MAX_ENTRIES, "the oldest steps are dropped");
    }
}
