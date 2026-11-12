# baash — the bourne again again shell

A POSIX- and bash-compatible interactive shell in Rust. The design rationale
lives in [DESIGN.md](DESIGN.md); the current direction is
[docs/direction.md](docs/direction.md); this file is the operator's manual.

**Status: it runs, and conformance is now measured.** `baash` executes scripts,
and `baash` with no arguments gives you an interactive editor with syntax
highlighting, bash-style TAB completion and history.

Conformance is measured against the vendored Oils suite: 3971 cases, of which
1184 are skipped as out of scope (mostly the `ysh` dialect), leaving 2787
comparable.

| | passing | of 2787 |
| --- | ---: | ---: |
| `baash`, any `## OK <shell>` alternative | **2344** | 84.1% |
| `baash`, strict (primary expectation only) | **2292** | 82.2% |
| bash 5.3.20, on the same run | 2577 | 92.5% |

53 of the passing cases pass only because a per-shell `## OK …` alternative
matched, which is the difference between the two `baash` rows. bash passes 464
cases we do not (and we pass 67 it fails), so we pass 84.6% of what bash passes. Both numbers live in
`crates/baash-spec/baseline.txt`, and CI fails if either drops below it.

Conformance is measured but no longer the priority: interactive use comes
first, and [`docs/state.md`](docs/state.md) is the resume document (what
exists, what is proven, what is next). `docs/roadmap.md` is the conformance
queue. Reproduce the numbers with:

```
cargo run -p baash-spec -- --shell target/debug/baash --compare bash
cargo run -p baash-spec -- --shell target/debug/baash --ratchet --strict
```

Do not read the unit-test count as conformance: it is a different number
measuring a different thing.

What works today: expansion (words, `IFS`, `$@`/`$*`, quoting), redirections
including here-documents, pipelines with `pipefail` and `$PIPESTATUS`,
`if`/`for`/`while`/`until`/`case`, functions with `local`, subshells, command
substitution, arithmetic, `[[ ]]` including `=~`, globbing with
`extglob`/`failglob`, indexed and associative arrays, namerefs, `declare`/
`typeset`/`readonly`/`export` with bash's own `-p` layout, the pattern
operators (`${x#pat}`, `${x%pat}`, `${x//a/b}`, `${x@Q}`), traps, `set -e` and
`set -u`, `break`/`continue`/`return`/`exit`, `exec`, background `&` with
`$!`/`wait`/`kill` by pid, history (`history` only — entries are numbered by a global database id in a
Turso database, cleared per session, with no `HIST*` knobs),
history expansion (`!!`, `!n`, `!-n`, `!string`, `!?string?`, `!#`, the word
designators and modifiers, `^old^new^`), and the built-ins listed in
`docs/contracts.md`.

There is **no job control** (see `docs/direction.md`): no `jobs`/`fg`/`bg`/
`disown`, no `%` specifications, no per-job process groups and no terminal
handover, so `Ctrl-Z` stops the child and the shell together, as bash does
with `set +m`. Those four names are still known and report `BAASH-E0302`;
`set -m` is accepted and does nothing.

The interactive prompt is built from the shell's own variables before every
line: `PS1`/`PS2` from an rc file, the environment or a command all take
effect at the next prompt, the escapes read the shell's logical `PWD`,
background children and history ring, `shopt promptvars` runs a second
expansion (so
`PS1='$(date +%H:%M) $ '` works), and `PROMPT_COMMAND` (string or array) runs
before each prompt with its output above it. `Ctrl-R` is
readline-compatible reverse incremental search.

While `PS1` is untouched, the prompt is one tide-like line:

```text
~/sr/gith/fo/nixos-config master ↑23 !2 ?5 $
```

The folder and branch glyphs tide puts in front of those two segments are Nerd
Font private-use codepoints (U+F115 and U+F418), and a font without them draws
a box where the picture should be: they are off by default, and
`PROMPT_ICONS=1` turns them on. The outer path components are shortened to the
shortest prefix that is unique among their siblings, the last component is kept
whole, and `$HOME` is `~`. `↑N` is commits
ahead of the upstream and `↓N` commits behind it, `!N` tracked files with staged
or unstaged changes, and `?N` untracked files; each is omitted at zero, `$` is
`#` for root, and one plain space follows the marker. A command that took
long enough says so on the next prompt — `2.3s`, `1m02s` — dim, before the
marker: 500 ms is the floor, `PROMPT_DURATION=0` turns the segment off and
`PROMPT_DURATION=1` shows it for every command. A command that runs past
`BAASH_NOTIFY` seconds (unset by default) also asks the terminal for a desktop
notification when it finishes, with its duration and exit status but never its
output. The branch is read from
`.git/HEAD` on the prompt path; the counters come from a `gix` status on a
worker (500 ms rate limit, coalesced, a 2 s deadline), so no `git` binary is
needed and a slow repository never delays a prompt
([`docs/prompt-watch.md`](docs/prompt-watch.md)).
`NO_COLOR` keeps the text and drops the ANSI colours. Set `PS1` to get your own
prompt back.

While a line is being typed, a syntax error the parser is sure about — `echo )`,
a `fi` with nothing to close — is underlined in the line and explained on one
row under it, so it is visible before Enter. An unfinished line (`if x; then`, an
open quote) says nothing: it is not an error yet, and a message that flashed on
every keystroke of a block would teach you to ignore it.

History expansion is bash's: the typed line is rewritten before parsing, so
the expanded text is what runs. `!!`, `!$`, `!n`, `!?string?`, `!*`, `!^`, word designators,
`:h :t :r :e :p :q :x :s :gs :&` and `^old^new^` follow the measured 5.3.20
rules (single quotes and `$'…'`
inhibit, double quotes do not, `\` escapes a `!`, an event that does not exist
abandons the line). The ring stores the *expanded* line, as bash does;
`shopt -s histverify` puts an expansion back on the editing line for review
instead of running it; `set +H` (and `set +o history`) turns it off. A `-c`
string and a non-interactive script never expand, even with `set -H`.

TAB completion is refreshed from the shell before every prompt and follows
bash: command position is the deduplicated union of aliases, functions,
builtins, keywords and `PATH` (`fo<TAB>` sees `for`, `ec<TAB>` sees `exec`);
`$HO<TAB>` completes `$HOME` and `${HO<TAB>` completes `${HOME}`; files and
directories are inserted with the quoting that gives the name back (a space, a
`$`, a quote, a newline, non-UTF-8), and a directory ends in `/` without a
trailing space. Programmable completion from `complete` is honoured for `-W`
word lists, the `-A` actions the engine can answer, `-P`/`-S` and
`-o nospace|default|bashdefault|dirnames|plusdirs`; `cd` completes directories.
`complete -F` runs: on Tab the shell calls the function with `COMP_LINE`,
`COMP_POINT`, `COMP_WORDS`, `COMP_CWORD`, `COMP_TYPE` and `COMP_KEY` set and
reads `COMPREPLY` back, so bash completion functions work — output the
function prints is captured and painted above the prompt, where bash puts it
([`docs/contracts.md`](docs/contracts.md)). That includes the
bash-completion library itself: sourcing it registers its `-D` loader and a
command's script is loaded on its first Tab (the loader's status `124` means
"complete again"), so the scripts installed for bash complete here too.
The library can also be **inside the binary**: a build with
`embedded-bash-completion` mounts it in memory at
`/__baash__/bash-completion`, so `source`, `test` and pathname expansion read
it without a single file on disk. That is the default on Windows — where a
bare `baash.exe` is the whole installation and there is no package manager to
install the library — and off elsewhere, where the distribution's copy is the
one in use and the smaller binary is the better default. The tree is not
vendored: [`flake.nix`](flake.nix) copies the pinned nixpkgs
`bash-completion` into `vendor/bash-completion/` before the build (the dev
shell does the same), and `nix build .#bash-completion` is the exact tree.
[`config/config.sh`](config/config.sh) offers the mounted copy after any copy
the system has, so a distribution's newer library still wins;
`BAASH_BASH_COMPLETION` names a library explicitly, `BAASH_EMBEDDED_BASH_COMPLETION`
names the mounted one, and `BAASH_NO_BASH_COMPLETION=1` turns all of it off.
`-C` and `-G` are not run yet.

Not implemented yet: the directory stack (`pushd`/`popd`/`dirs`), `umask`,
`let`, `ulimit`, `times`, `enable`, `$RANDOM`/`$SECONDS`/`$FUNCNAME`, xtrace
(`set -x`), `~user`,
process substitution (`<(...)`, rejected as `BAASH-E0301`), and `complete -C`/
`-G` at the prompt. Missing built-ins
answer status 2 with `BAASH-E0508`; missing special variables expand to nothing.
`printf`, `pwd`, `test` and `[` are served from `PATH` by design.
[`docs/state.md`](docs/state.md) has the full inventory; the interactive
editor's deferred list (vi mode, undo, search highlighting, `SIGWINCH` reflow)
is at the top of `crates/baash-repl/src/lib.rs`.

`baash-vm` is a tree-walking evaluator, and for now it is the only engine: the
bytecode VM is deferred until the bash surface is smaller, and the crate keeps
its name for it (`DESIGN.md` §5, `docs/direction.md` §2). Dispatch is not where
a shell spends its time.

## Layout

```
crates/
  baash-diag       diagnostic IR: spans, stable codes, sources   (no dependencies)
  baash-syntax     lexer, spans, parse outcomes, expectations
  baash-highlight  token + theme -> styled spans, ANSI rendering
  baash-lang       words, quoting, expansion
  baash-runtime    options, variables, the ShellHost interface
  baash-vm         the evaluator
  baash-sys        syscalls, tty detection, fork gate   <- the only `unsafe` crate
  baash-complete   completion candidates and ranking
  baash-repl       line editor and terminal rendering
  baash-tools      dev helpers (stub)
  baash-bench      the `=~` benchmarks (dev only)
  baash            the binary
```

Cross-crate contracts (fork gate, spawn plans, renderers) are in
[docs/contracts.md](docs/contracts.md).

Dependency direction follows DESIGN.md §4.1: `baash-sys` is the only crate
permitted `unsafe`, and everything else carries `#![forbid(unsafe_code)]`.
`baash-repl` depends on `baash-runtime` for exactly one type — the history ring —
so the editor and the `history`/`fc` built-ins share one ring; it still builds
its own in tests and never touches an interpreter.

An interactive shell reads two configuration layers. The first is baash's own
defaults, compiled into the binary: `ls` in colour with clickable OSC 8
hyperlinks, `ll` and `la` for long listings, and a coloured `grep`. A
distributor can replace that layer by installing
`$XDG_CONFIG_DIRS/baash/config.sh` (`/etc/xdg/baash/config.sh`). The second
layer is yours — `$XDG_CONFIG_HOME/baash/config.sh`, normally
`~/.config/baash/config.sh`, or `~/.bashrc` when you have none — and it is
sourced last, so an alias defined there overrides the default instead of
having to copy it. `--rcfile`/`--norc` still win. History goes to
`$XDG_DATA_HOME/baash/history.db` (`~/.local/share/baash/history.db`) rather
than a `HISTFILE`, so two shells share one database instead of rewriting each
other's history.

## Building

```sh
nix develop          # cargo, rustc, clippy, rustfmt, rust-analyzer
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- --deny warnings

cargo run -p baash -- -c 'echo hi'     # run a command
cargo run -p baash                     # interactive (on a terminal)
cargo run -p baash -- script.sh        # run a file
cargo run -p baash -- -n script.sh     # parse only, report diagnostics
printf 'if x; then\n' | cargo run -p baash -- --highlight   # paint stdin
```

Use `--all-features` for both. `baash-diag`'s `miette` adapter is behind the
`fancy` feature and is invisible to a default-feature build, so a count without
the flag is not the project's test count.

or without a dev shell:

```sh
cargo build --workspace
nix build .#default
```

`nix build .#default` installs more than the binary: `bin/baash`, the default
`etc/xdg/baash/config.sh`, and the artwork in freedesktop's icon theme layout —
`share/icons/hicolor/scalable/apps/baash.svg` plus a PNG at every size from 16
to 256 px. There is deliberately no `.desktop` file: a shell is not a GUI
program, and an entry that started `baash` without a terminal would fail. The
icon sits where a desktop entry would look for it, waiting for something to
name it.

`just` wraps the loops the pipeline runs:

```sh
just check         # nix flake check: build, fmt, clippy, licenses, windows, module
just test          # the full suite, on a real pty
just conformance   # the ratchet
just licenses      # cargo deny check licenses
just build         # result/bin/baash
just windows       # result/bin/baash.exe
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

## NixOS

The flake exports a NixOS module, `nixosModules.default`:

```nix
{
  inputs.baash.url = "git+https://lab.home.5kw.li/foldu/baash.git";

  imports = [ inputs.baash.nixosModules.default ];

  programs.baash = {
    enable = true;
    zoxide.enable = true;
  };
}
```

It installs the package, adds it to `environment.shells` — `/etc/shells` is what
`chsh` and PAM consult before they let a shell be someone's login shell — and
writes `/etc/xdg/baash/config.sh`, the one startup file a distribution owns. To
make it the login shell as well,
`users.users.<name>.shell = config.programs.baash.package;` — the package carries
a `shellPath`, so this resolves to the same `/run/current-system/sw/bin/baash`
that `/etc/shells` lists and PAM compares against.

The generated file starts by sourcing the copy its own package installed,
because a system config **replaces** the defaults built into the binary rather
than extending them: without that line, enabling the module would drop the `ls`
alias and the completion loader. What the options add goes after the defaults
and before the user's `~/.config/baash/config.sh`.

| option | default | what it does |
|---|---|---|
| `programs.baash.enable` | `false` | install `baash` and write the startup file |
| `programs.baash.package` | this flake's package | the build to install |
| `programs.baash.bashCompletion` | `true` | point `BAASH_BASH_COMPLETION` at `pkgs.bash-completion`, so TAB completion for the machine's commands works — NixOS has no `/usr/share/bash-completion` for [`config/config.sh`](config/config.sh) to find |
| `programs.baash.carapace.enable` | `true` | install `carapace`; the shell asks a `carapace` on `PATH` for completions and falls back to its own tables without one |
| `programs.baash.carapace.package` | `pkgs.carapace` | the carapace to install |
| `programs.baash.zoxide.enable` | `false` | install zoxide and run `eval "$(zoxide init bash)"` at startup |
| `programs.baash.zoxide.package` | `pkgs.zoxide` | the zoxide to install |
| `programs.baash.extraConfig` | `""` | shell code appended to the system file |

This machine's library is the flake's pinned `bash-completion`; a
`BAASH_BASH_COMPLETION` already in the environment is left alone, and
`BAASH_NO_BASH_COMPLETION=1` still turns completion off inside the shell.

`nix/check-module.nix` is the gate (`nix flake check` builds it as
`checks.<system>.module`). It evaluates a system three times — both options on,
`bashCompletion = false`, and the module imported but not enabled — copies the
generated files into `$XDG_CONFIG_DIRS` trees, and runs the shell that was
installed against them, so the completion framework, a real per-command script
from the library, zoxide's hook and the shell's own `ls` alias are asserted
rather than assumed. What it does not cover is a real `/etc` and PAM.

## CI

GitLab CI on `lab.home.5kw.li` is the source of truth; the repository is
mirrored to GitHub, which runs a lighter copy of the same gates.

Every GitLab job is tagged `nix`, because everything runs through the flake and
the untagged runner has no nix daemon. The pipeline is:

* `nix flake check` — the release build, `cargo fmt`, `cargo clippy`, the
  license gate and the Windows executable are all derivations (`flake.nix`), so
  the job is one command. Nothing in it knows what a compiler needs on `PATH`.
* `cargo test --workspace --all-features`.
* The conformance ratchet.

The last two are jobs rather than checks for reasons the sandbox dictates: the
pty-driven `interactive_*` tests need a real `/dev/pts` (in a build sandbox
`openpty` succeeds and reads hang), and 18 conformance cases read the machine —
hostname, tzdata, effective uid, wall-clock timers — so a sandbox measures 2326
where a real environment measures 2344.

GitHub Actions (`.github/workflows/ci.yml`) mirrors the same three gates with a
rustup toolchain instead of nix, which is enough because the build has no C
dependencies. It is the second opinion, not the ratchet of record.

## Releases

A tag is a version: `vX.Y.Z`, and it must match the `baash` version in
`Cargo.toml` — the release workflow refuses a hand-made tag that disagrees.

```sh
just release 0.2.0      # or: just release patch
```

runs the gates (`check`, `test`, `conformance`), then bumps, commits, tags
`v0.2.0` and pushes. The tag builds both binaries from the
flake and publishes a GitHub release containing

```
baash-0.2.0-x86_64-linux.tar.gz
baash-0.2.0-x86_64-pc-windows-gnu.zip
SHA256SUMS
```

each archive holding `bin/baash` (or `bin/baash.exe`), the default
`share/baash/config.sh`, `LICENSE` and `README.md`; the Linux archive also
carries the icon theme tree under `share/icons/hicolor/`, so extracting it into
`~/.local` puts the artwork where the desktop looks. A tag with a suffix
(`v0.2.0-rc.1`) publishes as a prerelease. The tooling is `cargo-release`
configured in `release.toml` and driven by the `justfile`.

## Conformance

The [Oils](https://oilshell.org) spec suite is vendored unmodified under
[spec/oils/](spec/README.md) — 223 files, 3971 cases, Apache-2.0, with the
upstream commit recorded alongside it. The harness is not written yet; it will
run each case against `baash` and against the reference shells on `PATH`, honour
the per-shell override blocks, and report a pass count per file. That count is
the project's only real progress metric (DESIGN.md §8).

## License

GNU GPL, version 3 or later — the same license as bash, and deliberately so:
bash's implementation was read closely enough to count, which rules out a
permissive claim (and the two licenses could not be combined anyway). See
[LICENSE](LICENSE).

    baash is free software: you can redistribute it and/or modify it under the
    terms of the GNU General Public License as published by the Free Software
    Foundation, either version 3 of the License, or (at your option) any later
    version.

    baash is distributed in the hope that it will be useful, but WITHOUT ANY
    WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR
    A PARTICULAR PURPOSE.  See the GNU General Public License for more details.

Compiled-in and vendored third-party work keeps its own license: the uutils
subcrates embedded in `baash-utils` are MIT, and the vendored Oils spec suite is
Apache-2.0. [THIRD-PARTY.md](THIRD-PARTY.md) has the notices.
