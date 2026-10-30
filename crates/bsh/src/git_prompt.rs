//! The live git segment in the prompt.
//!
//! Two cost classes, deliberately kept apart (`docs/prompt-watch.md` §2):
//!
//! * The **branch** is read from `.git/HEAD` on the prompt path. That is one
//!   small file read — microseconds — so it is exact and never stale.
//! * The **dirty marker** needs `git status`, which is a subprocess that can
//!   take hundreds of milliseconds on a cold cache. It never runs on the prompt
//!   path: a worker spawns it through compio's process API, and the prompt
//!   renders whatever the last completed refresh produced.
//!
//! Refreshes are rate-limited ([`MIN_INTERVAL`]) and coalesced: the worker
//! drains every queued request down to the newest one, so a burst of prompts
//! cannot queue a burst of `git status` runs. The shell knows nothing about
//! this — `PS1`, `PROMPT_COMMAND` and the segment composer see the same
//! rendered prompt as before, and the segment is appended by the REPL.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

/// The shortest gap between two `git status` runs.
const MIN_INTERVAL: Duration = Duration::from_millis(500);

/// What the prompt currently knows about the repository.
#[derive(Debug, Default)]
struct State {
    /// The branch name, when `HEAD` names one; `None` for a detached `HEAD` or
    /// no repository at all.
    branch: Option<Vec<u8>>,
    /// Modified paths from the newest completed `git status`, or `None` when
    /// no refresh has finished yet.
    dirty: Option<usize>,
}

/// A finished refresh.
struct Update {
    /// The directory it described. A result for a previous directory must not
    /// be shown as if it described the current one.
    cwd: PathBuf,
    dirty: Option<usize>,
}

/// The prompt's git segment.
pub struct GitSegment {
    request: Sender<PathBuf>,
    updates: Receiver<Update>,
    last_request: Option<Instant>,
    state: State,
    rendered: Option<Vec<u8>>,
    /// The directory `state` describes.
    cwd: Option<PathBuf>,
}

impl GitSegment {
    /// Start the worker and return an empty segment.
    ///
    /// A worker that cannot start (no threads, no runtime) is not fatal: the
    /// segment simply stays empty and the prompt is unaffected.
    pub fn new() -> Self {
        let (request, requests) = mpsc::channel::<PathBuf>();
        let (sender, updates) = mpsc::channel::<Update>();
        std::thread::Builder::new()
            .name("bsh-git-prompt".to_owned())
            .spawn(move || worker(requests, sender))
            .ok();
        Self {
            request,
            updates,
            last_request: None,
            state: State::default(),
            rendered: None,
            cwd: None,
        }
    }

    /// Bring the segment up to date for `cwd`, and schedule a refresh if due.
    ///
    /// Called once per prompt. Reads `.git/HEAD` (cheap, exact), installs any
    /// finished `git status`, and asks the worker for a new one at most every
    /// [`MIN_INTERVAL`].
    pub fn tick(&mut self, cwd: &Path) {
        // A finished refresh describes the directory it was asked about.
        while let Ok(update) = self.updates.try_recv() {
            if self.cwd.as_deref() == Some(update.cwd.as_path()) {
                self.state.dirty = update.dirty;
                self.rendered = None;
            }
        }

        if self.cwd.as_deref() != Some(cwd) {
            self.cwd = Some(cwd.to_path_buf());
            // A new directory: the old dirty count says nothing about this one.
            self.state.dirty = None;
            self.rendered = None;
            self.last_request = None;
        }

        let branch = read_branch(cwd);
        if branch != self.state.branch {
            self.state.branch = branch;
            self.rendered = None;
        }

        if self.state.branch.is_none() {
            return;
        }
        let due = self
            .last_request
            .is_none_or(|last| last.elapsed() >= MIN_INTERVAL);
        if due {
            self.last_request = Some(Instant::now());
            let _ = self.request.send(cwd.to_path_buf());
        }
    }

    /// The bytes to append to the prompt, or `None` outside a repository.
    ///
    /// Shape is ` [branch]` or ` [branch ~3]`; ahead/behind is not computed yet
    /// (`docs/prompt-watch.md` §2 leaves it as a later refinement).
    pub fn render(&mut self) -> Option<&[u8]> {
        if self.rendered.is_none() {
            let branch = self.state.branch.as_ref()?;
            let mut text = Vec::with_capacity(branch.len() + 16);
            text.extend_from_slice(b" [");
            text.extend_from_slice(branch);
            if let Some(dirty) = self.state.dirty {
                if dirty > 0 {
                    text.extend_from_slice(format!(" ~{dirty}").as_bytes());
                }
            }
            text.push(b']');
            self.rendered = Some(text);
        }
        self.rendered.as_deref()
    }
}

impl Default for GitSegment {
    fn default() -> Self {
        Self::new()
    }
}

/// The branch `cwd`'s repository has checked out.
///
/// `None` for a detached `HEAD`, a missing `.git`, or a `.git` that is a file
/// (a worktree or a submodule): the segment then shows nothing rather than a
/// guess.
fn read_branch(cwd: &Path) -> Option<Vec<u8>> {
    let head = std::fs::read(cwd.join(".git/HEAD")).ok()?;
    let head = head.strip_suffix(b"\n").unwrap_or(&head);
    let name = head.strip_prefix(b"ref: refs/heads/")?;
    (!name.is_empty()).then(|| name.to_vec())
}

/// Count the entries `git status --porcelain=v2 -z` reported.
///
/// Headers start with `#`; every other NUL-terminated record is a changed
/// path or a rename pair's first half, which is one change either way.
fn count_changes(stdout: &[u8]) -> usize {
    stdout
        .split(|&byte| byte == 0)
        .filter(|record| !record.is_empty() && !record.starts_with(b"#"))
        .count()
}

/// Ask git for the dirty count of one directory, off the prompt path.
fn worker(requests: Receiver<PathBuf>, updates: Sender<Update>) {
    let Ok(runtime) = compio::runtime::Runtime::new() else {
        return;
    };
    while let Ok(first) = requests.recv() {
        // Coalesce: a queued older directory is worthless, since only the
        // newest prompt is on screen.
        let mut cwd = first;
        while let Ok(newer) = requests.try_recv() {
            cwd = newer;
        }
        let dirty = runtime.block_on(status(&cwd));
        if updates.send(Update { cwd, dirty }).is_err() {
            return;
        }
    }
}

/// Run `git status` for `cwd` and return the number of changed paths.
async fn status(cwd: &Path) -> Option<usize> {
    let mut command = compio::process::Command::new("git");
    command
        .arg("--no-optional-locks")
        .arg("status")
        .arg("--porcelain=v2")
        .arg("-z")
        .current_dir(cwd)
        // Byte-exact output, no lock contention, and never a credential prompt:
        // this runs while the user is typing (`docs/prompt-watch.md` §2).
        .env("LC_ALL", "C")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdout(std::process::Stdio::piped())
        .ok()?
        .stderr(std::process::Stdio::null())
        .ok()?;
    let output = command.output().await.ok()?;
    output
        .status
        .success()
        .then(|| count_changes(&output.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_is_read_from_head() {
        let root = std::env::temp_dir().join(format!("bsh-git-{}", std::process::id()));
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        assert_eq!(read_branch(&root), Some(b"main".to_vec()));

        // Detached HEAD is not a branch, and neither is a missing file.
        std::fs::write(
            root.join(".git/HEAD"),
            b"2f4a9c1d0e5b6a7c8d9e0f1a2b3c4d5e6f708192\n",
        )
        .unwrap();
        assert_eq!(read_branch(&root), None);
        assert_eq!(read_branch(&root.join("nowhere")), None);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn changes_are_counted_without_the_headers() {
        let porcelain = concat!(
            "# branch.oid abc\0",
            "# branch.head main\0",
            "1 .M N... 100644 100644 100644 a b file\0",
            "? untracked\0"
        );
        assert_eq!(count_changes(porcelain.as_bytes()), 2);
        assert_eq!(count_changes(b"# branch.head main\0"), 0);
        assert_eq!(count_changes(b""), 0);
    }

    #[test]
    fn the_segment_renders_only_with_a_branch() {
        let mut segment = GitSegment {
            request: mpsc::channel().0,
            updates: mpsc::channel().1,
            last_request: None,
            state: State::default(),
            rendered: None,
            cwd: None,
        };
        assert_eq!(segment.render(), None);
        segment.state.branch = Some(b"main".to_vec());
        assert_eq!(segment.render(), Some(&b" [main]"[..]));
        segment.rendered = None;
        segment.state.dirty = Some(3);
        assert_eq!(segment.render(), Some(&b" [main ~3]"[..]));
    }
}
