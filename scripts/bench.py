#!/usr/bin/env python3
"""Microbenchmark: bsh against bash on small command scripts.

This is the harness behind docs/performance.md. It is deliberately tiny and has
no dependencies: each case is one `-c` script, run N times, and the *minimum*
wall time is kept (the least noisy estimator for CPU-bound work).

    python3 scripts/bench.py                       # release build if there is one
    python3 scripts/bench.py --shell target/debug/bsh
    python3 scripts/bench.py --bash /bin/bash --reps 7
    python3 scripts/bench.py --filter expansion --json

Use a release build for any number you intend to quote: a debug build is
14-25x slower than bash on interpreted work purely because it is not
optimised, which says nothing about the design.

The comparison shell is run with `--norc --noprofile` and both run under
`LC_ALL=C`, matching the conformance harness.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# (name, script, repetitions). Loop counts are chosen so a release build takes
# a few tens of milliseconds per run: larger and the table takes minutes,
# smaller and process-start noise dominates.
CASES: list[tuple[str, str, int]] = [
    ("arith loop 20k ((i++))", "i=0; while (( i < 20000 )); do (( i++ )); done", 3),
    (
        "expansion 20k ${s#*c}",
        "s=abcdefghij; i=0; while (( i < 20000 )); do x=${s#*c}; (( i++ )); done",
        3,
    ),
    (
        "expansion 20k ${s//d/e}",
        "s=abcdefghij; i=0; while (( i < 20000 )); do x=${s//d/e}; (( i++ )); done",
        3,
    ),
    (
        "function call 20k",
        "f() { :; }; i=0; while (( i < 20000 )); do f; (( i++ )); done",
        3,
    ),
    (
        "case 20k",
        "x=abc; i=0; while (( i < 20000 )); do case $x in a*) ;; esac; (( i++ )); done",
        3,
    ),
    (
        "builtin echo 20k",
        "i=0; while (( i < 20000 )); do echo -n ''; (( i++ )); done",
        3,
    ),
    # `env true`, not `/bin/true`: on NixOS /bin has no `true`, and a missing
    # program makes the case measure error handling instead of fork+exec.
    ("fork+exec 200 (env true)", "i=0; while (( i < 200 )); do env true; (( i++ )); done", 5),
    (
        "pipeline 200 (echo | cat)",
        "i=0; while (( i < 200 )); do echo x | cat >/dev/null; (( i++ )); done",
        5,
    ),
    (
        "cmd subst 1000 $(echo hi)",
        "i=0; while (( i < 1000 )); do x=$(echo hi); (( i++ )); done",
        5,
    ),
    ("subshell 1000 ( : )", "i=0; while (( i < 1000 )); do ( : ); (( i++ )); done", 5),
    # A scope clones the shell state, so a large array in scope is the one case
    # where the clone is visible (`docs/direction.md` §9).
    (
        "subshell 200, 20k-element array",
        "arr=( $(seq 20000) ); i=0; while (( i < 200 )); do ( : ); (( i++ )); done",
        3,
    ),
]

ENV = dict(os.environ, LC_ALL="C", LANG="C")


def default_shell() -> Path | None:
    """The built shell, preferring a release build over a debug one."""
    # `CARGO_TARGET_DIR` moves the binaries out of the checkout; honour it so
    # this script works from a normal and from a redirected build.
    roots = []
    if os.environ.get("CARGO_TARGET_DIR"):
        roots.append(Path(os.environ["CARGO_TARGET_DIR"]))
    roots.append(REPO / "target")
    for root in roots:
        for profile in ("release", "debug"):
            candidate = root / profile / "bsh"
            if candidate.is_file():
                return candidate
    return None


def run_once(shell: Path, script: str, is_bash: bool, reps: int) -> float:
    command = [str(shell)]
    if is_bash:
        command += ["--norc", "--noprofile"]
    command += ["-c", script]
    best = float("inf")
    for _ in range(reps):
        started = time.perf_counter()
        subprocess.run(
            command,
            env=ENV,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        best = min(best, time.perf_counter() - started)
    return best


def startup(shell: Path, is_bash: bool, spawns: int = 100) -> float:
    """Seconds per process start.

    One `subprocess.run` costs more than either shell's startup, so the spawns
    are counted inside a single `/bin/sh` loop and divided.
    """
    flags = " --norc --noprofile" if is_bash else ""
    script = (
        f"i=0; while [ $i -lt {spawns} ]; do {shell}{flags} -c :; i=$((i+1)); done"
    )
    started = time.perf_counter()
    subprocess.run(
        ["/bin/sh", "-c", script],
        env=ENV,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    return (time.perf_counter() - started) / spawns


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--shell", type=Path, help="bsh binary (default: target/{release,debug}/bsh)")
    parser.add_argument("--bash", type=Path, help="comparison shell (default: bash on PATH)")
    parser.add_argument("--reps", type=int, default=None, help="override repetitions per case")
    parser.add_argument("--filter", default="", help="only run cases whose name contains this")
    parser.add_argument("--json", action="store_true", help="machine-readable output")
    args = parser.parse_args()

    shell = args.shell or default_shell()
    if shell is None or not shell.is_file():
        print(
            "bsh: no binary found; build one first, e.g. cargo build --release -p bsh",
            file=sys.stderr,
        )
        return 2
    bash = args.bash or (Path(shutil.which("bash")) if shutil.which("bash") else None)
    if bash is None or not Path(bash).is_file():
        print("bsh: no bash to compare against (use --bash)", file=sys.stderr)
        return 2

    if "debug" in str(shell):
        print(
            "note: this looks like a debug build; expect 14-25x bash on interpreted work",
            file=sys.stderr,
        )

    rows = []
    bash_startup = startup(bash, True)
    shell_startup = startup(shell, False)
    rows.append(
        {
            "case": "startup, per spawn",
            "bash": bash_startup,
            "bsh": shell_startup,
        }
    )
    for name, script, reps in CASES:
        if args.filter and args.filter not in name:
            continue
        reps = args.reps or reps
        rows.append(
            {
                "case": name,
                "bash": run_once(bash, script, True, reps),
                "bsh": run_once(shell, script, False, reps),
            }
        )

    if args.json:
        print(json.dumps({"shell": str(shell), "bash": str(bash), "rows": rows}, indent=2))
        return 0

    print(f"bash = {bash}")
    print(f"bsh  = {shell}")
    print(f"{'case':34} {'bash ms':>9} {'bsh ms':>10} {'ratio':>7}")
    print("-" * 64)
    for row in rows:
        ratio = row["bsh"] / row["bash"] if row["bash"] else float("inf")
        print(
            f"{row['case']:34} {row['bash'] * 1000:9.2f} "
            f"{row['bsh'] * 1000:10.2f} {ratio:6.1f}x"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
