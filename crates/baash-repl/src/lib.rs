//! The interactive line editor.
//!
//! This crate is the editor half of the interactive shell: a byte-oriented line
//! buffer, key decoding, syntax highlighting, `PS1`/`PS2` expansion, history,
//! `PS2` continuation, and the completion menu. The *interpreter* is not here
//! and is not depended on: completion arrives through
//! [`baash_complete::Completer`] and the parser's "is this line finished?"
//! question through [`Syntax`], so the editor can be driven by a fake terminal
//! in tests and by a real one in `baash`. The one exception is the history
//! *type*: the ring lives in `baash-runtime` so the editor and the
//! `history`/`fc` built-ins cannot drift apart, and the editor still builds
//! its own [`History`] in tests.
//!
//! # Layers
//!
//! | layer | responsibility |
//! |---|---|
//! | `term` | the [`Terminal`] trait and its real-tty implementation |
//! | `key` | bytes → [`Key`], including bracketed paste |
//! | `line` | the [`Line`] buffer, cursor and kill ring |
//! | `display` | bytes → display cells (`unicode-width`) |
//! | `render` | one frame: prompt, line, ghost text, menu ([`Frame`]) |
//! | `prompt` | the rendered `PS1`/`PS2` the shell hands in ([`expand`]) |
//! | `editor` | the [`Editor`] state machine and read loop |
//!
//! The history ring itself (`baash_runtime::History`) is re-exported as
//! [`History`]: entries, navigation, the `HIST*` policies and persistence.
//!
//! # Deferred
//!
//! Consciously left out of this wave, in rough order of how much a daily user
//! would miss them:
//!
//! * **Match highlighting in the search.** `Ctrl-R` shows the match with the
//!   cursor on its first byte (readline reverses the matched range as well);
//!   the renderer has no selection concept yet.
//! * **`Ctrl-S` forward search.** `Ctrl-R` only walks backwards, which is also
//!   what a terminal without `stty -ixon` can deliver.
//! * **Readline's menu-complete.** Tab cycles the menu (fish's pager) and
//!   Enter inserts the selection; it does not insert on each repeated Tab.
//! * **Vi mode and `set -o vi`.** No modal editing, no `bind`, no `inputrc`.
//! * **The full-screen picker** (`DESIGN.md` §6.8). The inline menu is
//!   columnar, paged and has a description column, but there is no separate
//!   screen with a preview pane.
//! * **A `poll(2)`-based key timeout.** `baash-sys` has no `poll` wrapper, so a
//!   lone `Esc` is only decoded as `Esc` at end of input; on a real tty it is
//!   indistinguishable from the start of `Alt-<key>`. Pressing `Esc` twice
//!   cancels the menu (`Ctrl-C` and typing also do), and a timeout in the
//!   `Terminal` implementation is the real fix; it does not touch the editor.
//! * **Pty-driven editor tests.** The editor's tests drive a fake
//!   [`Terminal`]; pty coverage lives in the binary
//!   (`crates/baash/tests/interactive_jobs.rs`, `interactive_history.rs`),
//!   because a real terminal needs a real process.
//! * **`SIGWINCH` handling.** The size is read on every repaint, but a resize
//!   mid-line forces a fresh frame rather than reflowing history above it.
//! * **A prompt after output that did not end in a newline.** The editor
//!   anchors each prompt at column 0 because it cannot see what the command
//!   wrote; a partial output line is erased when the next prompt is painted.
//!   Querying the cursor position (DSR) or tracking the output column fixes
//!   it.
//! * **Editing earlier lines of a multi-line command.** `PS2` continuation
//!   makes the submitted lines scrollback; only the live segment is editable,
//!   and Up/Down history navigation is suppressed while continuing.
//! * **`Ctrl-D` at an empty `PS2` line reports a syntax error**, as bash does.
//!   It abandons the whole logical command instead, because the [`Syntax`]
//!   trait can say "more input" but not "incomplete, now complain".
//! * **Mouse support**, selection, and copy/paste chords.
//! * **Semantic highlighting** (unknown commands coloured differently) and
//!   incremental highlighting; the whole buffer is re-lexed per keystroke.
//! * **Ghost text without colour.** Suggestions are painted, and accepted with
//!   `→`/`End` at the end of the line, only when ANSI dim is available.
//! * **A history search UI.** `Ctrl-R` walks the session ring with the same
//!   incremental matcher as completion; the database is a source for the ring,
//!   not something the editor queries.
//! * **`PS1` change detection inside a line.** The shell re-renders the
//!   prompt before each line; a `PS1` changed by the line being typed (a
//!   `$(…)` in `PS1` with a side effect, say) is visible at the next prompt,
//!   not mid-line.
//! * **Bracketed-paste text inserted as real newlines**; a pasted newline is
//!   inserted verbatim and painted as the row break it is.
//! * **Differential repaint**; every keystroke paints the whole frame.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod completion;
mod display;
mod editor;
mod key;
mod line;
mod prompt;
mod render;
mod term;

pub use baash_highlight::Theme;

pub use baash_runtime::History;
pub use editor::{
    Config, Editor, Flow, LivePrompt, NeverNeedsMore, NeverRefreshes, ReadResult, ShellCompletion,
    ShellMenu, Syntax,
};
pub use key::{Code, Decoder, Key, Modifiers};
pub use line::Line;
pub use prompt::{ExpandedPrompt, Prompt, PromptState, expand, is_root};
pub use render::{Frame, MenuRow, MenuView, Renderer};
pub use term::{Input, RealTerminal, Size, Terminal};

/// Paint `line` for display.
///
/// Returns bytes rather than `str` so that undecodable filenames and editors
/// containing arbitrary bytes survive the round trip.
pub fn paint(line: &[u8], theme: &Theme, color: bool) -> Vec<u8> {
    baash_highlight::render(line, theme, color)
}

/// Whether colour should be emitted on `fd`: a terminal, and `NO_COLOR` unset.
pub fn color_supported(fd: i32) -> bool {
    std::env::var_os("NO_COLOR").is_none() && baash_sys::is_tty(fd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_paint_is_byte_exact() {
        let line = b"echo 'hi'  # note";
        assert_eq!(paint(line, &Theme::DARK, false), line);
    }
}
