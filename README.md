# baash — the bourne again again shell

Like [bash](https://www.gnu.org/software/bash/) but not bash. It's a completely
different design, removing the insane POSIX `fork()` support. Of course this
limits some constructs like job control, but `fork()` is design poison. This
envisions a better bash, with a better prompt helped by multithreading,
asynchronous completion (currently carapace only) and other things that aren't
possible with "regular" shell designs.

Wouldn't be possible without the [oil shell](https://oilshell.org), whose test
suite was used extensively to slop up a somewhat working bash clone.

## Features

- Fancy prompt with live git tree status
- No `fork()`: subshells, `$( … )` and shell-code pipeline stages run as
  in-process subinterpreters, not as copies of the shell
- External commands still spawn, through `posix_spawn` rather than `fork`+`exec`
- Works on windows natively
- Builtin [uutils](https://github.com/uutils/coreutils) for less bad windows
  experience
- Mostly bash compatible. Don't expect everything to work though

## Configuration

Configuration is in `~/.config/baash/config.sh`, or
`%APPDATA%\baash\config.sh` on Windows. Every interactive shell reads the
defaults compiled into the binary first, then your file — and if that file does
not exist, `~/.bashrc` is used instead, so an existing bash setup is a starting
point rather than a requirement. `--rcfile` and `--norc` override both.

### Autocomplete

[bash-completion](https://github.com/scop/bash-completion) is embedded on
Windows. On Linux it's not shipped, except with the NixOS module. If
[carapace](https://carapace.sh) is on `PATH`, it will be used for asynchronous
completions; the NixOS module ships it too.

### Recommendations

Use [zoxide](https://github.com/ajeetdsouza/zoxide) and
[carapace](https://carapace.sh). Carapace offers far better asynchronous
completions.

On Windows you can't use zoxide's native shell hook, as it calls `cygpath`.
Instead put this snippet in `%APPDATA%\baash\config.sh`:

```sh
z() {
    local dir
    dir=$(zoxide query --exclude "$PWD" -- "$@") && cd -- "$dir"
}

# The interactive picker, on this shell's own fzf filter rather than fzf.
zi() {
    local dir
    dir=$(zoxide query --list | baashzf) && cd -- "$dir"
}

__zoxide_add() {
    [ "$__zoxide_last" = "$PWD" ] && return
    __zoxide_last=$PWD
    zoxide add -- "$PWD" >/dev/null 2>&1
}

PROMPT_COMMAND="${PROMPT_COMMAND:+$PROMPT_COMMAND; }__zoxide_add"
```

## Installing

A release is one executable per target, published on the
[releases page](https://github.com/foldu/baash/releases), named
`baash-<version>-<triple>`: `x86_64-linux`, `aarch64-linux` and
`x86_64-pc-windows-gnu.exe`. The Linux ones are linked against glibc 2.17 and
need no nix and no shared library beyond libc, so unpacking them is the whole
installation. Or with [mise](https://mise.jdx.dev), which picks the release
asset for the platform it is installing on:

```sh
mise use -g github:foldu/baash
```

### From the flake

The flake builds it, on Linux or cross-compiled for Windows:

```sh
nix run github:foldu/baash                  # run it without installing
nix build github:foldu/baash                # ./result/bin/baash
nix build github:foldu/baash#windows-fast   # baash.exe, no LTO, for iterating
nix build github:foldu/baash#windows        # baash.exe, fat LTO, for a release
```

### NixOS

The flake exports a NixOS module, `nixosModules.default`:

```nix
{
  inputs.baash.url = "github:foldu/baash";

  imports = [ inputs.baash.nixosModules.default ];

  programs.baash = {
    enable = true;
    zoxide.enable = true;
  };

  # if you want it as a login shell
  users.users.<name>.shell = config.programs.baash.package;
}
```

| option                            | default              | what it does                                                                                                                                                                                               |
| --------------------------------- | -------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `programs.baash.enable`           | `false`              | install `baash` and write the startup file                                                                                                                                                                 |
| `programs.baash.package`          | this flake's package | the build to install                                                                                                                                                                                       |
| `programs.baash.bashCompletion`   | `true`               | point `BAASH_BASH_COMPLETION` at `pkgs.bash-completion`, so TAB completion for the machine's commands works — NixOS has no `/usr/share/bash-completion` for [`config/config.sh`](config/config.sh) to find |
| `programs.baash.carapace.enable`  | `true`               | install `carapace`; the shell asks a `carapace` on `PATH` for completions and falls back to its own tables without one                                                                                     |
| `programs.baash.carapace.package` | `pkgs.carapace`      | the carapace to install                                                                                                                                                                                    |
| `programs.baash.zoxide.enable`    | `false`              | install zoxide and run `eval "$(zoxide init bash)"` at startup                                                                                                                                             |
| `programs.baash.zoxide.package`   | `pkgs.zoxide`        | the zoxide to install                                                                                                                                                                                      |
| `programs.baash.extraConfig`      | `""`                 | shell code appended to the system file                                                                                                                                                                     |

## Implementation details

Tree walker evaluator, even if it's called "vm" (it's not). Probably no reason
to make it a real vm except benchmark number going up. Being close to bash
baseline is good enough.

History backed by sqlite via sqlx. Of course this removes HISTFILE but who cares about that.
IO/fs support (most except readdir) via
[compio](https://github.com/compio-rs/compio) (io uring/completion ports).

## Conformance

The [Oils](https://oilshell.org) spec suite is vendored unmodified under
[spec/oils/](spec/README.md) — 223 `*.test.sh` files, 3971 cases, Apache-2.0,
with the upstream commit recorded alongside it.

`crates/baash-spec` runs them. Each case is executed against `baash` and, with
`--compare`, against the reference shells on `PATH`; stdout is compared when a
case declares it (`## STDOUT:`, `## stdout:`, `## stdout-json:`), as is the exit
status, and
per-shell `## OK <shell> STDOUT:` alternatives are accepted for the shell under
test. Cases that declare `## our_shell: ysh`, or that live in the YSH suite, are
skipped rather than counted as passes: baash does not implement YSH and will
not.

That leaves **2787 comparable cases**. The committed baseline is **2357** passes
(84.6%), or **2305** (82.7%) when a per-shell alternative is not allowed to stand
in for the primary expectation — 52 of the passes are that kind. A run on this
machine lands one case lower on each count, which is what the ratchet's tolerance
of five is for. bash 5.3.20 passes 2578 of the same 2787 (92.5%): it passes 295
cases baash fails, and baash passes 73 it fails.

## License

Sadly GPL 3.0 or later, as we looked through the bash source code quite extensively.
All other "relevant" licenses in [THIRD-PARTY.md](THIRD-PARTY.md).
