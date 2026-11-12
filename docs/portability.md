# Portability: no fork, and a Windows target

Status: **landed.** One execution model, two supported platforms. This file
records the model, what each platform does with it, the diagnostics that stand
in for what is missing, and the measurements behind each decision. The dated
sections are observations from the host or the guest, kept because they are the
evidence; §10 is the build shape, and [windows.md](windows.md) is the day-to-day
protocol.

The motivating facts: PowerShell 7 takes seconds to start and its completion
times out; a native Rust shell starts in well under a millisecond (measured:
`baash -c :` 0.40 ms in release) and completes a 59k-entry directory in ~4 ms
in-process. The gap is worth closing on Windows, where "a fast shell with tab
completion" is currently a niche filled by Git Bash and WSL.

---

## 1. The one execution model

The shell never calls `fork` for shell code. Every construct either works in
process, works through `posix_spawn`/`CreateProcessW`, or is **rejected with a
diagnostic**:

> **No silent wrongness.** Every script either behaves as POSIX says, or fails
> with a diagnostic that names the construct and the reason. A construct is
> never "best effort".

That contract is what made the rest possible: with no second copy of the process
there is nothing to be unsafe about, so helper threads are legal (the completion
worker, the prompt watcher); the state a subshell can change is enumerated and
restored instead of copy-on-written (`baash-vm/src/tree/scope.rs`); and Windows
is the same shell over a different `baash-sys`, not a port of a forking program.

There is no profile flag. `docs/direction.md` §2 chose a hard cut, and the fork
path was deleted rather than kept behind a switch. What is left of `fork(2)` in
the tree is one test helper, `spawn::fork_child_status`, which the pty harness
needs to start a shell in its own session; `spawn` also keeps an explicit
"would this plan need the fallback?" predicate that today never fires, because
every descriptor action maps to `posix_spawn_file_actions_*`.

The cost is the rejection list: a construct whose POSIX meaning is a second
process cannot be emulated honestly in one, so it says so (§4).

## 2. What used to depend on `fork`

`( … )`, `$( … )`, pipeline stages that are shell code and completion capture
all ran as children. Each is an in-process mechanism now; the table is kept
because it is the map of what the port had to answer for:

| construct | then | now |
|---|---|---|
| `( … )` subshell | fork | a scope: snapshot the shell state, run, restore (`baash-vm/src/tree/scope.rs`) |
| `$( … )` command substitution | fork + capture | the same scope with fd 1 on a pipe |
| a pipeline stage that is shell code | fork per stage | in this process; a non-final stage's output goes to an unlinked temporary file, since its writer is this process and would block against its own read end |
| completion capture (`-F`, `PROMPT_COMMAND`) | fork | `capture_stdout`, in this process |

Everything else about spawning already avoided `fork`: external commands go
through `posix_spawn` (`CreateProcessW` on Windows), and job control is gone, so
there is no terminal handover, `WUNTRACED` wait or per-job process group to port.

`capture_stdout` in `baash/src/main.rs` was the prototype: it already ran shell
code in this process with fd 1 on a seekable temporary file. The subshell scope
is that pattern generalised, plus the state snapshot.

## 3. Construct by construct

The authoritative list of what is rejected, and why, is `docs/direction.md` §4.
This is the platform view of the same constructs.

| construct | Unix | Windows |
|---|---|---|
| external command | `posix_spawn` | `CreateProcessW` |
| `( … )`, `$( … )`, `cmd \| while read` | in-process scope | same |
| `( exec cmd )` | runs, then ends the scope | `exec` fails (`E0205`): there is no image to replace |
| `cmd &`, external | real child, remembered by pid | same |
| `f &`, `{ … } &` | rejected (`E0302`) | rejected |
| `jobs`, `fg`, `bg`, `disown`, `%1` | rejected (`E0302`) | rejected |
| `exec 3<file` | the fd table | the CRT fd table |
| `<(...)`, `>(...)` | rejected (`E0301`) | rejected |
| `$$` | the shell's pid, also inside `( … )` | same |
| `$BASHPID` | **unset** — see §8 | same |
| `umask` | process mask | rejected: no mask on Windows |
| `set -m` | accepted, inert | same |
| `^Z` | no job control: the child stops with the shell | `0x1a`, end-of-input at the prompt |
| `trap INT` | signals | console control events, coarse (§9) |
| `kill` | signals, `TerminateProcess` for externals | `TerminateProcess` only |

Two rows are policy rather than mechanism, and both went the same way: `set -m`
is accepted and inert so an rc file does not break, and `jobs`/`fg`/`bg` stay
*registered* so `type` finds them, failing when run instead of reporting
"command not found".

## 4. Rejection is a first-class diagnostic

Two codes carry it, from one gating table over the AST (the same mechanism the
POSIX superset uses):

* **`BAASH-E0301`** (`UNSUPPORTED`) — a construct the shell has no mechanism
  for at all: process substitution, which needs `/dev/fd`.
* **`BAASH-E0302`** (`CAPABILITY`) — a construct whose POSIX meaning is a
  second process or a job table: `f &`, `{ … } &`, `jobs`, `fg`, `bg`,
  `disown`, and `%1` in `wait`/`kill`.

(`exec` on Windows is a third: `BAASH-E0205`, the `exec` failure code, because
it is a named command that cannot do its job rather than a construct the shell
has no mechanism for.)

Each names the construct and the reason, at the point of refusal:

```
$ baash -c 'f() { echo fn; }; f &'
baash:1:19: error[BAASH-E0302]: &: f is not an external command, and shell code cannot run in the background
$ baash -c 'jobs'
baash:1:1: error[BAASH-E0302]: jobs: job control is not implemented in this shell
$ baash -c 'echo <(echo hi)'
baash:1:6: error[BAASH-E0301]: unsupported expansion: process substitution
```

The status is 1, the same failure any command returns, so `if` and `&&` behave
as they would around a failing command. `jobs`, `fg`, `bg` and `disown` are
registered built-ins that fail when run, because `type jobs` must still answer
and an rc file must not see "command not found".

Where the construct is syntactically identifiable the refusal is at parse time;
where it is not — `exec` depends on runtime context — it is at the point of use,
never later weirdness. `crates/baash/tests/execute.rs` asserts on the code and
the message for each of them.

## 5. Windows: the four boundaries

The shell core (`baash-lang`, `baash-syntax`, `baash-runtime`, `baash-pattern`,
`baash-diag`, `baash-complete`, `baash-highlight`) is platform-neutral. The port
was concentrated in four boundaries:

1. **Process creation.** `SpawnPlan`/`FdAction` map to `CreateProcessW` with
   `STARTUPINFOEX` + `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` (explicit handle
   inheritance), or `bInheritHandles` with everything else non-inheritable.
   `PATH` search needs `PATHEXT` and the "no exec bit" rule: an extension in
   `PATHEXT` *is* the execute permission.
2. **Bytes vs UTF-16.** File contents and pipes stay bytes. Four things are
   UTF-16: directory enumeration, argv, the environment block, and the console.
   The shell's text is **WTF-8** (`baash_sys::wtf8`), so every name
   round-trips: `bytes_from_os`/`os_from_bytes` combine a surrogate pair into
   the four-byte form and keep a lone surrogate as its three-byte one. That is
   what makes an NTFS name no `String` can hold addressable — `cd`, `rm`, a
   variable, a `PWD` and a history entry all keep it, and a child receives it
   unchanged. The one lossy direction is bytes that are not WTF-8 at all: they
   cannot name anything on Windows, so `os_from_bytes` substitutes U+FFFD
   rather than inventing an escape. *Display* stays lossy on purpose — a lone
   surrogate paints as U+FFFD in the editor and matches as its bytes in
   completion, while the inserted bytes are exact (`DESIGN.md` §4.10).
3. **Terminal.** The editor talks to the console it was started on —
   `SetConsoleMode` instead of `termios`, `ReadConsoleW`/`WriteConsoleW` for
   bytes — because a shell that was started in a console already has one;
   ConPTY is for hosting *another* program (the pty analogue). ANSI/VT strings
   work in Windows Terminal and under ConPTY, and `ENABLE_VIRTUAL_TERMINAL_INPUT`
   makes the console deliver the escape sequences `key.rs` already decoded, so
   `baash-repl` keeps its renderer: only its `term` layer has a Windows
   implementation (`console.rs`).
4. **Job control and signals.** No process groups, no `SIGTSTP`, no
   `tcsetpgrp`. `Ctrl-C` is a console control event delivered to the console
   process group — coarse. Job Objects give grouping and terminate/suspend but
   not POSIX semantics. `fg`/`bg`/`^Z` are rejected; a background external is a
   real child remembered by pid, so `$!`, `wait` and `kill` work.

Also needed, and easy to forget: no shebang (`#!` scripts must be detected and
run by us), `NUL` instead of `/dev/null` (done: `open_cloexec` translates it
through `paths::Flavour::host_device`, and `/dev/tty` is still a POSIX path),
CRLF in script files (a trailing `\r` is dropped before the lexer's newline),
case-insensitive completion and globbing, drive letters and UNC paths in the
parser's "is this a path" logic, and `\` remaining an escape — so Windows paths
are written `C:/Users/...` or quoted, as msys does.

The bash-completion library is the other thing the binary has to carry, and it
carries it without the disk, by the mechanism that makes the binary
self-sufficient: it is compiled into the executable
(`crates/baash/src/bash_completion.rs`) and **mounted in memory** at
`/__baash__/bash-completion` for the first interactive start, so a Windows
machine that never had bash gets the same `complete -F` functions its scripts
were written for. The path is handed to the startup file with forward slashes,
because that is how a Windows path in a shell word is written. Nothing
is written to disk: an earlier design unpacked the tree into `%LOCALAPPDATA%`
and paid 475 ms on the first start for it — Defender reads every one of 1106
files — which is what a shell that promises no lag cannot afford. The tree's
576 version aliases (`gcc-5` → `gcc`, `python3.13` → `python3`) are symlinks in
nixpkgs' copy; the mount has no symlinks at all, and every alias is its own
path with the target's bytes.

What is still outside the binary is the completion a tool ships for itself
(git, docker, kubectl, systemd): bash-completion never contained those on any
platform, so they complete only when `BASH_COMPLETION_COMPAT_DIR` or
`BASH_COMPLETION_USER_DIR` names the directory the tool installed them in.

Measured on the Windows 11 guest: the executable mounts the tree without
touching the disk, the framework sources (`_comp_load` and the v2
`_comp_initialize` are defined), an embedded script registers its rule
(`complete -F _comp_cmd_tar tar`), and a user's `~/.bash_completion` — which is
a single file, so it survives the library's `:`-separated directory lookups
that a drive letter breaks — is sourced at startup and its function returns
candidates through `COMPREPLY`. That needed `/dev/null` to work, which it now
does (`>`, `>>` and `<` all reach the `NUL` device), because 556 of the 1097
scripts redirect to it and the framework does 62 times.

`[[ … =~ … ]]` used to be the remaining shell gap here: Windows has no
`regcomp`, so it reported `BAASH-E0204` and the 46 scripts that use it —
`tar`'s option parser among them — printed a diagnostic at Tab. It now runs on
[MinRX](../crates/baash-sys/src/regex/minrx.rs), a small POSIX ERE engine
compiled from vendored C, which is the default engine here (`Engine::Minrx`;
`docs/contracts.md`). Verified on the guest with the cross-built `baash.exe`:
`tar`'s `_longopt` ERE matches with `BASH_REMATCH` filled, a capture pattern
returns `file|some/path`, the stray-`)` pattern bash-completion produces
compiles, and `--regex-engine system` is a usage error (exit 2) because there
is no platform engine to ask for.

It is not glibc, so the measured differences apply on this platform: a
backreference is a literal `1`, `{,n}` is rejected, and a nested-capture
tie-break can report a different (still valid) extent. `docs/contracts.md`
records the 234-case measurement; no bash-completion pattern differs except one
capture slot.

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

Two consequences reached into `baash-vm` and shaped the design:

* **Handles are not descriptors.** `OwnedFd`, `borrowed()`, `dup2` and the
  numbered fd table above 2 are Unix affordances; Windows has inheritable
  handles and `STARTUPINFO`/`SetStdHandle`. The answer was the first of the two
  options: `SpawnPlan`/`FdAction` is the portable vocabulary, implemented twice
  (`posix_spawn` and `CreateProcessW`), while the Windows side keeps the fd
  *number* model by delegating to the CRT. The redirection code did not have to
  change shape.
* **`FdAction` has no Windows spelling.** `Dup2`/`Close`/`Open` become
  "which handles go in the handle list, and what are their std handles" —
  a different data structure, not a translation of the same one.

## 6. uutils as built-ins

The point is not "coreutils exist on Windows" (uutils already ships binaries).
The point is **not spawning** for the commands people loop over: 1,000 `wc`
calls at ~2–5 ms of `CreateProcess` is seconds, and in-process it is
milliseconds. On Windows there is no other coreutils to spawn anyway.

What exists:

* `crates/baash-utils` links the individual uutils binaries it uses — `cat`,
  `mkdir`, `rm`, `ls`, `wc`, `head`, `tail` — each a small library with a
  `uumain(args)` entry point, not the umbrella crate. Feature-gated as
  `builtin-utils`: on for Windows, off by default on Linux, where the system
  coreutils are there and `--features builtin-utils` embeds them anyway.
* The fd boundary is the shell's: the redirection plan is applied to 0/1/2 for
  the duration of the call and restored afterwards — the `capture_stdout`
  pattern, already proven.
* Inclusion criteria, checked per utility: no `process::exit`, no internal fork,
  and a body that writes to a descriptor rather than opening its own files for
  its main job.
* Rejected on measurement, not taste: a multicall `coreutils` binary next to
  `baash` is simple, but still one spawn per command, which is the cost this
  exists to remove.

The set is curated, not complete. The shell's own bodies (`printf`, `test`,
`cd`, `read`, `ulimit`, …) are written against `std` and `baash-sys` and need no
uutils at all.

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

## 8. What is left

Known gaps, stated so they are not mistaken for behaviour:

* **`$BASHPID` is unset.** `$$` is the shell's pid and stays the same inside
  `( … )` (bash does that too), but the variable bash fills with the subshell's
  own pid does not exist, so `[ "$BASHPID" != "$$" ]` is a test of `""`.
* **`exec cmd` fails on Windows**, with `E0205` and "there is no exec on
  Windows", rather than a spawn-and-exit approximation: the pid would change and
  the redirects would not carry.
* **`Ctrl-C` is coarse on Windows.** The console handler returns `TRUE` for the
  event, so a `Ctrl-C` during a built-in loop does nothing until it returns, and
  `trap INT` never runs for a console event (§9, "Console handles").
* **`umask` is rejected on Windows**, since there is no mask to set.
* **`test` answers from Windows metadata:** `-x` is existence, `-w` the
  read-only attribute, and `-O`/`-G`, `-u`/`-g`/`-k`, `-nt`/`-ot`/`-ef` and the
  file-type tests are false (§6, "Built-in gaps").
* **`printf '%(…)T'`** formats in UTC and ignores `TZ`, because the Windows C
  library has no IANA database behind it.
* **`coproc` is not in the grammar.** `coproc NAME { … }` is a parse error
  (`E0003`) and `coproc cmd` is looked up as a command, so it is the one
  construct in the "up to and including `coproc`" ceiling
  (`docs/direction.md` §7) the shell has not taken on.
* **`^Z`** has no job table to stop a job against; the child stops with the
  shell.
* **The terminal protocol is unverified on a Windows guest.** The cwd report
  writes `file://HOST/C:/…` and ConEmu's `OSC 9;9`, but no Windows terminal has
  been observed consuming either; a UNC, drive-relative or drive-rooted path
  yields no report rather than a guessed one, and `TERM=dumb` suppresses every
  announcement. The console also has no bracketed-paste protocol: a paste
  arrives as keystrokes, so the editor's `\x1b[200~` handling is never reached
  there, and no focus sequence or cursor-position report, so `?1004` is not
  written and the prompt keeps anchoring at column 0 after output that did not
  end with a newline (`docs/terminal.md` §2, §5, §6).

Conformance is the other backlog, and it is measured: `docs/state.md` §6 and
`docs/roadmap.md` carry the per-file picture, and `crates/baash-spec/baseline.txt`
is the number that must not drop.

## 9. Measured: the port, the console and the path model

Dated, and kept because it is the evidence: what stopped the first build, the
order the port happened in, and then the parts that only exist on Windows — the
console host, console handles and `Ctrl-C`, command resolution, the path model —
plus Turso. The current protocol for building and running is
[windows.md](windows.md).

### What fails, measured

The first attempt was `cargo zigbuild --target x86_64-pc-windows-gnu -p baash`
against a private toolchain in `/tmp`, because the host had no Windows std and
its `rustup` home was read-only. Wine could run `-c` commands but has no usable
console, so it was ever only a smoke test; the console path needed the guest. It
stopped on Unix-only APIs, in this order:

| crate | `std::os::unix` / `std::os::fd` / `rustix` references |
|---|---|
| `baash-sys` | 61 |
| `baash-vm` | 24 |
| `baash-lang` | 3 |
| `baash-repl` | 3 |
| `baash` | 4 |

`baash-sys` was the wall: it is fd-shaped (`RawFd`, `dup2`, `OwnedFd`) and the
redirection model is built on `dup2`; it now has a CRT backend and
cross-compiles, as do `baash-lang`, `baash-runtime` and `baash-history`. `baash-vm` was
next, and its failures were the same three shapes: `OsStrExt` byte conversions,
`std::os::fd` imports, and `baash_sys::Fd` (no `as_raw_fd`/`From<Fd> for File` on the
Windows side then), plus `baash_sys::Regex`, `SpawnPlan::new_with_env`, the
`SignalSet`/`NoEcho` helpers and two `PermissionsExt::mode` calls.

### The port, in the order it happened

1. **`baash-sys` platform split**: `mod unix; mod windows;` behind `cfg`, one
   public API. The Windows side keeps the fd *number* model by delegating to the
   CRT (`_dup2`, `_read`, `_write`, `_open`, `_close`), so redirection code did
   not change shape; `CreateProcessW` replaced `posix_spawn`, with the parent's
   CRT fds converted to inheritable handles.
2. **The ten non-`baash-sys` references** to `std::os::unix`: a small
   `baash_sys::os` surface for "path bytes ↔ `OsString`" and "file mode", so no
   other crate names it.
3. **`-c 'echo hi'` under Wine**, then a script with `$( … )`, `for`, `if` and a
   function — the W4 acceptance in `docs/direction.md` §6.
4. **The console host** (2026-11-02), the section below.

§5a is why step 1 was a platform module rather than `cfg` sprinkled through
`fd.rs`: `rustix` stops at the Windows boundary.

### The console host (2026-11-02)

The interactive editor works on Windows without ConPTY. ConPTY is for hosting
*another* program in a pseudo-console (`script(1)`, `tmux`, an embedded
terminal); a shell that was started in a console already has one, and the
editor's needs are the same as an ncurses program's: raw input, ANSI output, a
size.

`baash-sys`'s `console.rs` is that boundary, and it is the counterpart of
`term.rs`:

| Unix | Windows |
|---|---|
| `/dev/tty`, stdio fallback | `CONIN$`/`CONOUT$`, `GetStdHandle` fallback |
| `tcgetattr`/`tcsetattr` + `cfmakeraw` | `GetConsoleMode`/`SetConsoleMode` |
| `TIOCGWINSZ` | `GetConsoleScreenBufferInfo` (the *window*, not the buffer) |
| `read(2)` bytes | `ReadConsoleW` → UTF-8 |
| `write(2)` bytes | `WriteConsoleW` ← UTF-8 |

The piece that makes one editor serve both is `ENABLE_VIRTUAL_TERMINAL_INPUT`:
the console then puts the same ANSI escape sequences in the input buffer that an
xterm sends, so `key.rs` decodes arrows and `Alt-…` unchanged.
`ENABLE_VIRTUAL_TERMINAL_PROCESSING` does the same on the way out.

Two consequences are written into the code:

* **`Ctrl-C` is a keystroke at the prompt.** `ENABLE_PROCESSED_INPUT` is off
  (otherwise the console turns `Ctrl-C` into a signal and the editor never sees
  it), so the byte arrives as `0x03` and abandons the line the way it does on
  Unix. The same switch means `Ctrl-Z` arrives as `0x1a`, so the editor honours
  it as end-of-input on an empty line — the console's EOF key, where Unix uses
  `Ctrl-D`. **While a command runs** that mode is restored, so the key becomes a
  console event instead; see "Console handles, `Ctrl-C` and `$( … )`" below.
* **There is no poll on a console handle.** `SetConsoleMode` is not a waitable
  object, so the prompt watcher (`docs/prompt-watch.md`) stays
  `#[cfg(target_os = "linux")]`; on Windows the prompt updates between lines.

`RawMode`, `NoEcho` and `window_size_fd` are the same module underneath, so
`read -s`, the `COLUMNS` fallback and the editor's wrapping all work on Windows
for the first time.

**Verified on real Windows** (2026-11-06), in the `winvm` guest — a Windows 11
Pro libvirt domain driven over ssh, with the binary started in a console window
in the interactive session and keystrokes injected through QMP. What was seen:
the coloured prompt, `echo hi`, ghost text accepted with `Ctrl-F`, single-line
and multi-line `for` loops (including the `> ` secondary prompt), an external
command and a pipeline, and `Ctrl-Z` returning to `cmd`'s prompt. History opens
there (`history.db` plus a WAL, so the multiprocess WAL the Wine run refused is
supported by the real IOCP backend).

Two bugs came out of that first run, both invisible under Wine:

* `CONOUT$` was opened write-only and then queried with `GetConsoleMode`, which
  needs read access — so a perfectly good console was reported as absent. Both
  ends are opened read+write now, and the standard handle is preferred when it
  is a console.
* Command lookup found nothing, for five reasons recorded in the next section.

### Console handles, `Ctrl-C` and `$( … )` (2026-11-12)

Three symptoms, one part of the console boundary, found in the same session on
the guest:

| symptom | what was actually wrong |
|---|---|
| `for i in $(seq 2); do echo "$i"; done` printed `1 2`, then `baash: The handle is invalid. (os error 6)` and the shell exited | the editor had cached the *process's* standard-handle number, and `_dup2` on descriptor 1 replaces the handle behind it |
| `cmd /c echo hi 2>&1; echo after` printed `hi` and lost `after` | a child's stdio took ownership of the shell's descriptor and closed its handle after `CreateProcess` |
| `Ctrl-C` during a running command ended the shell | no console control handler, so the event's default action was `ExitProcess` |

**The stale number.** `Console` preferred the standard handle when it was a
console, which is right — it is the same console, with both access bits already
— but it kept the borrowed `HANDLE` value. `_dup2` on a standard descriptor
closes the old handle and installs a new one; a later `_dup2` back puts *some*
handle in that slot, and whether its number is the old one is luck. With no
child process in between, Windows reuses the number and the bug hides; a
command substitution around an external command allocates handles in the middle,
the restored handle gets a different number, and the next `WriteConsoleW` on the
cached one reports error 6. Measured with a purpose-built probe in a console
session:

```
h0=112 start=ok(7)
after-dup2 h0=INVALID(6) std=112
no-spawn-restored h0=ok(7) std=112
after-spawn h0=INVALID(6) std=112
spawn-restored h0=INVALID(6) std=112
```

The console now **duplicates** its ends instead of borrowing them: `End` is one
owned `File`, so redirecting descriptor 0/1/2 — a capture, a pipeline, a
committed `exec >file` — cannot invalidate the handle the editor paints and
reads with. That is the Windows shape of `/dev/tty`.

**The child's handles.** Capture and pipelines were already right: the shell
redirects descriptor 1 with `_dup2`, and `_dup2` updates the process's standard
handle for descriptors 0–2 as a side effect, so `Stdio::Inherit`'s
`GetStdHandle` followed the capture (measured on the old build:
`baash -c 'x=$(cmd /c echo hi); echo [$x]'` printed `[hi]`). What was wrong is
the *other* path: when a command carries a redirection action, `spawn` wrapped
the descriptor's own handle in `Stdio`, and `Stdio::from_raw_handle` takes
ownership of what it is given — `Command` closes it after `CreateProcess`, so
the shell's descriptor was left pointing at a closed handle. The old build's
`cmd /c echo hi 2>&1; echo after` printed `hi` and nothing else. Every handle
is duplicated now, and every child gets descriptors 0, 1 and 2 explicitly
(duplicated, or `NUL` when the shell closed one), so the shell neither relies on
the CRT's `SetStdHandle` side effect nor loses a descriptor to its own child.

**`Ctrl-C` while a command runs.** Windows has no `SIGINT`: a console delivers
`CTRL_C_EVENT` to every process attached to it, and the default handler calls
`ExitProcess`. An interactive shell now installs a `SetConsoleCtrlHandler` that
returns `TRUE` for `CTRL_C_EVENT` and `CTRL_BREAK_EVENT` (close/logoff/shutdown
still fall through), which is the Windows analogue of bash ignoring `SIGINT`
while it waits for a foreground job: the child receives the same event and dies
on its own. `STATUS_CONTROL_C_EXIT` is reported as `Signaled(SIGINT)`, so `$?`
is 130 rather than the low byte of `0xC000013A` (58). A non-interactive shell
installs nothing, so `baash -c 'sleep 10'` still dies on `Ctrl-C` the way a
script does.

**Verified on the `winvm` guest** (2026-11-12), in a Windows Terminal tab
(ConPTY) opened in the console session, with keystrokes injected through QMP:
`x=$(cmd /c echo hi)` returns to the prompt and `echo $x` prints `hi`; the
`for i in $(seq 2)` loop prints `1 2` and returns; `echo hi | cmd /c findstr hi`
still prints `hi` and `cmd /c echo plain` still prints `plain`; a long `ping`
interrupted with `Ctrl-C` prints `Control-C`, returns to the prompt, and
`echo $?` prints 130; `Ctrl-C` at the prompt still abandons the line
(`echo abc^C` then a fresh prompt). The `2>&1` case was compared against the
previous build in `-c` mode: the old one printed only `hi`, the new one prints
`hi` and `after`.

What is still coarse: the handler swallows the event for the whole interactive
process, so a `Ctrl-C` pressed while a *built-in* loop runs does nothing until
it returns (bash would abandon it at the next command boundary — that needs a
pending-interrupt check in the interpreter), `trap INT` never runs for a console
event, and `Ctrl-\` has no equivalent at all.

### Command resolution on Windows (2026-11-06)

`whoami: command not found` had five separate causes; all are fixed, and all
are things that only exist on this platform:

| what | why it mattered |
|---|---|
| `PATH` arrives as `Path` | environment names are case-insensitive and keep the parent's spelling; shell variables are not, so `$PATH` was unset and the shell seeded a POSIX default |
| a `PATH` is `;`-separated | splitting on `:` found no directory at all (one constant now: `baash_sys::PATH_SEPARATOR`) |
| bare names get `PATHEXT` | `whoami` is `whoami.exe`; the machine's own list is used |
| `C:\…` is a path | the "contains a separator" test only knew `/`, so a backslash path was searched for in `PATH` |
| entries join with `\` | a `PATH` hit became `C:\WINDOWS\system32/cmd.EXE`, and `cmd.exe` refuses to *start* from that mixed path while the same file with backslashes runs (`baash_sys::PATH_JOIN`) |

### The Windows path model: `cd`, `PWD` and the drive roots (2026-11-07)

`cd` used to fail with `os error 123` in every form, and `pwd` after `cd ..`
printed `/`, because the shell resolved names with POSIX rules: it split on `/`
and joined from one root, so `C:\Users\win` became the single component
`C:\Users\win` under a leading `/` — a name Windows does not accept. The walk
lives in `baash_sys::paths` now and knows what a *root* is:

| | POSIX | Windows |
|---|---|---|
| root | `/` | `C:\`, `\\server\share\` |
| separator | `/` | `\`, and `/` everywhere |
| `..` at a root | stays at `/` | stays at the drive or share root |
| `\foo` | not absolute | rooted on the current *drive* |
| `C:foo` | — | drive-relative: only the kernel can place it |
| `\\?\…`, `\\.\…` | — | device namespaces: the kernel's, not ours |

Everything else is unchanged on purpose. The walk is still lexical — no
`canonicalize` — so `cd symlink` keeps the link's name and `pwd -P` is still the
only way to ask the kernel. Three consequences are worth stating:

* A typed `/` is recorded as `\` in `PWD`: the shell normalises the separators
  it already collapsed, so `cd C:/Windows` then `pwd` prints `C:\Windows`.
  This is the only rewriting it does; it never rewrites a name.
* `C:foo` and `\\?\…` are handed to `chdir` as written and `PWD` becomes
  `getcwd()`'s answer, which is what `cmd` does too.
* `\` is still an escape character, as it is in every POSIX shell: a Windows
  path is written `C:/Users/...` or quoted (`cd 'C:\Users'`). That is the
  msys convention and it is in the list below.

`cd ""` changed too, on both platforms: bash reports it as "null directory"
rather than as a failed `chdir`, does not search `CDPATH` for it, and `pushd`
says the same. baash used to enter the current directory and succeed.

`~` and a bare `cd` also work on Windows now. A native process starts with no
`HOME` at all — the profile is `%USERPROFILE%` — so `baash_sys::os::own_home`
answers with it, and `cd`/`dirs`/the prompt fall back to it when `HOME` is
unset. On POSIX there is deliberately no fallback: bash fails a bare `cd` with
no `HOME`, and so does baash.

**Verified on the `winvm` guest** (2026-11-07), with the release build from this
tree: `cd 'C:\Windows'` and `pwd` → `C:\Windows`; `cd ..` → `C:\`, and `..`
again → `C:\`; `cd C:/Users/win/AppData` → `C:\Users\win\AppData`; a rooted
`cd '\Windows\System32'`; `cd /` → `C:\`; textual collapse through
`'C:\Windows\System32\..\..\Users'` → `C:\Users`; `cd ""` → "null
directory", status 1; `cd 'nowhere\..'` → error; `pwd -L`/`-P`;
`pushd`/`popd`/`dirs -p`; `cd -`; `cd ~` and `cd "$USERPROFILE"` →
`C:\Users\win`; the kernel fallback through `cd C:.`; a UNC share
(`cd '\\localhost\C$\Users'`, and `..` stopping at `\\localhost\C$\`);
`\?\C:\Windows` through the kernel; and `$(cmd /c cd)` in a child agreeing
with the shell. Interactively, in a console window: `pwd` → `C:\Users\win`
with the prompt abbreviated to `~`, then `cd C:/Windows` and the prompt showing
`C:\Windows` — the path segment and the `$` are correct in both.

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

A `turso` limitation that shaped the schema: the column form
`id INTEGER PRIMARY KEY AUTOINCREMENT` is accepted and keeps a high-water mark
in memory (a delete then insert gives the next id), but with a **file**
database reopened in multiprocess-WAL mode it does not: after `history -c`, the
next insert reused id 1. Measuring that is why `baash-history` never deletes a
row whose id it has shown — it tombstones it and prunes only rows below the
newest id (see `docs/contracts.md`).

Two shells share one history file, so the connection is opened with
`experimental_multiprocess_wal(true)` plus a five-second `busy_timeout`: without
the first, the second `Builder::build()` on the same path fails with "database
is locked" even when the first is idle; without the second, a write that lands
while the other shell is writing fails instead of waiting. Both were verified
in one process (two stores on one file, each seeing the other's rows), and the
mode is refused for `:memory:`, which takes the plain builder.

The runtime behaviour was verified on Linux first. `baash-history` — and with
it turso, with both of those options — cross-compiles for
`x86_64-pc-windows-gnu` and has since run on the guest, where the interactive
shell opens `history.db` plus a WAL (§9, "The console host"). `baash-runtime`
cross-compiles too, because deleting the history file removed its last
unix-only code.

One Windows build detail: `turso_sdk_kit`'s build script links a version
resource by invoking plain `windres`, which `cargo-zigbuild` does not provide.
The shim is checked in as `ci/windres` — `exec` the compiler named by
`BAASH_WINDRES`, or `llvm-windres` by default, with the arguments it was given —
and the Windows derivation puts it on `PATH` under the plain name with
`BAASH_WINDRES` naming the real compiler (`llvmPackages.bintools-unwrapped`:
the wrapped path needs a stdenv environment that a build script does not have).

It is a wrapper *script*, not a symlink, on purpose: nix-wrapped binutils
resolve their own directory from `argv[0]`, so a symlink to one of those breaks
it. LLVM's binary does not care, but the script is the form that stays correct
whichever resource compiler is behind it.

Either way, the resource compiler **preprocesses** the script before compiling
it, so a C preprocessor has to be on `PATH` as well — this is the part that is
easy to miss because it only bites where the wrapper was not in the picture:

* LLVM's looks for `clang` and gives up instead, so the derivation takes
  `llvmPackages.clang` next to the bintools. `--no-preprocess` is not a way out:
  the script turso generates is built out of `#define`s (`FLAGSMASK`, `VORG_NT`
  …), which then never expand.
* The MinGW cross-`windres` shells out to `x86_64-w64-mingw32-gcc` and fails
  with `x86_64-w64-mingw32-gcc: command not found`, so that wrapper has to
  prepend the MinGW `bin` to `PATH` rather than just `exec` the one binary.

## 10. The two Windows builds, and why there are four derivations (2026-11-12)

`x86_64-pc-windows-gnu` is cross-compiled, so nothing about it is shared with
the Linux build: no artifacts, no incremental state, no cache. That makes the
edit / compile / copy-to-the-guest loop the slowest thing in the tree, and the
default link step the most expensive part of it.

Two builds of the same program, differing by one Cargo profile:

| command | profile | fat LTO | codegen units | executable | for |
|---|---|---|---|---|---|
| `just windows` | `[profile.windows]` | yes | 1 | 20.6 MB | CI, the guest, a release |
| `just windows-fast` | `[profile.windows-fast]` | no | 16 | 29.7 MB | editing |

`windows-fast` inherits `windows` and overrides only `lto` and `codegen-units`,
so `opt-level`, `panic = "abort"` and the feature set — everything that decides
behaviour — are identical. A fast build that behaved differently would not be a
fast build of the same program.

### Four derivations, not two

Both builds are split into a dependency half and a binary half, cached
separately:

* `packages.windows-deps` / `packages.windows-fast-deps` build the crates.io
  graph from a *dummy* copy of the tree: crane's `mkDummySrc` keeps only
  `Cargo.lock`, `.cargo/config.toml` and the manifests, and stubs every
  workspace source. The derivation therefore does not depend on `crates/*/src`
  at all — editing the shell cannot invalidate it.
* `packages.windows` / `packages.windows-fast` inherit that `target/` and
  compile the workspace crates and the link.

The rule that keeps this working: **the two halves must be configured
identically.** `flake.nix`'s `mkWindows` builds one argument set for both, and
a linker, a C flag or a profile that appeared in only one of them would not be
an error — it would be a silent full rebuild of every dependency in the other,
which is the cost the split exists to remove. The check is
`nix eval .#windows-deps.drvPath`: it must not move when only `crates/*/src`
changed.

### Measured

32 cores, one added line in `crates/baash/src/main.rs`, `nix build -L`:

| build | units compiled | wall clock |
|---|---|---|
| monolithic `.#windows` (before this split) | 413 | 193 s |
| `.#windows` after an edit | 14 | 79–92 s |
| `.#windows-fast` first build | 427 | 50 s |
| `.#windows-fast` after an edit | 14 | 15 s |

Reading it:

* The split does not make a *cold* build faster; that is still the whole graph
  (213 s for `.#windows`, 50 s for `.#windows-fast`). What it buys is the case
  that happens twenty times an hour: the 14 units are the workspace, and 15 s is
  the time to copy the new executable to the guest.
* The two edit cases compile the same 14 units, and cargo *starts* all of them
  within five seconds of each other, so the difference is not how many crates
  there are but what happens afterwards: `baash` is the largest crate, and under
  `[profile.windows]` it emits LTO bitcode which the link step then optimises
  as one program — 71 s of the 78 s. `windows-fast` finishes the same work in
  15 s. That is the whole argument for two profiles.
* Fat LTO is also why `.#windows` costs four times the cold build of
  `.#windows-fast` for the same 427 units: every dependency pays the same
  bitcode-and-serial-codegen cost, not just `baash`. It is a fair trade for the
  artifact that ships and a bad one for iterating.

The day-to-day protocol — which check catches `cfg(windows)` code, which build
to use, copying to the guest, a GUI run — is [windows.md](windows.md). This
section is the shape and the numbers behind it.
