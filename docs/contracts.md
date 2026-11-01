# Cross-crate contracts

Written as crates land. The failure mode this file exists to prevent: an agent
documents a contract in its branch report, the next agent never sees it, and the
two assumptions quietly disagree six weeks later.

Each section is owned by the producing crate. If you are implementing a consumer
(`baash-vm`, `baash-repl`, `baash-tools`) read the relevant sections before you start,
and treat every item here as a fact about the API you are calling.

Status: everything except the tooling has landed: `baash-diag`, `baash-sys`,
`baash-runtime`, `baash-lang`, `baash-syntax`, `baash-complete`, the `baash-vm`
tree-walking executor and the `baash-repl` editor.

---

## baash-diag

Renderers take `&Diagnostic` + `&SourceFile`; nothing borrows a parse arena, so
a diagnostic can be rendered long after the chunk that produced it is gone.

| renderer | use |
|---|---|
| `render_compact` | interactive: ≤ 3 lines, two-space indent, no box drawing |
| `render_report` | `-n`, lint, fmt: gutter, context, secondary labels, `help:` |
| `render_json` | editors and machine consumers; hand-written, stable key order |
| `render_line` / `render_line_with_help` | one greppable line; never ANSI |
| `fancy::render_fancy` | miette narratable report, behind the `fancy` feature |

- No renderer emits ANSI. Colour is the terminal layer's problem.
- Byte → display: **one cell per invalid byte** (`U+FFFD` for display), so byte
  offsets and columns stay in step. `TAB_WIDTH = 4`, `CONTEXT_LINES = 3`,
  `MAX_DIAGNOSTICS = 32`.
- `render_line` flattens newlines, tabs and control characters (including ESC), so
  a hostile filename cannot break the line format.
- The IR has no `Fix` yet, so no renderer produces a machine-applicable fix.
- `fancy` is off by default. `NarratableReportHandler` is used because miette
  7.6 gates `GraphicalReportHandler` behind `fancy-base`; the adapter is
  handler-agnostic, so enabling graphical output is one construction line.

---

## baash-sys

**Forking is gone, so threads are unconstrained.** There is no `fork_gate()`:
the server-side runs subshells, `$( … )` and pipeline stages in process
(`docs/direction.md` §2), and `posix_spawn` never needed a gate because it shares
the address space. The only remaining `fork` in the workspace is
`baash_sys::fork_child_status`, test scaffolding the pty harness uses to start a
shell in a child; it is documented as such and nothing in the shell calls it.

**Spawn plans carry no environment and no search.** `SpawnPlan::new` takes an
already-resolved program path and the full argv *including* `argv[0]`, and
starts with an **empty** `envp` (POSIX semantics). The runtime must push
exported `NAME=value` entries. There is no `posix_spawnp`, no `PATH` lookup and
no shebang handling here — those are VM policy.

**Signal state must be set on every plan.** Set `plan.sigmask =
Some(SignalSet::empty())` and populate `reset_signals` from trap state: `exec`
resets caught signals to default, but a signal set to `SIG_IGN` stays ignored, so
a `trap '' INT` leaks a Ctrl-C-proof child unless the plan says otherwise.

**Exec failures are `io::Error` in the parent, not a child status.**
`ENOENT`/`EACCES`/`ENOEXEC` come back as errors; map them to 127/126 rather than
inspecting the child's exit status. The report pipe makes this reliable: EOF on
the read end means `exec` succeeded.

**`SIGCHLD` is a wakeup hint, not an event.** Until `wait_any_nohang` returns
`None`, there may be more children to reap.

**There is no terminal handover.** The job-control wave's `tcsetpgrp` wrappers,
per-job process groups and `WUNTRACED` wait forms are gone. The shell and its
children share one process group, which is why `Ctrl-Z` stops the shell too.

**Process-group placement remains a spawn capability, unused by the shell.**
`ProcessGroup::SetPgid { pgroup: 0 }` creates a group led by the child;
`pgroup: leader` joins an existing one. `spawn` implements it, but `baash-vm`
never sets it: there is no job control to maintain groups for. `SIGTTOU`,
`SIGTTIN`, `SIGTSTP`, `SIGCONT`, `SIGSTOP` and `SIGCHLD` are still re-exported
next to `SIGINT`/`SIGQUIT`.

**`become_session_leader(fd)` is the pty-test helper**: `setsid` + `TIOCSCTTY`
in a forked child, so a test can give its shell a real controlling terminal and
`Ctrl-C` reaches the foreground group. It is not used by the shell itself (an
interactive shell already has one).

Linux-only: `SignalFd` (elsewhere `new`/`read` return `Unsupported` and
`as_raw_fd()` is `-1`), `memfd_create`, `O_TMPFILE`, and `ptsname`'s non-thread-
safe static buffer on the non-Linux `open_pty` path. Everything else is generic
POSIX.

**New in this wave, all thin and all unsafe-free at the API:** `lseek` (plus
`fd::seek`) is how `read` rewinds the bytes it buffered but did not consume, and
`ESPIPE` is how it discovers that a descriptor is a pipe; `NoEcho` is `read -s`
(echo off, canonical mode and signal generation kept); `Disposition` /
`set_disposition` / `ignore_signals` are the two places the shell changes a
signal *action* rather than blocking it (`trap '' SIG` is `SIG_IGN`, which
children inherit across `exec`, and `SIGINT`/`SIGQUIT` are ignored while the
shell waits for a foreground child); `replace_signal_mask` is the permanent form
of `SignalSet::set`, for a forked child that must not inherit the shell's
blocked traps. `SIGALRM`, `SIGTERM`, `SIGINT`, `SIGQUIT` and `ECHILD` are
re-exported so `baash-vm` needs no `libc`.

**`exec(plan) -> io::Error` replaces the process in place** and returns only on
failure. The caveat is inherent to exec ordering: by the time `execve` fails,
the plan's file actions, signal mask, `reset_signals` and process group have
all been applied. The image is intact, but the *environment* is not — so the
only sound response is to report 127/126 and exit. A caller that wanted to
continue after a failed `exec` (bash does) would have to restore descriptors and
signal dispositions itself. Also: keep the owned fds referenced by
`FdAction::Dup2 { from }` alive across the call, or the actions fail `EBADF`.

Terminal code is now tested on a real pty. `open_pty` opens the slave
`O_NOCTTY` so a test can never steal the harness's controlling terminal; the
tests install a known canonical `termios` state first, assert every flag raw
mode clears, and assert full restoration on `Drop`. `set_window_size_fd` states
the geometry a layout test asserts about: a pty starts at zero columns, which
`window_size_fd` reads as "not a terminal", and the prompt a test measures can
be longer than the narrow default the editor then falls back to.

**`parent_pid()` answers `Option<u32>`, and `None` means "not known here".**
Unix always answers (zero included: `getppid` reporting no parent is what an
isolated PID namespace looks like, and bash shows `PPID=0` there). Windows has
no `getppid`, so it says `None` and `baash-vm` leaves `$PPID` *unset* rather
than filling in a plausible number — `UID`/`EUID` already behave that way
where the platform cannot answer.

---

## baash-runtime

**`ShellHost` is the VM's obligation.** It now *requires* `state()`,
`run_source()`, `write_out()` and `write_err()`; `is_interactive()` and
`diagnose()` have defaults. `state()` hands out `ShellState`, whose main pieces are `opts`, `vars`
and `history`.

**The per-command sequence is fixed, and the order matters:**

```
resolve the command's tier
→ Vars::apply_prefix_assignments(assigns, tier)
→ run the builtin (BuiltinDef::run(host, argv))
→ Vars::finish_prefix_assignments()
→ host.state().take_error() → consequence(error, interactive)
→ check take_exit_request()
```

Skipping `finish_prefix_assignments` leaks a temporary local frame into every
later command; running it before the builtin makes `FOO=1 export FOO` behave as
if the assignment were discarded.

**Prefix assignments persist for special built-ins in both profiles.** POSIX
requires it; bash (non-posix) drops them and `bash --posix` persists them. We
follow POSIX unconditionally — a deliberate deviation recorded in DESIGN §3.3
Tier C.

**`VarValue::Indexed` is sparse, keyed by `i64`.** An ordered
`BTreeMap<i64, Vec<u8>>`, not a `Vec`: `a[100]=x` costs one entry, and
`unset 'a[0]'` leaves a hole that iteration skips. The key is `i64` even though
subscripts are non-negative after resolution, because bash subscripts are
machine integers (the suite exercises `a[0x7FFFFFFFFFFFFFFF]`). Reading it is
`Vars::element_indexed`/`element_assoc`; writing is `set_indexed`/`set_assoc`
or `append_indexed`/`append_assoc`; `unset_element_indexed`/`unset_element_assoc`
remove one element. `VarValue::Assoc` remains an insertion-ordered
`Vec<(Vec<u8>, Vec<u8>)>`. A scalar converts to either shape with the old value
at element/key zero; indexed↔associative conversion is
`Error::ArrayShapeMismatch` (bash refuses it).

**Array element removal needs word work, so the VM supplies it.** `unset
'a[$i]'` re-parses its operand as an assignment word, which is expansion, not
state. `ShellHost::unset_array_element(&mut self, operand) -> Option<Status>`
is the seam: the default returns `None` and `unset` keeps its "not implemented"
diagnostic; the VM expands the subscript and calls back into the store.

**`$@` and `$*` are not here.** `special_scalar` returns `None` for them because
they are multi-field; expansion reads `positional().argv` and applies the
quoting rules itself.

**Integer attributes store raw bytes.** Arithmetic evaluation happens in
`baash-lang`/the VM *before* `assign`; the store does not parse numbers.

**Construct `Opts` from the presets** (`Opts::POSIX`, `Opts::BASH`, `Default`).
It carries ~70 flags now; an exhaustive struct literal outside this crate will
break on the next flag.

**`ShellState::command_number` is `\#`, and the REPL advances it.**
`begin_command()` increments it when the interactive loop hands a line to the
interpreter; the prompt for the line about to be typed and `${x@P}` inside a
running line both report `command_number + 1`, which is bash's
`current_command_number` rule measured on 5.3.20 (prompt `\#=2` for a line
whose `${PS1@P}` says `3`). The evaluator does not touch it: it sees whole
chunks and cannot tell a REPL line from a script, so a non-interactive run
never advances the counter.

**History is one ring, and the database is behind it.** `ShellState::history`
is a `baash_history::History`: numbered entries, Up/Down navigation, ghost-text
suggestions. `baash_history::Store` is the Turso database on its own thread; the
ring queues a write when a line is submitted and an update when it finishes
(`$?` and wall time), so nothing on the keystroke path waits for a disk. The
interactive loop swaps the ring in and out of the editor around each command
(`baash-repl` depends on this crate for the *type*), so the built-in and Up/Down
navigate the same entries.

Numbers are the database's row ids: global, assigned at insert, and never
reused. The ring is seeded with the newest 1000 rows of every session,
`history` prints the ids, and `!12` resolves to the row with id 12 in any shell.
Trimming leaves numbers alone, deleting leaves a gap, and nothing renumbers.

Deletion is a tombstone. Turso hands out `max(id) + 1` — `AUTOINCREMENT` parses
but did not keep its high-water mark across a reopen in the multiprocess WAL
mode, so after `history -c` the next command was numbered 1 again while id 1
already meant `echo one` in another session. A real `DELETE` therefore cannot
be used if a number is to keep meaning one command. `-d` and `-c` set `deleted`,
reads filter it, and startup prunes hidden rows *below* the newest id, which
cannot shrink the maximum and so cannot cause reuse. The insert is the
one write the shell waits for — the id it returns *is* the number the line will
be shown with — and it costs 16 µs p50 / 86 µs max in release (260 µs p50 in
the debug build), which is below the noise floor of pressing Enter. `finish` is
fire-and-forget.

There is no history file, and there is no `sync_history`: `HISTFILE`,
`HISTSIZE`, `HISTFILESIZE`, `HISTCONTROL`, `HISTIGNORE`, `HISTTIMEFORMAT`, the
`histappend` shopt, `history -a/-n/-r/-w/-p/-s` and `fc` are deleted rather
than emulated. `History::push` records every non-blank line; `remove_current`
deletes the row again when history expansion replaces the typed line. `history
-c` empties the ring and deletes only *this session's* rows: the rest of the
table is the history, and it is back in the seed at the next start. A database
that cannot be opened is reported once and the shell keeps an in-memory ring,
because losing recall is better than refusing to start.

**Builtin tiers are exact and load-bearing** (DESIGN §2): 15 special, 15
intrinsic, 7 regular, 18 bash extensions. `fc` is not registered at all: it
edited a history file that no longer exists. `printf`, `test`, `[`, `echo`,
`pwd`, `true` and `false` are regular, deliberately *not* intrinsic, because the
standard requires them to remain exec-able. Search order comes from
`Tier::search_priority`.

**Traps are state, and the table lives here.** `ShellState::traps` is a
`TrapTable` keyed by `TrapKey` (`Exit`, `Signal(i32)`, `Err`, `Debug`,
`Return`) with `TrapAction::{Command(Vec<u8>), Ignore}`. It parses and renders
signal specifications (`signal_number`, `signal_short_name`, `signal_table`),
iterates in bash's listing order (`EXIT`, signals ascending, then
`ERR`/`DEBUG`/`RETURN`) and bumps a `generation` on every mutation so the VM
knows when to re-sync process dispositions without diffing. `trap` itself is a
state-only handler here. It implements bash's surprising surface rule: when the
*first* operand is a signal specification there is no action, and every operand
is a signal to reset (`trap 0 2`, `trap 2 EXIT`); otherwise the first operand is
the action and `''`/`-` mean ignore/reset. A failing `trap` records a
**utility** failure on purpose — the §2.8.1 special-built-in rule would
otherwise exit the shell, and no shell does that for `trap 'x' SIGINVALID`.

**`ShellHost` grew two seams for `trap`.** `check_syntax(src) -> bool` validates
a handler at registration time (parsing is `baash-syntax`'s, which this crate must
not depend on), and `trap_saved_status() -> Option<Status>` answers the exit
status a bare `exit` inside the `EXIT` trap must reuse. Both are defaulted, so a
new host compiles without them.

**Diagnostic text is unstable; match the codes.** State errors are `BAASH-E05xx`;
an unimplemented builtin returns status 2 with `BAASH-E0508`.

---

## baash-lang

**One object implements both traits.** `Vars` (read variables, positional
parameters, write) and `Shell` (the callbacks expansion needs) must be
implemented on the same type, because expansion takes `&mut S` where
`S: Vars + Shell`.

```rust
pub trait Vars  { fn get(&self, name: &[u8]) -> Option<&[u8]>;
                  fn positional(&self) -> &[Vec<u8>];
                  fn set(&mut self, name: &[u8], value: Vec<u8>) -> Result<(), SetError>; }
pub trait Shell: Vars {
    fn command_substitution(&CommandSub) -> Result<Vec<u8>, Error>;
    fn arithmetic(&Arithmetic) -> Result<Vec<u8>, Error>;
    fn pathname(&Field) -> Result<Option<Vec<Vec<u8>>>, Error>;
    // defaulted: process_substitution -> Unsupported, home_dir
}
```

- `get` must serve the specials `?`, `-`, `$`, `!`, `0` as ordinary lookups;
  `$#` is computed from `positional()`. `None` (unset) and `Some(b"")` (null) are
  different values and the difference is observable under `${x-}`, `${x:-}` and
  `set -u`.
- `pathname` returning `None` means *no match* — leave the word alone. Returning
  `Some(vec![])` would mean "matched nothing and vanished", which is `nullglob`
  behaviour and not the default.
- `expand_words` returns `Vec<Vec<u8>>`. If the VM wants `SmallVec` argv it
  collects into it; the expansion layer does not guess the allocation strategy.
- `quote(bytes, ctx)` covers `Unquoted`, `DoubleQuoted` and `SingleQuoted`, and
  emits `$'\n'` for an unquoted newline (a bare backslash-newline is a line
  continuation and would corrupt the line). `baash-complete` must use this rather
  than its own escaping.

**Two properties, deliberately separate.** A part is *quoted* (glob-inert) or
not, and separately a part's bytes are *expansion results* (splittable) or not.
Literal source text is unquoted but never field-split — `IFS=:; a:b` is one
word — while unquoted literal text inside a parameter replacement word is
splittable — `${x:-a:b}` is two fields. Collapsing these into one boolean is the
classic way to get IFS subtly wrong.

**Arrays are part of the word model.** `ParamName::Element { name, subscript }`
is `${a[i]}` and `ParamName::Elements { name, star }` is `${a[@]}`/`${a[*]}`;
both carry the array's name. The subscript is a `Word` because it is expanded
first; for an indexed array the result is then evaluated with
`Shell::arithmetic_value`, for an associative array it is the key verbatim.
`ParamOp::Length` covers `${#a[@]}` and `${#a[i]}`, `ParamOp::Indices` is
`${!a[@]}`, `ParamOp::Indirect` is `${!a[i]}` (the element names a variable),
and `ParamOp::ArraySlice` carries the two bounds as words. `Vars::array(name)
-> Option<ArrayValue<'_>>` is how expansion sees the storage: an
`ArrayValue::Indexed(&BTreeMap<i64, Vec<u8>>)` in index order or
`ArrayValue::Assoc(&[(Vec<u8>, Vec<u8>)])` in insertion order. A scalar is a
one-element array for reads, and a bare `${a}` decays to element/key zero.

**Quoted and splittable, also for arrays.** `"${a[@]}"` uses the same
`Expanded::break_field` boundaries as `"$@"` — one field per element, zero
fields when empty — and `"${a[*]}"` joins with the first `IFS` character.
Unquoted forms are a single splittable expansion result. No third path was
introduced.

**Pattern operators carry their patterns as `Word`s** and match through
`baash-pattern` via `Shell::pattern_flags()`: `RemovePrefix`/`RemoveSuffix
{ pattern, longest }`, `Replace { pattern, replacement, mode }` with modes
`First|All|Prefix|Suffix`, and `Case { how, pattern: Option<Word> }` for
`${x^pat}`/`${x^^pat}`/`${x,pat}`/`${x,,pat}`. Case conversion applies
element-wise to `$@` and `a[@]`, like every other list operator.

There is no quote mask in the matcher, so quoted pattern bytes reach it as
backslash escapes (a quoted backslash becomes `\\`). Two rules follow from
bash and are tested: the enclosing double quotes do **not** quote a pattern
(`"${x##$p}"` behaves exactly like `${x##$p}`), and tilde expansion of a
pattern word ignores them too.

Decisions, each verified against bash 5.3.20 under `LC_ALL=C`: an empty pattern
strips nothing; in substitution an **empty match is never replaced** (copy a
byte and advance), while an empty pattern under `/#/` prepends and `/%/`
appends; the pattern/replacement separator is the first *unquoted* `/` after a
word parse; the four strip operators force `nocasematch: false` whatever
`shopt` says; case conversion matches one character at a time, ASCII only, in
byte units.

**Unsupported is still an error, not a guess** for what remains: `${!x}` on a
non-array, `${x@Q}`/`@E`/`@u` (every transformation except `@P`), and process
substitution return `ErrorKind::Unsupported` with the operator's span.

**Prompt strings have one expander.** `baash_lang::prompt::expand(template,
context)` decodes `PS1`-style backslash escapes into an `ExpandedPrompt`:
`bytes()` plus `non_printing()`, the byte ranges that came from between `\[`
and `\]` and must not count toward the display width. The line editor renders
`PS1` with it and `${x@P}` uses the same function, so the prompt a user sees
and the string a script gets cannot drift.

- Escapes, measured against bash 5.3.20 in `LC_ALL=C`: `\a \e \r \n \t \T
  \@ \A \d \D{…} \h \H \u \w \W \s \v \V \$ \# \! \j \l \nnn
  \\ \[ \]`. An unknown escape (`\z`) keeps its backslash, as does a
  trailing lone one. `\nnn` consumes up to three octal digits and fails *as a
  whole* when any byte in that three-byte window is not octal (`\1x` stays
  literal), wraps modulo 256, and drops a zero byte (`\400` is empty).
  `\D{}` means `%X`; `%V` is not implemented and formats as itself.
- `\$` is the prompt character, not a literal `$`: `#` for root, `$`
  otherwise. With `prompt_vars` (bash's default) the non-root form is written
  `\$` so the second pass removes the backslash; that is the whole
  `PS1='$'`/`'\$'`/`'\\$'` table.
- Inserted values (user, host, `\w`, `\s`, time) are double-quote escaped when
  `prompt_vars` is on, because a directory named `$x` must not be expanded by
  the second pass.
- `\[`/`\]` are removed from the bytes. An unmatched `\[` hides the rest of
  the string; a stray `\]` is ignored; a `\[` while one is open does not
  nest. The renderer consumes `non_printing()`; a caller that only needs bytes
  uses `into_bytes()`.

**The stock `PS1` renders as the fish-like default prompt.** The `PS1`
*variable* keeps bash's `\s-\v\$ ` value — `${PS1@P}` and the conformance case
that reads it still decode bash's prompt — but `Interp::render_prompt`
substitutes `baash_lang::prompt::fish_prompt` while the value is still that one.
The rendered shape is

```text
~/src/bshell main !2 ?5 $
```

In a session that came in over `ssh`, `user@host` comes first:

```text
barnabas@jupiter ~/src/bshell main !2 ?5 $
```

The marker is any of `SSH_TTY`, `SSH_CONNECTION` or `SSH_CLIENT` being set and
non-empty — `sshd` sets the first for an interactive login and the second for
every session, and a nested shell or tmux inherits them, so all three mean the
keyboard is not at this machine's console. The host is cut at the first dot
(a prompt does not need the FQDN, and `\h` does not print one). Nothing else
decides it: not `$TERM`, and not whether the terminal is local. The segment is
one ANSI colour, yellow, and it is captured once per line, so a session cannot
change remotely halfway through one.

`cwd` (a leading `$HOME` abbreviated to `~`, bytes unchanged), then the git
branch, then `!N` tracked files that differ from `HEAD` or the index when
`N > 0`, then `?N` untracked files when `N > 0`, then `$` (or `#` for root),
each preceded by one space, and one space after the marker. A component's space
is omitted with the component, so a clean repository is `~/src/bshell main $ `
rather than a double space, and the prompt always ends with the marker `\$`
would have produced.

The counts come from `baash_lang::prompt::git_status`, which opens the
repository with `gix` (gitoxide: index and worktree in process) and counts
*distinct paths*, so a file changed in the index and again in the worktree is
one dirty file rather than two. Nothing here forks a `git status`.

On a terminal the prompt is painted in ANSI SGR colours only — never 24-bit
values — so it follows the terminal's palette: cyan for the path, bright cyan
for the last component, green for the branch, `!N` and the marker, blue for
`?N`, red for `#`, yellow for `user@host`. `NO_COLOR` or a non-terminal gets the same text with no
escapes.

The branch is `baash_lang::prompt::git_branch`: walk up for `.git` (a directory,
or a file whose `gitdir:` names the real one for a linked worktree or
submodule), read `HEAD`, and show `refs/heads/<name>` as the branch. Any other
symbolic ref is resolved through loose refs, `commondir` and `packed-refs` and
shown as a seven-character commit id; a detached `HEAD` is a raw object id and
is abbreviated directly. It is all file reads — no `git`, no fork. An
unreadable or absent repository omits the branch, never guesses.

The default is built directly from data, not decoded as a prompt template, so
a directory or branch containing `$`, a backslash or a `\u`-looking sequence is
painted once and cannot be expanded again. **A user-defined `PS1` always
wins**: the environment, an rc file or an assignment at the prompt replaces
the value and the normal template path renders exactly those bytes. The REPL's
legacy `[branch]` segment is appended only to user-defined prompts, because the
stock prompt already contains the branch inline.

`expand_with_variables(template, context, shell) -> ExpandedPrompt` is the
two-pass form: it decodes the escapes, then — when `context.prompt_vars` is set
(`shopt promptvars`, bash's default) — expands the result as if it were
double-quoted. `${x@P}` and the interactive `PS1`/`PS2` both go through it, so
they cannot drift. The second pass expands `$name`, `${name}`, `${#name}`,
`$@`, `$*`, the specials, `$((…))`, `$(…)` and backticks through the `Shell`
callbacks, and applies double-quote backslash removal. Measured against bash
5.3.20: a failing command substitution contributes nothing (its error goes to
stderr, the status is untouched) and a failing arithmetic expansion keeps its
text. Forms with an operator this scanner does not implement (`${x:-word}`,
`${a[0]}`) are copied through **literally** rather than guessed at; a full
implementation needs the word parser, which is the front end's job.

`prompt_vars` off is exactly `expand()`: no second pass, no double-quote
escaping of inserted values, and `$PWD` in `PS1` prints as `$PWD`. `\!` and
`\#` expand in both modes, because they are first-pass escapes. The second
pass runs **per non-printing stretch**, so `non_printing()` indexes the final
bytes: `PS1='\[\e[1;32m\]\u@\h\[\e[0m\] \w\$ '` keeps two ranges
(0..7 and 14..18 after `ada`/`lab`), which is what lets the editor put the
cursor after a coloured prompt that also contains a variable. `ExpandedPrompt`
is `Clone + PartialEq + Eq` and has `bytes()`, `into_bytes()`,
`from_bytes(plain)`, `non_printing()` and `is_non_printing(offset)`.

The editor does **not** expand anything itself: the shell renders both prompts
before each line (it is the only party with the variable table and an
interpreter for `$(…)`). `PromptContext::default()` reports the crate version
for `\v` (`major.minor`) and `\V` (full), and `shell` as `baash`; root, user,
host, home and cwd come from the process when the caller does not override
them.

**The transform letter is a `Shell` seam.** The parser classifies every
`${x@…}` as `ParamOp::Unsupported(UnsupportedOp::Transform)`; it does not copy
the letter into the word. `ParamOp::Transform(Transform)` is the direct form
(the fixture parser produces it), and expansion resolves the generic form with
`Shell::transform_operator(&span) -> Option<Transform>`. `baash-vm` must
implement it by reading the byte after the last `@` from the chunk being
evaluated — `self.line_source`, **not** `self.current`, because a command
substitution evaluates its chunk without replacing `current`:

```rust
fn transform_operator(&self, span: &baash_lang::Span) -> Option<baash_lang::Transform> {
    let text = self.line_source.bytes().get(span.clone())?;
    let at = text.iter().rposition(|&byte| byte == b'@')?;
    baash_lang::Transform::from_letter(*text.get(at + 1)?)
}
```

The default returns `None`, so a `Vars` source with no source text keeps the
honest unsupported error. `Shell::prompt_context() -> PromptContext` has a
default built from the process environment (user from `/etc/passwd` by
effective uid, host from `/proc`, cwd from `current_dir`, local time from
TZif). `baash-vm` overrides it with what only it knows: the logical
`$PWD`/`$OLDPWD`/`$HOME`/`$USER`/`$HOSTNAME`, the background-child count
(`\j`; there is no job table),
`prompt_vars` from `shopt`, last-failed, root, `\l` (the tty basename from
`/proc/self/fd/0`, `tty` when stdin is not a terminal) and the two numbers
below. An interactive shell also starts with bash's `PS1='\s-\v\$ '` and
`PS2='> '` as *variables* (unexported), which is what `${PS1}` and `${PS1@P}`
read; `unset PS1` renders an empty prompt, like bash.

**History expansion is a pure text transform in this crate.**
`baash_lang::histexpand` turns `(typed line, &[Entry], &mut Session)` into an
`Expansion { text, changed, print, print_only }` or an `Error { kind, token }`.
It does no I/O and knows nothing about options or the interpreter; the binary
owns the policy (interactive, `histexpand` and `history` on) and the printing.

- `Entry::new(number, text)` takes the history number (`history -n` is not the
  index) and the text; the caller passes the ring *without the line being
  typed*. `Session` carries what bash keeps across lines: the last
  substitution (`:&`, empty `:s//new/`), the last `!?string?` search and its
  matched word (`!?` with no operand, `%`), the quote state that spans `PS2`
  lines, and the here-document queue whose bodies are never expanded.
- `Session::expand_line` is the one-line form (interactive stdin);
  `Session::expand` preserves newlines and expands each physical line with the
  same state. `Session::abandon_command` clears the quote/here-doc state after
  a failed event, because bash abandons that command.
- `changed()` is false when the line contains no expandable `!`, which means
  the caller must not record, echo or prefill anything. `print()` is a `:p`
  that appeared; `print_only()` is true only when the *last* modifier was
  `:p`, because bash executes `!!:p:q` after printing it (measured).
- Syntax: `!!`, `!n`, `!-n`, `!string`, `!?string?`, `!#`; word designators
  `0`, `n`, `^`, `$`, `*`, `%`, `x-y`, `x-` (up to but excluding the last),
  `-y`, `x*`; modifiers `h`, `t`, `r`, `e`, `p`, `q`, `x`, `s<d>old<d>new<d>`,
  `gs`/`as`, `&`, `g&`; and quick substitution `^old^new^` at the start of a
  line (a remainder starting `:` continues as modifiers, anything else is
  appended).
- Measured bash 5.3.20 rules that the implementation encodes: single quotes
  and `$'…'` inhibit expansion, double quotes do not (but `!"` before the
  closing quote is literal), an odd run of backslashes escapes the `!`, `#`
  starts a comment only at a word start, and `!` followed by a blank,
  newline, `=`, `(`, `)`, `;`, `&`, `|`, `<` or `>` is literal. Words selected
  by a designator are split quote-aware (the quote bytes are kept) and
  rejoined with single spaces; `h`/`t`/`r`/`e`/`q` then apply to the whole
  joined result, while `x` splits it again. `$` falls back to word 0 for a
  one-word event; `*` is empty there; `^` out of range is `bad word
  specifier`.
- There is **no `!{…}` event form in bash**: `!{echo}` is an event named
  `{echo}` and fails like `!{xyz}`. That is the implemented behaviour; the
  zsh-style brace form was deliberately not invented.
- Error messages are the bash shape (`!tok: event not found`,
  `:spec: bad word specifier`, `c: unrecognized history modifier`,
  `:s…: substitution failed`); the caller prefixes the shell name. A failed
  expansion preserves `$?` and abandons the line.
- Not implemented: `:G` (per-word substitution) and `histchars`; `histreedit`
  is accepted by `shopt` but not wired, while `histverify` is the binary's.
- `history -p` is **not** routed through this module yet (it still prints its
  operands unchanged); see `docs/state.md` §4.

**Brace expansion is stage 0, and it is the only switchable stage.**
`expand_braces(&Word) -> Vec<Word>` runs before tilde expansion in
`expand_word`/`expand_word_with` when `Shell::brace_expansion()` is true
(default true, bash's setting); `Mode::Assignment` never runs it, because bash
does not brace-expand an assignment value (`v={X,Y}` stays literal). The stage
works on `Part`s and never flattens to bytes: only `LitKind::Unquoted` bytes
are structural, so `"…"`, `'…'`, `\x` and every opaque part (`Param`, `CmdSub`,
`Arith`, `ProcSub`, `Tilde`) are inert — that is what keeps `${x:-{a,b}}`
literal, since the replacement word is a separate expansion unit. Alternatives
are slices of the original parts (their quoting kinds preserved) or, for a
range, new unquoted literals; parts the stage did not touch reach the later
stages unchanged and are expanded exactly once (`{a,b,c}-$((i++))` evaluates
the arithmetic once per word). `baash-vm` overrides `brace_expansion()` from
`opts.braceexpand`, which is the `set +B` switch.

Rules measured against bash 5.3.20 under `LC_ALL=C`: a group expands when its
contents hold a top-level comma or parse as `x..y[..step]`; an unmatched `{`
ends the scan for the rest of the word (`{{a,b}` stays literal, the suite's
primary, where bash expands the inner group; complete groups to its left still
expand, so `{a,b}_{` is `a_{ b_{`); range steps use their magnitude with the
endpoints' direction, and zero means one; zero padding is triggered by a
leading zero on an operand (a leading `+` suppresses it) with the width of the
wider operand including its sign; character ranges need both endpoints to be
single ASCII alphabetic bytes. A range that does not parse (overflow, quoted
endpoint, `0x1`, `1...3`) leaves the group literal rather than erroring.

**Errors** are `baash_lang::Error { kind, span }` with `Display`, `code()` and
`diagnostic() -> baash_diag::Diagnostic`. Codes 301–308 are provisional; 302 is the shell-wide `CAPABILITY` code.

Documented deviation: for unquoted `$@` inside a `${…}` replacement word we
apply the ordinary unquoted rules, where bash space-joins and suppresses
splitting. Every DESIGN §3.3 Tier C case, including `${x:-"$@"}`, matches
bash 5.3.20.

---

## baash-syntax

**Two entry points, and the difference matters.** `lex(src) -> Vec<Token>` is the
token stream for highlighting and simple scans. `lex_full(src) -> Lexed` adds
what the parser needs: `{ tokens, heredocs, unterminated }`. Here-document bodies
and delimiters only appear as `TokenKind::HereDocBody` /`HereDocDelimiter` in
the full form; a consumer that colours them must use `lex_full`. Bodies are
**not** shell code and are styled as literal data, not as commands.

**Words are `baash_lang`'s types, re-exported.** `baash-syntax` re-exports
`Word`, `Part`, `Lit`, `LitKind`, `ParamExp`, `ParamName`, `ParamOp`,
`CommandSub`, `Arithmetic`, `ProcessSub`, `TildeSpec`, `UnsupportedOp`. There is
no parallel word model, deliberately.

**The AST is owned and flat**: `Ast { root, nodes: Vec<Node>, heredocs, tokens }`
with `NodeId` indices, not a boxed tree and not an arena. Two consequences:
recursion on the AST is bounded by construction, and the parser enforces an
explicit `max_depth` (default 200) that yields a diagnostic instead of a stack
overflow. Node kinds: `Empty, List, AndOr, Pipeline, Simple, If, For,
ForArith, While, Case, BraceGroup, Subshell, Function, Redirected, ArithCommand,
TestCommand, Timed`.

**`ParseOpts { posix_mode, extglob, max_depth }`** is local to the front end on
purpose; the shell maps its own options onto it. `parse(&SourceFile)` is a
default-profile wrapper over `parse_with`.

**`NeedMoreInput` is the PS2 signal**, produced for an unterminated quote,
substitution, `(`, `{`, `if`, `do`, `case`, here-doc, redirect target, and
dangling pipeline/and-or tails — each with the opener's span and the `Expect`
set.

**Expectations answer for the cursor.** `expectations_at(src, offset, opts)`
reports what may be typed *at* `offset`, not after the token that ends there:
`ec` at 2 is `Command`, `ls ` at 3 is `Word`, `for f in ` at 9 is `ForList`.
Ordering is most-specific first: a compound's own keyword precedes the generic
command slot (`if true; ` → `[Then, Command]`), so read the first entry for the
headline and the last for the innermost slot the cursor is in.
`expectation_sites_at` is the same answer with the span that governs each
expectation. `expectations_after` keeps the old "slot after the prefix" answer
for callers that need it. Inside an unclosed `[[ … ]]` the answer is
`Expect::TestOp`, and its site's span is the operator just typed (or being
typed) — no consumer counts unbalanced `[[`. A redirection target reports
`Expect::RedirectTarget` whether the target has been typed or not. The closing
keywords are reachable: `Then`, `Do`, `Done`, `Fi`, `Esac`, `In` and
`FunctionBody` appear once a list boundary makes them legal.

**Open constructs are a stack.** `lex_full` returns
`Lexed { tokens, heredocs, unterminated, open }`; `open` is the chain of
constructs still open at end of input, **innermost first**, each with its
opener span and `content_start` (the first byte of the construct's content).
`open_constructs_at(src, offset)` is the same chain at an arbitrary offset.
`Unterminated` carries the same `content_start`, and its `opener` is the whole
opening delimiter (`$'`, `${`, `$(`, `$((`, `<(`, `((`, the quote byte, the
backtick). No consumer encodes opener widths, and a word containing a
substitution is still one token: the chain is the nested view.

**Redirections carry their split.** A redirection is still one token, but
`redirect_op_len(text)` (and `Token::redirect_op_len`) give the length of the
file-descriptor prefix plus the operator, and `Token::redirect_target` returns
the fused target's span. The operator set is defined once, in `baash-syntax`;
consumers must not carry their own.

**Tokens have simple-command roles.** `classify_tokens(tokens, src)` returns a
`TokenRole` per token, in order: `Assignment`, `CommandWord`, `Argument`,
`RedirectOperator`, `RedirectTarget`, `Boundary` (an operator, newline or
reserved word that ends the command) or `Filler` (layout, comments,
here-document data). Command boundaries come from the grammar, not from a
keyword table in the consumer, and a separate redirect target is not an
argument.

**One word can be parsed on its own.** `parse_word(src, opts)` and
`parse_word_at(src, offset, opts)` consume exactly one word — for `[[ … ]]`
operands, `case` patterns (without the terminating `)`) and `$…` constructs
inside here-document data — and reject a second word or an operator with
`ParseWordError::{Missing, Trailing, Unterminated, Invalid}`. Every span and
diagnostic is relative to the caller's `src`; nothing is synthetic. Leading
layout is skipped, trailing layout allowed.

**Array literals are structured.** `Assignment.array:
Option<Vec<ArrayElement>>` and `SimpleCommand::array_args: Vec<ArrayArg>` carry
`ArrayElement { span, index: Option<ArrayIndex>, value: Word }`;
`ArrayIndex { subscript: Word, append }` is the `[k]` / `[k]+=` prefix. The
distinction is structural on purpose: `[k]=$v` is one scalar element while a
bare `$v` field-splits. `ArrayArg` additionally carries the variable `name`
span and its own `append`, and the argument in `words` is still a quoted
placeholder holding the raw text (the VM matches it). The parser only accepts
array-literal arguments after `local`/`declare`/`typeset`/`export`/`readonly`/
`alias`/`eval`, matching bash.

**Subscripts may contain spaces and `=`.** `a[1 + 2]=v` is stitched back
together from several tokens (bash's parser does the same), and
`a[x=1]=v`'s assignment `=` is found after the closing bracket.
`a[$(cmd)]=v` is still refused by design (DESIGN §3.3), but `${a[$(cmd)]}` is
not: the expansion subscript is an ordinary word.

**Codes (1xx)**: 1 unterminated, 2 need more input, 3 unexpected, 4 bashism,
5 array subscript, 6 depth, 7 here-doc.

Known limits, accepted rather than hidden: `select` and `time` parse as command
words; `$"…"` gets double-quote semantics without locale translation; the
`$( … )` end scanner tracks `case` state heuristically, so a *pattern* literally
named `esac` can confuse it; CRLF scripts fail the way bash fails; and
`a[$(cmd)]=v` is refused by design (DESIGN §3.3), so scripts relying on
`shopt eval_unsafe_arith` will not parse.

---

## baash-complete

**Call `apply`, do not re-implement insertion.** `Menu::insertion(i)` returns
already-quoted bytes (trailing `/` for a directory, trailing space for a unique
non-directory), and `Menu::apply(&mut line, i)` splices it in and returns the new
cursor position. The editor must use these: quoting comes from
`baash_lang::quote` with the context the parser derived, and a second
implementation of that logic will disagree.

**`replace` is not `word_span`.** `replace` starts *after* an opening quote,
`${`, `((` or `=`, so inserting never deletes the opener. The editor replaces
`replace`, not the word.

**Ranking tiers are semantic, not advisory.** `tier` 0 = exact-case prefix,
1 = caseless prefix, 2 = fuzzy (`score` holds the nucleo score). The vector is
already ordered by (tier, frecency, score). If a consumer filters, it must
preserve order; it must not re-sort, because typed-prefix matches outranking
fuzzy ones is the product promise (§6.5).

**The snapshot traits are `Send + Sync` and return owned data.** `VarNames::names`,
`History::entries`, `ShellNames::functions`/`aliases` all return
`Vec<Vec<u8>>`. `baash-runtime` therefore cannot hand over a borrow of live
interpreter state; it must supply a snapshot or a `Mutex`-backed view. This is
deliberate: completion runs while the editor holds the terminal, and it must not
observe a half-mutated shell.

**After `cd`, call `set_root`.** `invalidate_commands` is only needed for the
pathological case; the `PATH` index already notices changes. The binary does
both the after-command `set_root` and a full refresh before every prompt, so a
`PWD` set by an rc file or by `PROMPT_COMMAND` is also honoured.

**The engine outlives the prompt; its sources do not.** The editor borrows
`&dyn Completer` once for the whole interactive loop, so every mutable source
has a `set_…(&self)` seam — `set_vars`, `set_shell_names`, `set_history`,
`set_case_tags`, `set_specs`, `set_path_var`, `set_home` — and the shell
replaces the snapshots in `refresh_prompt`. `set_path_var` is a no-op when the
value is unchanged, so a per-prompt refresh does not throw away the `PATH`
index. The consuming `with_…` builders are for tests and one-shot engines.

**Command position is a deduplicated union.** `PROVIDERS` answers
`Expect::Command` with aliases, functions, builtins, keywords, `PATH`
executables, variables and history; `Candidate::dedupe` keeps one entry per
insertion text, at the position of the first provider (bash's order: aliases,
functions, builtins, keywords, `PATH`). Keywords also answer a wrapper's
argument (`sudo wh<TAB>`), which is a command slot. `$HO` is a variable slot: a
candidate that starts with `$` is `Insertion::Source`. Inside `${…}` a bare
name is offered as `NAME}`, because bash closes the expansion (`${PA<TAB>` →
`${PATH}`) and a name without the brace would not re-parse.

**Programmable completion is data.** `Spec` is what the engine needs from a
`complete` rule (`-W` words, `-A` names, `-P`/`-S`, `-o` options, the raw `-F`
name) and `SpecSource::spec(command)` is the lookup. `baash-runtime` owns the
registry and this crate must not depend on it, so `crates/baash/src/complete.rs`
translates a cloned `CompletionTable` into `Spec`s. Rules are consulted only
for an argument slot: not the command word, not a redirection target, not an
assignment value.

`Action` maps the `-A` names this crate can generate onto its own providers:
`alias`, `arrayvar`/`export`/`variable` (bare names, no `$`),
`builtin`/`helptopic`, `command`, `directory`, `file`, `function`, `keyword`,
`signal`. The rest (`setopt`, `shopt`, `user`, `group`, `hostname`, `job`,
`service`, `binding`, `disabled`, `enabled`, `running`, `stopped`) need shell
state this crate does not have and offer nothing; `complete` still accepts
them. `-P`/`-S` decorate *after* matching (`compgen -P pre -W 'one' -- o` is
`preone`, `-- pre` is nothing). `-o nospace|default|bashdefault|dirnames|plusdirs`
are applied; `filenames` is accepted and has no effect (`-W` words and `-A`
results are already inserted as literals), and `noquote`/`fullquote` are
accepted and ignored.

`-W` word lists are split on the shell's `IFS` by the bridge and **not
expanded**: expansion is shell code (`$(…)` would run on every prompt refresh),
which is the same blocker as `-F`. `-F`, `-C` and `-G` are not run from the
completer at all; a rule whose only source is one of them is reported as
absent, so the command keeps the default completion instead of losing it.
`compopt` therefore changes nothing at the prompt yet (it only becomes
observable together with `-F`).

`cd`, `pushd`, `popd`, `rmdir` and `mkdir` get a built-in rule expressed as
data: `-A directory` with `-o default`, so `cd /tm<TAB>` offers `/tmp/` and
`cd $HO<TAB>` still falls back to the variable provider.

**Ranking uses nucleo's greedy matcher.** `nucleo-matcher` 0.3.1's optimal
matcher panics (`should have been caught by prefilter`) on ordinary ASCII
subsequences such as `HOSTNAME` ← `HOM`, and a menu candidate must never take
the shell down; greedy matching is `O(n)` as well. Tiers and frecency — not the
fuzzy score — are the product promise, so this is a robustness choice, not a
behaviour change.

**`Candidate::display` is not sanitised.** A hostile filename can carry
newlines or control bytes into the menu. The editor must flatten it when
painting.

**The context is derived, not guessed.** `context` uses
`open_constructs_at` for the nesting stack, `expectations_at` for the slot at
the cursor, `Token::redirect_op_len` for a fused redirection, and
`classify_tokens` for the command word and argument index. It does not
re-enter substitution bodies, count `[[` tokens, or carry a redirection
operator table. `Ctx::expect` is the last entry of `Ctx::expects` (the
innermost slot); the earlier entries are the construct's own keyword
candidates.

Known copies: the builtin-name and keyword lists are duplicated from
`baash-runtime` because the dependency runs the other way, and `SIGNALS` is
spelled the way bash's `compgen -A signal` prints it (`SIGHUP`, `EXIT`, …).
`compgen -A signal` in this shell still prints the runtime's short names
(`HUP`); that existing inconsistency is a `baash-runtime` fix, recorded here so
the completion menu is not "fixed" to match the wrong spelling.

---

## baash-vm (tree evaluator)

**Which built-ins the executor owns.** `break`, `continue`, `return`, `exit`,
`exec`, `eval`, `.`/`source`, `cd`, `echo`, `read`, `wait` and `kill` are
implemented in `baash_vm`; `jobs`, `fg`, `bg` and `disown` are registered there
only to reject job control. They unwind control flow, re-enter the interpreter,
or need the descriptor table, `lseek`, termios and signal delivery, which a
`fn(host, argv) -> Status` signature cannot express. The executor consults them only when `BuiltinDef::is_implemented()` is
false, so a runtime implementation takes over silently — deliberate, but it
means whoever implements one of those in `baash-runtime` must delete the
executor's body in the same change. `trap`, by contrast, *is* implemented in
`baash-runtime`: its work is table mutation, and the execution half stays here
(see below).

**Trap execution is a command-boundary poll.** Caught signals are blocked in the
shell thread and drained from a `signalfd` at the start of each simple command,
after a foreground child is reaped, and when the shell finishes; no native
handler exists, so nothing can touch interpreter state (§4.5). Ignored signals
are `SIG_IGN` (inherited by children, which is what `trap '' INT` promises).
`reset_signals` on a spawn plan comes from `TrapTable::caught_signals`, and
stays excluded for ignored signals. A forked child clears caught traps, keeps
ignores and unblocks the parent's reader mask. `EXIT` runs at the end of the
outermost `run_named` — inside the unwind, so `exit 42` in the handler decides
the process status — and from `Drop` for an interactive shell, whose editor loop
is outside the VM. `$?` inside `EXIT` is the pending status, every other handler
preserves `$?`, a bare `exit` in `EXIT` reuses the saved status, and a trap never
re-enters itself. `SIGINT`/`SIGQUIT` are ignored while waiting for a foreground
child, and *discarded* if they were trapped (bash's rule: `kill -INT` during
`sleep` runs no `INT` trap, while `USR1` runs afterwards).

**`read` never over-reads.** A seekable descriptor is read in one chunk and
rewound by the surplus with `lseek`; a pipe is read one byte at a time because
bytes cannot be given back to it. `Interp::read_buffers` only holds a surplus
whose rewind failed, and a forked child starts with it empty.

**There is no job control, and that is a contract.** `bg` children are real
children remembered only by pid in `Interp::background` (pid → status, so `$!`
names the last one and `wait PID` can report a child reaped several commands
ago; a waited status is consumed, so a second `wait PID` is 127). There is no
job table, no `%N`/`%%`/`%prefix` resolution, no process-group handover and no
stopped state. `jobs`, `fg`, `bg` and `disown` stay registered so `type` and
command completion still find them, and running one reports `BAASH-E0302`
(`CAPABILITY`) naming the construct: `jobs: job control is not implemented in
this shell`, status 1. A `%` operand to `wait`/`kill` gets the same diagnostic
and status 1 (`wait: %1: job control is not implemented in this shell`), not
"no such job". The diagnostic code is `302` (`CAPABILITY`), allocated next to
`UNSUPPORTED = 301` in `baash-lang`; `PARAMETER_NULL`, which held 302, moved to
308 so the pivot's `BAASH-E0302` is unambiguous. `set -m`/`set -o monitor` is
accepted and inert — the option parses and reads back, and nothing consumes
it, so an rc file that sets it does not break. Because the
shell does not put children in their own group, `Ctrl-Z` stops the child *and*
the shell, which is bash's behaviour with `set +m`.

**One foreground wait path.** `run_external`, `run_pipeline`, `eval_subshell`,
command substitution and completion capture all call
`wait_foreground_children`: it holds the `SIGINT`/`SIGQUIT` ignore across the
waits, and there is no terminal to hand over or restore. `reap_background` is
the non-blocking drain and does nothing at all when this shell has no tracked
child: `waitpid(-1)` is process-wide, and the integration tests run several
interpreters in one process.

**`exec` is a real `execve`.** `baash_sys::exec(plan)` replaces the process; the
VM maps its `io::Error` to 127/126 and exits. See the `baash-sys` section for why
a failed `exec` is not a rollback.

**The VM owns `PWD`/`OLDPWD` around `baash_sys::chdir`.** Descriptors saved while
a built-in's redirects are applied are `CLOEXEC` copies above fd 20, so a child
spawned from inside a redirected built-in cannot inherit the shell's saved copy.

**Fragments go through `baash_syntax::parse_word`.** `[[ ]]` operands, `case`
patterns and the `$…` constructs in a here-document body are single words, so
they use the real front end with caller-relative spans. (An earlier revision
synthesised `": <fragment>"` and took `words[1]`; that is gone.)

**Here-document body expansion lives here**, as a double-quote-like scanner, and
is a candidate to move into `baash-lang` once a here-doc body has a word model.

**`$?`, `$-`, `$$`, `$!` are cached** and refreshed at command boundaries,
because `Vars::get(&self)` cannot compute them. A runtime-owned `special()`
accessor would remove the bookkeeping.

**A forked child leaves through `baash_sys::exit_child(status)`**, never
`std::process::exit`: the latter runs libc `atexit` handlers inside a forked
child. `fork_child_status(f: impl FnOnce() -> i32)` does this for you.

**Status precedence**: an explicit exit request outranks the POSIX §2.8.1
consequence table — `exit 7` is 7, not 1.

**`pipefail` takes the *rightmost* failing status**, and is sampled at pipeline
start (DESIGN §2).

**Arrays are executed here.** The parser's `ArrayElement`s reach the executor
through `AssignmentAction`; `exec_simple` keeps declaration-command literals on
an `Interp` side channel (`ArrayLiteral`, keyed by the placeholder text) so
`local`/`declare` can apply `-a`/`-A` first. `Interp` also implements
`baash_lang::Vars::array` (a view over `VarValue`) and
`ShellHost::unset_array_element`, and its arithmetic `IntVars` exposes element
lvalues, so `(( a[99]=1 ))` and `${a[i+1]}` share one evaluator. A variable
whose value is not a number is recursively evaluated as an expression, bounded
by `MAX_RECURSION`.

Deferred, and listed in `baash_vm::tree`'s `# Deferred`: `select`, `time`,
`coproc`, `set -u`/nounset, `$LINENO`/`$RANDOM`/`$SECONDS`/`$PIPESTATUS`,
`command`/`type`/`hash`/`builtin`, `getopts`, aliases, `mapfile`/`readarray`,
pattern parameter operators, `[[ =~ ]]`, process substitution, `~user`, and
`set -x`. An executable text file without a shebang is reported as 126 instead
of being retried as a shell script (POSIX requires the retry), which is what
blocks the three `$REPO_ROOT/spec/testdata/*.sh` trap cases.

**Platform constants come from `baash-sys`.** `baash_sys::{flags, SIGPIPE, c_int,
mode_t}` are re-exported so no other crate needs `libc`; `baash-vm` has no `libc`
dependency, which keeps "only `baash-sys` touches the platform" true.

---

## baash-repl

**The editor takes bytes and a `Terminal`; it does not know about the shell.**
`baash-repl` depends on `baash-lang` for the one prompt expander
(`baash_lang::prompt`) and on `baash-runtime` for exactly one *type*: the history
ring, re-exported as `baash_repl::History`, so the editor's Up/Down and the
`history`/`fc` built-ins cannot drift apart. It still does not depend on an
interpreter or parser crate — not even `baash-syntax` — and it constructs its own
`History::new()` in tests. Two traits carry everything else it needs from
above:

```rust
pub trait Terminal {                       // the pty wave implements these five
    fn read_byte(&mut self) -> io::Result<Option<u8>>;
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn size(&mut self) -> Size;            // Size { columns, rows }
    fn enter_raw(&mut self) -> io::Result<()>;
    fn leave_raw(&mut self) -> io::Result<()>;
}

pub trait Syntax { fn needs_more_input(&self, line: &[u8]) -> bool; }
```

`RealTerminal` opens `/dev/tty` (falling back to stdio) and uses
`baash_sys::RawMode` plus `window_size_fd`, because `baash_sys::window_size()` is
wired to stdout. A pty harness needs only those five methods; no editor change.

**Continuation goes through `Syntax`, implemented in the binary** with
`baash_syntax::parse_bytes`. The trait is deliberately thin, and that thinness has
a visible cost: it can say "more input" but not "incomplete, and now the user
wants to stop", so Ctrl-D on an empty `PS2` line abandons the command instead of
reporting the syntax error the way bash does. Widening it to a three-state
status (`Complete | Incomplete | Invalid`) would fix that; it is a seam, not a
wall.

**Raw mode is held only while `read_line` runs**, so a child command runs with
the terminal cooked. Without job control that also means `Ctrl-Z` reaches the
whole foreground process group, shell included.

**Key semantics**: the first TAB inserts the candidates' longest common prefix
when it extends the typed word, and otherwise opens the menu; the second TAB
opens the menu even when there was no further prefix to add. Inside the menu
TAB and Shift-Tab cycle (so do Up/Down and Ctrl-P/Ctrl-N), Enter inserts the
selection, and Esc closes. This supersedes the earlier "TAB accepts" rule: a
pager whose Tab both advances and inserts is readline's menu-complete, not
fish's behaviour. One candidate is inserted through `Menu::apply` without
opening a menu at all. Ghost text comes from history and is accepted with Right
or End; Esc dismisses it for the current line, and any edit brings it back.

The menu is columnar: entries are packed into as many columns as the terminal
width allows, descriptions start at one column across rows, and a narrow
terminal degrades to one clipped column rather than wrapping. It is painted
*below* the line and the cursor is put back on the line; scrolling within the
menu is by row and keeps the selected cell visible. At a blank command word
history contributes whole previous commands (newest first, fuzzy-ranked like
every other source), which is the fzf-like history picker; once a command word
exists only its words are candidates.

**A frame must fit on the screen, and the cursor ends up on the line.**
The renderer repaints the whole edit region — prompt, line, ghost text, menu —
by moving back to the region's first row, erasing to the end of the screen and
drawing again. That works because each frame is measured before it is painted:
`Frame::height` runs the same wrapping code and reports the rows the prompt,
line and ghost text need, and the editor gives the menu only `TERM_ROWS - that`
rows (dropping the ghost text, then the menu, when a short screen has no room).
A frame taller than the terminal would scroll its own first row out of reach,
and the next repaint would then start below the frame and paint a second prompt
under it — the editor must move the cursor *up* to the line, never down past
the last painted row, for the same reason. Regions taller than the screen (a
line or `PS1` longer than the terminal) are still painted, with the top
scrolled away; that is the one case that needs a viewport rather than a frame.

**The editor paints a rendered prompt; it does not expand one.**
`baash_repl::Prompt` holds two `baash_lang::prompt::ExpandedPrompt`s (built by the
shell with `Prompt::from_expanded`, or by `Prompt::new`/`from_template` for a
caller with no interpreter), and `Frame::prompt` carries their non-printing
ranges so `paint_prompt` writes hidden stretches without advancing the column.
`Editor::set_prompt(Prompt)` is called by the binary before *each* line, which
is what makes a `PS1` from an rc file, from the environment or from the command
just run take effect at the next prompt. There is no `set_cwd`: `PS1`'s `\w`
follows `$PWD` because the shell re-renders, not because the editor patches a
field. The editor *borrows* history through `Config`: the binary takes the ring
out of `ShellState`, hands it to the editor, swaps it back for the duration of
each command (and around `PROMPT_COMMAND`), and the editor's `finish` saves it
on exit.

**`Editor::set_initial_line(&[u8])` is the `histverify` seam.** It seeds the
next `read_line` buffer with the expanded command and puts the cursor at the
end; the prompt is the one already rendered, so the shell does not re-render
`PS1` or run `PROMPT_COMMAND` for the review prompt (bash 5.3.20, measured).
The line is submitted and recorded by the normal Enter path.

**`Ctrl-R` is a prompt swap, not a screen mode.** `Search` remembers the
original line and cursor, the query, the matched history index and whether the
last search failed; while it is active `draw` uses
`(reverse-i-search)`query': ` (or `(failed reverse-i-search)…`) as the prompt
and the line is the match, so wrapping, painting and cursor arithmetic are the
existing code. `Enter` submits the match, `Left`/`Right` accept it with the
cursor on the first byte of the match and then act normally, `Esc`/`Ctrl-G`
restore the original line and cursor, `Backspace` edits the query, `Ctrl-R`
walks older, `Ctrl-C` returns `ReadResult::Interrupted`, and any other key
accepts the match and is reprocessed. Matching is a substring search from the
newest entry downwards; an empty ring is a failed search, never a panic.

Deferred, with the list maintained at the top of `crates/baash-repl/src/lib.rs`:
search-match highlighting and `Ctrl-S`, vi mode, multi-column menus and the
full-screen picker, a `poll(2)` key timeout (a lone Esc is only distinguishable
at end of input), `SIGWINCH` reflow, mouse, incremental/semantic highlighting.
History expansion itself is the binary's (`baash_lang::histexpand`); the editor
only supplies the `set_initial_line` prefill used by `histverify`. The ring's
`HISTCONTROL`/`HISTSIZE` policies are wired in `baash-runtime`; the pty tests are
`crates/baash/tests/interactive_signals.rs`, `interactive_history.rs`,
`interactive_history_expansion.rs` and `interactive_prompt.rs`.

---

## baash-pattern

One matcher for every pattern in the shell: `${x#pat}`, `${x%pat}`, `case`,
`[[ x == pat ]]`, extglob, `${x/pat/rep}` and globbing (DESIGN §4.8).

```rust
pub struct Flags { pub extglob: bool, pub nocasematch: bool, pub pathname: bool }
pub fn matches(pattern: &[u8], text: &[u8], flags: Flags) -> bool;
pub fn prefix_end(pattern: &[u8], text: &[u8], longest: bool, flags: Flags) -> Option<usize>;
pub fn suffix_start(pattern: &[u8], text: &[u8], longest: bool, flags: Flags) -> Option<usize>;
pub struct Pattern;   // compile once, match many; for glob loops
```

`matches` is anchored at both ends. `Some(0)` / `Some(len)` are real empty
matches; `None` is no match. Parsing is total — a malformed pattern is literal
text, so a `case` pattern can never fail to compile.

**Algorithm**: pattern → AST → Thompson NFA simulated as a set of states per
text position, so it never backtracks. O(n·m) without `!(…)`, O(n²·m) worst
case with it. bash's own matcher is exponential (`a*a*a*a*b` hangs it); this one
is not, and there is a watchdog test to keep it that way.

**Wiring rules the caller must get right** (each verified against bash 5.3.20,
and each is surprising):

- `[[ x == pat ]]` matches with **extglob semantics even when `shopt -u
  extglob`**; `case` and `${…}` honour the option. Pass `extglob: true`
  unconditionally for `[[ == ]]`.
- `shopt -s nocasematch` affects `case`, `[[ ]]` and `${x/pat/rep}` but **not**
  `${x#pat}`/`${x%pat}`. Pass `nocasematch: false` for the four strip
  operators. Pathname expansion uses `nocaseglob`, which is a different flag.
- `${x/pat/rep}` takes the **longest** match at the leftmost position:
  `prefix_end(…, longest: true)` on the tail, offset added back. The empty-match
  policy (`${x//""/X}` unchanged, `${x/#/X}` prepends) lives in the
  substitution loop, not here.
- Globbing compiles one `Pattern` per pattern and matches per path component
  with `pathname: true`; dotfile rules, `globstar`, `nullglob` and sorting
  belong to the caller.
- There is no quote-mask API: a caller holding a per-byte quoting mask must
  convert quoted bytes to backslash escapes (a quoted backslash becomes `\\`).

Documented deviations: POSIX classes are ASCII-only (locale independence, unlike
glibc); `[[.ch.]]`/`[[=a=]]` are ordinary bracket members; bash's
`*@()`-matches-nothing quirk is not reproduced (we implement the compositional
meaning); malformed extglob groups degrade to literal text; nesting is capped at
128.

### baash-runtime additions (built-in cluster)

- `AliasTable` lives in `ShellState`; a fork copies it and `exec` does not.
- `CommandKind { Alias, Keyword, Function, Builtin(Tier), External }`,
  `CommandInfo`, and `ResolveQuery { all, skip_functions, files_only,
  classify, default_path, fresh }` with `EXECUTION`/`CLASSIFY` presets.
- Five defaulted `ShellHost` seams: `resolve_command`, `run_builtin_direct`,
  `run_command_direct`, `unset_function`, `function_source`. Anything that
  answers "what would run" must go through these, so `type`, `command -v` and
  execution cannot disagree.
- `ShellError::reached_via_wrapper` plus `codes::NOT_FOUND` (511): a failing
  special built-in reached through `command`/`builtin` must not exit the shell
  (POSIX §2.8.1 note 1).
- `OPTIND` is initialised to 1 by the VM at startup; assigning to it is
  getopts' reset signal, which is why `Vars::optind_writes` exists.

### baash-sys: `Regex` (POSIX ERE)

`baash_sys::{Regex, RegexError, CaptureRanges}`, module `regex`, `#[cfg(unix)]`.
The engine is the platform's `regcomp`/`regexec`, not a Rust regex crate, because
the conformance target is bash and bash uses the platform engine: POSIX
leftmost-**longest** matching (`[[ "abcd" =~ (ab|abcd) ]]` yields `abcd`, where
a Perl-style engine yields `ab`), `{n,m}` parsing, and the same rejection
messages for invalid patterns.

```rust
Regex::new(pattern: &[u8]) -> Result<Regex, RegexError>;    // REG_EXTENDED
Regex::new_icase(pattern: &[u8]) -> Result<Regex, RegexError>; // + REG_ICASE
Regex::group_count(&self) -> usize;                          // true count, uncapped
Regex::captures(&self, text: &[u8]) -> Option<CaptureRanges>;
Regex::try_captures(&self, text: &[u8]) -> Result<Option<CaptureRanges>, RegexError>;
```

Contract points that the caller must honour:

- **Byte offsets.** Index 0 is the whole match, one slot per group, `None` for a
  group that did not participate. `BASH_REMATCH` is built from exactly
  `captures().len()` entries, `None` becomes the empty string, and a failed
  match **clears** the array (bash does; leaving the previous match is a bug).
- **`nocasematch` is `new_icase`**, not case folding of the subject: `REG_ICASE`
  also folds `[[:upper:]]`/`[[:lower:]]`, which lowercasing does not.
- **`MAX_GROUPS` is 64 slots** (whole match + 63 groups); longer patterns
  truncate rather than error, and `group_count()` still reports the truth.
- **NUL**: a pattern containing NUL is `RegexError::NulInPattern`; a subject
  containing NUL makes `captures` give up with `None` (bash cannot represent
  it either) while `try_captures` reports `NulInText`.
- **No `REG_NEWLINE`** (bash passes none): `^`/`$` anchor at subject ends, `.`
  matches newline, `[^x]` matches newline.
- **Locale**: classes and collation come from the process locale, and Rust never
  calls `setlocale`, so matching runs in the C locale whatever `LANG` says. That
  is deterministic and matches bash under `LC_ALL=C`; bash under a UTF-8 locale
  is Unicode-aware and we are not, because `setlocale` is process-global state
  and mutating it would be the same class of bug as a global signal disposition.
- **`Regex` is neither `Send` nor `Sync` nor `Clone`**: one thread, one pattern.

### Completion built-ins and the `--rcfile` startup file

`compgen`, `complete` and `compopt` are wired: `impl CompletionSource for
Interp` answers the 24 `-A` actions from shell state, `ShellState::completions`
holds the `CompletionTable` (definition-ordered, like `AliasTable`), and
`ShellState::completion` is the run in progress, which is what makes `compopt`
valid inside a completion and an error outside it. `baash-runtime` gained the
workspace leaf `baash-pattern` for `-G`/`-X` matching.

At the prompt, the same registry also drives Tab: `crates/baash/src/complete.rs`
clones `ShellState::completions` into the engine before each prompt (see
`baash-complete`). The clone is the seam, so `complete -W` takes effect on the
line typed after the one that defined it — the rule must be in the prompt
snapshot, and the snapshot is taken before `read_line`. `-F`/`-C`/`-G` rules
fall back to the default completion because the completer cannot run shell code
(`docs/state.md` §4).

Sources that would be lies are stubs returning nothing rather than plausible
lists: `binding` (no line editor), `service`, `job`/`running`/`stopped` (no
job table), `disabled` built-ins (`enable -n` absent).

Two status codes are deliberately not bash's, both annotated as bugs in the
suite: `complete -p NAME` for an unknown NAME is 0 (bash 1), and `complete NAME`
with no action is 2 (bash 0).

`crates/baash` reads startup files only when the shell is interactive, and it
reads two layers. First **the shell's defaults**: `$XDG_CONFIG_DIRS/baash/config.sh`
when a distributor installed one, otherwise `config/config.sh` compiled into the
binary (coloured, hyperlinked `ls`, `ll`/`la`, coloured `grep`). Then **the
user's own**: `$XDG_CONFIG_HOME/baash/config.sh`
(`~/.config/baash/config.sh`), or `~/.bashrc` when there is none. Later layers
override earlier ones, which is what lets a user retune one alias without
copying the file, and what keeps the defaults improving with the shell.
`--rcfile FILE` replaces both layers and `--norc` disables both, and a missing
file is ignored at every step — which is what makes `--rcfile /dev/null` the
portable way to ask for no startup file at all. The bases come from
`baash_sys::dirs`, which applies the XDG rules: a relative `XDG_*` path is
ignored rather than resolved against the working directory, and the first
`$XDG_CONFIG_DIRS` entry with a `baash/config.sh` wins.

### Arithmetic: who expands the text

`baash-vm/src/arith.rs` performs `$name`, `${name}` and `${name[expr]}` splicing
and double-quote removal itself, because `tree/words.rs` hands it the raw
arithmetic text. That makes it the **only** layer that may interpret escapes
inside an arithmetic expression: if the word layer ever expands first, the two
must not both do it, or `\$x`, single quotes, `${x:-w}`, `$(…)` and backticks
will double-expand.

The evaluator is total: every input either produces an `i64` (wrapping, like
bash) or an error with a message, never a panic and never a silently wrong
value. Short-circuiting is enforced by a `dead` flag: the untaken operand is
still *parsed* (so syntax errors surface) but reads, writes and division are
suppressed, which is what makes `0 && 1/0` and `1 ? 5 : x++` behave.

One measurement worth keeping: `**` binds *below* unary sign, so `-2**2` is
`4`, not `-4` — bash 5.3.20 and the suite agree, and the intuition is wrong.

### `[[ … =~ … ]]` and the right-hand side

Measured against bash 5.3.20, because the rule is not "quote removal" and not
"pass the bytes through":

- `a\.b` matches `a.b` and does **not** match `axb` — the backslash survives
  into the regex.
- a literal `\babc\b` does **not** match `x abc y` — the shell consumes the
  backslash, so the engine receives `b`.
- `r='\babc\b'; [[ $t =~ $r ]]` **does** match — a variable's backslashes
  reach the engine, where glibc treats `\b` as a word boundary.
- `"a.b"` matches only a literal dot, because every byte is quoted.

The one rule that explains all four: expand the word normally (which yields
bytes plus a per-byte quoting mask — [`Field`]), then **re-escape the
metacharacters that are quoted**. Quoted `.` becomes `\.`, quoted `b` stays
`b`. Unquoted bytes pass through untouched. This is why the matcher must take a
`Field`, not `&[u8]`.

`nocasematch` compiles with `new_icase`, not by folding the subject. An invalid
pattern reports bash's message and makes the `[[` command fail with status 2;
`BASH_REMATCH` is cleared on failure and otherwise holds one entry per capture
with non-participating groups as the empty string.

### Lexing and parse-time options

`lex_full` uses the default profile (`extglob` off); `lex_full_with(src,
extglob)` takes the parse-affecting option, and `parse_with` passes
`ParseOpts::extglob` through. This matters for exactly one decision: a
word-initial `!(…)` at the start of a statement is pipeline negation when the
option was off, and an extglob group when it was on. bash settles this at parse
time, so a chunk parsed with the option on must not invent the negation
reading. Every other extglob operator (`?(`, `*(`, `+(`, `@(`) is always a
group, because none of them can begin a command.

The lexer is a **superset** of bash's: with the option off, `echo @(a)` is the
word `@(a)` where bash reports a syntax error. A group that never closes is not
an error either — the `(` stays an ordinary word byte, which is how
`baash-pattern` degrades the same text.

### baash-spec

`--shell PATH` and every `--compare` reference are made absolute, but never
canonicalized: `/bin/sh` is a symlink to a multi-call binary on Alpine, and
`busybox`/`toybox` pick their applet from `argv[0]`, so resolving the link runs
the usage banner instead of a shell. Being absolute matters just as much: each
case runs in its own temporary directory, so a relative `--shell
target/debug/baash` would be looked up inside that directory and fail every
case. `std::path::absolute` is exactly that pair of properties.

`--locale NAME` sets `LC_ALL` for the shell under test **and** every reference,
so the comparison is like-for-like. `C` is the default because the suite's
primary expectations are byte-calibrated: measured, bash falls from 2574 to
2435 under `C.UTF-8`, while counting characters gains us 15. UTF-8 is therefore
a diagnostic mode, not the baseline.

**The helpers name their interpreter by absolute path.** `spec/oils/bin/*.py`
start with `#!/usr/bin/env python2`; when the harness substitutes its Python 3
equivalents it writes the shebang itself, from the `python3` it found on `PATH`.
`#!/usr/bin/env` assumes `/usr/bin/env` exists, which a minimal container and a
nix build sandbox both lack — the shims then exit 127 and every case that uses
`argv.py` fails for a reason that has nothing to do with the shell (357 cases,
measured). The same substitution runs whenever `python3` exists, even if
`python2` does too, so the vendored helpers' `env` shebang is never used.

### The binary's REPL loop (`crates/baash/src/main.rs`)

Before each `editor.read_line()` the loop, in order: swaps the history ring into
`ShellState`; runs
`PROMPT_COMMAND` (a scalar command or an indexed array, each element run in
turn) with its stdout captured through a seekable temporary file and re-emitted
above the prompt, appending a newline when the output did not end in one;
restores `$?`; syncs the `HIST*` knobs; renders `PS1` and `PS2` with
`Interp::render_prompt`; refreshes the completion engine from the shell
(`complete::refresh`: `PATH`, `PWD`, `HOME`, variable/function/alias names, the
history ring and the `complete` registry); swaps the ring back; and hands the
pair to `editor.set_prompt`. A `PROMPT_COMMAND` that calls `exit` leaves the shell. The
capture is why `PROMPT_COMMAND='echo -n x'` cannot corrupt the editor's column
model; a write straight to `/dev/tty` bypasses it. With `set -e` and a failing
`PROMPT_COMMAND`, bash exits and this shell does not (`docs/state.md` §4).

After `editor.read_line`, `read_command` strips the submit newline and, when
`interactive && histexpand && history`, expands the line against the ring
*without the current entry*. A changed line is echoed to stderr, the typed
entry is removed and the expanded one recorded (`:p` included; a failed event
is not recorded and leaves `$?` alone). `shopt -s histverify` sets the expanded
line with `Editor::set_initial_line` and reads again instead of running it; a piped
`-i` has no editing buffer, so `histverify` has no effect there (the line runs).
The editor's ring is swapped into the shell only for `run_named`, so the
`history`/`fc` built-ins see the same entries. A non-interactive pipe is still
one chunk; an interactive pipe now uses the line-at-a-time reader (bash
expands each line of `-i`, and the second line must see the first), while
`-i -c` stays whole-string and never expands.

### baash-vm additions

`Interp::each_candidate` is the single resolution walk (alias → keyword →
special → function → intrinsic → regular/extension → `PATH`); `resolve()` is
gone. The `PATH`/hash table lives on `Interp`. `hash` and `mapfile`/`readarray`
are executor bodies with registry stubs, like `read` and `cd`, because their
state is the hash table and the descriptor table. `read::read_record` is the
record reader `mapfile` shares with `read`.

`Interp::function_names()` lists the function table for the completion bridge
(the other completion-relevant state is already reachable through
`ShellHost::state`). It returns names only, sorted and deduplicated.

`Interp::repl_prompt_context()` and `Interp::render_prompt(template)` are the
prompt seam the binary uses before every line. The context is
`Shell::prompt_context()` with `\!` set to the ring's *next* number (the line
has not been recorded yet) and `\l`/width read from the terminal on stdin;
`render_prompt` runs the shared expander with the shell as its second-pass host
and puts `$?` back afterwards (a prompt `$(false)` must not change the status
the next command sees). `Interp::restore_status(status)` is the same
save/restore for `PROMPT_COMMAND`, which runs between commands.

`Interp::pattern_flags()` answers from `shopt`: `extglob` and `nocasematch` are
the pattern dialect, while `utf8` stays off (byte units, C locale — the policy
`${#x}` counts with). `shopt` itself is a state-only runtime built-in over the
option table, so an unknown name is an error rather than a silent no-op.


---

## Diagnostic codes

Each crate owns a hundred-range, so a code identifies the layer that produced
it and never has to be looked up to be disambiguated. Codes are stable; message
text is not — anything matching on stderr should match the code.

| range | owner | examples |
|---|---|---|
| 1xx | `baash-syntax` | 1 unterminated, 2 need more input, 3 unexpected token |
| 2xx | `baash-vm` (reserved) | the POSIX §2.8.1 consequence layer |
| 3xx | `baash-lang` | 301 unsupported operator, 302 parameter null, 303 unset |
| 4xx | reserved | redirection and I/O |
| 5xx | `baash-runtime` | 508 builtin not implemented yet, other state errors |

`baash-diag` produces no codes of its own; it renders them.

---

## Pending

Wave 2 (execution) and wave 3 (the editor) have not landed yet.
