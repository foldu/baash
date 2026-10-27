# Cross-crate contracts

Written as crates land. The failure mode this file exists to prevent: an agent
documents a contract in its branch report, the next agent never sees it, and the
two assumptions quietly disagree six weeks later.

Each section is owned by the producing crate. If you are implementing a consumer
(`bsh-vm`, `bsh-repl`, `bsh-tools`) read the relevant sections before you start,
and treat every item here as a fact about the API you are calling.

Status: all of wave 1 has landed — `bsh-diag`, `bsh-sys`, `bsh-runtime`,
`bsh-lang` and `bsh-syntax`.

---

## bsh-diag

Renderers take `&Diagnostic` + `&SourceFile`; nothing borrows a parse arena, so
a diagnostic can be rendered long after the chunk that produced it is gone.

| renderer | use |
|---|---|
| `render_compact` | interactive: ≤ 3 lines, two-space indent, no box drawing |
| `render_report` | `-n`, lint, fmt: gutter, context, secondary labels, `help:` |
| `render_json` | editors and machine consumers; hand-written, stable key order |
| `render_line` / `render_line_with_help` | one greppable line; never ANSI |
| `fancy::render_fancy` | miette narratable report, behind the `fancy` feature |

- No renderer emits ANSI. Colour is the terminal layer's problem.
- Byte → display: **one cell per invalid byte** (`U+FFFD` for display), so byte
  offsets and columns stay in step. `TAB_WIDTH = 4`, `CONTEXT_LINES = 3`,
  `MAX_DIAGNOSTICS = 32`.
- `render_line` flattens newlines, tabs and control characters (including ESC), so
  a hostile filename cannot break the line format.
- The IR has no `Fix` yet, so no renderer produces a machine-applicable fix.
- `fancy` is off by default. `NarratableReportHandler` is used because miette
  7.6 gates `GraphicalReportHandler` behind `fancy-base`; the adapter is
  handler-agnostic, so enabling graphical output is one construction line.

---

## bsh-sys

**The fork gate is global and singular.** `bsh_sys::fork_gate()` returns the one
process-wide `ThreadGate`. Threaded modes — the completion picker, the debugger
— must hold `fork_gate().enter()` guards for as long as their threads live, and
no other crate may construct its own gate for fork checks. `posix_spawn`
deliberately does not consult the gate, because it shares the address space and
is safe while threaded; only `fork` is gated.

**Spawn plans carry no environment and no search.** `SpawnPlan::new` takes an
already-resolved program path and the full argv *including* `argv[0]`, and
starts with an **empty** `envp` (POSIX semantics). The runtime must push
exported `NAME=value` entries. There is no `posix_spawnp`, no `PATH` lookup and
no shebang handling here — those are VM policy.

**Signal state must be set on every plan.** Set `plan.sigmask =
Some(SignalSet::empty())` and populate `reset_signals` from trap state: `exec`
resets caught signals to default, but a signal set to `SIG_IGN` stays ignored, so
a `trap '' INT` leaks a Ctrl-C-proof child unless the plan says otherwise.

**Exec failures are `io::Error` in the parent, not a child status.**
`ENOENT`/`EACCES`/`ENOEXEC` come back as errors; map them to 127/126 rather than
inspecting the child's exit status. The report pipe makes this reliable: EOF on
the read end means `exec` succeeded.

**`WaitOutcome::exit_status()` folds `Stopped` into 128+signal.** For `$?` the
VM must match on the enum (`Exited`/`Signaled`/`Stopped`), not on this helper,
or a stopped job will look like a failure.

**`SIGCHLD` is a wakeup hint, not an event.** Until `wait_any_nohang` returns
`None`, there may be more children to reap.

**`set_foreground_pgrp` assumes the caller is the only thread that could receive
`SIGTTOU`** — true for the single-threaded event loop or a fork-gated mode,
which is why the gate exists.

Linux-only: `SignalFd` (elsewhere `new`/`read` return `Unsupported` and
`as_raw_fd()` is `-1`), `memfd_create`, `O_TMPFILE`. Everything else is generic
POSIX.

Known gap that affects `bsh-repl`: **`RawMode`, `window_size` and `tcsetpgrp`
have no real-tty test.** The sandbox that built them has no controlling
terminal. pty-based integration tests are owed before the editor is trusted.

---

## bsh-runtime

**`ShellHost` is the VM's obligation.** It now *requires* `state()`,
`run_source()`, `write_out()` and `write_err()`; `is_interactive()` and
`diagnose()` have defaults. `state()` hands out `ShellState { opts, vars }`.

**The per-command sequence is fixed, and the order matters:**

```
resolve the command's tier
→ Vars::apply_prefix_assignments(assigns, tier)
→ run the builtin (BuiltinDef::run(host, argv))
→ Vars::finish_prefix_assignments()
→ host.state().take_error() → consequence(error, interactive)
→ check take_exit_request()
```

Skipping `finish_prefix_assignments` leaks a temporary local frame into every
later command; running it before the builtin makes `FOO=1 export FOO` behave as
if the assignment were discarded.

**Prefix assignments persist for special built-ins in both profiles.** POSIX
requires it; bash (non-posix) drops them and `bash --posix` persists them. We
follow POSIX unconditionally — a deliberate deviation recorded in DESIGN §3.3
Tier C.

**`$@` and `$*` are not here.** `special_scalar` returns `None` for them because
they are multi-field; expansion reads `positional().argv` and applies the
quoting rules itself.

**Integer attributes store raw bytes.** Arithmetic evaluation happens in
`bsh-lang`/the VM *before* `assign`; the store does not parse numbers.

**Construct `Opts` from the presets** (`Opts::POSIX`, `Opts::BASH`, `Default`).
It carries ~70 flags now; an exhaustive struct literal outside this crate will
break on the next flag.

**Builtin tiers are exact and load-bearing** (DESIGN §2): 15 special, 16
intrinsic, 7 regular, 18 bash extensions. `printf`, `test`, `[`, `echo`, `pwd`,
`true` and `false` are regular, deliberately *not* intrinsic, because the
standard requires them to remain exec-able. Search order comes from
`Tier::search_priority`.

**Diagnostic text is unstable; match the codes.** State errors are `BSH-E05xx`;
an unimplemented builtin returns status 2 with `BSH-E0508`.

---

## bsh-lang

**One object implements both traits.** `Vars` (read variables, positional
parameters, write) and `Shell` (the callbacks expansion needs) must be
implemented on the same type, because expansion takes `&mut S` where
`S: Vars + Shell`.

```rust
pub trait Vars  { fn get(&self, name: &[u8]) -> Option<&[u8]>;
                  fn positional(&self) -> &[Vec<u8>];
                  fn set(&mut self, name: &[u8], value: Vec<u8>) -> Result<(), SetError>; }
pub trait Shell: Vars {
    fn command_substitution(&CommandSub) -> Result<Vec<u8>, Error>;
    fn arithmetic(&Arithmetic) -> Result<Vec<u8>, Error>;
    fn pathname(&Field) -> Result<Option<Vec<Vec<u8>>>, Error>;
    // defaulted: process_substitution -> Unsupported, home_dir
}
```

- `get` must serve the specials `?`, `-`, `$`, `!`, `0` as ordinary lookups;
  `$#` is computed from `positional()`. `None` (unset) and `Some(b"")` (null) are
  different values and the difference is observable under `${x-}`, `${x:-}` and
  `set -u`.
- `pathname` returning `None` means *no match* — leave the word alone. Returning
  `Some(vec![])` would mean "matched nothing and vanished", which is `nullglob`
  behaviour and not the default.
- `expand_words` returns `Vec<Vec<u8>>`. If the VM wants `SmallVec` argv it
  collects into it; the expansion layer does not guess the allocation strategy.
- `quote(bytes, ctx)` covers `Unquoted`, `DoubleQuoted` and `SingleQuoted`, and
  emits `$'\n'` for an unquoted newline (a bare backslash-newline is a line
  continuation and would corrupt the line). `bsh-complete` must use this rather
  than its own escaping.

**Two properties, deliberately separate.** A part is *quoted* (glob-inert) or
not, and separately a part's bytes are *expansion results* (splittable) or not.
Literal source text is unquoted but never field-split — `IFS=:; a:b` is one
word — while unquoted literal text inside a parameter replacement word is
splittable — `${x:-a:b}` is two fields. Collapsing these into one boolean is the
classic way to get IFS subtly wrong.

**Unsupported is an error, not a guess.** `${x#pat}`, `${x%pat}`, `${x/p/r}`,
`${!x}`, `${x@Q}`, `${x^^pat}`, `${@:o:l}`, arrays and process substitution
return `ErrorKind::Unsupported` with the operator's span. The parser must map
those surface forms to the corresponding `ParamOp`, so the error can point at
the right place instead of failing later.

**Errors** are `bsh_lang::Error { kind, span }` with `Display`, `code()` and
`diagnostic() -> bsh_diag::Diagnostic`. Codes 301–306 are provisional.

Documented deviation: for unquoted `$@` inside a `${…}` replacement word we
apply the ordinary unquoted rules, where bash space-joins and suppresses
splitting. Every DESIGN §3.3 Tier C case, including `${x:-"$@"}`, matches
bash 5.3.20.

---

## bsh-syntax

**Two entry points, and the difference matters.** `lex(src) -> Vec<Token>` is the
token stream for highlighting and simple scans. `lex_full(src) -> Lexed` adds
what the parser needs: `{ tokens, heredocs, unterminated }`. Here-document bodies
and delimiters only appear as `TokenKind::HereDocBody` /`HereDocDelimiter` in
the full form; a consumer that colours them must use `lex_full`. Bodies are
**not** shell code and are styled as literal data, not as commands.

**Words are `bsh_lang`'s types, re-exported.** `bsh-syntax` re-exports
`Word`, `Part`, `Lit`, `LitKind`, `ParamExp`, `ParamName`, `ParamOp`,
`CommandSub`, `Arithmetic`, `ProcessSub`, `TildeSpec`, `UnsupportedOp`. There is
no parallel word model, deliberately.

**The AST is owned and flat**: `Ast { root, nodes: Vec<Node>, heredocs, tokens }`
with `NodeId` indices, not a boxed tree and not an arena. Two consequences:
recursion on the AST is bounded by construction, and the parser enforces an
explicit `max_depth` (default 200) that yields a diagnostic instead of a stack
overflow. Node kinds: `Empty, List, AndOr, Pipeline, Simple, If, For,
ForArith, While, Case, BraceGroup, Subshell, Function, Redirected, ArithCommand,
TestCommand, Timed`.

**`ParseOpts { posix_mode, extglob, max_depth }`** is local to the front end on
purpose; the shell maps its own options onto it. `parse(&SourceFile)` is a
default-profile wrapper over `parse_with`.

**`NeedMoreInput` is the PS2 signal**, produced for an unterminated quote,
substitution, `(`, `{`, `if`, `do`, `case`, here-doc, redirect target, and
dangling pipeline/and-or tails — each with the opener's span and the `Expect`
set. `expectations_at(src, offset, opts)` answers the same question at an
arbitrary offset for the completer.

**Arrays are a gap with a shape.** `bsh-lang` has no array-literal word part, so
`local x=(a b)` parses with the word at that position replaced by a quoted
placeholder and the elements carried in `SimpleCommand::array_args:
Vec<ArrayArg>` (only after `local`/`declare`/`typeset`/`export`/`readonly`/
`alias`/`eval`, matching bash). The VM must read `ArrayArg`, not the placeholder
word. Giving arrays a real word part is deferred until bash arrays are
implemented.

**Codes (1xx)**: 1 unterminated, 2 need more input, 3 unexpected, 4 bashism,
5 array subscript, 6 depth, 7 here-doc.

Known limits, accepted rather than hidden: `select` and `time` parse as command
words; `$"…"` gets double-quote semantics without locale translation; the
`$( … )` end scanner tracks `case` state heuristically, so a *pattern* literally
named `esac` can confuse it; CRLF scripts fail the way bash fails; and
`a[$(cmd)]=v` is refused by design (DESIGN §3.3), so scripts relying on
`shopt eval_unsafe_arith` will not parse.

---

## Diagnostic codes

Each crate owns a hundred-range, so a code identifies the layer that produced
it and never has to be looked up to be disambiguated. Codes are stable; message
text is not — anything matching on stderr should match the code.

| range | owner | examples |
|---|---|---|
| 1xx | `bsh-syntax` | 1 unterminated, 2 need more input, 3 unexpected token |
| 2xx | `bsh-vm` (reserved) | the POSIX §2.8.1 consequence layer |
| 3xx | `bsh-lang` | 301 unsupported operator, 302 parameter null, 303 unset |
| 4xx | reserved | redirection and I/O |
| 5xx | `bsh-runtime` | 508 builtin not implemented yet, other state errors |

`bsh-diag` produces no codes of its own; it renders them.

---

## Pending

Wave 2 (execution) and wave 3 (the editor) have not landed yet.
