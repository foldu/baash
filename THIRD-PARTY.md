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

## Oils spec tests — Apache-2.0

`spec/oils/` vendors the shell spec-test suite from [Oils][oils] unmodified, at
the upstream commit recorded in [spec/README.md](spec/README.md). It is test
data: it is not compiled into the binary and does not ship in a release archive.
The upstream license is [spec/oils/LICENSE.txt](spec/oils/LICENSE.txt).

[oils]: https://oilshell.org

## Rust dependencies

The crates.io dependencies keep their own licenses — overwhelmingly
`MIT OR Apache-2.0`, with a few `Unicode-3.0`, `ISC`, `BSD-3-Clause`, `Zlib` and
`Unlicense OR MIT`. `Cargo.lock` pins the exact set, and `cargo metadata` prints
each crate's declared license.
