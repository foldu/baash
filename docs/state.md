# State of the implementation

**Purpose.** This is the resume document: what exists, what is proven, what is
suspected, and what to do next. It was written at commit `c2f791a`
(2026-10-28) with a clean tree and a green gate. Update it when the picture
changes, not every commit.

**Recent priority change.** Interactive use now outranks script conformance.
The conformance backlog is still the largest body of measured truth about where
the shell misbehaves, but it is no longer the work queue — see
[§8](#8-next-steps-in-order) for the interactive-first order.

---

## 1. Getting back to a working loop

```sh
export CARGO_HOME=/tmp/bsh-cargo-home CARGO_TARGET_DIR=/tmp/bsh-target
cargo build -p bsh -p bsh-spec

# the gate that must pass before a merge
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- --deny warnings
cargo test --workspace --all-features
cargo test --workspace --all-features -- --list | grep -c ': test$'   # 1096

# conformance
target/debug/bsh-spec --shell target/debug/bsh --ratchet   # must be >= baseline
target/debug/bsh-spec --shell target/debug/bsh --compare bash --json /tmp/rep.json
```

`bsh-spec --json` writes per-case results; the per-file failure histogram that
drives triage is built from it (see `docs/roadmap.md` for the shape of the
query). **Regenerate triage every wave** — the map changes on every merge, and
a stale brief has already cost two wrong scopes (arrays: 54 vs real; namerefs:
4/32 vs 4/33).

**Two environment traps.**

* The checkout path contains `ß`. Linking fails if crate or worktree paths are
  non-ASCII, so worktrees live at `/tmp/bsh-wt-*`.
* Several `bsh-vm` test binaries lose their libtest summary when stdout is
  redirected, so `cargo test` alone under-reports. Always take the count from
  `--list | grep -c ': test$'`.

## 2. Where the numbers stand

| measure | value |
|---|---|
| comparable cases | **1986 / 2787 = 71.3 %** |
| strict (primary expectation only) | 1931 / 2787 = 69.3 % |
| failing | 801 |
| timeouts | 0 |
| of the whole suite (3971 cases) | 50.0 % |
| as a fraction of bash's score | 1986 / 2574 = 77.2 % |
| share of the bash gap closed since 1013 | 973 / 1561 = 62.3 % |
| our test count | 1096 |

Measured with `bash 5.3.20` as the comparison shell, harness forced to
`LC_ALL=C` (bash scores *worse* under `C.UTF-8`: 2574 → 2435, so the C locale
is the fair denominator). We pass 46 cases bash fails; bash passes 635 we fail.

**1184 skipped** cases: 745 from the ysh-only suite files, 417 marked
`our_shell: ysh`, 22 needing a `python2` helper we do not provide. 100 % is not
a meaningful target: bash itself scores 92.4 %, and 55 of our passes come from
`## OK <shell>` alternatives rather than the primary expectation — which is
exactly the 71.1 % vs 69.1 % split, and why both numbers are ratcheted.

Baseline lives in `crates/bsh-spec/baseline.txt` (`total = 1986`,
`strict = 1931`). `--ratchet` exits 1 below either, 2 if it cannot measure.

## 3. Layers

61 731 lines of `src` across 14 crates.

| crate | lines | responsibility |
|---|---|---|
| `bsh-sys` | 3547 | the only `unsafe`. fork/exec (`posix_spawn` by default), descriptors, pipes/pty, termios, signals, `wait_any`, process groups and `tcsetpgrp`, libc regex, global `fork_gate()` |
| `bsh-syntax` | 6607 | lexer + recursive-descent parser producing `Word`/`Command`; the superset grammar |
| `bsh-lang` | 9862 | shell *text* semantics: word expansion, patterns (`bsh-pattern`), parameter operators, prompt escapes, arithmetic; no I/O |
| `bsh-runtime` | 11736 | shell *state*: variables/arrays/namerefs, options, builtin registry and state-only bodies, the history ring and `HIST*` policies, traps bookkeeping, completion registry, `declare -p` rendering |
| `bsh-vm` | 13224 | the evaluator (tree-walking) and everything that needs both state and syscalls: command execution, redirection, jobs, traps |
| `bsh-pattern` | 3947 | Thompson-NFA glob: `O(n·m)`, never exponential; `extglob`/`nocasematch`/`utf8` flags |
| `bsh-diag` | 1946 | diagnostic model and miette rendering; error-code space |
| `bsh-complete` | 3137 | completion engine: providers, contexts, nucleo ranking, menu layout |
| `bsh-repl` | 3041 | line editor, key decoding, history navigation, prompt adapter, renderer, terminal abstraction |
| `bsh-highlight` | 386 | syntax highlighting (token → style), shared with the editor |
| `bsh-spec` | 3242 | the Oils-suite runner, comparator, ratchet |
| `bsh` | 920 | the binary: argument parsing, startup files, REPL loop, history ownership swap |
| `bsh-compile` | 120 | bytecode types only (`Chunk`, `Op`, `Vm{chunk,pc}`) — wave 5, unwired |
| `bsh-tools` | 16 | dev helpers |

The evaluator is the runtime; `bsh-compile` is a placeholder. This is
deliberate (see `DESIGN.md` §5 and the note in `bsh-vm/src/tree/mod.rs`): ship
an evaluator, keep it as the oracle, and only then add the bytecode VM behind
the same semantics tests.

## 4. The interactive shell today

`bsh -i` (or a terminal on stdin) runs `bsh::run_repl`, which owns the loop and
delegates everything else: line editing to `bsh-repl`, execution to the
`Interp`, completion to `bsh-complete`.

**Works** (verified under a real PTY, `script -qec`):

* prompt, per-keystroke editing, backspace, arrows, Home/End/Delete;
* **the prompt comes from the shell**: `PS1`/`PS2` are read from the shell's
  variable table before every line, so an rc file, an exported `PS1` and a
  `PS1=` typed at the prompt all take effect at the next prompt; the escapes
  (`\u \h \w \W \$ \! \# \j \l \d \t …`) read the shell's logical
  `PWD`, its job table and its history ring; `\!` is the number the line will
  get and `\#` bash's command number;
* **`promptvars`** (`shopt`, on by default): the decoded prompt is expanded a
  second time as if double-quoted, so `PS1='$PWD $ '` and
  `PS1='$(date +%H:%M) $ '` work and `$(…)` runs at prompt time; with the
  shopt unset nothing is expanded;
* **`PROMPT_COMMAND`** (string or indexed array) runs before each primary
  prompt, and its stdout appears above the prompt — including output that does
  not end in a newline, which is terminated so the editor's column arithmetic
  holds;
* **`Ctrl-R` reverse incremental search** with the `(reverse-i-search)` prompt,
  repeat-search, `Enter`/arrows to accept, `Esc`/`Ctrl-G` to abort;
* **`PS2` continuation** — `for x in a b; do` ⏎ shows `> `, and the assembled
  command runs correctly after `done`;
* entering commands, running programs, `$((…))`, `exit`;
* syntax highlighting (raw mode, `bsh-highlight`, colour decided by fd 0);
* completion on `Tab` with a navigable menu (Up/Down/BackTab/Ctrl-P);
* **history as a shell feature**: one ring owned by `bsh-runtime` and shared by
  the editor and the `history`/`fc` built-ins; `history [n]`, `-c`, `-d`,
  `-a`, `-n`, `-r`, `-w`, `-p`, `-s`; `fc -l` (`-n`/`-r`/ranges) and `fc -s`;
  `HISTFILE`, `HISTSIZE`, `HISTFILESIZE`, `HISTCONTROL` (`ignorespace`,
  `ignoredups`, `ignoreboth`, `erasedups`), `HISTIGNORE` (including `&`), and
  the `histappend` shopt; default `~/.bsh_history`; the file is loaded at
  startup and saved on exit; `Up`/`Down` navigation and ghost text;
* `Ctrl-C` on a line → discards it, `$?` = 130; `Ctrl-D` on an empty line →
  exit 0;
* **job control**: every foreground command (and pipeline) gets its own
  process group, `tcsetpgrp` handover, `WUNTRACED` waits and terminal restore.
  `Ctrl-Z` stops the foreground job, returns the prompt and `$?` 148; `jobs`
  (with `-l`, `-p`, `-r`, `-s`), `fg`, `bg`, `disown` match bash's output;
  completion/stop notices print before the next prompt; `%N`, `%%`, `%+`, `%-`,
  `%prefix` and `%?substring` resolve; `wait` on a stopped job returns 148
  instead of hanging. Verified by `crates/bsh/tests/interactive_jobs.rs`, which
  runs `bsh -i` on a real pty;
* `$!`, `wait`, `kill` for children *this* shell started;
* `--rcfile FILE` / `--norc`; the default startup file is `~/.bshrc` when it
  exists, otherwise `~/.bashrc` (see §9).

**Missing, in order of how badly it hurts an interactive user:**

1. **Directory stack.** `pushd`, `popd`, `dirs` missing.
2. **Small but common builtins.** `umask`, `let`, `times`, `ulimit`, `enable`.
   `printf`, `pwd`, `test`, `[` are served from `PATH` — exec-able by design,
   but that means no `printf -v` and no `%q` portability, and `pwd` ignores
   `-P`/`-L` semantics.
3. **Editor depth.** No undo, no `vi` mode (`set -o vi` sets a shopt today but
   the editor ignores it), no word-level kill/yank verification, no `Ctrl-L`. Bracketed paste appears in the key decoder (`Paste` in
   `key.rs`) but is unverified.
4. **Search depth.** `Ctrl-R` does not highlight the matched bytes (the cursor
   sits on their first byte), and there is no `Ctrl-S` forward search.

**Known prompt limitations** (measured or deliberately deferred):

* `\s` is `bsh`, so the conformance case that greps for `bash|osh` still
  fails; `\v`/`\V` now report the crate version (`0.0` / `0.0.0`).
* With `set -e`, a failing `PROMPT_COMMAND` exits bash and does not exit this
  shell; the status is ignored.
* `PROMPT_COMMAND`'s stdout is captured and re-emitted so the prompt always
  starts on a fresh line. bash leaves the prompt on the partial line instead
  (`partialP> `), and a command that writes to `/dev/tty` bypasses both the
  capture and bash's column tracking. stderr is not captured, so output
  interleaving with a captured stdout command can differ.
* `${x@P}` reports `\!` as the number of the line already recorded, so under
  `set +o history` or in a script (which records nothing) it can print `0`
  where bash prints the next number. The interactive prompt is unaffected.
* `$?` is restored after the prompt is rendered, so a prompt substitution does
  not change the status the next command sees; a *later* `$(…)` inside the
  same `PS1` can see the status the earlier one left.
* `\j` in a non-interactive `${PS1@P}` still counts no jobs for a `sleep &`
  under `set -m` — that is the job table's business, not the prompt's. The
  interactive count is real (measured 0 → 1 → 0).

**Known history limitations** (measured or deliberately deferred): `fc` with
neither `-l` nor `-s` reports "editing history is not implemented yet" instead
of opening `$FCEDIT`/`$EDITOR`; history expansion (`!`, `!!`, `!$`) does not
exist, so `history -p` prints its arguments unchanged (which is what bash
prints when there is nothing to expand); a multi-line command is kept in memory
but skipped when the line-oriented file is written (bash's `cmdhist` joins it
with `;`); `HISTTIMEFORMAT` is not implemented; the conformance suite's
interactive history cases mostly still fail on the bash-only `^D` marker, not
on history behaviour.

**Known job-control limitations** (measured, not defects to rediscover): a
foreground stop inside a function or brace group resumes the *child*, not the
containing compound command; `$!` for a background pipeline is the one subshell
this shell forks, not bash's last-stage pid; `jobs -l` prints one line for a
background pipeline (the shell runs it as one child); the command text is the
source text, so bash's normalisation of `>/dev/null` to `> /dev/null` and of
runs of spaces is not reproduced; `disown -h` marks the job but the shell still
does not send `SIGHUP` on exit.

**Known-untested:** terminal resize (there is a `TIOCGWINSZ` read; no `SIGWINCH`
handling seen); whether the line editor is restored correctly when a child
leaves the tty in raw/cbreak mode; and an interactive shell started in the
background (`bsh -i &`), where the shell is not the terminal's foreground group
— bash stops itself with a job notice, and `enable_job_control` merely tolerates
the failed `tcsetpgrp` (`let _ =`). `Ctrl-C`/`Ctrl-Z` on a foreground command are
now covered by the pty test.

Harnesses: `crates/bsh-sys/tests/pty.rs` (real pty, termios, window size,
foreground pgroup, `become_session_leader`), `crates/bsh/tests/interactive_jobs.rs`
(interactive job control end to end), `crates/bsh/tests/interactive_history.rs`
(history persistence), `crates/bsh/tests/interactive_prompt.rs` (the rendered
prompt, `PROMPT_COMMAND`, prompt escapes, `PS1` from an rc file and from a
command, `Ctrl-R` end to end) and `crates/bsh-repl/tests/editor.rs` (scripted
keystrokes against a fake terminal, including the search states).

## 5. Builtin inventory

56 names registered in `bsh-runtime`; 22 have a registry body, and several
more are the executor's. Eight are missing outright: `times`, `ulimit`,
`umask`, `dirs`, `enable`, `let`, `popd`, `pushd`. Four more (`[`, `printf`,
`pwd`, `test`) are intentionally exec-able.

* special (15): all but `times` are implemented.
* intrinsic (16): only `ulimit` and `umask` are missing; `fc` landed here.
* regular (7): `false` and `true` in-process; `echo` is the VM's body; `[`,
  `printf`, `pwd`, `test` fall through to `PATH` by design.
* extension (18): missing `dirs`, `enable`, `let`, `popd`, `pushd`; `history`
  landed here.

Note the ownership rule that keeps this honest: the VM's table is consulted
only when the registry entry has no body, so filling in a runtime body silently
retires the VM one instead of shadowing it.

## 6. Conformance backlog

Not the priority any more, but it is the measured map of misbehaviour. Last
triage (before the nameref wave; regenerate before use): `var-ref` 25,
`errexit-osh` 23, `vars-special` 22, `builtin-umask` 21, `builtin-printf` 19,
`sh-options` 19, `builtin-trap-bash` 19, `builtin-process` 18, `trap-err` 16,
`xtrace` 16, `globignore` 16, `builtin-dirs` 16, `var-op-slice` 15.

Known parser gaps: a subscript containing `$(…)` or `$((…))`; rejection with
status 2 of malformed subscripts. Known diagnostic gap: sparse-array messages
need the Oils caret format.

**The suite is blind to interactive behaviour.** Job control is covered only by
`spec/oils/stateful/job_control.py`, a Python pty harness that `bsh-spec` does
not run, and prompt/editing behaviour is not covered at all. So the conformance
numbers will not move when interactive work lands: the evidence for that work
has to be PTY tests, and their absence is not a pass.

## 7. Decisions already made

Do not re-litigate these without a reason; several were expensive.

* **Bytes, never `String`**, for anything the shell can hold or print.
* **`bsh-sys` is the only crate with `unsafe`**; everything else is
  `#![forbid(unsafe_code)]`. `fork_gate()` is global.
* **One superset grammar + post-parse gating.** No profile-dependent parsing;
  portability is a linter over one AST.
* **Ambiguity budget, three tiers:** lexical ties go to bash; the parser uses
  bounded lookahead; runtime ties are settled by bash behaviour, and by
  measuring bash 5.3.20 rather than reasoning about it. That rule found
  `pipefail` and the `[[ =~ ]]` right-hand rule.
* **Regex through libc `regcomp`/`regexec`** (`bsh-sys::regex`), not the
  `regex` crate: POSIX is leftmost-**longest**. Measured:
  `[[ abcd =~ (ab|abcd) ]]` must yield `abcd`.
* **`[[ =~ ]]` RHS**: expand the word, then re-escape only the metacharacters
  that were *quoted*. That single rule explains every observed case.
* **Locale is opt-in from the binary** (`shell.detect_locale()`), never from
  `Interp::new`, so unit tests stay deterministic; the spec harness forces
  `LC_ALL=C`. `utf8: false` *is* the C locale in `bsh-pattern`.
* **UTF-8 is a pattern/completion flag, not a string type.**
* **Special-builtin consequences follow POSIX §2.8.1** — fatal
  non-interactively. Deliberate exceptions: syntax errors inside `eval`/`source`
  are not fatal.
* **`failglob` fails the command and continues** (`ErrorKind::GlobNoMatch`,
  code 307); `${x:?}` still exits a non-interactive shell.
* **`pipefail`** = rightmost non-zero status, sampled at pipeline start.
* **`declare -p` renders bash's real layout** (`declare -a a=([0]="b" [1]="c")`),
  not a compact form. Chasing the compact form cost 7 regressions.
* **Namerefs**: the cell holds reference text, chains are capped at 16 hops,
  attributes live on the cell, self-reference is rejected, cycles are fatal on
  use.
* **Arrays slice by subscript boundary** (negative offsets from
  `max_index + 1`); positional parameters are one-based with offset 0 = `$0`.
* **Completion ranking is nucleo** (as specified); menu rendering is ours.
* **`bsh-vm` keeps the evaluator** even after the bytecode VM lands; it is the
  oracle for the VM's semantics tests.

## 8. Next steps, in order

Interactive first. Each item is sized to be one wave with a brief and a
measured finish line.

1. ~~**Job control.**~~ Landed (see §4): process groups, `tcsetpgrp`, `WUNTRACED`
   waits, `jobs`/`fg`/`bg`/`disown`, completion notices, `monitor` on by
   default for a terminal shell, and a pty integration test. Remaining edge
   cases are listed under §4's known limitations (`$!` for pipelines, stops
   inside compound commands, `SIGHUP` on exit).
2. ~~**History.**~~ Landed: one ring in `bsh-runtime`, `history`/`fc`,
   `HISTFILE`/`HISTSIZE`/`HISTFILESIZE`/`HISTCONTROL`/`HISTIGNORE`,
   `histappend`, default `~/.bsh_history`, save/load across sessions, and the
   `.bshrc`/`.bashrc` startup-file rule. **`Ctrl-R` incremental search** landed
   with the prompt wave (item 3); the ring, the editor and the built-ins share
   one ring, and the search is a prompt swap in the existing renderer.
3. ~~**Prompt parity.**~~ Landed: the prompt is rendered from the shell's
   variable table before every line, `promptvars` (with `$(…)` and `$((…))`),
   `PROMPT_COMMAND` (string and array), bash's default `PS1`/`PS2`, and
   `\!`/`\#`/`\j` from real state. A stale inherited `PWD` is now replaced by
   the real directory, which is what makes `\w` truthful. Remaining gaps are
   listed under §4's known prompt limitations (`\s`, `set -e` with a failing
   `PROMPT_COMMAND`, `${x@P}`'s history number without a recorded line).
4. **Common builtins.** `pushd`/`popd`/`dirs`, `umask`, `let`, `times`,
   `ulimit`, `enable`, and `printf`/`pwd` bodies so `printf -v` and `pwd -P`
   exist.
5. **Editor depth.** `vi` mode, undo, `Ctrl-L`, word motions, bracketed paste
   (verify first), search-match highlighting and `Ctrl-S`.
6. **Completion depth.** `$VAR`/`${…}` completion, command names from `PATH`,
   quoting of inserted filenames, and wiring the `complete -F` registry to the
   engine so programmable completion is real.
7. **Back to conformance**, resuming with the clusters in §6 — interactive
   semantics first (`sh-options`, `errexit-osh`), then builtins, then patterns.

## 9. Open questions

* **Startup file — decided.** `~/.bshrc` is sourced when it exists, otherwise
  `~/.bashrc`; `--rcfile` and `--norc` still win. A bashrc full of
  `bind`/`complete -D`/`PROMPT_COMMAND` half-applies, so a user who wants a
  bsh-specific file gets one without renaming the bash file.
* **Default history file — deliberate deviation.** `HISTFILE` defaults to
  `~/.bsh_history`, not bash's `~/.bash_history`. Sharing bash's file means two
  shells rewriting each other's history (and bash's `histappend` default is
  off), so the history would be lost or interleaved. The default is applied
  only to the terminal REPL, after the startup file, and only when neither the
  environment nor the startup file set `HISTFILE`; an explicitly empty
  `HISTFILE` disables persistence. A non-terminal `-i` (piped stdin, `-c`)
  does not invent a file: it would otherwise make an interactive conformance
  run read and write the user's home directory. Verified: the ratchet reports
  the same 1986/1931 with a `~/.bsh_history` full of unrelated entries, and
  the file is left untouched. `/bin/sh` compatibility is unaffected because
  non-interactive shells neither load nor save.
* **The prompt is rendered by the shell, not the editor — decided.** Only the
  shell has the variable table (`PS1` must come from there; the environment is
  a startup snapshot) and only the shell can run `$(…)` for `promptvars`. So
  `bsh_repl::Prompt` holds *rendered* `ExpandedPrompt`s and the binary calls
  `Editor::set_prompt` before every line. The editor stayed free of `PS1`
  knowledge, which is why an rc-file prompt, a prompt changed at run time and
  an exported prompt all take effect identically. `bsh-lang` keeps the single
  expander shared with `${x@P}`; the second pass now runs `$(…)`, `$((…))`
  and backticks for both callers (bash does this for `@P` too; measured)
  instead of copying them through.
* **`\[`/`\]` spans are part of the API, not the renderer's guess.** The
  second pass returns `ExpandedPrompt` and recomputes the non-printing ranges
  over its output, so a coloured prompt containing a variable keeps its width
  bookkeeping. The renderer additionally ignores ANSI CSI sequences as a
  fallback for a user who forgets the markers; that fallback is not a
  substitute for the spans (a control byte inside `\[…\]` is invisible to
  it).
* **An interactive shell starts with bash's prompt variables.** `PS1` is
  `\s-\v\$ ` and `PS2` is `> `, as unexported shell variables, set before
  the environment and the startup file can replace them. That is what bash
  does (`declare -p PS1` in `bash -i`), it makes `$PS1`/`${PS1@P}` agree, and
  it is what the `default PS1` conformance case measures. `unset PS1` is an
  empty prompt, as in bash. `\s` stays `bsh`, so the case that greps for
  `bash|osh` still fails.
* **`\!` and `\#` are two different counters.** `\!` is the history ring's
  next number for the prompt (the line is not recorded yet) and its last
  number for `${x@P}` (the line is recorded); `\#` is a `ShellState` counter
  the *interactive loop* advances per line, because the evaluator sees whole
  chunks and cannot tell a REPL line from a script. Both prompt and `@P`
  report `counter + 1`, which is bash's measured rule.
* **`PROMPT_COMMAND` runs in the shell, with its stdout captured.** Running it
  in a subshell to capture output would lose `PS1=…` assignments, so the shell
  runs it in-process with fd 1 pointed at a seekable temporary file and
  re-emits the bytes, adding a newline if the output did not end with one.
  That guarantees the editor's column model (bash leaves the prompt on the
  partial line instead) and cannot deadlock the way a pipe would. `$?` is
  saved and restored. Two deviations are recorded under §4's prompt
  limitations: `set -e` on a failing `PROMPT_COMMAND`, and stderr/stdout
  interleaving.
* **A stale inherited `PWD` is replaced at startup.** `PWD` is checked by
  device and inode against the process's directory and replaced when it does
  not name it (bash does the same silently); a valid logical path through a
  symlink is kept. Without this, `\w`, `$PWD` and `cd ..` could all be wrong
  in an environment built by `su` or a wrapper.
* **History expansion is still absent**, and `fc`'s editor form is deferred.
  Both are recorded under the known history limitations in §4.
* **`set -o vi`** exists in the option table but has no editor behind it.
* **Bytecode VM.** Types exist; `DESIGN.md` §5 disagrees with the code on
  `Vec<u32>` vs `Vec<Op>` and on static vs push/pop handler tables. The
  recommendation is `Vec<Op>` + a static handler table, but that decision is
  not recorded in `DESIGN.md` yet.
* **GitLab remote** does not exist; CI is written but has never run.

## 10. Conventions

* Merge protocol: rebase the wave branch in its worktree, `git merge --ff-only`,
  run the whole gate, re-record the baseline, remove the worktree and branch.
* Agents get a crate-level brief, no new dependencies (workspace-level leaves
  are allowed, e.g. `bsh-pattern` from `bsh-runtime`), no edits outside the
  crates they own, `#![forbid(unsafe_code)]`, and
  `Co-authored-by: DeepSeek-V4.1-Flash <noreply@deepseek.com>` in every commit.
* `docs/contracts.md` is the landed-API record per crate; update it in the same
  commit as the code that changes an interface.
* Every merged wave must leave: gate green, test count recorded, baseline
  re-recorded, and the triage regenerated if conformance moved.
