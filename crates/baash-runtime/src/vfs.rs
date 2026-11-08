//! Read-only trees that live in memory and are mounted at a path in the shell.
//!
//! The shell carries data that is not a file on this machine: the
//! bash-completion library (1106 files, 2.97 MB) is compiled into the binary.
//! Writing it out and reading it back is possible and was the first design, but
//! on Windows it costs 475 ms of the first start — Defender reads every one of
//! those files — and it puts a cache directory in the user's profile that has
//! to be versioned, cleaned and trusted. A tree that is only ever read by the
//! shell itself (`source`, `test`, pathname expansion) does not need to be a
//! file at all.
//!
//! So a mount is a path prefix plus a table of `(relative path, bytes)`, and the
//! interpreter consults it before the file system for exactly the operations
//! that can name one of its paths:
//!
//! * `source`/`.` reads the bytes from the table ([`Vfs::file`]);
//! * `test`/`[`/`[[` answer file predicates from [`Vfs::kind`];
//! * pathname expansion lists [`Vfs::entries`] of a mounted directory.
//!
//! Everything else — `ls`, `cat`, an external command — still goes to the OS and
//! finds nothing there, which is the one caveat this design accepts: a mounted
//! tree is shell data, not a file system. The shell's own layers see it, a child
//! process does not.
//!
//! Mounted paths are matched as bytes after dropping a trailing `/`, and a
//! component boundary is a `/`: `/a/b` never matches `/a/bc`. There is no `.` or
//! `..` resolution inside a mount, no symlinks and no writes; a path that is not
//! in the table is simply not in the tree.

use std::borrow::Cow;

/// What a path names in a mounted tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A file in the table; [`Vfs::file`] has its bytes.
    File,
    /// A directory: the mount root, its ancestors, or a prefix of some file.
    Dir,
}

/// One tree, mounted at `root`.
#[derive(Clone, Debug)]
pub struct Mount {
    /// The path the tree hangs from, without a trailing slash.
    root: Vec<u8>,
    /// Files below `root`, each with a path relative to it.
    files: Vec<(Vec<u8>, Cow<'static, [u8]>)>,
}

/// The mounted trees, in mount order. Empty in a shell that carries none.
///
/// The tables are static data from the binary, so cloning a [`ShellState`] for
/// a subshell copies paths and slices, never file contents.
///
/// [`ShellState`]: crate::ShellState
#[derive(Clone, Debug, Default)]
pub struct Vfs {
    mounts: Vec<Mount>,
}

impl Vfs {
    /// Mount `files` (paths relative to `root`) under `root`.
    ///
    /// The last mount wins for a path two mounts both claim, which is what a
    /// shadowing mount should do and what nothing currently does.
    pub fn mount(&mut self, root: &[u8], files: Vec<(Vec<u8>, Cow<'static, [u8]>)>) {
        self.mounts.push(Mount {
            root: trim_slashes(root).to_vec(),
            files,
        });
    }

    /// Whether nothing is mounted, the common case and the cheap answer.
    pub fn is_empty(&self) -> bool {
        self.mounts.is_empty()
    }

    /// The bytes of the file at `path`, if a mount has it.
    pub fn file(&self, path: &[u8]) -> Option<&[u8]> {
        let path = trim_slashes(path);
        for mount in self.mounts.iter().rev() {
            let Some(relative) = mount.relative(path) else {
                continue;
            };
            if let Some((_, bytes)) = mount.files.iter().find(|(name, _)| name == relative) {
                return Some(bytes);
            }
        }
        None
    }

    /// What `path` names, if anything.
    pub fn kind(&self, path: &[u8]) -> Option<Kind> {
        let path = trim_slashes(path);
        for mount in self.mounts.iter().rev() {
            if mount.is_ancestor(path) {
                return Some(Kind::Dir);
            }
            let Some(relative) = mount.relative(path) else {
                continue;
            };
            if relative.is_empty() {
                // The mount root itself.
                return Some(Kind::Dir);
            }
            if mount.files.iter().any(|(name, _)| name == relative) {
                return Some(Kind::File);
            }
            // A file below `relative` makes it a directory, which is what the
            // tree's shape says and what `-d` on a path nobody listed expects.
            let mut prefix = relative.to_vec();
            prefix.push(b'/');
            if mount
                .files
                .iter()
                .any(|(name, _)| name.starts_with(&prefix))
            {
                return Some(Kind::Dir);
            }
        }
        None
    }

    /// The names directly inside the mounted directory `dir`, sorted.
    ///
    /// Empty for a path that is not a mounted directory, which is also what
    /// pathname expansion wants for an unreadable one.
    pub fn entries(&self, dir: &[u8]) -> Vec<Vec<u8>> {
        let dir = trim_slashes(dir);
        for mount in self.mounts.iter().rev() {
            if let Some(relative) = mount.relative(dir) {
                return mount
                    .children(relative)
                    .into_iter()
                    .map(<[u8]>::to_vec)
                    .collect();
            }
            if mount.is_ancestor(dir) {
                // Above the root: the single name that leads down to it, so a
                // glob that walks towards the mount finds it.
                let rest = &mount.root[dir.len()..];
                let name = rest.strip_prefix(b"/").unwrap_or(rest);
                return vec![name.to_vec()];
            }
        }
        Vec::new()
    }
}

impl Mount {
    /// `path` relative to the root, or `None` when `path` is not below it.
    fn relative<'a>(&self, path: &'a [u8]) -> Option<&'a [u8]> {
        let rest = path.strip_prefix(self.root.as_slice())?;
        if rest.is_empty() {
            Some(b"")
        } else {
            rest.strip_prefix(b"/")
        }
    }

    /// The names directly below `relative` (which is `""` for the root).
    fn children(&self, relative: &[u8]) -> Vec<&[u8]> {
        let mut names: Vec<&[u8]> = Vec::new();
        for (name, _) in &self.files {
            let tail = if relative.is_empty() {
                name.as_slice()
            } else {
                let mut prefix = relative.to_vec();
                prefix.push(b'/');
                match name.strip_prefix(prefix.as_slice()) {
                    Some(tail) => tail,
                    None => continue,
                }
            };
            let first = tail.split(|&byte| byte == b'/').next().unwrap_or(tail);
            if !first.is_empty() && !names.contains(&first) {
                names.push(first);
            }
        }
        names.sort();
        names
    }

    /// Whether `path` is above the root (`/a` for a mount at `/a/b`).
    fn is_ancestor(&self, path: &[u8]) -> bool {
        if path.len() >= self.root.len() {
            return false;
        }
        self.root.starts_with(path) && self.root.get(path.len()) == Some(&b'/')
    }
}

/// Drop trailing slashes, keeping the root slash itself.
fn trim_slashes(path: &[u8]) -> &[u8] {
    let mut end = path.len();
    while end > 1 && path.get(end - 1) == Some(&b'/') {
        end -= 1;
    }
    &path[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> Vfs {
        let mut vfs = Vfs::default();
        vfs.mount(
            b"/__baash__/bash-completion",
            vec![
                (b"bash_completion".to_vec(), Cow::Borrowed(b"framework")),
                (b"completions-core/tar.bash".to_vec(), Cow::Borrowed(b"tar")),
                (b"completions-core/jq.bash".to_vec(), Cow::Borrowed(b"jq")),
                (
                    b"startup-core/000_compat.bash".to_vec(),
                    Cow::Borrowed(b"c"),
                ),
                (b"PROVENANCE".to_vec(), Cow::Borrowed(b"store")),
            ],
        );
        vfs
    }

    #[test]
    fn a_file_in_the_tree_is_read_from_memory() {
        let vfs = tree();
        assert_eq!(
            vfs.file(b"/__baash__/bash-completion/bash_completion"),
            Some(&b"framework"[..])
        );
        assert_eq!(
            vfs.file(b"/__baash__/bash-completion/completions-core/tar.bash/"),
            Some(&b"tar"[..]),
            "a trailing slash names the same file in a shell"
        );
        assert_eq!(vfs.file(b"/__baash__/bash-completion/nope"), None);
        assert_eq!(
            vfs.file(b"/__baash__/bash-completion/completions-core"),
            None
        );
    }

    #[test]
    fn the_shape_of_the_tree_answers_the_file_predicates() {
        let vfs = tree();
        assert_eq!(vfs.kind(b"/__baash__/bash-completion"), Some(Kind::Dir));
        assert_eq!(
            vfs.kind(b"/__baash__/bash-completion/completions-core"),
            Some(Kind::Dir)
        );
        assert_eq!(
            vfs.kind(b"/__baash__/bash-completion/completions-core/tar.bash"),
            Some(Kind::File)
        );
        // The mount's own ancestors are directories: a script that walks up
        // (`dirname`, `..`) must not fall off the tree at the first step.
        assert_eq!(vfs.kind(b"/__baash__"), Some(Kind::Dir));
        assert_eq!(
            vfs.kind(b"/__baash__/bash-completion/startup-core/nope"),
            None
        );
        assert_eq!(vfs.kind(b"/__baashx"), None, "a prefix is not a parent");
    }

    #[test]
    fn a_directory_lists_its_children_and_not_the_whole_tree() {
        let vfs = tree();
        assert_eq!(
            vfs.entries(b"/__baash__/bash-completion"),
            vec![
                b"PROVENANCE".to_vec(),
                b"bash_completion".to_vec(),
                b"completions-core".to_vec(),
                b"startup-core".to_vec(),
            ]
        );
        assert_eq!(
            vfs.entries(b"/__baash__/bash-completion/completions-core"),
            vec![b"jq.bash".to_vec(), b"tar.bash".to_vec()]
        );
        assert_eq!(vfs.entries(b"/nope"), Vec::<Vec<u8>>::new());
        assert_eq!(
            vfs.entries(b"/__baash__"),
            vec![b"bash-completion".to_vec()]
        );
    }

    #[test]
    fn an_empty_vfs_answers_nothing() {
        let vfs = Vfs::default();
        assert!(vfs.is_empty());
        assert_eq!(vfs.kind(b"/"), None);
        assert_eq!(vfs.file(b"/bash_completion"), None);
        assert_eq!(vfs.entries(b"/"), Vec::<Vec<u8>>::new());
    }
}
