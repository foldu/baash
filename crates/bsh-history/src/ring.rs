//! The session ring: numbered entries the editor walks, mirrored into the
//! database.
//!
//! There is one ring per shell. It lives in the shell state so the `history`
//! built-in and the editor cannot drift apart, and the editor holds it while a
//! line is being read (the REPL swaps it across at exactly those two points).
//!
//! # Numbers
//!
//! A number is the command's database id. `history` prints it, `!12` resolves
//! it, and it means the same command in every shell: ids are global, assigned
//! by the database, and never reused while the row exists. Two shells therefore
//! see the same number for the same command, and `history -d 12` removes that
//! row wherever it came from.
//!
//! Without a database (a test, a script, a piped `-i`) there is nothing to
//! allocate ids, so the ring counts from 1 itself. That is the only case where
//! a number is merely local.
//!
//! # What is recorded
//!
//! Every non-blank submit is recorded, including a line that is later replaced
//! by history expansion (`remove_current` deletes that row again). Nothing is
//! filtered by pattern or by leading space: the file-era policies are gone.

use std::path::Path;
use std::sync::Arc;

use crate::store::Store;

/// How many earlier commands a new session starts with.
pub const RECENT_DEPTH: usize = 1000;

/// How many entries the in-memory ring keeps.
///
/// The database keeps everything; this only bounds what Up/Down and `history`
/// can see in one very long-lived session.
pub const RETENTION: usize = 5000;

/// One command line in the ring.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The database row id, which is also the number `history` prints.
    id: i64,
    text: Vec<u8>,
}

impl Entry {
    /// The number `history` prints and `!` accepts.
    pub fn number(&self) -> u32 {
        u32::try_from(self.id).unwrap_or(u32::MAX)
    }

    /// The command text, exactly as it was submitted (no trailing newline).
    pub fn text(&self) -> &[u8] {
        &self.text
    }
}

/// Shell history: the ring, its navigation cursor and the database behind it.
#[derive(Clone, Debug)]
pub struct History {
    entries: Vec<Entry>,
    /// The best guess at the next id; `\!` and `${x@P}` report it.
    ///
    /// With a database the real id comes back from the insert, so this is only
    /// a prediction between the prompt and the next command. Without one it is
    /// the counter that assigns numbers.
    next: i64,
    /// Where Up/Down is in `entries`.
    index: Option<usize>,
    /// What the user was typing before the first Up press.
    draft: Vec<u8>,
    /// Whether the last entry is the line the shell is executing right now.
    current: bool,
    store: Option<Arc<Store>>,
    /// The next `seq` this session will use.
    next_seq: i64,
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

impl History {
    /// An empty ring that remembers nothing. This is what a test, a script and
    /// `bsh -c` get.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            next: 1,
            index: None,
            draft: Vec::new(),
            current: false,
            store: None,
            next_seq: 1,
        }
    }

    /// Open the database at `path` and seed the ring with its newest rows.
    pub fn open(path: &Path, depth: usize) -> std::io::Result<Self> {
        let store = Store::open(path)?;
        Ok(Self::with_store(store, depth))
    }

    /// Wrap an already-open store, seeding the ring from it.
    ///
    /// The seed is the newest `depth` rows of *every* session: the database is
    /// one history, and a second shell's commands are as much part of it as
    /// this one's. Later sessions append at the end, which a running ring only
    /// picks up on the next start.
    pub fn with_store(store: Arc<Store>, depth: usize) -> Self {
        let entries: Vec<Entry> = store
            .recent(depth)
            .into_iter()
            .map(|row| Entry {
                id: row.id,
                text: row.text,
            })
            .collect();
        let next = entries.last().map_or(1, |entry| entry.id.saturating_add(1));
        Self {
            entries,
            next,
            index: None,
            draft: Vec::new(),
            current: false,
            store: Some(store),
            next_seq: 1,
        }
    }

    /// The database, when this shell has one.
    pub fn store(&self) -> Option<&Arc<Store>> {
        self.store.as_ref()
    }

    /// Wait until every queued write has been applied.
    pub fn flush(&self) {
        if let Some(store) = &self.store {
            store.flush();
        }
    }

    // -- recording ---------------------------------------------------------

    /// Record the command line that is about to run.
    ///
    /// Trailing newlines are stripped and a blank (or whitespace-only) line is
    /// not recorded. The row
    /// is written immediately, without its outcome; [`History::finish`] fills
    /// that in, so a shell that dies mid-command still leaves the line behind.
    pub fn push(&mut self, line: &[u8]) {
        let line = trim(line);
        if line.iter().all(u8::is_ascii_whitespace) {
            self.current = false;
            return;
        }
        let id = match &self.store {
            Some(store) => {
                let seq = self.next_seq;
                self.next_seq = self.next_seq.saturating_add(1);
                store
                    .begin(seq, line, current_directory())
                    .unwrap_or(self.next)
            }
            None => self.next,
        };
        self.entries.push(Entry {
            id,
            text: line.to_vec(),
        });
        self.current = true;
        self.trim();
        self.update_hint();
        self.reset_navigation();
    }

    /// Point `next` at the id after the newest entry.
    ///
    /// With a database this is a prediction, not an allocation: the insert
    /// returns the real id. It is what `\!` and `${x@P}` report before the
    /// line has run, and it is the counter when there is no database.
    fn update_hint(&mut self) {
        match self.entries.last() {
            Some(last) => self.next = last.id.saturating_add(1),
            // An empty ring with a database continues where the table does;
            // only a ring with no database starts over at 1.
            None => {
                if self.store.is_none() {
                    self.next = 1;
                }
            }
        }
    }

    /// Whether the last entry is the command line currently executing.
    pub fn is_current(&self) -> bool {
        self.current
    }

    /// Retire the current-line marker without recording an outcome.
    pub fn clear_current(&mut self) {
        self.current = false;
    }

    /// Record the outcome of the current line and retire the marker.
    pub fn finish(&mut self, status: Option<i32>, duration_ms: Option<i64>) {
        if !self.current {
            return;
        }
        if let (Some(store), Some(entry)) = (&self.store, self.entries.last()) {
            store.finish(entry.id, status, duration_ms);
        }
        self.current = false;
    }

    /// Remove the command line the editor just recorded.
    ///
    /// History expansion uses this to take back the typed line before
    /// recording the expanded one. Only does something when the last entry
    /// really is the current line.
    pub fn remove_current(&mut self) -> bool {
        if !self.current {
            return false;
        }
        self.pop_last();
        true
    }

    fn pop_last(&mut self) {
        let Some(entry) = self.entries.pop() else {
            self.current = false;
            return;
        };
        if let Some(store) = &self.store {
            store.delete(entry.id);
        }
        self.current = false;
        self.update_hint();
        self.reset_navigation();
    }

    /// Drop the oldest entries beyond [`RETENTION`], keeping their numbers.
    fn trim(&mut self) {
        if self.entries.len() > RETENTION {
            let excess = self.entries.len() - RETENTION;
            self.entries.drain(..excess);
        }
    }

    // -- queries -----------------------------------------------------------

    /// Number of entries held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the ring is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every entry, oldest first.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The command texts, oldest first.
    pub fn commands(&self) -> impl DoubleEndedIterator<Item = &[u8]> {
        self.entries.iter().map(|entry| entry.text.as_slice())
    }

    /// The number the next added entry will probably receive.
    ///
    /// A prediction between the prompt and the insert (`\!`, `${x@P}`), and
    /// the exact answer when there is no database.
    pub fn next_number(&self) -> u32 {
        u32::try_from(self.next).unwrap_or(u32::MAX)
    }

    /// The last `n` entries (`history n`); `None` means all of them.
    pub fn list(&self, n: Option<usize>) -> &[Entry] {
        match n {
            Some(n) if n < self.entries.len() => &self.entries[self.entries.len() - n..],
            _ => &self.entries,
        }
    }

    /// Resolve a history reference: a positive number, or a negative offset
    /// from the end (`-1` is the newest entry). `0` is always invalid.
    pub fn get(&self, reference: i64) -> Option<&Entry> {
        self.index_of(reference).map(|index| &self.entries[index])
    }

    /// Index of a history reference, using the same rules as [`History::get`].
    pub fn index_of(&self, reference: i64) -> Option<usize> {
        if self.entries.is_empty() || reference == 0 {
            return None;
        }
        if reference > 0 {
            self.entries.iter().position(|entry| entry.id == reference)
        } else {
            let offset = usize::try_from(reference.unsigned_abs()).ok()?;
            if offset > self.entries.len() {
                None
            } else {
                Some(self.entries.len() - offset)
            }
        }
    }

    /// Delete the entry a reference names. Returns whether one was removed.
    ///
    /// Numbers are not reassigned: `history` shows a gap, and an entry keeps
    /// the number it was printed with.
    pub fn delete(&mut self, reference: i64) -> bool {
        let Some(index) = self.index_of(reference) else {
            return false;
        };
        let entry = self.entries.remove(index);
        if let Some(store) = &self.store {
            store.delete(entry.id);
        }
        if index == self.entries.len() {
            self.current = false;
        }
        self.update_hint();
        self.reset_navigation();
        true
    }

    /// Empty the ring and forget this session's rows (`history -c`).
    ///
    /// The table keeps every other session's commands, and they are back in
    /// the seed the next time a shell starts: clearing is "not what I typed
    /// here", not "erase the history".
    pub fn clear(&mut self) {
        self.entries.clear();
        if let Some(store) = &self.store {
            // Rows go; `seq` does not restart, so a queued insert can never
            // collide with the unique `(session, seq)` index if a clear lands
            // out of order.
            store.clear_session(store.session());
        }
        self.current = false;
        self.update_hint();
        self.reset_navigation();
    }

    // -- navigation --------------------------------------------------------

    /// The previous (older) entry, starting from the draft `current`.
    ///
    /// `None` when there is nothing to recall. The draft is saved on the first
    /// Up press so Down can restore exactly what was being typed.
    pub fn older(&mut self, current: &[u8]) -> Option<Vec<u8>> {
        if self.entries.is_empty() {
            return None;
        }
        match self.index {
            None => {
                self.draft = current.to_vec();
                self.index = Some(self.entries.len() - 1);
            }
            Some(0) => {}
            Some(index) => self.index = Some(index - 1),
        }
        self.index.map(|index| self.entries[index].text.clone())
    }

    /// The next (newer) entry, or the saved draft after the newest one.
    ///
    /// `None` means there is no navigation in progress.
    pub fn newer(&mut self) -> Option<Vec<u8>> {
        let index = self.index?;
        if index + 1 < self.entries.len() {
            self.index = Some(index + 1);
            Some(self.entries[index + 1].text.clone())
        } else {
            self.index = None;
            Some(std::mem::take(&mut self.draft))
        }
    }

    /// Forget the navigation cursor; the next Up starts from the newest entry.
    pub fn reset_navigation(&mut self) {
        self.index = None;
        self.draft.clear();
    }

    /// The most recent entry starting with `prefix` and longer than it.
    ///
    /// This is the ghost-text suggestion source: the newest history line that
    /// continues what is already typed.
    pub fn suggestion(&self, prefix: &[u8]) -> Option<&[u8]> {
        if prefix.is_empty() {
            return None;
        }
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.text.len() > prefix.len() && entry.text.starts_with(prefix))
            .map(|entry| &entry.text[prefix.len()..])
    }
}

/// Strip the newline (or CRLF) the editor puts on a submitted line.
fn trim(line: &[u8]) -> &[u8] {
    let mut line = line;
    while let [rest @ .., b'\n' | b'\r'] = line {
        line = rest;
    }
    line
}

/// The working directory at submit time, as shell bytes.
fn current_directory() -> Option<Vec<u8>> {
    std::env::current_dir().ok().map(bsh_sys::os::bytes_from_os)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(history: &History) -> Vec<Vec<u8>> {
        history.commands().map(<[u8]>::to_vec).collect()
    }

    fn numbers(history: &History) -> Vec<u32> {
        history.entries().iter().map(Entry::number).collect()
    }

    fn lines(items: &[&[u8]]) -> Vec<Vec<u8>> {
        items.iter().map(|item| item.to_vec()).collect()
    }

    fn file(tag: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "bsh-history-ring-{tag}-{}-{count}/history.db",
            std::process::id()
        ))
    }

    #[test]
    fn entries_are_numbered_from_one_and_keep_their_numbers() {
        let mut history = History::new();
        for line in lines(&[b"one", b"two", b"three"]) {
            history.push(&line);
        }
        assert_eq!(numbers(&history), [1, 2, 3]);
        assert_eq!(history.next_number(), 4);
        assert!(history.delete(2));
        assert_eq!(texts(&history), lines(&[b"one", b"three"]));
        assert_eq!(numbers(&history), [1, 3]);
        history.push(b"four");
        assert_eq!(numbers(&history), [1, 3, 4]);
    }

    #[test]
    fn deleting_the_newest_entry_reuses_its_number() {
        let mut history = History::new();
        history.push(b"one");
        history.push(b"two");
        assert!(history.delete(2));
        assert_eq!(history.next_number(), 2);
        history.push(b"three");
        assert_eq!(numbers(&history), [1, 2]);
    }

    #[test]
    fn negative_offsets_count_from_the_end() {
        let mut history = History::new();
        for line in lines(&[b"one", b"two", b"three"]) {
            history.push(&line);
        }
        assert_eq!(history.get(-1).map(Entry::text), Some(&b"three"[..]));
        assert_eq!(history.get(-3).map(Entry::text), Some(&b"one"[..]));
        assert_eq!(history.get(-4), None);
        assert_eq!(history.get(0), None);
        assert!(history.delete(-3));
        assert_eq!(texts(&history), lines(&[b"two", b"three"]));
    }

    #[test]
    fn only_the_submitted_line_is_current() {
        let mut history = History::new();
        history.push(b"typed");
        assert!(history.is_current());
        history.clear_current();
        assert!(!history.is_current());
        // A blank line never became an entry.
        history.push(b"  \n");
        assert!(!history.is_current());
    }

    #[test]
    fn blank_lines_are_not_recorded_and_the_submit_newline_is_stripped() {
        let mut history = History::new();
        history.push(b"\n");
        history.push(b"");
        history.push(b"  \t ");
        history.push(b"echo hi\n");
        history.push(b"printf x\r\n");
        assert_eq!(texts(&history), lines(&[b"echo hi", b"printf x"]));
    }

    #[test]
    fn removing_the_current_line_takes_the_number_back() {
        let mut history = History::new();
        history.push(b"one");
        history.push(b"typed");
        assert!(history.remove_current());
        assert_eq!(texts(&history), lines(&[b"one"]));
        assert_eq!(history.next_number(), 2);
        assert!(!history.remove_current());
    }

    #[test]
    fn navigation_walks_and_restores_the_draft() {
        let mut history = History::new();
        for line in lines(&[b"one", b"two", b"three"]) {
            history.push(&line);
        }
        assert_eq!(history.older(b"dra"), Some(b"three".to_vec()));
        assert_eq!(history.older(b"dra"), Some(b"two".to_vec()));
        assert_eq!(history.older(b"dra"), Some(b"one".to_vec()));
        assert_eq!(
            history.older(b"dra"),
            Some(b"one".to_vec()),
            "the oldest entry is a wall, not a wrap"
        );
        assert_eq!(history.newer(), Some(b"two".to_vec()));
        assert_eq!(history.newer(), Some(b"three".to_vec()));
        assert_eq!(history.newer(), Some(b"dra".to_vec()));
        assert_eq!(history.newer(), None);
    }

    #[test]
    fn suggestions_are_the_newest_continuation() {
        let mut history = History::new();
        history.push(b"git status\n");
        history.push(b"git commit -m x\n");
        assert_eq!(history.suggestion(b"git c"), Some(&b"ommit -m x"[..]));
        assert_eq!(history.suggestion(b"zzz"), None);
        assert_eq!(history.suggestion(b""), None);
    }

    #[test]
    fn a_new_session_starts_with_the_previous_one() {
        let path = file("reopen");
        {
            let mut history = History::open(&path, RECENT_DEPTH).unwrap();
            history.push(b"echo one");
            history.finish(Some(0), Some(3));
            history.push(b"echo two");
            history.finish(Some(1), Some(4));
        }
        let mut history = History::open(&path, RECENT_DEPTH).unwrap();
        assert_eq!(texts(&history), lines(&[b"echo one", b"echo two"]));
        assert_eq!(numbers(&history), [1, 2]);
        history.push(b"echo three");
        assert_eq!(numbers(&history), [1, 2, 3]);
        // Writes are queued; another connection may read before they land.
        history.flush();
        let reloaded = History::open(&path, RECENT_DEPTH).unwrap();
        assert_eq!(
            texts(&reloaded),
            lines(&[b"echo one", b"echo two", b"echo three"])
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn the_seed_is_limited_to_the_requested_depth() {
        let path = file("depth");
        {
            let mut history = History::open(&path, RECENT_DEPTH).unwrap();
            for line in lines(&[b"one", b"two", b"three"]) {
                history.push(&line);
                history.finish(Some(0), Some(1));
            }
        }
        let history = History::open(&path, 2).unwrap();
        assert_eq!(texts(&history), lines(&[b"two", b"three"]));
        // Numbers are row ids, so a window keeps the ids it was cut from.
        assert_eq!(numbers(&history), [2, 3]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_deleted_row_is_gone_and_its_number_is_not_reused_in_the_ring() {
        let path = file("delete");
        {
            let mut history = History::open(&path, RECENT_DEPTH).unwrap();
            for line in lines(&[b"one", b"two", b"three"]) {
                history.push(&line);
                history.finish(Some(0), Some(1));
            }
            assert!(history.delete(2));
            history.push(b"four");
            history.finish(Some(0), Some(1));
            assert_eq!(numbers(&history), [1, 3, 4]);
        }
        let history = History::open(&path, RECENT_DEPTH).unwrap();
        assert_eq!(texts(&history), lines(&[b"one", b"three", b"four"]));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn clearing_forgets_this_session_and_keeps_the_earlier_history() {
        let path = file("clear");
        {
            let mut history = History::open(&path, RECENT_DEPTH).unwrap();
            for line in lines(&[b"one", b"two"]) {
                history.push(&line);
                history.finish(Some(0), Some(1));
            }
        }
        {
            let mut history = History::open(&path, RECENT_DEPTH).unwrap();
            assert_eq!(numbers(&history), [1, 2]);
            history.push(b"mine");
            history.finish(Some(0), Some(1));
            assert_eq!(numbers(&history), [1, 2, 3]);
            history.clear();
            assert!(history.is_empty());
            // The table continues where it was: the ids are not the ring's to
            // give back.
            assert_eq!(history.next_number(), 4);
        }
        let history = History::open(&path, RECENT_DEPTH).unwrap();
        assert_eq!(texts(&history), lines(&[b"one", b"two"]));
        assert_eq!(numbers(&history), [1, 2]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn removing_an_expanded_line_deletes_its_row() {
        let path = file("expanded");
        {
            let mut history = History::open(&path, RECENT_DEPTH).unwrap();
            history.push(b"echo one");
            history.finish(Some(0), Some(1));
            history.push(b"!!");
            assert!(history.remove_current());
            history.push(b"echo one");
            history.finish(Some(0), Some(1));
        }
        let history = History::open(&path, RECENT_DEPTH).unwrap();
        assert_eq!(texts(&history), lines(&[b"echo one", b"echo one"]));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_reference_without_a_database_has_no_key() {
        let mut history = History::new();
        history.push(b"a");
        assert!(history.store().is_none());
        assert_eq!(history.len(), 1);
    }
}
