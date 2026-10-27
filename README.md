# ßhell

A POSIX- and bash-compatible interactive shell in Rust. The design rationale
lives in [DESIGN.md](DESIGN.md); this file is the operator's manual.

**Status: scaffold.** The workspace, crate graph, build and CI are in place.
One feature is actually finished: `bsh --highlight`. Everything else parses,
compiles and tests cleanly but is not implemented.

## Layout

```
crates/
  bsh-diag       diagnostic IR: spans, stable codes, sources   (no dependencies)
  bsh-syntax     lexer, spans, parse outcomes, expectations
  bsh-highlight  token + theme -> styled spans, ANSI rendering
  bsh-lang       words, quoting, expansion
  bsh-compile    AST -> bytecode chunk
  bsh-runtime    options, variables, the ShellHost interface
  bsh-vm         the interpreter loop
  bsh-sys        syscalls, tty detection, fork gate   <- the only `unsafe` crate
  bsh-complete   completion candidates and ranking
  bsh-repl       line editor and terminal rendering
  bsh-tools      fmt, lint, explain, debugger
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
cargo run -p bsh -- -c 'echo hi'
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
