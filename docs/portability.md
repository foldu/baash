# Portability: no fork, and a Windows target

Status: **landed.** One execution model, two supported platforms. This file
records the model, what each platform does with it, the diagnostics that stand
in for what is missing, and the measurements behind each decision. The dated
sections are observations from the host or the guest, kept because they are the
evidence; §10 is the build shape, and [windows.md](windows.md) is the day-to-day
protocol.

The motivating facts: PowerShell 7 takes seconds to start and its completion
times out; a native Rust shell starts in well under a millisecond (measured:
`baash -c :` 0.40 ms in release on Linux) and completes a 59k-entry directory in
~4 ms in-process. The gap is worth closing on Windows, where "a fast shell with
tab completion" is currently a niche filled by Git Bash and WSL. The Windows
number is not that one: a start there is `CreateProcess` and the loader, measured
at 12.3 ms for `baash -c exit` on the guest — of which the shell itself is
0.2 ms, and it is faster than `cmd.exe` on the same machine
(`performance.md`, 2026-11-17).

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

**Those temporary files are unlinked on both platforms, by different means.**
Unix gets one from `memfd` or `tmpfile`; Windows has neither, so
`create_unlinked_temp` opens a `std::fs` file — whose share mode includes
`FILE_SHARE_DELETE` — hands the handle to `_open_osfhandle`, and removes the
name before returning: the descriptor keeps the bytes and the file goes away
with the last handle, so `%TEMP%` does not collect one file per substitution.
It used to `_wopen` a name that `remove_file` could not delete (the CRT asks for
no share-delete) and use `…-0.tmp` for every call, so two live captures were
one file — a command substitution inside `PROMPT_COMMAND` wrote into the
prompt-command capture as well. The name now carries a process-wide counter.

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
| `jobs` | lists nothing, status 0 | same |
| `fg`, `bg`, `disown`, `%1` | rejected (`E0302`) | rejected |
| `exec 3<file` | the fd table | the CRT fd table |
| `<(...)`, `>(...)` | rejected (`E0301`) | rejected |
| `$$` | the shell's pid, also inside `( … )` | same |
| `$BASHPID` | **unset** — see §8 | same |
| `umask` | process mask | rejected: no mask on Windows |
| `set -m` | accepted, inert | same |
| `^Z` | no job control: the child stops with the shell | `0x1a`, end-of-input at the prompt |
| `trap INT` | signals | console control events, coarse (§9) |
| `kill` | signals, `TerminateProcess` for externals | `TerminateProcess` only |

Three rows are policy rather than mechanism: `set -m` is accepted and inert so
an rc file does not break, `fg`/`bg`/`disown` stay *registered* so `type` finds
them and fail when run instead of reporting "command not found", and `jobs`
answers what it knows — an empty job table — rather than failing, because
`starship_precmd` and other prompt frameworks run `jobs -p` before every
prompt.

## 4. Rejection is a first-class diagnostic

Two codes carry it, from one gating table over the AST (the same mechanism the
POSIX superset uses):

* **`BAASH-E0301`** (`UNSUPPORTED`) — an expansion the shell has no mechanism
  for: `${x@A}` and the other transformations it does not implement, and the
  substring forms of `$@` it cannot express. Process substitution is not in this
  list any more: both directions work, buffered (`contracts.md`, `<(…)` hands a
  path).
* **`BAASH-E0302`** (`CAPABILITY`) — a construct whose POSIX meaning is a
  second process or a job table: `f &`, `{ … } &`, `fg`, `bg`, `disown`,
  `%1` in `wait`/`kill`, and a job operand to `jobs`.

(`exec` on Windows is a third: `BAASH-E0205`, the `exec` failure code, because
it is a named command that cannot do its job rather than a construct the shell
has no mechanism for.)

Each names the construct and the reason, at the point of refusal:

```
$ baash -c 'f() { echo fn; }; f &'
baash:1:19: error[BAASH-E0302]: &: f is not an external command, and shell code cannot run in the background
$ baash -c 'fg'
baash:1:1: error[BAASH-E0302]: fg: job control is not implemented in this shell
$ baash -c 'x=hi; echo ${x@A}'
baash:1:12: error[BAASH-E0301]: unsupported expansion: parameter transformation (`${x@Q}`)
```

The status is 1, the same failure any command returns, so `if` and `&&` behave
as they would around a failing command. `fg`, `bg` and `disown` are registered
built-ins that fail when run, because `type fg` must still answer and an rc file
must not see "command not found". `jobs` is registered for the same reason but
succeeds with no output: an empty job table is not a capability gap.

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

* `crates/baash-utils` links the individual uutils binaries it uses — each a
  small library with a `uumain(args) -> i32` entry point, not the umbrella
  crate — and the table there is the whole surface. Feature-gated as
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

### The set

| group | utilities |
| --- | --- |
| file work | `cat` `cp` `du` `ln` `ls` `mkdir` `mktemp` `mv` `readlink` `realpath` `rm` `rmdir` `split` `touch` `truncate` |
| text | `base64` `cksum` `comm` `cut` `head` `md5sum` `nl` `od` `paste` `sha1sum` `sha256sum` `shuf` `sort` `tac` `tail` `tee` `tr` `uniq` `wc` |
| process, time | `date` `expr` `nproc` `seq` `sleep` |

The criterion for a name being in that table is **not** "uutils has it": it is
that a script needs it on a machine with no POSIX userland, and that the
utility needs nothing Windows does not have. What is out, and why:

* **No truthful Windows answer.** `chmod`, `chown`, `chgrp` (no mode bits),
  `df` (no mount table), `id`, `groups`, `logname`, `users`, `who`, `pinky`,
  `uptime`, `hostid`, `arch`, `uname`, `tty`, `stty`, `dircolors`, `sync`,
  `sum`, `yes`, `dd`, `pr`, `ptx`, `tsort`, `factor`, `pathchk`, `link`,
  `unlink`, `dir`, `vdir`, `more`, and `stat` — uutils 0.8's `stat` imports
  `std::os::unix::fs::{MetadataExt, FileTypeExt}` without a gate, so it does not
  compile for Windows at all, which settles the question. A wrong answer is
  worse than "command not found": a script that reads one makes a decision on
  it.
* **Not a body in this process.** `env`, `nice`, `nohup`, `timeout`, `stdbuf`,
  `chroot`, `runcon`, `install` spawn, and spawning is the thing the embedded
  set exists to remove.
* **Not uutils at all.** `grep`, `sed`, `awk`, `less`, `which`, `tree` (GNU or
  someone else's), `find`/`xargs` (uutils' *findutils* project), `ps`/`top`
  (its *procps* project), `tar`/`gzip`/`curl`/`ssh` (nothing). `clear` is
  ncurses' and is written by hand here — see §6.1.

### 6.1 Names Windows already answers, which is its own hazard

`PATH` is searched first, so on Windows some of these names resolve to a
program that means something **else**, which is worse than a missing command:

| typed | runs | difference |
| --- | --- | --- |
| `find . -name x` | `find.exe` | searches *text*, and blocks reading stdin |
| `sort -n` | `sort.exe` | different ordering and flags — ours is embedded for this reason |
| `fc a b` | `fc.exe` | a file compare with different output, not `diff` |
| `where x` | `where.exe` | roughly `which` |
| `more f` | `more.com` | same idea, fine |
| `tar`, `curl`, `ssh`, `whoami`, `hostname` | the system's | the same meaning |

`clear` is the other side of the same problem: `cls` exists only inside
`cmd.exe`, so there is no program to find and nothing to fall back on. That is
what `crates/baash-utils/src/clear.rs` is, and why it is mine rather than
uutils's: it writes the ECMA-48 clear (`CSI H`, `CSI 2 J`, and `CSI 3 J` for
the scrollback unless `-x`), which is what every common `terminfo` entry reduces
to, and on Windows it asks the console for `ENABLE_VIRTUAL_TERMINAL_PROCESSING`
first (`baash_sys::screen`), because a console that has not been asked renders
those bytes literally. It does not read `terminfo`: `TERM=dumb` clears nothing
at all, and any other value gets the same sequence — the `docs/terminal.md` rule
for the shell's own bytes, where an unparsed sequence is ignored rather than
worked around.

Verified on Windows 11 (26300.9711, `win` guest, fat-LTO build):

* `baash.exe -c "clear -T xterm-256color"` with stdout redirected writes
  `1b 5b 48 1b 5b 32 4a 1b 5b 33 4a` (11 bytes), `-x` writes the first seven,
  `-T dumb` writes nothing, all with status 0; `--help` is status 0 and
  `clear -q` is status 2 with the usage on stderr.
* In a console — a scheduled task in the logged-on session, so `stdout` is the
  console and not the ssh pipe — a `cmd.exe` window printed `BEFORE-CLEAR` and
  twenty filler lines, ran `baash.exe -c clear`, then `echo AFTER-CLEAR`. The
  screenshot afterwards shows **only** `AFTER-CLEAR` at the top left: the
  screen was erased, the cursor homed, and the literal `ESC[H ESC[2J` bytes did
  not appear, which is the mode change working.

### The environment a body reads

An embedded utility reads configuration from the **process** environment
(`std::env`), not from the shell's variables: a prefix assignment and `export`
both live in the shell's table, and the redirect plan is what the call shares
with the shell, not the environment. So `LC_ALL=C sort` inside a script does not
change the embedded `sort`'s collation, `TZ=UTC date` ignores the assignment,
`COLUMNS` is not what `ls` sees, and `TERM=dumb clear` clears whereas
`clear -T dumb` does not. Only the variables the shell *inherited* — the process
environment it started with — are visible, because they are the process's own.

This is a gap, not a design: publishing the exported environment around the call
is the fix, and it is recorded here so nobody assumes otherwise in the meantime.
`clear` is the one body where the difference is user-visible today.

### Cost

`cargo build --release -p baash` on Linux (thin LTO, 16 codegen units), no
feature versus the two sets:

| build | release binary | crate tree of `baash-utils` |
| --- | ---: | ---: |
| no `builtin-utils` | 11.56 MiB | 12 |
| ten utilities (the first wave) | 15.64 MiB | 126 |
| this set | 25.15 MiB | 243 |

Nine and a half mebibytes for thirty-one utilities is what makes `cp`, `mv`,
`sort` and `date` exist at all; the marginal text utilities are a rounding
error beside them (`sort`'s own rlib is 3.1 MiB, `cp`'s 1.3). The *shipped*
Windows exe, fat LTO and stripped, is smaller: 16.40 → 17.83 MiB, because most
of the Linux figure is symbols.

`stat` is not in the set for a reason worth writing down: uutils 0.8's `stat`
imports `std::os::unix::fs::{MetadataExt, FileTypeExt}` without a gate, so it
does not *compile* for `x86_64-pc-windows-gnu` — the cross-build says so in 29
errors, and it was the only one of the thirty-two candidates that failed.

### Built-in gaps on Windows

The shell's own bodies (`printf`, `test`/`[`, `cd`, `read`, `ulimit`, …) are
written against `std` and `baash-sys`, so they build and run on Windows without a
separate code path. Two of them necessarily approximate a Unix answer there:

* `printf '%(…)T'` has no IANA timezone database behind the Windows C library.
  The common `strftime` specifiers are formatted in UTC and `TZ` is ignored;
  `%Z`/`%z` report `UTC`/`+0000`.
* `test`/`[` and `[[ … ]]` answer `-x` as existence (Windows has no execute
  bit) and `-w` from the read-only attribute, and `-O`/`-G`, `-u`/`-g`/`-k`,
  `-nt`/`-ot`/`-ef` and the socket/character/block/FIFO tests false because the
  metadata is not there to inspect. Both commands share the predicates
  (`baash_runtime::filetest`, `filecmp`), so they cannot disagree about it.

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
* **The file tests answer from Windows metadata:** `test`/`[` and `[[ … ]]`
  both, `-x` as existence, `-w` from the read-only attribute, and `-O`/`-G`,
  `-u`/`-g`/`-k`, `-nt`/`-ot`/`-ef` and the file-type tests false (§6,
  "Built-in gaps").
* **`printf '%(…)T'`** formats in UTC and ignores `TZ`, because the Windows C
  library has no IANA database behind it.
* **A batch-file argument is parsed twice.** A `.cmd`/`.bat` runs through
  `cmd.exe /d /s /c`, so an argument containing `%NAME%` is expanded by the
  interpreter (which has no escape for `%`), and an embedded `"` can end
  `cmd`'s quoted run early. `.exe` targets take the direct path and are
  unaffected; §9, "The name is `code.cmd`", has the measurements.
* **`coproc` is not in the grammar.** `coproc NAME { … }` is a parse error
  (`E0003`) and `coproc cmd` is looked up as a command, so it is the one
  construct in the "up to and including `coproc`" ceiling
  (`docs/direction.md` §7) the shell has not taken on.
* **`^Z`** has no job table to stop a job against; the child stops with the
  shell.
* **The terminal protocol is only partly verified on a Windows guest.** On
  Windows 11 (26300.9711) in Windows Terminal the prompt renders, OSC 0/1 set
  the tab title, and duplicating the tab (`Ctrl+Shift+D`) starts `cmd.exe` in
  the shell's directory, so the cwd report — `file://HOST/C:/…` or ConEmu's
  `OSC 9;9`, both are written — is consumed. A UNC, drive-relative or
  drive-rooted path yields no report rather than a guessed one, and `TERM=dumb`
  suppresses every announcement. The keyboard requests are written, but that
  host ignores both: `Ctrl-M` still submits and `Ctrl-Left` arrives as a plain
  Left, so the editor sees only the legacy encodings and the new
  `Ctrl-Left`/`Ctrl-Delete` bindings do not fire (`docs/terminal.md` §12). The
  console has no bracketed-paste protocol — a paste arrives as keystrokes, so
  `\x1b[200~` is never reached and the paste quoting in `docs/contracts.md`
  ("A paste that is data is quoted") never fires: on Windows a pasted URL is
  typed, and `&` is read as syntax exactly as it would be if the user had
  typed it. `Ctrl-Z` still undoes, except on an empty line, where the console
  has no end-of-file byte and that key leaves the session. There is no focus
  sequence or cursor-position report either, so `?1004` is not written and the
  prompt keeps anchoring at column 0 after output that did not end with a
  newline (`docs/terminal.md` §5, §6).
  Still unobserved: the 250 ms resize sampling path (`§7`), `Ctrl-L`'s
  scrollback push (`§8`), whether OSC 52 reaches the Windows clipboard (`§9`),
  click-to-move, which needs both `click_events` and the cursor report (`§10`),
  the background-colour query, which the console does not answer, so Windows
  keeps the dark palette (`§11`), and the whole of `Ctrl-R`'s full-screen
  picker (`docs/terminal.md` §14) including `baashzf`, which needs the console
  to honour `ESC [ ? 1049 h` and `ENABLE_VIRTUAL_TERMINAL_PROCESSING`; the
  picker's frames and keys are platform-neutral bytes, so what is unverified is
  the console's own alternate screen, not the picker.

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
script does. Unix reaches the same promise with a blocked mask rather than a
handler (`docs/contracts.md`, "An interactive shell blocks the terminal's
signals for its whole life"): the editor has `ISIG` off, but a command runs with
the terminal back in cooked mode, and without the block the `SIGINT` that is a
keystroke at the prompt killed the shell mid-command.

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
process, so the shell has no pending signal to drain and a `Ctrl-C` pressed
while a *built-in* loop runs does nothing until it returns, `trap INT` never
runs for a console event, and `Ctrl-\` has no equivalent at all. The
Unix-side abort (`Unwind::Interrupt`) is shared, though: a foreground child
killed by `Ctrl-C` arrives as `WaitOutcome::Signaled(SIGINT)`
(`STATUS_CONTROL_C_EXIT`), and the interpreter's first boundary after that wait
abandons the command.

**Verified on the `winvm` guest (2026-11-16)** — the plain console host
(`conhost`, not the Windows Terminal tab the 2026-11-12 run used) started with
`schtasks /it` in the interactive session, keys injected with
`virsh send-key --holdtime` and read back from screenshots:

| case | what was seen |
|---|---|
| `ping -t localhost` | the replies, `Control-C`, `Ping statistics`, the prompt, `$?` = 130 — `ping` handles the key itself and still exits `STATUS_CONTROL_C_EXIT` |
| `ping -t localhost; echo not-reached` | statistics, `Control-C`, the prompt: `not-reached` was never printed |
| `while true; do ping -n 100 127.0.0.1; done` | one key stopped the `ping` *and* the loop — no second `Pinging …`, the prompt, `$?` = 130 |
| `echo abc`, then `Ctrl-C` | `echo abc^C`, a fresh prompt, and the next command runs |
| three keys in a row while a child ran, then `echo still-works` | `still-works`: nothing stayed latched to swallow the next line |
| `while true; do i=$((i+1)); echo $i; done` | two keys changed nothing: the counter was at 50903 afterwards and 339785 a minute later, with the process alive and using CPU |
| `baash.exe script`, its body `ping -t localhost`, then `Ctrl-C` | the shell died with its child: the window closed and the scheduled task's result was `0xC000013A` |

The last row is the symmetry the two platforms are meant to have: an
interactive shell survives what kills its child, and a script dies of it. The
built-in loop stays the gap a user notices first, and it is the shape of the
platform that makes it a gap — `Ctrl-C` is a *byte* at the prompt and a console
*event* only while a command runs, and the event is exactly what the handler
swallows.

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

### The name is `code.cmd`, not `code` (2026-11-17)

The suffix search above was in the wrong order, and a `.cmd` could not have
been started even if it had been found. Reported against VS Code, which is where
it shows up: `code` is in `PATH`, so it should run.

**Symptom, measured on the guest with the build before this change**, with VS
Code 1.146.0 installed by `winget --scope user`:

```
$ baash -c 'command -v code'
C:\Users\win\AppData\Local\Programs\Microsoft VS Code\bin\code
$ baash -c 'code --version'
baash:1:1: error[BAASH-E0205]: C:\Users\win\AppData\Local\Programs\Microsoft VS Code\bin\code: %1 is not a valid Win32 application. (os error 193)
$ cmd /c code --version
1.146.0
```

`…\Microsoft VS Code\bin` holds `code` (a 2 KB `#!/usr/bin/env sh` script),
`code.cmd` (the batch file that runs the real `Code.exe`) and `code-tunnel.exe`.
Windows has no execute bit, so `baash`'s `PATH` walk asked "is `dir\code` a
regular file?" — the shell script is — and stopped there; `CreateProcess` then
refuses a script that is not a PE image (193). The suffix search existed, but it
ran *after* the bare name, which is precisely the candidate that must not win.

**What Windows does**, measured on Windows 11 26300 with `cmd.exe`, in a
hand-built directory (a bare `zz` and `zz.cmd`; a copy of `whoami.exe` named
`barefile`; `yy.foo` beside `yy.foo.cmd`; `cc.qzx` beside `cc.qzx.cmd`):

| case | `cmd /c` | why |
|---|---|---|
| `zz` with bare `zz` and `zz.cmd` | runs `zz.cmd` | the `PATHEXT` suffixes are the candidates; the bare name is not one |
| `barefile` (a PE image, no extension) | `'barefile' is not recognized` | even by full path: a name with no `PATHEXT` extension is not a program |
| `cc.qzx` with only `cc.qzx.cmd` present | runs `cc.qzx.cmd` | suffixes are appended even when the name already has an extension that is *not* in `PATHEXT` |
| `yy.foo` with bare `yy.foo` **and** `yy.foo.cmd` | opens `yy.foo` through its file association (a modal dialog in session 0) | a dotted name that exists as a file is handed to `ShellExecute`, not to the suffix search |
| `dd.cmd` typed in full | runs it directly | a name that already carries a known suffix is the file |
| `PATHEXT=` cleared, and `PATHEXT` unset | `zz` still runs `zz.cmd` | empty and unset both mean the platform's default list |

So the order is: `PATHEXT`'s suffixes, in `PATHEXT`'s order, and nothing else;
a name that already ends in a known suffix is used as written; the literal file
is a candidate only for a name the user spelled as a path, and then it is last
(a path names a file, and `CreateProcess` will load a PE image of any name,
which `cmd` refuses). `where zz` lists the bare file as well — `where` reports
*files*, `cmd` executes *names* — which is why `command -v` must not be built by
listing directories.

**The fix** is `baash_sys::program` (`docs/contracts.md`): one parameterised
walk produces the candidate files for a name, in this platform's order, and
every VM resolution path calls it — `path_hits` (which is what `type`,
`command -v`, `hash` and execution share) and the spelled-path branch of
`resolve` — so what runs, what is reported, what is cached and what a subshell
sees are one answer by construction. `PATH` entries are still tried in order,
and within an entry only the suffix forms count: a directory holding just the
extensionless script is not a hit, so a later directory can still supply the
program (`cmd` refuses such a file even by full path, measured above).

**Starting it** is the other half: a `.cmd`/`.bat` is text, so
`CreateProcess` cannot load it. `baash-sys` now starts one as
`%ComSpec% /d /s /c "<path>" <args…>`. `/s` is what makes an outer quote pair
mean "this whole line is the command", so a path with spaces — "Microsoft VS
Code" has one — stays one word; every argument is quoted even without a space
because `/s` strips the line's first and last quote; inside a token the
backslash/quote rules are the C runtime's, which is what the batch file's `%1`
… `%9` end up being parsed with. `/d` skips a machine's `AutoRun` commands.

**Verified on the `winvm` guest (2026-11-17)**, VS Code 1.146.0 from `winget`,
cross-built `baash.exe` (`nix build .#windows-fast`), scripts run as
`baash.exe C:/build/verify.sh`:

| case | result |
|---|---|
| `command -v code` | `…\Microsoft VS Code\bin\code.CMD` |
| `type code` | `code is …\Microsoft VS Code\bin\code.CMD` |
| `code --version` | `1.146.0` / commit / `x64`, status 0 |
| `code.cmd --version` | same, status 0 (a full name with a known suffix is used as written) |
| `whoami.exe` typed in full | `winvm\win`, status 0 (a PE image keeps the direct path) |
| `hash` after both | the two entries above, i.e. what was cached is what ran |
| `echo "$(command -v code)"` and `x=$(code --version)` | the same path, and the captured output |
| a spelled path with no suffix (`"C:/…/bin/code" --version`) and `./code` in that directory | `code.CMD`, status 0; `command -v ./code` agrees |
| a name in a directory with spaces, arguments `'C:\temp\'`, `""`, `"a b"`, `'say "hi"'` | the child's CRTs saw `C:\temp\`, ``, `a b`, `say "hi"` — exact |
| `PATH` without the VS Code directory, then with it | 127, then 0 (a miss is not cached) |
| `PATHEXT=".CMD"` | `code --version` runs |
| a directory holding only the extensionless script | `code: command not found`, 127 — and `cmd` says `'…\code' is not recognized` |
| `cmd.exe /c echo hi`, `echo hi \| cmd.exe /c more` | unchanged: the interpreter is a PE image |

Two details worth knowing. The resolved spelling carries `PATHEXT`'s case
(`code.CMD`) rather than the file's (`code.cmd`), because the suffix is
appended as the machine wrote it; NTFS folds the case and `cmd` does the same.
And `exec code` still fails with `BAASH-E0205` ("there is no exec on Windows"),
now naming `code.CMD`; that is the Windows `exec` gap in §8, not this one.

**Still open**, all measured or reasoned above and none of them this bug:

* An argument containing `%NAME%` is expanded by `cmd.exe` before the batch
  file sees it (measured: `%USERNAME%` arrived as `win`), and `cmd` has no
  escape for `%`. An embedded `"` is written `\"` for the program's parser but
  `cmd`'s own quote scan does not treat backslash as an escape, so it can end
  the quoted run early.
* The hash table is not invalidated when `PATH` or `PATHEXT` is assigned —
  pre-existing, and bash flushes its table on a `PATH` assignment. Measured:
  after `PATHEXT=".EXE"` a previously hashed `code` still ran; `hash -r` made
  the next lookup honour the new list (127). Nothing caches a *failure*, so a
  directory that appears later in the same shell is found (measured above), and
  a path resolved by this change is the one cached, so the new answer cannot be
  defeated by an old entry.
* `./code` in a directory holding only the extensionless script is still tried
  as that file (the spelled-path fallback) and fails 193. `cmd` refuses it with
  "not recognized" instead. Keeping the attempt is deliberate: it is how an
  extensionless PE the user named explicitly still runs.
* A batch file's pid is the interpreter's: `$!` and `kill` name `cmd.exe`, and
  `kill` ends the interpreter, not necessarily the program the batch file
  started.

### The Windows path model: `cd`, `PWD` and the drive roots (2026-11-07)

`cd` used to fail with `os error 123` in every form, and `pwd` after `cd ..`
printed `/`, because the shell resolved names with POSIX rules: it split on `/`
and joined from one root, so `C:\Users\win` became the single component
`C:\Users\win` under a leading `/` — a name Windows does not accept. The walk
lives in `baash_sys::paths` now and knows what a *root* is:

| | POSIX | Windows |
|---|---|---|
| root | `/` | `C:/`, `//server/share/` |
| separator | `/` | `\` to the kernel; the shell writes `/` |
| `..` at a root | stays at `/` | stays at the drive or share root |
| `\foo` | not absolute | rooted on the current *drive* |
| `C:foo` | — | drive-relative: only the kernel can place it |
| `\\?\…`, `\\.\…` | — | device namespaces: the kernel's, not ours |

Everything else is unchanged on purpose. The walk is still lexical — no
`canonicalize` — so `cd symlink` keeps the link's name and `pwd -P` is still the
only way to ask the kernel. Three consequences are worth stating:

* **The shell spells every path it prints with `/`.** `PWD`, `pwd`, `dirs`,
  the prompt, the window title and the `OSC 7` cwd report all say
  `C:/Users/win`, whatever the user typed and whatever the parent process
  exported. A name the *kernel* chose — `getcwd`, after a drive-relative
  `cd C:foo` — goes through `baash_sys::paths::respell`, which rewrites the
  separators and leaves a device namespace (`\\?\C:\…`) exactly as it came,
  because that is the spelling the kernel requires. Windows accepts `/`
  wherever it accepts `\`, and no Windows file name can contain either, so this
  names the same directory and never a different one: `cd C:/Windows` and
  `cd 'C:\Windows'` alike print `C:/Windows`.
* `C:foo` and `\\?\…` are handed to `chdir` as written and `PWD` becomes
  `getcwd()`'s answer, which is what `cmd` does too.
* `\` is still an escape character, as it is in every POSIX shell: a Windows
  path is written `C:/Users/...` or quoted (`cd 'C:\Users'`). That is the
  msys convention and it is in the list below.

**Switching drives** is a root change and needs no extra verb: `cd D:/`,
`cd 'D:\'` and `cd D:` all enter that drive's root, `pwd` then says `D:/`, `..`
stops there, a bare `cd` goes home (on another drive if that is where `HOME`
is), and `cd -` comes back. Two spellings that do *not* work are worth knowing:
`/d/` is not a drive — `/` is the *current* drive's root, so `cd /e/` looks for
`C:\e` — and after `cd E:/`, a rooted name such as `cd /Windows` means
`E:/Windows`, not the C: drive's, exactly as it does in `cmd`.

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

**Re-verified on the guest** (2026-11-17) after the `/` spelling and the drive
in the prompt, with the release build: `pwd` at home → `C:/Users/win` (the
inherited `PWD` is rewritten, not just one the shell worked out); `cd C:/Windows`
→ `C:/Windows` and `pwd -P` → `C:/Windows/System32` from inside it; `cd E:/` and
`cd E:` → `E:/`, `cd ..` there → `E:/`; `echo $PWD` → `C:/Windows`; a UNC share
→ `//localhost/C$/Users` whether typed with `\` or `/`; `cd C:` (drive-relative,
the kernel's) → `C:/Users/win`; `pushd`/`popd`/`dirs` print `/`. Interactively at
a real prompt: `C:~` at home, `C:~/p/a/beta` in `C:\Users\win\probe\alpha\beta`,
`E:/` after `cd E:/`, `C:/Windows` outside home, and `PS1='[\w] $ '` → `[C:~/probe]`
— `\w` carries the drive too. The window title was `[win@WINVM] C:~/probe/alpha/beta`
and the cwd report `file://WINVM/C:/Users/win/probe/alpha/beta`.

### The history database on Windows: SQLite through sqlx

`baash-history` stores history in SQLite, compiled into the binary: `sqlx 0.9`
with the `sqlite` feature, which means `libsqlite3-sys` with `bundled`, so the
amalgamation is built by `cc` and cross-compiled with everything else — the
host's `libsqlite3` is never consulted, on Windows or anywhere else.

The reason it is not a bespoke engine is the property two shells need: WAL mode
with working cross-process locking. On Windows that is the default behaviour of
SQLite — its wal-index lives in a `-shm` file that is mmapped precisely because
anonymous shared memory is not portable to Windows — so nothing here is
platform-conditional. The pool is opened with `journal_mode=WAL`, a five-second
`busy_timeout` (a write that lands while the other shell is writing waits rather
than failing) and `synchronous=NORMAL`.

`sqlx` is async and has no compio backend, so the runtime is `tokio`'s
**current-thread** runtime, built inside the history worker thread and nowhere
else: the shell's own thread stays synchronous, `Store`'s callers see ordinary
`std` channels, and compio keeps owning the terminal. Nothing outside
`crates/baash-history/src/store.rs` names `tokio`.

The C dependency does bring the build's usual Windows questions — a `cc`
compiler for the amalgamation — which the existing `cargo zigbuild` recipe
already provides for `libmimalloc-sys`; see §10 for the derivations.

Verified on the guest (2026-11-16): three interactive sessions sharing one
`%LOCALAPPDATA%\baash\history.db`, the second opened while the first was still
running. The row ids came out 1 `echo beta-marker` and 2 `exit` from the second
shell, 3 `history` and 4 `exit` from the first, 5 `history` from a third
started after both closed — one counter, so one database. The third shell read
the second one's command back, and no session printed a history diagnostic.
Before the swap the second shell could not open the file at all, so this is the
behaviour the change exists for, not a side effect.

One Windows detail that outlived the engine swap: `build.rs` stamps the exe's
icon with a resource compiler, and LLVM's `llvm-windres` — the one the
derivation puts in `BAASH_WINDRES` — **preprocesses** its script, so a `clang`
has to be reachable on `PATH` too. Nothing calls plain `windres` any more, so
the shim that existed only for turso's hardcoded `Command::new("windres")` is
gone, and `nix build .#windows-fast` is the check that it stays gone.

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
