# The live prompt: what is cheap, what is not, and who pays

Status: **built.** This document describes the stock prompt's git segment as it
ships. It replaces the earlier design note that assumed an in-process `gix`
status; the cost class is the same question, and the answer moved to a worker.

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
| ahead / behind | `# branch.ab +N -M` from one `git status` | subprocess | last completed refresh |
| modified tracked files | `1`, `2`, `u` records from the same run | subprocess | last completed refresh |
| untracked files | `?` records from the same run, `-uall` | subprocess | last completed refresh |

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

## 3. The worker

`crates/baash/src/git_prompt.rs` owns one worker thread and one channel. One
run is

```sh
git --no-optional-locks status --porcelain=v2 --branch --untracked-files=all -z
```

with `LC_ALL=C`, `GIT_OPTIONAL_LOCKS=0` and `GIT_TERMINAL_PROMPT=0`, spawned
through compio's process API. The `-z` form is parsed byte-exactly: `2` records
(a rename or copy) are followed by the original path as a separate NUL field,
which is skipped rather than counted as a second change; `1` and `u` are one
tracked file each; `?` is one untracked file each; `#` is a header.

* **Rate limit:** at most one run per [`MIN_INTERVAL`] = 500 ms. The prompt asks;
  the worker decides when it is due.
* **Coalescing:** the worker drains every queued request down to the newest
  directory before running, so a burst of prompts cannot queue a burst of
  processes.
* **Failure back-off:** a run that produces nothing (no `git` binary, a broken
  repository, a repository with no `HEAD` yet) is silent and delays the next
  attempt by 5 s. A permanent condition does not spawn a doomed process on every
  prompt.
* **No timeout.** A hung `git` (a network filesystem, a stuck lock) leaves the
  counters stale; it cannot delay the prompt, and the prompt keeps painting the
  last completed numbers. This is a deliberate trade: killing a child safely
  needs an ownership model the current `compio` output future does not give us,
  and a leaked or half-read process would be worse than a stale number.

One run yields all of ahead, modified and untracked, so the prompt never needs
a second process.

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
   calls `GitSegment::tick`, which schedules the rate-limited `git status`.
2. The editor also has a one-second backstop. On a wake-up with nothing in the
   queue it calls `GitSegment::poll`; a result that landed since the last frame
   is painted then. That is how the new count appears without a keystroke, even
   though a subprocess completion cannot itself wake the inotify descriptor.
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
| not a repository | no branch, no counters, no process; the segment is omitted |
| `git` missing or not executable | branch still exact; counters silent; retry after 5 s |
| repository with no commits | branch exact; `git status` succeeds with an empty `branch.ab`, so no `↑`; counters work |
| corrupt repository | branch may still resolve; `git status` fails; silent, retry after 5 s |
| linked worktree / submodule | `.git` file is followed; the git dir is watched through it |
| huge repository | the watcher may refuse (`Why::TooBig`); the prompt then refreshes once per prompt, still off the prompt path |
| network filesystem | inotify may deliver nothing; the prompt still refreshes at each prompt, and the one-second backstop cannot hurt |
| `git status` slow or hung | prompt unaffected; counters stale until the run finishes |
| an editor save touching many paths | coalesced into one run, and the watcher's event queue is drained in one read |

The invariant: **the prompt never blocks, never spawns in the keystroke path,
and never shows a number it cannot justify.** Stale is acceptable; wrong is not;
slow is not.

## 7. Measurements

`git status --porcelain=v2 --branch --untracked-files=all -z` on this repository
(574 tracked files), warm page cache, on the development host: **1–9 ms** over
ten runs, average about 1 ms. A cold repository is dominated by the stat of
every tracked path and the index read; the figure was not measured because
dropping the page cache needs root on the test host, and the design does not
depend on it: the run is off the prompt path, and the prompt paints the last
completed result meanwhile.

The branch read is a handful of `stat`/`read` calls and is not measurable at
prompt granularity.

## 8. What landed

* `crates/baash/src/git_prompt.rs` — the worker, the porcelain parser, the
  shared cache (`GitSegment`), the 500 ms limit, coalescing, failure back-off.
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
  prompts.
* No shell hook. A `BAASH_PROMPT_WATCH` function would run shell code between
  keystrokes, which is the one thing this design refuses.
