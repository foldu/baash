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
cargo test --workspace --all-features -- --list | grep -c ': test$'   # 1012

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
| comparable cases | **1980 / 2787 = 71.0 %** |
| strict (primary expectation only) | 1925 / 2787 = 69.1 % |
| failing | 807 |
| timeouts | 0 |
| of the whole suite (3971 cases) | 49.9 % |
| as a fraction of bash's score | 1980 / 2574 = 76.9 % |
| share of the bash gap closed since 1013 | 967 / 1561 = 62.0 % |
| our test count | 1012 |

Measured with `bash 5.3.20` as the comparison shell, harness forced to
`LC_ALL=C` (bash scores *worse* under `C.UTF-8`: 2574 → 2435, so the C locale
is the fair denominator). We pass 46 cases bash fails; bash passes 640 we fail.

**1184 skipped** cases: 745 from the ysh-only suite files, 417 marked
`our_shell: ysh`, 22 needing a `python2` helper we do not provide. 100 % is not
a meaningful target: bash itself scores 92.4 %, and 55 of our passes come from
`## OK <shell>` alternatives rather than the primary expectation — which is
exactly the 71.0 % vs 69.1 % split, and why both numbers are ratcheted.

Baseline lives in `crates/bsh-spec/baseline.txt` (`total = 1980`,
`strict = 1925`). `--ratchet` exits 1 below either, 2 if it cannot measure.

## 3. Layers

57 779 lines of `src` across 14 crates.

| crate | lines | responsibility |
|---|---|---|
| `bsh-sys` | 3505 | the only `unsafe`. fork/exec (`posix_spawn` by default), descriptors, pipes/pty, termios, signals, `wait_any`, libc regex, global `fork_gate()` |
| `bsh-syntax` | 6607 | lexer + recursive-descent parser producing `Word`/`Command`; the superset grammar |
| `bsh-lang` | 9574 | shell *text* semantics: word expansion, patterns (`bsh-pattern`), parameter operators, prompt escapes, arithmetic; no I/O |
| `bsh-runtime` | 10030 | shell *state*: variables/arrays/namerefs, options, builtin registry and state-only bodies, traps bookkeeping, completion registry, `declare -p` rendering |
| `bsh-vm` | 11485 | the evaluator (tree-walking) and everything that needs both state and syscalls: command execution, redirection, jobs, traps |
| `bsh-pattern` | 3947 | Thompson-NFA glob: `O(n·m)`, never exponential; `extglob`/`nocasematch`/`utf8` flags |
| `bsh-diag` | 1946 | diagnostic model and miette rendering; error-code space |
| `bsh-complete` | 3137 | completion engine: providers, contexts, nucleo ranking, menu layout |
| `bsh-repl` | 3092 | line editor, key decoding, history, prompt adapter, renderer, terminal abstraction |
| `bsh-highlight` | 386 | syntax highlighting (token → style), shared with the editor |
| `bsh-spec` | 3242 | the Oils-suite runner, comparator, ratchet |
| `bsh` | 692 | the binary: argument parsing, startup files, REPL loop |
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
* **`PS2` continuation** — `for x in a b; do` ⏎ shows `> `, and the assembled
  command runs correctly after `done`;
* entering commands, running programs, `$((…))`, `exit`;
* syntax highlighting (raw mode, `bsh-highlight`, colour decided by fd 0);
* completion on `Tab` with a navigable menu (Up/Down/BackTab/Ctrl-P);
* history: loaded from `$HISTFILE`, appended on save, `Up`/`Down` navigation;
* `Ctrl-C` on a line → discards it, `$?` = 130; `Ctrl-D` on an empty line →
  exit 0;
* `$!`, `wait`, `kill`, job specs `%1`/`%%`/`%-` for children *this* shell
  started;
* `--rcfile FILE` / `--norc`; default startup file is `~/.bashrc`
  (bash-compatible by choice — **flagged as a question**, see §9).

**Missing, in order of how badly it hurts an interactive user:**

1. **Job control.** `jobs`, `fg`, `bg`, `disown` all answer
   `error[BSH-E0508]: …: not implemented yet` (verified with `bsh -c`). There is
   no per-job process group, no `tcsetpgrp` handover, no `SIGCHLD` reaping with
   `WUNTRACED`, no `set -m`. `bsh-vm/src/tree/jobs.rs` says so in its header:
   *"Real job control (`fg`, `bg`, `jobs`, `set -m`) is out of scope"* — that
   was true for the conformance wave and is the top item now.
   Consequence: `Ctrl-Z` stops the child (and the shell, since they share a
   process group) with nothing to resume them; a stopped child is never
   reaped.
2. **History as a shell feature.** No `history` builtin, no `fc`, no
   `Ctrl-R` search, no `HISTSIZE`/`HISTFILESIZE`/`histappend`/`HISTCONTROL`,
   and **no default history file**: `History::from_env` persists only when
   `$HISTFILE` is set, so out of the box nothing survives a session.
3. **Directory stack.** `pushd`, `popd`, `dirs` missing.
4. **Small but common builtins.** `umask`, `let`, `times`, `ulimit`, `enable`.
   `printf`, `pwd`, `test`, `[` are served from `PATH` — exec-able by design,
   but that means no `printf -v` and no `%q` portability, and `pwd` ignores
   `-P`/`-L` semantics.
5. **`promptvars`.** The editor renders the decoded `PS1` and never runs
   bash's second expansion, so `PS1='$PWD $ '` prints literally (equivalent to
   `shopt -u promptvars`). Documented in `bsh-repl/src/prompt.rs`.
6. **Editor depth.** No undo, no `vi` mode (`set -o vi` sets a shopt today but
   the editor ignores it), no `Ctrl-R`, no word-level kill/yank verification,
   no `Ctrl-L`. Bracketed paste appears in the key decoder (`Paste` in
   `key.rs`) but is unverified.
7. **No `PROMPT_COMMAND`.**

**Measured** with a PTY probe driving `bsh -i` and a foreground `sleep 30`:

* `Ctrl-C` **during** a foreground command already works. `SIGINT` to the
  terminal's foreground group kills the child, the shell survives, the prompt
  returns, `$?` is 130. The child shares the shell's process group today, so it
  gets the same signal — that is why this works, and it is also why `Ctrl-Z`
  fails.
* `Ctrl-Z` wedges the terminal. `SIGTSTP` to the group stops the child *and*
  the shell; nothing reaps the stopped child; after `SIGCONT` the shell is
  still blocked in `waitpid` on a child with 30 seconds left to run. Keystrokes
  are echoed by the tty and never read.

**Still untested:**

* terminal resize (there is a `TIOCGWINSZ` read; no `SIGWINCH` handling seen).
* whether the line editor is restored correctly when a child leaves the tty in
  raw/cbreak mode.

Harnesses that already exist for this: `crates/bsh-sys/tests/pty.rs` (real
pty, termios, window size, foreground pgroup) and
`crates/bsh-repl/tests/editor.rs` (scripted keystrokes against a fake
terminal). A job-control test should be built on the former.

## 5. Builtin inventory

55 names registered in `bsh-runtime`; 37 have a body (runtime handler or VM
body); 18 do not, of which 4 are intentionally exec-able.

* special (15): all but `times` are implemented.
* intrinsic (16): missing `bg`, `fc`, `fg`, `jobs`, `ulimit`, `umask`.
* regular (6): `false`, `true`, `echo` implemented; `[`, `printf`, `pwd`,
  `test` fall through to `PATH` by design.
* extension (18): missing `dirs`, `disown`, `enable`, `history`, `let`, `popd`,
  `pushd`.

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

1. **Job control.** Process groups per job, `tcsetpgrp` handover around
   foreground execution, `SIGCHLD`/`WUNTRACED` reaping, `jobs`/`fg`/`bg`/
   `disown`, `set -m` (on by default interactive), stopped-job notices, `$!`
   for pipelines. Touches `bsh-sys` (pgroups, `tcsetpgrp`, signal masks),
   `bsh-vm` (job table, builtins, `wait`), `bsh-repl` (`Ctrl-Z` passthrough,
   tty restore), `bsh` (interactive wiring).
2. **History.** `history` and `fc` builtins, default `~/.bsh_history`,
   `HISTSIZE`/`HISTFILESIZE`/`histappend`/`HISTCONTROL`, `Ctrl-R` incremental
   search in the editor.
3. **Prompt parity.** `promptvars` second expansion, `PROMPT_COMMAND`, bash's
   default `PS1`, `\!`/`\#` history numbers.
4. **Common builtins.** `pushd`/`popd`/`dirs`, `umask`, `let`, `times`,
   `ulimit`, `enable`, and `printf`/`pwd` bodies so `printf -v` and `pwd -P`
   exist.
5. **Editor depth.** `vi` mode, undo, `Ctrl-L`, `Ctrl-R`, word motions,
   bracketed paste (verify first).
6. **Completion depth.** `$VAR`/`${…}` completion, command names from `PATH`,
   quoting of inserted filenames, and wiring the `complete -F` registry to the
   engine so programmable completion is real.
7. **Back to conformance**, resuming with the clusters in §6 — interactive
   semantics first (`sh-options`, `errexit-osh`), then builtins, then patterns.

## 9. Open questions

* **Startup file.** We source `~/.bashrc` (bash-compatible). For a shell with
  partial bash support that is a footgun as much as a feature: a bashrc with
  `bind`, `complete -D`, `PROMPT_COMMAND` will half-apply. Decide between
  `~/.bshrc` with `.bashrc` fallback, or keep bash parity and document it.
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
