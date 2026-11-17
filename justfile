# Developer tasks. Everything here assumes `nix develop`, which provides
# cargo, just, cargo-release and the pinned toolchain.
set shell := ["bash", "-uc"]

# Show this list.
default:
    @just --list

# Everything a derivation can hold: build, fmt, clippy, licenses, Windows.
check:
    nix flake check --print-build-logs

# The full suite. It drives a real pty, so it is not part of `check`.
# One process per test, in parallel: 1697 cases in ~5 s instead of ~37
# (`docs/performance.md`). Each case gets its own process, so the descriptor-1
# property that forces `cargo test` to one thread at a time stops mattering.
# Doctests are separate because nextest does not run them.
test:
    cargo nextest run --workspace --all-features
    cargo test --workspace --all-features --doc

# The same cases through libtest, one at a time: for a debugging session where
# a breakpoint or a single-threaded assumption is the point.
test-serial:
    cargo test --workspace --all-features

# A sampled CPU profile of one baash command line (scripts/perf.sh).
prof *ARGS:
    ./scripts/perf.sh {{ARGS}}

# Wall-clock cases against bash: evaluator, scopes, variables, start-up.
microbench:
    cargo build --release -p baash
    python3 scripts/bench.py

# The `=~` benchmarks: the engine, and the shell's loop against bash.
bench:
    cargo build --release -p baash
    cargo bench -p baash-bench -- --shell "${CARGO_TARGET_DIR:-target}/release/baash" --bash "$(command -v bash)"

# The conformance ratchet against the vendored Oils suite.
conformance:
    cargo build -p baash
    cargo run -p baash-spec -- --shell target/debug/baash --list-failures --ratchet --tolerance 5

# The license gate: every dependency must be in `deny.toml`'s allow-list.
licenses:
    cargo deny check --deny warnings licenses

# The Linux package: `result/bin/baash`.
build:
    nix build .#default

# The Windows executable for release: `result-windows/bin/baash.exe`.
windows:
    nix build .#windows -o result-windows

# The same executable without LTO: `result-windows-fast/bin/baash.exe`.
windows-fast:
    nix build .#windows-fast -o result-windows-fast

# The published Linux binaries: glibc 2.17, linked by zig, so the tarball runs
# outside nix. The release workflow builds exactly these.
portable:
    nix build .#portable -o result-portable

portable-aarch64:
    nix build .#portable-aarch64 -o result-portable-aarch64

# Cut a release: gate, bump by `<target>` (`patch`|`minor`|`0.2.0`|`rc`…), commit, tag, push.
release target: (_release-target target) check test conformance
    # `--no-confirm`: this recipe is the confirmation, and cargo-release's own
    # prompt is read from a stdin this sandboxed terminal does not give it —
    # the first 0.1.0-rc.1 attempt answered `y`, printed the plan, and changed
    # nothing while reporting success.
    cargo release --execute --no-confirm {{target}}
    git push --follow-tags
    # `.github/workflows/release.yml` is what publishes, and it runs on GitHub,
    # so the same tag has to reach that remote too. GitLab is the source of
    # truth; both remotes are the same history (`docs/releasing.md`).
    #
    # A sandboxed terminal cannot use the system `ssh` — root-owned config
    # includes and an agent it cannot reach — so `GIT_SSH_COMMAND` may need to
    # name a wrapper that can (`docs/releasing.md`, "When the push is refused").
    git push shithub master --follow-tags

# `just release <target>`, with the whole session kept under `dist/release-logs/`.
release-logged *args:
    scripts/release-log.sh {{args}}

# Show what `just release <target>` would do: the bump, no gate, no commit, no tag.
release-plan target: (_release-target target)
    cargo release {{target}}

# Check a release target before the gate runs. Two reasons: `cargo release`
# parses its argument only after `check test conformance` have finished, so a
# typo used to cost the whole gate; and baash is **0ver** — `minor` carries the
# breaking changes, `major` would leave 0.x, so `major` and any explicit
# version whose major is non-zero are refused here, not by a release that
# already bumped something.
_release-target target:
    @t='{{target}}'; case "$t" in \
        major) printf 'release: baash is 0ver — `minor` is the breaking bump, and `major` would leave 0.x\n' >&2; exit 2 ;; \
        v[0-9]*) printf 'release: the version, not the tag — %s, not %s\n' "${t#v}" "$t" >&2; exit 2 ;; \
        [1-9].*) printf 'release: baash is 0ver — %s would leave 0.x\n' "$t" >&2; exit 2 ;; \
        patch|minor|release|rc|beta|alpha) ;; \
        [0-9]*.[0-9]*) ;; \
        *) printf 'release: unknown target %s — a version (0.2.0) or a level (%s)\n' "$t" 'patch minor release rc beta alpha' >&2; exit 2 ;; \
    esac

# Regenerate the embedded completion tables from the pinned carapace.
carapace-data:
    python3 scripts/carapace-data.py --carapace "${CARAPACE_BIN:-$(nix build --no-link --print-out-paths nixpkgs#carapace)/bin/carapace}"
