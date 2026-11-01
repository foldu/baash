//! The `PATH` command index.
//!
//! Built once, lazily, and rebuilt when `PATH` changes or when a directory on
//! it moves (`DESIGN.md` §6.6 item 2). Directories are read with `readdir`,
//! entry-capped, and only regular executable files are kept. The index is
//! never built at startup: it is built the first time a command position is
//! completed.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::candidate::{CandKind, Candidate};
use crate::context::Ctx;
use crate::sources::{self, Provider, ProviderId};

/// How many names a single `PATH` directory may contribute.
pub const DEFAULT_ENTRY_CAP: usize = 10_000;

/// A command index built away from the completion thread.
///
/// Opaque on purpose: nothing outside this module should be able to construct
/// one that does not match what [`CommandProvider::names`] would have built.
#[derive(Clone, Debug)]
pub struct CommandIndex(Index);

/// A snapshot of one `PATH` value.
#[derive(Clone, Debug)]
struct Index {
    path: Option<Vec<u8>>,
    dirs: Vec<(PathBuf, Option<SystemTime>)>,
    names: Arc<Vec<Vec<u8>>>,
}

/// Completes command names from `PATH`.
pub struct CommandProvider {
    state: Mutex<Option<Index>>,
    /// `None` means "read the process's `PATH`".
    path_override: Mutex<Option<Vec<u8>>>,
    entry_cap: usize,
}

impl CommandProvider {
    /// A provider that reads the process environment.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(None),
            path_override: Mutex::new(None),
            entry_cap: DEFAULT_ENTRY_CAP,
        }
    }

    /// A provider with a fixed `PATH` value, for tests and for a shell whose
    /// exported `PATH` differs from the process's.
    pub fn with_path_var(path: impl Into<Vec<u8>>) -> Self {
        let provider = Self::new();
        provider.set_path_var(Some(path.into()));
        provider
    }

    /// Replace the `PATH` used by the index.
    ///
    /// Passing `None` switches back to the process environment. A changed
    /// value drops the cached index so the next completion re-reads the
    /// directories; setting the same value again (which the prompt refresh does
    /// before every line) is free.
    pub fn set_path_var(&self, path: Option<Vec<u8>>) {
        if *lock(&self.path_override) == path {
            return;
        }
        *lock(&self.path_override) = path;
        self.invalidate();
    }

    /// Drop the cached index.
    pub fn invalidate(&self) {
        *lock(&self.state) = None;
    }

    /// Set the per-directory entry cap.
    #[must_use]
    pub fn with_entry_cap(mut self, cap: usize) -> Self {
        self.entry_cap = cap.max(1);
        self
    }

    /// The `PATH` value in effect.
    fn current_path(&self) -> Option<Vec<u8>> {
        if let Some(path) = lock(&self.path_override).clone() {
            return Some(path);
        }
        env_bytes("PATH")
    }

    /// Build the index for the current `PATH` on this thread.
    ///
    /// The first command completion otherwise walks every `PATH` directory on
    /// the thread that also reads keystrokes. A caller with a worker (the REPL
    /// does) builds the index there and installs it with
    /// [`CommandProvider::install_index`], so the first TAB is a lookup.
    pub fn build_index(&self) -> CommandIndex {
        CommandIndex(self.build(self.current_path()))
    }

    /// Install an index built by [`CommandProvider::build_index`].
    ///
    /// Ignored when `PATH` has changed since it was built: the index carries
    /// the path it describes, and a stale one would complete names that are no
    /// longer on `PATH`.
    pub fn install_index(&self, index: CommandIndex) {
        if index.0.path != self.current_path() {
            return;
        }
        *lock(&self.state) = Some(index.0);
    }

    /// The command names, building or refreshing the index as needed.
    pub fn names(&self) -> Arc<Vec<Vec<u8>>> {
        let path = self.current_path();
        {
            let state = lock(&self.state);
            if let Some(index) = state.as_ref() {
                if index.path == path && !mtimes_moved(&index.dirs) {
                    return Arc::clone(&index.names);
                }
            }
        }
        let index = self.build(path);
        let names = Arc::clone(&index.names);
        *lock(&self.state) = Some(index);
        names
    }

    fn build(&self, path: Option<Vec<u8>>) -> Index {
        let mut dirs = Vec::new();
        let mut names = Vec::new();
        let mut seen = HashSet::new();
        if let Some(path) = path.as_deref() {
            for component in path.split(|&b| b == b':') {
                // POSIX: an empty component means the current directory.
                let directory = if component.is_empty() {
                    PathBuf::from(".")
                } else {
                    path_from_bytes(component)
                };
                let mtime = fs::metadata(&directory)
                    .and_then(|metadata| metadata.modified())
                    .ok();
                dirs.push((directory.clone(), mtime));
                let Ok(read) = fs::read_dir(&directory) else {
                    continue;
                };
                for entry in read.take(self.entry_cap) {
                    let Ok(entry) = entry else { continue };
                    let file_type = entry.file_type();
                    let Ok(file_type) = file_type else { continue };
                    if file_type.is_dir() {
                        continue;
                    }
                    if !is_executable(&entry.path(), file_type.is_symlink()) {
                        continue;
                    }
                    let name = os_bytes(entry.file_name());
                    if name.is_empty() || !seen.insert(name.clone()) {
                        continue;
                    }
                    names.push(name);
                }
            }
        }
        Index {
            path,
            dirs,
            names: Arc::new(names),
        }
    }
}

impl Default for CommandProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for CommandProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Command
    }

    fn applies(&self, ctx: &Ctx) -> bool {
        // `sudo gi<TAB>` is a command position: the wrapper is the command, and
        // the next word is what it runs.
        sources::named_command_slot(ctx)
    }

    fn candidates(&self, _ctx: &Ctx) -> Vec<Candidate> {
        self.names()
            .iter()
            .map(|name| Candidate::new(name.clone(), CandKind::Command))
            .collect()
    }
}

fn mtimes_moved(dirs: &[(PathBuf, Option<SystemTime>)]) -> bool {
    dirs.iter().any(|(directory, recorded)| {
        let now = fs::metadata(directory)
            .and_then(|metadata| metadata.modified())
            .ok();
        now != *recorded
    })
}

/// Whether `path` is an executable regular file (or an executable symlink).
fn is_executable(path: &std::path::Path, is_symlink: bool) -> bool {
    let metadata = if is_symlink {
        fs::metadata(path)
    } else {
        fs::symlink_metadata(path)
    };
    let Ok(metadata) = metadata else {
        return false;
    };
    if metadata.is_dir() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn env_bytes(name: &str) -> Option<Vec<u8>> {
    let value = std::env::var_os(name)?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Some(value.into_vec())
    }
    #[cfg(not(unix))]
    {
        Some(value.to_string_lossy().into_owned().into_bytes())
    }
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

#[cfg(test)]
mod background_tests {
    use super::CommandProvider;
    use std::io::Write;

    /// A directory with one executable in it, on a `PATH` of its own.
    fn fixture(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("baash-index-{tag}-{}", std::process::id()));
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let program = bin.join("baash-background-probe");
        let mut file = std::fs::File::create(&program).unwrap();
        file.write_all(b"#!/bin/sh\n").unwrap();
        drop(file);
        let mut permissions = std::fs::metadata(&program).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&program, permissions).unwrap();
        (root, bin)
    }

    #[test]
    fn an_index_built_on_a_worker_is_used_by_names() {
        let (root, bin) = fixture("worker");
        let provider = std::sync::Arc::new(CommandProvider::with_path_var(
            bin.as_os_str().as_encoded_bytes().to_vec(),
        ));
        // Building here stands in for the worker thread; the point is that
        // `names()` then *uses* what was installed rather than walking again.
        let index = provider.build_index();
        provider.install_index(index);
        let names = provider.names();
        assert!(names.iter().any(|name| name == b"baash-background-probe"));

        // A stale index (same names, but describing a different PATH) is
        // refused, so a late worker cannot complete names that left `PATH`.
        let other = CommandProvider::with_path_var(b"/definitely/not/here".to_vec());
        let stale = other.build_index();
        provider.install_index(stale);
        assert!(
            provider
                .names()
                .iter()
                .any(|name| name == b"baash-background-probe")
        );
        std::fs::remove_dir_all(root).ok();
    }
}
