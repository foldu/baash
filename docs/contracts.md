# Cross-crate contracts

Written as crates land. The failure mode this file exists to prevent: an agent
documents a contract in its branch report, the next agent never sees it, and the
two assumptions quietly disagree six weeks later.

Each section is owned by the producing crate. If you are implementing a consumer
(`bsh-vm`, `bsh-repl`, `bsh-tools`) read the relevant sections before you start,
and treat every item here as a fact about the API you are calling.

Status: everything except the tooling has landed: `bsh-diag`, `bsh-sys`,
`bsh-runtime`, `bsh-lang`, `bsh-syntax`, `bsh-complete`, the `bsh-vm`
tree-walking executor and the `bsh-repl` editor. `bsh-compile` still holds
types only.

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
`as_raw_fd()` is `-1`), `memfd_create`, `O_TMPFILE`, and `ptsname`'s non-thread-
safe static buffer on the non-Linux `open_pty` path. Everything else is generic
POSIX.

**New in this wave, all thin and all unsafe-free at the API:** `lseek` (plus
`fd::seek`) is how `read` rewinds the bytes it buffered but did not consume, and
`ESPIPE` is how it discovers that a descriptor is a pipe; `NoEcho` is `read -s`
(echo off, canonical mode and signal generation kept); `Disposition` /
`set_disposition` / `ignore_signals` are the two places the shell changes a
signal *action* rather than blocking it (`trap '' SIG` is `SIG_IGN`, which
children inherit across `exec`, and `SIGINT`/`SIGQUIT` are ignored while the
shell waits for a foreground job); `replace_signal_mask` is the permanent form
of `SignalSet::set`, for a forked child that must not inherit the shell's
blocked traps. `SIGALRM`, `SIGTERM`, `SIGINT`, `SIGQUIT` and `ECHILD` are
re-exported so `bsh-vm` needs no `libc`.

**`exec(plan) -> io::Error` replaces the process in place** and returns only on
failure. The caveat is inherent to exec ordering: by the time `execve` fails,
the plan's file actions, signal mask, `reset_signals` and process group have
all been applied. The image is intact, but the *environment* is not — so the
only sound response is to report 127/126 and exit. A caller that wanted to
continue after a failed `exec` (bash does) would have to restore descriptors and
signal dispositions itself. Also: keep the owned fds referenced by
`FdAction::Dup2 { from }` alive across the call, or the actions fail `EBADF`.

Terminal code is now tested on a real pty. `open_pty` opens the slave
`O_NOCTTY` so a test can never steal the harness's controlling terminal; the
tests install a known canonical `termios` state first, assert every flag raw
mode clears, and assert full restoration on `Drop`; the foreground-pgrp test
restores the terminal *from the background* with `SIGTTOU` at `SIG_DFL`, which
is what proves `set_foreground_pgrp`'s block is load-bearing rather than
incidental.

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

**`VarValue::Indexed` is sparse, keyed by `i64`.** An ordered
`BTreeMap<i64, Vec<u8>>`, not a `Vec`: `a[100]=x` costs one entry, and
`unset 'a[0]'` leaves a hole that iteration skips. The key is `i64` even though
subscripts are non-negative after resolution, because bash subscripts are
machine integers (the suite exercises `a[0x7FFFFFFFFFFFFFFF]`). Reading it is
`Vars::element_indexed`/`element_assoc`; writing is `set_indexed`/`set_assoc`
or `append_indexed`/`append_assoc`; `unset_element_indexed`/`unset_element_assoc`
remove one element. `VarValue::Assoc` remains an insertion-ordered
`Vec<(Vec<u8>, Vec<u8>)>`. A scalar converts to either shape with the old value
at element/key zero; indexed↔associative conversion is
`Error::ArrayShapeMismatch` (bash refuses it).

**Array element removal needs word work, so the VM supplies it.** `unset
'a[$i]'` re-parses its operand as an assignment word, which is expansion, not
state. `ShellHost::unset_array_element(&mut self, operand) -> Option<Status>`
is the seam: the default returns `None` and `unset` keeps its "not implemented"
diagnostic; the VM expands the subscript and calls back into the store.

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

**Traps are state, and the table lives here.** `ShellState::traps` is a
`TrapTable` keyed by `TrapKey` (`Exit`, `Signal(i32)`, `Err`, `Debug`,
`Return`) with `TrapAction::{Command(Vec<u8>), Ignore}`. It parses and renders
signal specifications (`signal_number`, `signal_short_name`, `signal_table`),
iterates in bash's listing order (`EXIT`, signals ascending, then
`ERR`/`DEBUG`/`RETURN`) and bumps a `generation` on every mutation so the VM
knows when to re-sync process dispositions without diffing. `trap` itself is a
state-only handler here. It implements bash's surprising surface rule: when the
*first* operand is a signal specification there is no action, and every operand
is a signal to reset (`trap 0 2`, `trap 2 EXIT`); otherwise the first operand is
the action and `''`/`-` mean ignore/reset. A failing `trap` records a
**utility** failure on purpose — the §2.8.1 special-built-in rule would
otherwise exit the shell, and no shell does that for `trap 'x' SIGINVALID`.

**`ShellHost` grew two seams for `trap`.** `check_syntax(src) -> bool` validates
a handler at registration time (parsing is `bsh-syntax`'s, which this crate must
not depend on), and `trap_saved_status() -> Option<Status>` answers the exit
status a bare `exit` inside the `EXIT` trap must reuse. Both are defaulted, so a
new host compiles without them.

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

**Arrays are part of the word model.** `ParamName::Element { name, subscript }`
is `${a[i]}` and `ParamName::Elements { name, star }` is `${a[@]}`/`${a[*]}`;
both carry the array's name. The subscript is a `Word` because it is expanded
first; for an indexed array the result is then evaluated with
`Shell::arithmetic_value`, for an associative array it is the key verbatim.
`ParamOp::Length` covers `${#a[@]}` and `${#a[i]}`, `ParamOp::Indices` is
`${!a[@]}`, `ParamOp::Indirect` is `${!a[i]}` (the element names a variable),
and `ParamOp::ArraySlice` carries the two bounds as words. `Vars::array(name)
-> Option<ArrayValue<'_>>` is how expansion sees the storage: an
`ArrayValue::Indexed(&BTreeMap<i64, Vec<u8>>)` in index order or
`ArrayValue::Assoc(&[(Vec<u8>, Vec<u8>)])` in insertion order. A scalar is a
one-element array for reads, and a bare `${a}` decays to element/key zero.

**Quoted and splittable, also for arrays.** `"${a[@]}"` uses the same
`Expanded::break_field` boundaries as `"$@"` — one field per element, zero
fields when empty — and `"${a[*]}"` joins with the first `IFS` character.
Unquoted forms are a single splittable expansion result. No third path was
introduced.

**Unsupported is an error, not a guess.** `${x#pat}`, `${x%pat}`, `${x/p/r}`,
`${!x}` (on a non-array), `${x@Q}`, `${x^^pat}`, `${@^^}` and process
substitution return `ErrorKind::Unsupported` with the operator's span. The
parser must map those surface forms to the corresponding `ParamOp`, so the
error can point at the right place instead of failing later.

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
set.

**Expectations answer for the cursor.** `expectations_at(src, offset, opts)`
reports what may be typed *at* `offset`, not after the token that ends there:
`ec` at 2 is `Command`, `ls ` at 3 is `Word`, `for f in ` at 9 is `ForList`.
Ordering is most-specific first: a compound's own keyword precedes the generic
command slot (`if true; ` → `[Then, Command]`), so read the first entry for the
headline and the last for the innermost slot the cursor is in.
`expectation_sites_at` is the same answer with the span that governs each
expectation. `expectations_after` keeps the old "slot after the prefix" answer
for callers that need it. Inside an unclosed `[[ … ]]` the answer is
`Expect::TestOp`, and its site's span is the operator just typed (or being
typed) — no consumer counts unbalanced `[[`. A redirection target reports
`Expect::RedirectTarget` whether the target has been typed or not. The closing
keywords are reachable: `Then`, `Do`, `Done`, `Fi`, `Esac`, `In` and
`FunctionBody` appear once a list boundary makes them legal.

**Open constructs are a stack.** `lex_full` returns
`Lexed { tokens, heredocs, unterminated, open }`; `open` is the chain of
constructs still open at end of input, **innermost first**, each with its
opener span and `content_start` (the first byte of the construct's content).
`open_constructs_at(src, offset)` is the same chain at an arbitrary offset.
`Unterminated` carries the same `content_start`, and its `opener` is the whole
opening delimiter (`$'`, `${`, `$(`, `$((`, `<(`, `((`, the quote byte, the
backtick). No consumer encodes opener widths, and a word containing a
substitution is still one token: the chain is the nested view.

**Redirections carry their split.** A redirection is still one token, but
`redirect_op_len(text)` (and `Token::redirect_op_len`) give the length of the
file-descriptor prefix plus the operator, and `Token::redirect_target` returns
the fused target's span. The operator set is defined once, in `bsh-syntax`;
consumers must not carry their own.

**Tokens have simple-command roles.** `classify_tokens(tokens, src)` returns a
`TokenRole` per token, in order: `Assignment`, `CommandWord`, `Argument`,
`RedirectOperator`, `RedirectTarget`, `Boundary` (an operator, newline or
reserved word that ends the command) or `Filler` (layout, comments,
here-document data). Command boundaries come from the grammar, not from a
keyword table in the consumer, and a separate redirect target is not an
argument.

**One word can be parsed on its own.** `parse_word(src, opts)` and
`parse_word_at(src, offset, opts)` consume exactly one word — for `[[ … ]]`
operands, `case` patterns (without the terminating `)`) and `$…` constructs
inside here-document data — and reject a second word or an operator with
`ParseWordError::{Missing, Trailing, Unterminated, Invalid}`. Every span and
diagnostic is relative to the caller's `src`; nothing is synthetic. Leading
layout is skipped, trailing layout allowed.

**Array literals are structured.** `Assignment.array:
Option<Vec<ArrayElement>>` and `SimpleCommand::array_args: Vec<ArrayArg>` carry
`ArrayElement { span, index: Option<ArrayIndex>, value: Word }`;
`ArrayIndex { subscript: Word, append }` is the `[k]` / `[k]+=` prefix. The
distinction is structural on purpose: `[k]=$v` is one scalar element while a
bare `$v` field-splits. `ArrayArg` additionally carries the variable `name`
span and its own `append`, and the argument in `words` is still a quoted
placeholder holding the raw text (the VM matches it). The parser only accepts
array-literal arguments after `local`/`declare`/`typeset`/`export`/`readonly`/
`alias`/`eval`, matching bash.

**Subscripts may contain spaces and `=`.** `a[1 + 2]=v` is stitched back
together from several tokens (bash's parser does the same), and
`a[x=1]=v`'s assignment `=` is found after the closing bracket.
`a[$(cmd)]=v` is still refused by design (DESIGN §3.3), but `${a[$(cmd)]}` is
not: the expansion subscript is an ordinary word.

**Codes (1xx)**: 1 unterminated, 2 need more input, 3 unexpected, 4 bashism,
5 array subscript, 6 depth, 7 here-doc.

Known limits, accepted rather than hidden: `select` and `time` parse as command
words; `$"…"` gets double-quote semantics without locale translation; the
`$( … )` end scanner tracks `case` state heuristically, so a *pattern* literally
named `esac` can confuse it; CRLF scripts fail the way bash fails; and
`a[$(cmd)]=v` is refused by design (DESIGN §3.3), so scripts relying on
`shopt eval_unsafe_arith` will not parse.

---

## bsh-complete

**Call `apply`, do not re-implement insertion.** `Menu::insertion(i)` returns
already-quoted bytes (trailing `/` for a directory, trailing space for a unique
non-directory), and `Menu::apply(&mut line, i)` splices it in and returns the new
cursor position. The editor must use these: quoting comes from
`bsh_lang::quote` with the context the parser derived, and a second
implementation of that logic will disagree.

**`replace` is not `word_span`.** `replace` starts *after* an opening quote,
`${`, `((` or `=`, so inserting never deletes the opener. The editor replaces
`replace`, not the word.

**Ranking tiers are semantic, not advisory.** `tier` 0 = exact-case prefix,
1 = caseless prefix, 2 = fuzzy (`score` holds the nucleo score). The vector is
already ordered by (tier, frecency, score). If a consumer filters, it must
preserve order; it must not re-sort, because typed-prefix matches outranking
fuzzy ones is the product promise (§6.5).

**The snapshot traits are `Send + Sync` and return owned data.** `VarNames::names`,
`History::entries`, `ShellNames::functions`/`aliases` all return
`Vec<Vec<u8>>`. `bsh-runtime` therefore cannot hand over a borrow of live
interpreter state; it must supply a snapshot or a `Mutex`-backed view. This is
deliberate: completion runs while the editor holds the terminal, and it must not
observe a half-mutated shell.

**After `cd`, call `set_root`.** `invalidate_commands` is only needed for the
pathological case; the `PATH` index already notices changes.

**`Candidate::display` is not sanitised.** A hostile filename can carry
newlines or control bytes into the menu. The editor must flatten it when
painting.

**The context is derived, not guessed.** `context` uses
`open_constructs_at` for the nesting stack, `expectations_at` for the slot at
the cursor, `Token::redirect_op_len` for a fused redirection, and
`classify_tokens` for the command word and argument index. It does not
re-enter substitution bodies, count `[[` tokens, or carry a redirection
operator table. `Ctx::expect` is the last entry of `Ctx::expects` (the
innermost slot); the earlier entries are the construct's own keyword
candidates.

Known copy: the builtin-name list is duplicated from `bsh-runtime` because the
dependency runs the other way. Replace it with a runtime-supplied list once the
runtime exposes one.

---

## bsh-vm (tree evaluator)

**Which built-ins the executor owns.** `break`, `continue`, `return`, `exit`,
`exec`, `eval`, `.`/`source`, `cd`, `echo`, `read`, `wait` and `kill` are
implemented in `bsh_vm`. They unwind control flow, re-enter the interpreter, or
need the descriptor table, `lseek`, termios and signal delivery, which a
`fn(host, argv) -> Status` signature cannot express. The executor consults them
only when `BuiltinDef::is_implemented()` is false, so a runtime implementation
takes over silently — deliberate, but it means whoever implements one of those
in `bsh-runtime` must delete the executor's body in the same change. `trap`, by
contrast, *is* implemented in `bsh-runtime`: its work is table mutation, and the
execution half stays here (see below).

**Trap execution is a command-boundary poll.** Caught signals are blocked in the
shell thread and drained from a `signalfd` at the start of each simple command,
after a foreground child is reaped, and when the shell finishes; no native
handler exists, so nothing can touch interpreter state (§4.5). Ignored signals
are `SIG_IGN` (inherited by children, which is what `trap '' INT` promises).
`reset_signals` on a spawn plan comes from `TrapTable::caught_signals`, and
stays excluded for ignored signals. A forked child clears caught traps, keeps
ignores and unblocks the parent's reader mask. `EXIT` runs at the end of the
outermost `run_named` — inside the unwind, so `exit 42` in the handler decides
the process status — and from `Drop` for an interactive shell, whose editor loop
is outside the VM. `$?` inside `EXIT` is the pending status, every other handler
preserves `$?`, a bare `exit` in `EXIT` reuses the saved status, and a trap never
re-enters itself. `SIGINT`/`SIGQUIT` are ignored while waiting for a foreground
job, and *discarded* if they were trapped (bash's rule: `kill -INT` during
`sleep` runs no `INT` trap, while `USR1` runs afterwards).

**`read` never over-reads.** A seekable descriptor is read in one chunk and
rewound by the surplus with `lseek`; a pipe is read one byte at a time because
bytes cannot be given back to it. `Interp::read_buffers` only holds a surplus
whose rewind failed, and a forked child starts with it empty.

**Background children are tracked by pid with their statuses** (`Interp`'s
`background`/`jobs`), so `wait PID` can report a child reaped several commands
ago, `%1`/`%%`/`%-` resolve against the pids this shell started, and a waited
status is consumed (a second `wait PID` is 127). `wait` reports a signalled
child's description (`Hangup`, …) on stderr, as bash's job notification does.

**`exec` is a real `execve`.** `bsh_sys::exec(plan)` replaces the process; the
VM maps its `io::Error` to 127/126 and exits. See the `bsh-sys` section for why
a failed `exec` is not a rollback.

**The VM owns `PWD`/`OLDPWD` around `bsh_sys::chdir`.** Descriptors saved while
a built-in's redirects are applied are `CLOEXEC` copies above fd 20, so a child
spawned from inside a redirected built-in cannot inherit the shell's saved copy.

**Fragments go through `bsh_syntax::parse_word`.** `[[ ]]` operands, `case`
patterns and the `$…` constructs in a here-document body are single words, so
they use the real front end with caller-relative spans. (An earlier revision
synthesised `": <fragment>"` and took `words[1]`; that is gone.)

**Here-document body expansion lives here**, as a double-quote-like scanner, and
is a candidate to move into `bsh-lang` once a here-doc body has a word model.

**`$?`, `$-`, `$$`, `$!` are cached** and refreshed at command boundaries,
because `Vars::get(&self)` cannot compute them. A runtime-owned `special()`
accessor would remove the bookkeeping.

**A forked child leaves through `bsh_sys::exit_child(status)`**, never
`std::process::exit`: the latter runs libc `atexit` handlers inside a forked
child. `fork_child_status(f: impl FnOnce() -> i32)` does this for you.

**Status precedence**: an explicit exit request outranks the POSIX §2.8.1
consequence table — `exit 7` is 7, not 1.

**`pipefail` takes the *rightmost* failing status**, and is sampled at pipeline
start (DESIGN §2).

**Arrays are executed here.** The parser's `ArrayElement`s reach the executor
through `AssignmentAction`; `exec_simple` keeps declaration-command literals on
an `Interp` side channel (`ArrayLiteral`, keyed by the placeholder text) so
`local`/`declare` can apply `-a`/`-A` first. `Interp` also implements
`bsh_lang::Vars::array` (a view over `VarValue`) and
`ShellHost::unset_array_element`, and its arithmetic `IntVars` exposes element
lvalues, so `(( a[99]=1 ))` and `${a[i+1]}` share one evaluator. A variable
whose value is not a number is recursively evaluated as an expression, bounded
by `MAX_RECURSION`.

Deferred, and listed in `bsh_vm::tree`'s `# Deferred`: job control (`jobs`,
`fg`, `bg`, `set -m`), `select`, `time`, `coproc`, `set -u`/nounset,
`$LINENO`/`$RANDOM`/`$SECONDS`/`$PIPESTATUS`, `command`/`type`/`hash`/`builtin`,
`getopts`, aliases, `mapfile`/`readarray`, pattern parameter operators,
`[[ =~ ]]`, process substitution, `~user`, and `set -x`. An executable text file
without a shebang is reported as 126 instead of being retried as a shell script
(POSIX requires the retry), which is what blocks the three
`$REPO_ROOT/spec/testdata/*.sh` trap cases.

**Platform constants come from `bsh-sys`.** `bsh_sys::{flags, SIGPIPE, c_int,
mode_t}` are re-exported so no other crate needs `libc`; `bsh-vm` has no `libc`
dependency, which keeps "only `bsh-sys` touches the platform" true.

---

## bsh-repl

**The editor takes bytes and a `Terminal`; it does not know about the shell.**
`bsh-repl` deliberately depends on no interpreter crate — not even `bsh-syntax`.
Two traits carry everything it needs from above:

```rust
pub trait Terminal {                       // the pty wave implements these five
    fn read_byte(&mut self) -> io::Result<Option<u8>>;
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn size(&mut self) -> Size;            // Size { columns, rows }
    fn enter_raw(&mut self) -> io::Result<()>;
    fn leave_raw(&mut self) -> io::Result<()>;
}

pub trait Syntax { fn needs_more_input(&self, line: &[u8]) -> bool; }
```

`RealTerminal` opens `/dev/tty` (falling back to stdio) and uses
`bsh_sys::RawMode` plus `window_size_fd`, because `bsh_sys::window_size()` is
wired to stdout. A pty harness needs only those five methods; no editor change.

**Continuation goes through `Syntax`, implemented in the binary** with
`bsh_syntax::parse_bytes`. The trait is deliberately thin, and that thinness has
a visible cost: it can say "more input" but not "incomplete, and now the user
wants to stop", so Ctrl-D on an empty `PS2` line abandons the command instead of
reporting the syntax error the way bash does. Widening it to a three-state
status (`Complete | Incomplete | Invalid`) would fix that; it is a seam, not a
wall.

**Raw mode is held only while `read_line` runs**, so a child command runs with
the terminal cooked. That is a job-control prerequisite, not an accident.

**Key semantics**: TAB and Enter accept a menu selection; Up/Down, Shift-Tab and
Ctrl-P/Ctrl-N navigate; Esc cancels. One candidate is inserted through
`Menu::apply` without opening a menu. Ghost text comes from history and is
accepted with Right or End.

**The editor owns the prompt** (`$PS1`/`$PS2` expanded from the environment only,
so `PS1=…` typed at the prompt does not take effect until the runtime exposes
shell variables to it), history (`$HISTFILE` appended, never rewritten) and the
`cd`-driven `set_root` call, which the binary makes after each chunk.

Deferred, with the list maintained at the top of `crates/bsh-repl/src/lib.rs`:
Ctrl-R reverse search, vi mode, multi-column menus and the full-screen picker,
a `poll(2)` key timeout (a lone Esc is only distinguishable at end of input),
`SIGWINCH` reflow, mouse, incremental/semantic highlighting, `HISTCONTROL`, and
automated pty tests.

---

## bsh-pattern

One matcher for every pattern in the shell: `${x#pat}`, `${x%pat}`, `case`,
`[[ x == pat ]]`, extglob, `${x/pat/rep}` and globbing (DESIGN §4.8).

```rust
pub struct Flags { pub extglob: bool, pub nocasematch: bool, pub pathname: bool }
pub fn matches(pattern: &[u8], text: &[u8], flags: Flags) -> bool;
pub fn prefix_end(pattern: &[u8], text: &[u8], longest: bool, flags: Flags) -> Option<usize>;
pub fn suffix_start(pattern: &[u8], text: &[u8], longest: bool, flags: Flags) -> Option<usize>;
pub struct Pattern;   // compile once, match many; for glob loops
```

`matches` is anchored at both ends. `Some(0)` / `Some(len)` are real empty
matches; `None` is no match. Parsing is total — a malformed pattern is literal
text, so a `case` pattern can never fail to compile.

**Algorithm**: pattern → AST → Thompson NFA simulated as a set of states per
text position, so it never backtracks. O(n·m) without `!(…)`, O(n²·m) worst
case with it. bash's own matcher is exponential (`a*a*a*a*b` hangs it); this one
is not, and there is a watchdog test to keep it that way.

**Wiring rules the caller must get right** (each verified against bash 5.3.20,
and each is surprising):

- `[[ x == pat ]]` matches with **extglob semantics even when `shopt -u
  extglob`**; `case` and `${…}` honour the option. Pass `extglob: true`
  unconditionally for `[[ == ]]`.
- `shopt -s nocasematch` affects `case`, `[[ ]]` and `${x/pat/rep}` but **not**
  `${x#pat}`/`${x%pat}`. Pass `nocasematch: false` for the four strip
  operators. Pathname expansion uses `nocaseglob`, which is a different flag.
- `${x/pat/rep}` takes the **longest** match at the leftmost position:
  `prefix_end(…, longest: true)` on the tail, offset added back. The empty-match
  policy (`${x//""/X}` unchanged, `${x/#/X}` prepends) lives in the
  substitution loop, not here.
- Globbing compiles one `Pattern` per pattern and matches per path component
  with `pathname: true`; dotfile rules, `globstar`, `nullglob` and sorting
  belong to the caller.
- There is no quote-mask API: a caller holding a per-byte quoting mask must
  convert quoted bytes to backslash escapes (a quoted backslash becomes `\\`).

Documented deviations: POSIX classes are ASCII-only (locale independence, unlike
glibc); `[[.ch.]]`/`[[=a=]]` are ordinary bracket members; bash's
`*@()`-matches-nothing quirk is not reproduced (we implement the compositional
meaning); malformed extglob groups degrade to literal text; nesting is capped at
128.

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
