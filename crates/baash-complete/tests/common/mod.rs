//! Test helpers: a temporary directory tree and a real word expander.
//!
//! The tree lives under `std::env::temp_dir()` and is removed on drop. Tests
//! never change the process's working directory, so they can run in parallel.

#![allow(dead_code)]

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// A temporary directory tree.
pub struct TempTree {
    /// The tree's root.
    pub root: PathBuf,
}

impl TempTree {
    /// Create a uniquely named tree.
    pub fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "baash-complete-{}-{tag}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create temporary tree");
        Self { root }
    }

    /// A path inside the tree.
    pub fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// Create a regular file.
    pub fn file(&self, relative: &str, contents: &[u8]) -> PathBuf {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(&path, contents).expect("write file");
        path
    }

    /// Create a directory.
    pub fn dir(&self, relative: &str) -> PathBuf {
        let path = self.path(relative);
        fs::create_dir_all(&path).expect("create directory");
        path
    }

    /// Create an executable file.
    pub fn exe(&self, relative: &str) -> PathBuf {
        let path = self.file(relative, b"#!/bin/sh\nexit 0\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        path
    }

    /// Create a file whose name is arbitrary bytes (Unix only).
    #[cfg(unix)]
    pub fn raw_file(&self, name: &[u8]) -> PathBuf {
        use std::os::unix::ffi::OsStrExt;
        let path = self.root.join(OsStr::from_bytes(name));
        fs::write(&path, b"x").expect("write raw file");
        path
    }

    /// Make a symbolic link to `target` at `relative`.
    #[cfg(unix)]
    pub fn symlink(&self, target: &Path, relative: &str) -> PathBuf {
        use std::os::unix::fs::symlink;
        let path = self.path(relative);
        symlink(target, &path).expect("symlink");
        path
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Parse `line` with the real front end and expand its first simple command to
/// argv with the real expansion pipeline.
///
/// `Passthrough` leaves pathname expansion alone (no globbing), which is what a
/// quoting round trip needs.
pub fn expand_line(line: &[u8]) -> Vec<Vec<u8>> {
    use baash_syntax::{NodeKind, ParseOpts, ParseOutcome, parse_bytes};

    let ast = match parse_bytes(line, &ParseOpts::DEFAULT) {
        ParseOutcome::Ok(ast) => ast,
        other => panic!(
            "line {:?} did not parse: {other:?}",
            String::from_utf8_lossy(line)
        ),
    };
    let words = ast
        .nodes
        .iter()
        .find_map(|node| match &node.kind {
            NodeKind::Simple(command) => Some(command.words.clone()),
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!(
                "line {:?} has no simple command",
                String::from_utf8_lossy(line)
            )
        });
    let mut shell = baash_lang::Passthrough::empty();
    baash_lang::expand_words(&words, &mut shell).expect("expand")
}

/// The value bytes of the only argument of `cat <arg>`.
pub fn expand_single_argument(line: &[u8]) -> Vec<u8> {
    let argv = expand_line(line);
    assert_eq!(
        argv.len(),
        2,
        "expected exactly one argument in {:?}, got {argv:?}",
        String::from_utf8_lossy(line)
    );
    argv[1].clone()
}
