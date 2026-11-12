# The live prompt: what is cheap, what is not, and who pays

Status: **built.** This document describes the stock prompt's git segment as it
ships. The counters are computed in process by `gix` on a worker; an earlier
version computed them on the prompt path, which was the latency bug, and a
later one briefly used a `git` subprocess, which made the shell depend on a
binary it does not need.

The prompt is one line, rebuilt from the shell's state before every command:

```text
 ~/sr/gith/fo/nixos-config   master  ↑23 !2 ?5 $
```

`baash_lang::prompt::tide_prompt` paints it. It is built from data, never decoded
as a `PS1` template, so a directory or branch containing `$`, a backslash or a
`\u`-looking sequence is painted literally and cannot be expanded twice.

## 1. The rule that decides the design

**The prompt never waits and never spawns.** A prompt that blocks on `git` is
the "bash locks up" feeling this shell exists to avoid, and a prompt that runs a
process in the keystroke path is worse. So the work is split by cost, and only
the cheap half is allowed in front of the cursor.

## 2. Two cost classes

| fact | how | cost | freshness |
|---|---|---|---|
| cwd abbreviation | `read_dir` per outer component, once per `cd` | µs (local disk) | exact for that directory |
| branch / detached HEAD | walk to `.git`, read `HEAD`, resolve refs/packed-refs | ~µs–ms, no process | exact |
| ahead / behind | `gix` commit walk: reachable from `HEAD`, not from the upstream ref (and the reverse) | worker, no process | last completed refresh |
| modified tracked files | `gix` status: index↔worktree and tree↔index, distinct paths | worker, no process | last completed refresh |
| untracked files | `gix` status dirwalk, one entry per file | worker, no process | last completed refresh |

The branch is the only git fact read on the prompt path, by
`baash_lang::prompt::git_branch`. It never forks: `.git` is a directory, or a
file naming the real git directory for a linked worktree or a submodule;
`HEAD` is read, `refs/heads/<name>` is shown as the branch, another symbolic ref
is resolved through loose refs, `commondir` and `packed-refs`, and a detached
`HEAD` is shown as its seven-character commit abbreviation. An absent or
unreadable repository omits the segment; nothing is guessed.

**A detached `HEAD` shows the abbreviated commit id.** That is the same value
the file read already resolves to, so it costs no extra work and answers "which
commit am I on" instead of leaving a gap.

## 3. The refresh task

`crates/baash/src/git_prompt.rs` schedules at most one refresh at a time on the
shell's `compio` runtime, and each refresh opens the repository with
`gix::discover`, then:

* **modified / untracked:** `repo.status(Discard)` with
  `UntrackedFiles::Files`, iterating the index↔worktree and tree↔index
  entries. Distinct paths go into sets, so a file changed in the index and
  again in the worktree is one modified file, and a rename is one file, not
  two. `gix` honours `.gitignore` and the repository's configuration.
* **ahead / behind:** commits reachable from `HEAD` but not from the upstream
  tracking ref, and the reverse. `rev_walk(...).with_hidden(...)` is exactly
  `git rev-list --count HEAD ^upstream`; no merge-base call and no subprocess.
  The upstream name is derived by `gix` from `branch.<name>.remote` and the
  fetch refspec, so a renamed remote or a custom `refs/remotes` layout follows
  the repository's own configuration.

It is deliberately **not** `git status`: the counters must work on a machine
with no `git` binary. Only the branch read (`.git/HEAD`) is cheaper still, and
that happens on the prompt path.

* **One at a time:** a `in_flight` flag, so a burst of prompts cannot queue a
  burst of walks. The second prompt in a burst wants the same answer as the
  first, and the next `tick` after the run lands starts the next one.
* **Rate limit:** at most one run started per [`MIN_INTERVAL`] = 500 ms. The
  prompt asks; the cache decides when it is due.
* **Deadline:** 2 s, and the deadline is the runtime's timer, not a thread:
  `compio::time::timeout(STATUS_TIMEOUT, spawn_blocking(walk))`. `gix`'s status
  also takes the `Arc<AtomicBool>` it checks between chunks
  (`should_interrupt_owned`), and the task sets it when the timer wins, so a
  walk the prompt gave up on stops soon after instead of finishing a tree
  nobody wants. `spawn_blocking` is not cancellable by contract, so a walk
  stuck inside one uninterruptible syscall keeps a pool thread until it
  returns; nothing in front of the cursor waits for it.
* **Fire and forget:** the task is `detach`ed, because dropping a `compio`
  `JoinHandle` cancels its task and this one must outlive the prompt that
  started it.
* **Failure back-off:** a run that produces nothing (not a repository, a
  corrupt one, a timeout) is silent and delays the next attempt by 5 s. A
  permanent condition does not repeat an expensive walk on every prompt.

One run yields all of ahead, behind, modified and untracked, so the prompt never
needs a second walk.

## 4. What is painted while a refresh is in flight

The prompt always paints a **completed** result, never a half-updated one:

* Same directory: the previous completed counters stay on screen until the new
  run lands. The number is a moment old, never a mixture of two directories.
* A `cd`: the counters are dropped immediately and the segment reappears when
  the first run for the new directory lands. Showing another repository's
  numbers in the gap would be worse than showing none.
* Before the first run in a session: the branch is there (exact), the counters
  are not.

The worker result is installed by `GitSegment::poll`, which the prompt path
calls before composing (`tick`) and the editor calls on its idle wake-up. There
is one cache per `GitSegment`; the shell and the editor share the clone, so
there is no second worker and no race about which result is newer.

## 5. The watcher and the live repaint

On Linux, `baash_sys::watch` arms an inotify watch on the working tree and its
git directory. The editor awaits that descriptor beside the keyboard
(`Terminal::await_input`), so while a line is idle:

1. A file change wakes the editor. `PromptState::refresh` drains the watch and
   calls `GitSegment::tick`, which schedules the rate-limited `gix` walk.
2. The editor also has a one-second backstop. On a wake-up with nothing in the
   queue it calls `GitSegment::poll`; a result that landed since the last frame
   is painted then. That is how the new count appears without a keystroke, even
   though a worker completion cannot itself wake the inotify descriptor.
3. The directory abbreviation is cached, so the repaint does not re-run
   `read_dir` per component.

The worst case for a visible count is therefore about one second after a change,
or the next prompt, whichever comes first. A slow repository does not affect the
keystroke, the repaint or the prompt.

A user-defined `PS1` is not live: re-rendering it while the user types would run
shell code between keystrokes. It keeps the legacy `[branch ~N]` suffix, fed by
the same worker cache.

## 6. Failure modes

| situation | behaviour |
|---|---|
| not a repository | no branch, no counters, no walk; the segment is omitted |
| repository with no commits | branch exact; there are no commits to be ahead or behind of, and the index/worktree walk still counts; no `↑`/`↓` |
| corrupt repository | branch may still resolve; `gix` fails; silent, retry after 5 s |
| linked worktree / submodule | `.git` file is followed; the git dir is watched through it |
| huge repository | the watcher may refuse (`Why::TooBig`); the prompt then refreshes once per prompt, still off the prompt path |
| network filesystem | inotify may deliver nothing; the prompt still refreshes at each prompt, and the one-second backstop cannot hurt |
| `gix` walk slow or hung | prompt unaffected; the 2 s deadline discards slow results; counters stale until a run finishes |
| an editor save touching many paths | coalesced into one run, and the watcher's event queue is drained in one read |

The invariant: **the prompt never blocks, never spawns in the keystroke path,
and never shows a number it cannot justify.** Stale is acceptable; wrong is not;
slow is not.

## 7. Measurements

The `gix` status walk on this repository (574 tracked files), warm page cache,
on the development host: **7–9 ms** per run. A cold repository is dominated by
the stat of every tracked path and the index read; the figure was not measured
because dropping the page cache needs root on the test host, and the design does
not depend on it: the run is off the prompt path, the deadline caps it at 2 s,
and the prompt paints the last completed result meanwhile.

The branch read is a handful of `stat`/`read` calls and is not measurable at
prompt granularity.

## 8. What landed

* `crates/baash/src/git_prompt.rs` — the worker, the `gix` status and
  ahead/behind walk, the 2 s deadline, the shared cache (`GitSegment`), the
  500 ms limit, coalescing, failure back-off.
* `crates/baash/src/prompt_state.rs` — the directory cache, prompt context,
  composer and watcher; implements `baash_repl::LivePrompt`.
* `crates/baash-lang/src/prompt.rs` — `tide_prompt`, `shorten_path`,
  `PathAbbreviator`, `GitStatus`, `git_branch`.
* `crates/baash-repl` — unchanged: it already measures the prompt with
  `unicode-width` and strips ANSI CSI sequences, so the glyphs count as one cell
  and the SGR escapes as zero. `\[`/`\]` remain the marker for a *user's*
  coloured prompt; the stock prompt needs none.

## 9. Non-goals

* Not a general-purpose file-watch API for scripts.
* Not a git client: no fetching, no hooks, no index writes, no credential
  prompts, and no `git` binary.
* No shell hook. A `BAASH_PROMPT_WATCH` function would run shell code between
  keystrokes, which is the one thing this design refuses.
