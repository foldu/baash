#!/usr/bin/env bash
# Sample one baash command line with `perf`, the way docs/performance.md's
# "where the CPU goes" sections were taken.
#
#   scripts/perf.sh --norc -c 'i=0; while (( i < 20000 )); do ( : ); (( i++ )); done'
#   scripts/perf.sh --norc script.sh
#
# The binary is a *release* build with frame pointers and debuginfo in
# `target/perf` (env `PERF_TARGET_DIR` moves it): the shipped build has neither,
# and without symbols a profile is a list of addresses. Everything after this
# script's own name goes to baash unchanged.
#
# `task-clock` rather than a hardware counter, because a container or a VM
# usually has no PMU — `perf record -e cpu/cycles` then fails with "No such
# file". `task-clock` counts CPU time, so a blocked process samples as nothing;
# that is the right clock for these cases, and it is why a wall-clock claim
# never comes from a profile.
#
# Callers instead of self time:
#   perf report -i target/perf/record.data --stdio --children -g graph
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo"
target=${PERF_TARGET_DIR:-target/perf}
data=${PERF_DATA:-$target/record.data}
freq=${PERF_FREQ:-4000}
limit=${PERF_LIMIT:-1.0}

echo "building $target/release/baash (frame pointers, debuginfo)" >&2
RUSTFLAGS="-Cforce-frame-pointers=yes" \
CARGO_TARGET_DIR="$target" \
CARGO_PROFILE_RELEASE_DEBUG=2 \
CARGO_PROFILE_RELEASE_STRIP=none \
    cargo build --release -p baash >&2

if [ "$#" -eq 0 ]; then
    echo "usage: $0 [baash arguments...]" >&2
    exit 2
fi

perf record -e task-clock -F "$freq" -g --call-graph fp -o "$data" -- \
    "$target/release/baash" "$@"

echo
echo "self time, samples of task-clock (data: $data)" >&2
# `-F overhead,symbol` drops the IPC columns perf pads to the width of the
# longest Rust symbol; the awk keeps the rows and trims the tail.
perf report -i "$data" --stdio --no-children --no-call-graph \
    -F overhead,symbol --percent-limit "$limit" 2>/dev/null \
    | awk '/^ *[0-9]+\.[0-9]+%/ { sub(/ +$/, ""); print substr($0, 1, 120) }'
