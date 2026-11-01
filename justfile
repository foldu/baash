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
test:
    cargo test --workspace --all-features

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

# The Windows executable: `result/bin/baash.exe`.
windows:
    nix build .#windows

# Cut a release: gate, bump, commit, tag `v<version>`, push; the tag publishes.
release version: check test conformance
    cargo release --execute {{version}}
    git push --follow-tags
