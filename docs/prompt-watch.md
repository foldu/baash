# The live prompt: a directory watcher without threads

Status: **design, not built.** Written 2026-10-28, after the fork/subinterpreter
discussion in [`portability.md`](portability.md). The feature: while the prompt
is sitting idle in a git repository, show the branch, ahead/behind, and a
dirty-file count, and update them when something changes — new commits, a
checkout, `git add`, a file edit — without the user pressing Enter.

The point of writing it down before building it: this is the first feature that
*wants* a background thread, and the obvious implementation (a watcher thread) is
the one that trips the fork gate on the next `$( … )`. The design below gets the
same behaviour on one thread, with no async runtime and no new dependency.

---

## 1. Why the obvious design is wrong here

This was written when a watcher thread had to be registered with
`baash_sys::fork_gate()`, or the next `$( … )`, `( … )` or `builtin | builtin`
would panic (`fork() with 1 helper thread(s) live`). The 2026-10-30 pivot
deleted the shell's last `fork`, so the gate and that whole problem are gone;
the design below is kept because it is still the shape the watcher should take,
not because threads are forbidden.
That is the wrong trade for a cosmetic prompt.

The editor already blocks waiting for exactly one thing: a keystroke. Waiting on
two things instead of one is not a concurrency problem, it is a `poll(2)` call.
So:

```
loop {
    ready = poll([tty, watch_fd], timeout = next_deadline)
    if tty readable:  decode + handle keystroke, repaint
    if watch_fd ready: mark prompt dirty (debounce)
    if deadline:      recompute prompt segment if dirty and idle, repaint
}
```

One thread, no gate interaction, and it is also the wrapper the editor already
needs for another reason: a lone `Esc` is currently indistinguishable from the
start of an `Alt-` sequence because the decoder blocks on `read`.

`baash_sys::poll_readable(fd, timeout_ms)` exists today (single fd). The first
code change is a multi-fd `poll(fds: &mut [(RawFd, PollFlags)], timeout_ms)` —
small, testable in `baash-sys/tests/pty.rs`, and shared by both consumers.

## 2. Watching: cheap events continuously, expensive check only when idle

The mistake to avoid is running `git status` per event. Split the work by cost:

| what | how | cost | freshness |
|---|---|---|---|
| branch name / detached HEAD | read `.git/HEAD`; if `ref: refs/heads/x`, name `x`, else short sha | ~10 µs | exact |
| ahead / behind | parse `packed-refs` + `refs/remotes/**`, compare object ids, walk parents only when they differ | µs–ms | exact |
| staged / index changed | inotify on `.git/index` | event | heuristic |
| commits / checkouts / branch switches | inotify on `.git/HEAD`, `.git/refs/**`, `.git/packed-refs` | event | exact |
| **modified files in the worktree** | `git status --porcelain=v2 -z --branch` (subprocess) **or** per-directory inotify | 10–500 ms | debounced |

Two watch scopes, and the choice between them is a configuration question:

* **`.git` only** (default): the prompt is exact for branch/commits, and shows a
  *stale* dirty marker that is refreshed by an idle-time `git status` on a
  debounce (see §3). Cheap: a handful of watches.
* **`.git` + the worktree, recursively**: one watch per directory. A normal
  repository is a few hundred; a monorepo is thousands, and
  `fs.inotify.max_user_watches` is a real limit to respect (see §6). This is the
  only way to notice `vim foo.rs` immediately without running `git status`.

Recommendation: ship `.git`-only first, with the idle refresh; add recursive
worktree watches only if the debounce refresh proves visible.

### Never in the input path

`git status` is a subprocess. Spawning it from the keystroke path would put
10–500 ms between keystrokes, which is exactly the "bash locks up" feeling this
shell exists to avoid. Rules:

1. The prompt is rendered from a **cache**; rendering never runs a process.
2. A refresh is scheduled only when (a) something changed, (b) the buffer is
   empty and no keystroke has arrived for `debounce` (default 150 ms), and
   (c) `now - last_refresh >= min_interval` (default 500 ms).
3. A refresh is **abandoned** if a keystroke arrives while it would run: poll
   with the tty fd before spawning, and again after; if the user typed, discard
   the result and mark dirty again.
4. The spawn itself is a normal `posix_spawn` (no `fork`), so the gate is not
   involved; environment is `LC_ALL=C`, `GIT_OPTIONAL_LOCKS=0`,
   `GIT_TERMINAL_PROMPT=0`, `--no-optional-locks`, `-z` so the parser is
   byte-exact and not affected by locale or quoting.

## 3. The prompt segment

The prompt is already a rendered byte string (`ExpandedPrompt`) rebuilt per line
from the shell's variables. A live segment is one more input to that rendering:

```rust
/// One prompt segment whose text can change without a new line.
pub struct Segment {
    /// Rendered bytes, already encoded for the prompt.
    text: Vec<u8>,
    /// When `text` was computed.
    computed_at: Instant,
    /// Set by an event; cleared by a successful recompute.
    dirty: bool,
}

pub trait SegmentSource {
    /// Cheap: called from the event loop. Must not spawn or block.
    fn on_event(&mut self, kind: WatchEvent);
    /// Expensive: only called when idle, and must be cancellable between steps.
    fn refresh(&mut self, shell: &mut PromptState) -> Option<Vec<u8>>;
}
```

Two implementations, in this order:

1. **Built-in git segment** (fast path): parses `.git` directly for branch and
   ahead/behind; the dirty count comes from the debounced `git status`, cached
   with a timestamp. No shell code runs, so no hooks, no aliases, no surprises.
2. **Shell hook** (optional): a function such as `BAASH_PROMPT_WATCH` called on
   the same debounce, for people who want something else (hg, jj, nix, a
   directory of todos). This one runs shell code in the input loop, so it gets
   the same abandonment rules and must be documented as "keep it fast; it runs
   while you are not typing".

The segment is rendered as `[branch +2 ~3]`-style text; the exact spelling and
colours belong to the prompt configuration, not to this document. It must be
wrapped so it does not disturb the cursor maths: the renderer already tracks
non-printing ranges (`\[`/`\]`), so a coloured segment is emitted the same way.

## 4. Where the state lives

* The watcher and the cache live in `baash-repl` (it owns the event loop), behind
  a trait so the editor can be tested with a fake watcher.
* The git parsing lives in `baash-repl` too, or in a small `baash-git` module if it
  grows; it needs no shell state beyond the working directory.
* The shell does not learn about the watcher. `PROMPT_COMMAND`, `PS1` and the
  segment composer see the same rendered prompt as today.
* `cd` invalidates everything: the editor already calls `set_cwd` on a directory
  change; the watcher re-arms there.

## 5. Configuration

Proposed defaults, all open to argument:

* `shopt -s prompt_watch` — on by default for an interactive shell whose `cwd`
  is inside a git worktree; off otherwise. No watchers in a non-repo directory,
  which is most of them.
* `PROMPT_WATCH_DEBOUNCE_MS` (150), `PROMPT_WATCH_MIN_INTERVAL_MS` (500) as
  variables, so a user can tune or disable the expensive refresh.
* `BAASH_PROMPT_WATCH=1` / `shopt -u prompt_watch` to disable entirely.
* Per-directory escape hatch: a `.baash-watch-off` file, or just `shopt -u` in an
  rc function.

## 6. Failure modes, and what the prompt must do about them

| situation | behaviour |
|---|---|
| `inotify` unavailable, or `max_user_watches` exhausted | fall back to TTL polling (`min_interval` becomes the only trigger); report once at debug level, never at the prompt |
| network filesystem (NFS, sshfs) | inotify does not deliver remote changes; the TTL fallback is the whole feature. Same fallback path. |
| huge repository | recursive watches refused by the kernel limit; degrade to `.git`-only + TTL, do not error |
| `git` missing, or not a repo | segment disappears; no diagnostic, no repeated spawns |
| `git status` slow (cold cache, huge tree) | abandoned on keystroke; if it exceeds a hard limit (say 2 s) the result is discarded and the segment marked stale rather than wrong |
| `git` writes the index (status refresh) | `GIT_OPTIONAL_LOCKS=0` plus `--no-optional-locks`; we are a shell, not a git client |
| worktrees / submodules / bare repos | `.git` may be a file (`gitdir: …`); resolve it; a bare repo has no worktree, so no dirty count |
| rename storms, editor save patterns | debounce coalesces; one refresh per quiet period |

The invariant: **the prompt never blocks, never spawns in the keystroke path,
and never shows a number it cannot justify.** Stale is acceptable; wrong is not;
slow is not.

## 7. Implementation order

1. `baash-sys`: multi-fd `poll` + tests on a pty; `inotify` wrapper (Linux-only,
   `cfg`, no new crate — raw `libc::inotify_init1`/`inotify_add_watch`).
2. `baash-repl`: editor loop polls `[tty, watch_fd, timer]`; a `Watcher` trait
   with a fake for tests; `Esc` decoding improves as a side effect.
3. `baash-repl`/`baash-git`: parse `.git/HEAD`, `packed-refs`, refs; a
   `git status --porcelain=v2 -z` runner with the environment above.
4. Segment rendering + `shopt prompt_watch` + the debounce/abandon rules.
5. Optional: recursive worktree watches; shell hook; Windows
   (`ReadDirectoryChangesW` in the same wait set).

Steps 1–2 are worth doing regardless: they fix `Esc`/`Alt` decoding and give the
completion scanner a place to be cancelled from, which is the other feature that
wants to be interruptible.

## 8. Non-goals

* Not a general-purpose file-watch API for scripts. If users want that, it is a
  separate builtin and a separate design.
* Not a git client: no fetching, no hooks, no index writes, no credential
  prompts.
* Not a reason to add an async runtime. `poll` on two fds is 40 lines; compio
  would be a dependency, a runtime and a second threading model for the same
  result. The earlier conclusion stands: io_uring does not make a directory walk
  or a `git status` cheaper.

## 9. Open questions

* Default on or off? A prompt that updates itself is delightful when it is right
  and infuriating when it costs 200 ms. The debounce makes it safe, but the
  first-run experience in a monorepo is the risk.
* Does the dirty count belong in the prompt at all, or only a `*` when the
  cached status is older than `min_interval`? A wrong number is worse than no
  number.
* Segment spelling/colours: `[main +2 ~3]`, `main*`, a powerline-ish block?
  Needs a real decision before it ships.
* Should the shell hook be able to *replace* the built-in segment (hg/jj users)
  or only add to it?
