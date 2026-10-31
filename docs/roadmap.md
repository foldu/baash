# Roadmap: the 555-case gap

The conformance baseline (README, DESIGN §8.2) started at **1013/2787** and is
now **2019/2787** (1965 strict). bash passes 2574/2787 on the same run, so 555
cases are things bash does and we do not.
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

Measured with `bsh-spec --json` on the current tree: 2019/2787 total, 1965
strict, 768 failures, of which 555 are cases bash passes and 213 are cases
bash fails too (not worth chasing). Ordered by cases per unit of work; every
entry names the suite file that measures it.

### Tier A — cheap, and the design already supports them (~140 cases)

- [ ] `${!prefix@}` / `${!prefix*}` and `${x@Q}`/`${x@E}` — `var-op-bash` (14).
      Needs a name listing on the `Vars` seam for the prefix forms.
- [ ] `$-` reflects the *active* options, and `sh` invocation flags
      (`+c`, `--`, flags after the script) — `sh-options` (19) + `sh-usage`
      (13). Today `$-` is a fixed `Bhu`.
- [x] `umask`, numeric and symbolic — `builtin-umask` (21), now 24/24. The
      symbolic grammar follows `osh`, including `X`, `s`/`t`, permission copies
      and the mask truncation bash rejects.
- [~] `exec --`, `exec -a`, redirect-only `exec` — `builtin-process` (18),
      now 7/25 → 10/25; the rest of the file is `exec` failure modes and
      dynamic-loader behaviour.
- [x] `pushd` / `popd` / `dirs` — `builtin-dirs` (16), now 18/18. The stack
      lives in `ShellState` so a subshell cannot leak it; `cd` rewrites element
      0, and paths stay lexical so a symlink keeps the name typed.
- [ ] Special variables initialised only when unset (`PATH`, `PWD`, `IFS`,
      `PS4`) — `vars-special` (22; `HISTFILE` is a deliberate divergence).
- [ ] `GLOBIGNORE` — `globignore` (16). One rule in the glob layer.
- [ ] `${x:o:l}` unicode/negative slices, `${x@}` case folding — `var-op-slice`
      (14) + `var-op-bash` (part of 14).

### Tier B — medium, still inside the design (~150 cases)

- [ ] `DEBUG`/`ERR` trap refinements (`$?`, `LINENO`, `errexit` interplay) —
      `builtin-trap-bash` (19) + `builtin-trap-err` (17).
- [ ] `printf -v`, `%q`, no-arg usage — `builtin-printf` (19). **Needs a
      decision**: `printf` is deliberately external; only bash's builtin has
      `-v`. Either add a VM body with PATH-first kept for plain calls, or drop
      this cluster.
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

54 cases pass today *only* because a per-shell `## OK …` alternative matched.
Matching the primary expectation on them raises strict by up to 54 without
changing behaviour; work through `--list-failures` on a full run and compare
each case's primary output with the alternative we accept.

### Not planned

`history -a/-w/-r` (15), `fc` (12), `wait -n`/jobspec/job control (18),
`HISTFILE` (inside `vars-special`), `strict_errexit`/`strict_arith` (23 + 11),
and the 151 cases bash fails too. These are decisions, not gaps.

**Target if Tier A and B land: +250–290 → about 2230/2787 (80%), strict
around 2170.**
