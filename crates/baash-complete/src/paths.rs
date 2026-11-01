//! Path completion: component-prefix matching with a cached `readdir`.
//!
//! The rules come straight from `DESIGN.md` §6.6 item 1 and §6.5:
//!
//! * Only the directory the user typed is listed; nothing descends eagerly.
//! * A directory is inserted with a trailing `/` and never gains a trailing
//!   space.
//! * `readdir` is capped (10 000 entries by default) so pressing TAB in a
//!   pathological directory cannot hang the editor.
//! * Each directory listing is cached together with the directory's mtime and
//!   re-read when the mtime moves.
//! * Matching is by component prefix: the directory part of the typed word is
//!   resolved, and only the final component is matched. Entries that do not
//!   match at all are still returned — ranking's fuzzy tier decides — but a
//!   `dirs first` request orders directories ahead of files.
//!
//! `~` and `~/…` are expanded against `HOME`; `~user`, `$VAR` and glob
//! characters in a path prefix are not expanded (that would need the runtime's
//! expansion, which completion must not trigger).

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::candidate::{CandKind, Candidate};
use crate::context::Ctx;
use crate::sources::{self, Provider, ProviderId};
use crate::text;

/// How many entries one directory may contribute before the listing is
/// truncated.
pub const DEFAULT_ENTRY_CAP: usize = 10_000;

/// How many directory listings are cached before the whole cache is dropped.
const MAX_CACHED_DIRS: usize = 256;

/// One directory entry, as read from disk.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    name: Vec<u8>,
    is_dir: bool,
}

#[derive(Clone, Debug)]
struct DirCache {
    mtime: Option<SystemTime>,
    entries: Arc<Vec<Entry>>,
}

/// Which entries a path completion may offer.
///
/// The default depends on the command (`cd` and friends ask for directories
/// only). `complete -A file` and `complete -A directory` override it, which is
/// why this is a parameter of the provider rather than a fact about the
/// context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathMode {
    /// Files and directories.
    Any,
    /// Regular files only.
    Files,
    /// Directories only.
    Directories,
}

/// Provides files and directories.
pub struct PathProvider {
    root: Mutex<PathBuf>,
    home: Mutex<Option<PathBuf>>,
    entry_cap: usize,
    cache: Mutex<HashMap<PathBuf, DirCache>>,
}

impl PathProvider {
    /// A provider rooted at the process's current directory.
    pub fn new() -> Self {
        Self::with_root(PathBuf::from("."))
    }

    /// A provider that resolves relative paths against `root`.
    ///
    /// Tests use this instead of changing the process's working directory,
    /// which would race with other tests.
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Mutex::new(root.into()),
            home: Mutex::new(std::env::var_os("HOME").map(PathBuf::from)),
            entry_cap: DEFAULT_ENTRY_CAP,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Override the directory `~` expands to.
    #[must_use]
    pub fn with_home(mut self, home: impl Into<PathBuf>) -> Self {
        self.home = Mutex::new(Some(home.into()));
        self
    }

    /// Replace the directory `~` expands to, as `HOME` currently says.
    ///
    /// The shell calls this before each prompt: `HOME=/elsewhere` must change
    /// what `~/` completes, and the environment the process started with is
    /// only a snapshot.
    pub fn set_home(&self, home: Option<PathBuf>) {
        *lock(&self.home) = home;
    }

    /// Override the per-directory entry cap.
    #[must_use]
    pub fn with_entry_cap(mut self, cap: usize) -> Self {
        self.entry_cap = cap.max(1);
        self
    }

    /// The directory relative paths resolve against.
    pub fn root(&self) -> PathBuf {
        lock(&self.root).clone()
    }

    /// Change the directory relative paths resolve against.
    ///
    /// The shell calls this after `cd`. Cached listings stay valid because the
    /// cache is keyed by the resolved path.
    pub fn set_root(&self, root: impl Into<PathBuf>) {
        *lock(&self.root) = root.into();
    }

    /// Drop every cached listing.
    pub fn clear_cache(&self) {
        lock(&self.cache).clear();
    }

    /// List `directory`, using the cache when its mtime has not moved.
    fn entries(&self, directory: &Path) -> Arc<Vec<Entry>> {
        let mtime = fs::metadata(directory)
            .and_then(|metadata| metadata.modified())
            .ok();
        {
            let cache = lock(&self.cache);
            if let Some(cached) = cache.get(directory) {
                if cached.mtime == mtime && mtime.is_some() {
                    return Arc::clone(&cached.entries);
                }
            }
        }

        let mut entries = Vec::new();
        if let Ok(read) = fs::read_dir(directory) {
            for entry in read.take(self.entry_cap) {
                let Ok(entry) = entry else { continue };
                let name = os_bytes(entry.file_name());
                if name == b"." || name == b".." {
                    continue;
                }
                // `file_type` uses the directory-entry type on platforms that
                // provide it; a symlink needs a follow-up check so a link to a
                // directory still gets its trailing slash.
                let file_type = entry.file_type();
                let is_dir = match file_type {
                    Ok(file_type) if file_type.is_dir() => true,
                    Ok(file_type) if file_type.is_symlink() => {
                        fs::metadata(entry.path()).is_ok_and(|meta| meta.is_dir())
                    }
                    _ => false,
                };
                entries.push(Entry { name, is_dir });
            }
        }

        let mut cache = lock(&self.cache);
        if cache.len() >= MAX_CACHED_DIRS {
            cache.clear();
        }
        let entries = Arc::new(entries);
        cache.insert(
            directory.to_path_buf(),
            DirCache {
                mtime,
                entries: Arc::clone(&entries),
            },
        );
        entries
    }

    /// Resolve the typed directory component to a real path.
    fn resolve_dir(&self, typed: &[u8]) -> Option<PathBuf> {
        let home = lock(&self.home).clone();
        if typed.starts_with(b"~/") {
            let rest = path_from_bytes(&typed[2..]);
            return home.map(|home| home.join(rest));
        }
        if typed == b"~" || typed == b"~/" {
            return home;
        }
        if typed.starts_with(b"~") {
            // `~user` would need the password database, which is the runtime's
            // `home_dir` hook; completion does not guess.
            return None;
        }
        let path = path_from_bytes(typed);
        if path.is_absolute() {
            Some(path)
        } else if typed.is_empty() {
            Some(self.root())
        } else {
            Some(self.root().join(path))
        }
    }
}

impl Default for PathProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for PathProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Path
    }

    fn applies(&self, ctx: &Ctx) -> bool {
        if ctx.param || ctx.arith || ctx.heredoc {
            return false;
        }
        match ctx.expect {
            baash_syntax::Expect::Word
            | baash_syntax::Expect::RedirectTarget
            | baash_syntax::Expect::ForList => true,
            baash_syntax::Expect::Command => text::looks_like_path(&ctx.prefix),
            _ => false,
        }
    }

    fn candidates(&self, ctx: &Ctx) -> Vec<Candidate> {
        let directories_only = ctx
            .command_name
            .as_deref()
            .is_some_and(|name| sources::is_one_of(name, sources::DIRECTORY_COMMANDS));
        let mode = if directories_only {
            PathMode::Directories
        } else {
            PathMode::Any
        };
        self.candidates_in(ctx, mode)
    }
}

impl PathProvider {
    /// Candidates for `ctx`, restricted to `mode`.
    ///
    /// `complete -A file` and `complete -A directory` ask for a mode that the
    /// command table cannot know, so the engine calls this directly.
    pub fn candidates_in(&self, ctx: &Ctx, mode: PathMode) -> Vec<Candidate> {
        let prefix = &ctx.prefix;
        let (typed_dir, base) = if prefix == b"~" {
            // A bare `~` names the home directory; the candidate replaces the
            // whole word, so the trailing slash belongs to the insert.
            (&b"~/"[..], &b""[..])
        } else {
            match prefix.iter().rposition(|&b| b == b'/') {
                Some(index) => (&prefix[..=index], &prefix[index + 1..]),
                None => (&prefix[..0], &prefix[..]),
            }
        };
        let Some(directory) = self.resolve_dir(typed_dir) else {
            return Vec::new();
        };

        let dirs_first = ctx.expect == baash_syntax::Expect::RedirectTarget;
        let show_hidden = base.first() == Some(&b'.');

        let mut candidates = Vec::new();
        for entry in self.entries(&directory).iter() {
            if !show_hidden && entry.name.first() == Some(&b'.') {
                continue;
            }
            match mode {
                PathMode::Directories if !entry.is_dir => continue,
                PathMode::Files if entry.is_dir => continue,
                _ => {}
            }
            let mut insert = Vec::with_capacity(typed_dir.len() + entry.name.len() + 1);
            insert.extend_from_slice(typed_dir);
            insert.extend_from_slice(&entry.name);
            if entry.is_dir {
                insert.push(b'/');
            }
            let mut candidate = Candidate::new(insert, CandKind::Path);
            candidate.append_space = !entry.is_dir;
            if dirs_first && entry.is_dir {
                // Directories first for redirection targets: a redirect almost
                // always names a file, but a directory is still a legal target
                // and a common way to build a path.
                candidate.frecency = 1.0;
            }
            candidates.push(candidate);
        }
        candidates
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

#[cfg(unix)]
fn os_bytes(value: OsString) -> Vec<u8> {
    use std::os::unix::ffi::OsStringExt;
    value.into_vec()
}

#[cfg(not(unix))]
fn os_bytes(value: OsString) -> Vec<u8> {
    value.to_string_lossy().into_owned().into_bytes()
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}
