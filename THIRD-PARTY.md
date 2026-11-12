# Third-party notices

baash is distributed under the GNU General Public License, version 3 or later
(see [LICENSE](LICENSE)). It contains or vendors the following work, which keeps
its own license.

## uutils coreutils — MIT

`crates/baash-utils` embeds these [uutils coreutils][uutils] subcrates (0.8.0)
so that a machine with no coreutils still has `ls`, `cat`, `printf` and friends:
`uu_cat`, `uu_mkdir`, `uu_rm`, `uu_ls`, `uu_wc`, `uu_head`, `uu_tail`, `uu_tr`,
`uu_seq`, `uu_sleep`. The feature is on by default for Windows and opt-in
elsewhere, so the Windows executable always contains this code.

    Copyright (c) uutils developers

    Permission is hereby granted, free of charge, to any person obtaining a copy
    of this software and associated documentation files (the "Software"), to
    deal in the Software without restriction, including without limitation the
    rights to use, copy, modify, merge, publish, distribute, sublicense, and/or
    sell copies of the Software, and to permit persons to whom the Software is
    furnished to do so, subject to the following conditions:

    The above copyright notice and this permission notice shall be included in
    all copies or substantial portions of the Software.

    THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
    IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
    FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
    AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
    LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
    FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS
    IN THE SOFTWARE.

[uutils]: https://github.com/uutils/coreutils

## MinRX — BSD-2-Clause

`crates/baash-sys` compiles [MinRX][minrx] 0.2.1 through the `minrx-sys` crate:
a POSIX extended-regular-expression engine that gives `[[ x =~ y ]]` an engine
on Windows and a portable alternative to the platform `regcomp` on Unix. The
engine's C sources are vendored by `minrx-sys` (whose Rust wrapper is MIT OR
Apache-2.0). MinRX is derived from gawk's `regexp.c`; the character tables in
`charset.c` carry Copyright (C) 2023–2026 Arnold David Robbins under the same
licence.

    Copyright (C) 2023, 2024 Michael J. Haertel.

    Redistribution and use in source and binary forms, with or without
    modification, are permitted provided that the following conditions
    are met:

    1. Redistributions of source code must retain the above copyright
       notice, this list of conditions and the following disclaimer.

    2. Redistributions in binary form must reproduce the above copyright
       notice, this list of conditions and the following disclaimer in the
       documentation and/or other materials provided with the distribution.

    THIS SOFTWARE IS PROVIDED BY THE AUTHOR AND CONTRIBUTORS “AS IS” AND
    ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
    IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
    ARE DISCLAIMED. IN NO EVENT SHALL THE AUTHOR OR CONTRIBUTORS BE LIABLE
    FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
    DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS
    OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION)
    HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT
    LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY
    OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF
    SUCH DAMAGE.

[minrx]: https://github.com/michaeljhaertel/minrx

## Oils spec tests — Apache-2.0

`spec/oils/` vendors the shell spec-test suite from [Oils][oils] unmodified, at
the upstream commit recorded in [spec/README.md](spec/README.md). It is test
data: it is not compiled into the binary and does not ship in a release archive.
The upstream license is [spec/oils/LICENSE.txt](spec/oils/LICENSE.txt).

[oils]: https://oilshell.org

## bash-completion — GPL-2.0-or-later

The [bash-completion][bash-completion] library (2.18.0, as pinned by
[`flake.lock`](flake.lock)) ships **inside the binary**: a `complete -F`
function written for bash-completion runs on a machine that never installed
it. It is not vendored in this repository — [`flake.nix`](flake.nix) copies the
pinned nixpkgs tree into `vendor/bash-completion/` before the build, so the
bytes come from a pinned source rather than from a directory someone edited.
`nix build .#bash-completion` prints that store path, and the mounted copy
carries it in its `PROVENANCE` file.

The GPL's "or later" is what makes this combination possible: bash-completion
is distributed under GPL-2.0-or-later, baash under GPL-3.0-or-later, and the
combined work is distributed under GPL-3.0 (see [LICENSE](LICENSE)). The
library's own license text is not in this repository for the same reason its
code is not; it is in the upstream release (and in `nixpkgs`' source, pinned by
`flake.lock`). Upstream: <https://github.com/scop/bash-completion>.

[bash-completion]: https://github.com/scop/bash-completion

The gate for all of this is `cargo deny check --deny warnings licenses`,
configured in [deny.toml](deny.toml): a dependency whose license is not in the
allow-list fails the build, which is what keeps the paragraph below true.

## carapace-bin — MIT

`crates/baash-complete-data/src/data.rs` is generated from
[carapace-bin][carapace] by `scripts/carapace-data.py`: for each command in
`crates/baash-complete-data/commands.txt`, carapace's `export` backend prints its
cobra command tree as JSON, and the generator compiles that into the
`Cmd`/`Flag` tables the shell carries. The file
is committed, so neither carapace nor Go is part of the build or the binary; the
pinned version is named in the generated header and in `SOURCE`.

carapace-bin is MIT-licensed:

    Copyright (c) 2020 rsteube

    Permission is hereby granted, free of charge, to any person obtaining a copy
    of this software and associated documentation files (the "Software"), to
    deal in the Software without restriction, including without limitation the
    rights to use, copy, modify, merge, publish, distribute, sublicense, and/or
    sell copies of the Software, and to permit persons to whom the Software is
    furnished to do so, subject to the following conditions:

    The above copyright notice and this permission notice shall be included in
    all copies or substantial portions of the Software.

    THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
    IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
    FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
    AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
    LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
    FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
    DEALINGS IN THE SOFTWARE.

MIT is GPL-3.0-compatible, so the tables can be distributed under baash's
licence with this notice kept. Regenerating requires the pinned carapace
(`nix build nixpkgs#carapace`); the diff is the review: a command or option that
disappears upstream is a data change, not a build detail.

[carapace]: https://github.com/carapace-sh/carapace-bin

## Rust dependencies

The crates.io dependencies keep their own licenses — overwhelmingly
`MIT OR Apache-2.0`, with a few `Unicode-3.0`, `ISC`, `BSD-3-Clause`, `Zlib` and
`Unlicense OR MIT`. `Cargo.lock` pins the exact set, and `cargo metadata` prints
each crate's declared license.
