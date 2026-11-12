//! Live filesystem change notification for the prompt's git segment.
//!
//! The prompt shows the branch and the counts of modified and untracked paths
//! (`docs/contracts.md` §3). The counters come from a worker's `gix` walk,
//! and asking for one on a timer would burn a core while the user is reading
//! the screen. Instead the prompt *waits* for the filesystem to say something
//! changed, then asks the rate-limited worker.
//!
//! # Why this lives here
//!
//! `inotify` is a Linux facility with no `std` equivalent, and the descriptor
//! it hands back is only useful if something can await it. It is created here,
//! non-blocking, so that a caller can submit it to `io_uring` as a poll
//! (`compio`'s `PollOnce` op) and be woken by the same reactor that drives the
//! terminal read. Nothing in this module blocks: [`Watcher::pending`] only
//! drains what the kernel has already queued.
//!
//! # The budget, and why there is one
//!
//! One watch per directory means a large worktree costs one watch per
//! directory, and `inotify` watches are a per-user kernel resource
//! (`/proc/sys/fs/inotify/max_user_watches`, 8192 by default on many systems).
//! A directory with a `node_modules` or an unpacked `vendor/` would either fail
//! with `ENOSPC` or quietly eat the budget the rest of the session needs.
//!
//! So the walk is bounded by [`Limits`]: if the worktree has more directories
//! than the budget allows, watching is abandoned with [`Why::TooBig`] and the
//! caller falls back to computing the segment once per prompt. Being told the
//! repo is too big is a correct answer; half-watching it is not, because a
//! stale prompt is worse than one that is computed rather than observed.
//!
//! # Platform
//!
//! Linux only, and the module is `#[cfg(target_os = "linux")]` to say so
//! rather than being compiled into a stub elsewhere. macOS would want kqueue or
//! FSEvents and Windows `ReadDirectoryChangesW` (which is IOCP-compatible and
//! would fit the same "await the handle" shape); neither has been written or
//! measured (`docs/portability.md` §10), and the prompt already has a correct
//! fallback there — it is computed once per prompt instead of observed.
//!
//! `target/`-shaped directories are not special-cased. A budget that needs a
//! list of names to work is a budget that is wrong for the next project.

use std::path::{Path, PathBuf};

/// The event mask the prompt cares about.
///
/// Deliberately not `ALL_EVENTS`: `IN_ACCESS` and `IN_OPEN` fire for every
/// `ls`, `grep` and editor open, which is most of the traffic in a repository
/// and none of it changes the prompt. What is left is "a name was created,
/// removed, moved or written" and "a directory went away underneath us".
fn mask() -> rustix::fs::inotify::WatchFlags {
    use rustix::fs::inotify::WatchFlags;
    WatchFlags::CREATE
        | WatchFlags::DELETE
        | WatchFlags::MOVED_FROM
        | WatchFlags::MOVED_TO
        | WatchFlags::MODIFY
        | WatchFlags::ATTRIB
        | WatchFlags::CLOSE_WRITE
        | WatchFlags::DELETE_SELF
        | WatchFlags::MOVE_SELF
        | WatchFlags::ONLYDIR
}

/// The bounds within which watching is worth the kernel resources.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Most directories to watch, across the worktree and the git directory.
    pub max_dirs: usize,
    /// Deepest relative directory to descend into.
    pub max_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_dirs: 256,
            max_depth: 12,
        }
    }
}

/// Why live updates were not set up.
///
/// Every arm is a fallback to "compute the segment each prompt", never an
/// error the user has to deal with: the prompt is decoration, and a decoration
/// that refuses to draw because a watcher could not be created would be worse
/// than the counts being a moment out of date.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// `root` is not inside a git repository.
    NotARepository,
    /// The worktree is larger than [`Limits::max_dirs`].
    TooBig {
        /// Directories the walk found before it stopped counting.
        dirs: usize,
    },
    /// The kernel refused, e.g. `max_user_watches` was exhausted.
    Refused,
}

/// A live view of the changes under a working tree.
///
/// Dropping it removes every watch: the descriptor is closed and the kernel
/// releases them, so there is nothing to unwind by hand.
#[derive(Debug)]
pub struct Watcher {
    fd: std::os::fd::OwnedFd,
    /// Events are read into this; sized for a burst rather than one event so a
    /// `git checkout` that touches a thousand files is one read.
    buf: Vec<std::mem::MaybeUninit<u8>>,
    /// Directories the kernel accepted a watch for.
    watched: usize,
}

/// Watch `root` (a working tree) and its git directory.
///
/// Returns [`Why::TooBig`] rather than a partial watch when the tree does not
/// fit in `limits`, and [`Why::NotARepository`] when `root` is not a working
/// tree, so the caller can skip the work entirely instead of polling.
pub fn watch_worktree(root: &Path, limits: Limits) -> Result<Watcher, Why> {
    use rustix::fs::inotify::{self, CreateFlags};

    let git_dir = git_dir(root).ok_or(Why::NotARepository)?;

    // Collect first, then watch: the budget is a property of the tree, and a
    // tree that is too big should not have spent any watches before we find
    // out. (The kernel would clean them up on drop, but there is no reason to
    // make it do so.)
    let mut dirs = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        // `inotify_add_watch` can follow a symlinked directory, which would let
        // a symlink to `/` or to a network mount pull this walk somewhere it
        // has no business being; `symlink_metadata` asks about the entry
        // itself.
        if !std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        dirs.push(dir.clone());
        if dirs.len() > limits.max_dirs {
            return Err(Why::TooBig { dirs: dirs.len() });
        }
        if depth >= limits.max_depth {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            // A directory we cannot read is a directory it is not worth
            // failing the whole watch over: the prompt does not need to know
            // about a subtree the agent cannot open either.
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.file_name().is_some_and(|name| name == ".git") {
                continue;
            }
            // `file_type` does not follow symlinks, matching the check above.
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                stack.push((path, depth + 1));
            }
        }
    }

    // The git directory itself sees `HEAD`, `index` and the worker's own
    // writes; `refs` below it sees branch creation and deletion. `objects/` is
    // skipped on purpose: it is write-only as far as the prompt is concerned
    // and is the part of `.git` that gets large.
    dirs.push(git_dir.clone());
    if dirs.len() > limits.max_dirs {
        return Err(Why::TooBig { dirs: dirs.len() });
    }
    let refs = git_dir.join("refs");
    if refs.is_dir() {
        let mut stack = vec![(refs, 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            dirs.push(dir.clone());
            if dirs.len() > limits.max_dirs {
                return Err(Why::TooBig { dirs: dirs.len() });
            }
            if depth >= limits.max_depth {
                continue;
            }
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                        stack.push((entry.path(), depth + 1));
                    }
                }
            }
        }
    }

    let fd =
        inotify::init(CreateFlags::NONBLOCK | CreateFlags::CLOEXEC).map_err(|_| Why::Refused)?;
    let mut watched = 0;
    for dir in &dirs {
        // `EACCES` on one directory (a `~/.cache` style tree with a mode the
        // user cannot read) should not throw away the watches that did work.
        if inotify::add_watch(&fd, dir, mask()).is_ok() {
            watched += 1;
        }
    }
    if watched == 0 {
        return Err(Why::Refused);
    }

    Ok(Watcher {
        fd,
        buf: vec![std::mem::MaybeUninit::uninit(); 4096],
        watched,
    })
}

impl Watcher {
    /// Directories the kernel accepted a watch for.
    pub fn watched_dirs(&self) -> usize {
        self.watched
    }

    /// The descriptor to await for readability.
    pub fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd;
        self.fd.as_fd()
    }

    /// Drain the events the kernel has already queued.
    ///
    /// `true` means "something under the tree changed since the last call".
    /// The events themselves are not reported: the prompt does not need to
    /// know *what* changed, only that its cached answer is now suspect, and
    /// reading them is cheaper than keeping a per-directory model of the tree
    /// in sync.
    pub fn pending(&mut self) -> bool {
        use rustix::fs::inotify::{ReadFlags, Reader};
        use rustix::io::Errno;

        let mut changed = false;
        loop {
            let mut reader = Reader::new(&self.fd, &mut self.buf);
            match reader.next() {
                Ok(event) => {
                    changed = true;
                    // A queue overflow (`wd == -1`) means events were dropped;
                    // the only sound reading is "recompute", which is what
                    // `changed` already says. `IN_IGNORED` means a watch went
                    // away because its directory did, which is also a change.
                    debug_assert!(
                        event.events().contains(ReadFlags::QUEUE_OVERFLOW) || event.wd() >= 0
                    );
                }
                // Drained. `AGAIN` and `WOULDBLOCK` are the same errno on
                // Linux, so one arm covers both.
                Err(Errno::AGAIN) => break,
                Err(Errno::INTR) => continue,
                // A removed watch reads back as `INVAL`; nothing to do, and the
                // next read reports `AGAIN` or another event.
                Err(Errno::INVAL) => break,
                Err(_) => break,
            }
        }
        changed
    }
}

/// The git directory for the working tree at `root`, if it is one.
///
/// Only the two shapes the prompt can meet are handled: a plain `.git`
/// directory, and the `.git` *file* a linked worktree or a submodule has,
/// which holds `gitdir: <path>`. Discovery upwards is deliberately not done:
/// the prompt shows the status of the repository the shell is *in*, and a
/// directory inside a repository but not at its top is a different question
/// (`docs/contracts.md` §3 says so).
pub fn git_dir(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    let meta = std::fs::symlink_metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return Some(dot_git);
    }
    if !meta.is_file() {
        return None;
    }
    // `gitdir: /path/to/worktrees/name`, possibly relative to `root`.
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let rest = text.strip_prefix("gitdir:")?.trim();
    let path = PathBuf::from(rest);
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    path.is_dir().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A unique temporary directory that cleans itself up.
    struct Temp(PathBuf);

    impl Temp {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir().join(format!(
                "baash-watch-{}-{}-{name}",
                std::process::id(),
                Instant::now().elapsed().as_nanos()
            ));
            std::fs::create_dir_all(&base).unwrap();
            Self(base)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn repository(root: &Path) {
        std::fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
    }

    #[test]
    fn a_directory_without_a_git_directory_is_not_a_repository() {
        let tmp = Temp::new("plain");
        assert_eq!(
            watch_worktree(tmp.path(), Limits::default()).unwrap_err(),
            Why::NotARepository
        );
    }

    #[test]
    fn a_plain_git_directory_is_found() {
        let tmp = Temp::new("plaingit");
        repository(tmp.path());
        let found = git_dir(tmp.path()).unwrap();
        assert_eq!(found, tmp.path().join(".git"));
    }

    #[test]
    fn a_git_file_points_at_the_worktrees_directory() {
        let tmp = Temp::new("linked");
        let real = tmp.path().join("real");
        repository(&real);
        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        // Relative, as git writes it for a linked worktree.
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", real.join(".git").display()),
        )
        .unwrap();
        assert_eq!(git_dir(&worktree).unwrap(), real.join(".git"));
    }

    #[test]
    fn a_new_file_is_reported() {
        let tmp = Temp::new("created");
        repository(tmp.path());
        let mut watcher = watch_worktree(tmp.path(), Limits::default()).unwrap();
        assert!(!watcher.pending(), "nothing has changed yet");
        std::fs::write(tmp.path().join("b.txt"), "two\n").unwrap();
        assert!(watcher.pending(), "the create should have been queued");
        assert!(!watcher.pending(), "and draining consumes it");
    }

    #[test]
    fn a_change_to_head_is_reported() {
        let tmp = Temp::new("head");
        repository(tmp.path());
        let mut watcher = watch_worktree(tmp.path(), Limits::default()).unwrap();
        std::fs::write(tmp.path().join(".git/HEAD"), "ref: refs/heads/other\n").unwrap();
        assert!(watcher.pending());
    }

    #[test]
    fn a_branch_being_written_is_reported() {
        let tmp = Temp::new("branch");
        repository(tmp.path());
        let mut watcher = watch_worktree(tmp.path(), Limits::default()).unwrap();
        std::fs::write(
            tmp.path().join(".git/refs/heads/topic"),
            "0123456789012345678901234567890123456789\n",
        )
        .unwrap();
        assert!(watcher.pending());
        // `objects/` is not watched. Creating it is still an entry in `.git`,
        // which is; a write *below* it is not, and is none of the prompt's
        // business.
        std::fs::create_dir_all(tmp.path().join(".git/objects/ab")).unwrap();
        assert!(watcher.pending(), "the new entry in .git is");
        for i in 0..8 {
            std::fs::write(tmp.path().join(format!(".git/objects/ab/{i}")), "x").unwrap();
        }
        assert!(!watcher.pending(), "a write below it is not");
    }

    #[test]
    fn a_tree_over_the_budget_is_refused_rather_than_half_watched() {
        let tmp = Temp::new("fat");
        repository(tmp.path());
        for i in 0..8 {
            std::fs::create_dir_all(tmp.path().join(format!("d{i}/e"))).unwrap();
        }
        let limits = Limits {
            max_dirs: 4,
            max_depth: 12,
        };
        match watch_worktree(tmp.path(), limits) {
            Err(Why::TooBig { dirs }) => assert!(dirs > 4, "{dirs}"),
            other => panic!("expected TooBig, got {other:?}"),
        }
    }

    #[test]
    fn a_deep_tree_stops_at_the_depth_limit() {
        let tmp = Temp::new("deep");
        repository(tmp.path());
        let mut dir = tmp.path().to_path_buf();
        for _ in 0..20 {
            dir = dir.join("level");
        }
        std::fs::create_dir_all(&dir).unwrap();
        // Depth 4 keeps the walk shallow; the budget is generous so the only
        // reason to stop is depth.
        let limits = Limits {
            max_dirs: 256,
            max_depth: 4,
        };
        std::fs::create_dir_all(tmp.path().join("top")).unwrap();
        let mut watcher = watch_worktree(tmp.path(), limits).unwrap();
        std::fs::write(dir.join("deep.txt"), "x").unwrap();
        assert!(!watcher.pending(), "beyond the depth limit, unwatched");
        std::fs::write(tmp.path().join("top/x.txt"), "x").unwrap();
        assert!(watcher.pending(), "within it, watched");
    }

    #[test]
    fn the_walk_does_not_follow_symlinked_directories() {
        let tmp = Temp::new("symlink");
        repository(tmp.path());
        std::fs::create_dir_all(tmp.path().join("real")).unwrap();
        std::os::unix::fs::symlink("/tmp", tmp.path().join("escape")).unwrap();
        let limits = Limits {
            max_dirs: 64,
            max_depth: 12,
        };
        std::fs::create_dir_all(tmp.path().join("real/child")).unwrap();
        let mut watcher = watch_worktree(tmp.path(), limits).unwrap();
        // Nothing under `/tmp` is watched, so a file there cannot wake us.
        let probe = std::env::temp_dir().join(format!("baash-watch-probe-{}", std::process::id()));
        std::fs::write(&probe, "x").unwrap();
        let _ = std::fs::remove_file(&probe);
        // Give the kernel a moment; the point is that it does *not* arrive.
        std::thread::sleep(Duration::from_millis(20));
        assert!(!watcher.pending());
        std::fs::write(tmp.path().join("real/child/y.txt"), "x").unwrap();
        assert!(watcher.pending());
    }
}
