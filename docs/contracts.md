# Cross-crate contracts

Written as crates land. The failure mode this file exists to prevent: an agent
documents a contract in its branch report, the next agent never sees it, and the
two assumptions quietly disagree six weeks later.

Each section is owned by the producing crate. If you are implementing a consumer
(`bsh-vm`, `bsh-repl`, `bsh-tools`) read the relevant sections before you start,
and treat every item here as a fact about the API you are calling.

Status: `bsh-diag`, `bsh-sys` and `bsh-runtime` have landed. `bsh-syntax` and
`bsh-lang` are in flight.

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

## Pending

`bsh-syntax` (the `Expect` vocabulary and `ParseOutcome` a completer depends on),
`bsh-lang` (the `Vars` trait and expansion seams), `bsh-runtime` (the builtin
tier table and assignment-persistence rule) will each add a section when they
land.
