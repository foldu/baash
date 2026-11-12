//! The live git facts in the stock prompt.
//!
//! Two cost classes, deliberately kept apart (`docs/prompt-watch.md` §2):
//!
//! * The **branch** is read from `.git/HEAD` on the prompt path, by
//!   [`baash_lang::prompt::git_branch`]. That is a few small file reads and
//!   `stat`s — microseconds — so it is exact and never stale, and a detached
//!   `HEAD` is resolved to its abbreviated commit id at the same cost.
//! * The **counters** (ahead, modified, untracked) need `git status`, which is
//!   a subprocess that can take hundreds of milliseconds on a cold or large
//!   repository. It never runs on the prompt path: a worker spawns it through
//!   compio's process API, and the prompt renders whatever the last completed
//!   refresh produced.
//!
//! One `git status --porcelain=v2 --branch -z` run supplies all three: the
//! `# branch.ab +N -M` header is the ahead/behind count, `1`, `2` and `u`
//! records are tracked files with staged or unstaged changes, and `?` records
//! are untracked files. The parser is byte-exact and NUL-delimited, so a path
//! containing a newline, a space or a quote cannot confuse it.
//!
//! Refreshes are rate-limited ([`MIN_INTERVAL`]) and coalesced: the worker
//! drains every queued request down to the newest one, so a burst of prompts
//! cannot queue a burst of `git status` runs. A failed refresh (no `git`
//! binary, a broken repository, a repository with no commits yet) is silent and
//! backs off to [`RETRY_AFTER_FAILURE`] rather than spawning a doomed process
//! on every prompt.
//!
//! The prompt never waits for any of this: [`GitSegment::tick`] reads the
//! branch and *schedules* a refresh, and [`GitSegment::facts`] returns the last
//! completed counters for the current directory. A directory change drops the
//! counters immediately rather than showing another repository's numbers; they
//! reappear when the first refresh for the new directory lands.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use baash_lang::prompt::GitStatus;

/// The shortest gap between two `git status` runs.
const MIN_INTERVAL: Duration = Duration::from_millis(500);

/// How long to wait after a refresh that produced nothing.
///
/// A missing `git`, a directory that is not a repository, or a corrupt
/// repository would otherwise fail once per prompt. The prompt stays silent
/// either way; this only stops the worker from burning a process spawn every
/// half second on a permanent condition.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(5);

/// The facts the prompt paints.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    /// The branch, or a detached `HEAD`'s abbreviated commit id. `None` when
    /// the directory is not in a repository.
    pub branch: Option<Vec<u8>>,
    /// The newest completed `git status` for this directory, or `None` while
    /// the first refresh is still in flight (or when there is no repository).
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
    /// A worker that cannot start (no threads, no runtime) is not fatal: the
    /// segment simply stays empty and the prompt is unaffected.
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
    /// screen without a keystroke and without paying for another `git status`.
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
    /// finished `git status`, and asks the worker for a new one no more often
    /// than [`MIN_INTERVAL`].
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
            // Not a repository: no counters to ask for, and a `git status`
            // here would only fail.
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

/// Ask git for the status of one directory, off the prompt path.
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
        let status = runtime.block_on(status(&cwd));
        if updates.send(Update { cwd, status }).is_err() {
            return;
        }
    }
}

/// Run `git status` for `cwd` and return what the prompt needs.
async fn status(cwd: &Path) -> Option<GitStatus> {
    let mut command = compio::process::Command::new("git");
    command
        .arg("--no-optional-locks")
        .arg("status")
        .arg("--porcelain=v2")
        .arg("--branch")
        // Every untracked file, not git's default "this whole new directory is
        // one entry": `?N` promises a file count. The walk happens on the
        // worker, never in front of the cursor.
        .arg("--untracked-files=all")
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
        .then(|| parse_status(&output.stdout))
}

/// Read the four facts out of `git status --porcelain=v2 --branch -z`.
///
/// The NUL-separated form matters: in `--porcelain=v2 -z` a rename is a `2`
/// record followed by its original path as a *separate* NUL field, so counting
/// records without accounting for that would count one rename as two changes.
/// The header lines (`#`) are not changes.
fn parse_status(stdout: &[u8]) -> GitStatus {
    let mut status = GitStatus::default();
    let records: Vec<&[u8]> = stdout.split(|&byte| byte == 0).collect();
    let mut at = 0;
    while at < records.len() {
        let record = records[at];
        at += 1;
        match record.first() {
            Some(b'#') => {
                if let Some(rest) = record.strip_prefix(b"# branch.ab ") {
                    // `+N -M`; the header is absent when there is no upstream,
                    // in which case both stay zero and no `↑` is painted.
                    let mut parts = rest.split(|&byte| byte == b' ');
                    status.ahead = parts.next().map_or(0, count);
                    status.behind = parts.next().map_or(0, count);
                }
            }
            // `1`: ordinary changed entry; `u`: unmerged. Both are one tracked
            // path, whether the change is staged, unstaged or both.
            Some(b'1' | b'u') if record.get(1) == Some(&b' ') => {
                status.modified = status.modified.saturating_add(1);
            }
            // `2`: renamed or copied tracked entry, followed by its original
            // path as the next NUL field.
            Some(b'2') if record.get(1) == Some(&b' ') => {
                status.modified = status.modified.saturating_add(1);
                at += 1;
            }
            // `?`: untracked. `!`: ignored, which we do not ask for.
            Some(b'?') if record.get(1) == Some(&b' ') => {
                status.untracked = status.untracked.saturating_add(1);
            }
            _ => {}
        }
    }
    status
}

/// A `+N`/`-M` counter from the branch header.
fn count(field: &[u8]) -> u32 {
    let digits = field
        .strip_prefix(b"+")
        .or_else(|| field.strip_prefix(b"-"))
        .unwrap_or(field);
    std::str::from_utf8(digits)
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_of(porcelain: &[u8]) -> GitStatus {
        parse_status(porcelain)
    }

    #[test]
    fn the_porcelain_parser_reads_all_four_facts() {
        let porcelain = concat!(
            "# branch.oid 1111111111111111111111111111111111111111\0",
            "# branch.head main\0",
            "# branch.upstream origin/main\0",
            "# branch.ab +23 -4\0",
            "1 .M N... 100644 100644 100644 a b modified.txt\0",
            "1 M. N... 100644 100644 100644 a b staged.txt\0",
            "2 R. N... 100644 100644 100644 a b renamed.txt\0original.txt\0",
            "u UU N... 100644 100644 100644 100644 a b c conflict.txt\0",
            "? untracked.txt\0",
            "? another untracked file.txt\0",
        );
        assert_eq!(
            status_of(porcelain.as_bytes()),
            GitStatus {
                modified: 4,
                untracked: 2,
                ahead: 23,
                behind: 4,
            }
        );
    }

    #[test]
    fn a_rename_is_one_file_not_two() {
        // The original path follows the `2` record as its own NUL field; it is
        // not a second change.
        let porcelain = b"2 R. N... 100644 100644 100644 a b new-name\0old-name\0";
        assert_eq!(status_of(porcelain).modified, 1);
    }

    #[test]
    fn no_upstream_and_no_changes_is_all_zero() {
        let porcelain = concat!(
            "# branch.oid 1111111111111111111111111111111111111111\0",
            "# branch.head main\0",
        );
        assert_eq!(status_of(porcelain.as_bytes()), GitStatus::default());
        assert_eq!(status_of(b""), GitStatus::default());
    }

    #[test]
    fn a_malformed_or_truncated_count_is_zero() {
        assert_eq!(count(b"+23"), 23);
        assert_eq!(count(b"-4"), 4);
        assert_eq!(count(b"+"), 0);
        assert_eq!(count(b"nonsense"), 0);
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
                behind: 0,
            });
        }
        assert_eq!(segment.legacy(), Some(b" [main ~5]".to_vec()));
        segment.lock().status = Some(GitStatus::default());
        assert_eq!(segment.legacy(), Some(b" [main]".to_vec()));
    }

    #[test]
    fn outside_a_repository_there_is_nothing_to_paint() {
        let root = std::env::temp_dir().join(format!("baash-git-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
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
        let root = std::env::temp_dir().join(format!("baash-git-detached-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
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
