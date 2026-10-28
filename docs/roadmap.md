# Roadmap: the 1186-case gap

The conformance baseline (README, DESIGN §8.2) was **1013/2787**; after the
tranches below it is **1388/2787**. bash passes 2574/2787 on the same run, so
1186 cases are things bash does and we do not.
This file is the work queue, ordered by cases recovered per unit of work, with
the suite file that measures each item.

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
**1681/2787 total, 1626 strict** (recorded in `crates/bsh-spec/baseline.txt`).

- [x] Shared pattern engine (`bsh-pattern`): Thompson NFA, byte and UTF-8
      modes, extglob, brackets, POSIX classes, no exponential blowup.
- [x] `${x#pat}`, `${x##pat}`, `${x%pat}`, `${x%%pat}` — 0/29 → 26/29.
- [x] `${x/a/b}`, `${x//a/b}`, `${x/#a/b}`, `${x/%a/b}` — 1/28 → 25/28.
- [x] `${x^pat}`, `${x^^pat}`, `${x,}`, `${x,,}`, including on `$@`/arrays.
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
- [ ] Brace expansion `{a,b}` and `{1..3}` (92 regressions with extglob).
- [x] `extglob` `?(…)`, `*(…)`, `+(…)`, `@(…)`, `!(…)` — lexed
      unconditionally (superset grammar, §3.3), matched per `shopt`.
      `extglob-files` 0/23 → 10/23. Two documented deviations: with extglob off
      `echo @(a)` prints where bash errors, and unspaced `!(…)` at the start of
      a statement stays pipeline negation (so `if !(a && b)` works, and
      `for x in !(a)` is a group).
      Remaining extglob failures are mostly brace expansion, not matching.
- [ ] `[[ =~ ]]` with `BASH_REMATCH` (34).
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
