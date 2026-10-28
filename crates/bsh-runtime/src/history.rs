//! The shell history ring: numbered entries, the `HIST*` policies and the
//! history file.
//!
//! There is exactly one ring per shell. It lives here, in the runtime, so that
//! the two consumers cannot drift apart: the `history`/`fc` built-ins read and
//! mutate it through [`crate::ShellHost`], and the line editor in `bsh-repl`
//! borrows the same type for Up/Down navigation and ghost text.
//!
//! # Numbers
//!
//! Entries carry the number bash prints, and the number is not simply the
//! array index. Trimming the oldest entries because of `HISTSIZE` leaves the
//! remaining numbers alone (bash prints `501…` after a 500-entry window),
//! while deleting an entry with `history -d` renumbers the entries after it.
//! Both rules are bash 5.3.20's, measured:
//!
//! * `HISTSIZE=3`, five commands, then `history` prints `5 6 7`;
//! * `history -d 2` on a three-entry list prints `1 2` (the third became the
//!   second), not `1 3`.
//!
//! # Filtering
//!
//! `HISTCONTROL` and `HISTIGNORE` are applied in [`History::add`], which is
//! the single entry point for new lines. `history -s` therefore filters the
//! same way an entered command does, which is what bash does. `&` in
//! `HISTIGNORE` stands for the previous stored line.
//!
//! # The file
//!
//! The file is line-oriented. An entry that contains a newline (a multi-line
//! `if`, a here-document) lives in memory but is skipped when writing, rather
//! than corrupting the record format. bash's `cmdhist` joins such commands
//! with `;`; that is not implemented here and is documented as a gap.
//!
//! Reads and writes distinguish four operations, mirroring the built-in:
//! [`History::read_file`] (`-r`, append the whole file), [`History::read_new`]
//! (`-n`, append the part not read before), [`History::append_file`] (`-a`,
//! append the lines added this session) and [`History::write_file`] (`-w`,
//! rewrite the whole list). [`History::save`] is the exit path: `histappend`
//! selects append or rewrite, and the file is then truncated to
//! `HISTFILESIZE`.
//!
//! Measured bash quirk: `history -w` ignores `HISTFILESIZE`; the truncation
//! happens when the shell saves the file on exit. That is what this module
//! does, because the measurement beats the manual.

use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

/// Default `HISTSIZE` (and `HISTFILESIZE`) for an interactive bash shell.
pub const DEFAULT_ENTRIES: usize = 500;

/// One history entry: the command text and the number `history` prints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    number: u32,
    text: Vec<u8>,
}

impl Entry {
    /// The history number of this entry.
    pub fn number(&self) -> u32 {
        self.number
    }

    /// The command text, exactly as it was read (no trailing newline).
    pub fn text(&self) -> &[u8] {
        &self.text
    }
}

/// The `HISTCONTROL` filtering modes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Control {
    /// `ignorespace`: a line beginning with a space is not recorded.
    pub ignorespace: bool,
    /// `ignoredups`: a line equal to the previous one is not recorded.
    pub ignoredups: bool,
    /// `erasedups`: earlier copies of the line are removed when it is added.
    pub erasedups: bool,
}

impl Control {
    /// Parse a colon-separated `HISTCONTROL` value.
    ///
    /// `ignoreboth` is `ignorespace` plus `ignoredups`; unknown words are
    /// ignored, which is bash's behaviour.
    pub fn parse(value: &[u8]) -> Self {
        let mut control = Self::default();
        for word in value.split(|byte| *byte == b':') {
            match word {
                b"ignorespace" => control.ignorespace = true,
                b"ignoredups" => control.ignoredups = true,
                b"ignoreboth" => {
                    control.ignorespace = true;
                    control.ignoredups = true;
                }
                b"erasedups" => control.erasedups = true,
                _ => {}
            }
        }
        control
    }
}

/// Shell history: entries, navigation, policies and the history file.
#[derive(Clone, Debug)]
pub struct History {
    entries: Vec<Entry>,
    /// Number the next added entry receives.
    next: u32,
    max_entries: usize,
    file: Option<PathBuf>,
    file_max: usize,
    append_mode: bool,
    control: Control,
    ignore_patterns: Vec<Vec<u8>>,
    /// Lines added since the last append/write/read, for `history -a`.
    pending: Vec<Vec<u8>>,
    /// Physical lines of the history file already read, for `history -n`.
    file_read: usize,
    /// Editor navigation cursor into `entries`.
    index: Option<usize>,
    /// What the user was typing before the first Up press.
    draft: Vec<u8>,
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

impl History {
    /// An empty in-memory history with bash's interactive defaults.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            next: 1,
            max_entries: DEFAULT_ENTRIES,
            file: None,
            file_max: DEFAULT_ENTRIES,
            append_mode: false,
            control: Control::default(),
            ignore_patterns: Vec::new(),
            pending: Vec::new(),
            file_read: 0,
            index: None,
            draft: Vec::new(),
        }
    }

    /// Load `path`, then remember it for the file operations.
    ///
    /// A file that cannot be read is treated as absent; losing history is
    /// better than refusing to start a shell.
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let mut history = Self::new();
        history.file = Some(path.into());
        let _ = history.read_file();
        history
    }

    /// Use `$HISTFILE` when the environment names a non-empty path.
    ///
    /// This is convenience for a host without shell variables; the shell
    /// itself takes the path from its `HISTFILE` variable and calls
    /// [`History::set_file`].
    pub fn from_env() -> Self {
        match std::env::var_os("HISTFILE") {
            Some(path) if !path.is_empty() => Self::load(PathBuf::from(path)),
            _ => Self::new(),
        }
    }

    // -- configuration -----------------------------------------------------

    /// Set the maximum number of in-memory entries (`HISTSIZE`).
    pub fn set_max_entries(&mut self, max: usize) {
        self.max_entries = max;
        self.trim();
    }

    /// The in-memory limit.
    pub fn max_entries(&self) -> usize {
        self.max_entries
    }

    /// Set the file limit (`HISTFILESIZE`).
    pub fn set_file_max(&mut self, max: usize) {
        self.file_max = max;
    }

    /// The file limit.
    pub fn file_max(&self) -> usize {
        self.file_max
    }

    /// Whether the exit save appends instead of rewriting (`histappend`).
    pub fn set_append_mode(&mut self, append: bool) {
        self.append_mode = append;
    }

    /// Whether the exit save appends.
    pub fn append_mode(&self) -> bool {
        self.append_mode
    }

    /// Set the `HISTCONTROL` modes.
    pub fn set_control(&mut self, control: Control) {
        self.control = control;
    }

    /// Set the `HISTIGNORE` patterns.
    pub fn set_ignore_patterns(&mut self, patterns: Vec<Vec<u8>>) {
        self.ignore_patterns = patterns;
    }

    /// Remember the file the operations use.
    pub fn set_file(&mut self, path: Option<PathBuf>) {
        self.file = path;
    }

    /// The file, if one is configured.
    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    // -- reading and adding ------------------------------------------------

    /// Add one command line.
    ///
    /// Applies `HISTCONTROL`/`HISTIGNORE`, strips trailing newlines and
    /// ignores an empty line. Returns whether the line was stored.
    pub fn add(&mut self, line: &[u8]) -> bool {
        let mut line = line;
        while line.last() == Some(&b'\n') {
            line = &line[..line.len() - 1];
        }
        while line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        if line.is_empty() {
            return false;
        }
        if self.control.ignorespace && line.first() == Some(&b' ') {
            return false;
        }
        if self.control.ignoredups && self.entries.last().is_some_and(|last| last.text == line) {
            return false;
        }
        if self.is_ignored(line) {
            return false;
        }
        if self.control.erasedups {
            self.erase_duplicates(line);
        }
        let number = self.next;
        self.next = self.next.saturating_add(1);
        self.entries.push(Entry {
            number,
            text: line.to_vec(),
        });
        self.pending.push(line.to_vec());
        self.trim();
        self.reset_navigation();
        true
    }

    /// Add a line without any `HISTCONTROL`/`HISTIGNORE` filtering.
    ///
    /// Used by the file-read operations: the file is already the product of
    /// whatever filtering the shell that wrote it applied. A blank line is
    /// still skipped, because bash's reader never records one.
    fn add_from_file(&mut self, line: &[u8]) -> bool {
        if line.is_empty() {
            return false;
        }
        let number = self.next;
        self.next = self.next.saturating_add(1);
        self.entries.push(Entry {
            number,
            text: line.to_vec(),
        });
        self.trim();
        self.reset_navigation();
        true
    }

    /// Editor entry point: add a submitted line, ignoring navigation errors.
    pub fn push(&mut self, line: &[u8]) {
        self.add(line);
    }

    /// Whether `line` matches a `HISTIGNORE` pattern.
    fn is_ignored(&self, line: &[u8]) -> bool {
        let previous = self.entries.last().map(|entry| entry.text.as_slice());
        self.ignore_patterns.iter().any(|pattern| {
            let pattern = if pattern == b"&" {
                match previous {
                    Some(previous) => previous,
                    None => return false,
                }
            } else {
                pattern.as_slice()
            };
            bsh_pattern::matches(pattern, line, bsh_pattern::Flags::default())
        })
    }

    /// Remove earlier copies of `line` for `HISTCONTROL=erasedups`.
    ///
    /// The remaining entries are renumbered from the first one, matching
    /// bash's `history -d` behaviour.
    fn erase_duplicates(&mut self, line: &[u8]) {
        let Some(first) = self.entries.first().map(Entry::number) else {
            return;
        };
        self.entries.retain(|entry| entry.text != line);
        self.pending.retain(|pending| pending != line);
        for (offset, entry) in self.entries.iter_mut().enumerate() {
            entry.number = first.saturating_add(u32::try_from(offset).unwrap_or(u32::MAX));
        }
        self.next = self
            .entries
            .last()
            .map_or(first, |entry| entry.number.saturating_add(1));
    }

    /// Drop the oldest entries beyond `HISTSIZE`, keeping their numbers.
    fn trim(&mut self) {
        if self.entries.len() > self.max_entries {
            let excess = self.entries.len() - self.max_entries;
            self.entries.drain(..excess);
        }
    }

    /// Remove the most recent entry. Used by the editor when recording is off
    /// and by `fc -s`, which replaces the command that invoked it.
    pub fn remove_last(&mut self) -> bool {
        if self.entries.pop().is_some() {
            self.next = self
                .entries
                .last()
                .map_or(1, |entry| entry.number.saturating_add(1));
            self.reset_navigation();
            true
        } else {
            false
        }
    }

    // -- queries -----------------------------------------------------------

    /// Number of entries held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there is no history yet.
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

    /// The number the next added entry will receive.
    pub fn next_number(&self) -> u32 {
        self.next
    }

    /// The last `n` entries (`history n`); `None` means all of them.
    pub fn list(&self, n: Option<usize>) -> &[Entry] {
        match n {
            Some(n) if n < self.entries.len() => &self.entries[self.entries.len() - n..],
            _ => &self.entries,
        }
    }

    /// Resolve a history reference: a positive absolute number, or a negative
    /// offset from the end (`-1` is the newest entry). `0` is always invalid.
    pub fn get(&self, reference: i64) -> Option<&Entry> {
        self.index_of(reference).map(|index| &self.entries[index])
    }

    /// Index of a history reference, using the same rules as [`History::get`].
    pub fn index_of(&self, reference: i64) -> Option<usize> {
        if self.entries.is_empty() || reference == 0 {
            return None;
        }
        if reference > 0 {
            let target = u32::try_from(reference).ok()?;
            self.entries.iter().position(|entry| entry.number == target)
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
    /// Deleting renumbers the entries after the hole from the first surviving
    /// entry, which is what bash does for `history -d`.
    pub fn delete(&mut self, reference: i64) -> bool {
        let Some(index) = self.index_of(reference) else {
            return false;
        };
        let first = self.entries.first().map_or(1, Entry::number);
        self.entries.remove(index);
        for (offset, entry) in self.entries.iter_mut().enumerate() {
            entry.number = first.saturating_add(u32::try_from(offset).unwrap_or(u32::MAX));
        }
        self.next = self
            .entries
            .last()
            .map_or(first, |entry| entry.number.saturating_add(1));
        self.reset_navigation();
        true
    }

    /// Remove every entry, reset the numbering and forget the file read
    /// position (`history -c`).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.pending.clear();
        self.next = 1;
        self.file_read = 0;
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

    // -- the file ----------------------------------------------------------

    /// The file a write would target, or a "no history file" error.
    fn target(&self) -> std::io::Result<&Path> {
        self.file.as_deref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "HISTFILE is not set or is empty",
            )
        })
    }

    /// Read every line of the history file and append it (`history -r`).
    ///
    /// Returns the number of physical lines the file contains.
    pub fn read_file(&mut self) -> std::io::Result<usize> {
        let path = self.target()?.to_path_buf();
        let lines = read_lines(&path)?;
        for line in &lines {
            self.add_from_file(line);
        }
        self.file_read = lines.len();
        Ok(lines.len())
    }

    /// Read only the lines not read before (`history -n`).
    ///
    /// The position is a physical line count, not a content comparison, which
    /// is bash's rule.
    pub fn read_new(&mut self) -> std::io::Result<usize> {
        let path = self.target()?.to_path_buf();
        let lines = read_lines(&path)?;
        let start = self.file_read.min(lines.len());
        let mut added = 0;
        for line in &lines[start..] {
            if self.add_from_file(line) {
                added += 1;
            }
        }
        self.file_read = lines.len();
        Ok(added)
    }

    /// Append the lines added since the last write (`history -a`).
    pub fn append_file(&mut self) -> std::io::Result<()> {
        let path = self.target()?.to_path_buf();
        let mut file = open_append(&path)?;
        let mut existing = Vec::new();
        file.read_to_end(&mut existing)?;
        file.seek(std::io::SeekFrom::End(0))?;
        if !existing.is_empty() && !existing.ends_with(b"\n") {
            file.write_all(b"\n")?;
        }
        for line in &self.pending {
            if line.contains(&b'\n') {
                continue;
            }
            file.write_all(line)?;
            file.write_all(b"\n")?;
        }
        file.flush()?;
        self.pending.clear();
        self.file_read = count_lines(&path).unwrap_or(self.file_read);
        Ok(())
    }

    /// Rewrite the file with the whole in-memory list (`history -w`).
    ///
    /// As measured under bash, this does not apply `HISTFILESIZE`; the exit
    /// save truncates.
    pub fn write_file(&mut self) -> std::io::Result<()> {
        let path = self.target()?.to_path_buf();
        let mut file = create_history_file(&path)?;
        let mut lines = 0;
        for entry in &self.entries {
            if entry.text.contains(&b'\n') {
                continue;
            }
            file.write_all(&entry.text)?;
            file.write_all(b"\n")?;
            lines += 1;
        }
        file.flush()?;
        self.pending.clear();
        self.file_read = lines;
        Ok(())
    }

    /// Truncate the file to its newest `HISTFILESIZE` lines.
    pub fn truncate_file(&mut self) -> std::io::Result<()> {
        let Some(path) = self.file.clone() else {
            return Ok(());
        };
        let lines = match read_lines(&path) {
            Ok(lines) => lines,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if lines.len() <= self.file_max {
            return Ok(());
        }
        let keep = self.file_max;
        let mut file = create_history_file(&path)?;
        for line in &lines[lines.len() - keep..] {
            file.write_all(line)?;
            file.write_all(b"\n")?;
        }
        file.flush()?;
        self.file_read = keep;
        Ok(())
    }

    /// Save on exit: append when `histappend` is on, rewrite otherwise, then
    /// truncate to `HISTFILESIZE`.
    ///
    /// An empty history does not create the file, matching bash: a shell that
    /// recorded nothing leaves no file behind.
    pub fn save(&mut self) -> std::io::Result<()> {
        if self.file.is_none() {
            self.pending.clear();
            return Ok(());
        }
        if self.entries.is_empty() && self.pending.is_empty() {
            return Ok(());
        }
        if self.append_mode {
            self.append_file()?;
        } else {
            self.write_file()?;
        }
        self.truncate_file()
    }
}

/// Read a history file as physical lines, ignoring a trailing empty line.
///
/// Missing files are an error; callers that tolerate absence check first.
fn read_lines(path: &Path) -> std::io::Result<Vec<Vec<u8>>> {
    let bytes = std::fs::read(path)?;
    let mut lines: Vec<Vec<u8>> = bytes
        .split(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    if lines.last().is_some_and(Vec::is_empty) {
        lines.pop();
    }
    Ok(lines)
}

/// Count the physical lines of a file, if it can be read.
fn count_lines(path: &Path) -> std::io::Result<usize> {
    read_lines(path).map(|lines| lines.len())
}

/// Open `path` for appending, creating it with bash's `0600` permission.
fn open_append(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

/// Create (or truncate) `path` with bash's `0600` permission.
fn create_history_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn free_name(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("bsh-history-{tag}-{}-{count}", std::process::id()))
    }

    fn texts(history: &History) -> Vec<Vec<u8>> {
        history.commands().map(<[u8]>::to_vec).collect()
    }

    fn numbers(history: &History) -> Vec<u32> {
        history.entries().iter().map(Entry::number).collect()
    }

    fn lines(items: &[&[u8]]) -> Vec<Vec<u8>> {
        items.iter().map(|item| item.to_vec()).collect()
    }

    #[test]
    fn entries_keep_their_numbers_across_trimming() {
        let mut history = History::new();
        history.set_max_entries(3);
        for line in lines(&[b"one", b"two", b"three", b"four", b"five"]) {
            history.add(&line);
        }
        assert_eq!(texts(&history), lines(&[b"three", b"four", b"five"]));
        assert_eq!(numbers(&history), [3, 4, 5]);
        assert_eq!(history.next_number(), 6);
    }

    #[test]
    fn deleting_renumbers_the_later_entries() {
        let mut history = History::new();
        for line in lines(&[b"one", b"two", b"three"]) {
            history.add(&line);
        }
        assert!(history.delete(2));
        assert_eq!(texts(&history), lines(&[b"one", b"three"]));
        assert_eq!(numbers(&history), [1, 2]);
        history.add(b"four");
        assert_eq!(numbers(&history), [1, 2, 3]);
    }

    #[test]
    fn deleting_after_trimming_renumbers_from_the_base() {
        let mut history = History::new();
        history.set_max_entries(3);
        for line in lines(&[b"one", b"two", b"three", b"four", b"five"]) {
            history.add(&line);
        }
        // Numbers are 3, 4, 5; deleting the first renumbers the rest from 3.
        assert!(history.delete(3));
        assert_eq!(texts(&history), lines(&[b"four", b"five"]));
        assert_eq!(numbers(&history), [3, 4]);
    }

    #[test]
    fn negative_offsets_count_from_the_end() {
        let mut history = History::new();
        for line in lines(&[b"one", b"two", b"three"]) {
            history.add(&line);
        }
        assert_eq!(history.get(-1).map(Entry::text), Some(&b"three"[..]));
        assert_eq!(history.get(-3).map(Entry::text), Some(&b"one"[..]));
        assert_eq!(history.get(-4), None);
        assert_eq!(history.get(0), None);
        assert!(history.delete(-3));
        assert_eq!(texts(&history), lines(&[b"two", b"three"]));
    }

    #[test]
    fn blank_lines_are_not_recorded_and_the_submit_newline_is_stripped() {
        let mut history = History::new();
        assert!(!history.add(b"\n"));
        assert!(!history.add(b""));
        assert!(history.add(b"echo hi\n"));
        assert_eq!(texts(&history), lines(&[b"echo hi"]));
    }

    #[test]
    fn duplicates_are_kept_without_histcontrol() {
        let mut history = History::new();
        history.add(b"ls");
        history.add(b"ls");
        assert_eq!(texts(&history), lines(&[b"ls", b"ls"]));
    }

    #[test]
    fn ignoredups_compares_with_the_previous_entry_only() {
        let mut history = History::new();
        history.set_control(Control::parse(b"ignoredups"));
        history.add(b"ls");
        history.add(b"ls");
        history.add(b"cd");
        history.add(b"ls");
        assert_eq!(texts(&history), lines(&[b"ls", b"cd", b"ls"]));
    }

    #[test]
    fn ignorespace_skips_lines_starting_with_a_space() {
        let mut history = new_with(b"ignorespace");
        history.add(b"echo a");
        history.add(b" echo b");
        history.add(b"\techo c");
        assert_eq!(texts(&history), lines(&[b"echo a", b"\techo c"]));
    }

    #[test]
    fn ignoreboth_combines_both_modes() {
        let mut history = new_with(b"ignoreboth");
        history.add(b"echo a");
        history.add(b"echo a");
        history.add(b" echo space");
        history.add(b"echo b");
        assert_eq!(texts(&history), lines(&[b"echo a", b"echo b"]));
    }

    #[test]
    fn erasedups_removes_earlier_copies() {
        let mut history = new_with(b"erasedups");
        history.add(b"echo a");
        history.add(b"echo b");
        history.add(b"echo a");
        history.add(b"echo c");
        assert_eq!(texts(&history), lines(&[b"echo b", b"echo a", b"echo c"]));
        assert_eq!(numbers(&history), [1, 2, 3]);
    }

    fn new_with(control: &[u8]) -> History {
        let mut history = History::new();
        history.set_control(Control::parse(control));
        history
    }

    #[test]
    fn histignore_matches_whole_lines() {
        let mut history = History::new();
        history.set_ignore_patterns(vec![b"echo*".to_vec(), b"*secret*".to_vec()]);
        history.add(b"echo a");
        history.add(b"ls");
        history.add(b"echo secret");
        history.add(b"echo b");
        assert_eq!(texts(&history), lines(&[b"ls"]));
    }

    #[test]
    fn ampersand_means_the_previous_stored_line() {
        let mut history = History::new();
        history.set_ignore_patterns(vec![b"&".to_vec()]);
        history.add(b"echo a");
        history.add(b"echo a");
        history.add(b"ls");
        history.add(b"echo a");
        assert_eq!(texts(&history), lines(&[b"echo a", b"ls", b"echo a"]));
    }

    #[test]
    fn history_ignore_does_not_filter_the_file_reader() {
        let path = free_name("ignore-read");
        std::fs::write(&path, b"echo a\necho a\n").unwrap();
        let mut history = History::new();
        history.set_ignore_patterns(vec![b"echo*".to_vec()]);
        history.set_file(Some(path.clone()));
        history.read_file().unwrap();
        assert_eq!(texts(&history), lines(&[b"echo a", b"echo a"]));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn saving_appends_without_corrupting_the_file() {
        let path = free_name("history");
        std::fs::write(&path, b"old-cmd-no-newline").unwrap();
        let mut history = History::load(&path);
        assert_eq!(texts(&history), lines(&[b"old-cmd-no-newline"]));
        history.push(b"new one\n");
        history.push(b"if true\nthen echo hi\nfi\n");
        history.save().unwrap();
        let contents = std::fs::read(&path).unwrap();
        assert_eq!(contents, b"old-cmd-no-newline\nnew one\n");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn write_file_rewrites_and_clears_pending() {
        let path = free_name("write");
        std::fs::write(&path, b"stale\n").unwrap();
        let mut history = History::new();
        history.set_file(Some(path.clone()));
        history.add(b"one");
        history.add(b"two");
        history.write_file().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"one\ntwo\n");
        // Nothing is pending after a write, so a later append writes nothing.
        history.append_file().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"one\ntwo\n");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn append_file_writes_only_new_lines() {
        let path = free_name("append");
        std::fs::write(&path, b"old\n").unwrap();
        let mut history = History::new();
        history.set_file(Some(path.clone()));
        history.add(b"one");
        history.append_file().unwrap();
        history.add(b"two");
        history.append_file().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"old\none\ntwo\n");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn read_file_appends_every_line_and_read_new_continues_the_offset() {
        let first = free_name("read-first");
        let second = free_name("read-second");
        std::fs::write(&first, b"a\nb\n").unwrap();
        std::fs::write(&second, b"a\nb\nc\nd\n").unwrap();
        let mut history = History::new();
        history.set_file(Some(first.clone()));
        history.read_file().unwrap();
        assert_eq!(texts(&history), lines(&[b"a", b"b"]));
        history.read_new().unwrap();
        assert_eq!(texts(&history), [b"a", b"b"], "no new lines in the file");
        history.set_file(Some(second.clone()));
        history.read_new().unwrap();
        assert_eq!(texts(&history), lines(&[b"a", b"b", b"c", b"d"]));
        std::fs::remove_file(&first).unwrap();
        std::fs::remove_file(&second).unwrap();
    }

    #[test]
    fn read_file_twice_appends_twice() {
        let path = free_name("read-twice");
        std::fs::write(&path, b"orig\n").unwrap();
        let mut history = History::new();
        history.set_file(Some(path.clone()));
        history.read_file().unwrap();
        history.read_file().unwrap();
        assert_eq!(texts(&history), lines(&[b"orig", b"orig"]));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn exit_write_truncates_to_histfilesize() {
        let path = free_name("truncate");
        let mut history = History::new();
        history.set_file(Some(path.clone()));
        history.set_file_max(2);
        for line in lines(&[b"one", b"two", b"three", b"four"]) {
            history.add(&line);
        }
        history.save().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"three\nfour\n");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn histappend_save_appends_then_truncates() {
        let path = free_name("append-truncate");
        std::fs::write(&path, b"old1\nold2\nold3\nold4\n").unwrap();
        let mut history = History::new();
        history.set_file(Some(path.clone()));
        history.set_append_mode(true);
        history.set_file_max(3);
        history.add(b"new");
        history.save().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"old3\nold4\nnew\n");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_missing_file_loads_empty_and_saving_an_empty_history_creates_nothing() {
        let path = free_name("missing");
        let mut history = History::load(&path);
        assert!(history.is_empty());
        history.save().unwrap();
        assert!(!path.exists(), "an empty history leaves no file");
    }

    #[test]
    fn file_operations_without_a_file_report_an_error() {
        let mut history = History::new();
        assert!(history.read_file().is_err());
        assert!(history.read_new().is_err());
        assert!(history.append_file().is_err());
        assert!(history.write_file().is_err());
        // Saving without a file is a no-op, not an error: a non-interactive
        // shell has no history file and must still exit cleanly.
        history.add(b"x");
        assert!(history.save().is_ok());
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
    fn clear_resets_numbering_and_the_read_offset() {
        let path = free_name("clear");
        std::fs::write(&path, b"a\nb\n").unwrap();
        let mut history = History::new();
        history.set_file(Some(path.clone()));
        history.read_file().unwrap();
        history.clear();
        assert!(history.is_empty());
        assert_eq!(history.next_number(), 1);
        history.add(b"fresh");
        assert_eq!(numbers(&history), [1]);
        // The reader starts over at the beginning of the file.
        history.read_new().unwrap();
        assert_eq!(texts(&history), lines(&[b"fresh", b"a", b"b"]));
        std::fs::remove_file(&path).unwrap();
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
}
