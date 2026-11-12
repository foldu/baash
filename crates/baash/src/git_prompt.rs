//! The live git facts in the stock prompt.
//!
//! Two cost classes, deliberately kept apart (`docs/prompt-watch.md` §2):
//!
//! * The **branch** is read from `.git/HEAD` on the prompt path, by
//!   [`baash_lang::prompt::git_branch`]. That is a few small file reads and
//!   `stat`s — microseconds — so it is exact and never stale, and a detached
//!   `HEAD` is resolved to its abbreviated commit id at the same cost.
//! * The **counters** (ahead, behind, modified, untracked) need a status, which
//!   walks the index and the worktree. That never runs on the prompt path: it
//!   is a task on the shell's own `compio` runtime, a blocking walk on the
//!   runtime's blocking pool, and the prompt renders whatever the last
//!   completed refresh produced.
//!
//! It is deliberately **not** `git status`: the shell must not depend on a `git`
//! binary to show the counters. `gix` reads the index, honours `.gitignore` and
//! runs the directory walk itself. Ahead/behind come from the commit graph:
//! commits reachable from `HEAD` but not from its upstream tracking ref (and
//! the reverse), which is exactly `git rev-list --count HEAD ^upstream`.
//!
//! A refresh runs under a deadline, and the deadline is a runtime timer:
//! [`compio::time::timeout`] races the blocking walk, `gix`'s status takes the
//! shared interrupt flag as a second line of defence, and a late result is
//! dropped rather than painted. Dropping it does not cancel the walk —
//! `spawn_blocking` runs to completion by contract — but nothing waits on it,
//! and one that is stuck cannot delay the prompt.
//!
//! Refreshes are rate-limited ([`MIN_INTERVAL`]) and never overlap: there is
//! one refresh in flight at a time, so a burst of prompts cannot queue a burst
//! of status runs. A refresh that produced nothing (no repository, a corrupt
//! one, a timeout) is silent and backs off to [`RETRY_AFTER_FAILURE`].
//!
//! The prompt never waits for any of this: [`GitSegment::tick`] reads the
//! branch and *schedules* a refresh, and [`GitSegment::facts`] returns the last
//! completed counters for the current directory. A directory change drops the
//! counters immediately rather than showing another repository's numbers; they
//! reappear when the first refresh for the new directory lands.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use baash_lang::prompt::GitStatus;

/// The shortest gap between two status runs.
const MIN_INTERVAL: Duration = Duration::from_millis(500);

/// How long a single refresh may run before its result is discarded.
///
/// This is a hard deadline in front of the *user*, not a target: a cold
/// monorepo may legitimately take longer, in which case the prompt simply keeps
/// showing the previous completed numbers.
const STATUS_TIMEOUT: Duration = Duration::from_secs(2);

/// How long to wait after a refresh that produced nothing.
///
/// A directory that is not a repository, a corrupt repository, or a status that
/// timed out would otherwise retry once per prompt. The prompt stays silent
/// either way; this only stops the worker from repeating a doomed or expensive
/// run every half second.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(5);

/// The facts the prompt paints.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    /// The branch, or a detached `HEAD`'s abbreviated commit id. `None` when
    /// the directory is not in a repository.
    pub branch: Option<Vec<u8>>,
    /// The newest completed status for this directory, or `None` while the
    /// first refresh is still in flight (or when there is no repository).
    pub status: Option<GitStatus>,
}

/// The state shared by the prompt path and the refresh task.
#[derive(Default)]
struct State {
    /// The branch of [`State::cwd`]; `None` outside a repository.
    branch: Option<Vec<u8>>,
    /// The newest completed status for [`State::cwd`].
    status: Option<GitStatus>,
    /// The directory `branch` and `status` describe.
    cwd: Option<PathBuf>,
    /// Whether a refresh task is running. One at a time: a second would walk
    /// the same tree twice and the newer result is not more correct.
    in_flight: bool,
    /// When the next refresh may be started.
    next_attempt: Option<Instant>,
    /// How many times the prompt asked the cache for facts. Test-only: it is
    /// what proves a prompt ticks the cache even when the directory did not
    /// change, which is the only path a platform without a watcher has.
    #[cfg(test)]
    ticks: u64,
}

/// The prompt's git facts, owned by the prompt state and updated with the
/// prompt.
///
/// Cloning it is cheap and shares the same cache, so the line editor's live
/// repainter and the per-prompt composer see exactly the same facts without a
/// second refresh.
#[derive(Clone)]
pub struct GitSegment {
    state: Arc<Mutex<State>>,
}

impl GitSegment {
    /// An empty segment. Nothing runs until the first [`GitSegment::tick`] on
    /// a thread with a runtime.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bring the segment up to date for `cwd`, and start a refresh if due.
    ///
    /// Called once per prompt, and again whenever the editor is woken while
    /// the line is idle. Reads `.git/HEAD` (cheap, exact) on this thread and
    /// schedules the walk on the runtime, no more often than
    /// [`MIN_INTERVAL`].
    ///
    /// Without a current `compio` runtime — a unit test, an embedding — there
    /// is nothing to schedule on, so the branch is still exact and the counters
    /// simply stay absent. The prompt path never blocks either way.
    pub fn tick(&self, cwd: &Path) {
        let mut state = self.lock();
        #[cfg(test)]
        {
            state.ticks += 1;
        }

        if state.cwd.as_deref() != Some(cwd) {
            state.cwd = Some(cwd.to_path_buf());
            // A new directory: the old counters say nothing about this one, and
            // the first refresh is due immediately.
            state.status = None;
            state.next_attempt = None;
        }

        let bytes = baash_sys::os::bytes_from_os(cwd.as_os_str());
        state.branch = baash_lang::prompt::git_branch(&bytes);
        if state.branch.is_none() {
            // Not a repository: no counters to ask for, and a walk here would
            // only fail.
            return;
        }
        if state.in_flight {
            return;
        }
        let now = Instant::now();
        if state.next_attempt.is_some_and(|at| now < at) {
            return;
        }
        // `try_current` rather than `current`: the prompt is composed in tests
        // and in code that never started the shell's runtime loop, and neither
        // may panic because a repository happened to be in the way.
        let Some(runtime) = compio::runtime::Runtime::try_current() else {
            return;
        };
        state.in_flight = true;
        state.next_attempt = Some(now + MIN_INTERVAL);

        // The task owns nothing from the prompt: it writes into the same cache
        // and the next `tick` or `facts` sees the result. `detach` because
        // dropping a `JoinHandle` *cancels* the task in compio, and this one is
        // deliberately fire-and-forget: the deadline inside it, not the prompt,
        // decides when to stop.
        let shared = Arc::clone(&self.state);
        let directory = cwd.to_path_buf();
        runtime
            .spawn(async move {
                let status = refresh_with_deadline(&directory).await;
                let mut state = lock(&shared);
                // A result for a directory the prompt has already left is not
                // installed, and its failure does not back off the directory
                // the user is in now: that one has not been tried yet.
                if state.cwd.as_deref() == Some(directory.as_path()) {
                    state.status = status;
                    if status.is_none() {
                        state.next_attempt = Some(Instant::now() + RETRY_AFTER_FAILURE);
                    }
                }
                state.in_flight = false;
            })
            .detach();
    }

    /// The facts to paint, cloned out of the cache.
    pub fn facts(&self) -> Facts {
        let state = self.lock();
        Facts {
            branch: state.branch.clone(),
            status: state.status,
        }
    }

    /// The legacy `[branch ~N]` suffix for a user-defined `PS1`.
    ///
    /// The stock prompt uses [`GitSegment::facts`] instead. This spelling is
    /// kept because a `PS1` set by the user still gets the old bracketed
    /// segment (`docs/contracts.md` §3), and its `~N` is the sum of modified and
    /// untracked paths — the two numbers the stock prompt shows separately.
    pub fn legacy(&self) -> Option<Vec<u8>> {
        let state = self.lock();
        let branch = state.branch.as_deref()?;
        let mut text = Vec::with_capacity(branch.len() + 16);
        text.extend_from_slice(b" [");
        text.extend_from_slice(branch);
        if let Some(status) = state.status {
            let dirty = status.modified.saturating_add(status.untracked);
            if dirty > 0 {
                text.extend_from_slice(format!(" ~{dirty}").as_bytes());
            }
        }
        text.push(b']');
        Some(text)
    }

    /// How many times [`GitSegment::tick`] ran. Test-only.
    #[cfg(test)]
    pub fn ticks(&self) -> u64 {
        self.lock().ticks
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }
}

/// A poisoned lock is recovered rather than propagated.
///
/// The mutex only guards plain data; a panic while holding it would be a bug in
/// this module, and poisoning is then no reason to take the whole shell down
/// with the prompt.
fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|poison| poison.into_inner())
}

impl Default for GitSegment {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
        }
    }
}

/// Compute the status of one directory, on the runtime, under the deadline.
///
/// The walk is blocking — `gix` reads the index and the worktree with ordinary
/// file I/O — so it runs on the runtime's blocking pool rather than on an
/// executor thread. `timeout` is the runtime's own timer: no timer thread, no
/// `AtomicBool` handshake, and a late result is dropped by dropping the join
/// handle. The walk itself keeps running to completion (`spawn_blocking` is not
/// cancellable by contract), which costs one pool thread until it finishes and
/// nothing in front of the cursor.
async fn refresh_with_deadline(directory: &Path) -> Option<GitStatus> {
    let directory = directory.to_path_buf();
    // The walk's own stop button: `gix` asks it between chunks, so a walk that
    // the deadline gave up on stops soon after rather than finishing a tree the
    // prompt no longer wants.
    let interrupt = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&interrupt);
    let walk = compio::runtime::spawn_blocking(move || refresh(&directory, &flag));
    match compio::time::timeout(STATUS_TIMEOUT, walk).await {
        // `spawn_blocking` answers a join error if the walk panicked; a panic
        // in the prompt's cache is not worth taking the shell down for.
        Ok(Ok(status)) => status,
        Ok(Err(_panicked)) => None,
        Err(_elapsed) => {
            interrupt.store(true, Ordering::Release);
            None
        }
    }
}

/// Open the repository containing `cwd` and compute what the prompt paints.
fn refresh(cwd: &Path, interrupt: &Arc<AtomicBool>) -> Option<GitStatus> {
    let repo = gix::discover(cwd).ok()?;
    status_with(&repo, interrupt)
}

/// The status of `repo`, or `None` if it was interrupted or failed.
///
/// The interrupt flag is the one the deadline owns; `gix` checks it between
/// chunks of the worktree walk. The counters are *distinct paths*: a file
/// changed in the index and again in the worktree is one modified file, and a
/// rename is one file, not two.
fn status_with(repo: &gix::Repository, interrupt: &Arc<AtomicBool>) -> Option<GitStatus> {
    use std::collections::BTreeSet;

    let platform = repo
        .status(gix::progress::Discard)
        .ok()?
        // Each untracked file, not git's default "this whole new directory is
        // one entry": `?N` promises a file count.
        .untracked_files(gix::status::UntrackedFiles::Files)
        // Let the deadline stop the walk at its next chunk boundary.
        .should_interrupt_owned(Arc::clone(interrupt));

    let mut modified = BTreeSet::new();
    let mut untracked = BTreeSet::new();
    for item in platform.into_iter(Vec::new()).ok()? {
        let item = item.ok()?;
        match item {
            gix::status::Item::IndexWorktree(item) => match item {
                gix::status::index_worktree::Item::Modification { rela_path, .. } => {
                    modified.insert(rela_path);
                }
                gix::status::index_worktree::Item::Rewrite { dirwalk_entry, .. } => {
                    modified.insert(dirwalk_entry.rela_path);
                }
                gix::status::index_worktree::Item::DirectoryContents { entry, .. } => {
                    if entry.status == gix::dir::entry::Status::Untracked {
                        untracked.insert(entry.rela_path);
                    }
                }
            },
            gix::status::Item::TreeIndex(change) => {
                modified.insert(change.location().to_owned());
            }
        }
    }
    if interrupt.load(Ordering::Acquire) {
        return None;
    }

    let (ahead, behind) = ahead_behind(repo);
    if interrupt.load(Ordering::Acquire) {
        return None;
    }

    Some(GitStatus {
        modified: u32::try_from(modified.len()).unwrap_or(u32::MAX),
        untracked: u32::try_from(untracked.len()).unwrap_or(u32::MAX),
        ahead,
        behind,
    })
}

/// How many commits `HEAD` has that its upstream does not, and the reverse.
///
/// This is the same answer as `git rev-list --count HEAD ^upstream` (and the
/// mirrored form): hide every commit reachable from the other tip and count
/// what is left. No merge-base call is needed, and a diverged history is
/// handled the same way as a fast-forward. No upstream (a detached `HEAD`, a
/// branch with no `branch.<name>.remote`/`merge`, or one whose remote-tracking
/// ref does not exist yet) is `(0, 0)`, which paints no arrows.
fn ahead_behind(repo: &gix::Repository) -> (u32, u32) {
    let Ok(head) = repo.head_id() else {
        // An unborn `HEAD` (a fresh repository) has no commits to be ahead or
        // behind of.
        return (0, 0);
    };
    let Some(upstream) = upstream_id(repo) else {
        return (0, 0);
    };
    let (head, upstream) = (head.detach(), upstream.detach());
    (
        count_excluding(repo, head, upstream),
        count_excluding(repo, upstream, head),
    )
}

/// The commit `refs/remotes/<remote>/<branch>` points at for the checked-out
/// branch, when there is one.
///
/// The name is derived by `gix` from `branch.<name>.remote` and the remote's
/// fetch refspec, so a renamed remote and a non-default `refs/remotes` layout
/// both follow the repository's own configuration rather than a guess.
fn upstream_id(repo: &gix::Repository) -> Option<gix::Id<'_>> {
    let name = repo.head_name().ok()??;
    let tracking = repo
        .branch_remote_tracking_ref_name(name.as_ref(), gix::remote::Direction::Fetch)?
        .ok()?;
    let mut reference = repo.find_reference(&tracking).ok()?;
    reference.peel_to_id().ok()
}

/// Count the commits reachable from `tip` but not from `hidden`.
fn count_excluding(repo: &gix::Repository, tip: gix::ObjectId, hidden: gix::ObjectId) -> u32 {
    let Ok(walk) = repo.rev_walk([tip]).with_hidden([hidden]).all() else {
        return 0;
    };
    u32::try_from(walk.count()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("baash-git-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// Run git for test *setup* only. The shell itself never does; this is how
    /// the fixture creates commits and a remote.
    fn git(directory: &Path, arguments: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-c")
            .arg("commit.gpgsign=false")
            .args(arguments)
            .current_dir(directory)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Baash Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Baash Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .status()
            .expect("run git");
        assert!(status.success(), "git {arguments:?}");
    }

    /// A runtime for the tests that exercise the task, not just the walk.
    fn runtime() -> compio::runtime::Runtime {
        compio::runtime::Runtime::new().expect("a compio runtime")
    }

    #[test]
    fn a_refresh_that_finishes_wins_the_race() {
        let status = runtime().block_on(async {
            compio::time::timeout(
                Duration::from_secs(10),
                compio::runtime::spawn_blocking(|| {
                    Some(GitStatus {
                        modified: 3,
                        ..GitStatus::default()
                    })
                }),
            )
            .await
        });
        let status = status
            .expect("finished before the deadline")
            .expect("the walk did not panic");
        assert_eq!(status.map(|status| status.modified), Some(3));
    }

    #[test]
    fn a_refresh_that_outlives_the_deadline_is_discarded() {
        // The deadline is now the runtime's timer; what this pins is that a
        // blocking walk that is still going cannot hold the prompt, and that
        // its result is dropped rather than painted.
        let status = runtime().block_on(async {
            compio::time::timeout(
                Duration::from_millis(10),
                compio::runtime::spawn_blocking(|| {
                    std::thread::sleep(Duration::from_millis(120));
                    Some(GitStatus::default())
                }),
            )
            .await
        });
        assert!(status.is_err(), "the deadline must win over a slow walk");
    }

    #[test]
    fn a_real_repository_counts_modified_and_untracked() {
        let root = scratch("status");
        git(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("tracked.txt"), "one\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-q", "-m", "one"]);
        std::fs::write(root.join("tracked.txt"), "two\n").unwrap();
        std::fs::write(root.join("untracked.txt"), "x\n").unwrap();

        assert_eq!(
            refresh(&root, &Arc::new(AtomicBool::new(false))),
            Some(GitStatus {
                modified: 1,
                untracked: 1,
                ahead: 0,
                behind: 0,
            })
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_diverged_branch_counts_both_directions() {
        let root = scratch("diverged");
        let work = root.join("repo");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("tracked.txt"), "one\n").unwrap();
        git(&work, &["add", "tracked.txt"]);
        git(&work, &["commit", "-q", "-m", "one"]);
        git(&root, &["init", "-q", "--bare", "upstream.git"]);
        git(&work, &["remote", "add", "origin", "../upstream.git"]);
        git(&work, &["push", "-q", "-u", "origin", "main"]);

        // One commit on our side, one on the upstream's, from the same base.
        std::fs::write(work.join("tracked.txt"), "local\n").unwrap();
        git(&work, &["commit", "-q", "-am", "local"]);
        let clone = root.join("clone");
        git(
            &root,
            &["clone", "-q", "-b", "main", "upstream.git", "clone"],
        );
        std::fs::write(clone.join("upstream.txt"), "upstream\n").unwrap();
        git(&clone, &["add", "upstream.txt"]);
        git(&clone, &["commit", "-q", "-m", "upstream"]);
        git(&clone, &["push", "-q", "origin", "main"]);
        // The work tree's remote-tracking ref only moves on fetch.
        git(&work, &["fetch", "-q", "origin"]);

        let status = refresh(&work, &Arc::new(AtomicBool::new(false))).expect("a status");
        assert_eq!((status.ahead, status.behind), (1, 1));
        assert_eq!((status.modified, status.untracked), (0, 0));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_legacy_suffix_sums_modified_and_untracked() {
        let segment = GitSegment::new();
        {
            let mut state = segment.lock();
            state.branch = Some(b"main".to_vec());
            state.status = Some(GitStatus {
                modified: 2,
                untracked: 3,
                ahead: 9,
                behind: 4,
            });
        }
        assert_eq!(segment.legacy(), Some(b" [main ~5]".to_vec()));
        segment.lock().status = Some(GitStatus::default());
        assert_eq!(segment.legacy(), Some(b" [main]".to_vec()));
    }

    #[test]
    fn outside_a_repository_there_is_nothing_to_paint() {
        let root = scratch("none");
        let segment = GitSegment::new();
        segment.tick(&root);
        assert_eq!(segment.facts(), Facts::default());
        assert_eq!(segment.legacy(), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_detached_head_is_the_branch_segment() {
        // `git_branch` resolves a detached HEAD to the abbreviated commit; the
        // segment carries it exactly like a branch name.
        let root = scratch("detached");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(
            root.join(".git/HEAD"),
            b"2f4a9c1d0e5b6a7c8d9e0f1a2b3c4d5e6f708192\n",
        )
        .unwrap();
        let segment = GitSegment::new();
        segment.tick(&root);
        assert_eq!(segment.facts().branch, Some(b"2f4a9c1".to_vec()));
        let _ = std::fs::remove_dir_all(&root);
    }
}
