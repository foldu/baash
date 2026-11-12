# AGENTS.md

Guidance for agents (and humans) working in this repository.

This file is the day-to-day loop. The narrative lives elsewhere and is worth
reading first:

| file | what it answers |
|---|---|
| [README.md](README.md) | the operator's manual: what works, what does not |
| [DESIGN.md](DESIGN.md) | why the shell is shaped the way it is |
| [docs/direction.md](docs/direction.md) | the five commitments and current waves |
| [docs/state.md](docs/state.md) | the resume document: what exists, what is proven, what is next |
| [docs/contracts.md](docs/contracts.md) | the landed API per crate, and the cross-crate rules |
| [docs/portability.md](docs/portability.md) | what Windows does and does not do yet |

`baash` is a POSIX/bash-*similar* shell in Rust. Keep its commitments
(`docs/direction.md` §1) in mind, because they decide most arguments:

1. **No `fork()`.** Subshells and `$( … )` are in-process subinterpreters with
   a scoped state. Anything that cannot work that way is rejected with a
   diagnostic, not approximated.
2. **Windows is a first-class target.** Cross-compiled, and verified on a real
   guest for behaviour that only exists there.
3. **Interactive is first.** No lag at the prompt, in completion or in prompt
   refresh. That is what `compio` (io_uring on Linux, IOCP on Windows) and the
   worker threads are for.
4. **Shell code runs on the main thread.** Workers handle I/O and data, never
   the interpreter.
5. **uutils are embedded** as built-ins, curated and feature-gated.

## The loop

`just` is the task runner (`justfile`); there is no Makefile and should not be
one.

```sh
just                 # list the tasks
just build           # nix build .#default -> result/bin/baash
just windows         # nix build .#windows      -> result-windows/bin/baash.exe
just windows-fast    # nix build .#windows-fast -> result-windows-fast/ (no LTO)
just test            # cargo test --workspace --all-features
just bench           # cargo bench -p baash-bench -> the `=~` tables
just conformance     # the Oils ratchet against crates/baash-spec/baseline.txt
just licenses        # cargo deny check licenses
just check           # nix flake check: build + fmt + clippy + licenses + windows
```

Outside `nix develop` the same gate is:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- --deny warnings
cargo test --workspace --all-features
cargo build -p baash
cargo run -p baash-spec -- --shell "${CARGO_TARGET_DIR:-target}/debug/baash" \
    --list-failures --ratchet --tolerance 5
```

**`just release` is not part of the loop.** It bumps, tags, pushes and
publishes the release.

## Before you call it done

A change is finished when the gate is green *and* the behaviour was verified —
not when it compiles.

1. `cargo fmt` and clippy, as above. CI runs clippy with `--deny warnings` for
   the whole workspace, all targets, all features.
2. `cargo test --workspace --all-features` with zero failures. Tests run one at
   a time on purpose (`.cargo/config.toml` sets `RUST_TEST_THREADS=1`):
   `$( … )` redirects descriptor 1, which is a property of the process, so two
   capture tests at once take each other's bytes apart.
3. The conformance ratchet. It must not drop below the baseline;
   `crates/baash-spec/baseline.txt` is the record and `--tolerance 5` is the
   noise margin. Lowering either is a decision to explain in the commit, never
   a way to turn red green. The suite's own numbers are in the README.
4. If the change touches code under `cfg(windows)` it has *not* been compiled by
   any of the above. Run a Windows check (next section); a borrow error and
   several stale clippy lints have already reached commits because only the
   Windows target compiles those lines.
5. `nix flake check` when the change touches the flake or the vendored sources,
   or adds a source file (see [Nix and files](#nix-and-files)).

## Tests

* Logic gets unit tests beside it (`#[cfg(test)] mod tests`); something a user
  can see gets an integration test through the real binary in
  `crates/baash/tests/`.
* A bug fix starts with a case that fails for the right reason, and the commit
  says what the wrong behaviour was.
* `interactive_*` tests drive a real pty: they need a working `/dev/pts`, and
  they do not work in a nix build sandbox (`openpty` succeeds, reads hang). Run
  them on a real host, not inside `nix build`.
* 18 conformance cases read the machine rather than the shell (hostname,
  timezone data, effective uid, wall-clock timers), so a build sandbox measures
  about 18 fewer passes than a real host. Run the ratchet on a host.
* Count tests with `cargo test --workspace --all-features -- --list | grep -c
  ': test$'`: a few pty-driven binaries lose their libtest summary when stdout
  is redirected, and the per-binary "ok" lines therefore under-report.

## Layout

```
crates/
  baash-diag       diagnostic IR: spans, stable codes, sources    (no dependencies)
  baash-syntax     lexer, spans, parse outcomes, expectations
  baash-highlight  token + theme -> styled spans, ANSI rendering
  baash-lang       words, quoting, expansion, prompt escapes
  baash-pattern    pattern matching (`${x%pat}`, globbing, `case`)
  baash-runtime    options, variables, the ShellHost interface, completion contract
  baash-vm         the evaluator: builtins, redirections, conditions, the tree walk
  baash-sys        syscalls, tty/console, spawn, clocks          <- the only `unsafe` crate
  baash-history    the Turso-backed history ring
  baash-complete   completion candidates and ranking
  baash-repl       line editor and terminal rendering
  baash-spec       the Oils suite runner and the ratchet
  baash-tools      dev helpers (`fmt`/`lint`/`explain`; stub)
  baash-bench      the `=~` benchmarks (`cargo bench -p baash-bench`)
  baash-utils      embedded coreutils (uutils), the fallback when `PATH` finds nothing
  baash            the binary: CLI, rc files, wiring
```

Where a change belongs: a builtin is `baash-vm/src/tree/builtins.rs` (plus its
registry); expansion is `baash-lang`; lexing and word syntax are `baash-syntax`;
anything that differs by platform is `baash-sys` (`fd.rs`/`term.rs` versus
`windows.rs`/`console.rs`) with the platform-neutral grammar in `paths.rs`;
editor, keys and rendering are `baash-repl`; candidate sources and ranking are
`baash-complete`.

Dependency direction is `DESIGN.md` §4.1: the crates are layered, and
`baash-sys` is the only crate permitted `unsafe` — and it denies undocumented
unsafe blocks.

## Invariants

* **No fork, ever.** A subshell is a scope (`baash-vm/src/tree/scope.rs`); a
  pipeline's stages are in process except for externals, which `posix_spawn`.
  Backgrounding shell code is rejected (`BAASH-E0302`). There is no job
  control.
* **Shell text is bytes.** `&[u8]`, not `String`, for anything that came from a
  command line or a file: filenames, expansions, patterns. `String` is for
  messages and diagnostics. OS-string conversion goes through
  `baash_sys::os`, never a lossy `to_str`.
* **Platform code lives behind `baash-sys`.** Do not sprinkle `cfg(windows)` in
  the evaluator: put the difference in the sys layer, or in a
  platform-parameterised value like `baash_sys::paths::Flavour`, which is
  testable on Linux.
* **Diagnostics are data.** Stable codes (`BAASH-Exxxx`), spans, `miette`
  rendering, help text. A bad command line or a bad script is a diagnostic, not
  a panic; a panic in the shell loses the user's session.
* **Public APIs are recorded.** `docs/contracts.md` is the landed-API record;
  update it in the same commit that changes an interface.
* **Documentation is part of the change.** `docs/portability.md` for platform
  behaviour and gaps, `docs/performance.md` for measurements,
  `docs/state.md` when the picture changes (not every commit).
* **Dependencies need a decision.** A new crate is a conversation, not a
  `cargo add`: it must be GPL-3.0-compatible for `cargo deny`, and it lands in
  `THIRD-PARTY.md`.
* **Comments explain why.** The code is prose-heavy on purpose: the reason a
  POSIX rule is shaped the way it is, the bug a guard prevents, the measurement
  behind a choice. Match that voice; a bare "what" comment adds nothing.

## Windows

Windows code is the part of this tree that a Linux `cargo` never sees, so it
gets its own protocol.

* **Compile it before claiming it.** There are two Windows builds of the same
  program. `nix build .#windows` (`just windows`) is the release shape CI ships
  and the guest runs: fat LTO, one codegen unit, stripped.
  `nix build .#windows-fast` (`just windows-fast`) is the same profile with LTO
  off — larger, a few percent slower, and the one to use while editing.
  * Both are split into a cached dependency derivation and a thin per-change
    one, so an edit under `crates/*/src` must cost seconds, not the graph.
    `docs/portability.md` §11 has the numbers and the rule that keeps it true:
    the two halves must be configured identically.
  * `nix build .#windows-deps` builds the cached half on its own; its store
    path must not move when only `crates/*/src` changed. If it does, the halves
    have drifted.
  * The derivation (`flake.nix`, `mkWindows`) is the source of truth for the
    flags: `cargo zigbuild --profile windows --target x86_64-pc-windows-gnu -p
    baash` with a `windres` shim on `PATH` and `BAASH_WINDRES` pointing at
    LLVM's `windres`; `libmimalloc-sys` needs `-Wno-error=date-time`, which the
    derivation sets.
* **`cargo clippy --target x86_64-pc-windows-gnu -p baash -- -D warnings`** is
  the cheapest compile check for `cfg(windows)` code. It needs a toolchain with
  that target installed (the flake's `windows` derivation pins one; with rustup
  it is `rustup target add x86_64-pc-windows-gnu`). `--all-targets` does not
  work for the target: the pty tests and `baash-spec` are Unix-only by design.
* **Verify behaviour on the real guest, not Wine.** Wine can run `-c` commands,
  but it has no usable console, so the entire interactive path (the line
  editor, keys, prompt) only exists on Windows. The guest is the `winvm`
  Windows 11 domain; `winvm reset` first, then copy the exe into `C:\build`.
* **A GUI run needs a console session.** A process started over ssh runs in
  session 0 with no window. The way that works is a scheduled task with `/it`:
  `schtasks /create /tn <name> /tr '"cmd /k C:\build\baash.exe"' /sc once /st
  23:59 /it /f` then `/run`; `cmd /k` keeps the window after the shell exits.
  Screenshot with `virsh --connect qemu:///system screenshot win /tmp/x.png`.
* **The exe cannot be replaced while it runs.** End the task and kill the
  session-1 `cmd`/`conhost` first, or the copy fails with a sharing violation.
* **Quote Windows paths in baash, because `\` is an escape.** `cd
  C:\Windows` reaches `cd` as `C:Windows`; write `cd C:/Windows` or
  `cd 'C:\Windows'`. This is the msys convention and is deliberate.
* Windows facts that have already cost a debugging session: environment names
  are case-insensitive (`Path` vs `PATH`); a path list is `;`-separated; a bare
  name is resolved through `PATHEXT`; `GetConsoleMode` needs *read* access to
  the handle; there is no `HOME` (`%USERPROFILE%` stands in); a root is a drive
  or a UNC share, and `..` stops there (`baash_sys::paths`).
* **Names are WTF-8, and every conversion must go through the codec.**
  `baash_sys::wtf8` carries a Windows name — including an unpaired surrogate —
  through shell bytes exactly, so `chdir`, `CreateProcessW`, the CRT open and
  the environment block all use it (`os::bytes_from_os`/`os::os_from_bytes`).
  A new `from_utf8_lossy` on a path is a bug: display may be lossy, the name
  may not.

## Nix and files

* `nix develop` provides the pinned toolchain. `flake.nix`'s `rustVersion` and
  `RUST_VERSION` in `.github/workflows/ci.yml` must stay in step.
* `nix flake check` is the whole gate at once; its checks are `build`, `fmt`,
  `clippy`, `licenses`, `windows` and `module` (the NixOS module's, in
  `nix/check-module.nix`).
* The flake's `src` is a **git-tracked** tree (`git ls-files`), so a new file
  that is not `git add`ed is missing from the build — the failure looks like a
  missing module or a missing test file, not like a staging mistake.
* GitLab is the source of truth and runs the flake; the GitHub workflow is the
  mirror and runs the plain-toolchain equivalent.

## Commits and git

* Work on `master`. Keep the tree clean and one logical change per commit.
* **Never push.** Never amend, rebase or force-push a commit that is on
  `origin`: history there is not yours to rewrite.
* Subject: `area: imperative summary`, lower case, no full stop. Areas follow
  the crate or the concern: `vm:`, `runtime:`, `lang:`, `sys:`, `windows:`,
  `editor:`, `prompt:`, `complete:`, `history:`, `perf:`, `docs:`.
* Body: the symptom, the cause, the fix, the verification. The existing
  messages are the template — they state what was *not* fixed and what was
  verified on which machine. A table is fine when a rule has shapes
  (`docs/portability.md` is full of them).
* Every agent commit ends with:
  `Co-authored-by: DeepSeek-V4.1-Flash <noreply@deepseek.com>`
  (`docs/state.md` §10).
* Do not commit `target/`, `result`, `dist/` or captures.

## Environment traps

* If the checkout path contains non-ASCII bytes (this one contains `ß`),
  linking can fail; `docs/state.md` §1 keeps its worktrees under `/tmp` for
  that reason.
* **`vendor/bash-completion/` is not in git.** A build with the
  `embedded-bash-completion` feature mounts the bash-completion library inside
  the shell, and the bytes come from nixpkgs: `flake.nix` copies
  `pkgs.bash-completion` into that folder in `preBuild`, and `nix develop` does
  the same when the shell starts (guarded by the recorded store path, so an
  unchanged tree is not recopied and the crate is not rebuilt). `nix build
  .#bash-completion` prints the exact tree. The feature is on for Windows and
  off otherwise, so `cargo test --workspace` runs the library tests only with
  `--all-features`; without the tree they print a skip
  (`crates/baash/tests/embedded_completion.rs` and `bash_completion.rs` say
  why).
* `CARGO_TARGET_DIR` redirects where `cargo` writes, but `baash-spec` defaults
  to `target/debug/baash`; pass `--shell` explicitly, or you will measure the
  shell you built an hour ago.
* `--list-failures` is worth the noise: the alternative is guessing which
  POSIX rule a red case was about.
