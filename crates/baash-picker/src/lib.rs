//! The full-screen fuzzy picker: candidates in, one chosen candidate out.
//!
//! One implementation serves two callers, which is why it is a crate of its own
//! and not part of the line editor:
//!
//! * `Ctrl-R` — the shell's history search, over the ring and the database;
//! * the `baashzf` builtin — `ls | baashzf`, a filter for any list of lines.
//!
//! Both show the same screen (query, ranked matches, a count) and take the same
//! keys, so both use this state machine. What differs is only how bytes reach
//! it: the editor decodes the terminal with the kitty/modicfier protocols
//! requested for a line ([`Action`] is its output), while a builtin that owns
//! the screen decodes the legacy escapes a console sends ([`Decoder`]).
//!
//! # Nothing here touches a terminal
//!
//! [`Picker::frame`] returns the bytes of one screen; writing them is the
//! caller's job. That keeps the ranking, the layout and the scrolling testable
//! without a pty — the interesting failures (`gco` not finding `git checkout`,
//! a selection that scrolls out of view, a row that is one column too wide) are
//! all pure functions of the state.
//!
//! # Ranking
//!
//! The query is split on spaces into terms, and every term must match the line
//! as a *subsequence* (`fco` finds `flutter create --offline`), which is what
//! makes this fzf-like rather than a prefix filter. Scores add up, ties keep
//! the order the caller supplied, and the caller's order is the caller's
//! meaning: newest first for history, input order for `baashzf`. So an empty
//! query lists the candidates exactly as they arrived.
//!
//! `nucleo-matcher`'s *greedy* matcher is used, exactly as the completion menu
//! does: version 0.3.1's optimal matcher panics on ordinary ASCII subsequences,
//! and a keystroke inside the picker must never take the shell down.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod decode;
mod frame;
mod pick;

pub use decode::Decoder;
pub use frame::{ENTER_SCREEN, LEAVE_SCREEN};
pub use pick::{Action, Outcome, Picker};
