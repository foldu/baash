# Terminal protocol: what baash says to a terminal

Status: **started.** This is the record of the bytes baash writes that are not
the prompt: the announcements the terminal interprets as shell integration.
It grows one feature per commit; what is not listed here is not sent.

The reference implementation is fish's
`doc_src/terminal-compatibility.rst` — the same escape codes, the same
sequence shapes. What is deliberately *not* copied is fish's per-terminal
quirks table: no Konsole carve-out, no tmux version gate, no WezTerm or
iTerm2 workaround. A terminal that mis-parses an OSC it does not know should
be fixed or replaced; baash writes the standard sequences unconditionally and
lets unknown ones be ignored.

## 1. The rule

* An announcement is **not part of the prompt**. It has no display width, a
  user's `PS1` has no way to express it, and the renderer must not count it.
* It is written **once per prompt event**, not per repaint: the editor writes
  the preamble at the start of a line, and keystrokes never repeat it.
* It is **never on the keystroke path**: the bytes are built from data the
  prompt already read (`$PWD`, the hostname), not by running anything.
* It is **never necessary for the shell to work**. A terminal that ignores it
  still shows a correct prompt.

`TERM=dumb` gets no announcements at all.

## 2. OSC 7: the working directory

```
ESC ] 7 ; file://HOST/PATH BEL
```

The terminal uses it for a new tab or split, for tmux's
`#{pane_current_path}`, and for session restore. baash sends it at **every
prompt**, not only after `cd`, because the terminal's copy and the shell's
`PWD` can disagree: a child such as `ssh` can change the terminal's idea of
the directory without changing the shell's.

The payload is a file URI, not a shell string:

* the host is the machine's hostname and is not `localhost`; the terminal may
  be on a different machine from the shell;
* the path is percent-encoded **byte by byte** (only `A-Z a-z 0-9 / . ~ - _`
  stay literal), so a file name that is not UTF-8 round-trips exactly;
* `?`, `#`, `@`, `&` and `%` are encoded rather than left to RFC 3986's
  `pchar` set, because they are legal in a POSIX file name and would otherwise
  turn a directory into a query or a fragment.

`baash_sys::osc::cwd_report` builds it and is testable without a terminal: the
Windows shape is exercised on Linux through `paths::Flavour`.

### Windows

A drive-absolute path becomes `file://HOST/C:/Users/...` (backslashes
normalised to `/`, the drive colon the only literal `:`), and the same prompt
also writes ConEmu's

```
ESC ] 9 ; 9 ; "C:\Users\..." ESC \
```

which Windows Terminal reads for the same purpose. A path with no truthful URI
form — a UNC `\\server\share` path, a drive-relative `C:x`, a
drive-rooted `\x` — produces no report rather than a guessed one.

On Windows 11 (26300.9711) with Windows Terminal as the console host, the
report is consumed: duplicating the tab (`Ctrl+Shift+D`) starts `cmd.exe` in
the shell's directory. Which of the two forms the terminal used is not
separable from outside, because both are written on every prompt; what a path
with no truthful URI form does is still "writes neither".

## 3. OSC 0/1: the window and tab titles

```
ESC ] 0 ; TEXT BEL      window title
ESC ] 1 ; TEXT BEL      tab title
```

Both are set, to the same text, at every prompt: `~/src/lab` locally,
`[ada@lab] ~/src` in an SSH session (detected the same way the prompt detects
it, from `SSH_TTY`/`SSH_CONNECTION`/`SSH_CLIENT`), and `baash` when there is no
directory. The directory is abbreviated at `~` only. The prompt's
shortest-unique-component form belongs next to the cursor, where it saves
columns; in a title bar it is noise, and it would cost a `read_dir` per
component on every prompt.

The text is sanitised: C0 controls and DEL become `?`. A directory name may
contain `ESC`, and a title is not a place to inject a sequence. Bytes above
`0x7f` pass through, because they may be UTF-8 continuation bytes.

The title is set at the prompt. A foreground command that wants its own title
is a separate decision and is not done yet.

## 4. OSC 133: prompt marks (A, B, C and D)

```
ESC ] 133 ; A ; click_events=1 ESC \   prompt starts here
ESC ] 133 ; B ESC \                    prompt ends here, the line begins
```

`click_events=1` is kitty's extension on `A`: the terminal reports
left-button presses on the prompt and command line as mouse events, which is
what click-to-move (§10) uses. A terminal that does not know the parameter
ignores it, and general mouse tracking is never enabled.

`A` is written after the preamble and before the prompt's first byte; `B` is
written after the first frame, so it lands at the end of the prompt with the
command line still empty. Both are written once per prompt, not once per
frame: a keystroke repaints the frame but does not re-mark it. That is what
lets a terminal treat a command line as a unit — prompt-jump, selecting a
command's output, per-command cwd for a new tab.

`A` is written by the editor because it brackets painting, not by the shell's
preamble: the marks are a property of *a line being read*, not of the prompt's
text, and the editor is the only party that knows when the first frame has
been painted. The title and cwd announcements still go first, in the shell's
preamble.

`C` and `D` are written by the loop, not the editor, because only the loop
sees a command start and finish:

```
ESC ] 133 ; C ; cmdline_url=… ESC \    a command starts; text percent-encoded
ESC ] 133 ; D ; STATUS ESC \           the command finished
```

`C` carries the command text as kitty's `cmdline_url` extension, encoded so a
`;`, a newline or an `ESC` cannot end the sequence early; the trailing newline
that submitted the line is not part of the command. `D` carries the exit
status, so a terminal can show it beside the prompt or group the output. The
pair surrounds execution; the next prompt's `A` follows `D`.

The four marks are the whole protocol: a terminal that understands them can
jump between prompts, select one command's output including its prompt, open a
new tab in the command's directory, and show the exit status. `TERM=dumb`
suppresses all four, and `A`/`B` are not sent again for a repaint.

Kitty's `click_events=1` extension on `A` is what §10 relies on: it asks the
terminal to report left-button presses on the prompt and command line as mouse
events, and without it there is nothing to map a click through.

## 5. Focus reporting

```
ESC [ ? 1004 h      focus reporting on
ESC [ ? 1004 l      focus reporting off
```

The mode is on only while a line is read, like bracketed paste: a command gets
the terminal without it. The terminal then sends `ESC [ I` when it gains focus
and `ESC [ O` when it loses it. Gaining focus repaints the frame — another
window may have painted over the terminal, or a multiplexer may have
reattached it showing a stale prompt — and losing focus changes nothing on
screen.

Windows: the console has no focus sequence. `ReadConsoleW` reports keys, and
the console's own focus handling is not observable through it, so a Windows
baash writes neither the mode nor an event handler for it.

## 6. Cursor position: anchoring the prompt

```
ESC [ 6 n           where is the cursor?
ESC [ row ; col R   the answer, both coordinates one-based
```

The command before a prompt can leave the cursor in the middle of a line
(`printf` without `\n`, a program that does not end its output). Frames are
anchored at column 0 because the editor cannot see what a child wrote, so
without asking, the prompt paints over that partial line.

The editor asks before the first frame of every line. A column other than 1
means the frame would overwrite output, so a `\r\n` goes out first. The wait is
bounded at 50 ms: a terminal answers in microseconds, and the deadline only
costs anything on one that does not answer at all, which then keeps the old
behaviour. `TERM=dumb` is never asked.

The query reads ahead while it waits, so a keystroke that arrives meanwhile is
put back in the decoder and decoded normally — including the case where the
whole answer was a key the user typed. A late answer, after the deadline, is
just an unknown key and is ignored.

Windows: the console has no cursor-position report. `reports_cursor_position`
is `false` there, the query is never sent, and the prompt keeps anchoring at
column 0.

## 7. Resize

On Unix the editor waits for `SIGWINCH` beside the keyboard and the directory
watch: one `poll_fn` over the readiness futures, with `compio::signal`
installing a real handler. A handler is reset to the default across `exec`, so
no signal mask is inherited by a child. The signal returns `Input::Resize`, the
editor drops the frame's anchor, and the next frame is painted at the width the
renderer reads. The terminal has already reflowed its own contents; reflowing
the history above the edit region is still out of scope.

On Windows the console has no resize event. `WaitForSingleObject` on the input
handle, with a 250 ms deadline, gives the idle prompt a chance to compare
`GetConsoleScreenBufferInfo`'s window size with the last one; a difference
returns the same `Resize`. The wait is also what makes the console path stop
blocking in a read, which is why the prompt there can notice anything at all
while idle.

## 8. Clearing the screen

`Ctrl-L` writes `ESC [ ROWS S` — ECMA-48's Scroll Up — before
`ESC [ H ESC [ 2 J`. `2J` erases the viewport and keeps no text, and the output
above the prompt is exactly what a user presses `Ctrl-L` to scroll back to;
scrolling the page up first moves that text into the terminal's scrollback.
The cursor does not move during `SU`, so the clear that follows homes it as it
always did. A terminal that does not implement `SU` ignores it and clears as
before; there is no capability query.

Windows: the console's VT output support is assumed to include `SU`. That is
unverified on a guest, like the rest of the Windows terminal path.

## 9. Clipboard (OSC 52)

```
ESC ] 52 ; c ; BASE64 BEL
```

`Ctrl-X` copies the current line. The clipboard belongs to the terminal, not to
the process, which is what makes this work over `ssh`; base64 keeps a newline
or a control byte in the line from ending the sequence early. There is no
acknowledgement in the protocol: a terminal may ignore the sequence (xterm
needs `allowWindowOps`) or cap the payload size, and the shell cannot tell.
There is no selection yet, so the whole line is copied; mouse selection is what
will make a partial copy possible.

## 10. Click to move the cursor

Because the prompt-start mark asks for `click_events=1`, a supporting terminal
reports a left-button press on the prompt or command line as an SGR mouse
event:

```
ESC [ < 0 ; column ; row M
```

The editor maps the cell back to a byte offset. The frame is a `columns`-wide
strip of rows beginning at the prompt's row (from the cursor-position report in
§6); the renderer records how many cells the prompt occupied, wrapped rows
included; the difference between the clicked cell's linear index and that count
is the offset into the editable line. A click on the prompt itself lands at the
start of the line.

Nothing happens without a cursor-position report: on a terminal that does not
answer `ESC [ 6 n`, or on the Windows console, the row is unknown and the click
is ignored rather than guessed at. A release (`m`), a modified button and a
scroll wheel decode as unknown sequences and are ignored. Double-click, drag
and text selection are not implemented yet.

## 11. Background colour (OSC 11)

```
ESC ] 11 ; ? ESC \                     what is the background colour?
ESC ] 11 ; rgb:RRRR/GGGG/BBBB BEL       the answer, one to four hex digits each
```

The shell asks once, before the first prompt, and waits at most 100 ms. A
terminal answers in microseconds; one that does not answer at all keeps the
dark palette. The reply's Rec. 601 luma decides light or dark, which is the
only question being asked here — not a colour-managed match.

The answer picks two things: the syntax theme (`Theme::LIGHT`/`DARK`, both in
the 16 base colours) and the stock prompt's accents. On a light background the
bright cyan of the current directory and the yellow of the counters are
invisible, so both become dark magenta; the reds of `behind`, `untracked` and
root's `#` stay.

It is asked once, not per prompt: there is no theme-change event to subscribe
to (OSC 2031 is not requested), and a round trip in front of every cursor
would be a latency bug. A terminal whose theme changes mid-session keeps the
palette it started with until the shell restarts.

Bytes typed while the question is outstanding are not an answer; they are put
back for the editor. The question is asked in raw mode, because a cooked
terminal would echo it and wait for a newline the reply does not have.

## 12. The keyboard: unambiguous keys

```
ESC [ = 1 u        kitty: set the progressive-enhancement flags to `disambiguate`
ESC [ > 4 ; 1 m    xterm: modifyOtherKeys, level 1
ESC [ > 4 ; 0 m    xterm: turn it off again
ESC [ = 0 u        kitty: clear the flags
```

Both requests are written when a line starts being read, after bracketed paste
and focus reporting, and are undone when the line ends, so a command is handed
the terminal in the encoding it expects.

The legacy encoding cannot say several different things: `Ctrl-I` *is* the tab
byte, `Ctrl-M` *is* Enter, `Ctrl-Shift-R` is `Ctrl-R`, and `Ctrl-Left` is its
own sequence only if the terminal has `modifyCursorKeys` on. The two protocols
above fix that:

* kitty's `disambiguate` flag makes every key that does not produce text
  arrive as `CSI code ; mods u`, with the modifiers in a bit field (shift 1,
  alt 2, ctrl 4, super 8, hyper 16, meta 32, then the lock bits). `Ctrl-I` is
  `CSI 105 ; 5 u`, and `Esc` is `CSI 27 u`, so the shell no longer needs a
  timeout to tell Escape from Alt.
* `modifyOtherKeys` level 1 does the same for keys that have *no* legacy
  encoding at all: `Ctrl-Tab`, `Ctrl-.`, `Ctrl-;`. Level 2 is not requested —
  it reports shifted letters as escape codes too, which turns ordinary typing
  into sequences the editor would have to undo.
* modified arrows, Home, End and Delete use the legacy shape with a modifier
  field (`CSI 1 ; 5 D`, `CSI 3 ; 5 ~`), and shift+Tab is `CSI 9 ; 2 u` or the
  legacy `CSI Z`.

The editor's key is a code plus the modifiers held — `Key { code, mods }`, with
`Code` and `Modifiers` in `baash-repl/src/key.rs` — so a binding reads
`(Code::Char('r'), Modifiers::CTRL)` and all three encodings converge on it.
The legacy control bytes map to the same keys (`0x12` is `Ctrl-R`), which is
what keeps a terminal that answers neither request working.

What the disambiguation buys today: `Ctrl-Left`/`Ctrl-Right` and
`Alt-Left`/`Alt-Right` move by a word, `Ctrl-Delete` kills the next word, and
`Ctrl-I`/`Ctrl-M`/`Ctrl-Shift-R` stop meaning tab, Enter and `Ctrl-R`.
Everything else is unchanged.

**Observed, not only specified.** Ghostty 1.4.0 sends `Ctrl-D` as
`CSI 100 ; 5 u` and `Alt-B` as `CSI 98 ; 3 u` with the locks off, and
`CSI 100 ; 133 u` / `CSI 98 ; 131 u` with Num Lock on: the lock bits are 64 and
128 in the same field as Ctrl, which is why a binding is matched through
`Modifiers::binding()` (`docs/contracts.md`). Letters still arrive as plain
text. `AGENTS.md`, "Harness a real terminal", says how those bytes were taken
and how to take them again.

A release report is ignored; the editor edits on presses. Neither protocol is
asked to report release or repeat events, so a terminal sending one is
answering a different application's request.

The byte constants are `baash_sys::osc::KEYBOARD_ENABLE`/`KEYBOARD_DISABLE` and
`MODIFY_OTHER_KEYS_ENABLE`/`MODIFY_OTHER_KEYS_DISABLE`; the decoding tests are
in `key.rs`; the pty tests prove the pair is written and undone around a real
line, and that a `CSI u` key edits the line.

Windows writes the same requests — `Console` puts the input handle in
`ENABLE_VIRTUAL_TERMINAL_INPUT`, and the escape sequences go out like any
other output — but the host observed on the guest ignores both: `Ctrl-M` still
submits and `Ctrl-Left` arrives as a plain Left. The legacy encodings are
therefore what the decoder sees there (`docs/portability.md` §8).

## 13. Desktop notifications (OSC 9)

```
ESC ] 9 ; TEXT BEL      ask the terminal to raise a notification
```

A command that outlived `BAASH_NOTIFY` seconds asks the terminal to notify the
user, and one that failed says so:

```
baash: 42s — cargo build
baash: 1m02s, exit 1 — make test
```

`BAASH_NOTIFY` is a number of seconds. Unset, empty, `0` and the false words
turn it off, which is the default; a value that is not a number is ignored
rather than guessed. The sequence goes out after `D`, so a terminal reacting to
`OSC 133 D` has already had its chance, and `TERM=dumb` sends nothing. The body
is the first line of the command, bounded to 120 bytes on a character boundary,
and never its output — an output line can be a secret, and a desktop
notification is readable by everything on the session.

**The threshold is not a focus test, because there is no focus fact to read.**
Focus reporting (§5) is on for the line and off again before a command runs: a
child that reads stdin must never be handed `CSI I` or `CSI O` mixed into its
input. While a command runs the shell therefore cannot see the terminal lose
focus, and the state it had at the last keystroke was "focused", because typing
takes focus. A terminal that raises its own notification from `OSC 133 D`
knows its own focus and is doing the better version; the shell's is the one
that works everywhere, which is why it is a threshold and not a guess.

## 14. What is not here, and why

No terminal item is left. Two things were considered and are deliberately
absent:

**Cursor shape (DECSCUSR).** `CSI Ps SP q` sets the shape a terminal draws for
its cursor, and it is only worth sending when the editor has a second mode —
vi normal mode, a selection — for the shape to signal. baash has one editing
mode, so the sequence would be written once at startup and never changed,
which is a terminal setting rather than shell integration. When a second mode
lands, DECSCUSR belongs to that change, next to the keys that enter it. There
is deliberately no helper for it in `baash_sys::osc`: an escape sequence with
no caller is not a primitive, it is dead code with a test.

**Drag-selection.** Click-to-move (§10) is a press, not a drag: no selection
model, no double-click word, no clipboard of a range. §9's `Ctrl-X` copies the
whole line. A selection is the natural follow-up to §10 and would give DECSCUSR
its second mode.

The ordered list of terminal work is the session's task list; this file records
what has actually landed.
