# Vendored: Oils spec tests

`oils/` is an **unmodified copy** of the `spec/` directory from
[oils-for-unix/oils](https://github.com/oils-for-unix/oils):

- upstream commit: `6778e71bec2d91b0e6e1fa492675ad4f64d4377e`
- license: Apache-2.0 (see `oils/LICENSE.txt`)
- contents: 223 `*.test.sh` files plus the helper programs in `oils/bin/` and
  the fixtures in `oils/testdata/` that those tests invoke

Do not edit anything under `oils/`. Keeping it byte-identical is what makes
"our shell passes N of the upstream suite" a meaningful sentence. Local
expectations belong in our harness or in our own extra cases, never as edits to
a vendored file.

## What this buys us

The suite is the project's conformance oracle (DESIGN.md §8). It is useful for
two reasons beyond sheer case count:

- Each case declares its expected output, and where shells genuinely disagree
  it carries per-shell overrides:
  `## OK dash STDOUT:` alongside `## STDOUT:`. That is the ambiguity budget of
  DESIGN.md §3.3 made executable.
- The header annotations describe how a case must be run:
  `## compare_shells: bash dash mksh`, `## our_shell: ysh`,
  `## status: N`, `## stdout-json:`.

## Running it

The harness is not written yet. It will read these files, run each case against
our shell and against reference shells found on `PATH`, honour the per-shell
overrides, and report a pass count per file. The pass count is the project's
only real progress metric and will be tracked in the top-level README.

## Scope note

80 of the files are `ysh-*.test.sh`, covering the YSH language. baash does not
implement YSH and will not; the harness should skip cases whose header says
`## our_shell: ysh` rather than counting them as failures. They are vendored
anyway, because a subset is harder to justify as "the upstream suite" and the
selection is cheap to express in the harness.

## Updating

```sh
git clone --depth 1 https://github.com/oils-for-unix/oils /tmp/oils
rm -rf spec/oils && mkdir -p spec/oils
cp -a /tmp/oils/spec/. spec/oils/
cp /tmp/oils/LICENSE.txt spec/oils/LICENSE.txt
git -C /tmp/oils rev-parse HEAD   # record the new commit here
```
