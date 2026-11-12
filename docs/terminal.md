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

The Windows half is **unverified on a real guest**: it is cross-compiled and
unit-tested through `Flavour::Windows`, but no Windows terminal has been
observed consuming it yet. That is recorded in
[portability.md](portability.md) rather than claimed here.

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

## 4. OSC 133: prompt marks (A and B)

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

Kitty's `click_events=1` extension on `A` belongs with click-to-move and is
not sent yet.

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

## 12. What is next
In landing order, each with its own commit: the kitty keyboard protocol.
Drag-selection and the clipboard integration it would bring are the natural
follow-ups to §10. The ordered list is the session's task list; this file
records what has actually landed.
