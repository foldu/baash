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

## 3. What is next

In landing order, each with its own commit: window/tab titles (OSC 0/1),
prompt marks (OSC 133 A–D), focus reporting, a cursor-position query for
anchoring the prompt after output that did not end in a newline, resize
repaint, background-colour detection with a light theme, clipboard copy
(OSC 52), a scrollback-preserving `Ctrl-L`, the kitty keyboard protocol, and
prompt-line clicks. The ordered list is the session's task list; this file
records what has actually landed.
