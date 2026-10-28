# ßhell

A POSIX- and bash-compatible interactive shell in Rust. The design rationale
lives in [DESIGN.md](DESIGN.md); this file is the operator's manual.

**Status: it runs, and conformance is now measured.** `bsh` executes scripts,
and `bsh` with no arguments gives you an interactive editor with syntax
highlighting, TAB completion and history.

**1980 of 2787 comparable cases pass** in the vendored Oils suite (71.0%; 1184
cases skipped as out of scope), of which **1925 (69.1%) also pass strictly** —
that is, matching only a case's primary expectation rather than any
`## OK <shell>` alternative. Both numbers are recorded in
`crates/bsh-spec/baseline.txt` and CI fails if either drops.

For scale, bash passes 2574/2787 (92.4%) on the same run, so 640 cases are
things bash does and we do not yet — 62% of that gap is closed. Conformance is
measured but no longer the priority: interactive use comes first, and
[`docs/state.md`](docs/state.md) is the resume document (what exists, what is
proven, what is next). `docs/roadmap.md` is the conformance queue. Run it
yourself with
`cargo run -p bsh-spec -- --shell target/debug/bsh --compare bash`.

Do not read the unit-test count as conformance: it is a different number
measuring a different thing.

What works today: expansion (words, `IFS`, `$@`/`$*`, quoting), redirections
including here-documents, pipelines with `pipefail` and `$PIPESTATUS`,
`if`/`for`/`while`/`until`/`case`, functions with `local`, subshells, command
substitution, arithmetic, `[[ ]]` including `=~`, globbing with
`extglob`/`failglob`, indexed and associative arrays, namerefs, `declare`/
`typeset`/`readonly`/`export` with bash's own `-p` layout, the pattern
operators (`${x#pat}`, `${x%pat}`, `${x//a/b}`, `${x@Q}`), traps, `set -e` and
`set -u`, `break`/`continue`/`return`/`exit`, `exec`, and the built-ins listed
in `docs/contracts.md`.

Not implemented yet: job control (`jobs`, `fg`, `bg`, `disown` — there are no
per-job process groups), `history`/`fc`, the directory stack
(`pushd`/`popd`/`dirs`), `umask`, `let`, `ulimit`, `times`, `enable`,
`$RANDOM`/`$SECONDS`/`$FUNCNAME`, xtrace (`set -x`), `~user`, and process
substitution (`<(...)`, rejected as `BSH-E0301`). Missing built-ins answer
status 2 with `BSH-E0508`; missing special variables expand to nothing.
`printf`, `pwd`, `test` and `[` are served from `PATH` by design.
[`docs/state.md`](docs/state.md) has the full inventory; the interactive
editor's deferred list (Ctrl-R, vi mode, undo, `SIGWINCH` reflow) is at the top
of `crates/bsh-repl/src/lib.rs`.

The bytecode backend is not written yet either: `bsh-vm` runs a tree-walking
evaluator, which is kept afterwards as the reference backend for the compiler.

## Layout

```
crates/
  bsh-diag       diagnostic IR: spans, stable codes, sources   (no dependencies)
  bsh-syntax     lexer, spans, parse outcomes, expectations
  bsh-highlight  token + theme -> styled spans, ANSI rendering
  bsh-lang       words, quoting, expansion
  bsh-compile    bytecode types (unwired; DESIGN §5)
  bsh-runtime    options, variables, the ShellHost interface
  bsh-vm         the interpreter loop
  bsh-sys        syscalls, tty detection, fork gate   <- the only `unsafe` crate
  bsh-complete   completion candidates and ranking
  bsh-repl       line editor and terminal rendering
  bsh-tools      dev helpers (stub)
  bsh            the binary
```

Cross-crate contracts (fork gate, spawn plans, renderers) are in
[docs/contracts.md](docs/contracts.md).

Dependency direction follows DESIGN.md §4.1: `bsh-sys` is the only crate
permitted `unsafe`, and everything else carries `#![forbid(unsafe_code)]`.
`bsh-repl` does not depend on `bsh-runtime`, so the editor can be tested
without an interpreter.

## Building

```sh
nix develop          # cargo, rustc, clippy, rustfmt, rust-analyzer
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- --deny warnings

cargo run -p bsh -- -c 'echo hi'     # run a command
cargo run -p bsh                     # interactive (on a terminal)
cargo run -p bsh -- script.sh        # run a file
cargo run -p bsh -- -n script.sh     # parse only, report diagnostics
printf 'if x; then\n' | cargo run -p bsh -- --highlight   # paint stdin
```

Use `--all-features` for both. `bsh-diag`'s `miette` adapter is behind the
`fancy` feature and is invisible to a default-feature build, so a count without
the flag is not the project's test count.

or without a dev shell:

```sh
cargo build --workspace
nix build .#default
```

### If the checkout path is not ASCII

The directory name contains `ß`, and some toolchains mangle non-ASCII paths
when they appear in linker arguments: the failure looks like

```
clang: error: no such file or directory: '$/home/.../303237hell/target/...'
```

Note `303237` — that is the UTF-8 for `ß` rendered as octal. Two things trigger
it: a `target/` directory inside the checkout, and `NIX_LDFLAGS` in the nix
shell. The dev shell detects a non-ASCII `$PWD` and works around both
(`CARGO_TARGET_DIR` to a temporary path, `NIX_LDFLAGS` unset); outside the dev
shell, set `CARGO_TARGET_DIR` to an ASCII path yourself. Renaming the checkout
is the real fix (DESIGN.md §11).

## CI

GitLab CI on `lab.home.5kw.li`. Every job is tagged `nix`, because everything
runs through the flake and the untagged runner has no nix daemon. Jobs:
`fmt` + `clippy`, `cargo test --workspace`, and `nix build` with the binary
exported as an artifact. The repository must be public.

## Conformance

The [Oils](https://oilshell.org) spec suite is vendored unmodified under
[spec/oils/](spec/README.md) — 223 files, 3971 cases, Apache-2.0, with the
upstream commit recorded alongside it. The harness is not written yet; it will
run each case against `bsh` and against the reference shells on `PATH`, honour
the per-shell override blocks, and report a pass count per file. That count is
the project's only real progress metric (DESIGN.md §8).

## License

Apache-2.0; see [LICENSE](LICENSE).
