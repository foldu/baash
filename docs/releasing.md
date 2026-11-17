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

## Keeping the log

A release is the one command whose output is worth keeping: the gate prints
pages of cargo and nix output, `cargo release` prints the plan, and the two
pushes each have their own. `just release-logged <version>` runs the same
recipe with the whole session written down:

```sh
just release-logged 0.1.2
```

```text
dist/release-logs/0.1.2-20261117T124000Z/
    transcript.log   what the terminal saw, in order: ANSI, prompts, everything
    output.txt       the same, colour stripped and CR turned into line breaks
    meta.txt         HEAD, branch, remotes, dirty files, tool versions
    result.txt       exit status, duration, and what to check next
dist/release-logs/latest -> that directory
```

It is `script(1)` around `just release`, so the shell keeps its terminal: a
push that asks for a passphrase asks *you* from the same pty, and `Ctrl-C`
stops where it would have stopped anyway. `transcript.log` is flushed as it
goes, so a dropped ssh connection or a killed run still leaves the record. The
directory is under `dist/`, which is ignored, so nothing lands in the tree.

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

The run publishes four assets, named `baash-<version>-<triple>`, where the
triple is the workflow matrix's: `x86_64-linux`, `aarch64-linux`,
`x86_64-pc-windows-gnu.exe`, plus `SHA256SUMS`. Check that all four exist, that the checksums verify, and that an
artifact's `baash --version` names the commit the tag points at — the label is
what makes a bug report about a release actionable.

The release body is generated from the commits. A release worth announcing
wants a hand-written summary on top: `gh release edit v0.1.0 --notes-file …`.

## When the push is refused

`git push` can fail for reasons that have nothing to do with the release, and
the tag is still local when it does — `git tag -l` answers, and nothing was
published. The two that have happened:

* **`Bad owner or permissions on …ssh_config.d/…`, `Could not read from remote
  repository`.** A sandboxed terminal maps the user differently, so OpenSSH
  refuses the system config's root-owned `Include`, and the ambient
  `SSH_AUTH_SOCK` is not the agent that holds the key. Name an `ssh` that works:

  ```sh
  GIT_SSH_COMMAND=$HOME/.dsh/bin/fwd-ssh git push shithub master --follow-tags
  ```

  Both remotes accept that command; it is the same history, and it is what the
  recipe's last line runs when the variable is set.
* **`/etc/bashrc: … unbound variable`** after a recipe line is shell startup
  noise from the environment, not a failed command: look at the line's own
  output.

A release commit and tag are cheap to correct while they are local: delete the
tag (`git tag -d v0.1.0-rc.1`), undo the commit (`git reset --hard HEAD~1`), fix,
and run the recipe again. Once the tag is pushed, cut `-rc.2` instead.
