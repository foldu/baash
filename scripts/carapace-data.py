#!/usr/bin/env python3
"""Compile carapace's command-tree export into the static tables baash carries.

carapace-bin (MIT) knows about a thousand commands: subcommands, options and a
one-line help text for each. `carapace <command> export` prints that tree as
JSON (carapace's `internal/shell/export` backend). The shell does not want JSON
at run time and does not want a Go binary in the product, so this script runs
once, at development time, and emits Rust:

    just carapace-data

The output is committed; the Rust build never needs carapace or Go. Regenerate
after bumping the pinned carapace, and read the diff: a command that vanished
from an upstream tool should be a deliberate change, not a surprise.

Usage:
    scripts/carapace-data.py [--carapace PATH] [--manifest FILE] [--out FILE]
"""

import argparse
import hashlib
import json
import os
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_MANIFEST = os.path.join(ROOT, "crates/baash-complete-data/commands.txt")
DEFAULT_OUT = os.path.join(ROOT, "crates/baash-complete-data/src/data.rs")


def rust_string(text):
    """A Rust string literal. Descriptions are UTF-8 and may contain quotes."""
    out = ['"']
    for ch in text:
        if ch == "\\":
            out.append("\\\\")
        elif ch == '"':
            out.append('\\"')
        elif ch == "\n":
            out.append("\\n")
        elif ch == "\t":
            out.append("\\t")
        elif ch == "\r":
            out.append("\\r")
        elif ord(ch) < 0x20:
            out.append("\\u{%x}" % ord(ch))
        else:
            out.append(ch)
    out.append('"')
    return "".join(out)


def one_line(text):
    """Menu descriptions are one line; collapse the help text's whitespace."""
    return " ".join((text or "").split())


def flags_of(node):
    """Local and persistent options, deduplicated by spelling, stable order."""
    seen = set()
    result = []
    for flag in (node.get("LocalFlags") or []) + (node.get("PersistentFlags") or []):
        long = flag.get("Longhand") or ""
        short = flag.get("Shorthand") or ""
        if not long and not short:
            continue
        key = (long, short)
        if key in seen:
            continue
        seen.add(key)
        # `Type` is pflag's value type; a bool never takes a value. pflag also
        # uses `NoOptDefVal` for optional arguments (--color=auto), which still
        # means the option may take one, so `Type` is the honest signal.
        takes_value = (flag.get("Type") or "bool") != "bool"
        result.append(
            {
                "long": long,
                "short": short,
                "desc": one_line(flag.get("Usage")),
                "takes_value": takes_value,
            }
        )
    return result


def emit_flag(flag):
    return (
        "Flag{long:%s,short:%s,desc:%s,takes_value:%s}"
        % (
            rust_string(flag["long"]),
            rust_string(flag["short"]),
            rust_string(flag["desc"]),
            "true" if flag["takes_value"] else "false",
        )
    )


def emit_command(node, name=None):
    name = name if name is not None else (node.get("Name") or "")
    subs = [emit_command(sub) for sub in (node.get("Commands") or []) if sub.get("Name")]
    flags = [emit_flag(flag) for flag in flags_of(node)]
    return "Cmd{name:%s,desc:%s,subs:&[%s],flags:&[%s]}" % (
        rust_string(name),
        rust_string(one_line(node.get("Short"))),
        ",".join(subs),
        ",".join(flags),
    )


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--carapace", default=os.environ.get("CARAPACE", "carapace"))
    parser.add_argument("--manifest", default=DEFAULT_MANIFEST)
    parser.add_argument("--out", default=DEFAULT_OUT)
    args = parser.parse_args()

    with open(args.manifest, "rb") as handle:
        manifest_bytes = handle.read()
    names = [line.strip() for line in manifest_bytes.decode().splitlines() if line.strip()]
    digest = hashlib.sha256(manifest_bytes).hexdigest()[:16]

    version = subprocess.run(
        [args.carapace, "--version"],
        capture_output=True,
        text=True,
        timeout=30,
        check=True,
    ).stdout.strip()

    roots = []
    skipped = []
    for name in sorted(set(names)):
        try:
            proc = subprocess.run(
                [args.carapace, name, "export"],
                capture_output=True,
                timeout=30,
            )
        except subprocess.TimeoutExpired:
            skipped.append((name, "timeout"))
            continue
        if proc.returncode != 0 or not proc.stdout.strip():
            skipped.append((name, "no export"))
            continue
        try:
            tree = json.loads(proc.stdout)
        except json.JSONDecodeError as error:
            skipped.append((name, "bad json: %s" % error))
            continue
        roots.append(emit_command(tree, name=name))

    commands = sum(1 for _ in roots)
    header = (
        "// @generated by scripts/carapace-data.py -- do not edit.\n"
        "// Source: %s; manifest digest %s; %d commands.\n"
        "// Licence: MIT (carapace-bin), see THIRD-PARTY.md.\n\n"
        "use crate::{Cmd, Flag};\n\n"
        "pub(crate) const SOURCE: &str = %s;\n\n"
        "#[rustfmt::skip]\n"
        "pub static COMMANDS: &[Cmd] = &[\n" % (
            version,
            digest,
            commands,
            rust_string("%s manifest %s" % (version, digest)),
        )
    )
    body = ",\n".join(roots)
    with open(args.out, "w", encoding="utf-8") as handle:
        handle.write(header)
        handle.write(body)
        handle.write(",\n];\n")

    print("wrote %s: %d commands, %d skipped" % (args.out, len(roots), len(skipped)))
    for name, why in skipped:
        print("  skipped %-24s %s" % (name, why), file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
