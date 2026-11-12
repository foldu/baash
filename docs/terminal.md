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
ESC ] 133 ; A ESC \      prompt starts here
ESC ] 133 ; B ESC \      prompt ends here, the command line begins
```

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

## 5. What is next
In landing order, each with its own commit: focus reporting, a cursor-position
query for anchoring the prompt after output that did not end in a newline,
resize repaint, background-colour detection with a light theme, clipboard copy
(OSC 52), a scrollback-preserving `Ctrl-L`, the kitty keyboard protocol, and
prompt-line clicks. The ordered list is the session's task list; this file
records what has actually landed.
