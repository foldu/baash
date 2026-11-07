#!/usr/bin/env python3
"""Parse a corpus of shell scripts with `baash -n` and report what breaks.

This is the harness behind the "parse everything installed" check: point it at
a store closure (not the whole store) and it walks the paths, keeps the files
that claim to be shell scripts, and runs the parser over each one.

    python3 scripts/parse_corpus.py                       # system + user profile
    python3 scripts/parse_corpus.py /nix/store/...-hello  # one or more roots
    python3 scripts/parse_corpus.py --json report.json --jobs 16

A file counts as a shell script when its name ends in .sh/.bash/.bats, or when
its shebang names a POSIX/bash-family interpreter. Symlinks and files the
parser cannot see as text are skipped; the summary records both. Exit status
is 0 unless --strict is given and something failed to parse.

The parser is run as a real subprocess per file, exactly as `baash -n` runs
for a user, so startup and diagnostics are part of what is measured.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
import re
import stat
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
DEFAULT_SHELLS = ("sh", "bash", "dash", "ash", "ksh", "ksh93", "mksh", "pdksh", "posh", "yash")
SHELL_EXTENSIONS = {".sh", ".bash", ".bats"}
# A real script this large is a generated blob and not useful as a parser case;
# the cap also keeps the worker pool from holding hundreds of megabytes.
DEFAULT_MAX_BYTES = 8 * 1024 * 1024
CODE_RE = re.compile(r"error\[([A-Za-z0-9_.-]+)\]")
# Shell/Tcl/Perl polyglots keep a tiny shell preamble whose only job is to
# exec the real interpreter on the same file; the rest is not shell at all.
POLYGLOT_RE = re.compile(
    rb"^[ \t]*exec[ \t]+['\"]?(?:wish|tclsh|perl|python[0-9.]*|ruby)\b[^\n]*\$(?:0|argv0)\b",
    re.MULTILINE,
)
# `#!/usr/bin/env -S bash -eu` and friends: skip env's own options.
ENV_OPTIONS_WITH_VALUE = {"-u", "--unset", "-C", "--chdir", "-P", "--default-path"}


def interpreter(path: str, head: bytes) -> str | None:
    """Return the shell named by a shebang, or None if it is not one."""
    if not head.startswith(b"#!"):
        return None
    line = head.split(b"\n", 1)[0][2:].strip()
    try:
        words = line.decode("utf-8", "replace").split()
    except ValueError:  # pragma: no cover - split() does not raise
        return None
    if not words:
        return None
    program = os.path.basename(words[0])
    rest = words[1:]
    if program == "env":
        # env [OPTION]... [NAME=VALUE]... COMMAND [ARG]...
        index = 0
        split = False
        while index < len(rest):
            word = rest[index]
            if word == "-S" or word == "--split-string":
                split = True
                index += 1
                continue
            if word.startswith("-"):
                index += 2 if word in ENV_OPTIONS_WITH_VALUE else 1
                continue
            if "=" in word:
                index += 1
                continue
            break
        if index >= len(rest):
            return None
        program = os.path.basename(rest[index])
        if split:
            # -S means the remainder is one command line, already split above.
            pass
        rest = rest[index + 1 :]
    if program.endswith(".exe"):
        program = program[:-4]
    # busybox sh, toybox sh: the shell is an argument to a multiplexer.
    if program in {"busybox", "toybox"} and rest and rest[0] == "sh":
        return "sh"
    return program


def looks_like_script(name: str, head: bytes) -> tuple[bool, str]:
    """Decide whether to parse this file, and record why.

    Returns (kind, interpreter). kind is "extension", "shebang", or "".
    """
    suffix = Path(name).suffix.lower()
    shell = interpreter(name, head)
    if suffix in SHELL_EXTENSIONS:
        # A .sh with an explicit foreign shebang is somebody else's script.
        if shell is not None and shell not in DEFAULT_SHELLS:
            return False, shell
        return True, shell or ""
    if shell in DEFAULT_SHELLS:
        return True, shell
    return False, shell or ""


@dataclass
class Corpus:
    roots: list[str] = field(default_factory=list)
    files: list[str] = field(default_factory=list)
    skipped: dict[str, int] = field(default_factory=lambda: {
        "symlink": 0,
        "unreadable": 0,
        "binary": 0,
        "too_large": 0,
        "polyglot": 0,
        "not_shell": 0,
        "non_regular": 0,
    })
    by_interpreter: dict[str, int] = field(default_factory=dict)
    by_extension: dict[str, int] = field(default_factory=dict)


def scan(roots: list[str], max_bytes: int) -> Corpus:
    corpus = Corpus(roots=roots)
    seen: set[tuple[int, int]] = set()
    stack = list(roots)
    while stack:
        root = stack.pop()
        try:
            entries = os.scandir(root)
        except OSError:
            corpus.skipped["unreadable"] += 1
            continue
        with entries:
            for entry in entries:
                try:
                    info = entry.stat(follow_symlinks=False)
                except OSError:
                    corpus.skipped["unreadable"] += 1
                    continue
                if stat.S_ISLNK(info.st_mode):
                    corpus.skipped["symlink"] += 1
                    continue
                if stat.S_ISDIR(info.st_mode):
                    stack.append(entry.path)
                    continue
                if not stat.S_ISREG(info.st_mode):
                    corpus.skipped["non_regular"] += 1
                    continue
                key = (info.st_dev, info.st_ino)
                if key in seen:
                    continue
                seen.add(key)
                if info.st_size > max_bytes:
                    corpus.skipped["too_large"] += 1
                    continue
                try:
                    with open(entry.path, "rb") as stream:
                        head = stream.read(4096)
                except OSError:
                    corpus.skipped["unreadable"] += 1
                    continue
                if b"\0" in head:
                    corpus.skipped["binary"] += 1
                    continue
                wanted, shell = looks_like_script(entry.name, head)
                if not wanted:
                    corpus.skipped["not_shell"] += 1
                    continue
                if POLYGLOT_RE.search(head):
                    corpus.skipped["polyglot"] += 1
                    continue
                corpus.files.append(entry.path)
                label = shell or "(no shebang)"
                corpus.by_interpreter[label] = corpus.by_interpreter.get(label, 0) + 1
                suffix = Path(entry.name).suffix.lower() or "(none)"
                corpus.by_extension[suffix] = corpus.by_extension.get(suffix, 0) + 1
    corpus.files.sort()
    return corpus


@dataclass
class Result:
    path: str
    status: str  # ok | failed | incomplete | timeout | error
    code: str = ""
    message: str = ""
    stderr: str = ""
    rc: int | None = None
    elapsed_ms: float = 0.0


def parse_one(shell: str, path: str, timeout: float) -> Result:
    started = time.monotonic()
    env = dict(os.environ)
    env["LC_ALL"] = "C"
    env.pop("BASH_ENV", None)
    env.pop("ENV", None)
    try:
        done = subprocess.run(
            [shell, "-n", path],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            timeout=timeout,
            env=env,
        )
    except subprocess.TimeoutExpired:
        return Result(path, "timeout", elapsed_ms=(time.monotonic() - started) * 1000)
    except OSError as error:
        return Result(path, "error", message=str(error))
    elapsed = (time.monotonic() - started) * 1000
    text = done.stderr.decode("utf-8", "replace")
    code = ""
    match = CODE_RE.search(text)
    if match:
        code = match.group(1)
    if done.returncode == 0:
        return Result(path, "ok", elapsed_ms=elapsed)
    status = "incomplete" if code == "BAASH-E0002" else "failed"
    # The report is multi-line and repeats the path; keep the first two lines.
    lines = [line for line in text.splitlines() if line.strip()]
    message = lines[0] if lines else f"exit status {done.returncode}"
    return Result(path, status, code=code, message=message, stderr=text, rc=done.returncode, elapsed_ms=elapsed)


def format_summary(corpus: Corpus, results: list[Result], elapsed: float) -> str:
    counts: dict[str, int] = {}
    codes: dict[str, int] = {}
    messages: dict[str, int] = {}
    for result in results:
        counts[result.status] = counts.get(result.status, 0) + 1
        if result.code:
            codes[result.code] = codes.get(result.code, 0) + 1
        if result.status not in {"ok"} and result.message:
            messages[result.message] = messages.get(result.message, 0) + 1
    lines = [
        f"roots: {len(corpus.roots)}",
        f"scripts: {len(corpus.files)}",
    ]
    ordered = ["ok", "failed", "incomplete", "timeout", "error"]
    lines.append("results: " + "  ".join(f"{name}={counts.get(name, 0)}" for name in ordered))
    lines.append("skipped: " + "  ".join(f"{name}={count}" for name, count in sorted(corpus.skipped.items()) if count))
    lines.append("interpreters: " + "  ".join(f"{name}={count}" for name, count in sorted(corpus.by_interpreter.items())))
    if codes:
        lines.append("diagnostics: " + "  ".join(f"{name}={count}" for name, count in sorted(codes.items())))
    if messages:
        lines.append("top messages:")
        for message, count in sorted(messages.items(), key=lambda item: (-item[1], item[0]))[:20]:
            lines.append(f"  {count:6d}  {message}")
    lines.append(f"elapsed: {elapsed:.1f}s")
    return "\n".join(lines)


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("roots", nargs="*", help="directories (or store paths) to walk")
    parser.add_argument("--shell", default=str(REPO / "target/debug/baash"), help="baash binary")
    parser.add_argument("--jobs", type=int, default=os.cpu_count() or 4, help="parallel parses")
    parser.add_argument("--timeout", type=float, default=15.0, help="per-file timeout in seconds")
    parser.add_argument("--max-bytes", type=int, default=DEFAULT_MAX_BYTES)
    parser.add_argument("--json", metavar="PATH", help="write the full report here")
    parser.add_argument("--strict", action="store_true", help="exit 1 when any file fails to parse")
    parser.add_argument("--quiet", action="store_true")
    args = parser.parse_args(argv)

    roots = args.roots
    if not roots:
        out = subprocess.run(
            ["nix-store", "-qR", "/run/current-system"],
            capture_output=True, text=True, check=True,
        ).stdout.split()
        roots = out
        profile = Path.home() / ".nix-profile"
        if profile.exists():
            roots += subprocess.run(
                ["nix-store", "-qR", str(profile)],
                capture_output=True, text=True, check=True,
            ).stdout.split()
        # A closure lists the same path once, but keep the invariant explicit.
        roots = sorted(set(roots))

    if not Path(args.shell).exists():
        print(f"parse_corpus: no such shell: {args.shell}", file=sys.stderr)
        return 2

    started = time.monotonic()
    corpus = scan(roots, args.max_bytes)
    if not args.quiet:
        print(f"scan: {len(corpus.files)} scripts from {len(roots)} roots", file=sys.stderr)

    results: list[Result] = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.jobs)) as pool:
        futures = [pool.submit(parse_one, args.shell, path, args.timeout) for path in corpus.files]
        for index, future in enumerate(concurrent.futures.as_completed(futures), 1):
            results.append(future.result())
            if not args.quiet and index % 1000 == 0:
                print(f"  {index}/{len(corpus.files)}", file=sys.stderr)
    results.sort(key=lambda result: result.path)
    elapsed = time.monotonic() - started

    summary = format_summary(corpus, results, elapsed)
    print(summary)
    failures = [result for result in results if result.status not in {"ok"}]
    if failures:
        print(f"\nfailures ({len(failures)}):")
        for result in failures:
            print(f"  {result.status:10s} {result.code or '-':12s} {result.path}")
            if result.message:
                print(f"             {result.message}")

    if args.json:
        report = {
            "roots": corpus.roots,
            "summary": {
                "scripts": len(corpus.files),
                "status": {name: sum(1 for r in results if r.status == name) for name in ("ok", "failed", "incomplete", "timeout", "error")},
                "skipped": corpus.skipped,
                "interpreters": corpus.by_interpreter,
                "extensions": corpus.by_extension,
                "elapsed_seconds": round(elapsed, 3),
            },
            "results": [
                {
                    "path": r.path,
                    "status": r.status,
                    "code": r.code,
                    "message": r.message,
                    "stderr": r.stderr,
                    "returncode": r.rc,
                    "elapsed_ms": round(r.elapsed_ms, 3),
                }
                for r in results
            ],
        }
        Path(args.json).write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n")

    return 1 if args.strict and failures else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
