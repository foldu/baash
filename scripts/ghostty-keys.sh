#!/usr/bin/env bash
#
# What does a real terminal send for a key?
#
# Runs ghostty on a throwaway X display, makes the same keyboard request the
# line editor makes (`CSI = 1 u`, `CSI > 4 ; 1 m`), presses the keys with
# xdotool, and hexdumps what came back. The keys are concatenated in the order
# given, so run it with one key when the boundaries are not obvious.
#
#   nix shell nixpkgs#ghostty nixpkgs#xvfb-run nixpkgs#xdotool \
#       --command xvfb-run -a -s "-screen 0 1280x800x24" \
#       scripts/ghostty-keys.sh ctrl+d
#
# `LOCKS` turns lock keys on before typing, because the modifier field carries
# them and a binding must not care:
#
#   LOCKS=Num_Lock scripts/ghostty-keys.sh ctrl+d alt+b
#
# AGENTS.md, "Harness a real terminal", explains the rest.
set -euo pipefail

if [ $# -eq 0 ]; then
    echo "usage: $0 <xdotool key>...   (LOCKS=Num_Lock|Caps_Lock to hold a lock)" >&2
    exit 2
fi
for tool in ghostty xdotool stty dd timeout; do
    command -v "$tool" >/dev/null || {
        echo "$0: $tool is not on PATH; run it through the nix shell in AGENTS.md" >&2
        exit 1
    }
done

bytes=$(mktemp -t ghostty-keys.XXXXXX)
program=$(mktemp -t ghostty-capture.XXXXXX)
ready=$(mktemp -u -t ghostty-ready.XXXXXX)
trap 'rm -f "$bytes" "$program" "$ready"' EXIT

# The program inside ghostty: ask for the protocol, then take the terminal raw
# so the request is not echoed and the reply is not cooked. A shell cannot wait
# for "no more bytes", so the read is bounded and whatever arrived is the
# answer.
#
# `timeout --foreground` is not a detail: plain `timeout` puts `dd` in its own
# process group, the pty is not in the foreground group any more, and the read
# stops on SIGTTIN — an empty capture with no error at all.
cat >"$program" <<'CAPTURE'
#!/bin/sh
printf '\033[=1u\033[>4;1m'
stty raw -echo
# The window is painted before this program runs; the marker says the terminal
# is raw and the request has been written, so keys typed now are captured.
: >"$2"
timeout --foreground 8 dd bs=1 count=512 of="$1" 2>/dev/null
CAPTURE
chmod +x "$program"

export LIBGL_ALWAYS_SOFTWARE=1
ghostty -e /bin/sh "$program" "$bytes" "$ready" >/dev/null 2>&1 &
ghostty_pid=$!

# llvmpipe takes seconds to paint the first frame, and the window appears before
# the program inside it has run, so poll rather than sleep.
window=
for _ in $(seq 1 80); do
    for candidate in $(xdotool search --onlyvisible --class com.mitchellh.ghostty 2>/dev/null); do
        # A broken config opens a second window that would take the keys.
        if [ "$(xdotool getwindowname "$candidate" 2>/dev/null)" = "Configuration Errors" ]; then
            continue
        fi
        window=$candidate
        break
    done
    [ -n "$window" ] && break
    sleep 0.25
done
if [ -z "$window" ]; then
    echo "ghostty opened no window; is DISPLAY set? (run it under xvfb-run)" >&2
    kill "$ghostty_pid" 2>/dev/null || true
    exit 1
fi

# There is no window manager on a bare Xvfb: nothing focuses the window for us.
xdotool windowfocus "$window" >/dev/null 2>&1 || true
xdotool windowraise "$window" >/dev/null 2>&1 || true

# Wait for the program to have asked for the protocol and gone raw.
for _ in $(seq 1 80); do
    [ -e "$ready" ] && break
    sleep 0.1
done
if [ ! -e "$ready" ]; then
    echo "the program inside ghostty never started" >&2
    kill "$ghostty_pid" 2>/dev/null || true
    exit 1
fi
sleep 0.3

for lock in ${LOCKS:-}; do
    xdotool key --clearmodifiers "$lock"
    sleep 0.6
done
for key in "$@"; do
    xdotool key --clearmodifiers "$key"
    sleep 0.4
done

# The capture ends when its timeout fires; give it the time, then clean up.
sleep 8
kill "$ghostty_pid" 2>/dev/null || true
wait "$ghostty_pid" 2>/dev/null || true

if [ -s "$bytes" ]; then
    od -An -tx1 -c "$bytes"
else
    echo "the terminal sent nothing; the request was never made or the keys never arrived" >&2
    exit 1
fi
