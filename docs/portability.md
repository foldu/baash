# Portability: the subinterpreter profile and a Windows target

Status: **design, not built.** Written 2026-10-28. This records a direction the
owner asked for — an optional mode that never forks, so the shell can be
threaded and can run on Windows — and the parts of it that are cheap to prove
on Linux before any Windows code exists.

The motivating facts: PowerShell 7 takes seconds to start and its completion
times out; a native Rust shell starts in well under a millisecond (measured:
`bsh -c :` 0.40 ms in release) and completes a 59k-entry directory in ~4 ms
in-process. The gap is worth closing on Windows, where "a fast shell with tab
completion" is currently a niche filled by Git Bash and WSL.

---

## 1. What "subinterpreter only" means

Two axes, deliberately orthogonal:

| axis | values | decided by |
|---|---|---|
| **execution model** | `fork` (today), `subinterpreter` | a runtime profile/flag |
| **platform** | `unix` (today), `windows` | the target + a profile |

`subinterpreter` mode never calls `fork(2)`. Every construct either works
in-process, works via `posix_spawn`/`CreateProcess`, or is **rejected with a
diagnostic**. That is the whole contract:

> **No silent wrongness.** In this mode every script either behaves as POSIX
> says, or fails with a diagnostic that names the construct and the reason. A
> construct is never "best effort".

This is measurable on Linux today with the existing suite: run all 3971 Oils
cases under `--profile=subinterpreter` and require that each case is *pass* or
*rejected with a code* — the number of silently-wrong cases must be zero. That
becomes a second ratchet, alongside the conformance one.

The Windows port then becomes "same shell, different `bsh-sys`, narrower
profile", instead of a fork of the codebase.

## 2. What actually depends on `fork` today

Four call sites, all through `bsh_sys::fork_child`/`fork_shell` (which is why
the dependency is narrow in code and deep in semantics):

| construct | site | what it needs from the child |
|---|---|---|
| `( … )` subshell | `bsh-vm/src/tree/eval.rs`, `fork_shell` | run shell code on a copy of the state, discard changes |
| `$( … )` command substitution | same helper | run shell code, capture its stdout |
| `&` on a builtin/function, and pipeline stages that contain shell code | same helper | a process to be the job / a stage |
| completion capture (`complete -F`-style, and `PROMPT_COMMAND`) | `bsh-vm/src/tree/complete.rs`, `capture_stdout` | run shell code with stdout redirected |

Everything else about spawning already avoids `fork`: external commands go
through `posix_spawn`, and `bsh-sys` already has the pieces job control needed
(`ProcessGroup::SetPgid`, `wait_pid(wuntraced)`, `set_foreground_pgrp`).

**The key observation:** `capture_stdout` in `bsh/src/main.rs` already runs
shell code *in this process* with fd 1 temporarily `dup2`'d to a seekable temp
file, then restores it. That is an in-process command substitution. The
"subinterpreter" is not new machinery — it is the generalisation of a pattern
we already use, plus a state scope.

## 3. Construct-by-construct matrix

| construct | `fork` (today) | `subinterpreter` | Windows |
|---|---|---|---|
| external command | `posix_spawn` | same | `CreateProcessW` |
| `( cd /tmp; ls )` | fork | **in-process scope**: push var/func/option/trap/fd/cwd/umask scope, run, pop | same |
| `x=$(cmd)` | fork | **in-process capture** (fd 1 → temp/pipe), then pop the scope | same |
| `( exec foo )` | fork + exec | **rejected** — `exec` would replace the shell | rejected |
| `builtin \| builtin`, `fn \| fn` | fork per stage | **rejected** (needs two runtimes at once; `lastpipe` changes semantics) | rejected |
| `sleep 1 &` | `posix_spawn` (already) | same | same |
| `f &` (function or builtin) | fork | **rejected** (a job needs a pid and a signal target) | rejected |
| `${ … } &` | fork | rejected | rejected |
| `jobs`/`fg`/`bg`/`%1` | process groups | **rejected** (no per-job pid to signal) | rejected; Job Objects could later give suspend/resume, not `SIGTSTP` |
| `set -m` | process groups | accepted but inert, or rejected — decide | rejected |
| `^Z` | `SIGTSTP` | no-op (nothing can be suspended) | no-op |
| `exec cmd` | `execve` | **rejected** | rejected (no `exec` on Windows) |
| `exec 3<file` (redirection only) | `dup2` in-process | **works** — it is our fd table | works |
| `<(...)`, `>(...)` | already `BSH-E0301` | rejected | rejected |
| `$$` | parent pid | same value (this is *easier* in-process) | same |
| `$BASHPID` | child pid | synthesised id, or rejected | same |
| `trap INT` on a background job | signal to pgroup | rejected for shell-code jobs | coarse console events |
| `kill %1` | pgroup signal | rejected for in-process jobs | `TerminateProcess` only |
| `umask` | syscall | works | **no-op** (ACLs); decide whether to accept silently or warn |

Two entries are policy choices rather than facts, and they should be decided
before the mode ships:

* **`set -m`**: accept and ignore, or reject? Ignoring is friendlier to rc
  files; rejecting is honest. Recommendation: accept, and make `jobs`/`fg`/`bg`
  the things that report "not available in this profile".
* **`$BASHPID`**: synthesise (a counter) or reject? Synthesising keeps
  `[[ $BASHPID != $$ ]]` tests working; it lies to `/proc`-style scripts we
  cannot run anyway. Recommendation: synthesise.

## 4. Rejection must be a first-class diagnostic

The mechanism already exists: one superset parse, then post-parse gating (built
for POSIX portability). A profile is a second gating table over the same AST.

* New code, next to `UNSUPPORTED = 301`: **`CAPABILITY = 302`**, "this construct
  needs X, and the current profile does not have it".
* Message shape: `bsh: <file>:<line>:<col>: error[BSH-E0302]: process
  substitution needs fork; this shell runs without it (--profile=subinterpreter)`.
* Parse-time where the construct is syntactically identifiable (`exec` in a
  subshell is *not* — it depends on runtime context; but `<( … )` is).
* Runtime otherwise, and always at the point of refusal, never "later weirdness".

The linter form matters as much as the runtime form:

```sh
bsh --check --profile=windows script.sh    # on Linux: what would Windows refuse?
bsh --profile=windows script.sh            # on Windows: same, enforced
bsh --profile=subinterpreter               # Linux, no fork, for threading/fuzzing
```

This is how the Windows story gets tested years before the Windows build: every
rejection is a diagnostic we can assert on Linux, and the suite can carry a
"expected rejections" list per profile.

## 5. Windows: the four boundaries

The shell core (`bsh-lang`, `bsh-syntax`, `bsh-runtime`, `bsh-pattern`,
`bsh-diag`, `bsh-complete`, `bsh-highlight`) is platform-neutral today. The port
work is concentrated and can be described as four boundaries:

1. **Process creation.** `SpawnPlan`/`FdAction` map to `CreateProcessW` with
   `STARTUPINFOEX` + `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` (explicit handle
   inheritance), or `bInheritHandles` with everything else non-inheritable.
   `PATH` search needs `PATHEXT` and the "no exec bit" rule: an extension in
   `PATHEXT` *is* the execute permission.
2. **Bytes vs UTF-16.** File contents and pipes stay bytes. Four things are
   UTF-16: directory enumeration, argv, the environment block, and the console.
   Policy: the shell's text is WTF-8 (UTF-8 plus lone-surrogate encoding), so
   every name round-trips; the ~25 `OsStrExt::as_bytes` sites become a codec in
   `bsh-sys` (`os_bytes` / `os_from_bytes`). Non-WTF-8 input (impossible from
   Windows APIs) is a diagnostic, not lossy.
3. **Terminal.** ConPTY for the pty analogue; `SetConsoleMode` instead of
   `termios`; ANSI/VT strings work in Windows Terminal and ConPTY. `bsh-repl`
   keeps its renderer; its `term` layer gets a Windows implementation.
4. **Job control and signals.** No process groups, no `SIGTSTP`, no
   `tcsetpgrp`. `Ctrl-C` is a console control event delivered to the console
   process group — coarse. Job Objects give grouping and terminate/suspend but
   not POSIX semantics. `fg`/`bg`/`^Z` are rejected; `jobs` for background
   externals is possible later.

Also needed, and easy to forget: no shebang (`#!` scripts must be detected and
run by us), `NUL` instead of `/dev/null` (we can provide the virtual paths
`/dev/null` and `/dev/tty`), CRLF in script files (strip a trailing `\r` before
the lexer's newline, script files only), case-insensitive completion and
globbing, drive letters and UNC paths in the parser's "is this a path" logic,
and `\` remaining an escape — so Windows paths are written `C:/Users/...` or
quoted, as msys does.

## 6. uutils as built-ins

The point is not "coreutils exist on Windows" (uutils already ships binaries).
The point is **not spawning** for the commands people loop over. A shell that
spawns per command loses to one that does not: 1,000 `wc` calls at ~2–5 ms of
`CreateProcess` is seconds; in-process it is milliseconds.

Shape:

* `crates/bsh-uutils`, **feature-gated** (`--features coreutils`), the only
  crate besides `bsh-sys` allowed `unsafe` (uutils brings its own), built-in
  bodies calling uutils' `uumain(args) -> UResult`.
* A thin fd boundary: apply the shell's redirection plan to fds 0/1/2 for the
  duration of the call, then restore — the `capture_stdout` pattern, already
  proven.
* Utilities must not call `process::exit` and must not fork internally; that is
  an inclusion criterion, checked per utility before adding it.
* Alternatives considered: a single `coreutils` multicall binary shipped next
  to `bsh` (simple, still one spawn per command) or a hybrid (embed the hot
  dozen, spawn the rest).

**Spike before committing:** embed `cat` and `wc` behind the feature; measure
binary size delta, build-time delta, and per-call cost against spawning the same
uutils binary. If it is ~5 ms saved and +1 MB, expand; if clap costs 8 MB and
40 s of compile per utility, embed the hot set and ship the multicall binary.

## 7. What this is not

* **Not Cygwin-style fork emulation.** Replaying a process image to fake `fork`
  is the thing that makes Cygwin's model expensive and subtle. We reject
  instead, and only implement what can be done honestly.
* **Not a second grammar.** One superset parse, one AST, two gating tables.
* **Not a promise that POSIX scripts run on Windows.** It is a promise that they
  either run or say why, and that interactive use is fast and native.

## 8. Order of work

1. **Now (cheap, Linux-only):** define the profile enum + `CAPABILITY` code +
   `--check --profile=…`; add the rejection table for the constructs above;
   measure "no silent wrongness" over the Oils corpus.
2. **Before Windows:** replace the four `fork` sites with in-process scopes for
   the constructs marked "in-process" above; keep fork as the default profile
   until the suite is green in both modes. This is the same change that makes
   helper threads legal, so it doubles as the answer to the completion-worker
   question.
3. **Windows shell:** `bsh-sys` platform layer behind traits (spawn, terminal,
   jobs, paths), then a `windows` profile that rejects the rest.
4. **uutils:** the two-utility spike, then expand by measurement.

The live prompt ([`prompt-watch.md`](prompt-watch.md)) is the first feature that
looks like it needs a helper thread and deliberately does not: it polls two file
descriptors instead. It is the test case for "is this profile actually good
enough to live without `fork`", because it wants to run while the user is idle
and would otherwise hold the fork gate open.

## 9. Open questions

* Should `subinterpreter` mode be able to fall back to `fork` for the constructs
  it does not emulate (a third "hybrid" profile: no raw fork for externals, fork
  only for subshells)? That would make threading unsafe again, so probably no.
* What is `$BASHPID` in-process — a counter that never collides with a real pid,
  or the shell's pid with a subshell id appended? (No: `$$` must stay the pid.)
* Does `trap RETURN`/`DEBUG` interact with the in-process scope boundary?
  Probably yes, and the scope push/pop is the natural hook.
* CRLF: strip in the file reader, or teach the lexer? Reader is simpler and
  keeps the lexer byte-faithful.
