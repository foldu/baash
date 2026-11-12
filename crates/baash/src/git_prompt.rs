//! The live git facts in the stock prompt.
//!
//! Two cost classes, deliberately kept apart (`docs/prompt-watch.md` §2):
//!
//! * The **branch** is read from `.git/HEAD` on the prompt path, by
//!   [`baash_lang::prompt::git_branch`]. That is a few small file reads and
//!   `stat`s — microseconds — so it is exact and never stale, and a detached
//!   `HEAD` is resolved to its abbreviated commit id at the same cost.
//! * The **counters** (ahead, behind, modified, untracked) need a status, which
//!   walks the index and the worktree. That never runs on the prompt path: a
//!   worker thread computes it in process with `gix`, and the prompt renders
//!   whatever the last completed refresh produced.
//!
//! It is deliberately **not** `git status`: the shell must not depend on a `git`
//! binary to show the counters. `gix` reads the index, honours `.gitignore` and
//! runs the directory walk itself. Ahead/behind come from the commit graph:
//! commits reachable from `HEAD` but not from its upstream tracking ref (and
//! the reverse), which is exactly `git rev-list --count HEAD ^upstream`.
//!
//! A refresh runs under a deadline. `gix`'s status takes a shared interrupt
//! flag and checks it between chunks; a small timer sets it after
//! [`STATUS_TIMEOUT`], and an interrupted result is discarded rather than
//! painted. A status that is stuck inside one uninterruptible syscall can keep
//! the worker busy, but it cannot delay the prompt and it does not accumulate
//! threads: the next request is coalesced into the one the worker is already
//! running.
//!
//! Refreshes are rate-limited ([`MIN_INTERVAL`]) and coalesced: the worker
//! drains every queued request down to the newest one, so a burst of prompts
//! cannot queue a burst of status runs. A failed refresh (no repository, a
//! corrupt one, a timeout) is silent and backs off to
//! [`RETRY_AFTER_FAILURE`].
//!
//! The prompt never waits for any of this: [`GitSegment::tick`] reads the
//! branch and *schedules* a refresh, and [`GitSegment::facts`] returns the last
//! completed counters for the current directory. A directory change drops the
//! counters immediately rather than showing another repository's numbers; they
//! reappear when the first refresh for the new directory lands.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
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

/// A finished refresh.
struct Update {
    /// The directory it described. A result for a previous directory must not
    /// be shown as if it described the current one.
    cwd: PathBuf,
    status: Option<GitStatus>,
}

/// The state shared by the prompt path and the worker's consumer.
struct State {
    request: Sender<PathBuf>,
    updates: Receiver<Update>,
    /// The branch of [`State::cwd`]; `None` outside a repository.
    branch: Option<Vec<u8>>,
    /// The newest completed status for [`State::cwd`].
    status: Option<GitStatus>,
    /// The directory `branch` and `status` describe.
    cwd: Option<PathBuf>,
    /// When the next refresh may be requested.
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
/// second worker.
#[derive(Clone)]
pub struct GitSegment {
    state: Arc<Mutex<State>>,
}

impl GitSegment {
    /// Start the worker and return an empty segment.
    ///
    /// A worker that cannot start (no threads) is not fatal: the segment
    /// simply stays empty and the prompt is unaffected.
    pub fn new() -> Self {
        let (request, requests) = mpsc::channel::<PathBuf>();
        let (sender, updates) = mpsc::channel::<Update>();
        std::thread::Builder::new()
            .name("baash-git-prompt".to_owned())
            .spawn(move || worker(requests, sender))
            .ok();
        Self {
            state: Arc::new(Mutex::new(State {
                request,
                updates,
                branch: None,
                status: None,
                cwd: None,
                next_attempt: None,
                #[cfg(test)]
                ticks: 0,
            })),
        }
    }

    /// Install every refresh that has finished, without scheduling a new one.
    ///
    /// The editor calls this on its idle wake-up so a worker result reaches the
    /// screen without a keystroke and without paying for another status run.
    pub fn poll(&self) {
        let mut state = self.lock();
        let now = Instant::now();
        // The worker coalesces, so more than one can only be queued if the
        // prompt was not drawn for a while.
        while let Ok(update) = state.updates.try_recv() {
            if state.cwd.as_deref() != Some(update.cwd.as_path()) {
                // A result for a directory the prompt has already left.
                continue;
            }
            let failed = update.status.is_none();
            state.status = update.status;
            if failed {
                state.next_attempt = Some(now + RETRY_AFTER_FAILURE);
            }
        }
    }

    /// Bring the segment up to date for `cwd`, and schedule a refresh if due.
    ///
    /// Called once per prompt, and again whenever the editor is woken while
    /// the line is idle. Reads `.git/HEAD` (cheap, exact), installs any
    /// finished status, and asks the worker for a new one no more often than
    /// [`MIN_INTERVAL`].
    pub fn tick(&self, cwd: &Path) {
        self.poll();
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
        let branch = baash_lang::prompt::git_branch(&bytes);
        if branch != state.branch {
            state.branch = branch;
        }

        if state.branch.is_none() {
            // Not a repository: no counters to ask for, and a status here would
            // only walk nothing.
            return;
        }
        let now = Instant::now();
        if state.next_attempt.is_some_and(|at| now < at) {
            return;
        }
        state.next_attempt = Some(now + MIN_INTERVAL);
        let _ = state.request.send(cwd.to_path_buf());
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

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // The mutex only guards plain data and channel handles; a panic while
        // holding it would be a bug in this module, and poisoning is then no
        // reason to take the whole shell down with the prompt. Recover the
        // guard rather than unwrapping.
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

impl Default for GitSegment {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute the status of one directory, off the prompt path.
fn worker(requests: Receiver<PathBuf>, updates: Sender<Update>) {
    while let Ok(first) = requests.recv() {
        // Coalesce: a queued older directory is worthless, since only the
        // newest prompt is on screen.
        let mut cwd = first;
        while let Ok(newer) = requests.try_recv() {
            cwd = newer;
        }
        let status = refresh(&cwd);
        if updates.send(Update { cwd, status }).is_err() {
            return;
        }
    }
}

/// Open the repository containing `cwd` and compute what the prompt paints.
fn refresh(cwd: &Path) -> Option<GitStatus> {
    let repo = gix::discover(cwd).ok()?;
    run_with_deadline(STATUS_TIMEOUT, |interrupt| status_with(&repo, interrupt))
}

/// Run `refresh` under a deadline, returning `None` when the deadline won.
///
/// The timer is a short-lived thread rather than a poll in the worker: the
/// worker is inside `gix` and cannot watch a clock. It sleeps for the whole
/// deadline and is harmless afterwards — it holds only two `Arc<AtomicBool>`s
/// and exits. A refresh that finishes in milliseconds leaves at most a handful
/// of sleeping timer threads, never a running one.
fn run_with_deadline<F>(timeout: Duration, refresh: F) -> Option<GitStatus>
where
    F: FnOnce(&Arc<AtomicBool>) -> Option<GitStatus>,
{
    let interrupt = Arc::new(AtomicBool::new(false));
    let timed_out = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    {
        let interrupt = Arc::clone(&interrupt);
        let timed_out = Arc::clone(&timed_out);
        let done = Arc::clone(&done);
        let _ = std::thread::Builder::new()
            .name("baash-git-deadline".to_owned())
            .spawn(move || {
                std::thread::sleep(timeout);
                // Only a refresh that is still running is late. Checking
                // `done` first keeps a completed result from being thrown away
                // because the timer happened to wake at the same moment.
                if !done.load(Ordering::Acquire) {
                    timed_out.store(true, Ordering::Release);
                    interrupt.store(true, Ordering::Release);
                }
            });
    }

    let result = refresh(&interrupt);
    done.store(true, Ordering::Release);
    if timed_out.load(Ordering::Acquire) {
        // `gix` stops at its next interrupt check, which may be after the
        // deadline; whatever it collected is partial, so paint nothing.
        None
    } else {
        result
    }
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

    #[test]
    fn a_fast_refresh_keeps_its_result() {
        let result = run_with_deadline(Duration::from_secs(10), |_| {
            Some(GitStatus {
                modified: 3,
                ..GitStatus::default()
            })
        });
        assert_eq!(result.map(|status| status.modified), Some(3));
    }

    #[test]
    fn a_refresh_that_outlives_the_deadline_is_discarded() {
        let started = Instant::now();
        let result = run_with_deadline(Duration::from_millis(30), |interrupt| {
            while !interrupt.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            Some(GitStatus::default())
        });
        assert_eq!(result, None);
        assert!(
            started.elapsed() >= Duration::from_millis(25),
            "the deadline was not waited for"
        );
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
            refresh(&root),
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

        let status = refresh(&work).expect("a status");
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
