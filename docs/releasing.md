# Cutting a release

`just release <version>` does the whole local half: the gate (`check test
conformance`), the bump, the commit, the tag, and the push. The tag is what
publishes; nothing is uploaded from this machine.

```sh
just release 0.1.0-rc.1     # a prerelease: builds and publishes, marked
just release 0.1.0          # the release
```

`baash` is a shell, not a library: no crate goes to crates.io, and the only
artifact is the executable.

## What each file decides

| file | says |
|---|---|
| `Cargo.toml` | `[workspace.package] version` — the only place the version is written. Every crate inherits it (`version.workspace = true`), `flake.nix` reads it for the store path, and `--version` prints it. |
| `release.toml` | `cargo release`: master only, one shared version, tag `v{{version}}`, no publish, no push (the recipe pushes), one commit. |
| `justfile` | the recipe: gate, `cargo release --execute`, both remotes. |
| `.github/workflows/release.yml` | the tag build: version check, the full gate again on the tag, three executables, `SHA256SUMS`, a GitHub release. |

The version lives in one place on purpose. Before `0.1.0-rc.1`, `flake.nix`
repeated `version = "0.0.0"` five times and `cargo release` knew nothing about
them, so a released store path would have said `0.0.0`.

## Two remotes

GitLab (`origin`) is the source of truth and runs the flake check; GitHub
(`shithub`) is the mirror and runs `release.yml`, because that is where the
release is published. `just release` pushes both — a tag that only reaches
GitLab builds nothing, and the job that publishes never runs.

## Prereleases

A version with a suffix is a prerelease everywhere: `*-*` in the tag turns on
`--prerelease` in the workflow, so GitHub marks it and does not offer it as
"latest". `cargo release` moves between them normally — `0.1.0` after
`0.1.0-rc.1` is a bump, and `0.1.0-rc.2` after `0.1.0-rc.1` is another.

## After the push

```sh
gh run list -R foldu/baash --limit 5
gh run watch -R foldu/baash
```

The run publishes four assets: `baash-<version>-linux-x86_64`,
`baash-<version>-linux-aarch64`, `baash-<version>-windows-x86_64.exe` and
`SHA256SUMS`. Check that all four exist, that the checksums verify, and that an
artifact's `baash --version` names the commit the tag points at — the label is
what makes a bug report about a release actionable.

The release body is generated from the commits. A release worth announcing
wants a hand-written summary on top: `gh release edit v0.1.0 --notes-file …`.
