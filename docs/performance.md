# Performance

Measured 2026-10-28 at `e405c92` against `bash 5.3.20` (`/run/current-system/sw/bin/bash`),
32-core Linux, `LC_ALL=C`, no rc files, best of 3–5 runs per case. The debug
column is what `cargo build` gives you and what `cargo run`/`cargo test` use;
the release column is `cargo build --release`.

| case | bash | baash debug | baash release |
|---|---:|---:|---:|
| startup, per spawn (100 in a batch) | 0.52 ms | 0.59 ms (1.1×) | 0.40 ms (0.8×) |
| arithmetic loop, 20k `((i++))` | 9.2 ms | 233 ms (24.7×) | 14.8 ms (1.6×) |
| `${s#*c}`, 20k | 23.7 ms | 398 ms (17.1×) | 35.1 ms (1.5×) |
| `${s//d/e}`, 20k | 25.8 ms | 623 ms (24.6×) | 53.2 ms (2.1×) |
| function call, 20k | 31.8 ms | 435 ms (14.1×) | 33.1 ms (1.0×) |
| `case` dispatch, 20k | 15.2 ms | 329 ms (21.1×) | 26.8 ms (1.8×) |
| builtin `echo`, 20k | 21.1 ms | 413 ms (19.1×) | 32.2 ms (1.5×) |
| fork+exec, 200 (`env true`) | 184 ms | 174 ms (1.0×) | 164 ms (0.9×) |
| pipeline, 200 (`echo x \| cat`) | 123 ms | 161 ms (1.3×) | 133 ms (1.1×) |
| command substitution, 1000 | 171 ms | 220 ms (2.0×) | 128 ms (0.7×) |
| subshell, 1000 (`( : )`) | 143 ms | 158 ms (1.1×) | 105 ms (0.7×) |

### 2026-11-01: dynamic specials, and two variable-table fast paths

Re-measured at the commits that made `$SECONDS`/`$RANDOM` lazy and added the
lookup fast paths below. Same machine, same method, best of 3.

| case | bash | baash release |
|---|---:|---:|
| startup, per spawn | 0.52 ms | 0.69 ms (1.3×) |
| arithmetic loop, 20k `((i++))` | 9.4 ms | 6.9 ms (**0.7×**) |
| `${s#*c}`, 20k | 23.4 ms | 20.2 ms (**0.9×**) |
| `${s//d/e}`, 20k | 26.4 ms | 16.4 ms (**0.6×**) |
| function call, 20k | 31.4 ms | 38.7 ms (1.2×) |
| `case` dispatch, 20k | 15.3 ms | 14.1 ms (**0.9×**) |
| builtin `echo`, 20k | 21.5 ms | 21.0 ms (**1.0×**) |
| fork+exec, 200 (`env true`) | 186 ms | 169 ms (0.9×) |
| pipeline, 200 | 124 ms | 92.2 ms (0.7×) |
| command substitution, 1000 | 176 ms | 7.6 ms (0.04×) |
| subshell, 1000 (`( : )`) | 145 ms | 4.3 ms (0.03×) |
| subshell 200, 20k-element array | 50.6 ms | 65.5 ms (1.3×) |

Three changes, each measured with `perf stat -e instructions:u` and callgrind
rather than guessed at:

* `$SECONDS` and `$RANDOM` are dynamic variables now: their bytes are
  materialised into the interpreter's `Specials` once per command and read back
  from there, instead of being *written into the variable table* — a name lookup
  and a copy per command, on the hot path of every loop.
* Every variable read tried nameref resolution first, which copies the name and
  looks it up a second time; `Vars::plain` answers the ordinary case (not a
  nameref) from the cell it already found. `$-` and `SHELLOPTS`/`BASHOPTS` are
  compared against a cached `Opts` value (a memcmp of a `bool` bundle) rather
  than a per-command walk of the option table.
* `Vars` keeps a one-entry cache of the global index its last lookup hit,
  validated by name. A 20k loop reads the same handful of names, and every miss
  walked the whole table comparing names: that walk was the largest single line
  in the profile.

Function calls are the one case still behind the 2026-10-28 build (38.7 ms now,
33.1 ms then, 31.4 ms in bash). The remaining cost is the same table walk for
the *frame* locals a call pushes and pops; the global cache does not cover them.

## What this says

* **Startup was not the problem.** A one-shot `baash -c ':'` costs 0.40 ms in
  release, slightly *faster* than bash: a small Rust binary with no rc file and
  no linker-loaded baggage. Interactive latency is dominated by the editor and
  the terminal, not by process start.
* **The tree-walking evaluator costs 1–2× bash per interpreted statement in
  release**, and is *faster* where a process is spawned (posix_spawn instead of
  fork). That is the shape this design expects: interpretation is not the
  bottleneck, which is why the bytecode VM (`DESIGN.md` §5) is deferred
  (`docs/direction.md` §2) rather than built alongside.
* **Debug is 14–25× slower than bash on interpreted work.** Nothing about the
  architecture changed between the two columns; it is entirely optimiser
  absence (no inlining, bounds checks, overflow checks, unoptimised
  `Vec<u8>` handling). Anyone using the shell interactively from `cargo run` is
  using the debug column, which is why it feels slow.
* The largest remaining interpreter gap is `${s//d/e}` (global replacement,
  2.1×) — it allocates per iteration more than the single-pattern forms.

## Practical consequences

* Build with `--release` for interactive use, and for any timing comparison.
* If `cargo run` should also feel usable, `[profile.dev] opt-level = 1` plus
  `[profile.dev.package."*"] opt-level = 2` is the usual compromise; it costs
  compile time. Not applied yet — decide when the interactive work settles.

## Method

`scripts/bench.py` — `python3 scripts/bench.py`, optionally
`--shell target/debug/baash --bash /bin/bash --reps 5 --filter expansion --json`.
It prefers a release build over a debug one and honours `CARGO_TARGET_DIR`, and
it says so on stderr when the binary it found looks like a debug build.

Each case is a `-c` script, run 3–5 times, minimum wall time kept (min is the
least noisy estimator for CPU-bound work); the startup case runs 100 spawns
inside one `/bin/sh` loop and divides, because one Python `subprocess.run` costs
more than either shell's startup. `/bin/true` does not exist on NixOS, so the
fork+exec case uses `env true`.

The release binary scores the same conformance as the debug one
(1986/2787 total, 1931 strict), so the optimiser is not hiding a behavioural
difference.
