#!/usr/bin/env python3
"""A/B two baash builds on one script, with the protocol that makes it readable.

    python3 scripts/ab.py --a /tmp/baash-before --b /tmp/baash-after \
        --case 'interpreter loop 100k'
    python3 scripts/ab.py --a old --b new --script 'i=0; while (( i < 100000 )); do :; (( i++ )); done'

Why this is not three lines of `for` around `time`:

* **The file a binary lives in matters.** Two byte-identical copies measured
  3.5% apart on this host — one written while the filesystem was busy, one
  written after — which is larger than the effects these tables resolve. So
  neither side keeps its own path: each run copies its build over one shared
  path first, and the copy's first (cold-page) run is discarded. The control in
  `docs/performance.md` is that experiment: run this tool with `--a` twice.
* **Position matters.** With plain A,B,A,B alternation the second run of every
  pair was 1.5-3.5% slower even for identical binaries, so each repetition runs
  A,B,B,A: whatever the first slot loses, the other side loses in the fourth.
  That halves the bias; the shared path takes care of most of the rest.
* **The minimum is the estimator.** These are CPU-bound scripts of tens of
  milliseconds; the fastest run is the least polluted by a noisy neighbour, and
  the median mostly measures the machine.

What is left after that is a noise floor around 1%, which is why a change of
that size wants the deterministic evidence too: `valgrind --tool=callgrind`
counts instructions and does not care about frequency, layout or neighbours
(`docs/performance.md`, "the option table stops being compared as bytes").
"""

from __future__ import annotations

import argparse
import importlib.util
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

# Importing `bench.py` for its case list must not leave a `scripts/__pycache__`
# behind in a checkout that is about to be committed.
sys.dont_write_bytecode = True

HERE = Path(__file__).resolve().parent
ENV = dict(os.environ, LC_ALL="C", LANG="C")


def cases() -> dict[str, str]:
    """The named cases of `scripts/bench.py`, so both tools use one list."""
    spec = importlib.util.spec_from_file_location("bench", HERE / "bench.py")
    if spec is None or spec.loader is None:
        return {}
    bench = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(bench)
    return {name: script for name, script, _reps in bench.CASES}


def run(slot: Path, script: str) -> float:
    started = time.perf_counter()
    subprocess.run(
        [str(slot), "--norc", "-c", script],
        env=ENV,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    return time.perf_counter() - started


def measure(side: Path, slot: Path, script: str) -> float:
    # `copyfile` then `chmod`, not `copy`: the slot must be writable in place
    # and executable, and a partial copy would race the shell about to run it.
    shutil.copyfile(side, slot)
    os.chmod(slot, 0o755)
    run(slot, script)  # discard: this run pays for the pages the copy dirtied
    return run(slot, script)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--a", required=True, type=Path, help="the 'before' build")
    parser.add_argument("--b", required=True, type=Path, help="the 'after' build")
    parser.add_argument("--case", help="a case name from scripts/bench.py")
    parser.add_argument("--script", help="a -c script, instead of --case")
    parser.add_argument("--reps", type=int, default=8, help="ABBA blocks (4 runs each)")
    parser.add_argument("--slot", type=Path, help="the shared path (default: a temp dir)")
    args = parser.parse_args()

    if args.case:
        try:
            script = cases()[args.case]
        except KeyError:
            print(f"no such case: {args.case}", file=sys.stderr)
            return 2
        label = args.case
    elif args.script:
        script = args.script
        label = "script"
    else:
        parser.error("one of --case or --script is required")

    for side in (args.a, args.b):
        if not side.is_file():
            print(f"not a file: {side}", file=sys.stderr)
            return 2

    if args.slot:
        slot = args.slot
        slot.parent.mkdir(parents=True, exist_ok=True)
        temporary = None
    else:
        temporary = tempfile.TemporaryDirectory(prefix="baash-ab-")
        slot = Path(temporary.name) / "baash"

    # Keyed by slot, not by path: passing the same build as `--a` and `--b` is
    # the control that measures the floor, and keying by path would collapse it
    # into one entry and report a perfect zero.
    sides = {"a": args.a, "b": args.b}
    best = {"a": float("inf"), "b": float("inf")}
    try:
        for _ in range(args.reps):
            for name in ("a", "b", "b", "a"):
                best[name] = min(best[name], measure(sides[name], slot, script))
    finally:
        if temporary is not None:
            temporary.cleanup()

    before, after = best["a"] * 1e3, best["b"] * 1e3
    change = (after - before) / before * 100
    print(f"{label}")
    print(f"  a  {args.a}")
    print(f"  b  {args.b}")
    print(f"  a  {before:8.2f} ms")
    print(f"  b  {after:8.2f} ms")
    print(f"  b vs a  {change:+.2f}%   ({args.reps} ABBA blocks, minimum kept)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
