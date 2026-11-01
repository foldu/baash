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
cargo test --workspace --all-features -- --list | grep -c ': test$'   # 1161

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
| comparable cases | **2331 / 2787 = 83.6 %** |
| strict (primary expectation only) | 2278 / 2787 = 81.7 % |
| failing | 456 |
| timeouts | 0 |
| of the whole suite (3971 cases) | 56.3 % |
| as a fraction of bash's score | 2331 / 2577 = 90.5 % |
| share of the bash gap closed since 1013 | 1318 / 1564 = 84.3 % |
| our test count | 1274 |

Measured with `bash 5.3.20` as the comparison shell, harness forced to
`LC_ALL=C` (bash scores *worse* under `C.UTF-8`: 2574 → 2435, so the C locale
is the fair denominator). We pass 71 cases bash fails; bash passes 317 we fail.

**1184 skipped** cases: 745 from the ysh-only suite files, 417 marked
`our_shell: ysh`, 22 needing a `python2` helper we do not provide. 100 % is not
a meaningful target: bash itself scores 92.4 %, and 53 of our passes come from
`## OK <shell>` alternatives rather than the primary expectation — which is
exactly the 81.7 % vs 83.6 % split, and why both numbers are ratcheted.

Baseline lives in `crates/bsh-spec/baseline.txt` (`total = 2331`,
`strict = 2278`). `--ratchet` exits 1 below either, 2 if it cannot measure.

**The W1 drop is expected.** Removing job control failed seven Oils cases that
exercise `%` specifications, `jobs -p` and `kill %job` (one trap case improved
as a side effect); the pivot accepts a lower number as the price of deleting
the machinery. Future waves re-baseline the same way.

**The history drop is expected too.** Replacing the history *file* with a
database cost seven cases and gained one (1968/1914 → 1962/1908): `history
-w`/`-r`/`-n`, `HISTSIZE` shrinking, `fc -l` on an empty ring, and the two
`HISTFILE=my-history` interactive cases. A case that expected `fc` to reject
too many arguments now passes because `fc` does not exist (status 127). The
count was diffed case by case against the previous commit; nothing else moved.

## 3. Layers

64 582 lines of `src` across 15 crates.

| crate | lines | responsibility |
|---|---|---|
| `bsh-sys` | 3898 | the only `unsafe`. fork/exec (`posix_spawn` by default), descriptors, pipes/pty, signals, libc regex, global `fork_gate()`; descriptors, termios and `wait` go through `rustix`, and the 77 remaining blocks are leaf syscalls or `posix_spawn` plumbing |
| `bsh-syntax` | 6607 | lexer + recursive-descent parser producing `Word`/`Command`; the superset grammar |
| `bsh-lang` | 11842 | shell *text* semantics: word expansion, patterns (`bsh-pattern`), parameter operators, prompt escapes, arithmetic; no I/O |
| `bsh-runtime` | 10392 | shell *state*: variables/arrays/namerefs, options, builtin registry and state-only bodies, traps bookkeeping, completion registry, `declare -p` rendering; re-exports the history ring |
| `bsh-history` | 1244 | command history: the session ring (navigation, numbering) and the Turso database behind it, written from its own thread |
| `bsh-vm` | 12024 | the evaluator (tree-walking) and everything that needs both state and syscalls: command execution, redirection, child processes, traps |
| `bsh-pattern` | 4197 | Thompson-NFA glob: `O(n·m)`, never exponential; `extglob`/`nocasematch`/`utf8` flags |
| `bsh-diag` | 1946 | diagnostic model and miette rendering; error-code space |
| `bsh-complete` | 3928 | completion engine: providers, contexts, nucleo ranking, menu layout |
| `bsh-repl` | 3079 | line editor, key decoding, history navigation, prompt adapter, renderer, terminal abstraction |
| `bsh-utils` | 206 | embedded coreutils (uutils 0.8 subcrates): the `builtin-utils` fallback used when `PATH` finds nothing; on by default for Windows, opt-in on Linux |
| `bsh-highlight` | 386 | syntax highlighting (token → style), shared with the editor |
| `bsh-spec` | 3242 | the Oils-suite runner, comparator, ratchet |
| `bsh` | 1781 | the binary: argument parsing, startup files, REPL loop, history ownership swap |
| `bsh-tools` | 16 | dev helpers |

The evaluator is the runtime, and for now the only one: the bytecode backend is
deferred until the bash surface is smaller (`docs/direction.md` §2), with the
design kept in `DESIGN.md` §5. Dispatch is not where a shell spends its time — a
statement ends in a word expansion or a syscall — so a second engine today would
buy a few percent and cost a permanent agreement between two implementations.

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
  `PWD`, its background children (`\j`) and its history ring; `\!` is the
  number the line will get and `\#` bash's command number;
* **the stock prompt is fish-like**: while `PS1` is still the built-in
  `\s-\v\$ `, the rendered prompt is `~/src/bshell main !4 ?3 ` — the
  abbreviated `$PWD`, the git branch (read from `.git/HEAD`, following a `.git`
  file and `packed-refs` for worktrees), `!N` when background children exist,
  `?N` when the last status is non-zero, and one trailing space. It is built
  from bytes in one pass, so non-UTF-8 paths render exactly and nothing in the
  prompt is expanded twice. The `PS1` *variable* keeps bash's value, so
  `${PS1@P}` and the conformance `default PS1` case are unchanged. An explicit
  `PS1` wins and is rendered by the ordinary escape expander;
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
* **completion on `Tab`**, fish-like as well as bash-shaped:
  * the first Tab inserts the candidates' longest common prefix when it
    extends the typed word; the second opens the menu (a menu also opens on the
    first Tab when there is no new prefix to add), and one candidate is
    inserted directly without a menu;
  * the menu is fuzzy (`nucleo-matcher`; typed prefixes always outrank fuzzy
    matches), columnar with a description column, and degrades to one clipped
    column on a narrow terminal. Tab/Shift-Tab (and Up/Down, `Ctrl-P`/`Ctrl-N`)
    cycle, Enter inserts, Esc closes; its height is capped by the rows below
    the prompt;
  * command position is the deduplicated union of aliases, functions,
    builtins, keywords, `PATH` executables and variables, so `ec<TAB>` has
    `exec` and `fo<TAB>` has `for` (once per name, in bash's order);
  * variables: `$HO<TAB>` → `$HOME`, `${HO<TAB>` → `${HOME}` (bash closes the
    expansion), `$` alone lists names; `$?`, `$#`, `$@`, `$*`, `$$`, `$!`,
    `$-` and `$0`–`$9` are offered even though they are parameters rather than
    variables;
  * paths and directories, with insertion quoting from `bsh_lang::quote` that
    round-trips a space, `$`, a quote, a newline and non-UTF-8 (a file
    `a b$c` inserts as `a\ b\$c`); a directory ends in `/` and adds no space;
  * **programmable completion** from `complete`: `-W` word lists, the `-A`
    actions the engine can answer, `-P`/`-S`, and
    `-o nospace|default|bashdefault|dirnames|plusdirs`; `cd`-like commands
    complete directories;
  * history: at a blank command word the menu lists whole previous commands,
    newest first and fuzzy-ranked (the fzf-like picker); once a command word
    exists, history contributes words rather than whole lines. Ghost text
    offers the newest continuation of the line and is accepted with
    Right/End, dismissed with Esc for the current line (typing brings it
    back);
* **history as a shell feature**: one ring shared by the editor and the
  `history` built-in (`bsh-history`), backed by an embedded Turso database;
  `history [n]`, `-c` and `-d`; `Up`/`Down` navigation and ghost text. Numbers
  are the row ids, seeded from the newest 1000 rows of every session. There is
  no history file: `HISTFILE`, `HISTSIZE`, `HISTFILESIZE`, `HISTCONTROL`,
  `HISTIGNORE`, `HISTTIMEFORMAT`, the `histappend` shopt, `history
  -a/-n/-r/-w/-p/-s` and `fc` are gone. The database lives in
  `$XDG_DATA_HOME/bsh/history.db` (`%LOCALAPPDATA%\bsh\history.db` on
  Windows), `BSH_HISTORY_DB` overrides it, and an empty value disables
  persistence;
* **history expansion**: `!` events are rewritten before parsing by the pure
  `bsh_lang::histexpand` module and only in an interactive reader whose
  `histexpand` *and* `history` options are on (`-H` is part of `$-` for every
  interactive shell; a `-c` string and a script never expand). `!!`, `!n`,
  `!-n`, `!string`, `!?string?`, `!#`, `!$`/`!*`/`!^`/`!%`, `!:n`, ranges,
  `:h :t :r :e :p :q :x :s/…/…/ :gs/…/…/ :&`, and `^old^new^` are implemented
  with bash 5.3.20's measured quoting rules (single quotes and `$'…'` inhibit,
  double quotes do not, an odd backslash run escapes the `!`, a `#` comment is
  skipped, here-document bodies are not expanded). The ring stores the
  expanded line; a failed event abandons the line without recording it; `:p`
  prints it without running it and still records it. `shopt -s histverify`
  reloads the expansion into the editing line (the editor's
  `set_initial_line` seam) instead of running it;
* `Ctrl-C` on a line → discards it, `$?` = 130; `Ctrl-D` on an empty line →
  exit 0;
* **no job control** (W1, `docs/direction.md`): the shell and its children
  share one process group, so `Ctrl-Z` stops the child and the shell together,
  as bash does with `set +m`. There is no job table, no `%` specifications and
  no terminal handover. `jobs`, `fg`, `bg` and `disown` remain known names but
  report `BSH-E0302` (`CAPABILITY`) with status 1; `%1` operands to `wait`/`kill`
  do the same. `set -m`/`set -o monitor` is accepted and inert. The `Ctrl-C`
  path is unchanged: the child dies, the shell survives and `$?` is 130
  (`crates/bsh/tests/interactive_signals.rs`);
* `$!`, `wait` (pids, and bare `wait` for all live children) and `kill` (pids
  and signals) for children *this* shell started;
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

* The rendered stock prompt and the `PS1` variable differ on purpose: `$PS1`
  and `${PS1@P}` still report bash's `\s-\v\$ `, because the conformance suite
  reads that value, while the interactive renderer draws the fish-like
  prompt. A script cannot see the rendered form through `$PS1`.
* The default prompt's branch is the branch only: no dirty count, no
  ahead/behind, and no colour. The legacy `GitSegment` (bracketed `[branch]`
  with an optional `~N` dirty count from a worker-thread `git status`) is
  appended only to user-defined prompts, so the stock prompt never runs a
  subprocess.
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
* `\j` now counts live background children rather than jobs: it is 0 before a
  `&` child is tracked, 1 while it is, and 0 again after `wait`, in the prompt
  and in `${PS1@P}` alike. Without job control there is nothing else to count.

**Known history limitations** (measured or deliberately deferred): history
numbers are the database's row ids and are never reused, so `history -c` leaves
gaps (and the table keeps tombstones until the startup prune); another shell's
command can take the number `\!` would have predicted between the prompt and
the next insert;
`history -d` leaves a gap instead of renumbering; a running shell sees another
shell's commands only after a restart, because the ring is seeded once;
`shopt -s histreedit` is
accepted but a failed expansion abandons the line rather than reloading it for
editing (`histverify` *is* implemented); `:G` (per-word substitution) is not
implemented and reports an unknown modifier; the conformance suite's
interactive history cases mostly still fail on the bash-only `^D` marker, not
on history behaviour.

**Known job-control gaps** (deliberate, not defects to rediscover): there is no
job control, so `%` specifications and `jobs`/`fg`/`bg`/`disown` are capability
errors, `Ctrl-Z` stops the shell with its child, `disown -h` no longer exists
and the shell does not send `SIGHUP` to background children on exit. `$!` for a
background pipeline is the one subshell this shell forks, not bash's last-stage
pid.

**Known completion limitations** (measured against bash 5.3.20; not defects to
rediscover):

* **`complete -F` is not run.** A completion function is shell code. The
  completer is a `&dyn Completer` the editor borrows once for the whole loop and
  `Interp` is owned by `run_repl`; the provider traits are `Send + Sync` while
  `Interp` is not `Sync` (`Cell` fields), so a provider cannot hold the
  interpreter.
  Making it work needs a per-line completer that holds the shell mutably, a
  `COMP_*`/`COMPREPLY` save-restore around `ShellState`, and a way to keep a
  function's own stdout out of the editor's unflushed raw-mode frame (bash
  redraws above the prompt). `-C` and `-G` have the same blocker: they run
  shell code or expand a glob. A rule whose only source is one of them is
  reported as absent, so the command keeps its default completion.
  `compopt` is therefore only observable inside `compgen`, not at Tab.
* `-W` word lists are split on `IFS` but **not expanded**: expansion is shell
  code, and doing it at prompt-refresh time would run `$(…)` on every prompt.
  A `-W '$list'` therefore offers the literal `$list`.
* `-A setopt`, `shopt`, `user`, `group`, `hostname`, `job`, `service`,
  `binding`, `enabled`, `disabled`, `running` and `stopped` offer nothing at
  Tab: the engine has no shell state for them (`compgen -A` still answers them
  from the runtime).
* `complete -G`/`-X` are not applied at Tab, and no rule is applied to a
  redirection target or an assignment value (those are filenames/words).
* A completed filename containing `!` is inserted unescaped, so with history
  expansion on it is treated as an event at Enter. Bash's readline quoting
  avoids this; the completion insert path is out of this wave's scope.
* Array subscripts (`${arr[`) are not completed. bash 5.3.20 rings the bell
  there too, so this matches the reference rather than missing a feature.
* The fuzzy tier means a menu can be wider than bash's: `$HO<TAB>` lists
  `$HOME` first and then fuzzy matches such as `$DSH_HOME`. That is the
  recorded `DESIGN.md` §6.5 ranking decision, not a prefix-matching bug.
* `case` tags are still only the catch-all `*)`: nothing tracks the arms of the
  enclosing `case`, so `CaseTagProvider` stays at its default.
* `compgen -A signal` prints the runtime's short names (`HUP`) while the Tab
  menu uses bash's spelling (`SIGHUP`); the runtime spelling is the outlier.

**Known-untested:** terminal resize (there is a `TIOCGWINSZ` read; no `SIGWINCH`
handling seen); whether the line editor is restored correctly when a child
leaves the tty in raw/cbreak mode; and an interactive shell started in the
background (`bsh -i &`), which now shares its foreground group with no terminal
handover at all. `Ctrl-C` on a foreground command is covered by
`interactive_signals.rs`; `Ctrl-Z` is deliberately unhandled (it stops the shell
too) and is not tested.

Harnesses: `crates/bsh-sys/tests/pty.rs` (real pty, termios, window size,
`become_session_leader`), `crates/bsh/tests/interactive_signals.rs`
(`Ctrl-C` end to end), `crates/bsh/tests/interactive_history.rs`
(history persistence), `crates/bsh/tests/interactive_history_expansion.rs`
(`!!`, designators, quick substitution, `:p`, `histverify`, `set +H`, failed
events, and the expanded ring contents at a real prompt), `crates/bsh/tests/interactive_prompt.rs` (the rendered
prompt, `PROMPT_COMMAND`, prompt escapes, `PS1` from an rc file and from a
command, `Ctrl-R` end to end), `crates/bsh/tests/interactive_complete.rs`
(variable, keyword, function, `complete -W`, directory, `-o default`, history
and quoted-filename completion at a real prompt) and
`crates/bsh-repl/tests/editor.rs` (scripted keystrokes against a fake terminal,
including the search states).

## 5. Builtin inventory

56 names registered in `bsh-runtime`; 22 have a registry body, and several
more are the executor's. Eight are missing outright: `times`, `ulimit`,
`umask`, `dirs`, `enable`, `let`, `popd`, `pushd`. Four more (`[`, `printf`,
`pwd`, `test`) are intentionally exec-able.

* special (15): all but `times` are implemented.
* intrinsic (15): only `ulimit` and `umask` are missing; `fc` is not registered
  at all (it edited a file that no longer exists).
* regular (7): `false` and `true` in-process; `echo` is the VM's body; `[`,
  `printf`, `pwd`, `test` fall through to `PATH` by design.
* extension (18): missing `dirs`, `enable`, `let`, `popd`, `pushd`; `history`
  landed here.

`jobs`, `fg`, `bg` and `disown` are still registered and still VM-backed, but
their only behaviour is the `BSH-E0302` capability rejection. They are counted
as "implemented" above because the name resolves and the diagnostic is
deliberate, not because job control exists.

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
  `#![forbid(unsafe_code)]`. There is no fork gate: the shell does not fork.
  Inside it, every block
  carries a `// SAFETY:` comment and `clippy::undocumented_unsafe_blocks` is
  denied, so the invariant is enforced rather than remembered. Leaf syscalls
  prefer `rustix` over hand-written `libc` bindings: it already owns the
  `io-safety` shape, the `Errno` type and the per-architecture layouts. What
  stays raw is what no crate expresses with the shell's semantics —
  `posix_spawn` with file actions and `sigdefault`/`sigmask`, `execve`,
  target-number `dup2`, `trap` dispositions, and `regcomp`.
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
* **One execution engine, for now.** The tree-walking evaluator is the shell.
  A bytecode VM stays on the table for later — the crate is named for it — but
  it is not being built alongside.

## 8. Next steps

**The ordering in this section is superseded by [`docs/direction.md`](direction.md).**
The 2026-10-30 pivot replaced "conformance first, then interactive polish" with
five commitments (no `fork`, Windows, interactive-first, shell code on the main
thread, embedded uutils) and a six-wave plan. Read that file, not this list, to
decide what to do next; `docs/roadmap.md` is historical context for the
conformance queue.

Where the waves stand:

* **W1 (delete job control) — this change.** No job table, no `%` specs, no
  process groups/terminal handover, no `WUNTRACED`; the job built-ins report
  `BSH-E0302`; `set -m` is inert; the baseline is re-recorded at 1982/1927.
* **W3 (compio event loop) — in progress.** Done: the REPL runs inside one
  `compio` runtime (`Runtime::enter`, so individual operations can `block_on`
  without nesting the executor); raw stdin is read after a `PollFd` readiness
  wait instead of a blocking read; the `PATH` completion index is built on a
  worker and installed at the prompt; the prompt's git segment reads
  `.git/HEAD` synchronously and gets its dirty count from a worker's
  `git status` (500 ms minimum interval, coalesced, discarded if the directory
  changed). `read -t` keeps `poll(2)` on purpose: no runtime on the `-c` path.
  Measured with `scripts/bench.py` (release): startup 0.47 ms per spawn,
  `$(echo hi)` 7.5 ms per 1000 against bash's 170 ms, `( : )` 4.5 ms against
  149 ms — a scope costs ~2.3 µs and the snapshot model is *ahead* of bash.
  The clone only bites when the state is large: 200 subshells with a
  20 000-element array in scope are 69 ms against bash's 52 ms, which the
  bench now records as its own case. Copy-on-write (`Var.value` as
  `Arc<VarValue>`) was considered and deferred by decision on 2026-10-30;
  `direction.md` §9 records the trigger to revisit.
* **W2 (no-fork subshells)** is in progress. `( … )` and `$( … )` now run in
  a scope (`crates/bsh-vm/src/tree/scope.rs`) instead of a child; conformance is
  unchanged to the case. What is left, in order:
  1. **Pipelines — landed, with three follow-ups.** Externals keep
     `posix_spawn` and still run concurrently;
     the last stage runs in the parent (lastpipe, always on); a non-last stage
     that turns out to be shell code is buffered to an unlinked temp file with
     a 64 MiB cap and a diagnostic, because a pipe would deadlock against our
     own writer. The stage's kind cannot be decided by expanding the command
     word twice — that would run `$( … )`-bearing words twice — so the swap
     happens at the one dispatch site, after resolution: if the stage is not an
     external, descriptor 1 is moved from the pipe to the temp file there.
     Conformance is **+3** (`mapfile` from a pipeline, the two "`shopt -s
     lastpipe` is always on in OSH" cases, and the `ERR`-in-a-pipeline pairs),
     with one known loss: nested backticks. The `ERR` fix was a suppression
     counter: bash fires `ERR` once per *pipeline*, so a stage must not fire it
     at all — `{ false; } | { false; }` used to print twice.
  2. **Rejections — landed.** Shell-code `&` reports `BSH-E0302`; an external
     command still backgrounds, so `$!`, `wait $!` and `kill` work. `( exec cmd )`
     needed no rejection: the exit request is caught by the scope, so it already
     ends the subshell and not the shell. Cost: 16 Oils background cases
     (built-ins and compound commands in the background, `for … &`, and
     `a && b &`), which the baseline now records. One of them is recoverable
     later: a pipeline whose stages are *all* external could be backgrounded as
     a unit, since nothing needs a second interpreter.
  3. **Deletion — landed.** `fork_shell`, `enter_child`, `fork_gate`/`threadgate.rs`,
     `spawn_forked` (dead once nothing forks), and the completion-time external
     capture becomes spawn-and-read. Then re-measure and re-record the baseline.
  Job control is no longer in its way.

* **W4 (Windows) — in progress.** `bsh-sys` has a CRT-backed backend
  (`Fd(c_int)`, `CreateProcess` through `std::process::Command`,
  `TerminateProcess` for signals, explicit refusals for what Windows lacks),
  `bsh-lang`'s prompt helpers are portable, and deleting the history file took
  the last unix-only code out of `bsh-runtime`. `bsh-sys`, `bsh-lang`,
  `bsh-history` (turso 0.8.3 included, with the `windres` wrapper in
  `portability.md` §10), `bsh-runtime`, `bsh-vm`, `bsh-repl` and `bsh` itself
  cross-compile to `x86_64-pc-windows-gnu`. `bsh.exe -c 'echo hi'` runs under
  Wine, and so does a script with `$( … )`, `for`, `if`, a function, `case`,
  arithmetic, brace expansion and `while`. What is left: the standard utilities
  are exec-able by design and a Windows `PATH` has none (`printf`, `[`, `cat`
  report "not implemented" there — the embedded-uutils wave), ConPTY for the
  interactive editor, and PATHEXT-based command resolution.

  The utility fallback has landed: `crates/bsh-utils` embeds `printf`,
  `test`/`[`, `cat`, `mkdir`, `rm`, `ls`, `wc`, `head`, `tail`, `tr`, `seq` and
  `sleep`, the `builtin-utils` feature is opt-in on Linux and forced on for
  Windows, and the executor consults it only after `PATH` resolution fails
  (external utilities still win). Verified under Wine end to end. Cost:
  +3.7 MiB and 151 crates in that crate's tree; Linux release is thin LTO with
  16 codegen units (44 s), while `profile.windows` (fat LTO, one unit,
  stripped, 16.4 MiB) is built by CI so the local loop stays fast. Each
  embedded utility runs on its own worker thread, because uutils' message
  catalog is thread-local and single-utility: warm dispatch is 3.7 µs, and a
  utility's first call costs 57–191 µs (thread plus catalog).

## 9. Open questions

* **Startup file — decided.** `~/.bshrc` is sourced when it exists, otherwise
  `~/.bashrc`; `--rcfile` and `--norc` still win. A bashrc full of
  `bind`/`complete -D`/`PROMPT_COMMAND` half-applies, so a user who wants a
  bsh-specific file gets one without renaming the bash file.
* **History is a database, not a file — decided.** A line-oriented `HISTFILE`
  cannot record when a command ran, where, or with what status, and two shells
  sharing one rewrite each other's content. `bsh-history` keeps
  `commands(id, session, seq, text, cwd, started_at, duration_ms, status)` in
  an embedded Turso database (`$XDG_DATA_HOME/bsh/history.db`,
  `%LOCALAPPDATA%\bsh\history.db`), opens it only for the terminal REPL, and
  treats an empty `BSH_HISTORY_DB` as "no persistence". WAL plus a busy timeout
  is what lets two shells share the file: the second connection gets
  `experimental_multiprocess_wal(true)`, and a write that lands while another
  shell is writing waits instead of failing. A database that cannot be opened
  is reported once and the shell runs with an in-memory ring. Verified: two
  `Store`s on one file see each other's rows, and a non-terminal `-i` (piped
  stdin, `-c`) opens nothing, so the conformance run never touches the user's
  data directory.
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
* **Completion is refreshed as snapshots, not rebuilt per line — decided.**
  The editor borrows the engine once, so every source the completer can read is
  behind a `set_…(&self)` seam and `refresh_prompt` replaces it: `PATH`, `PWD`,
  `HOME`, variable/function/alias names, the history ring and a clone of the
  `complete` registry. `PROMPT_COMMAND` has already run and the ring is in the
  shell at that point, so a rule defined on the previous line is live on the
  next Tab. A per-line engine would have been simpler but would also rebuild the
  `PATH` index and the `nucleo` matcher on every keystroke's worth of typing.
* **`complete -F` is deferred deliberately.** The blockers are structural, not
  effort: the completer has no mutable interpreter (the provider traits are
  `Send + Sync`, `Interp` is not `Sync`), a function's output would land in the
  editor's unflushed raw-mode frame, and `COMP_*`/`COMPREPLY`/`compopt` need a
  begin/end lifecycle around `ShellState`. A `-F`-only rule falls back to the
  default completion instead of swallowing Tab, so sourcing bash-completion's
  `.bashrc` cannot make completion worse than it was before this wave.
* **`-W` word lists are split, not expanded.** Expansion is shell code; doing it
  in `refresh_prompt` would run a `$(…)` in a word list on every prompt, and
  the completer cannot run it at Tab. Literal lists (the common case) are exact.
* **The fuzzy matcher is nucleo's greedy path.** The optimal path in
  `nucleo-matcher` 0.3.1 panics on ordinary ASCII subsequences (`HOSTNAME` ←
  `HOM`, which variable completion now produces); greedy is `O(n)` and the tier
  rules still decide the product behaviour.
* **`cd`-style directory completion is a `complete` rule, not a special case.**
  `cd`/`pushd`/`popd`/`rmdir`/`mkdir` get `-A directory -o default`, so
  `cd $HO<TAB>` falls back to the variable provider and no provider carries a
  per-command conditional.
* **History expansion — measured, not assumed.** bash 5.3.20 stores the
  *expanded* line in the ring (`!!` raises the previous command's text, quick
  substitution its rewritten text, and `:p` its expansion), a failed event is
  not stored, and the expanded line is echoed to stderr before it runs. A
  `-c` string is never expanded, even with `-i -H`; a piped `-i` shell is
  line-at-a-time and does expand, which is why that path now runs the reader
  loop rather than the whole input as one chunk. `set +o history` disables
  expansion as well as recording, so both options gate it. `histverify`
  reloads the expanded line into the editor without re-running
  `PROMPT_COMMAND`; bash has no `!{…}` event form (`!{echo}` is an event named
  `{echo}` and reports "event not found"), so none was invented. Two bash
  quirks are deliberately reproduced because they are observable: the
  pre-scan only lets a backslash hide a following `'` or `!` (so `echo \!!`
  at end of line stays literal while `echo \!!x` expands), and quote state
  spans `PS2` lines (a `!` inside a continuing single-quoted string stays
  literal). `fc`'s editor form is still deferred.
* **`set -o vi`** exists in the option table but has no editor behind it.
* **Bytecode VM.** Deferred, not scheduled (`docs/direction.md` §2). The type
  sketch that used to sit in `bsh-compile` is deleted, and the two contradictions
  `DESIGN.md` §5 carried are resolved in it: chunks are `Vec<Op>`, and handlers
  are a static table rather than push/pop opcodes.
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
