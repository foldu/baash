//! In-memory history, up/down navigation, and optional persistence.
//!
//! History is an oldest-first vector with a hard cap. Consecutive duplicates
//! are dropped (typing `ls` three times should not cost three Up presses), and
//! a blank or whitespace-only line is not recorded at all.
//!
//! Persistence appends only the entries added during this session, so a
//! `HISTFILE` written by another shell is extended, never rewritten. The file
//! is line-oriented: an entry containing a newline (a multi-line `if`, a
//! here-doc) lives in memory and is skipped by [`History::save`] rather than
//! corrupting the file.

use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

/// The default number of entries kept in memory.
pub const DEFAULT_LIMIT: usize = 500;

/// Shell history.
#[derive(Clone, Debug, Default)]
pub struct History {
    entries: Vec<Vec<u8>>,
    pending: Vec<Vec<u8>>,
    limit: usize,
    file: Option<PathBuf>,
    index: Option<usize>,
    draft: Vec<u8>,
}

impl History {
    /// An in-memory history with the default limit and no file.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            pending: Vec::new(),
            limit: DEFAULT_LIMIT,
            file: None,
            index: None,
            draft: Vec::new(),
        }
    }

    /// Load `path`, then keep appending to it in [`History::save`].
    ///
    /// A file that cannot be read is treated as absent; losing history is
    /// better than refusing to start a shell.
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut history = Self::new();
        if let Ok(bytes) = std::fs::read(&path) {
            for line in bytes.split(|&byte| byte == b'\n') {
                if !line.is_empty() {
                    history.entries.push(line.to_vec());
                }
            }
            if history.entries.len() > DEFAULT_LIMIT {
                let excess = history.entries.len() - DEFAULT_LIMIT;
                history.entries.drain(..excess);
            }
        }
        history.file = Some(path);
        history
    }

    /// Load from `$HISTFILE` when it names a file, and remember it for appending.
    ///
    /// An unset or empty `HISTFILE` means no persistence: the shell does not
    /// guess a filename and write to it.
    pub fn from_env() -> Self {
        match std::env::var_os("HISTFILE") {
            Some(path) if !path.is_empty() => Self::load(PathBuf::from(path)),
            _ => Self::new(),
        }
    }

    /// Set the maximum number of in-memory entries.
    #[must_use]
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit.max(1);
        self.truncate();
        self
    }

    /// Set the file appended to by [`History::save`].
    pub fn set_file(&mut self, path: impl Into<PathBuf>) {
        self.file = Some(path.into());
    }

    /// The file this history appends to, if any.
    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    /// Number of entries held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there is no history yet.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entries, oldest first.
    pub fn entries(&self) -> &[Vec<u8>] {
        &self.entries
    }

    /// Add a command line.
    ///
    /// The trailing newline an editor's submit adds is not part of the command
    /// and is stripped; internal newlines are kept.
    pub fn push(&mut self, line: &[u8]) {
        let mut line = line;
        while line.last() == Some(&b'\n') {
            line = &line[..line.len() - 1];
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            return;
        }
        if self.entries.last().is_some_and(|last| last == line) {
            return;
        }
        let entry = line.to_vec();
        self.entries.push(entry.clone());
        self.pending.push(entry);
        self.truncate();
        self.reset_navigation();
    }

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
        self.index.map(|index| self.entries[index].clone())
    }

    /// The next (newer) entry, or the saved draft after the newest one.
    ///
    /// `None` means there is no navigation in progress.
    pub fn newer(&mut self) -> Option<Vec<u8>> {
        let index = self.index?;
        if index + 1 < self.entries.len() {
            self.index = Some(index + 1);
            Some(self.entries[index + 1].clone())
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

    /// Drop the oldest entries beyond the limit.
    fn truncate(&mut self) {
        if self.entries.len() > self.limit {
            let excess = self.entries.len() - self.limit;
            self.entries.drain(..excess);
        }
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
            .find(|entry| entry.len() > prefix.len() && entry.starts_with(prefix))
            .map(|entry| &entry[prefix.len()..])
    }

    /// Append the entries added this session to the history file.
    ///
    /// The existing file is preserved; if it does not end with a newline one is
    /// added first, and multi-line entries are skipped rather than written
    /// across several records.
    pub fn save(&mut self) -> std::io::Result<()> {
        let Some(path) = self.file.clone() else {
            self.pending.clear();
            return Ok(());
        };
        if self.pending.is_empty() {
            return Ok(());
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)?;
        let mut existing = Vec::new();
        file.read_to_end(&mut existing)?;
        file.seek(std::io::SeekFrom::End(0))?;
        if !existing.is_empty() && !existing.ends_with(b"\n") {
            file.write_all(b"\n")?;
        }
        for entry in &self.pending {
            if entry.contains(&b'\n') {
                continue;
            }
            file.write_all(entry)?;
            file.write_all(b"\n")?;
        }
        file.flush()?;
        self.pending.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn free_name(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("bsh-repl-{tag}-{}-{count}", std::process::id()))
    }

    #[test]
    fn consecutive_duplicates_are_dropped() {
        let mut history = History::new();
        history.push(b"ls\n");
        history.push(b"ls\n");
        history.push(b"cd\n");
        history.push(b"ls\n");
        assert_eq!(
            history.entries(),
            &[b"ls".to_vec(), b"cd".to_vec(), b"ls".to_vec()]
        );
    }

    #[test]
    fn blank_lines_are_not_recorded() {
        let mut history = History::new();
        history.push(b"\n");
        history.push(b"   \n");
        assert!(history.is_empty());
    }

    #[test]
    fn the_submit_newline_is_stripped() {
        let mut history = History::new();
        history.push(b"echo hi\n");
        assert_eq!(history.entries(), &[b"echo hi".to_vec()]);
    }

    #[test]
    fn navigation_walks_and_restores_the_draft() {
        let mut history = History::new();
        history.push(b"one\n");
        history.push(b"two\n");
        history.push(b"three\n");
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
    fn the_limit_drops_the_oldest() {
        let mut history = History::new().with_limit(2);
        history.push(b"one\n");
        history.push(b"two\n");
        history.push(b"three\n");
        assert_eq!(history.entries(), &[b"two".to_vec(), b"three".to_vec()]);
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
    fn saving_appends_without_corrupting_the_file() {
        let path = free_name("history");
        std::fs::write(&path, b"old-cmd-no-newline").unwrap();
        let mut history = History::load(&path);
        assert_eq!(history.entries(), &[b"old-cmd-no-newline".to_vec()]);
        history.push(b"new one\n");
        history.push(b"if true\nthen echo hi\nfi\n");
        history.save().unwrap();
        let contents = std::fs::read(&path).unwrap();
        assert_eq!(contents, b"old-cmd-no-newline\nnew one\n");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn saving_without_a_file_is_a_no_op() {
        let mut history = History::new();
        history.push(b"ls\n");
        history.save().unwrap();
    }

    #[test]
    fn a_missing_file_loads_empty() {
        let path = free_name("missing-history");
        let history = History::load(&path);
        assert!(history.is_empty());
    }
}
