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

### 2026-11-12: the carapace completion tables

`crates/baash-complete-data` carries command, option and description tables
generated from carapace-bin 1.8.1 by `scripts/carapace-data.py`, compiled in so
completion needs no process and no JSON parser. 117 commands, 27,896 options,
3.6 MB of generated Rust. `x86_64-linux`, release (`lto = "thin"`):

| | bytes |
|---|---:|
| `baash` with an empty table | 26,705,312 |
| `baash` with all 916 command tables | 41,359,768 (+14.0 MiB) |

The cost is dominated by a few large tools: bazel is 704 KB of carapace JSON,
git 439 KB, kubectl 194 KB, uv 187 KB. The `carapace` feature is **opt-in** in
`baash` (`--features carapace`), so the default build stays at 26.7 MB and the
tables are a deliberate 14 MiB. `just carapace-data` regenerates them from the manifest
(`crates/baash-complete-data/commands.txt`). The tables are static — a binary
search plus a walk over the typed words — and Tab with them costs 0.7 ms, the
first row of the runtime measurement below.

### 2026-11-12: runtime carapace lookups at Tab

When a `carapace` binary is on the shell's `PATH`, Tab asks it (`carapace
<command> export …`) instead of using the static tables. Debug build,
`x86_64-linux`, `LC_ALL=C`, `TERM=dumb`, n=15 per row, in a git worktree, Tab
sent through a pty and the first repaint containing the answer timed:

| case | median | min | max |
|---|---:|---:|---:|
| `git ch<TAB>`, no carapace (static table) | 0.7 ms | 0.4 ms | 0.9 ms |
| `git ch<TAB>`, carapace 1.8.1 on `PATH` | 20.8 ms | 20.5 ms | 21.0 ms |
| `git checkout <TAB>`, carapace dynamic branches | 30.7 ms | 20.9 ms | 31.0 ms |

The carapace numbers are dominated by the binary itself, not the shell: the same
`carapace git export git ch` run directly from Python costs 13.1 ms median
(12.0–15.6 ms), and a dynamic branch list (`carapace git export git checkout ''`)
costs 24.0 ms median (17.9–45.1 ms). The extra milliseconds on the Tab rows are
the editor's 10 ms poll interval: the answer is collected asynchronously, so the
prompt stays live and a keystroke can abandon the lookup instead of waiting for
it. The dynamic row needs `git` on the shell's `PATH` too — carapace runs it —
and `carapace git export git checkout ''` in a directory that is not a
repository exits 0 with `messages` and no values, which falls through. The bound is 500 ms (`carapace_ext::TIMEOUT`), after which the child is
killed and the next source answers; a fake carapace that sleeps for ten seconds
falls through in the same walk.


The memo is bounded twice: 16 entries, and a 64 KiB limit on the pattern itself
(`regex::MAX_PATTERN`), so a runaway generated pattern is refused rather than
compiled and remembered. A one-entry cache would miss on every iteration of a
loop rotating two patterns; with 16 slots both stay compiled. That loop
(`^--(no-)?file` alternating with the tar ERE, 40 000 evaluations) costs 2.25 µs
per `=~` on the system engine — the average of the two cached costs, as it
should be — and 2.69 µs on MinRX. The one-entry version measured 16.9 µs
(system) and 4.6 µs (MinRX) on the same loop, which is what the other fifteen
slots are for.

MinRX's compile is one pass into a flat Thompson program, matched by an NFA
simulation (`docs/contracts.md`). Measured with an `LD_PRELOAD` counting
allocator, one compile of the tar pattern makes **27 C allocator calls** on
MinRX against **366** on glibc (glibc includes 202 reallocs of its growing node
sets), which is the bulk of the compile-time difference; in the `C` locale
MinRX also takes its byte path (`LC_CTYPE=C` → no `mbrtowc`, no wide
characters, no Unicode tables). Forcing `C.UTF-8` makes its class-heavy compile
1.64 → 8.26 µs, so a future `setlocale` call would change this picture.


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
because it links a large Rust dependency graph (the dependency tree's vtables
are most of `.data.rel.ro`). Our own initialisation is ~5%, so no amount of tidying it
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

What ships is therefore glibc with a version floor instead of a store path:
`.#portable` in the flake links the release binaries against glibc 2.17 through
zig, so a tarball runs on any distribution from 2012 on, and the allocator is
the one the gnu column above measures.

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
plus a selectable engine (the platform `regcomp`, or MinRX; `docs/contracts.md`)
was measured with the harness the shell itself uses: `-c` scripts of 20 000
evaluations, release build, minimum of 11 interleaved process runs minus the
same loop with `[[ -n $s ]]`, against bash 5.3.20 on the identical scripts.

The harness is `crates/baash-bench` (`just bench` builds the release shell and
runs it), and it prints both tables below in the markdown they are written in,
so a figure here and a figure there cannot drift apart. Measured on `7e0fbfd`,
32 cores, ns per `=~`:

| pattern | bash | system engine | MinRX |
|---|---:|---:|---:|
| `create` (literal) | 1.29 µs | 0.69 µs | 0.71 µs |
| tar's `_longopt` ERE (84 B) | 37.1 µs | 3.50 µs | 4.36 µs |
| `^(foo|foobar|baz)([0-9]+)?$` | 8.65 µs | 1.20 µs | 1.07 µs |
| `^--(no-)?file` | 3.69 µs | 0.96 µs | 0.69 µs |
| no-match `^[[:blank:]]*--([[:alnum:]-]+)$` | 7.26 µs | 0.65 µs | 0.68 µs |
| capture `^--([[:alnum:]-]+)=(.*)$` | 6.60 µs | 1.68 µs | 1.47 µs |

Every shape but `no-match` gets a subject it actually matches, so the match —
and, for the capture shape, the group filling — is inside the timed work.

Neither engine wins the set. glibc is **20%** quicker on the fat tar ERE, and
MinRX is quicker on the three shapes a completion hits most: `^--(no-)?file` by
28%, the capture pattern by 13%, the alternation by 11%. The remaining two rows
are a few percent apart. Both engines are 8–11× faster than bash on the tar
ERE, which is the pattern bash's own tar completion runs.

The engine table explains the split, and the two tables agree row for row: once
the memo has removed the compile, the shape of the *match* is what the shell
pays, and glibc matches better on one pattern while MinRX matches better on
three. Where the time goes (`baash_sys::Regex`, µs per call, minimum of
7 × 20 000; the compile column moves 20–30% between runs because it is
allocation-bound, the match column is stable to a few percent):

| pattern | glibc compile / match | MinRX compile / match |
|---|---:|---:|
| `create` | 0.48 / 0.16 | 0.18 / 0.17 |
| tar ERE | 9.98 / 2.30 | 1.83 / 3.27 |
| `^(foo|foobar|baz)([0-9]+)?$` | 3.47 / 0.40 | 0.40 / 0.34 |
| `^--(no-)?file` | 1.70 / 0.33 | 0.26 / 0.16 |
| no-match pattern | 2.23 / 0.06 | 1.20 / 0.15 |
| capture pattern | 1.97 / 0.90 | 0.80 / 0.75 |

On the tar ERE the compile is 4.3× the match on glibc and 0.56× on MinRX, so
the memo was nearly all of glibc's cost and a minority of MinRX's; that is the
whole reason MinRX was worth keeping as a selectable engine rather than
retiring once the platform engine stopped compiling on every evaluation.

### 2026-11-14: the history picker (`Ctrl-R`) and `baashzf`

The picker's cost is one fuzzy rank per candidate per keystroke, and one
database read when it opens. `x86_64-linux`, release, `NO_COLOR=1`, driven
through a real pty by a script that timestamps the write and the frame that
answers it (so the number includes the terminal round trip):

| case | time |
|---|---:|
| `Ctrl-R` over a 21 050-row history database (20 000 rows read, de-duplicated, ranked) | 8.2–8.8 ms |
| a keystroke, 20 000 candidates | 0.15–0.46 ms |
| a keystroke, 200 000 candidates | 1.1–3.4 ms |
| `baashzf` over a list of 3 | 0.15–0.44 ms per keystroke |

The opening cost is the `SELECT id, text … ORDER BY id DESC LIMIT 20000` plus
one `String` per candidate for the display form; it is paid once, on the
keystroke that opens the picker, and it is the only place a picker waits for a
disk. Everything after that touches only what is in memory, which is what keeps
a keystroke sub-millisecond at the size a history actually reaches.

Two costs were measured and removed rather than reported: storing every match's
character positions allocated once per candidate per keystroke (47–184 ms at
200 000), and repainting on the read deadline instead of when the screen was
stale put a frame on the wire 40 times a second and delayed the first frame
after a key by a whole 25 ms deadline.

### 2026-11-16: the one write the shell waits for, Turso against SQLite

`Store::begin` runs once per submitted line, because the row id it returns *is*
the history number the line is shown with. It goes channel → worker thread →
INSERT → reply, so the measurement is the whole round trip, not the SQL. Same
harness on both engines: 50 warm-up inserts, then 2000 into a WAL-mode file
database, `x86_64-linux`, NixOS host disk:

| engine | build | p50 | p90 | p99 | max |
|---|---|---:|---:|---:|---:|
| Turso 0.8.3 | release | 12.0 µs | 15.9 µs | 26.4 µs | 165.1 µs |
| SQLite (sqlx 0.9) | release | 24.9 µs | 26.5 µs | 35.7 µs | 72.3 µs |
| Turso 0.8.3 | debug | 277.5 µs | 288.2 µs | 1503.6 µs | 1621.9 µs |
| SQLite (sqlx 0.9) | debug | 24.0 µs | 26.3 µs | 64.2 µs | 222.7 µs |

The median doubled in release and the tail halved; the debug build got eleven
times faster. That pattern is the explanation: under sqlx the round trip is
dominated by thread hand-offs — worker → `sqlx`'s connection thread → waker →
current-thread `tokio` → reply — which an optimiser cannot help, so debug and
release land on the same number. Turso's hand-rolled `block_on` needed one hop
fewer for the median, but its SQL work was compiled code, which is where its
debug build spent 1.5 ms at p99.

`cargo test -p baash-history --release --lib -- --ignored --nocapture` reproduces
it; the harness is the ignored `insert_round_trip` in `store.rs`.

### 2026-11-17: Windows startup is Windows' 12 ms, and 10 ms of library

The Linux figure at the top of this file (0.40 ms for `-c ':'`) says nothing
about Windows, where the clock starts with `CreateProcess`. Measured on the
`winvm` guest (Windows 11 26300.9711, 12 vCPU, no GPU) at `a9b3a57`, with the
shipping shape — `nix build .#windows`: opt-level 3, fat LTO, one codegen unit,
symbols stripped, 11,910,144 bytes — Defender's real-time protection on,
`wuauserv`/`TrustedInstaller` stopped and `MRT` finished, and `C:\baash` and
`C:\bench` excluded from scanning. Without those three conditions the same case
drifts by 30% between blocks, which is how a first attempt at this measured a
13.6 ms floor on a freshly booted guest and 19.7 ms ten minutes later.

The harness is one PowerShell process that starts each configuration with
`System.Diagnostics.Process`, waits for exit, and **alternates the two
configurations being compared sample by sample**, so a drifting VM cancels out
of the difference. The first four to six runs of each case are discarded:
Defender reads a newly copied binary once, and the virtio disk has not cached a
page of it.

```powershell
$psi = [Diagnostics.ProcessStartInfo]::new($exe); $psi.Arguments = '-c exit'
$psi.UseShellExecute = $false; $psi.CreateNoWindow = $true
$sw = [Diagnostics.Stopwatch]::StartNew(); $p = [Diagnostics.Process]::Start($psi)
$p.WaitForExit(); $sw.Stop()
```

| shape | median | min |
|---|---:|---:|
| `cmd /c exit` — what spawning *any* process costs here | 14.69 | 13.99 |
| `hostname.exe`, 40 KB, copied into `C:\bench` | 15.87 | 15.09 |
| `baash.exe --version` — image loaded, no shell built | 12.18 | 11.60 |
| `baash.exe -c exit` | 12.33 | 11.62 |
| `baash.exe -i --rcfile <empty> -c exit` | 12.47 | 11.96 |
| `baash.exe -i -c exit` with `BAASH_NO_BASH_COMPLETION=1` | 13.61 | 12.11 |
| `baash.exe -i -c exit` — the default interactive start | 22.42 | 21.40 |

n = 50 per row, warm, milliseconds.

Defender is not part of that number, and it was measured rather than assumed.
Re-running the same cases with the exclusions removed — real-time protection on,
the binary in a scanned directory — moves nothing:

| shape | with exclusions | without |
|---|---:|---:|
| `cmd /c exit` | 14.49 | 14.71 |
| `baash.exe -c exit` | 12.57 | 12.45 |
| `baash.exe -i -c exit` | 23.07 | 22.84 |

A binary that has been seen once is not rescanned on launch. What Defender does
charge for is *writing* one: a newly copied `baash.exe` takes 117–178 ms on its
first execution, and the same 117–178 ms for an identical copy in an excluded
directory, so that is the file being read off the virtio disk once, not a scan —
the Linux section's "a freshly copied exe costs another few hundred
milliseconds on its own first execution, at any design". The scattered 12 MB
copies this work made did keep `MsMpEng` busy in the background, which is what
pushed one early batch's median from 12.4 to 31 ms; that is interference with
the *measurement*, which is why every case discards its first runs and why the
two configurations are alternated instead of run in blocks.

Three things fall out of it.

**A Windows `baash` is not slow to start; it is faster than `cmd`.** The
non-interactive shell — argument parsing, `Interp::new`, `import_environment`,
`detect_locale`, the regex-engine choice, the `$PWD`/`$HOME` respelling, parsing
and running `exit` — costs **0.2 ms** over `--version` (12.33 against 12.18).
The other 12 ms is `CreateProcess` and the loader, and a 40 KB `hostname.exe`
pays the same 12 ms and ends up 3.5 ms *slower* than the 11.9 MB `baash.exe`, so
image size buys nothing here either. There is nothing to optimise in `baash -c`:
that number belongs to Windows.

**Everything the interactive shell costs above it is the bash-completion library
being interpreted.** `-i --rcfile <empty>` is the same number as `-c`, so the
merely-interactive path is free; `BAASH_NO_BASH_COMPLETION=1`, which still mounts
the tree and still runs the built-in `config.sh`, is 13.6 ms, so neither the
1106-file in-memory mount (~0.1 ms) nor `config.sh` (~1 ms) is the cost.
Sourcing `bash_completion` is **9.5 ms** (+9.50 min, +9.92 median, measured as
`. /__baash__/bash-completion/bash_completion` against an empty `--rcfile`). It
splits into **3.0 ms of parsing** — the same file wrapped in an uncalled
function, so nothing executes — and **6.5 ms executing** ~3800 lines of
top-level function definitions plus the 506-line `startup-core` file that its
own init sources.

**That is the Linux story, a fifth larger.** The section above measures 8 ms for
the same library. Windows is not paying for a port here: both platforms pay for
sourcing 139 KB of shell, which is what `bash` does too through a
distribution's `/etc/bash_completion`.

Ruled out by measurement rather than by reading:

* per-command overhead — 350 trivial commands cost 0.33 ms and 20 000 cost
  18.3 ms, so ~1 µs each and linear in the count; 96 function definitions cost
  0.37 ms;
* anything superlinear in state — a 700-entry associative array plus 250
  defined functions leave the cost of 3000 unrelated commands unchanged
  (2.89 ms against 2.94 ms without them), and 700 associative inserts cost
  1.26 ms, so inserts are ~1.8 µs and the array is not copied per command;
* `shopt` (6 µs each) and `complete` (30 µs each) at startup;
* stat'ing the POSIX paths `config.sh` probes (`/usr/share/bash-completion/…`
  do not exist on Windows): 10–20 µs each, 0.2–0.4 ms for twenty, so the path
  translation is not doing anything expensive;
* the block that a prefix bisection made look hot — `_comp__init_install_xspec`
  and its `!*.@(zip|…)` table — is 0.49 ms run on its own, and its *parse* costs
  nothing in any context; the 3.5 ms the prefixes suggested does not survive
  being compared with its parse-only twin, so the prefix numbers above were
  attribution error, not a cost.

The remaining lever is interpreter throughput, not a startup change: 6.5 ms for
a few thousand commands is the tree-walking evaluator doing what the sections
above measure it doing, and the deferred bytecode VM (`DESIGN.md` §5) is the
only thing that would move it. A shortcut that skips the source — deferring it
to the first `Tab`, say — would leave `_filedir` and `_init_completion`
undefined at the prompt, which is a behaviour change, not an optimisation.

The one piece of that 6.5 ms with a named cause is the largest single block,
the 700-entry `_comp_xspecs` table the library fills at startup:
`VarValue::Assoc` is a `Vec<(Vec<u8>, Vec<u8>)>`
(`crates/baash-runtime/src/vars.rs`), so an insert scans for the key and the
fill is O(n²) — 1.26 ms of the 9.5, and every later lookup is a linear walk
too. A hash table in place of the `Vec` would take that to ~0.1 ms, and would
matter far more to a script with a large associative array than it does here;
it is a separate change, because iteration order is observable in `declare -p`
and `${!a[@]}`, so it is not a swap to make while measuring startup.

Not measured: **time to the first prompt in a real console.** `ssh -tt` does
give a ConPTY, and the REPL paints its prompt through it (`win@WINVM C:~ $`,
with `OSC 0` titling the window and `OSC 133` marks around the prompt), but the
round trip's own ~300 ms swamps a 10 ms difference. The `-i -c` rows are the
reliable ones; they cover the mount and both startup files, and leave out the
history database open, the editor and the completion engine, which only the REPL
path pays for.

## The test suite: 37 s to 5 s

`cargo test` has to hold `RUST_TEST_THREADS=1` (`.cargo/config.toml`): `$( … )`
captures output by redirecting descriptor 1, which is a property of the process,
so two capture tests at once read each other's bytes. That serialises 1697
cases.

`cargo nextest run` gives every case its own *process*, so the descriptor
belongs to nobody else and the same cases run at once. Warm, same tree, 32
cores, debug profile:

| command | cases | wall |
| --- | ---: | ---: |
| `cargo test --workspace --all-features` | 1697 (+4 ignored) | **36.6 s** |
| `cargo nextest run --workspace --all-features` | 1692 (+4 skipped) | **5.1 s** |
| `cargo test --workspace --all-features --doc` | 5 | 1.5 s |

Wall time is not the only gain: a failure names one case and carries only that
process's output, and `fail-fast = false` reports every failure in one run
instead of stopping at the first.

The 5 doctests are the one thing nextest does not run, so `just test` and both
workflows run `cargo test … --doc` beside it. `threads-required = 2` on the
`interactive_*` binaries (`.config/nextest.toml`) stops the pty cases taking
every slot: each one is a child process, a pty and a temporary home, and
sixteen of those at once spend the machine on context switches.

## What this says

* **Startup was not the problem.** A one-shot `baash -c ':'` costs 0.40 ms in
  release, slightly *faster* than bash: a small Rust binary with no rc file and
  no linker-loaded baggage. Interactive latency is dominated by the editor and
  the terminal, not by process start. On Windows the same conclusion holds one
  order of magnitude up — 12.3 ms, which is `CreateProcess` and the loader, with
  the shell's own share at 0.2 ms (the 2026-11-17 section) — and what an
  interactive start adds there is the bash-completion library, not the port.
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
