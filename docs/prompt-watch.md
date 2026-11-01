# The live prompt: a directory watcher without threads

Status: **partly built.** Written 2026-10-28, after the fork/subinterpreter
discussion in [`portability.md`](portability.md); §7 records what landed. The
wait primitive in §1 is no longer `poll(2)`: the shell runs on compio, so the
watch descriptor is awaited as an io_uring poll (§1.1). The feature: while the prompt
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

`baash_sys::poll_readable(fd, timeout_ms)` exists today (single fd) and is what
the decoder still uses. The loop above is the *shape*, not the implementation.

### 1.1 The wait is an io_uring poll, not `poll(2)`

The shell is on compio (io_uring) already, and a hand-rolled `poll` would be a
second wait mechanism beside the reactor that drives the rest of the loop. So
the watch descriptor is submitted as `compio_driver::op::PollOnce` with
`Interest::READABLE` — `IORING_OP_POLL_ADD` on an arbitrary fd — and the
reactor that wakes the keystroke wakes the watcher through the same completion
queue. That is why [`baash_sys::watch`] hands back a non-blocking descriptor and
never reads on its own: `pending()` drains what the kernel has already queued,
and something else owns the waiting.

The multi-fd `poll` helper is therefore **not** needed for the watcher; it stays
useful only for the `Esc`/`Alt` decoding fix in §7 step 2.

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
| huge repository | the bounded walk stops at `Limits::max_dirs` and watching is abandoned (`Why::TooBig`); the segment is computed per prompt instead, so it stays correct and merely becomes a snapshot. Half-watching would be worse: a stale count is a lie, a computed one is a moment behind |
| `git` missing, or not a repo | segment disappears; no diagnostic, no repeated spawns |
| `git status` slow (cold cache, huge tree) | abandoned on keystroke; if it exceeds a hard limit (say 2 s) the result is discarded and the segment marked stale rather than wrong |
| `git` writes the index (status refresh) | `GIT_OPTIONAL_LOCKS=0` plus `--no-optional-locks`; we are a shell, not a git client |
| worktrees / submodules / bare repos | `.git` may be a file (`gitdir: …`); resolve it; a bare repo has no worktree, so no dirty count |
| rename storms, editor save patterns | debounce coalesces; one refresh per quiet period |

The invariant: **the prompt never blocks, never spawns in the keystroke path,
and never shows a number it cannot justify.** Stale is acceptable; wrong is not;
slow is not.

## 7. Implementation order

1. **Done** — `baash-sys`: `watch` (Linux-only, `cfg`), an `inotify` wrapper
   over `rustix::fs::inotify` (already a dependency; no new crate, no `unsafe`),
   with the budget (`Limits`) and the bail-out. Tests cover create, `HEAD`, a
   branch ref, the depth cap, the budget, symlinked directories and the
   `.git`-file of a linked worktree. `git_dir()` is here too, so the prompt and
   the watcher agree about which repository is being shown.
2. **Next** — `baash-repl`: the editor loop awaits `[tty, watch_fd]`; a
   `Watcher` trait with a fake for tests; `Esc` decoding improves as a side
   effect.
3. Segment rendering + `shopt prompt_watch` + the debounce/abandon rules. The
   parts of the old steps 3–4 that assumed a `git status` subprocess are gone:
   the segment is computed in process by `gix` and the branch by the
   hand-rolled `.git` reader, so nothing spawns and there is nothing to set
   `GIT_OPTIONAL_LOCKS` for (`docs/contracts.md` §3).
4. Optional: shell hook; Windows (`ReadDirectoryChangesW` awaited the same way
   as the inotify descriptor).

Steps 1–2 are worth doing regardless: they fix `Esc`/`Alt` decoding and give the
completion scanner a place to be cancelled from, which is the other feature that
wants to be interruptible.

## 8. Non-goals

* Not a general-purpose file-watch API for scripts. If users want that, it is a
  separate builtin and a separate design.
* Not a git client: no fetching, no hooks, no index writes, no credential
  prompts.
* Not a reason to add an async runtime. This was written when the shell had
  none; it has compio now, so the watcher rides the reactor that is already
  there instead of adding one (§1.1). What still holds is the second half:
  io_uring does not make a directory walk or a `git status` cheaper, which is
  why the segment is computed in process by `gix` and only *when something
  changed*, never per keystroke.

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
