# The Windows protocol

Windows is the part of this tree that a Linux `cargo` never sees: code under
`cfg(windows)` is compiled by no Linux gate, and the behaviour that only exists
there — the console, the line editor, keys, the prompt — has no Linux
equivalent at all. So it gets its own protocol.

What Windows *does* and does not do yet is
[portability.md](portability.md); this file is how to work on it.

## Compile it

The cheapest check is the cross-target clippy:

```sh
cargo clippy --target x86_64-pc-windows-gnu -p baash -- -D warnings
```

It needs the target installed (`rustup target add x86_64-pc-windows-gnu`;
the flake's `windows` derivation pins a toolchain that has it) and a resource
compiler for the exe's icon, which `build.rs` runs even for a check — set
`BAASH_WINDRES` to `llvm-windres`, or put `windres` on `PATH`. `--all-targets`
does not work for it: the pty tests and `baash-spec` are Unix-only by design.

## Build it

Two builds of the same program. [portability.md](portability.md) §10 has the
profiles, the four derivations and the timings; this is the part that matters
day to day.

| build | for | after an edit |
|---|---|---|
| `just windows` | CI, the guest, a release: fat LTO, cgu 1, stripped, 20.6 MB | ~80 s |
| `just windows-fast` | editing: the same profile, LTO off, 29.7 MB | ~15 s |

`flake.nix`'s `mkWindows` is the source of truth for the flags: a `windres`
shim on `PATH` with `BAASH_WINDRES` at LLVM's `windres`, and
`-Wno-error=date-time` for `libmimalloc-sys`.

Both are a cached dependency half plus a thin per-change half, so an edit under
`crates/*/src` recompiles the workspace and nothing else. That holds only while
the two halves are configured identically: `nix eval .#windows-deps.drvPath`
must not move when only `crates/*/src` changed, and if it does, the halves have
drifted.

## The exe icon

Explorer, the taskbar, Alt-Tab and the console window's title read a program's
icon from a PE resource, never from the code: the shell asks the loader for
`RT_GROUP_ICON`, which names one `RT_ICON` per size. So the sheep is built into
the image rather than drawn by it.

| file | what it is |
|---|---|
| `assets/logo.svg` | the mascot; the source of truth, and what the icon is built from |
| `assets/logo-dark.svg` | the same mark with a cream keyline on the ears, for dark surfaces; not used by the icon |
| `assets/baash.ico` | the same drawing at 16, 24, 32, 48, 64, 128 and 256 px |
| `crates/baash/baash.rc` | one `1 ICON "baash.ico"` statement |
| `crates/baash/build.rs` | compiles it for a Windows target, and only then |

`build.rs` finds the resource compiler in `BAASH_WINDRES` — the flake's
`prepare` phase already exports LLVM's `llvm-windres` there for
`turso_sdk_kit` — then falls back to `windres` on `PATH` (the `ci/windres`
shim) and to `llvm-windres`. It compiles the script to a COFF object under
`OUT_DIR` and hands the binary target one `cargo:rustc-link-arg-bins`, so the
object is linked into `baash.exe` and nowhere else. A non-Windows build returns
before any of that: no Linux toolchain grows a resource compiler requirement.

After editing the logo, regenerate both files:

```sh
magick -background none assets/logo.svg \
    -define icon:auto-resize=256,128,64,48,32,24,16 assets/baash.ico
```

Verify it in the image, not in the source: `inspect_pe_resources` on the built
`baash.exe` lists the `RT_GROUP_ICON` (type 14) and its seven `RT_ICON`
(type 3) entries with their declared dimensions, and each group member's
`declared_bytes` should equal the frame it names. A 256 px entry reports
width 0, which is the ICO convention, not a bug.

The artwork is drawn for 32 px and up. At 16 px the face is a dark blob with
two pale dots: legible as "something with eyes", not as a sheep. A dedicated
16 px cut would be a second drawing; none is checked in.

## Run it

On the guest, not Wine. Wine can run `-c` commands, but it has no usable
console, so the interactive path only exists on Windows. The guest is the
`winvm` Windows 11 domain — `winvm reset` first — and the exe goes to
`C:\build`.

The exe cannot be replaced while it runs: end the task and kill the session-1
`cmd`/`conhost` first, or the copy fails with a sharing violation.

**A GUI run needs a console session.** A process started over ssh runs in
session 0 with no window, so use a scheduled task with `/it`:

```bat
schtasks /create /tn <name> /tr '"cmd /k C:\build\baash.exe"' /sc once /st 23:59 /it /f
schtasks /run /tn <name>
```

`cmd /k` keeps the window after the shell exits. Screenshot it with
`virsh --connect qemu:///system screenshot win /tmp/x.png`.

Observed on Windows 11 26300.9711, with Windows Terminal as the console host:
the prompt renders with its icons and colours, the tab title follows the
directory (OSC 0/1), and duplicating the tab with `Ctrl+Shift+D` starts
`cmd.exe` in the shell's directory, so the cwd report is consumed. The
unambiguous-keyboard requests are not honoured by that host — `Ctrl-M` still
submits and `Ctrl-Left` arrives as a plain Left — so only the legacy
encodings reach the editor.

## Facts that have already cost a session

* **Quote Windows paths in baash, because `\` is an escape.** `cd C:\Windows`
  reaches `cd` as `C:Windows`: write `cd C:/Windows` or `cd 'C:\Windows'`. The
  msys convention, deliberate.
* Environment names are case-insensitive (`Path` vs `PATH`); a path list is
  `;`-separated; a bare name is resolved through `PATHEXT`.
* `GetConsoleMode` needs *read* access to the handle.
* There is no `HOME`; `%USERPROFILE%` stands in.
* A root is a drive or a UNC share, and `..` stops there (`baash_sys::paths`).
* **Names are WTF-8, and every conversion must go through the codec.**
  `baash_sys::wtf8` carries a Windows name — including an unpaired surrogate —
  through shell bytes exactly, so `chdir`, `CreateProcessW`, the CRT open and
  the environment block all use it (`os::bytes_from_os`/`os::os_from_bytes`).
  A new `from_utf8_lossy` on a path is a bug: display may be lossy, the name
  may not.
