# bsh-spec

`bsh-spec` runs the vendored Oils spec suite (`spec/oils/*.test.sh`) against
ßhell, and optionally against reference shells.  It is a dependency-free
reimplementation of the subset of Oils' `test/sh_spec.py` that this project
needs; the suite itself is never modified.

## Usage

```sh
# The default shell is target/debug/bsh; it is built if missing.
cargo run -p bsh-spec

# A smaller run, with failure details:
cargo run -p bsh-spec -- --filter quote.test.sh --list-failures

# The CI ratchet: fail when fewer than N cases pass.
cargo run -p bsh-spec -- --min 1012

# Three-way numbers against a reference shell on PATH.
cargo run -p bsh-spec -- --compare bash

# Machine-readable results for a dashboard.
cargo run -p bsh-spec -- --json target/spec-report.json
```

Useful flags: `--shell PATH`, `--spec-dir PATH`, `--filter SUBSTR`,
`--compare LIST`, `--list-failures`, `--json OUT`, `--min N`,
`--timeout SECS` (default 10), `--jobs N` (default 4), `--keep-temp`.

The summary is:

```
TOTAL: <passed>/<compared> compared, <skipped> skipped, of <all> cases
```

Only cases that were actually run are in `<compared>`; skipped cases are never
counted as passes.  `FAILED` counts ordinary failures, `TIMEOUTS` is separate,
and `OK-OVERRIDE PASSES` counts passes that matched a per-shell
`## OK ... STDOUT:` alternative rather than the unqualified expectation.

## Comparison rules

* A case starts at `#### name`, ends at the next `####` or EOF, and its script
  is the body before the first `## key: value` directive.  `## code:` supplies
  the script instead (used for deliberately invalid syntax).
* `## STDOUT:` / `## STDERR:` blocks are exact expected bytes, terminated by
  `## END` (or the next directive).  `## stdout:` / `## stderr:` are one-line
  forms with an implicit newline.  `## stdout-json:` / `## stderr-json:` are
  decoded JSON strings.
* `## status: N` defaults to 0.
* For the shell under test, any `## OK <shell> STDOUT:` block is an accepted
  alternative to the unqualified stdout, as specified by the harness contract.
  `## BUG` and `## N-I` blocks describe other shells and are not accepted for
  the shell under test.
* For a reference shell, only blocks naming that shell are candidates, falling
  back to the unqualified declaration per stream.  `BUG`/`N-I` blocks still
  state that shell's actual output.
* Stdout is unchecked when the case declares no stdout at all (upstream's
  behaviour; this is why parse-error tests can be status-only).  Stderr is
  compared only when declared.
* Unqualified duplicate declarations take the last value, matching upstream.
  Qualified duplicates and malformed directives are reported as parse errors
  for the whole file rather than being silently guessed at.

Failures include a truncated unified diff.  Timeouts are failures, not skips.

## Environment

Each case runs with cleared environment plus:

* `SH` — the shell under test (or the reference shell, for reference runs)
* `TMP` — a fresh directory, which is also the working directory
* `PATH` — `spec/oils/bin`, the Python helper shim (see below), then the
  inherited `PATH`
* `REPO_ROOT` — a temp shim directory whose `spec/` and `testdata/` symlinks
  point at the vendored tree, so `$REPO_ROOT/spec/testdata/...` works even
  though the vendored copy lives under `spec/oils/`
* `LC_ALL=C`

`## legacy_tmp_dir` creates the `_tmp` subdirectory the upstream runner would.

## Skips

A skip means "this suite entry is out of scope" or "the harness cannot express
this comparison in this environment".  It never means "we do not implement it".
Cases the shell cannot run (missing `read`, `trap`, arrays, …) are failures.

Current skip reasons:

* `our_shell: ysh` — the file or case declares the YSH language.
* `ysh suite (out of scope)` — `ysh-*.test.sh` / `hay*.test.sh`, which upstream
  also classifies as YSH.
* `missing helper: python2` — the case explicitly invokes `python2`, which is
  not installed here.  The vendored helper *programs* are handled separately,
  below.

## Python 2 helper shims

The Python programs in `spec/oils/bin` start with `#!/usr/bin/env python2` and
the environment has no `python2`.  Without a replacement, 772 argv.py
invocations across 58 files fail for *every* shell for an environmental reason,
which would make the pass count meaningless.

When `python2` is not on `PATH`, the harness writes Python 3 equivalents of
`argv.py`, `printenv.py`, `read_from_fd.py`, `show_fd_table.py` and
`stdout_stderr.py` into a temp `shim-bin` directory that precedes
`spec/oils/bin`.  `argv.py` reproduces Python 2's `repr` of byte strings
(`['a', '\xce\xbc']`), so the expected output bytes are unchanged.  The
vendored files themselves are untouched.  If `python2` exists, the shims are
not used.

## Baselines

Measured 2026-10-27 in this worktree with `--compare bash`:

```
TOTAL: 1012/2787 compared, 1184 skipped, of 3971 cases
FAILED: 1774   TIMEOUTS: 1
OK-OVERRIDE PASSES: 48
COMPARE bash: 2574/2787 passed, WE-REGRESS 1579, WE-IMPROVE 17
WALL: 15.2s
```

The suite has 3971 cases in 223 files, matching DESIGN.md §8.  1162 cases are
YSH; the rest of the skips are explicit-`python2` cases.  Of the 1774 failures,
1579 are cases bash passes (clear ßhell gaps) and 195 fail under bash too
(mostly environment-specific: NixOS has a sparse `/bin`, and the suite was
written for OSH's extensions).

Of the 1579 cases bash passes and ßhell does not, the largest clusters are
arrays (173 cases), builtins such as `read`, `trap`, `getopts`, `complete`,
`fc`/`history`, `kill`, `type` (411 cases), parameter expansion and variable
operators (`${x%…}`, `${x@…}`, nameref/vars; 152 cases), arithmetic (62),
extglob/brace expansion (92), aliases (44), `[[ =~ ]]` (34), and job control
(background/wait).  `loop.test.sh:251` (`break oops`) does not terminate — it
prints "loop count out of range" forever — and is the one timeout.

## Tests

`cargo test -p bsh-spec` covers the parser against real vendored files
(case counts, multiline and single-line stdout, `stdout-json` escapes,
per-shell overrides, CRLF and missing final newlines, malformed files), the
diff renderer, JSON output, and an end-to-end CLI test that asserts skips are
not counted as passes and that `--min` ratchets.
