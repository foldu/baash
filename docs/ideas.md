# Ideas: what the live architecture could add

**Purpose.** A place to put the features this shell's shape makes possible, and
the reasons the ones we are not building are not being built. This is not a
queue: `docs/direction.md` decides the order, and an idea leaves this file when
it becomes work.

The shape in question is one thing: **work can happen off the keystroke path,
and the screen can be rewritten when it lands.** bash cannot have that, which is
why the same ideas read as impossible there and cheap here.

---

## 1. The pattern worth naming: a gauge in a slot

`docs/prompt-watch.md` already describes one of these — the git counters — and
it is a *policy*, not a feature:

| field | rule |
|---|---|
| **key** | what makes the value stale (the directory, the candidate, the line) |
| **job** | computed on a worker or on the runtime, never on the prompt path |
| **debounce** | at most one run per interval, coalesced |
| **deadline** | the timer wins, and an interrupt flag tells the job |
| **staleness** | same key: the last *completed* value stays; new key: drop it |
| **back-off** | a failed run delays the next one |
| **slot** | prompt segment, right prompt, hint row, menu annotation |

A second gauge is a copy of that code; a fourth is four copies with four
slightly different staleness rules. When the second consumer exists outside the
prompt, extract `Gauge<T>` and make the slot a type instead of inviting the fifth
copy. Until then this is a paragraph, not a crate.

## 2. Channels, ranked by when they are available

| channel | available | cost |
|---|---|---|
| `OSC 0`/`2` title | always, even while a child owns the tty | free, no screen |
| frame above the line | only while idle in the editor | one repaint |
| the line itself | while typing | a frame per keystroke |
| menu / search panel | while open | a repaint plus a preview fetch |

The first row is underused: while a build runs, the title is the only thing that
can change, and `cargo build · 4s` in the title is live UI for free.

**What is not a channel: focus while a command runs.** Focus reporting
(`CSI ? 1004 h`) is turned on in `enter_raw` and off in `leave_raw`, so it covers
the line and not the command: a child that reads stdin must not be handed
`CSI I`/`CSI O` bytes. The consequence is a real limitation, not a missing
feature — see §4.

## 3. The ideas

| idea | needs | cost | status |
|---|---|---|---|
| **Parse-as-you-type diagnostics** — a hard parse error's span and message under the line | the parser, already written for the editor's "is it finished?"; a debounce only if a line is huge | small | **this wave** |
| **Command duration in the prompt** — `1.2s` before the marker | a timestamp the loop already takes for the history row | tiny | **this wave** |
| **Completion notification for long commands** — `OSC 9` when a command outlives a threshold | `BAASH_NOTIFY`; the terminal's notification policy | tiny | **this wave** |
| **`OSC 133 C`/`D`** — command start, text and exit status | nothing new | tiny | landed (`6bea487`) |
| **Resolution preview** — `ls` → `/run/…/bin/ls (alias: ls --color)` | the `PATH`/builtin/alias index built at the prompt | small | next |
| **Expansion preview** — show what `*.txt` will become | pure expansions only (tilde, parameter, glob, brace) | medium | next |
| **Menu preview pane** — arrow through candidates, see the directory, the file's head, the branch, the description | a worker per candidate, cached, debounced | medium | next |
| **Semantic highlighting** — unknown command, missing `cd` target | the index plus `access(2)` on a worker | small | later |
| **`PS2` that names the construct** — `if>` instead of `>` | the parse tree the editor already asks | small | later |
| **Transient prompt** — collapse the prompt once the command runs | rewriting the prompt row in place; only when it is one row | medium | later |
| **Right prompt** — exit status, duration, time at the right edge | width arithmetic the renderer already does | small | later |
| **History search over the database**, with `$?` and duration beside each hit | a worker query, if the ring is bounded | medium | later |
| **`Gauge<T>` extraction** | the second consumer outside the prompt | small | later |
| **Selection, `DECSCUSR`, copying a range** | the second editing mode §13 defers the cursor shape for | medium | later |
| **Undo/redo and a real kill ring** | nothing threaded at all | small | later |
| **A command palette** — the shell's knobs (`PROMPT_ICONS`, completion switches) as a menu | the menu renderer | small | later |
| **A probe cache** — remember the `OSC 11`/DA answers per terminal | a file keyed by `TERM` and version | small | later |

Each of these has to answer the same three questions before it is worth
building: which channel, which slot, and what the user learns that they cannot
already see.

## 4. Not doing, and why

* **"Notify only if the terminal is unfocused."** The shell cannot know. Focus
  reporting is off while a child owns the tty — keeping it on would inject
  `CSI I`/`CSI O` into `cat`, `less`, `python` — so at the moment a command ends
  there is no fact to consult, and the state at Enter is always "focused" because
  typing requires focus. A terminal that does this from the shell-integration
  marks (`OSC 133 D`) knows its own focus and is the right place for it; the
  shell's own notification is therefore a threshold, not a focus test
  (`docs/terminal.md` §14).
* **Painting while a child runs.** The screen belongs to the child. The title is
  the exception and is worth using.
* **Running shell code between keystrokes** (a live `PS1`, a `PROMPT_COMMAND`
  that re-runs). The prompt is composed from data, and `PROMPT_COMMAND` runs
  before the prompt, not inside it (`docs/prompt-watch.md` §5).
* **Parallel shell code.** Shell code runs on the main thread
  (`docs/direction.md` §1); a `par` builtin could parallelise externals only,
  which is a different feature.

## 5. Rules for anything painted live

* **Grow downward, never upward.** The frame is anchored at the prompt row and
  the renderer moves the cursor up from where it left it; a slot above the line
  would push the prompt down and the next repaint would find the wrong row.
* **Discard by key, not by arrival.** Two previews in flight will otherwise
  paint out of order.
* **A slot that is empty must be as tall as one that is full**, or the line
  moves as the user types and the screen reads as a flicker.
* **Ink budget.** A once-a-second repaint is fine locally and less fine over
  `ssh`; every sequence has to survive ConPTY as well (`docs/portability.md`).
* **Nothing in the keystroke path may block**, and nothing may run at all unless
  the user can see the difference.
