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

### 2026-11-01: the specials stop being written per command

Re-measured at `05fc4bf` plus the two changes below, same machine, same method.
`$SECONDS`, `$RANDOM` and `$-` were being *materialised into the variable table*
after every command; they are dynamic variables now, kept in the interpreter's
`Specials` and read back by `Vars::get` (and `IntVars::get` for arithmetic), and
`$-` is rebuilt only when the option fingerprint changes. `SHELLOPTS`/`BASHOPTS`
stopped being rebuilt per command in `05fc4bf`.

| case | bash | baash release |
|---|---:|---:|
| startup, per spawn | 0.55 ms | 0.66 ms (1.2×) |
| arithmetic loop, 20k `((i++))` | 9.6 ms | 15.5 ms (1.6×) |
| `${s#*c}`, 20k | 23.5 ms | 30.8 ms (1.3×) |
| `${s//d/e}`, 20k | 25.8 ms | 27.5 ms (1.1×) |
| function call, 20k | 32.4 ms | 52.3 ms (1.6×) |
| `case` dispatch, 20k | 15.2 ms | 24.4 ms (1.6×) |
| builtin `echo`, 20k | 21.6 ms | 29.6 ms (1.4×) |
| fork+exec, 200 (`env true`) | 189 ms | 172 ms (0.9×) |
| pipeline, 200 | 125 ms | 95.9 ms (0.8×) |
| command substitution, 1000 | 168 ms | 8.0 ms (0.05×) |
| subshell, 1000 (`( : )`) | 144 ms | 4.6 ms (0.03×) |
| subshell 200, 20k-element array | 49.0 ms | 67.4 ms (1.4×) |

Two things this table says that the 2026-10-28 one could not:

* **Interpretation is back inside ~1–1.6× bash** and every derived-string write
  is off the per-command path. The regression that prompted this measurement
  (`((i++))` 20k at 40 ms, bisected to the commit that made `SHELLOPTS`
  derived-and-refreshed-per-command) is gone: 40 → 25.7 → 15.5 ms.
* **The process-heavy column is a different design, not a faster loop.** Command
  substitution and subshells no longer `fork`, so they are 20–30× bash rather
  than 0.7×; `docs/direction.md` §2 is why. The debug column above is stale: it
  predates that pivot, so its ratios are not comparable to these.

Companion numbers: conformance 2344 (baseline 2344, +0) on the same binary.

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
