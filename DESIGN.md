# ßhell — design notes

Status: exploration, v2. Binary `bsh`. **Interactive-first**: the primary use is
a person typing at a prompt, and the primary feature is completion.

Evidence tags: `[spec]` checked against POSIX.1-2024 (Issue 8) text; `[oils]`
read out of the Oils tree (`oils-for-unix/oils`, Apache-2.0); `[nucleo]` checked
against `nucleo-matcher` 0.3.1 docs; `[zsh]`/`[bash]` checked empirically;
`[rec]` my recommendation.

---

## 1. Thesis

Three claims, in order of how much they constrain the design.

1. **A new shell is justified only by its interactive experience.** POSIX
   conformance and bash compatibility are table stakes — they are how you avoid
   being rejected, not why anyone switches. So the product is the line editor,
   the completion, and the feedback loop, and everything else is plumbing that
   must not get in their way.

2. **Completion quality is bounded by parse quality.** Every existing shell
   does completion with a pile of heuristics over `COMP_WORDS`, a naive
   whitespace split, because it has no usable parser at the point where the
   user is mid-word. We are writing a real parser anyway. Instrumenting it to
   answer *"what is syntactically valid at this byte offset"* is the single
   highest-leverage thing in this document, and nothing else can copy it
   cheaply.

3. **Bash compatibility is not a superset relation, it is a decision table.**
   Roughly forty constructs parse and evaluate differently. Oils' most valuable
   artifact is not its shell, it is [`doc/known-differences.md`][kd] (511 lines
   of exactly that table) plus `spec/` — 3971 cases in 223 files, per-shell
   override blocks, Apache-2.0, run against bash/dash/mksh as oracles `[oils]`.
   Vendor the suite in week one. Write the table before the lexer.

[kd]: https://www.oilshell.org/release/latest/doc/known-differences.html

---

## 2. What "fully POSIX compatible" can and cannot mean

Be precise or the requirement is unbounded. `[spec]` A conforming `sh` is:

1. **XCU chapter 2** — the shell command language.
2. **`sh(1p)`** — the utility interface (`-c`, `-i`, `-s`, `-n`, `-e`, `-u`, …).
3. **Special built-ins** (§2.15): `break : continue . eval exec exit export
   readonly return set shift times trap unset`.
4. **Intrinsic utilities** (§1.7, new in 2024): `alias bg cd command fc fg
   getopts hash jobs kill read type ulimit umask unalias wait`.
5. **Standard utilities must be exec-able** — `printf`, `test`, `[`, `echo`,
   `pwd`, `true`, `false` and all non-shell utilities must exist on disk and be
   reachable via `exec`, `env`, `find -exec`, `xargs`. They are *not* intrinsic.

So: ßhell implements the shell and its builtins. It does not ship coreutils.
`sh -c 'sed …'` is our problem only to the extent of finding and exec'ing
something.

Two 2024 facts that shape builtin tiering:

- The **intrinsic** tier means a shell need not ship exec-able `cd`/`read`/
  `type`, and **functions override intrinsics** — but never special built-ins.
  `kill` is the one intrinsic that must still be exec-able.
- `[spec]` **`pipefail` IS POSIX in 2024.** §2.9.2 defines pipeline exit status
  in terms of "whether or not the `pipefail` option (see `set`) is enabled", and
  `set` lists `-o pipefail`. It therefore belongs to strict mode, not the bash
  tier. (Earlier revisions of this document said the opposite. The error came
  from grepping `utilities/set.html`, which in the online edition is a
  JavaScript redirect stub that contains no prose at all — the real text is in
  `utilities/V3_chap02.html`. Any "I checked the spec" claim has to say which
  page it actually read.)
- `[spec]` The pipeline rule has a trap the specification calls out explicitly:
  *"The shell shall use the `pipefail` setting at the time it begins execution
  of the pipeline, not the setting at the time it sets the exit status."* So in
  `cmd1 | set -o pipefail`, `cmd1`'s failure does **not** affect that pipeline's
  status. The flag must be sampled at pipeline start (§5.2).

`[spec]` Still unspecified in 2024, and listed as such by name: `local`,
`declare`, `typeset`, `source`, `shopt`, `pushd`/`popd`, `let`, `mapfile`,
`readarray`, `history`, `print`, `suspend`, `disown`, `dirs`. That list is
effectively POSIX saying "we know everyone uses these and we decline to bless
them". Our bash tier is mostly that list.

---

## 3. The compatibility model

### 3.1 One parse, profiles gate acceptance

Two options:

- **(a) Two grammars.** In `sh` mode `[[` lexes as a command word. The same
  bytes parse differently, so a linter cannot say "this is a bashism, it works
  in bash" — it must first be told which mode you *intend*, which is the very
  question you are asking.
- **(b) One superset grammar, post-parse gating.** Parse all of bash, then in
  strict mode reject bash-only *nodes* with a span.

`[rec]` Take (b). `bsh -n --posix file` becomes a real portability checker with
line/column and a fix-it, and `bsh fmt` works on files written for either shell.
Only the lexically-unavoidable conflicts (§3.3 Tier A) are shared, so gating is
syntactic, never lexical.

### 3.2 Profiles

A profile is a named bundle of ~80 booleans, not a code path.

```rust
pub struct Opts {
    pub errexit: bool, pub nounset: bool, pub noglob: bool, pub noexec: bool,
    pub posix_mode: bool,          // strict: reject bash-only AST nodes
    pub extglob: bool, pub dotglob: bool, pub nullglob: bool, pub globstar: bool,
    pub lastpipe: bool, pub nocasematch: bool, pub xpg_echo: bool,
    pub expand_aliases: bool, pub histexpand: bool, /* … */
}
pub const POSIX: Opts = Opts { posix_mode: true, ..BASE };
pub const BASH:  Opts = Opts { posix_mode: false, ..POSIX_STYLE };
```

Dispatch by `argv[0]` basename (`sh` → POSIX, `bash` → BASH, `bsh` → BASH with
a friendlier `echo`), `--posix`/`--bash` overrides, `set -o`/`shopt` per-option.
Tools derive the profile from the `#!` line.

Interactive default: BASH preset, `histexpand` **off** (`!$` is a footgun and
nobody types it under a fuzzy-completion editor; expose it for people who want
it).

### 3.3 The ambiguity budget

This table is the product. Test every row.

**Tier A — lexical, the lexer must commit, no way back.**

| construct | POSIX reading | bash reading | our choice |
|---|---|---|---|
| `((` | nested subshell | arithmetic command | **bash** |
| `$((` | cmd-sub of a subshell | arithmetic expansion | **bash** |
| `[[` | command named `[[` | conditional keyword | **bash** |
| `$'…'` | `$` + `'…'` | ANSI-C quoting | **bash** (cost ≈ 0) |
| `<<<` | syntax error | here-string | **bash** |
| `<(cmd)` `>(cmd)` | syntax error | process substitution | **bash** |
| `\|&` | syntax error | pipe stderr too | **bash** |
| `&>` | `&`, then `>` | redirect both | **bash** |
| `function` | command name | function keyword | **bash** |

Not in this table: `{a,b}` brace expansion, `nullglob`, `dotglob`, `extglob`
*matching*. Those are expansion-time switches, so strict mode turns them off
without touching the lexer. Reserve the lexer for what truly cannot be deferred.

Cost of the `((` row: POSIX-legal `((cd /tmp))` now parses as arithmetic and
fails. Oils made the identical choice `[oils]`; the error message must say
"`((` starts arithmetic; write `( (` for a nested subshell". Accept it
consciously.

**Tier B — syntactic, bounded lookahead.**

| construct | decision |
|---|---|
| `f() { …; }` vs `f=()` | one token lookahead, require `()` adjacency |
| `a[1 + 2]=v` | **reject** `[oils]`; otherwise the parser needs unbounded re-lexing |
| `$((` vs `$( (` | never re-enter: `$((` is always arithmetic |
| `!(` extglob vs `! (` | space-sensitive; `extglob` never affects parsing `[oils]` |
| `case x in x) ;; esac )` | exactly **one** token of pushback `[oils]` |

**Tier C — runtime semantics.** Where bash compatibility actually costs money.
Rows marked ✔ verified against bash 5.3.20 rather than from memory:

| construct | POSIX | bash | our choice |
|---|---|---|---|
| `local x=$(cmd)` status | `local` unspecified | 0, masks failure ✔ | bash |
| `echo -e` | unspecified | interprets escapes ✔ | profile table |
| `test` with 5+ args | unspecified | own recursive grammar | bash's, documented |
| arithmetic overflow | ≥ signed long `[spec]` | 64-bit wrap | 64-bit wrap |
| `$(( x ))`, x non-numeric | error | 0 ✔ | bash |
| `${x:-"$@"}` | no splitting of `"$@"` | splits | **split** (bash); Oils declined and lists it as a known difference |
| `declare -A x=(a b)` | n/a | alternating k/v ✔ | bash (Oils errors; its error is arguably better) |
| `unset $lvalue`, `printf -v $lvalue`, `${!name}` | n/a | re-parses data as code | **opt-in only** (`shopt eval_unsafe_arith`) |
| `trap '' INT` then child | `SIG_IGN` inherited | same | POSIX — see §4.5 |
| prefix assignment before a special built-in | persists | bash (non-posix) drops it; `bash --posix` persists | **POSIX**, in both profiles — a deliberate, documented deviation |

The last three rows are one bug class: bash re-parses strings as code, so
`a[$(cmd)]=v` executes `cmd`. Oils refuses by default and exposes
`shopt -s eval_unsafe_arith`; that is correct. Ship the refusal, and have the
opt-in print a warning.

### 3.4 What bash compatibility is *not*

Not "runs all bash". Target, in Oils' phrasing, *all POSIX shell programs and
most bash programs* `[oils]`.

**In, day one:** arrays (indexed + assoc), `[[ ]]` incl. `=~` and `-v`,
`local`/`declare`/`typeset` with `-a -A -i -x -r -u -l`, `+=`, brace expansion,
`$'…'`, `((…))`/`let`, `function f {}`, `source`, `${x//pat/rep}`,
`${x:off:len}`, `${x^^}`, `${x@Q}`, `$RANDOM`, `$SECONDS`, `PIPESTATUS`, `&>`,
`|&`, `<<<`, `<(…)`, `shopt`, `set -o pipefail`, `printf -v`,
`read -a/-d/-n/-p/-t`, `mapfile`, `command -v`, `type -a`, `extglob`,
`globstar`, `nullglob`, `dotglob`, `nocasematch`, `lastpipe`, **and the
programmable-completion builtins** `complete`/`compgen`/`compopt` (§6.7) — for
an interactive shell those are not optional.

**Deferred:** `nameref` (`declare -n`), `coproc`, `select`, history expansion
`!$`, `bind`, `fc` editing, `$BASH_*` beyond what scripts actually read. Design
the variable store with a `resolve(name) -> &mut Cell` hook so `nameref` is a
lookup flag later, not a refactor (§4.6). bash-completion needs `nameref`, so it
comes in with §6.7.

**Refused:** shellshock-era `export -f`.

---

## 4. Architecture

### 4.1 Crates and the unsafe boundary

```
bsh-syntax    lexer, parser, AST, spans, unparse, diagnostics, expectation queries
bsh-highlight token + theme -> styled spans; ANSI rendering          ← §6.10
bsh-compile   AST → Chunk (bytecode)                        ← §5
bsh-vm        the interpreter loop                          ← §5
bsh-diag      diagnostic IR + renderers (compact / miette / json)
bsh-lang      words, expansion, arith, patterns, glob
bsh-runtime   vars, scopes, builtins, options, errors
bsh-sys       fork/exec, fd table, signals, pty, tty   ← the ONLY unsafe crate
bsh-repl      line editor, key decoding, rendering, history
bsh-complete  completion context, providers, ranking   ← §6
bsh-tools     fmt, lint, explain, debugger
bsh           binary: dispatch, main loop
```

`#![forbid(unsafe_code)]` on all but `bsh-sys`, enforced in CI. Everything
reachable from `fork()` is then safe Rust, and the code that runs between
`fork` and `exec` is auditable by reading one crate.

Contracts that cross crate boundaries — and therefore outlive a single branch —
are collected in `docs/contracts.md`: the global fork gate, spawn-plan
environment and signal rules, how exec failures surface, and the renderer
inventory.

`bsh-repl` must not depend on `bsh-runtime`. The editor talks to traits
(`Completer`, `Highlighter`), which keeps it testable headlessly and reusable.

### 4.2 Pipeline

```
bytes ─▶ Lexer ─▶ Ast ─▶ Compiler ─▶ Chunk ─▶ VM
          │        │                  (u32-code, span table)
          │        └─ spans for every diagnostic
          └─ expectation stack, so a *truncated* line can be introspected

REPL:  key ─▶ line buffer ─▶ lex/parse prefix ─▶ completion ctx ─▶ menu
                   ▲                                                   │
                   └──────────────── insertion (correctly quoted) ◀────┘
```

**Lossless invariant** `[oils]`: concatenating every token's source text must
reproduce the input byte for byte. Oils gates CI on it for thousands of scripts;
it forces the lexer to have an opinion about every byte, which is what you want
before you build a formatter on top. Adopt it from day one.

**Arena AST.** Node indices into a flat `Vec<Node>`, plus an explicit
`max_depth`. Adversarial `((((((…` must produce a diagnostic, not a stack
overflow.

**Words are the data structure that matters:**

```rust
pub enum Part {
    Lit(Lit),                  // unquoted / single-quoted / $'…'
    DoubleQ(Vec<Part>),
    Param(ParamExp),           // ${x:-y} ${x//a/b} ${x:1:2} ${!x}
    CmdSub(Box<Ast>),
    Arith(Box<Arith>),
    ProcSub(Box<Ast>),         // bash <(…) >(…)
    Tilde(TildeSpec),
}
pub struct Word { pub parts: Vec<Part>, pub span: Span }
```

Quotedness belongs to the *part*, not the word — otherwise `"$@"`,
`${x:-"$@"}` and `$*` cannot be got right. Make that a type, not a comment.

**Byte strings, never `String`.** Filenames are any non-NUL bytes, `$'\xff'`
exists, POSIX strings are byte sequences `[spec]`. Use `Vec<u8>`/`bstr`
throughout; decode to UTF-8 only for display, error messages, and the editor.
This is the one irreversible decision in the document.

### 4.3 Threads and the fork gate

Earlier drafts said "the interpreter is single-threaded". That was too strong,
and it matters here because a fuzzy matcher tempts you into threads.

The real hazard is narrower: after `fork()` in a multithreaded process, the
child may only call async-signal-safe functions — no allocation (the heap may
have been mid-update), no locks (they may be held by a thread that does not
exist in the child). A child that does *only* raw syscalls until `exec` is safe
regardless of how many threads the parent has.

So the invariant is:

> **No allocation, no locking, no dropping, no panicking between `fork()` and
> `exec()`.** Nothing else. Threads are allowed.

Which gives three rules:

1. **`posix_spawn` is the default for external commands.** It uses
   `CLONE_VM|CLONE_VFORK`, shares the address space, and executes only
   async-signal-safe operations — correct and fast in a threaded parent. `[rec]`
   `POSIX_SPAWN_SETPGROUP` exists and glibc implements `adddup2`/`addopen`, so
   job control does **not** force `fork`.
2. **`fork` is only for "run shell code in a child"**: subshells, pipeline
   stages that are functions or compound commands, `$( … )` around shell code,
   `&`. Those cannot be spawned, because they must run the interpreter.
3. **Anything spawned that uses threads is scoped to a mode where no process
   creation happens.** The full-screen picker (§6.8) may own a worker thread;
   the debugger may own one. While any thread exists, `fork()` is a bug.

Enforce rule 3 mechanically with a `ThreadGate` counter that panics in debug
builds if `fork()` is called while threads are live, plus a `--threadless` test
mode for CI. That turns a subtle rule into an assertion.

Two corollaries that are easy to miss:

- The child inherits the whole fd table. Helper pipes, `inotify` fds and
  terminal fds must all be `CLOEXEC`, or an exec'd program holds the shell's
  plumbing open and `wait`ing on a pipeline hangs forever.
- `exec` resets *caught* signals but leaves `SIG_IGN` ignored `[spec]`, so the
  spawn plan carries an explicit `reset_signals` list or `trap '' INT` leaks a
  Ctrl-C-proof child.

### 4.4 File descriptors and I/O

- Explicit fd table: `{ user: RawFd, internal: RawFd, cloexec: bool }`; the
  shell's own plumbing lives at ≥ 10.
- **Persistent read buffers per fd.** `read` must not read ahead: in
  `while read line; do cmd; done <f`, if the shell buffers 4 KiB, `cmd` sees a
  truncated file. Byte-at-a-time for `read`, with a fast path when `fstat` says
  `S_ISREG` and we can `lseek` back.
- **Here-docs.** Two problems: (i) the body must not be tokenised as code — the
  parser queues "read to delimiter at next newline" `[oils]`; (ii) a large body
  written to a pipe deadlocks when the reader never reads. Use `memfd_create`/
  `O_TMPFILE` with an unlink-in-place fallback, and make sure `SIGPIPE` from an
  abandoned here-doc writer cannot kill the shell.
- Redirect order is observable and left-to-right: `2>&1 >f` ≠ `>f 2>&1`. It is
  a list, never a set.

### 4.5 Signals

`[spec]` A trap runs after the current foreground command completes, not inside
it. So: self-pipe or `signalfd`; the handler sets a byte; the main loop drains
at defined safe points. No handler ever touches interpreter state.

Interactive Ctrl-C has three distinct meanings that must be three code paths:
cancel the line edit, interrupt the running foreground job, or interrupt a
completion. Conflating them is why some shells eat Ctrl-C.

`SIGCHLD` is a wakeup hint only — never trust it to be delivered once per child
(coalescing). Reap with `waitpid(WNOHANG)` in the loop.

### 4.6 Variables

```rust
pub struct Var { pub value: VarValue, pub attrs: Attrs }
pub enum VarValue { Unset, Str(BStr), Indexed(Vec<BStr>), Assoc(IndexMap<BStr, BStr>) }
```

Global stack → function frames → `local`/`declare` frames. Exportedness is an
attribute. Assignments preceding a simple command are temporary and exported
into that command's environment; preceding a *special built-in* they persist
`[spec]` — the one place a builtin's tier changes runtime behaviour rather than
search order.

Single entry point `fn resolve(&mut self, name: &[u8]) -> &mut Cell`; that is
where `nameref` lands later, with a chain limit and cycle check.

### 4.7 Errors

`[spec]` §2.8.1 is literally a table of *error class × interactive?* → *exit or
continue*. Encode it as one function and unit-test every cell — highest ratio of
spec text to code in the project.

```rust
pub enum ShellError {
    Syntax(Span, Msg), Expansion(Span, Msg), Redirect(Span, Msg),
    SpecialBuiltinFailed(Status), UtilityFailed(Status),
}
pub fn consequence(e: &ShellError, interactive: bool, opts: &Opts) -> Action;
```

`errexit` gets its own note: the POSIX text does not determine behaviour, every
shell differs, and the only oracle is empirical. Implement one documented
algorithm (ignore-context model: conditions of `if`/`while`/`until`, left
operands of `&&`/`||`, `!`, and all but the last pipeline element are exempt),
then let `spec/errexit.test.sh` arbitrate. Do not promise more. §5.3 makes this
a table entry instead of a `Result` at every AST node, which is the main reason
to have a VM.

### 4.8 Patterns: one engine, seven uses

Pattern matching appears in `case`, pathname expansion, `${x#pat}`, `${x%pat}`,
`${x/pat/rep}`, `[[ x == pat ]]`, and completion filtering. Oils converts
`${x/pat/rep}` to POSIX ERE and leans on libc `glob()` with
`FNM_EXTMATCH`/`GLOB_PERIOD`, and lists both as portability problems `[oils]`.

`[rec]` One hand-written byte-oriented pattern compiler in `bsh-lang`, shared by
all sites, `extglob` as a flag, with a backtracking budget so `*(a)*(a)*(a)…`
cannot hang the shell.

Separately, `[[ x =~ re ]]` needs **POSIX ERE** — leftmost-longest, POSIX
backreferences. The `regex` crate is leftmost-first and not ERE; do not use it.
Bind `regcomp`/`regexec` in `bsh-sys`. libc is the correct answer here.

### 4.9 Memory and allocation

Four allocation domains with different lifetimes. Conflating them is how shells
end up either leaking or copying everything on every command:

| domain | lifetime | strategy |
|---|---|---|
| tokens, AST, IR | one chunk | bump arena owned by the `Chunk`, freed wholesale |
| per-command expansion temporaries | one command | scratch sub-arena, reset between commands |
| stored values (vars, functions, history) | shell lifetime | heap, `Box<[u8]>` / refcounted |
| completion candidates | one keystroke | arena, dropped on re-query |

`bumpalo` fits domains 1, 2 and 4 exactly: parse once, use, free everything at
once, no per-node `Box`, no drop glue — and, relevant to §4.3, nothing to
deallocate in a forked child.

One hard constraint it imposes: **`bumpalo` runs no destructors**, so arena
types must not own heap. AST nodes hold `&'a [u8]` and `&'a [Node]`, never
`Vec`/`String`, or the arena frees the struct and leaks whatever it pointed at.
That constraint is a feature: it forces the flat, index-based AST of §4.2.

Three rules keep it honest:

1. **One `Bump` per chunk, owned by the chunk.** Never a global arena with
   `reset()`. An interactive shell accumulates `eval`'d code, function
   definitions and sourced files; a global reset would dangle any chunk that
   outlives the current line. Drop the chunk when the function is redefined.
2. **Stored values never point into a chunk arena.** `x=$y` must copy into the
   long-lived domain. Make that unrepresentable rather than documented: argv
   pieces are `&'a [u8]` tied to the chunk, stored values are an owned `Value`.
3. **Expansion is the allocation hotspot, not dispatch.** Per command we build
   argv plus each word's expansion; `Vec<Vec<u8>>` is thousands of small
   allocations in a `for` loop over builtins. Target `SmallVec<[&'a [u8]; 16]>`
   for argv, so a command with no splitting and no globbing allocates **zero**.

A counterpoint worth keeping, because it cuts against over-engineering this:
Cloudflare measured that boxing one closure per AST node "had negligible
performance impact" and that a memory pool was unnecessary ([blog][cf]). Arena
allocation here is justified by ergonomics and parse throughput on large
scripts, not by a claimed interpreter speedup. Measure domain 4 before
optimising it.

[cf]: https://blog.cloudflare.com/building-fast-interpreters-in-rust/

### 4.10 Diagnostics and error recovery

A shell must be able to say "you cannot write that here". No generated parser
can: Cloudflare's stated reason for hand-writing theirs was that their grammar
needed *contextual* parsing, which ruled out parser generators, and once you
replace a generated lexer with your own you are "so close to having a full
parser that the complexity outweighs any benefits" ([blog][cf]). The
error-quality argument follows from the same fact — a table-driven parser knows
the grammar but not what you meant. So: hand-written recursive descent.

But the parsing technique is not what makes RD errors *good*. Five mechanisms
are, and all of them must exist before the grammar is written:

**1. The expectation stack, doing triple duty.** One mechanism, three features:
what can be completed here (§6.2), whether the line is merely incomplete (PS2),
and what the error message should say.

```rust
pub enum ParseOutcome {
    Ok(ChunkId),
    NeedMoreInput { open: Span, expected: Vec<Expect> },  // → PS2, not an error
    Failed { errors: Vec<Diagnostic> },
}
pub struct Diagnostic {
    pub code: ErrCode,                        // stable, searchable
    pub span: Span,
    pub labels: Vec<(Span, &'static str)>,    // "opened here", "expected `fi`"
    pub help: Option<String>,
    pub fix: Option<Fix>,                     // machine-applicable rewrite
}
```

**2. `NeedMoreInput` is not an error.** Interactive shells need PS2
continuation, and an unterminated `if` is not a mistake until the user presses
Enter twice. Distinguishing "incomplete" from "wrong" in the *type* is what
stops the parser inventing errors mid-typing, and it is also what makes
completion work on partial input.

**3. An open-construct stack.** Push the span of every `(`, `$(`, `"`, `<<EOF`,
`if`, `{` as it opens. On failure the innermost entry yields "unclosed `(`
opened at 3:5" instead of "unexpected EOF". This one mechanism is most of the
difference between a parser people trust and one they fight.

**4. Recovery with synchronisation tokens.** For `bsh -n` and the linter you
want *all* the errors, not the first. Sync on `;`, newline, `fi`, `done`,
`esac`, `}` at matching depth, emit, continue. Cap the count and dedupe, or one
missing quote produces fifty cascading complaints.

**5. Stable error codes.** Oils tags every diagnostic with a searchable
`OILS-ERR-42` and maintains an error catalog with hints `[oils]`; the codes are
URL-shaped so they can become doc links. Do the same with `BSH-E####`. It makes
messages *learnable*, and lets you reword without breaking people's greps.

Rendering details that are not cosmetic: columns must be byte-accurate with tabs
expanded and wide characters counted once, or carets land in the wrong place on
real files; secondary labels ("opened here") carry most of the diagnostic value;
and a `fix` enables `bsh -n --fix`.

Testing: golden snapshot tests over a corpus of malformed scripts — Oils keeps
`test/parse-errors/` and a 27-case `spec/parse-errors.test.sh` `[oils]`. Note
that changing a message is a **breaking change** for anyone matching stderr,
which is why the code is stable and the text is not.

### 4.11 Rendering: miette, behind a boundary

`[rec]` Use miette (7.6.0, Apache-2.0) as the **renderer**, not as the error
type.

The reason is a hard constraint in its protocol rather than taste:
`miette::Report` and `Box<dyn Diagnostic>` want `'static`, but our spans borrow
the chunk. The way through is to call the trait's provided methods directly on a
borrowed `&dyn Diagnostic` and hand that to a report handler — `source_code()`
returns `Option<&dyn SourceCode>` with the lifetime of `&self`, so a view that
borrows the chunk is fine and `Report` never enters the picture. Keeping our own
IR also means `bsh-syntax` has no rendering dependency and diagnostics are
serialisable for tests and editors.

| layer | owns | depends on miette |
|---|---|---|
| `bsh-diag::Diagnostic` | code, span, labels, help, fix, severity | no |
| renderers | compact / graphical / JSON | optionally |
| `miette` | colour, snippets, wrapping, hyperlinks, `unicode-width` | — |

Consequences to design in now:

- **Chunks own their source as `Arc<[u8]>`.** A diagnostic must be renderable
  later — in `bsh -n` batch mode, from the linter, from a test snapshot —
  without borrowing an arena that has moved on.
- **Byte offsets versus `&str`.** `SourceSpan` is byte-based, which suits us,
  but `SourceCode` is `&str`-oriented and shell input can be invalid UTF-8. Do
  *not* use lossy conversion: it changes byte length (`U+FFFD` is 3 bytes) and
  every subsequent span lands in the wrong column. Project byte-for-byte
  instead, replacing each invalid byte with a single ASCII `?`, so all offsets
  stay exact.
- **`unicode-width` is a core miette dependency**, and it is what makes caret
  columns correct for wide characters — already on the §4.10 list.
- **Implement `miette::Highlighter` with our own lexer**, not `syntect`. We have
  the grammar; syntect's regex approach mis-highlights nested `$( … )` and
  heredocs, and it is a large dependency for a feature we can do correctly.

**Three renderers, chosen by context — the part I would argue about hardest:**

| context | output |
|---|---|
| interactive, stderr is a tty | ≤ 3 lines, no box drawing: `bsh: 3:4: unclosed '(' [BSH-E0102]`, plus one caret line |
| `bsh -n`, `lint`, `fmt` | full report: labels, "opened here", help, machine-applicable fix |
| stderr is not a tty | one line, greppable: `file:3:5: error[BSH-E0102]: …` |

miette's graphical theme is a *compiler report* format, which is right for
checking a file. It is wrong at a prompt, where the error competes with the next
prompt and the user types three commands a minute. bash prints one line for a
reason; do not let the pretty renderer become the default interactive one.

`url()` ties `BSH-E0102` to the catalog from §4.10, and with miette's
`supports-hyperlinks` the code becomes clickable in terminals that speak OSC 8 —
the stable-code decision paying off twice.

**Current state.** miette 7.6 gates `GraphicalReportHandler` behind its
`fancy-base` feature, which pulls `owo-colors`, `textwrap`, `terminal_size` and
the `supports-*` crates. The adapter therefore uses `NarratableReportHandler`,
available unconditionally, and the `fancy` feature stays dependency-free; the
borrowed-view adapter is handler-agnostic, so switching to graphical output is
one handler construction plus a feature declaration. Revisit once `-n`/lint
output is wired up and there is a real report to compare against the
hand-written one — colour and OSC 8 hyperlinks are the only things the
narratable handler does not give us.

Dependency discipline: `default-features = false`, enabling only colour/unicode
support and the handler actually used. Leave `syntect`, `backtrace` and
`textwrap` off, and build `--error-format=json` with serde on our own
`Diagnostic` rather than on miette's types.

---

## 5. Execution: the bytecode VM

### 5.1 What gets compiled

A **chunk** is one compilable unit: a function body, a `-c` string, a sourced
file, or an interactive line. Chunks are `Vec<u32>` code plus side tables.

There are **no variable registers**. Shell variables are dynamically scoped and
name-addressed; the VM stack holds only expansion temporaries and loop
iterators. This is why a shell VM is easier than a general-language VM: no
register allocation, no closures (bash functions are global), no stack-frame
GC, no static types, no inlining decisions. The hard part of a normal VM is
absent. The hard part of *this* project — IFS, `${x:-"$@"}`, `set -e` — is
identical in a tree-walker, so **bytecode will not improve conformance**. Be
clear about that going in.

### 5.2 The instruction set

Design rule, and it is the one that keeps the ISA small:

> **Words are operands, not instructions.** Word expansion depends on runtime
> state, so it is a *call*, not compiled code.

```rust
enum Op {
    ExecSimple  { words: WordListId, redirs: RedirListId, assigns: AssignListId },
    // `pipefail` is sampled when the pipeline STARTS, not when its
    // status is computed (POSIX 2024 §2.9.2) — see §2.
    ExecPipeline{ stages: u8, negated: bool, pipefail: bool },
    JumpIfOk/PushStatus/Jump(u32)         // && || ; control flow
    ForEach  { var: NameId, words: WordListId, body: ChunkId },
    While/Until { cond: ChunkId, body: ChunkId },
    Case     { word: WordId, arms: ArmTableId },
    CallFunc { name: NameId, words: WordListId },
    Subshell (ChunkId) | BgJob (ChunkId),
    PushHandler { kind: HandlerKind, target: u32, depth: u16 },
    PopHandler,
    Return | Break(u8) | Continue(u8) | Exit,
    Noop(Span),
}
```

Roughly 25 opcodes. If a proposed opcode mentions a word-expansion detail, it
belongs in `bsh-lang`, not the ISA.

### 5.3 Control flow as unwinding — the actual argument for a VM

Shell's control flow is exception semantics, and every tree-walker ends up
hand-rolling it with `Result` at every node:

| shell construct | exception analogue |
|---|---|
| `set -e` | throw on non-zero status |
| `trap ERR` | catch handler for the same throw |
| `return N` | unwind one function frame |
| `break N` / `continue N` | unwind to the Nth loop |
| `exit N` | unwind everything, then run the `EXIT` trap |
| `trap RETURN` | frame-exit hook |
| `… \|\| true`, `if`, `while`, `!` | suppressed-throw regions |

Compile that to a **handler table** — `{ start_pc, end_pc, kind, handler_pc,
stack_depth }` — exactly like JVM/CLR exception tables:

```rust
enum HandlerKind { Errexit, TrapErr, Loop(u8), FnFrame, Exit }
```

Consequences, all good:

- The `set -e` policy lives in **one** `unwind(status)` function, testable
  directly, instead of being smeared over 40 AST nodes.
- "Suppressed context" is a compile-time property of the region, so it costs a
  frame flag and zero instructions in the common path.
- The genuinely under-specified question — does `break` inside a function break
  the caller's loop? — becomes a one-line policy on how far to unwind, not an
  architectural problem. (POSIX leaves it unspecified; bash does unwind
  outwards.)
- `trap DEBUG` and the debugger (§5.6) hook the dispatch loop, not every node.

This is a real, non-speed argument for bytecode, and it is the one I would put
in the README.

### 5.4 Spans

Every `Op` carries a `Span`, or indexes a parallel span table. Non-negotiable:
`xtrace` line numbers, `bash -x` `file:line`, error carets, and the debugger all
read from it. Retrofitting spans after the compiler exists means rewriting the
compiler.

### 5.5 Caching

Key: `(source bytes, profile options, parse-affecting opts)`. Invalidate on
option change. Cache function bodies by definition site; cache sourced files
by `(path, mtime, size)`. Interactive chunks are cached by line text so
re-running the previous command is a lookup. Optional later: persist the cache
to disk like zsh's `.zwc` `[zsh]` to cut interactive startup.

`eval` and `trap` compile at run time; that is fine, the compiler is small.

### 5.6 What bytecode buys the interactive shell specifically

1. **Compile the line before running it.** The REPL accumulates source until it
   parses, compiles, then executes — so a syntax error is reported *before* any
   side effect, which is what users expect and what a naive tree-walk REPL gets
   wrong on multi-command lines.
2. **`set -n` is free** (compile, don't run), and the `--posix` bashism check is
   an IR walk.
3. **A real debugger.** With instruction-level spans you get `break file:42`,
   `step`, `next`, `print $x`. bashdb exists only because bash has no VM — it
   fakes breakpoints with `trap DEBUG` and source-line juggling. This is a
   genuinely novel interactive shell feature and it is cheap *only* on a VM.
4. **`bsh explain`** — dump the expansion stages and final `argv` for a command
   (§9).
5. **Serialisable chunks** for faster startup.

### 5.7 What it costs, and the escape hatch

Costs: three artifacts to keep in sync (AST, IR, VM); a compiler to debug; and
spans that silently degrade error messages if forgotten.

`[rec]` Sequence it *after* semantics are nailed:

> Build an **AST interpreter** first (milestones 2–4), ship the interactive
> shell on it, then add compiler + VM behind `--interp=vm`, and keep the AST
> interpreter forever as a **test-only backend**. Run the entire spec suite
> through both and diff. Two independent implementations of the same semantics,
> differential-tested against 3971 cases, is worth the maintenance: it is how
> you find out that your compiler's `||` handling differs from your evaluator's.

If the debugger is the goal, you may invert the order — but then write the
spec-suite harness before the compiler, not after.

### 5.8 Prior art

`[zsh]` zsh compiles functions and scripts to **wordcode** with `zcompile`
(`.zwc`), and `execlist()` executes that representation — so a compiled shell is
not exotic. But dash, bash, mksh, and OSH are all tree-walkers; Oils' "bytecode"
work is about running Oils *on* Python bytecode, not a shell VM. So there is
one precedent, and no one to copy the ISA from. Keep it small.

### 5.9 Dispatch technology — and why *not* to chase it

The obvious reference here is Cloudflare's wirefilter write-up ([blog][cf]),
commonly cited for "fast interpreters in Rust". Its measured conclusions are
worth reading precisely, because they do not say what people assume:

- Compiling each AST node to a **boxed closure** (`Box<dyn Fn(&Ctx) -> bool>`)
  beat AST interpretation by only **10–15%**, with a dynamic call per AST level.
- The large win (~2x) came from something else entirely: **pre-resolving
  dynamic lookups** into a slot array on an execution context, so per-node work
  stopped repeating name lookups — and from reporting type errors at set time
  rather than execute time.
- They considered and rejected a JIT, and noted that allocating many small
  boxed closures "had negligible performance impact", so a pool was not needed.

Translation for a shell: **dispatch mechanics are a small term, and the win is
in moving work out of the inner loop.** For a shell the term is smaller still,
because most statements end in a syscall — `fork`/`exec` costs thousands of
times more than any dispatch choice.

So:

`[rec]` Implement the VM as a plain `loop { match chunk.code[pc] { … } }` over
`Vec<Op>`.

- Portable stable Rust, no `become`/`musttail` tricks, no computed goto (which
  Rust does not offer), no `unsafe` — which keeps `#![forbid(unsafe_code)]`
  intact across every crate but `bsh-sys`.
- Trivially debuggable, which matters more than 10% when the thing being
  debugged is `set -e` unwinding.
- The escape hatch, if dispatch ever shows up in a profile, is Cloudflare's
  landing spot — compile each op to a closure — not threaded code. It is
  simpler, safe, and measured.

**Spend the optimisation budget on the shell analogue of their slot array
instead**: resolve at compile time everything that is statically knowable, and
guard it.

```rust
enum Words {
    Static(&'a [&'a [u8]]),   // every word literal after quote removal:
                              // no `$`, no glob metachars, no `~`
    Dynamic(WordListId),
}
enum CommandRef {
    Special(Builtin), Intrinsic(Builtin),
    Resolved(PathId, Epoch),  // PATH hit, invalidated when PATH changes or hash -r
    Function(DefId, Epoch),   // invalidated when the function is redefined
    Search,                   // non-literal word: full dynamic search
}
```

`ls "foo bar"` and `make -j8` then run with a pre-built argv and no expansion
work at all, which is the overwhelmingly common interactive case. Two epochs
(PATH, function table) keep it honest: `hash -r` bumps one, a redefinition bumps
the other, and a stale `Resolved`/`Function` falls back to `Search` instead of
being wrong.

Caveats: a "literal" word can still glob, so `Static` requires no unquoted glob
metacharacters; and `IFS` does not affect literal words, so no splitting check
is needed. Keep an `--no-static-argv` switch so the two paths can be
differential-tested (§5.7).

---

## 6. Interactive architecture

### 6.1 Latency budget

| interaction | target | notes |
|---|---|---|
| `bsh -i` to prompt | < 10 ms | dash ≈ 1 ms, bash ≈ 5–10 ms. Rust can win this; don't blow it on eager PATH indexing |
| keystroke → echo | < 5 ms | own key decoder, no allocation per keystroke |
| keystroke → autosuggestion | < 16 ms | from an in-memory history index |
| TAB → first paint | < 30 ms | **paint from cache synchronously**, stream slower sources after |
| TAB → full results | < 150 ms | cancellable; a new keystroke aborts the old query |

The rule that makes this achievable: **never block the render on a provider**.
Paint the fast tier (prefix matches from an index), then merge in streamed
results. This is exactly the model `nucleo` was built for, and we can implement
its snapshot pattern without its threadpool.

### 6.2 Why the parser is the moat

bash must guess what you are completing: it splits the line on
`COMP_WORDBREAKS`, guesses the command name, and hands the pieces to a shell
function. It cannot tell that you are on the third word of a `case` pattern,
inside an unterminated `$(`, or after a redirect operator.

We have the grammar. That buys, with no heuristics:

- Correct word index and the boundary of the word under the cursor, from real
  token spans.
- Exact quoting state, so insertion is correctly escaped (`ls My\ File`,
  `"$var"`, `'…'`) — a completion that inserts an unescaped space is worse than
  no completion.
- Completion **inside constructs**: `for f in <TAB>` → files; `case $x in <TAB>`
  → `*)` and the enclosing function's known tags; `> <TAB>` → files; `[[ -<TAB>`
  → test operators; `${<TAB>` → parameter expansion operators; `command -v
  <TAB>` → functions *and* commands.
- Profile awareness: in `--posix` mode, complete only what POSIX allows.

### 6.3 The parser must answer "what goes here?"

This is a parser requirement discovered by the completion feature, so design it
in now or pay for it forever.

The mechanism — an expectation stack that records the production at every rule
push, plus a `NeedMoreInput` outcome instead of an error at EOF — is specified
in §4.10, because it is a parser feature first: it serves error messages,
PS2 continuation and completion, in that order of seniority. This section is
about the third consumer.

```rust
pub enum Expect { Command, Word, RedirectTarget, ForList, CasePattern,
                  ParamOp, TestOp, ArithExpr, … }

/// Parse a possibly-truncated line and report what is valid at `offset`.
pub fn expectations_at(src: &[u8], offset: usize, opts: &Opts) -> Vec<Expect>;
```

`Expect` maps 1:1 to a provider set (§6.4), so completion is data-driven from
the grammar rather than per-command special cases. The lexer must likewise
treat EOF mid-token as normal, for both this and live syntax highlighting.

### 6.4 Completion pipeline

```
line + cursor
    │
    ├─ lex/parse prefix ──▶ CompletionCtx {
    │                          word_span, quote_state, Expect,
    │                          command_name, arg_index, … }
    ▼
providers ──▶ Vec<Candidate> ──▶ rank ──▶ render ──▶ insert(quoted)
  │              │                 │
  fact tier      │            tier 0: typed prefix, by frecency
  (sync, cached) │            tier 1: fuzzy, by nucleo score
  slow tier ─────┘
  (streamed, cancellable)
```

```rust
pub struct Candidate {
    pub insert: BString,       // bytes; authoritative, may be non-UTF-8
    pub display: String,       // UTF-8, for rendering only
    pub kind: CandKind,        // Path | Command | Var | Opt | Branch | Host | …
    pub desc: Option<String>,  // "branch, 3 days ago"
    pub frecency: f32,
    pub tier: u8,
}
```

Two tiers, always. `git ch<TAB>` must never be beaten by a fuzzy match on
something else: typed-prefix matches outrank fuzzy matches regardless of score.
nucleo's `prefer_prefix` helps but is a scoring bonus, not an ordering
guarantee `[nucleo]`.

### 6.5 Ranking: `nucleo-matcher`, not `nucleo`

`[nucleo]` Use the **matcher** crate, not the worker crate:

- `nucleo-matcher` 0.3.1 depends on `memchr` only (+ optional
  `unicode-segmentation`). **No threads.**
- `nucleo` (the high-level crate) pulls in `rayon` and `parking_lot` and runs a
  background threadpool. That is what §4.3's gate would have to police.

The matcher is a poor fit for nothing we need:

- `Config::prefer_prefix` is documented as *"recommended for autocompletion
  usecases where the expectation is that the user is typing the entire match"*
  `[nucleo]` — literally our case.
- `Config::match_paths()` applies separator bonuses for path candidates
  `[nucleo]`.
- `Matcher::fuzzy_indices` returns match positions, which we need for
  highlighting the matched characters in the menu.
- Licence is **MPL-2.0** (file-level copyleft, fine as a dependency; note it if
  the project is Apache/MIT).

Two integration wrinkles worth knowing up front:

- nucleo matches on `Utf32Str`, i.e. **decoded text**. Shell candidates are
  bytes. So the matching path is lossy-UTF-8 while the *insertion* path must
  stay exact bytes (`insert: BString`). Non-UTF-8 filenames can still be
  inserted; they just match against a lossy projection.
- Path completion in a shell wants *component* matching (`src/ma` →
  `src/main.rs`) more than substring matching. Use the component prefix as
  tier 0 and let nucleo rank only the final component; don't feed it a full
  fuzzy query and hope.

Cost is negligible: 5–20 k candidates × one `fuzzy_match` is well under a
millisecond and needs no parallelism, which is precisely why staying threadless
is viable.

### 6.6 Candidate sources

Ordered by value per unit of work:

1. **Paths** — component-prefix, cached `readdir` per directory with an mtime
   check, hard cap (10 k entries, then "refine") to avoid a hang in `~/.cache`.
2. **Command index** — PATH walk cached in memory, rebuilt when `PATH` changes
   or a directory mtime moves; built lazily off the prompt, never at startup.
3. **Variables and parameter operators** — cheap, from the var store, filtered
   by `Expect`.
4. **History** — for both autosuggestion and completion; frecency-weighted.
5. **Options/flags** — a curated table for our builtins plus a small database
   for common tools; `compgen -W` covers the rest.
6. **Domain providers** — git (branches, refs, remotes), ssh (hosts from
   `known_hosts`/`config`), `kill` (pids), `man`, `systemctl`, package managers.
7. **bash-completion compatibility** — see below; this is the difference
   between a toy and a daily driver.

`compgen`'s `-W`, `-F`, `-c`, `-d`, `-f`, `-v`, `-e`, `-a`, `-A` modes should be
implemented as *provider adapters*, not as a second code path.

### 6.7 bash-completion

For git/docker/kubectl/ssh/systemctl/apt to complete at all, the shell must run
the existing `bash-completion` package: ~4 000 shell functions that use
`complete`, `compgen`, `compopt` and read `COMP_WORDS`, `COMP_CWORD`,
`COMP_LINE`, `COMP_POINT`, `COMP_WORDBREAKS`, `COMP_TYPE`, `COMP_KEY`. Those
must be implemented, and completion functions must be executed re-entrantly by
the interpreter *while the line is being edited*, with the current state
save/restored and `set -e` suppressed inside them.

Two refinements that make it better than bash while staying compatible:

- Build `COMP_WORDS` from the **real parser's tokens** rather than splitting on
  `COMP_WORDBREAKS`. Compatible values, better accuracy.
- Keep a registry of native providers that are consulted *before* falling back
  to a bash function for the same command, so `git` gets a fast typed provider
  and any uncovered subcommand falls through to bash-completion.

### 6.8 Presentation

Three levels, so intensity matches intent:

1. **Ghost text** — dim inline continuation (fish-style), accept with `→`/`End`.
   Cheapest, highest value per keystroke, and it teaches the history.
2. **Inline menu** — on TAB, a multi-column menu with a description column
   (`checkout` │ *switch branches* │ `3 days ago`). Matched characters
   highlighted via `fuzzy_indices`.
3. **Full picker** — on a second TAB (or a key), a nucleo-style full screen with
   a preview pane (commit subject for a branch, file head for a path, help text
   for a flag) and multi-select. **This is the one place a worker thread earns
   its keep**, which §4.3 allows because no process is forked while the picker
   is open — start the pool on open, tear it down on close.

Rendering rules that matter more than the layout: never reflow under the cursor,
never steal the terminal without restoring it, and degrade to plain text when
`TERM`/`COLORTERM` says so.

### 6.9 Speed and correctness of insertion

The last 10% that decides whether completion is loved or disabled: insert the
correctly quoted form for the current context (unquoted → escape spaces/globs;
inside `"` → escape `$`, `` ` ``, `\`, `"`; inside `'` → close, insert, reopen);
directory results end with `/` and do not gain a trailing space; a unique match
without a trailing space if it is a directory; and never insert something that
changes the parse into a different construct.

---

### 6.10 Syntax highlighting

The editor repaints the current line on every keystroke, so the classifier runs
on input that is *not yet valid*. That rules out anything needing a successful
parse: highlighting is driven by the lexer alone, with unterminated quotes and
substitutions treated as ordinary token classes rather than errors.

`bsh-highlight` therefore maps `TokenKind` to `Style` and does nothing else.

- **Role-based, not RGB.** A theme says "keywords are bold magenta"; the
  renderer decides how to paint it. The same theme then drives the line editor,
  the completion menu and (later) `miette`'s snippet highlighter, and
  `NO_COLOR`/`TERM=dumb` degrade by swapping in `Theme::PLAIN`.
- **Bytes in, bytes out.** `render` returns `Vec<u8>`, so a line holding
  undecodable bytes survives being painted. Routing display through `str` would
  force a lossy conversion on every keystroke.
- **Unterminated constructs get their own style**, which is why
  `TokenKind::WordUnterminated` exists: a dangling quote should look *open*, not
  wrong, because the user is probably still typing it. Two more roles earn their keep once the lexer
  distinguishes them: `TokenKind::ArithCommand` covers a whole `(( … ))` as one
  opaque token (we do not lex arithmetic), and `HereDocBody`/`HereDocDelimiter`
  are styled as literal data, because a here-document body is not shell code.
- **Re-lex the buffer; don't diff it.** A command line is a few hundred bytes,
  and incremental highlighting would cost more in bookkeeping than it saves.
  Revisit only if a profile disagrees.
- **Semantic highlighting is a separate, optional pass.** Colouring a command
  name that is not on `PATH` (as fish does) needs a lookup per token. Worth
  having, but it must sit inside the keystroke latency budget (§6.1), not
  outside it.

`unicode-width` is the one dependency this needs, so a wide character cannot
shift the rest of the line; it arrives with the cursor-movement code rather than
being approximated.

## 7. Hard parts, ranked

The parser and the VM are the fun parts. Ranked by effective effort:

1. **Word expansion + field splitting.** `${x:-"$@"}`, IFS (`IFS=:` vs `IFS=`
   vs whitespace-IFS trailing-delimiter rules), `$*` vs `$@` quoted and
   unquoted, `"$@"` vanishing with zero args. ~60% of spec failures live here,
   and it is the same work in a tree-walker or a VM.
2. **`set -e` / traps / function frames.** Unbounded in the spec; contained by
   §5.3 if the handler table is right.
3. **The line editor and completion.** Not conceptually hard, but a real editor
   needs wide characters, grapheme clusters, bracketed paste, multi-line with
   continuation prompts, resize, and a key-decoding table that matches
   readline's muscle memory closely enough that nobody notices. This is now
   core, not a side quest, and it is where "feels good" is won or lost.
4. **Job control.** Process groups, `tcsetpgrp`, `SIGTTOU`/`SIGTTIN`,
   `WUNTRACED`, terminal restore after a stopped child, and the documented
   incompatibilities — Oils records that `lastpipe` + job control + `DEBUG`
   trap are mutually exclusive `[oils]`.
5. **Subshell process identity.** `$$` stays the parent's pid `[spec]`; `$!` is
   this shell's last async pid; `$PPID` is the real parent (bash extension);
   `jobs` inside a subshell; `wait`.
6. **bash-completion fidelity.** Thousands of functions with undocumented
   reliance on `COMP_*` details.
7. **`printf`, `test`/`[`, `getopts`, `read` exactness.** Unglamorous, highly
   testable; POSIX `test`'s 0–4 argument algorithm plus bash's 5+ grammar is a
   genuine oddity.
8. **`echo`.** Unspecifiable. A per-profile policy table and spec-test-driven
   tests. Accept that it is policy, not rule.

---

## 8. Conformance strategy

1. **Vendor `spec/` immediately.** Done: `spec/oils/` is an unmodified copy of
   the upstream `spec/` tree (223 files, 3971 cases, Apache-2.0, commit recorded
   in `spec/README.md`). Cases use `#### name` plus a `## STDOUT:` block, with
   per-shell overrides like `## OK dash STDOUT:` and annotations such as
   `## compare_shells:` and `## our_shell:`. Write a Rust harness that runs our
   shell plus the reference shells on `$PATH`; the per-shell override mechanism
   *is* the ambiguity budget made executable. Skip `## our_shell: ysh` cases —
   80 files cover a language we are not implementing.
2. **Conformance ratchet.** Pass count per file; CI fails on a drop. Put the
   number in the README. It is the only real progress metric.
3. **Real-world corpus** `[oils]`: every autoconf `configure` script (brutally
   portable shell, the best corpus that exists), Debian maintainer scripts,
   `/etc/init.d`, plus `bash-completion`'s own function files. Gate: parse 100%,
   then execute.
4. **Lossless property test** — parse then unparse is byte-identical, fuzzed.
5. **Differential fuzz the expander** against `dash`/`bash` on generated words.
6. **Differential test the two interpreters** (§5.7) over the whole suite.

---

## 9. What makes this worth building

1. **Completion that knows the grammar** (§6.2, §6.3). This is the product. No
   existing shell can copy it without rewriting its front end.
2. **A real debugger** (§5.6): breakpoints by `file:line`, stepping, `print`.
   Only cheap on a VM.
3. **`bsh explain`** — print every expansion stage and the final `argv`. Every
   shell user debugs field splitting by guessing. A thin client over the
   expander we must build anyway.
4. **`bsh lint --posix` / `bsh fmt`** — correct, because the parse is lossless
   and spans exist. `checkbashisms` is a pile of regexes; this would be real.
5. *(optional)* **Embeddability** — `bsh-runtime` as a library for build tools
   that want typed capture instead of a string from `sh -c`. Free byproduct of
   the crate split, not a goal.

---

## 10. Milestones

Interactive-first ordering: a usable shell early, conformance and VM after.

| # | Deliverable | Gate |
|---|---|---|
| 0 | Vendor `spec/`, harness, run against bash/dash | harness agrees with Oils on N cases |
| 1 | Lexer + parser + lossless + `-n` + diagnostics + **expectation queries** | lossless invariant on corpus; `expectations_at` correct on truncated input; golden diagnostics stable; `NeedMoreInput` drives PS2 |
| 2 | AST interpreter: commands, redirects, expansion, control flow, functions, pipelines | spec: `command_`, `redirect`, `word-split`, `quote`, `var-sub`, `if_`, `loop`, `case_` |
| 3 | REPL v1: raw mode, own key decoder, history, prompt; wire in `bsh-highlight` (§6.10) | usable daily for simple commands, with a highlighted line |
| 4 | Completion v1: paths/commands/vars via `expectations_at`, `nucleo-matcher` ranking, insertion quoting | feels better than bash for `cd`/`ls`/`git` |
| 5 | Completion v2: frecency, descriptions, menu + ghost text + picker, bash-completion compat | `git`, `docker`, `ssh` complete as in bash |
| 6 | Job control, signal handling, terminal restore; core builtins; §2.8.1 errors | pexpect/stateful tests; `Ctrl-C`/`Ctrl-Z` behave |
| 7 | Compiler + VM behind `--interp=vm`; AST interpreter kept as test oracle | whole spec suite passes identically under both |
| 8 | Debugger (`break`, `step`, `print`), `explain`, `lint`, `fmt` | — |
| … | ongoing conformance ratchet | → ~90% of spec, autoconf executes |

Realistic: steps 0–5 (a shell you would actually use, with best-in-class
completion) is the part worth committing to first, and it is maybe 6–9 months of
focused work. Full conformance plus VM plus debugger is a multi-year project at
Oils' level of rigour; steps 7–8 are strictly optional for the product.

---

## 11. Open questions

1. **readline muscle memory vs. better bindings.** How far do we go? A vi mode,
   a readline-compatible emacs mode, and a "fish-like" mode is three keymaps and
   a maintenance tail. `[rec]` ship readline-compatible emacs mode only in v1;
   the muscle memory is the adoption cost.
2. **Does the VM start at milestone 7, or earlier?** I recommend after, with the
   AST interpreter as a permanent differential oracle. If you want the debugger
   sooner, invert — but build the spec harness first.
3. **`nameref`** is now on the critical path (bash-completion needs it), so the
   `resolve` hook is not optional.
4. **`ß` as a binary name.** Case folding, argv[0] dispatch, filesystem
   portability — and it has already bitten: with a checkout path containing
   `ß`, linking fails outright in this environment, because non-ASCII bytes in
   a linker argument get mangled (`303237` is `ß` spelled in octal; see README).
   `bsh` for the binary, and consider an ASCII checkout path.
5. **Do we ship a `sh` symlink and claim `/bin/sh`?** It changes the conformance
   bar we are advertising, and the default profile.
