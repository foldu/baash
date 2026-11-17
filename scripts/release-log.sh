#!/usr/bin/env bash
# Run `just release <version>` with the whole session kept on disk.
#
# `just release` prints the gate's cargo and nix output, `cargo release`'s plan
# and two pushes. When the terminal scrolls, that is the only record of what
# was released and where it stopped — and a release is the one command whose
# log is worth keeping, because a tag that fails halfway exists and cannot be
# rewritten (`docs/releasing.md`).
#
# This is `script(1)` around the same recipe, so the log is a transcript: what
# the terminal showed, in order, prompts included. A push that asks for a
# passphrase asks you from the same pty; a `Ctrl-C` stops where it would have.
#
# Writes, under `dist/release-logs/<version>-<UTC timestamp>/`:
#
#   transcript.log  everything, exactly as the terminal saw it
#   output.txt      the same, ANSI stripped and CR turned into line breaks
#   meta.txt        what is being released: HEAD, branch, remotes, dirty files,
#                   tool versions
#   result.txt      exit status, duration, and what to check next
#
# and points `dist/release-logs/latest` at the directory. `dist/` is ignored.
#
# Usage: scripts/release-log.sh <version>    or    just release-logged <version>
set -euo pipefail

usage() {
    printf 'usage: %s <version>      (for example 0.1.2 or 0.1.2-rc.1)\n' "$(basename "$0")" >&2
    exit 2
}

[ "$#" -gt 0 ] || usage
if ! command -v just >/dev/null 2>&1; then
    printf '%s: just is not on PATH; run this inside `nix develop`\n' "$(basename "$0")" >&2
    exit 1
fi

root=$(git rev-parse --show-toplevel 2>/dev/null) || {
    printf '%s: not inside a git checkout\n' "$(basename "$0")" >&2
    exit 1
}
cd "$root"

# The label is the argument, not a parsed version: `cargo release` accepts a
# version or a bump keyword, and the directory should say what was asked for.
label=$(printf '%s' "$*" | tr -c 'A-Za-z0-9._-' '-')
dir="dist/release-logs/${label}-$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$dir"
ln -sfn "$(basename "$dir")" dist/release-logs/latest

# Best-effort: a tool that is missing, or that prints more than one line, must
# not turn the log into a reason the release fails.
first_line() {
    "$@" 2>/dev/null | head -1 || true
}

{
    printf 'command: just release %s\n' "$*"
    printf 'started: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf 'directory: %s\n' "$root"
    printf 'branch: %s\n' "$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo '?')"
    printf 'head: %s\n' "$(git log -1 --format='%H %s' 2>/dev/null || echo '?')"
    printf 'remotes:\n'
    git remote -v 2>/dev/null | sed 's/^/  /'
    printf 'tools:\n'
    for tool in just cargo rustc nix; do
        if command -v "$tool" >/dev/null 2>&1; then
            printf '  %s: %s\n' "$tool" "$(first_line "$tool" --version)"
        else
            printf '  %s: (not found)\n' "$tool"
        fi
    done
    if command -v cargo >/dev/null 2>&1; then
        release_version=$(first_line cargo release --version)
        printf '  cargo-release: %s\n' "${release_version:-(not installed)}"
    else
        printf '  cargo-release: (not found)\n'
    fi
    printf 'status:\n'
    if [ -n "$(git status --porcelain)" ]; then
        git status --porcelain | sed 's/^/  /'
    else
        printf '  (clean)\n'
    fi
} >"$dir/meta.txt"

# What the terminal wrote, in a file that survives `grep`: `script`'s own
# banner, CSI sequences (colour, cursor, erase), OSC sequences (titles, the cwd
# report), the older two-byte escapes a program still sends (`ESC ( B` is a
# charset designation; `ESC 7`/`ESC 8` save and restore the cursor) and the pty's
# CRLF. A bare CR is a progress update that overwrote its line on screen, so it
# becomes a line break here rather than gluing two updates together.
# `transcript.log` keeps all of it: that file is the record.
strip_ansi() {
    local esc=$'\033' bel=$'\007'
    sed -E \
        -e '/^Script (started|done) on /d' \
        -e "s/${esc}\][^${bel}]*${bel}//g" \
        -e "s/${esc}\][^${esc}]*${esc}\\\//g" \
        -e "s/${esc}\[[0-9;?]*[a-zA-Z]//g" \
        -e "s/${esc}([()*+][0-9A-Za-z]|[=><]|[0-9A-Za-z])//g" \
        -e 's/\r$//' \
        -e 's/\r/\n/g'
}

started=$(date +%s)

finish() {
    local code=$?
    set +e
    local elapsed=$(($(date +%s) - started))
    if [ -s "$dir/transcript.log" ]; then
        strip_ansi <"$dir/transcript.log" >"$dir/output.txt"
    else
        : >"$dir/output.txt"
    fi
    {
        printf 'finished: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        printf 'status: %d\n' "$code"
        printf 'duration: %dm%02ds\n' "$((elapsed / 60))" "$((elapsed % 60))"
        printf 'logs: %s\n' "$root/$dir"
        if [ "$code" -eq 0 ]; then
            printf 'next: the tag publishes from GitHub — watch `gh run watch -R foldu/baash`,\n'
            printf '      then check the four assets (`docs/releasing.md`, "After the push").\n'
        else
            printf 'next: nothing is published from this machine. Before retrying, check\n'
            printf '      `git tag --points-at HEAD` and `git status` — a local tag that the\n'
            printf '      failed run made exists already (`docs/releasing.md`).\n'
        fi
    } >"$dir/result.txt"
    printf '\nrelease-log: %s\n' "$root/$dir" >&2
    exit "$code"
}
trap finish EXIT

if [ ! -t 0 ]; then
    printf 'release-log: stdin is not a terminal; a prompt from a push cannot be answered.\n' >&2
fi

# util-linux `script` is the one whose `-e` returns the child's status; a
# `grep -q` on its help text would close the pipe early and, with `pipefail`,
# report the fallback instead, so the text is matched in the shell.
has_pty=0
if command -v script >/dev/null 2>&1; then
    script_help=$(script --help 2>&1 || true)
    case "$script_help" in
        *--return*) has_pty=1 ;;
    esac
fi

status=0
if [ "$has_pty" -eq 1 ]; then
    # util-linux `script`: `-q` keeps its own banner out of the live stream (the
    # file records the command and the exit code), `-e` exits with the child's
    # status, and `-f` flushes on every write so a killed run keeps what it had.
    set +e
    script -q -e -f -c "just release $(printf '%q ' "$@")" "$dir/transcript.log"
    status=$?
    set -e
else
    # No util-linux `script` (macOS, busybox): no pty, so a prompt from a push
    # is invisible — the log is still complete, and `docs/releasing.md` says
    # what a refused push means.
    printf 'release-log: script(1) is not util-linux; logging without a pty.\n' >&2
    set +e
    just release "$@" 2>&1 | tee "$dir/transcript.log"
    status=${PIPESTATUS[0]}
    set -e
fi
exit "$status"
