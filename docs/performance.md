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

The one piece of that 6.5 ms with a named cause was the largest single block,
the `_comp_xspecs` table the library fills at startup: `VarValue::Assoc` was a
`Vec<(Vec<u8>, Vec<u8>)>` (`crates/baash-runtime/src/vars.rs`), so an insert
scanned for the key and the fill was O(n²) — 1.26 ms of the 9.5 for the
700-insert loop measured then, with every later lookup a linear walk too. The
hash index landed later the same day ("the associative array gets a hash
index", below). The shipped library turns out to fill 138 keys, so sourcing it
did not move measurably; the cost that *was* there is a per-key cost, and that
is what the new section measures.

Not measured: **time to the first prompt in a real console.** `ssh -tt` does
give a ConPTY, and the REPL paints its prompt through it (`win@WINVM C:~ $`,
with `OSC 0` titling the window and `OSC 133` marks around the prompt), but the
round trip's own ~300 ms swamps a 10 ms difference. The `-i -c` rows are the
reliable ones; they cover the mount and both startup files, and leave out the
history database open, the editor and the completion engine, which only the REPL
path pays for.

### 2026-11-17: `[` opened the working directory on every call

`[` is the most-run word in a shell script, and it was being treated as a
pattern: a lone `[` set the field's glob metacharacter, so `[ 1 -lt 2 ]` sent
its first word to pathname expansion, which read the current directory and
matched every entry against a bracket expression that could never close. `set
-f` made `[` as cheap as `test`, which is how it surfaced while measuring the
Windows start above; the cost is in every shell, not only Windows.

The rule is glibc's: a `[` opens a bracket expression only when a `]` follows
it, after an optional `!`/`^` and an optional `]` as the first member. The set
now lives in one function (`baash_lang::has_active_glob`), asked by the language
layer before it expands a field *and* by the expander's per-component check, and
a field that is not a pattern is left alone — which also fixed two policy bugs:
under `nullglob` the word `[` used to vanish, and under `failglob` the command
used to refuse to run, where bash keeps it and runs.

| 20 000 calls in one `while (( ))` loop, median of 7 | before | after |
|---|---:|---:|
| `[ 1 -lt 2 ]`, guest | 31.6 µs | **0.63 µs** |
| `test 1 -lt 2`, guest | 0.44 µs | 0.54 µs |
| `[[ 1 -lt 2 ]]`, guest | 0.02 µs | 0.06 µs |
| `[ 1 -lt 2 ]`, Linux, debug build | 13.0 µs | **4.5 µs** |

`[` now costs what `test` costs. The same binary pair measured in the same guest
session: 664.8 ms against 43.6 ms for the whole 20 000-iteration loop.

### 2026-11-17: the associative array gets a hash index

`VarValue::Assoc` was a `Vec<(Vec<u8>, Vec<u8>)>`: `a[k]=v` scanned for the key
and `${a[k]}` scanned again, so an *n*-key array cost O(n) per element and a
fill loop O(n²). The Windows section above put a number on it and left the
change for later; this is that change.

The order had to survive: `docs/contracts.md` promises insertion order, which
bash does not promise (its order is the hash table's). Five representations,
all order-preserving, 1024 keys of 6 bytes holding 7-byte values, whole
operation, minimum of 11 runs:

| representation | `a[k]=v` | `${a[k]}` hit | miss | `unset` | B/entry |
|---|---:|---:|---:|---:|---:|
| `Vec` scan (before) | 650 ns | 582 ns | 1.14 µs | 1.52 µs | 60 |
| `Vec` + `HashMap` (std) | 89 ns | 9.4 ns | 6.8 ns | 2.26 µs | 131 |
| `Vec` + `HashMap` (ahash) | 72 ns | 5.1 ns | 2.7 ns | 2.18 µs | 131 |
| `IndexMap` (std) | 57 ns | 12.1 ns | 8.2 ns | 0.77 µs | 128 |
| `IndexMap` + ahash | **49 ns** | 6.8 ns | 3.4 ns | **0.75 µs** | 128 |

`IndexMap` over `ahash` wins the write, the removal and the memory, and loses
only the hit, to the `Vec`+index pair (6.8 against 5.1 ns). The `Vec`+index
designs lose exactly where a shell spends its time: a new key costs an extra
allocation for the index's copy of it, and `unset` has to renumber positions
where `IndexMap` moves entries. Walking every pair is 0.4 µs for all five (one
pass either way), which is what `${a[@]}` and `declare -p` do.

The container is `baash_lang::AssocArray` and not a runtime type because
expansion reads arrays through a borrowed `ArrayValue`: a pair slice cannot
carry an index, so `${a[k]}` in the expansion path — where most element reads
happen — would have kept the scan. `indexmap` is already in the tree through
`sqlx` and `toml_edit`; `ahash` is new (`MIT OR Apache-2.0`, `deny.toml`).

Shell level, release build, 2000 keys in one script, minimum of 7 runs:

| script | bash 5.3.20 | before | after |
|---|---:|---:|---:|
| 2000 `a[k$i]=v$i` | 4.9 ms | 5.8 ms | 4.7 ms |
| + 2000 `${a[k$i]}` | 8.7 | 10.8 | 8.8 |
| + 2000 misses | 8.6 | 10.6 | 8.5 |
| + `${!a[@]}` over the keys | 6.6 | 6.7 | 5.8 |
| + 2000 `unset`, oldest key first | 9.4 | 12.8 | 12.3 |
| 16 keys, rebuilt 120 times | 5.1 | 4.9 | 4.9 |
| 4000 iterations, no array (what a run costs) | 7.3 | 8.2 | 8.4 |

Subtracting that no-array loop, a write to a 2000-key array was 0.85 µs and is
0.25 µs; a read was ~0.45 µs and is ~0.05. Writing and reading now cost what
bash's own table costs. Two things did *not* improve: `unset` is 4% slower,
because keeping the order means shifting the entries where bash's unordered
table swaps the last one into the hole — the price of the order contract, on
the operation a script does least — and a 16-key array is unchanged, because at
that size a hash costs what a scan of seven keys costs.

On the Windows guest, the same writes inside one process against the same loop
with a scalar (2000 iterations, 21 interleaved rounds, minimum): 2.24 ms before
and 0.61 ms after, 1.12 µs against 0.31 µs per key.

What this does not show is a startup win. The prediction above was ~1 ms off the
library run from the fill alone; the shipped `bash_completion` fills 138 keys,
not 700, so sourcing the whole library on Linux went 7.49 ms to 7.81 ms, which
is noise. The prediction was about per-insert cost, and that is what moved.

### 2026-11-17: where the CPU goes, sampled

Everything above is a wall-clock A/B. This is the other view: what the CPU was
doing, sampled. `perf record -e task-clock -F 4000 -g --call-graph fp` over the
release profile with `-Cforce-frame-pointers=yes` and debuginfo — the optimiser
left on, symbols added, in `/tmp/baash-perf-target` so the shipped build is not
touched. `task-clock` charges running time only, and on this host there is no
PMU (`cpu/cycles` opens as "No such file"), so instruction counts and
`perf stat` are not available: the clock and the stack are. Machine: Ryzen 9
9950X3D, 32 threads, Linux, perf 7.3.4, 4 kHz.

Self time by crate, percent of samples. `libc` is `malloc`/`free`/`memcmp`/
`memmove` — the allocator and the byte comparisons, not shell code:

| workload | `libc` | kernel | `lang` | `vm` | `runtime` | `syntax` | `pattern` | `alloc`/`core` |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| interpreter loop, 100k iterations | 39.3 | 0.3 | 22.6 | 19.7 | 12.8 | – | 0.8 | 4.2 |
| 20k `[[ $s =~ … ]]` | 45.8 | 0.3 | 14.6 | 17.9 | 13.0 | 3.3 | – | 4.3 |
| 20k `x=$(echo $n)` | 44.0 | 24.1 | 8.6 | 8.2 | 9.5 | 1.4 | – | 1.8 |
| `source bash_completion`, before | 12.4 | 3.9 | 3.9 | 56.6 | 2.0 | 16.1 | 2.8 | 2.2 |
| `source bash_completion`, after | 28.5 | 6.4 | 10.5 | 12.2 | 4.6 | 27.3 | 2.9 | 4.1 |

Five things fell out of it.

**`$LINENO` was quadratic, and sourcing the completion library paid for it.**
`set_line` is called for every node the evaluator reaches, and it used a cursor
over the source: a span that arrived backwards (a loop body, a function called
twice) rescanned the file from byte zero. That is a byte-by-byte `filter` over
100 KB of `bash_completion`, per jump — **53.6% of the CPU of sourcing it**.
`SourceFile` (`crates/baash-diag/src/lib.rs`) now indexes the newline offsets
on the first query — `line_at`, a `partition_point` — and `set_line` in
`crates/baash-vm/src/tree/mod.rs` reads the line instead of counting it.
Sources never change, so the scan happens once:

| case (minimum of 15, alternating runs) | before | after | |
| --- | ---: | ---: | ---: |
| `source bash_completion` ×15 | 91.1 ms | 42.2 ms | **−54%** |
| — per source | 6.07 ms | 2.81 ms | |
| interpreter loop, 100k iterations | 280 ms | 267 ms | −4.9% |
| 3-line file sourced 2000× | 8.55 ms | 8.55 ms | noise |
| `baash -c exit` | 0.40 ms | 0.40 ms | noise |

A small file pays the one scan it already paid on its first pass, so nothing
regresses; a large one stops paying it thousands of times.

Two bugs on the way there are worth recording, because both were invisible to
the unit tests and visible to the shell. The first attempt reported line *zero*
for a command on line one: the base a chunk is counted from is 1, not 0. The
`$LINENO` buffer is a cache of the number, and five places restore the number
after a scope, a function or a trap without touching the text — so an `ERR`
trap fired for `x=$(false)` printed the substitution's line 1 instead of line
2, where the command sits. Three conformance cases (`builtin-trap-err` and
`builtin-trap-bash`, −3 on the ratchet) caught it; the restores now go through
one `restore_line`, which writes both. The new cases
`lineno_follows_a_loop_body_and_a_function_called_twice` and the substitution
line in `the_err_trap_sees_the_failing_line_and_its_status` pin them.

**The variable table is a scan, and that is visible.** In a loop that
alternates four names, 6.0% of samples are in `Vars::lookup` and 9.9% in
`memcmp` comparing the names it walks past. `Vars` is a `Vec<(Vec<u8>, Var)>`
with a one-entry "last hit" cache (`vars.rs`, and the 2026-11-01 section above),
which covers the same variable read twice but not four read in turn. A two- or
four-entry cache would cover the loop; a hash index is the other answer, and the
same trade the associative array just took. Not changed.

**`$( … )` copies the shell.** `open_scope` clones `ShellState` — the global
table, every frame, every `Vec<u8>` key — once per command substitution: 5.5% in
the clone plus its allocations, and `sigismember`/`SignalSet::eq` another 7% for
the disposition save. 8 µs per substitution. That is the price of no `fork()`
done the simple way, and the way out is a persistent scope stack (a frame per
depth, not a clone), not a micro-optimisation. Not changed.

**A Tab press is mostly the completion backend, not the ranker.** With
`carapace` on `PATH`, 60.5% of the CPU of typing `ec<Tab>` and `echo
$PA<Tab>` in a loop is the `carapace` child, 31.8% is baash, 6.4% is the pty
driver. Of baash's share, the top entries are `baash_complete::rank::Ranker`'s
`nucleo` fuzzy matcher, a `RandomState` hash per candidate (`to_lowercase` and
`Utf8Chunks` per candidate beside it), and the allocator. `ahash` is already a
dependency and `DefaultHasher` is SipHash-1-3 for four-byte keys; the lowercase
fold is recomputed per keystroke for the same list.

**Startup has nothing left to win.** Of the CPU a one-shot `baash -c exit`
accrues (300 spawns), 2.4% is baash's own code: 47.6% is the kernel (page
faults, `execve`, `mmap`) and 47.3% is the dynamic loader (`do_lookup_x`,
`_dl_relocate_object_no_relro`, `strcmp`). The wall-clock section says the same
thing from 0.40 ms; this says it at instruction granularity.

What this does not show: samples are CPU time, so a blocked or waiting process
samples as nothing and terminal latency is invisible; the pty harness shows
what a keystroke costs the shell, not what it costs the terminal; the profiling
build carries frame pointers the shipped build does not, so a tail call can
attribute differently; and a 0.4 ms process is 1–2 samples, which is why the
startup row is an aggregate of 300 spawns.

### 2026-11-17: the variable table gets an index, and the hasher stops reading the OS

The sampled profile above ended with "the variable table is a scan": the global
scope was a `Vec<(Vec<u8>, Var)>` with a one-entry last-hit cache (the
2026-11-01 section), so any read that was not a repeat compared the name
against every entry before it. With a 71-variable environment that is 7.3% of
an interpreter loop in `memcmp` plus 6.2% in `Vars::lookup`.

The table is now an `IndexMap<Vec<u8>, Var, ahash::RandomState>`, the same pair
`AssocArray` uses: insertion order in the entries, O(1) lookup in the index.
The order the shell promises does not change — a new name appends, an update
keeps its place, `unset` shifts the rest — which is what `envp` shows a child
and what `set` lists. Frames keep their `Vec`: a frame holds a handful of
locals, and comparing two of them is cheaper than hashing one.

| indirect expansion (200 passes over the names) | before | after | |
| --- | ---: | ---: | ---: |
| 20 variables | 3.82 ms | 3.51 ms | −8.2% |
| 100 variables | 17.31 ms | 13.80 ms | −20.3% |
| 400 variables | 93.93 ms | 52.23 ms | −44.4% |
| 1000 variables | 364.14 ms | 129.61 ms | −64.4% |

The quadratic shape is gone, and it was the point: a scan is O(names) per read,
so a script that keeps its working set in variables — `${!name}` loops, a
generated table of a few hundred, a dev shell with 200 exported names — paid for
the table size on every one. A 71-variable loop that reads four names, the
profile's own workload, moves +0.4% (in the noise): at that size the scan was
cheap and the win is elsewhere.

What the hash costs is the copy. A subshell clones the variable table — that is
how `$( … )` gets its isolation without `fork()` — and `IndexMap`'s clone
copies the index as well as the entries: 20k command substitutions went from
157.9 ms to 161.8 ms (+2.4%, ~190 ns per clone). Frames, `env true`, and a 20k
`local` loop do not move.

That copy is the same one the profile above already names as the thing to
remove — `open_scope` clones `ShellState` once per substitution, and the way out
is a persistent scope stack, not a cheaper clone — so the index adds 190 ns to a
path that is scheduled to stop copying the table at all.

**The hasher was reading the OS on every start.** `ahash`'s default
`RandomState` mixes in a seed read from the operating system once per process,
which is the right default for a network service and 10 µs of a 0.53 ms
startup — 2% of the number this file's first section is about — for a defence
with nothing to defend: a flood needs keys an attacker chose, and anything that
can choose a variable name or an array subscript already runs inside this
shell. `DESIGN.md` §4.6 has the blunt version of that rule. Bash's own variable
hash is fixed too. `baash-lang`'s `hasher::seeded`
is now the one place the seed is decided, and both tables use it:

| case (minimum of 7, alternating, per spawn) | before | ahash default seed | fixed seed |
| --- | ---: | ---: | ---: |
| `baash -c exit` | 533.7 µs | 543.6 µs | 526.8 µs |
| `declare -A a; a[k]=1` | 544.8 µs | 543.6 µs | 539.7 µs |

So the table does not cost a startup: with the seed fixed it is a few
microseconds *faster* than the vector, which grew its buffer six times to hold
71 names. The associative array takes the same seed; its first array no longer
pays a `getrandom` either.

### 2026-11-17: a subshell stops copying the variable table

The section above left one number on the table: the table's index clones with
the table, so 20k command substitutions went from 157.9 to 161.8 ms. Copy on
write was the obvious answer and it does not work: sharing the snapshot is only
cheaper for a body that does nothing, and every real body writes — a command
writes `_` — so the first write copied the 71 keys anyway and paid for an `Arc`
besides. The measurement:

| case | eager snapshot | copy-on-write |
| --- | ---: | ---: |
| `( : )` ×20k | 102.6 ms | 111.6 ms |
| `x=$(echo $n)` ×20k | 162.4 ms | 168.6 ms |

So the scope now does what fork does without the copy: it **journals**. A write
records what the name held (or that it did not exist) and `close_scope` replays
the records backwards — O(writes) at open and close instead of O(names).

What the record has to carry to be exact: a statement that did not exist before
is removed; one that was removed is put back **at its old index**, not at the
end, because `env` order is the table's order and a subshell must not shuffle
its parent's; a frame the body pushed is truncated; a write to a frame local
(the enclosing function's `local`) restores that cell. Positional parameters,
`$0`, `$?`, `$$`, `$!` and the `getopt` cursor are a handful of bytes, so they
are snapshotted instead — a copy cheaper than a record per write. `ShellState`
beyond the store (options, traps, aliases, completion specs) is still cloned:
it measured under 1% of the same loop.

| case (minimum of 15, alternating runs) | before | after | |
| --- | ---: | ---: | ---: |
| `( : )` ×20k | 102.8 ms | 62.2 ms | **−40%** |
| `x=$(echo $n)` ×20k | 162.4 ms | 112.5 ms | −31% |
| `x=$(y=$n; echo $y)` ×20k | 173.7 ms | 122.0 ms | −30% |
| `( f )` with a `local` ×10k | 74.7 ms | 53.0 ms | −29% |
| `x=$(env true)` ×3k | 2624.4 ms | 2616.1 ms | — |
| `source bash_completion` ×15 | 41.8 ms | 42.0 ms | — |
| interpreter loop, 100k | 259.3 ms | 257.0 ms | −0.9% |
| `baash -c exit` | 0.41 ms | 0.41 ms | — |

A substitution that spawns (`$(env true)`) does not move: the process is 800 µs
of its 875, and the scope was never what it waited for.

The risk in a journal is a write path that forgets to record, which leaks state
into the parent silently. Two things guard it: `debug_assertions` builds copy
the store at open and compare the restored one (`Vars: PartialEq`), so a missed
path fails the test that exercises it; and the conformance ratchet runs the
debug build over its 2787 cases. It is the same 2357 (+0), and the debug check
in the ratchet's own loop costs nothing measurable (7.79 s against 7.74 s).

### 2026-11-17: the signal set stops asking libc about each signal

The section above left one item in a scope: the signal mask. With the variable
copy gone, `sigismember` was **10.7%** of the `( : )` loop, called from
`sync_traps` (7.1%), `close_scope` (2.0%) and `exec_simple` (1.6%).

The cause was the representation. `SignalSet` wrapped `sigset_t` and asked
libc for everything: `iter()` tested all 64 numbers with `sigismember`, and
`PartialEq` — which `sync_traps` runs to compare the wanted set with the
blocked one — compared two sets by iterating both, so one comparison could be
128 libc calls. `sync_traps` walks both sets on every change.

It is a `u64` now. Linux numbers signals 1 to 64, which is exactly a word:
membership is a shift, iteration a bit scan (`trailing_zeros`, clear the lowest
bit), equality one compare, and the empty set is `0`. The `sigset_t` is built
only where a syscall wants one — `sigprocmask`, `signalfd`, `posix_spawn` —
and `current()` still reads the mask through libc, because it has to; that is
once per mask change rather than once per signal examined.

| case (minimum of 15, alternating runs) | before | after | |
| --- | ---: | ---: | ---: |
| `( : )` ×20k | 63.1 ms | 52.5 ms | **−17%** |
| `x=$(echo $n)` ×20k | 111.7 ms | 103.5 ms | −7.3% |
| `x=$(env true)` ×3k | 2611.6 ms | 2615.8 ms | — |
| interpreter loop, 100k | 256.9 ms | 257.2 ms | — |
| `env true` ×200 | 170.3 ms | 171.1 ms | — |
| `baash -c exit` | 0.39 ms | 0.39 ms | — |

`sigismember` is gone from the sampled profile of that loop; what is left there
is the command itself (allocator, expansion, one `sigprocmask` per scope).
Cumulatively, against the snapshot this page started from, `( : )` ×20k is
102.8 → 52.5 ms, −49%.

The profile now points at the next cost in a scope: `open_scope` calls
`getcwd` and `close_scope` calls `chdir` back to it on every `$( … )` and
`( … )`, whether or not the body changed directory — `link_path_walk` and
`__getcwd` are a few percent of the loop, and a path walk is not free. A scope
only needs the restore when something inside it moved the process, so the fix
is to record that it did (the `cd`/`pushd` path is one place) and skip both
calls otherwise. Not done here.

### 2026-11-17: a command word is not copied to look for a tilde

`expand_one` runs stage 1 (tilde expansion) for every word of every command,
and the owned form started with `let mut word = word.clone()`: a `Vec<Part>`
plus a copy of every literal's bytes, for words that mostly have no `~` in them
at all. In an interpreter loop that clone was 4.6% of the profile on its own,
and the literal copies were part of the 24% the allocator had.

Stage 1 now looks first and borrows when there is nothing to expand.
`expand_tilde_cow` returns `Cow<Word>`; `has_tilde` walks the parts it would
have to rewrite — the `Tilde` parts, an unquoted literal starting with `~` (a
command word), an unquoted literal containing `~` (an assignment, where `:` is
a separator too), and the replacement and pattern words inside `${…}` — and
says "no" only when it is certain. The check itself is 2.5% of the same loop.

| case (minimum of 15, alternating runs) | before | after | |
| --- | ---: | ---: | ---: |
| interpreter loop, 100k | 259.2 ms | 226.0 ms | **−12.8%** |
| `expansion 20k ${s#*c}` | 18.80 ms | 17.51 ms | −6.9% |
| `arith loop 20k` | 7.03 ms | 6.96 ms | — |
| `subshell 20k ( : )` | 31.03 ms | 30.74 ms | — |
| `cmd subst 20k $(echo i)` | 80.34 ms | 79.51 ms | — |
| `baash -c exit` | 528.6 µs | 533.9 µs | — |
| `source bash_completion` ×15 | unchanged | unchanged | — |

The predicate is an over-approximation on purpose: saying "no" when the
expander would have rewritten something would leave a `~` unexpanded, and
saying "yes" only costs the copy that was there before. The direction that
matters is checked against the expander itself —
`the_borrowing_path_agrees_with_the_owned_expander` runs a corpus of quoting
and `${…}` shapes through both and fails if a borrowed word is one the owned
path changes — and the ratchet is the same 2357 (+0), which is what says the
`~`/`~user`/`${x#~}`/`PATH=~:~/bin` behaviours are intact.

The same waste was one stage earlier: `expand_word_with` called
`expand_braces`, which handed back `vec![word.clone()]` when the word held no
`{` — another copy of the parts and their literals for every command word. It
returns `Cow<[Word]>` now (`expand_braces_cow`), borrowing the one word when
there is no brace group to expand:

| case (minimum of 21, alternating runs) | before | after | |
| --- | ---: | ---: | ---: |
| interpreter loop, 100k | 227.0 ms | 212.9 ms | −6.2% |
| `builtin echo 20k` | 19.59 ms | 18.55 ms | −5.3% |
| `: plain` ×20k | 17.73 ms | 17.28 ms | −2.5% |
| `: p{a,b}q` ×20k (real braces) | 25.86 ms | 25.92 ms | — |

One measurement trap this section had to get around: the two binaries must come
from the same build environment. Building one side through the ambient
toolchain and the other inside `nix develop` moved `.text` by 455 KB (the C
dependencies are compiled by whichever `cc` is on `PATH`) and the startup
number by 4% — with no source change. Build both sides with the same
`CARGO_TARGET_DIR` and flags, or the comparison measures the C compiler.

### 2026-11-17: the option table stops being compared as bytes

After the two borrowing changes above, `memcmp` was still 9% of an interpreter
loop, and 2% of that was the `SHELLOPTS`/`BASHOPTS`/`$-` refresh:
`refresh_option_derived` runs before every command, and the guard for "did an
option change" was a copy of the option bundle compared against the live one.
`Opts` is 73 `bool`s, so that is 73 bytes of `memcmp` — and it happened twice per
command, because `$-` kept a second copy of the same bundle beside its letters.

`Opts` carries a revision now (`rev: u64`), which `Opts::set_flag` bumps only
when a flag actually changes, and the shell caches one `Option<u64>` instead of
two bundles: one 8-byte comparison per command, with the two lists and `$-`
rebuilt together from it. The number comes from one process-wide counter,
because a scope clones the bundle, bumps the clone, and its close reinstates the
parent's number — a per-bundle `+ 1` repeats, which would make the guarantee depend
on the caller refreshing the derived variables before every change (it happens to
today) instead of on the type.

| case, wall clock (shared-path ABBA, 16 runs each) | before | after | |
| --- | ---: | ---: | ---: |
| `colon loop 20k` (`scripts/bench.py`) | 13.57 ms | 12.97 ms | −4.5% |
| `: plain` loop, 100k commands | 65.86 ms | 61.80 ms | **−6.2%** |
| `[ 1 -lt 2 ]` loop, 100k | 119.03 ms | 117.94 ms | −0.9% |
| assignment+expansion, 100k | 220.15 ms | 208.88 ms | −5.1% |
| `set -o nounset; set +o nounset`, 20k | 55.85 ms | 50.58 ms | −9.4% |
| `x=$SHELLOPTS`, 20k | 11.78 ms | 10.73 ms | −8.9% |
| — control: two byte-identical copies of one build | 65.82 ms | 65.47 ms | −0.5% |

A change this size is close to what a wall clock on this host can resolve (see
the control, and the method note below), so the same scripts were counted as
well. `valgrind --tool=callgrind` counts retired instructions, which does not
depend on frequency, thermal state, code layout or a neighbour, and it agrees on
the direction in every case — the work removed is 1–6%:

| case, retired instructions | before | after | |
| --- | ---: | ---: | ---: |
| `: plain` loop, 10k commands | 143,708,686 | 140,496,818 | −2.2% |
| `[ 1 -lt 2 ]` loop, 3k | 74,909,048 | 73,944,045 | −1.3% |
| assignment+expansion, 3k | 134,096,900 | 132,168,674 | −1.4% |
| `set -o nounset; set +o nounset`, 2k | 111,214,664 | 104,074,437 | −6.4% |
| `x=$SHELLOPTS`, 3k | 40,726,185 | 39,761,308 | −2.4% |

The wall clock moves further than the counts in most rows, which is what two
compares that went through a `memcmp` call across two cache lines and a branch to
the rebuild path cost beyond their instructions. The `[ 1 -lt 2 ]` row is the
exception and sits at the floor — where a loop spends its time in the builtin, the
count is the only measure that resolves the difference. The option-churn case
moves most in both, because every `set` also rebuilt `$-` from the second copy of
the bundle.

**The A/B itself was the trap.** Two *byte-identical* copies of one build
measured 3.5% apart, and with plain A,B,A,B alternation the second run of every
pair was 1.5–3.5% slower — both larger than the effect above. The file a build
lives in matters (one copy, written while the filesystem was busy, stayed slower
for every run), and so does position. `scripts/ab.py` now copies each side over
one shared path before running it, discards that copy's first run, and runs each
block A,B,B,A; the same control reads −0.5% through it. The earlier tables on this
page used plain alternation, so they were measured with a bias *against* the
`after` side — if anything they understate their wins.

**A direct field write is a stale `$SHELLOPTS`.** The flags stay `pub`, and the
revision is what makes the rebuild cheap, so a write that bypasses `set_flag`
changes an option without moving the revision: the three derived values go stale
and nothing fails to compile. The seven writes outside tests that existed were
converted, and so were the ones in tests. A `debug_assertions` build now compares
a two-word mask of the flags the revision was rendered from with the live table
(`Opts::mask`, with a `const` assertion that the table still fits in two words),
so the test suite and the conformance ratchet fail on the next one; release builds
pay nothing for it.

### 2026-11-17: the per-command allocations

With the option bundle no longer compared and expansion borrowing instead of
copying, 18% of an interpreter loop was `malloc`/`free`/`memmove`. `callgrind
--tree=calling` answers *who* allocates, which is the only view in which this is
a list of culprits rather than a percentage: `while (( i++ )); do :; done` made
**280,739 heap allocations for 10,000 iterations** — 20,000 commands, fourteen
each — and the per-caller counts named them:

| site | per 10k iterations | what it was |
| --- | ---: | --- |
| `BTreeMap::insert` + `SpecToString` | 30,002 + 30,002 | `$PIPESTATUS` rebuilt with `clear()` and `to_string()` after every command |
| `Vars::resolve_chain` | 20,017 | `name.to_vec()` before knowing whether a nameref was involved |
| `IntVars::get_text` | 20,001 | a clone of the variable's bytes for the "is this a number" check |
| `arith::expand` | 20,001 | a `Vec` for an expression that splices nothing |
| `IntVars::set` + `SpecToString` | 10,000 + 10,000 | `to_string()`, then another copy into the store |
| `finish_grow` | 70,028 | `Vec` growth, across argv, expansion output and the rest |

Three commits answered them — `vm:` for the `PIPESTATUS` slots, `runtime:` for the
borrowed name, `vm:` for the borrowed arithmetic text — each carrying its own
numbers; together:

| `while (( i++ )); do :; done`, 10k iterations | before | after | |
| --- | ---: | ---: | ---: |
| heap allocations | 280,739 | 130,716 | **−53.4%** |
| retired instructions | 140,496,805 | 123,171,670 | −12.3% |

On the harness's `assignment+expansion 100k` — assignments, `[`, `${s#pre-}`,
`${t%-post}`, `${#u}` — the same three take 342,865 → 249,849 allocations
(−27.1%) and 132.2 M → 121.6 M instructions (−8.0%).

Two things fell out of it.

**Allocation counts find this class of waste; instruction counts do not.** A
`Vec` that is allocated and freed costs a handful of instructions, so the clock
and the count of retired instructions barely move; what it costs is the
allocator's bookkeeping and the cache traffic, which is why this section's two
numbers disagree more than anywhere else on this page.

**Everything except `finish_grow` was the same shape:** something owned was
created so that a `&[u8]` could be read out of it. Borrowing it, or writing into
a buffer the caller already holds, removes the allocation without changing what
the shell stores — which is why none of the three commits changes behaviour.

**The next round: `smallvec` in that path does nothing, and a pre-check does.**
The rest of the list is short-lived vectors in the per-word expansion, so the
obvious move is to inline them — `Expanded::chunks`, `Field::quoted` and the
splitter's three vectors as `SmallVec`. Measured (same A/B protocol):

| per-word `SmallVec`s | before | after | |
| --- | ---: | ---: | ---: |
| allocations, 10k iterations of `:; (( i++ ))` | 130,716 | 90,715 | −30.6% |
| retired instructions | 123,171,670 | 119,202,080 | −3.2% |
| `: plain` loop, 100k commands (wall clock) | | | +0.5% |
| `cmd subst 20k` | | | +1.6% |
| `source bash_completion` ×15 | | | +0.5% |

Thirty percent of the allocations and three percent of the instructions, and no
time at all — the three wall-clock rows are inside the ±1% floor, and two of them
are on the wrong side of it. That is what removing *this* kind of allocation is
worth: glibc serves a one-to-eight-byte `Vec` from the tcache in a handful of
instructions, while an inline vector makes `Chunk`, `Field` and `Expanded` bigger,
so every move of one copies more bytes (and `memmove` was already 8.7% of the
loop). Reverted.

What did pay was a one-line pre-check in the same path. `expand_once`'s first act
was `structural(parts)`, which builds a `Vec<(Pos, u8)>` holding one entry per
unquoted byte of the word, and `find_group` then searches it for a `{` — for every
command word, to conclude that `:` has no brace group. Only an unquoted literal can
hold a group opener, so looking for the byte first skips the scan and its
allocation:

| case | before | after | |
| --- | ---: | ---: | ---: |
| `: plain` loop, 100k commands | 54.61 ms | 53.27 ms | **−2.4%** |
| assignment+expansion, 100k | 196.56 ms | 186.40 ms | **−5.2%** |
| allocations, 10k iterations | 130,716 | 120,716 | −7.6% |
| retired instructions | 123,171,670 | 120,901,856 | −1.8% |

Lowering one allocation per word bought five percent where lowering forty bought
none, because it also removed the walk that filled it. That is the same lesson
this page keeps arriving at from different directions: the win is in not doing the
work, not in making the allocation cheaper — which is why the way out of the rest
of this list is a buffer reused across commands, not a type with more room in it.

What is left is `finish_grow` (35% of what remains), one allocation per command
in `eval_node`, and the expansion path, which allocates once per word
(`expand_one`, `expand_parts`) even for a word with nothing to expand. The value
type is where that ends, but not with a tagged `Int`/`Float`: a shell variable's
value *is* text — `x=010; echo $x` is `010`, while `declare -i y=010; echo $y` is
`8`, which is exactly why bash carries an *attribute* rather than a type — and
`Float` does not exist in bash's arithmetic at all (`DESIGN.md` §3.3). The
candidate that would pay is a bytes type with short values inline (a path, `1`,
`pre-3-post` all fit in 22 bytes) behind `Deref<Target = [u8]>`, so that no match
arm and no call site changes.

### 2026-11-17: the variable table's hot path, and the per-command specials

With the allocation churn down, the profile of `:; (( i++ ))` stopped being
about `malloc` at all: `eval_node` 12.9% of samples, `memmove` 8.7%,
`IndexMap::get_index_of` 8.4%, `memcmp` 7.5%. The `memcmp` call graph named one
thing. `(( i++ ))` looks `i` up **twice** — once in `IntVars::get`, once in `set`
→ `assign_scalar` → `resolve_chain` — and each lookup hashed a one-byte name and
then compared keys through a `bcmp` call that costs about twenty cycles to
compare one byte.

Two changes came out of that.

**The last global lookup is remembered** (`crates/baash-runtime/src/vars.rs`). A
name of at most eight bytes packs into a `u64`, and then the packing *is* the
name: equal packing and equal length means equal name, with no hash and no call.
`Vars` keeps the packing, the length and the index of the last hit, and a hit is
two integer comparisons and a bounds check. Nothing has to be invalidated,
because the entry at the cached index is re-packed and compared: a table that
shifted under the cache (an `unset`, a restored scope) is a *miss*, never a wrong
variable. Longer names are not cached — their first eight bytes would not decide
it — which costs nothing, since a shell's hot variables are `i`, `x`, `IFS`,
`PATH`.

**The per-command specials are fixed buffers.** `$?`, `$$`, `$!`, `$SECONDS`,
`$RANDOM` and `$LINENO` were `Vec<u8>`s rewritten after every command, so every
command paid a `memmove` call per value to copy one to six bytes into a heap
buffer. They are a twenty-byte array and a length now, formatted in place; the
formatter takes digits two at a time from a static table (a division costs twenty
cycles, and a status is two digits) and moves the result to the front with a loop
rather than `copy_within`, which is another call at that size.

| `: plain` loop, 100k commands | before | after | |
| --- | ---: | ---: | ---: |
| remembered lookup | 53.01 ms | 50.69 ms | −4.4% |
| fixed specials (on top of it) | 51.34 ms | 45.62 ms | **−11.1%** |
| the two together | 52.91 ms | 45.90 ms | **−13.3%** |
| assignment+expansion, 100k, together | | | −3.9% |
| `source bash_completion` ×15 | | | — |
| retired instructions, 10k iterations | 120,901,856 | 109,982,314 | −9.0% |

Ten percent of that loop was five one-to-six-byte `memmove` calls and a pair of
hash lookups *per command* — which is the same lesson as the allocation round
seen from the other side: what costs is the call and the cache line, not the
bytes. A fixed buffer and a packed name remove both without changing a byte of
what the shell stores.

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

### Wall clock: `scripts/bench.py`

`python3 scripts/bench.py`, optionally
`--shell target/debug/baash --bash /bin/bash --reps 5 --filter expansion --json`.
It prefers a release build over a debug one and honours `CARGO_TARGET_DIR`, and
it says so on stderr when the binary it found looks like a debug build.

Each case is a `-c` script, run 3–5 times, minimum wall time kept (min is the
least noisy estimator for CPU-bound work); the startup case runs 100 spawns
inside one `/bin/sh` loop and divides, because one Python `subprocess.run` costs
more than either shell's startup. `/bin/true` does not exist on NixOS, so the
fork+exec case uses `env true`.

What the cases cover, so a change knows which one to watch:

| group | cases | what moves them |
| --- | --- | --- |
| interpreted work | `colon loop 20k`, `arith loop 20k`, `expansion 20k ${s#*c}`, `expansion 20k ${s//d/e}`, `function call 20k`, `case 20k`, `builtin echo 20k`, `assignment+expansion 100k` | the evaluator, expansion, the pattern cache; `colon loop` is one command boundary and nothing else |
| processes | `fork+exec 200 (env true)`, `pipeline 200 (echo \| cat)` | `posix_spawn`, the pipeline swap |
| scopes | `subshell 20k ( : )`, `cmd subst 20k $(echo $i)`, `cmd subst 20k, body writes`, `subshell 200, 20k-element array` | `open_scope`/`close_scope`: the journal, the signal set, the `getcwd`/`chdir` pair |
| the variable table | `indirect 100\|400\|1000 vars, 200 passes` | lookup cost at three table sizes |
| start-up | `startup, per spawn`, `source bash_completion x15` | the loader, the completion library |

The substitution cases run **once** per case and at ×20k, because bash needs
about 3 s for one run; the counts match the tables above, so a re-run reproduces
them rather than approximating them. The rest are 3–5 runs at counts that keep a
release build in the tens of milliseconds. `source bash_completion x15` is
skipped, with a note on stderr, when `vendor/bash-completion` is not there —
`nix develop` and `nix build` put it there, a bare checkout does not.

An A/B of *two baash builds* is a different tool: the harness above measures one
shell against bash.

### A/B of two builds: `scripts/ab.py`

    python3 scripts/ab.py --a /tmp/baash-before --b /tmp/baash-after \
        --case 'colon loop 20k'
    python3 scripts/ab.py --a old --b new --reps 8 --script 'i=0; while (( i < 100000 )); do :; (( i++ )); done'

`--a` is the `before` column and `--b` the `after`, so the sign is always "did
the change help". `--case` names a case from `scripts/bench.py` so the two tools
share one list; `--script` takes anything else. Both sides must be built by the
same toolchain (`--release`, same `CARGO_TARGET_DIR` and flags): a build made
through the ambient toolchain and one made in `nix develop` differ by 455 KB of
`.text`, because the C dependencies are compiled by whichever `cc` is on `PATH`.

What the tool does, and why, is the 2026-11-17 section above: each run copies its
side over **one shared path** and throws away that copy's first run (the file a
binary lives in was worth 3.5% between byte-identical copies), and each block
runs **A,B,B,A** (plain alternation made the second run of every pair 1.5–3.5%
slower even for identical binaries). The minimum is kept per side. Run it with
`--a` and `--b` pointing at the *same* build to measure that day's floor; on this
host it lands within ±1%, which is why a change smaller than that wants the
instruction counts too:

    nix shell nixpkgs#valgrind -c valgrind --tool=callgrind --quiet \
        --callgrind-out-file=/tmp/cg.out <binary> --norc -c '<script>'
    rg -m1 '^summary:' /tmp/cg.out

`callgrind` counts retired instructions, so it does not care about frequency,
layout or a neighbour — but it also does not price cache misses or a branch, which
is why it is a second opinion and not the headline.

### Allocations: `callgrind --tree=calling`

The allocation counts above come from the same recording as the instruction
counts, read differently: `callgrind_annotate --tree=calling` prints, for every
function, the calls it makes, so `malloc`'s block lists who allocated and how
many times. That is the view that turns "fourteen allocations per command" into a
list of culprits — a flamegraph shows the time but not the count, and `massif`
shows bytes, which is the less useful dimension here (a hundred 8-byte `Vec`s
cost more than one 2 KB one).

    nix shell nixpkgs#valgrind -c valgrind --tool=callgrind --quiet \
        --callgrind-out-file=/tmp/cg.out <binary> --norc -c '<script>'
    nix shell nixpkgs#valgrind -c callgrind_annotate --tree=calling /tmp/cg.out \
        | rg -A2 ':malloc'

Read the `free` list beside it: the totals should be within a few calls of each
other, and a gap is something retained rather than churn.

The release binary scores the same conformance as the debug one
(1986/2787 total, 1931 strict), so the optimiser is not hiding a behavioural
difference.

### Sampled: `scripts/perf.sh`, or `just prof`

`./scripts/perf.sh --norc -c 'i=0; while (( i < 20000 )); do ( : ); (( i++ )); done'`
builds a release binary with frame pointers and debuginfo *in `target/perf`*
(the shipped build has neither, and a profile of it is a list of addresses),
records it, and prints self time by symbol. All arguments go to baash.

The clock is `task-clock`, not a hardware counter: this host has no PMU
(`perf record -e cpu/cycles` fails with "No such file"), and a container or VM
usually is the same. `task-clock` counts CPU time, so a blocked or waiting
process samples as nothing — which is why no wall-clock claim here comes from a
profile. `-F 4000 -g --call-graph fp` is 4 kHz with frame-pointer stacks;
`perf report -i target/perf/record.data --children -g graph` answers "who
called this", which is how the `set_line` rescan was found.

Both are worth re-running on the commit that last touched the area, not on a
memory of the number: the sections above name the commit they measured.
