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

### 2026-11-08: the bash-completion library inside the binary

The library is compiled into the binary (feature `embedded-bash-completion`,
`crates/baash/src/bash_completion.rs`) and mounted in memory, so no part of it
touches the disk. `x86_64-linux`, release, the tree nixpkgs pins (1106 paths,
2.97 MB if it were written out):

| | bytes / time |
|---|---:|
| `baash` without the tree | 25,868,864 |
| `baash` with the tree | 27,259,280 (+1.39 MB) |
| mount, at startup | below the 1 ms resolution |
| first interactive start | 9 ms |
| every later interactive start | 9 ms |
| `--norc` (no startup file, no mount) | 1 ms |

The 8 ms between `--norc` and a normal start is sourcing the library, which is
what it cost before the mount existed; building the table and mounting it is
not measurable (with `BAASH_NO_BASH_COMPLETION=1` — mount, no source — a start
is 1 ms with the tree and 1 ms without). The binary grows by 1.39 MB rather
than 2.97 MB because nixpkgs' 576 version aliases (`gcc-5` → `gcc`,
`python3.13` → `python3`) are symlinks to their targets' bytes and the linker
merges the identical constants.

The first version of this feature unpacked the tree into the cache directory
instead. Measured on the Windows 11 guest, best of five runs each, with the
cache removed between runs:

| | first start | later starts |
|---|---:|---:|
| unpacking the tree into the cache | 302 ms | 30 ms |
| mounting it in memory | 16 ms | 16 ms |

The first start is the one a user meets after installing, and it was dominated
by writing 1106 files for Defender to read; the mount has no first start. (A
*freshly copied* 21 MB `baash.exe` costs another few hundred milliseconds on
its own first execution, at any design, because the operating system reads the
new file once — that is not the shell.)

### 2026-11-01: dynamic specials, and two variable-table fast paths

Re-measured at the commits that made `$SECONDS`/`$RANDOM` lazy and added the
lookup fast paths below. Same machine, same method, best of 3.

| case | bash | baash release |
|---|---:|---:|
| startup, per spawn | 0.52 ms | 0.69 ms (1.3×) |
| arithmetic loop, 20k `((i++))` | 9.4 ms | 6.9 ms (**0.7×**) |
| `${s#*c}`, 20k | 23.4 ms | 20.2 ms (**0.9×**) |
| `${s//d/e}`, 20k | 26.4 ms | 16.4 ms (**0.6×**) |
| function call, 20k | 32.0 ms | 32.6 ms (1.0×) |
| `case` dispatch, 20k | 15.3 ms | 14.1 ms (**0.9×**) |
| builtin `echo`, 20k | 21.5 ms | 21.0 ms (**1.0×**) |
| fork+exec, 200 (`env true`) | 186 ms | 169 ms (0.9×) |
| pipeline, 200 | 124 ms | 92.2 ms (0.7×) |
| command substitution, 1000 | 176 ms | 7.6 ms (0.04×) |
| subshell, 1000 (`( : )`) | 145 ms | 4.3 ms (0.03×) |
| subshell 200, 20k-element array | 50.6 ms | 5.2 ms (**0.1×**) |

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

The last row was 65.5 ms and is now 5.2 ms, because an indexed array is
*shared* until written: a subshell is a clone of the shell state, and cloning a
20k-element `BTreeMap` two hundred times was the whole cost. `IndexedArray` is
an `Arc` now and the first write copies it (`Arc::make_mut`), so
`arr=( $(seq 20000) ); while …; do ( : ); done` stopped copying 4M nodes while
`( arr[0]=x )` still pays for its own copy and cannot touch the parent's array.

### Static musl: startup yes, throughput no

Startup is dominated by the dynamic loader. callgrind on `baash -c ':'` puts
**78%** of the run in `_dl_relocate_object_no_relro` and another ~9% in symbol
lookup: the binary has **43,641 dynamic relocations** against bash's 1,563,
because it links a large Rust dependency graph (turso's vtables are most of
`.data.rel.ro`). Our own initialisation is ~5%, so no amount of tidying it
would be felt. Packing the relocations (`-z pack-relative-relocs`) shrinks the
section 1 MB → 17 KB and changes nothing, because the loader's work is per
relocation, not per byte.

A static musl build removes the loader entirely (built with the rustup musl
target, `zig cc` for the C dependencies, and `-nostdlib` at link time so
rustc's own startup objects are the only ones):

| | gnu + glibc | musl | musl + mimalloc | bash |
|---|---:|---:|---:|---:|
| startup, per spawn | 0.53 ms | **0.36 ms** | 0.43 ms | 0.40 ms |
| arith loop 20k | **6.8 ms** | 17.1 ms | 12.8 ms | 9.5 ms |
| `${s#*c}` 20k | **19.7 ms** | 41.3 ms | 31.1 ms | 24.0 ms |
| function call 20k | **32.6 ms** | 70.1 ms | 52.1 ms | 33.7 ms |
| builtin `echo` 20k | **20.4 ms** | 50.9 ms | 35.1 ms | 21.5 ms |
| subshell + 20k array | 5.2 ms | 6.9 ms | **4.5 ms** | 51.8 ms |

So musl buys 0.17 ms of process startup and costs 10–40 ms per second of
typing: musl's allocator is ~2.3× slower on the small-vector traffic every
interpreted statement generates, and mimalloc claws back only part of it (and
pays its own initialisation, so startup lands between the two). **glibc stays
the default**; the musl numbers are here so the question does not get re-opened
from a cold start.

The variant that would win both is a *static glibc* build (no loader, glibc's
allocator); it needs a static glibc toolchain at build time, which is a Nix
`pkgsStatic` decision rather than a flag.

### Allocator: mimalloc on Windows

Measured both ways on Linux, mimalloc trades one kind of work for another:
startup 0.66 → 0.80 ms, small-string loops 5–8% slower, `$( )`/subshells ~18%
faster and the 20k-array case 2.1× faster — because that case was 4M
allocations and mimalloc is better at them. With the array shared, the
allocation pressure that motivated it is gone, so Linux keeps the system
allocator.

Windows is the exception: `HeapAlloc` is the slow part of a process that
allocates small byte vectors constantly, so Windows builds use mimalloc
unconditionally (`[target.'cfg(windows)'.dependencies]` in
`crates/baash/Cargo.toml`, and a `#[cfg(windows)]` `#[global_allocator]`). It
needs one build flag: mimalloc's `options.c` prints its build date with
`__DATE__`, which Nix's cc wrapper rejects as unreproducible, so the Windows
derivation relaxes `-Wdate-time` for that C dependency only.

Function calls started this series at 52.5 ms (1.5× the 2026-10-28 build) and
are now 32.6 ms against bash's 32.0 — parity, and better than that build's 33.1.
What was left after the variable-table fast paths was the per-call rebuild of
`FUNCNAME`/`BASH_SOURCE`/`BASH_LINENO`: `declare_shape` + `clear_array` + one
`set_indexed` per element, for three arrays, three times per command. It is now
one `Vars::replace_indexed` per array, and the function's definition file is
stored on the `Function` instead of being copied per call.

An ablation that stubs the rebuild out entirely measures 24 ms (435 ns per
call), so that is the ceiling of the next step: answering the three arrays from
the call stack at read time instead of materialising them in the variable
table. It costs `declare -p FUNCNAME` (bash lists it; we would not), which is
why it is not in this commit.

### 2026-11-12: `[[ =~ ]]`, the pattern cache and the two engines

`=~` compiled the pattern on every evaluation, and compiling is what a loop
pays for. A 16-slot LRU memo in the interpreter (`baash_sys::regex::Cache`)
plus a selectable engine (the platform `regcomp`, or MinRX; `docs/contracts.md`) was
measured with the harness the shell itself uses: `-c` scripts of 20 000
evaluations, release build, minimum of 11 interleaved process runs minus the
same loop with `[[ -n $s ]]`, against bash 5.3.20 on the identical scripts.

| pattern | bash | system engine | MinRX |
|---|---:|---:|---:|
| `create` (literal) | 1.38 µs | 0.73 µs | 0.75 µs |
| tar's `_longopt` ERE (84 B) | 38.3 µs | **3.64 µs** | 4.57 µs |
| `^(foo|foobar|baz)([0-9]+)?$` | 4.44 µs | **0.48 µs** | 0.56 µs |
| `^--(no-)?file` | 1.76 µs | **0.48 µs** | 0.67 µs |
| no-match `^[[:blank:]]*--([[:alnum:]-]+)$` | 7.58 µs | 0.99 µs | **0.59 µs** |
| capture `^--([[:alnum:]-]+)=(.*)$` | 6.81 µs | 2.09 µs | **1.68 µs** |

Before the cache the same tar pattern cost 29.0 µs on the system engine and
7.1 µs on MinRX (same harness, cache removed), so the memo is an 8× win for
glibc and 1.5× for MinRX, and `=~` on that pattern went from roughly
bash-speed to 8–11× faster than bash.

The memo is bounded twice: 16 entries, and a 64 KiB limit on the pattern itself
(`regex::MAX_PATTERN`), so a runaway generated pattern is refused rather than
compiled and remembered. A one-entry cache would miss on every iteration of a
loop rotating two patterns, and 16 slots keep both compiled. Measured with the one-entry version
on the same harness, that loop cost 16.9 µs per `=~` on the system engine
against 4.6 µs on MinRX — the argument for keeping MinRX selectable rather than
deleting it, because glibc's `regcomp` is 8.7 µs on that pattern and MinRX's
`regncomp` 1.8 µs.

Where the time goes, engine-level (`baash_sys::Regex`, ns per call):

| pattern | glibc compile / match | MinRX compile / match |
|---|---:|---:|
| `create` | 281 / 64 | 167 / 90 |
| tar ERE | 8738 / 2329 | 1784 / 3348 |
| `^(foo|foobar|baz)([0-9]+)?$` | 2708 / 425 | 374 / 337 |

MinRX's compile is one pass into a flat Thompson program, matched by an NFA
simulation (`docs/contracts.md`). Measured with an `LD_PRELOAD` counting
allocator, one compile of the tar pattern makes **27 C allocator calls** on
MinRX against **366** on glibc (glibc includes 202 reallocs of its growing node
sets), which is the bulk of the compile-time difference; in the `C` locale
MinRX also takes its byte path (`LC_CTYPE=C` → no `mbrtowc`, no wide
characters, no Unicode tables). Forcing `C.UTF-8` makes its class-heavy compile
1.64 → 8.26 µs, so a future `setlocale` call would change this picture.

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
