# AGENTS.md

Guidance for agents (and humans) in this repository. Read first:

| file | answers |
|---|---|
| [README.md](README.md) | what works, what does not |
| [DESIGN.md](DESIGN.md) | why the shell is shaped this way |
| [docs/direction.md](docs/direction.md) | the commitments, current waves |
| [docs/state.md](docs/state.md) | what exists, what is proven, what is next |
| [docs/contracts.md](docs/contracts.md) | the landed API per crate, cross-crate rules |
| [docs/portability.md](docs/portability.md) | Windows: behaviour and gaps |
| [docs/windows.md](docs/windows.md) | the Windows protocol: compile, build, guest |

`baash` is a POSIX/bash-*similar* shell in Rust. Five commitments
(`docs/direction.md` §1) decide most arguments:

1. **No `fork()`.** Subshells and `$( … )` are in-process subinterpreters with
   scoped state; what cannot work that way gets a diagnostic, not an
   approximation.
2. **Windows is first-class** — cross-compiled, verified on a real guest.
3. **Interactive is first** — no lag at the prompt; that is what `compio`
   (io_uring, IOCP) and the workers are for.
4. **Shell code runs on the main thread**; workers only do I/O and data.
5. **uutils are embedded** as built-ins, curated and feature-gated.

## The loop

`just` is the task runner; there is no Makefile and should not be one.

```sh
just                 # list the tasks
just build           # .#default      -> result/bin/baash
just windows         # .#windows      -> result-windows/bin/baash.exe
just windows-fast    # .#windows-fast -> result-windows-fast/ (no LTO)
just test            # cargo test --workspace --all-features
just bench           # cargo bench -p baash-bench -> the `=~` tables
just conformance     # the Oils ratchet against crates/baash-spec/baseline.txt
just licenses        # cargo deny check licenses
just check           # nix flake check: build + fmt + clippy + licenses + windows
```

Without `nix develop`, that gate is:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- --deny warnings
cargo test --workspace --all-features
cargo build -p baash
cargo run -p baash-spec -- --shell "${CARGO_TARGET_DIR:-target}/debug/baash" \
    --list-failures --ratchet --tolerance 5
```

**`just release` is not part of the loop.** It bumps, tags, pushes and publishes.

## Before you call it done

The gate is green *and* the behaviour was verified — not "it compiles".

* Clippy runs with `--deny warnings` for the whole workspace, all targets, all
  features.
* Tests run one at a time on purpose (`RUST_TEST_THREADS=1` in
  `.cargo/config.toml`): `$( … )` redirects descriptor 1, a property of the
  process, so two capture tests at once take each other's bytes apart.
* The ratchet must not drop below `crates/baash-spec/baseline.txt`; the noise
  margin is `--tolerance 5`. Lowering either is explained in the commit, never a
  way to turn red green.
* Nothing above compiles `cfg(windows)`. Check it the way
  [docs/windows.md](docs/windows.md) says — a borrow error and stale clippy
  lints have reached commits because only that target compiles those lines.
* `nix flake check` when the change touches the flake, the vendored sources or a
  new source file.

## Tests

* Logic gets `#[cfg(test)] mod tests` beside it; user-visible behaviour gets an
  integration test through the real binary in `crates/baash/tests/`. A bug fix
  starts with a case that fails for the right reason, and the commit says what
  the wrong behaviour was.
* `interactive_*` tests drive a real pty: they need `/dev/pts` and do not work in
  a nix build sandbox (`openpty` succeeds, reads hang). Use a real host.
* 18 conformance cases read the machine, not the shell (hostname, timezone data,
  euid, wall-clock timers), so a sandbox measures ~18 fewer passes.
* `cargo test --workspace --all-features -- --list | grep -c ': test$'` counts
  tests honestly; pty-driven binaries lose their libtest summary when stdout is
  redirected.

## Where a change belongs

Builtins: `baash-vm/src/tree/builtins.rs` plus its registry. Expansion:
`baash-lang`. Lexing and word syntax: `baash-syntax`. Anything that differs by
platform: `baash-sys` (`fd.rs`/`term.rs` versus `windows.rs`/`console.rs`, the
neutral grammar in `paths.rs`). Editor, keys, rendering: `baash-repl`.
Candidates and ranking: `baash-complete`.

What each crate is responsible for is `docs/state.md` §3; the layering is
`DESIGN.md` §4.1. `baash-sys` is the only crate permitted `unsafe`, and it
denies undocumented blocks.

## Invariants

* **No fork, ever.** A subshell is a scope (`baash-vm/src/tree/scope.rs`);
  pipeline stages are in process except externals, which `posix_spawn`.
  Backgrounding shell code is rejected (`BAASH-E0302`). No job control.
* **Shell text is bytes.** `&[u8]`, not `String`, for anything from a command line
  or a file: filenames, expansions, patterns. `String` is for messages. OS-string
  conversion goes through `baash_sys::os`, never a lossy `to_str`.
* **Platform code lives behind `baash-sys`.** No `cfg(windows)` sprinkled in the
  evaluator: put the difference in the sys layer, or in a parameterised value
  like `baash_sys::paths::Flavour`, which is testable on Linux.
* **Diagnostics are data.** Stable codes (`BAASH-Exxxx`), spans, `miette`, help
  text. A bad command line or script is a diagnostic, not a panic — a panic loses
  the user's session.
* **Record the change.** `docs/contracts.md` for a landed API, in the same commit;
  `docs/portability.md` for platform behaviour and gaps; `docs/performance.md` for
  measurements; `docs/state.md` when the picture changes (not every commit).
* **Dependencies need a decision**, not a `cargo add`: GPL-3.0-compatible for
  `cargo deny`, and recorded in `THIRD-PARTY.md`.
* **Comments explain why** — the reason a POSIX rule is shaped that way, the bug a
  guard prevents, the measurement behind a choice. The code is prose-heavy on
  purpose; a bare "what" adds nothing.

## Commits and git

* Work on `master`, one logical change per commit, tree clean.
* **Never push.** Never amend, rebase or force-push a commit that is on `origin`:
  history there is not yours to rewrite.
* Subject `area: imperative summary`, lower case, no full stop; areas are `vm:`,
  `runtime:`, `lang:`, `sys:`, `windows:`, `editor:`, `prompt:`, `complete:`,
  `history:`, `perf:`, `docs:`.
* Body: symptom, cause, fix, verification, including what was *not* fixed and
  where it was verified. A table is fine when a rule has shapes.
* Every agent commit ends with

  `Co-authored-by: DeepSeek-V4.1-Flash <noreply@deepseek.com>`

  and never commits `target/`, `result`, `dist/` or captures.

## Nix and CI

* `nix develop` pins the toolchain; `flake.nix`'s `rustVersion` and
  `RUST_VERSION` in `.github/workflows/ci.yml` must stay in step.
* The flake's `src` is a **git-tracked** tree (`git ls-files`), so a new file
  that is not `git add`ed is missing from the build. The failure looks like a
  missing module or a missing test file, not like a staging mistake.
* `nix flake check` runs `build`, `fmt`, `clippy`, `licenses`, `windows` and
  `module` (the NixOS module's, in `nix/check-module.nix`). GitLab is the source
  of truth and runs the flake; the GitHub workflow is the mirror and runs the
  plain-toolchain equivalent.

## Traps

* A checkout path with non-ASCII bytes (this one has `ß`) can fail to link;
  `docs/state.md` §1 keeps its worktrees under `/tmp` for that reason.
* **`vendor/bash-completion/` is not in git.** With `embedded-bash-completion`
  the library is mounted from nixpkgs: `flake.nix` copies
  `pkgs.bash-completion` there in `preBuild`, and `nix develop` does the same on
  shell start. `nix build .#bash-completion` prints the tree. The feature is on
  for Windows and off otherwise, so the library tests need `--all-features`;
  without the tree they print a skip
  (`crates/baash/tests/embedded_completion.rs` says why).
* `CARGO_TARGET_DIR` moves where `cargo` writes, but `baash-spec` defaults to
  `target/debug/baash`: pass `--shell`, or you measure an hour-old shell.
* `--list-failures` is worth the noise; the alternative is guessing which POSIX
  rule a red case was about.
