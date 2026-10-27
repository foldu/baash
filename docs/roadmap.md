# Roadmap: the 1579-case gap

The conformance baseline (README, DESIGN §8.2) is **1013/2787**. bash passes
2574/2787 on the same run, so 1579 cases are things bash does and we do not.
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

### `read` and the built-in cluster — 411 regressions (712 compared, 224 pass)

- [ ] `read var`, IFS splitting into several names, last name takes the rest.
- [ ] `-r`, backslash escaping and line continuation.
- [ ] `-d delim`, `-n count`, `-N count`, `-p prompt`, `-s`, `-u fd`, `-t timeout`, `-a array`.
- [ ] EOF returns 1 with a partial line delivered; **never over-read** — a
      second reader must see the rest of the input.
- [ ] `getopts` with `OPTIND`/`OPTARG`, silent and verbose modes.
- [ ] `type`, `command -v`/`-V`, `hash`.
- [ ] `alias`/`unalias`, including aliases applied only at command position and
      not recursively to themselves.
- [ ] `unset -f`, `unset -v`, `readonly -a`.
- [ ] `kill` (also needed by the `trap` tests).

### `trap` — 38 cases in `builtin-trap.test.sh`, plus `trap-*.test.sh`

- [ ] `trap 'code' LIST`, several signals in one call, symbolic and numeric.
- [ ] `trap '' SIG` ignore (survives `exec`), `trap - SIG` reset.
- [ ] `trap` and `trap -p` print in re-input-safe form; `trap -l` lists names.
- [ ] `EXIT` runs on every exit path, with `$?` visible and its own status
      preserved when it does not call `exit`.
- [ ] `ERR` with and without `set -e`, and the ignore-context rules (§4.7).
- [ ] `DEBUG` and `RETURN`.
- [ ] Signal traps delivered at command boundaries, never mid-built-in; a trap
      that arrives during a foreground child runs after it.
- [ ] Traps reset in subshells, except ignored ones; a trap in a pipeline
      member affects only that member.

## Tranche 2 — expansion and pattern work

- [ ] `${x#pat}`, `${x##pat}`, `${x%pat}`, `${x%%pat}` (152 regressions) — needs
      the shared pattern engine (§4.8), which also unblocks `case` patterns
      with extglob and `[[ == ]]`.
- [ ] `${x/a/b}`, `${x//a/b}`, `${x/#a/b}`, `${x/%a/b}`.
- [ ] `${x^}`, `${x^^}`, `${x,}`, `${x,,}`, `${x@Q}`, `${x@E}`, `${x@P}`.
- [ ] Brace expansion `{a,b}` and `{1..3}` (92 regressions with extglob).
- [ ] `extglob` `?(…)`, `*(…)`, `+(…)`, `@(…)`, `!(…)`.
- [ ] `[[ =~ ]]` with `BASH_REMATCH` (34).
- [ ] `$RANDOM`, `$SECONDS`, `$PIPESTATUS`, `$FUNCNAME`, `$BASH_SOURCE`.

## Tranche 3 — process and job control

- [ ] `wait`, `jobs`, `fg`, `bg`, `disown`, and `set -m`.
- [ ] Process substitution `<(...)`, `>(...)`.
- [ ] `select`, `time`, `coproc` (or explicit refusals).
- [ ] `set -x` / `set -v` tracing.

## Not planned

`[[` in `sh` mode only, `ysh` (1162 skipped cases), `coproc` if it never earns
its keep, `zsh` emulation.
