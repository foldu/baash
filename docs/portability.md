# Portability: the subinterpreter profile and a Windows target

Status: **design, not built.** Written 2026-10-28. This records a direction the
owner asked for — an optional mode that never forks, so the shell can be
threaded and can run on Windows — and the parts of it that are cheap to prove
on Linux before any Windows code exists.

The motivating facts: PowerShell 7 takes seconds to start and its completion
times out; a native Rust shell starts in well under a millisecond (measured:
`baash -c :` 0.40 ms in release) and completes a 59k-entry directory in ~4 ms
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

The Windows port then becomes "same shell, different `baash-sys`, narrower
profile", instead of a fork of the codebase.

## 2. What used to depend on `fork` (historical)

Nothing does any more: `( … )`, `$( … )` and pipeline stages run in process
(`docs/direction.md` §2, W2), and `&` accepts only external commands. The table
below is kept because it is still the map of what a Windows port has to answer
for — each row is now an in-process mechanism rather than a child process, and
the "what it needs" column is what that mechanism has to reproduce:

| construct | site | what it needs from the child |
|---|---|---|
| `( … )` subshell | `baash-vm/src/tree/eval.rs`, `fork_shell` | run shell code on a copy of the state, discard changes |
| `$( … )` command substitution | same helper | run shell code, capture its stdout |
| `&` on a builtin/function, and pipeline stages that contain shell code | same helper | a process to be a background child / a stage |
| completion capture (`complete -F`-style, and `PROMPT_COMMAND`) | `baash-vm/src/tree/complete.rs`, `capture_stdout` | run shell code with stdout redirected |

Everything else about spawning already avoids `fork`: external commands go
through `posix_spawn`. Job control is gone (W1), so there is no terminal
handover, `WUNTRACED` wait or per-job process group to port.

**The key observation:** `capture_stdout` in `baash/src/main.rs` already runs
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
| `<(...)`, `>(...)` | already `BAASH-E0301` | rejected | rejected |
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
* Message shape: `baash: <file>:<line>:<col>: error[BAASH-E0302]: process
  substitution needs fork; this shell runs without it (--profile=subinterpreter)`.
* Parse-time where the construct is syntactically identifiable (`exec` in a
  subshell is *not* — it depends on runtime context; but `<( … )` is).
* Runtime otherwise, and always at the point of refusal, never "later weirdness".

The linter form matters as much as the runtime form:

```sh
baash --check --profile=windows script.sh    # on Linux: what would Windows refuse?
baash --profile=windows script.sh            # on Windows: same, enforced
baash --profile=subinterpreter               # Linux, no fork, for threading/fuzzing
```

This is how the Windows story gets tested years before the Windows build: every
rejection is a diagnostic we can assert on Linux, and the suite can carry a
"expected rejections" list per profile.

## 5. Windows: the four boundaries

The shell core (`baash-lang`, `baash-syntax`, `baash-runtime`, `baash-pattern`,
`baash-diag`, `baash-complete`, `baash-highlight`) is platform-neutral today. The port
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
   `baash-sys` (`os_bytes` / `os_from_bytes`). Non-WTF-8 input (impossible from
   Windows APIs) is a diagnostic, not lossy.
3. **Terminal.** ConPTY for the pty analogue; `SetConsoleMode` instead of
   `termios`; ANSI/VT strings work in Windows Terminal and ConPTY. `baash-repl`
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

### 5a. `rustix` stops at the Windows boundary

On Unix, descriptors, termios, ptys and child waiting go through `rustix` rather
than hand-written `libc` bindings, because it already owns the `io-safety` shape,
the `Errno` type and the per-architecture layouts. That is a Unix answer and it
does not generalise:

* `rustix::fs` is `#[cfg(not(windows))]`. File handling — open, dup, seek,
  `O_TMPFILE`, memfd, unlink — has no Windows implementation there.
* `termios` and `pty` are Unix concepts; the Windows analogues are
  `SetConsoleMode` and ConPTY, which `rustix` does not wrap.
* `rustix::process` is Unix-shaped: signals, `waitpid`, process groups. Windows
  has console control events and waitable process handles, neither of which maps
  onto those.

So `rustix` is **not** the portable layer. The Windows implementations are
separate `#[cfg(windows)]` modules that use `std` where it is enough
(`std::io::pipe`, `std::process::Command`) and `windows-sys` where it is not
(`CreateProcessW` with `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`, ConPTY,
`ReadConsoleInputW`). The dependency itself is `cfg(unix)`-gated so that a
Windows build does not carry a crate whose relevant modules vanish.

Two consequences worth deciding early, because they reach into `baash-vm`:

* **Handles are not descriptors.** `OwnedFd`, `borrowed()`, `dup2` and the
  numbered fd table above 2 are Unix affordances; Windows has inheritable
  handles and `STARTUPINFO`/`SetStdHandle`. Either `baash-sys` grows a portable
  `Redirect`/`SpawnPlan` vocabulary that both backends implement, or the fd
  table is abstracted behind a handle type. Doing this in W2/W3 while the shapes
  are still small is cheaper than discovering it during W4.
* **`FdAction` has no Windows spelling.** `Dup2`/`Close`/`Open` become
  "which handles go in the handle list, and what are their std handles" —
  a different data structure, not a translation of the same one.

## 6. uutils as built-ins

The point is not "coreutils exist on Windows" (uutils already ships binaries).
The point is **not spawning** for the commands people loop over. A shell that
spawns per command loses to one that does not: 1,000 `wc` calls at ~2–5 ms of
`CreateProcess` is seconds; in-process it is milliseconds.

Shape:

* `crates/baash-uutils`, **feature-gated** (`--features coreutils`), the only
  crate besides `baash-sys` allowed `unsafe` (uutils brings its own), built-in
  bodies calling uutils' `uumain(args) -> UResult`.
* A thin fd boundary: apply the shell's redirection plan to fds 0/1/2 for the
  duration of the call, then restore — the `capture_stdout` pattern, already
  proven.
* Utilities must not call `process::exit` and must not fork internally; that is
  an inclusion criterion, checked per utility before adding it.
* Alternatives considered: a single `coreutils` multicall binary shipped next
  to `baash` (simple, still one spawn per command) or a hybrid (embed the hot
  dozen, spawn the rest).

**Spike before committing:** embed `cat` and `wc` behind the feature; measure
binary size delta, build-time delta, and per-call cost against spawning the same
uutils binary. If it is ~5 ms saved and +1 MB, expand; if clap costs 8 MB and
40 s of compile per utility, embed the hot set and ship the multicall binary.

### Built-in gaps on Windows

The shell's own bodies (`printf`, `test`/`[`, `cd`, `read`, `ulimit`, …) are
written against `std` and `baash-sys`, so they build and run on Windows without a
separate code path. Two of them necessarily approximate a Unix answer there:

* `printf '%(…)T'` has no IANA timezone database behind the Windows C library.
  The common `strftime` specifiers are formatted in UTC and `TZ` is ignored;
  `%Z`/`%z` report `UTC`/`+0000`.
* `test -x` reports existence (Windows has no execute bit), `-w` follows the
  read-only attribute, and `-O`/`-G`, `-u`/`-g`/`-k`, `-nt`/`-ot`/`-ef` and the
  socket/character/block/FIFO tests answer false because the metadata is not
  there to inspect.

Both are deliberate approximations, not silent wrongness: a script that needs
the Unix answer gets a different value rather than a misleading success.

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
   the constructs marked "in-process" above. Hard cut, no profile switch: fork
   goes away entirely for shell code, and external commands keep using
   `posix_spawn`. This is the same change that makes helper threads legal, so it
   doubles as the answer to the completion-worker question.
3. **Windows shell:** `baash-sys` platform layer behind the portable spawn and
   redirect vocabulary described in §5a, then a `windows` profile that rejects
   the rest. Settle that vocabulary while the shapes are small, not during the
   port.
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

## 10. Building and running for Windows today (2026-10-30)

Cross-compilation works and the result runs under Wine; the tree does not build
yet, and this section exists so the toolchain is not rediscovered each time.

The host toolchain has no Windows std, and its `rustup` home is read-only, so a
private toolchain in `/tmp` is the one that carries the target:

```sh
export RUSTUP_HOME=/tmp/baash-rustup CARGO_HOME=/tmp/baash-cargo-home
rustup toolchain install stable-x86_64-unknown-linux-gnu --profile minimal \
    --target x86_64-pc-windows-gnu
```

Linking goes through `zig` via `cargo-zigbuild` (no mingw sysroot needed). The
important detail is PATH: the rustup toolchain must come *first*, so `cargo` is
the one that has the target, while `cargo-zigbuild` and `zig` come from the nix
shell and stay reachable *after* it. `RUSTFLAGS` must be unset — the workspace's
mold workaround is a Linux linker flag and breaks the cross build.

```sh
TC=$(ls -d /tmp/baash-rustup/toolchains/*/bin | head -1)
export CARGO_TARGET_DIR=/tmp/baash-target-win
unset RUSTFLAGS RUSTDOCFLAGS
nix shell nixpkgs#cargo-zigbuild nixpkgs#zig --command \
    sh -c "PATH=$TC:\$PATH cargo zigbuild --target x86_64-pc-windows-gnu -p baash"
```

Wine needs a prefix that it owns; `/tmp` is rejected, so use a directory under
the user's home:

```sh
export WINEPREFIX=$HOME/.cache/baash-wine WINEDEBUG=-all
nix shell nixpkgs#wine64 --command wine target/x86_64-pc-windows-gnu/debug/whello.exe
```

(`wine: failed to start L"C:\\windows\\syswow64\\rundll32.exe"` is noise from a
32-bit component the 64-bit prefix does not have; the binary still runs.)

### What fails, measured

`cargo zigbuild --target x86_64-pc-windows-gnu -p baash` stops on Unix-only APIs,
in this order:

| crate | `std::os::unix` / `std::os::fd` / `rustix` references |
|---|---|
| `baash-sys` | 61 |
| `baash-vm` | 24 |
| `baash-lang` | 3 |
| `baash-repl` | 3 |
| `baash` | 4 |

`baash-sys` was the wall: it is fd-shaped (`RawFd`, `dup2`, `OwnedFd`) and the
redirection model is built on `dup2`; it now has a CRT backend and
cross-compiles, as do `baash-lang`, `baash-runtime` and `baash-history`. `baash-vm` is
next and its failures are the same three shapes: `OsStrExt` byte conversions,
`std::os::fd` imports, and `baash_sys::Fd` (no `as_raw_fd`/`From<Fd> for File` on
the Windows side yet), plus `baash_sys::Regex`, `SpawnPlan::new_with_env`, the
`SignalSet`/`NoEcho` helpers and two `PermissionsExt::mode` calls.

### Order of work

1. **`baash-sys` platform split.** `mod unix; mod windows;` behind `cfg`, with one
   public API. The Windows side keeps the fd *number* model by delegating to the
   CRT (`_dup2`, `_read`, `_write`, `_open`, `_close`), so redirection code does
   not change shape; `CreateProcess` replaces `posix_spawn`, with the parent's
   CRT fds converted to inheritable handles.
2. **The ten non-`baash-sys` references.** A small `baash_sys::os` surface for
   "path bytes ↔ `OsString`" and "file mode", so no other crate names
   `std::os::unix`.
3. **`-c 'echo hi'` under Wine**, then a script with `$( … )`, `for`, `if` and a
   function — the W4 acceptance in `docs/direction.md` §6.
4. **ConPTY** last: the interactive editor needs a console host, and until then
   the REPL should report `BAASH-E0302` rather than misbehave.

The `Fs` column of §5a still applies: `rustix` stops at the Windows boundary,
which is why step 1 is a platform module rather than a `cfg` sprinkled through
`fd.rs`.

### Turso on Windows: verified (2026-10-30)

`turso = "0.8"` (0.8.3, the latest stable — `cargo add` picks it over the
`0.9.0-pre.2` pre-release) builds for `x86_64-pc-windows-gnu` with the recipe
above
and **runs under Wine**: `CREATE TABLE`, `INSERT`, `SELECT` and a row read back
all succeed. Its API is async (`Builder::new_local(path).build().await`,
`db.connect()`, `conn.execute(..).await`, `rows.next().await`), which is what a
compio-driven shell wants, and its I/O backends include io_uring and IOCP, the
same two runtimes the shell needs. Both Linux and the Wine run were verified with
0.8.3.

A `turso` limitation that shaped the schema: `id INTEGER PRIMARY KEY
AUTOINCREMENT` is accepted and keeps a high-water mark in memory (a delete then
insert gives the next id), but with a **file** database reopened in
multiprocess-WAL mode it does not: after `history -c`, the next insert reused
id 1. Measuring that is why `baash-history` never deletes a row whose id it has
shown — it tombstones it and prunes only rows below the newest id (see
`docs/contracts.md`).

Two shells share one history file, so the connection is opened with
`experimental_multiprocess_wal(true)` plus a five-second `busy_timeout`: without
the first, the second `Builder::build()` on the same path fails with "database
is locked" even when the first is idle; without the second, a write that lands
while the other shell is writing fails instead of waiting. Both were verified
in one process (two stores on one file, each seeing the other's rows), and the
mode is refused for `:memory:`, which takes the plain builder.

The runtime behaviour is verified on Linux; for Windows, `baash-history` (and
therefore turso with both of those options) cross-compiles for
`x86_64-pc-windows-gnu`, but the database has not been executed under Wine yet
— `baash` itself does not cross-build until `baash-vm` does. `baash-runtime` also
cross-compiles now, because deleting the history file removed its last
unix-only code.

One Windows build detail: `turso_sdk_kit`'s build script links a version
resource by invoking plain `windres`, which `cargo-zigbuild` does not provide.
Put a wrapper *script* (not a symlink — nix-wrapped binutils resolve their own
directory from `argv[0]`, so a symlink breaks them) on `PATH`:

```sh
MINGW_BIN=$(dirname "$(nix shell nixpkgs#pkgsCross.mingwW64.stdenv.cc \
    --command sh -c 'command -v x86_64-w64-mingw32-windres')")
printf '#!/bin/sh\nexec env PATH=%s:$PATH %s/x86_64-w64-mingw32-windres "$@"\n' \
    "$MINGW_BIN" "$MINGW_BIN" > "$HOME/.cache/baash-wbin/windres"
chmod +x "$HOME/.cache/baash-wbin/windres"
# then PATH=$HOME/.cache/baash-wbin:$TC:$PATH cargo zigbuild --target x86_64-pc-windows-gnu
```

The wrapper has to prepend the MinGW `bin` to `PATH`, not just `exec` the one
binary: `windres` shells out to `x86_64-w64-mingw32-gcc` as its preprocessor,
and `cargo-zigbuild`'s own toolchain does not contain one, so a bare wrapper
fails with `x86_64-w64-mingw32-gcc: command not found`.
