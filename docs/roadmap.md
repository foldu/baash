# Roadmap: the 397-case gap

The conformance baseline (README, DESIGN §8.2) started at **1013/2787** and is
now **2235/2787** (2180 strict). bash passes 2577/2787 on the same run and
410 of the cases we fail are ones bash passes (we pass 68 it fails), so the net
gap is 342 cases.
This file is the conformance queue, ordered by cases recovered per unit of
work, with the suite file that measures each item — **not** the current work
queue. Interactive usability now outranks script conformance; see
[`state.md`](state.md) §8 for the order of work.

Update the numbers here after each tranche by running:

```sh
cargo build -p bsh
cargo run -p bsh-spec -- --shell target/debug/bsh --filter <file>
```

## Tranche 1 — the three biggest clusters (584 cases of the gap)

### Arrays — 67 regressions (207 compared, 140 pass today)

Measured by `array.test.sh` (78), `array-sparse.test.sh` (40),
`array-literal.test.sh` (19), `array-basic.test.sh`, `array-assoc*.test.sh`,
`array-elem.test.sh`, `array-compat.test.sh`, `assign.test.sh`,
`var-sub-quote.test.sh`.

- [x] Sparse storage. `a[100]=x` must not allocate 101 slots, and `unset 'a[0]'`
      must leave a hole that `${a[@]}` skips. `VarValue::Indexed` is a
      `BTreeMap<i64, Vec<u8>>` now.
- [x] `${a[i]}` element read, including arithmetic in the subscript.
- [x] `${a[@]}` / `${a[*]}`, and the quoted forms: `"${a[@]}"` is one field per
      element (none when empty), `"${a[*]}"` is one field joined by the first
      `IFS` character.
- [x] `${#a[@]}` count, `${#a[i]}` element length.
- [x] `${!a[@]}` indices (sorted numerically for indexed, insertion order for
      associative).
- [x] `a=(1 '2 3' $x)` literal, `a+=(…)` append, `a=()` empty, plus the
      `[k]=v` / `[k]+=v` element forms and bare-element index continuity.
- [x] `a[i]=v`, `a[i]+=v`, and subscript assignment through `declare`/`local`.
- [x] `unset 'a[i]'`, `unset a`.
- [x] `declare -a`, `declare -A`, `local -a`, `local -A`, `readonly -a`/`-A`,
      array listing, and `declare -p` output that is re-input safe.
- [x] `${a[@]:offset:length}` slicing, including arithmetic bounds, plus
      `${@:offset:length}`.
- [ ] `read -a` (with the `read` tranche, in flight).
- [ ] `set -u` on a missing element; empty array under `"${a[@]}"` with `-u`
      (`nounset` itself is still unimplemented).

### `read` and the built-in cluster — 411 regressions — read is **DONE** (83/115), built-ins in flight

`builtin-read.test.sh` went 9/64 → **63/64** (the one left is `mapfile`), the
`read` filter 24/115 → **83/115**, and `kill` 2/21 → **17/21**.

- [x] `read var`, IFS splitting into several names, last name takes the rest.
- [x] `-r`, backslash escaping and line continuation.
- [x] `-d delim`, `-n count`, `-N count`, `-p prompt`, `-s`, `-u fd`, `-t timeout`, `-a array`.
- [x] EOF returns 1 with a partial line delivered; **never over-read** — a
      second reader must see the rest of the input.
- [x] `getopts` with `OPTIND`/`OPTARG` (28/31).
- [x] `type`, `command -v`/`-V`, `hash`, `builtin` (42/68 type, 114/184 command).
- [x] `alias`/`unalias` (47/63), applied only at command position and not
      recursively to themselves.
- [x] `unset -f`, `unset -v`, `readonly -a`.
- [x] `kill` (also needed by the `trap` tests).
- [ ] `builtin`, which several `kill -l` cases use (`builtin kill -l`).

### `trap` — **DONE: 0 → 43 of 84** (builtin-trap 32/38)

`builtin-trap.test.sh` went 0/38 → **32/38**; the whole `trap` filter 0/84 →
**43/84**. The six left in `builtin-trap.test.sh` need incremental parsing
(`trap EXIT` and then a parse error), `$SH -e` on the command line (the CLI
does not accept `-e`), or a script executed by path with no shebang (the shell
does not yet retry `ENOEXEC` as a script) plus `command -v`.

- [x] `trap 'code' LIST`, several signals in one call, symbolic and numeric.
- [x] `trap '' SIG` ignore (survives `exec`), `trap - SIG` reset.
- [x] `trap` and `trap -p` print in re-input-safe form; `trap -l` lists names.
- [x] `EXIT` runs on every exit path, with `$?` visible and its own status
      preserved when it does not call `exit`.
- [x] `ERR` with and without `set -e`, and the ignore-context rules (§4.7).
- [x] `DEBUG` and `RETURN` (bash's inheritance rules: `-o errtrace`,
      `-o functrace`).
- [x] Signal traps delivered at command boundaries, never mid-built-in; a trap
      that arrives during a foreground child runs after it.
- [x] Traps reset in subshells, except ignored ones; a trap in a pipeline
      member affects only that member.
- [ ] The bash DEBUG `$LINENO` cases: they need `$LINENO` (tranche 2) and
      bash's exact trap-count quirks, and are not worth chasing before it.

## Tranche 2 — expansion and pattern work

At the start of this tranche the suite stood at 1013/2787 total. It is now
**1797/2787 total, 1744 strict** (recorded in `crates/bsh-spec/baseline.txt`).

- [x] Shared pattern engine (`bsh-pattern`): Thompson NFA, byte and UTF-8
      modes, extglob, brackets, POSIX classes, no exponential blowup.
- [x] `${x#pat}`, `${x##pat}`, `${x%pat}`, `${x%%pat}` — 0/29 → 26/29.
- [x] `${x/a/b}`, `${x//a/b}`, `${x/#a/b}`, `${x/%a/b}` — 1/28 → 25/28.
- [x] `${x^pat}`, `${x^^pat}`, `${x,}`, `${x,,}`, including on `$@`/arrays.
- [x] `${!x}` and `${!x<op>}` indirect expansion (name, positional, special,
      subscripted; operators applied to the target). `var-ref.test.sh` 6/31 →
      16/31.
- [ ] `${x@Q}`, `${x@E}`, `${x@P}`, `${!prefix@}` (the rest of `var-op-bash`).
- [x] `shopt` plus the `pattern_flags()` override, so `extglob`/`nocasematch`
      reach the matcher.
- [x] `failglob` — fails the command, not the shell (bash's script behaviour);
      `${x:?}` still ends a non-interactive shell.
- [x] UTF-8 character units end to end: `Vars::utf8()` (from
      `LC_ALL`/`LC_CTYPE`/`LANG`, detected only by the binary so tests stay
      deterministic) drives `${#x}`, `${x:o:l}` and the matcher's `?` through
      one decoder (`bsh_pattern::char_len`). `LC_ALL=C` still counts bytes and
      still slices characters in half, like bash and dash.
- [x] Brace expansion `{a,b}`, `{1..3}`, `{01..03}`, `{a..e..2}` - stage 0,
      before every other expansion; 21/72 -> 59/72 on the brace filter, and it
      unblocked most of `extglob-files` (10/23 -> 21/23). `set +B` turns it off.
- [x] `extglob` `?(…)`, `*(…)`, `+(…)`, `@(…)`, `!(…)` — lexed
      unconditionally (superset grammar, §3.3), matched per `shopt`.
      `extglob-files` 0/23 → 10/23. Two documented deviations: with extglob off
      `echo @(a)` prints where bash errors, and unspaced `!(…)` at the start of
      a statement stays pipeline negation (so `if !(a && b)` works, and
      `for x in !(a)` is a group).
      Remaining extglob failures are mostly brace expansion, not matching.
- [x] `-O`/`+O name` on the command line, so a *script* can be parsed with
      `extglob` on (the option is parse-time, so `shopt` inside the file cannot
      do it). The `!(…)` reading is now decided by `ParseOpts::extglob`, which
      the lexer finally receives: with the option on, `!(x)` at line start is a
      group; with it off, it is pipeline negation.
- [x] `--locale NAME` in `bsh-spec`, so the suite can be run under any locale.
      Measured: **C gives 1701, C.UTF-8 gives 1716** for us, but bash drops
      from **2574 to 2435** under C.UTF-8 — the suite's primary expectations
      are byte-calibrated, so C stays the baseline and UTF-8 is diagnostic.
- [x] `[[ =~ ]]` with `BASH_REMATCH` - `regex.test.sh` 0/37 -> 25/37, via libc
      `regcomp`/`regexec` in `bsh-sys` so POSIX leftmost-longest matches bash.
- [~] `${x@P}` / prompt expansion - in flight (prompt 0/33 and most of
      `var-op-bash`).
- [x] `$LINENO` (from node spans) and `$PIPESTATUS`.
- [ ] `$RANDOM`, `$SECONDS`, `$FUNCNAME`, `$BASH_SOURCE`, `BASH_LINENO`.

## Tranche 3 — process and job control

- [ ] `wait`, `jobs`, `fg`, `bg`, `disown`, and `set -m`.
- [ ] Process substitution `<(...)`, `>(...)`.
- [ ] `select`, `time`, `coproc` (or explicit refusals).
- [ ] `set -x` / `set -v` tracing.

## Not planned

`[[` in `sh` mode only, `ysh` (1162 skipped cases), `coproc` if it never earns
its keep, `zsh` emulation.

## Tranche 4 — the measured queue (after indirect expansion)

Measured with `bsh-spec --json` on the current tree: 2180/2787 total, 2127
strict, 607 failures, of which 464 are cases bash passes and 143 are cases
bash fails too (not worth chasing). Ordered by cases per unit of work; every
entry names the suite file that measures it.

### Tier A — cheap, and the design already supports them (~140 cases)

- [x] `${!prefix@}` / `${!prefix*}` and `${x@a}` — `var-op-bash` 15/26 -> 22/26.
      `names()` and `attributes()` on the `Vars` seam; arrays repeat the
      attribute per element.
- [x] `noclobber` (`set -C`): `>` refuses an existing regular file, `>|`
      overrides it, and devices/FIFOs stay writable — `sh-options` 29/39 -> 32/39.
- [x] `$-` and invocation flags — `sh-options` 20/39 -> 32/39, `sh-usage`
      11/24 -> 23/24. `$-` lists only options that differ from the profile,
      `i` comes from the interactive path, and `shopt` gained the `set -o`
      table (`-o`, `-p -o`, `-q`).
- [x] `umask`, numeric and symbolic — `builtin-umask` (21), now 24/24. The
      symbolic grammar follows `osh`, including `X`, `s`/`t`, permission copies
      and the mask truncation bash rejects.
- [x] Executable files with no `#!` run as shell scripts, in process, and a
      name containing `/` is tried even when it is not executable so the
      failure is 126 rather than 127 — `command_` 8/16 -> 13/16. A shell
      reading stdin also keeps a `$0`, which fixed `${@:0}` (`var-op-slice`
      8/22 -> 12/22) and the `$0` cases in other files.
- [x] `ulimit` over the real `getrlimit`/`setrlimit` (one resource at a time,
      `-a` summary, `-S`/`-H`, 512/1024-byte units, `unlimited`) and `exit`’s
      usage statuses — `builtin-process` 10/25 -> 23/25.
- [~] `exec --`, `exec -a`, redirect-only `exec` — `builtin-process` (18),
      now 7/25 → 10/25; the rest of the file is `exec` failure modes and
      dynamic-loader behaviour.
- [x] `pushd` / `popd` / `dirs` — `builtin-dirs` (16), now 18/18. The stack
      lives in `ShellState` so a subshell cannot leak it; `cd` rewrites element
      0, and paths stay lexical so a symlink keeps the name typed.
- [~] Special variables — `vars-special` 20/42 -> 35/42. `PATH`, `IFS`,
      `PS4`, `SECONDS`, `_`, `RANDOM`, `UID`/`EUID`, `PPID`, `OSTYPE` and
      `HOSTNAME`; `HISTFILE` stays a deliberate divergence.
- [x] `GLOBIGNORE` — `globignore`, 2/18 -> 17/18. A match filter with a
      bracket-aware colon split; it implies `dotglob`, and an entry without `/`
      only hides files in the directory being expanded.
- [ ] `${x:o:l}` unicode/negative slices, `${x@}` case folding — `var-op-slice`
      (14) + `var-op-bash` (part of 14).

### Tier B — medium, still inside the design (~150 cases)

- [ ] `DEBUG`/`ERR` trap refinements (`$?`, `LINENO`, `errexit` interplay) —
      `builtin-trap-bash` (19) + `builtin-trap-err` (17).
- [x] `printf` as a real built-in (`-v`, `%q`, `%(…)T`, `\u` escapes, the
      full POSIX format grammar and bash's argument-count and error rules) and
      `test`/`[` ported from bash's `test.c` (`-v`, `-R`, `-o`, `-a`/`-o`
      ambiguity and all) — `builtin-printf` 42/61 -> **60/61** (the remaining
      case's primary expectation is an older bash's status) and the
      `test`/`[` clusters (`builtin-bracket`, `blog2`) now pass in full. Both
      bodies live in `bsh-runtime`, where `Vars`, `Opts` and the array
      subscripts already are.
- [ ] `set -x` / `PS4` / `set -v` — `xtrace` (16).
- [ ] `FUNCNAME` / `BASH_SOURCE` / `BASH_LINENO` — `introspect` (12), and the
      two remaining `var-ref` cases.
- [ ] Arithmetic inside `${…}` and quoted constants — `arith` (13).
- [ ] Array subscripts through an indirect reference, negative out-of-range
      assignment — `array-sparse` (19, of which 15 fail for bash too).
- [ ] Named file descriptors and `{fd}>` — `redirect` (11).
- [ ] `{` as a regex parse error, fatal errors inside `[[ =~ ]]` —
      `regex` (12).
- [ ] `sh --rcfile`/`--norc`/`--rcdir` startup selection — `interactive` (13).
- [ ] Array literal/parse edge cases — `array` (13).

### Tier C — cheap only in the strict column

53 cases pass today *only* because a per-shell `## OK …` alternative matched.
Matching the primary expectation on them raises strict by up to 54 without
changing behaviour; work through `--list-failures` on a full run and compare
each case's primary output with the alternative we accept.

### Not planned

`history -a/-w/-r` (15), `fc` (12), `wait -n`/jobspec/job control (18),
`HISTFILE` (inside `vars-special`), `strict_errexit`/`strict_arith` (23 + 11),
and the 151 cases bash fails too. These are decisions, not gaps.

**Target if Tier A and B land: +250–290 → about 2230/2787 (80%), strict
around 2170.**

## Tranche 5 — the remaining bash gap (active goal)

Re-measured with `--json --compare bash` after items 1–8 below:
**2235/2787 total, 2180 strict**. 552 failures, of which **410 are cases bash
passes** (the real queue) and 142 are cases bash fails too (osh-only
expectations, older-bash behaviour, environment). The plan is to work these in
order, cheapest first, re-measuring after each commit:

Status per item: **[done]** landed with fmt/clippy/tests/ratchet green;
**[open]** next; **[decision]**, **[won't]** — a design choice recorded here so
it is not re-litigated.

1. **[done]** `errexit` in command substitution (`dca52a3`).
2. **[done]** `SHELLOPTS`/`BASHOPTS` from the live option table, readonly, and
   an exported `SHELLOPTS` driving xtrace in children (`dca52a3`).
3. **[done]** `shopt` unknown-name status 1, permissive top-level
   `break`/`continue`, `shopt -p` (`ab0eafa`).
4. **[done]** `set` parsing (`set -`, `set +`, `set - -`, `set - +`) and
   `set -a` exporting `local`/`declare` variables (`33a3454`).
5. **[done]** Malformed `[[ … ]]` rejected before the chunk runs (`e9a97d6`).
6. **[done]** `echo -e` `\uHHHH`/`\UHHHHHHHH`, incomplete escapes
   (`50ca7ab`).
7. **[won't]** Brace ranges with zero, negative or wrong-sign steps as
   diagnostics: bash 5.3.20 accepts them exactly as we do (`{1..8..-3}` prints
   one number), so the osh cases that want a diagnostic are osh-only and
   unwinnable without diverging from bash.
8. **[done]** `~user` via the platform user database, `~` falling back to it
   when `HOME` is unset, and declaration operands (`declare x=~/p`) expanding
   the tilde that starts the value — `tilde`, `builtin-eval-source`, `var-ref`.
   Also made a malformed array subscript an *expansion* error in every context
   (`${a[~+]}`, `a[~+]=1`, `declare -i n; n=1+`), which bash fails the same
   way: `var-ref` "Bizarre tilde expansion in array index".
9. **[open]** `cd -L`/`-P`, `CDPATH`, and `pwd` through symlinks —
   `builtin-cd` (5–7).
10. **[open]** `${x:offset:length}` slices (arithmetic, negative starts,
    invalid UTF-8) and `${a[@]}`/`${a[*]}` `-`/`=`/`+` alternates with
    `set -u` — `var-op-slice` + `var-op-test` (16).
11. **[open]** `break`/`continue`/`return` through `eval`/`source`/subshells
    (the unwinding part is done), and alias expansion producing syntax across
    lines — `loop` (8) + `alias` (8).
12. **[open]** `FUNCNAME`, `BASH_SOURCE`, `BASH_LINENO` from the frame stack —
    `introspect` (10), part of `var-ref`.
13. **[open]** `${!x}` indirection edges: `$?`, array/assoc subscripts, bad
    references, strict-array cases — `var-ref` (rest).
14. **[open]** DEBUG/ERR trap exactness: `$?`, `LINENO`, `||`/`!`/pipeline
    exclusion, `errtrace` — `builtin-trap-bash` (19) + `builtin-trap-err` (16).
15. **[open]** Arithmetic contexts: `${…}`, quoted constants and backticks
    inside `$(( ))`, `nounset` — `arith` (13) + `arith-context` (7).
16. **[open]** `[[ =~ ]]` right-hand-side parsing: unquoted `(`/`)`/`{`,
    spaces, `;`/`&` — `regex` (12).
17. **[open]** NUL bytes through expansion and the quoting edge cases —
    `nul-bytes` (8) + `var-sub-quote` (7) + `command-sub` (4).
18. **[open]** Interactive `PROMPT_COMMAND` (four cases) and `--rcdir`/`--norc`
    (`interactive`, minus the excluded `HISTFILE` half).
19. **[decision]** Background shell code (`{ …; } &`, `for … &`) and process
    substitution (`<(…)`/`>(…)`): the only remaining architectural question.
    Either a worker-thread subinterpreter or a documented rejection —
    `background` (15) + `process-sub` (6).

Still out of scope (decisions, not gaps): `HISTFILE`/`history -w/-r/-n`
(~19), readline `bind` (9), job control/jobspecs (~8), the ysh dialect, and
the 142 cases bash fails too.

**Target: ≥2300 total / ≥2250 strict, then the background/process-sub
decision. Each step lands as its own commit with fmt, clippy, workspace tests
and the spec ratchet green.**
