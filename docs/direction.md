# Direction after the 2026-10-30 pivot

The project is not trying to be bash. It is trying to be a **fast, pleasant,
interactive shell that is similar enough to bash to run simple scripts**, with
an architecture that is not poisoned by `fork`. This document supersedes the
"conformance first" ordering in `roadmap.md` and the fork-based assumptions in
`state.md`.

## 1. The five commitments

1. **No `fork()`.** Subshells and command substitution are in-process
   subinterpreters with scoped state. Anything that cannot work that way is
   rejected with a diagnostic, not approximated.
2. **Windows is a first-class target.** Cross-compiled, not CI-tested at first.
3. **Interactive first.** No lag at the prompt, in completion, or in prompt
   refresh; enabled by a completion-based runtime (compio: io_uring on Linux,
   IOCP on Windows) and worker threads for pure CPU work.
4. **Shell code runs on the main thread.** Workers handle I/O and data, never
   the interpreter.
5. **uutils embedded** as built-ins (curated set), so installation does not
   require a coreutils distribution.

## 2. Decisions taken

| question | decision |
|---|---|
| job control | **removed for now.** No `jobs`/`fg`/`bg`/`disown`, no `%` specs, no process groups, no `^Z` handling. Revisit only if it is actually missed. |
| event loop | **compio**, not a hand-rolled `poll` and not tokio. Completion-based I/O maps onto io_uring (Linux) and IOCP (Windows); tokio's file I/O is a blocking thread pool on both, which is the part we do not want. Tokio ecosystem compatibility is not a goal. |
| thread ownership | interpreter on the main thread; workers only touch snapshots and I/O. |
| profiles | **hard cut.** No `--profile=fork`. The fork path is deleted, not kept behind a flag. |
| conformance | a **curated subset** later; for now measure what works. The Oils number stops being a goal and becomes a regression signal we re-baseline. |
| Windows CI | none; cross-compile. |
| uutils | embedded, curated (not all 100+), feature-gated so Linux builds can skip them if desired. |
| bytecode backend | **deferred, not dropped.** The tree-walking evaluator is the engine for now; the crate keeps its name because a real VM is the intent once the bash surface is cut down (DESIGN.md §5 records the design). Two engines today would be a permanent agreement to maintain, and dispatch is a small term when every statement ends in a syscall or a word expansion. |
| feature ceiling | "up to and including `coproc`" — see the open question in §7, because `coproc` conflicts with the other commitments. |

## 3. What this deletes

* `fork_gate`, `ThreadGate`, `spawn_forked`, `fork_child`, `fork_child_status`.
* Job control: `JobTable`, `Job`, `enable_job_control`, terminal handover,
  `TerminalGuard`, stopped-job handling, `WUNTRACED` waits, prompt notices,
  `SIGTTOU`/`SIGTTIN`/`SIGTSTP` manipulation, `%` job specifications.
* The completion-`-F` plan as scoped: shell functions may still be callable from
  completion, but on the main thread (no worker).
* The `poll`-based live-prompt design in `prompt-watch.md` — replaced by
  compio operations (the document's cost model stays valid, its plumbing does
  not).

## 4. What gets rejected, and how

New diagnostic `BAASH-E0302` (`CAPABILITY`): *this construct needs something this
shell does not have.* It must name the construct and the reason.

| construct | outcome | why |
|---|---|---|
| external command | works (`posix_spawn` / `CreateProcess`) | |
| `( … )` subshell | works (scope push/pop) | no fork needed |
| `$( … )` / backticks | works (scope + stdout capture) | the 95 % case |
| `( exec cmd )` | rejected | `exec` would replace the shell |
| `builtin \| builtin`, `fn \| fn` | **last stage in-process** (lastpipe), others rejected | `cmd \| while read` is everyday; see §7 |
| `f &`, `{ … } &` | rejected | a job needs a pid |
| `sleep &` | works | external, real process |
| `jobs`/`fg`/`bg`/`disown` | rejected with `CAPABILITY` | job control removed |
| `%1` in `wait`/`kill` | rejected | no job table |
| `exec cmd` (top level) | works on Unix, rejected on Windows | no `exec` on Windows |
| `exec 3<file` | works | just the fd table |
| `<(...)`, `>(...)` | rejected (today `BAASH-E0301`) | needs /dev/fd |
| `coproc` | **open** — see §7 | |
| `set -m` | accepted, inert | rc files should not break |
| `^Z` | unhandled: stops the child and the shell together (bash with `set +m`) | no job control |

## 5. Architecture

**Subinterpreter.** A scope records the mutable state a subshell can change —
variables (including arrays and namerefs), functions, options, aliases, traps,
positional parameters, the descriptor table, `cwd`, `umask`, `set -m` — and pops
it, discarding changes, when the body ends. `$( … )` is the same scope with
fd 1 temporarily redirected to a pipe (the existing `capture_stdout` pattern is
the prototype). Everything the previous design got from copy-on-write must be
enumerated explicitly; a missing field is a silent semantic leak, so the scope
push should be written as one exhaustive `impl` over the state struct rather
than a hand-written list in the REPL.

**Rejection is pre-parse where possible.** A subshell body is scanned for
constructs that cannot execute in-process (`exec`, `&` on shell code, a
builtin in a non-last pipeline position) *before* the body runs, so a rejection
cannot leave half its side effects behind. `eval` can still introduce one at run
time; that case rejects at the point of failure and is documented.

**compio.** One runtime, main thread. The REPL is an async loop; the interpreter
remains synchronous and is called from it, and blocking it during a command is
accepted (the prompt is not visible then). File I/O — directory scans, `stat`,
the `git status` subprocess — uses compio's completion-based operations instead
of a thread per blocked read. CPU-bound work (ranking thousands of completion
candidates) goes to a worker thread and returns through a channel. Inotify is
not provided by compio, so `baash-sys` grows a small inotify wrapper of its own
(`inotify_init1`/`inotify_add_watch`/event parsing) and the fd is awaited with
`compio::runtime::fd::PollFd::read_ready` — readiness-based, no extra crate.
Workers never receive `&mut Interp`.

**Why not tokio.** Our I/O is files and processes, not sockets. Tokio's `fs` is
`spawn_blocking` underneath on both platforms — a thread parked per operation,
which is the "hacky file solution" this pivot is trying to leave behind — and
tokio's Windows file support is likewise readiness-based with a blocking pool.
compio's model is the one Windows actually exposes (IOCP) and the one Linux
prefers (io_uring). The cost is maturity: compio is pre-1.0, so W3 begins with a
spike, not a migration.

**Windows.** See `portability.md` §5 for the four boundaries (process creation,
UTF-16 at the edges, ConPTY, job control — the last now deleted, which
simplifies it). No job control means no `tcsetpgrp`, no process groups, and
`^C` is a console control event.

## 6. Milestones

| # | milestone | acceptance |
|---|---|---|
| W1 | delete job control | gate green; `jobs`/`fg`/`bg` report `CAPABILITY`; `wait`/`kill`/`$!` still work for pids; baseline re-recorded |
| W2 | no-fork subshells | `( … )` and `$( … )` in-process; `exec` in subshell, shell-code `&`, non-last builtin pipeline stages rejected with 302; the four fork call sites and `fork_gate` deleted |
| W3 | compio event loop | spike proven (see §9); then the REPL loop on compio, `git status` spawned and awaited, directory scans through compio, completion scan and prompt refresh off the main thread; no custom poll code; no input lag while a scan runs |
| W4 | Windows build | cross-compiles; `baash -c 'echo hi'` runs; a simple script with `$( … )`, `for`, `if`, functions runs in a ConPTY |
| W5 | uutils embedded | curated set as built-ins behind a feature; measured size/build cost; `ls`/`cat`/`cp`/`rm`/`mkdir`/`head`/`tail`/`wc` work without external binaries |
| W6 | curated conformance | a "supported subset" document and a differential runner asserting *match bash or reject*, with silent divergence = 0 |

## 7. Open questions

* **`coproc` is in the ceiling but contradicts two commitments.** A coprocess's
  body is shell code that must run concurrently with the main thread; with no
  fork and with shell code pinned to the main thread, the only ways are (a)
  run the body as a re-exec'd `baash` child with serialized state, or (b) allow a
  subinterpreter thread for coproc bodies only. Neither is free. Recommendation:
  reject `coproc` with `CAPABILITY` until W4 is done, then revisit.
* **Pipelines with a builtin stage.** The plan is lastpipe semantics by default
  (`cmd | while read` runs the loop in the parent). That is a deliberate
  divergence from bash and needs a decision, because it changes variable scope
  for a very common idiom.
* **`^Z` with no job control** wedges the shell and the child together. Accept
  the bash-`set +m` behaviour, or ignore `SIGTSTP` in the shell and kill the
  stopped child? (Recommendation: accept, document, revisit with W4.)
* **Windows path spelling**: `C:/…` required, `\` remains an escape.
* **Where the completion `-F` shell function runs**: main thread, so a slow
  user completion still blocks the prompt. Acceptable, or reject `-F`?

## 8. Spike results (2026-10-30)

compio 0.19.2, a throwaway crate, real pty:

* **Raw-mode stdin**: `compio::fs::stdin()` is `AsyncFd`-backed on Unix;
  `read()` on a raw tty delivered the exact bytes typed (`a`, `bc`, `ESC [ A`),
  coalescing neighbouring bytes, with no blocking thread. This is the editor's
  input path.
* **inotify**: `inotify_init1(IN_NONBLOCK|IN_CLOEXEC)` + `inotify_add_watch` in
  ~40 lines, wrapped in `PollFd::new`, awaited with `read_ready()`; create,
  modify and delete events arrived with correct masks and names.

Both were driven through compio's own runtime; no poll loop and no extra crate
was needed. The one place this does not carry over is Windows console input
(see §8).

## 9. Risks

* **Scope leaks.** Every field of shell state that a subshell may mutate must be
  in the scope. Missing one is a silent bug that only the conformance suite (or
  a user) will find; this is the main correctness risk of W2.
* **Clone cost — measured, and bounded by state size.** A scope deep-clones
  `ShellState`, so the per-iteration cost is O(state) where fork was
  copy-on-write. In the common case that is cheap and *faster* than bash: the
  release bench has `$(echo hi)` at 7.5 ms per 1000 against bash's 170 ms, and
  `( : )` at 4.3 ms against 145 ms; a scope costs about 2.3 µs on top of the
  command. The cost is real only when the state is large — 200 subshells take
  66.6 ms with a 20 000-element array in scope against bash's 52.2 ms (and
  1.1 ms with no array). Measured 2026-10-30, and copy-on-write was considered
  and deferred: `Var.value` as an `Arc<VarValue>` would make the snapshot a
  pointer bump per variable, at the price of `Arc::make_mut` on every write.
  The trigger to revisit is a workload where large arrays and subshells mix;
  scalar-heavy scripts are already ahead of bash.
* **Conformance drop.** W1 and W2 will both lower the Oils number. The baseline
  is re-recorded each time, and the *quality* metric (silent divergence) is what
  matters.
* **Windows reality.** Cross-compilation proves it builds, not that it works;
  ConPTY and path semantics need a hand-run harness until a runner exists.
* **uutils size.** Curated set only; measure binary size and build time before
  adding a utility.
* **Runtime immaturity.** compio is pre-1.0 and its API moves. Accepted, because
  the interaction surface is small (fs, process, time, `PollFd`). One concrete
  consequence found the hard way: `default-features = false` drops `io-uring`,
  and compio then links its **stub** driver, which panics on the first
  operation ("Stub driver does not support any operations") rather than failing
  at startup. The feature is listed explicitly in the workspace manifest with a
  comment saying why.
* **`io_uring` availability.** No polling fallback by decision: this is an
  interactive shell and the owner does not care about seccomp-restricted
  sandboxes. If the driver cannot be created, startup must fail with a clear
  message rather than degrade silently — and the stub-driver panic above is
  exactly the degraded mode to avoid, so the feature list is load-bearing.
* **Windows console input.** compio documents that on Windows a console stdin
  read is a blocking read on a separate thread and cannot be cancelled. So the
  "no threads" property holds on Unix (verified) but not for Windows console
  input; W4 will use the platform's `ReadConsoleInputW` on a dedicated input
  thread, which is the standard approach there.
