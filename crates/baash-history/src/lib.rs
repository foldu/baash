//! Command history: the ring the editor walks, and the database it lives in.
//!
//! There are no history files here. Bash's `HISTFILE`, `HISTSIZE`,
//! `HISTFILESIZE`, `HISTCONTROL`, `HISTIGNORE` and `HISTTIMEFORMAT`, the
//! `history -a`/`-n`/`-r`/`-w` file operations and `fc` are all gone: they
//! exist to work around a line-oriented text file, and the shell has a
//! database.
//!
//! * [`History`] is the session ring the editor and the `history` built-in
//!   share: entries numbered by their database id, Up/Down navigation,
//!   ghost-text suggestions.
//! * [`Store`] is the embedded SQLite database behind it, written from its own
//!   thread so a slow disk never reaches the keystroke path.
//!
//! SQLite, not a bespoke engine: WAL mode is what lets two shells share one
//! file — and it is the one thing a shell database cannot do without — on
//! every platform the shell runs on, with no feature flag and no backend to
//! choose between. The engine is compiled into the binary (`sqlx`'s bundled
//! `libsqlite3-sys`), so the host's `libsqlite3` is never consulted.
//!
//! The database lives in `baash/history.db` under the platform's per-user data
//! directory — `$XDG_DATA_HOME` or `~/.local/share` on Unix,
//! `%LOCALAPPDATA%` on Windows, both resolved by [`baash_sys::dirs`].
//! [`default_path`] applies that rule, and `BAASH_HISTORY_DB` overrides it (an
//! empty value disables persistence).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod ring;
mod store;

use std::path::PathBuf;

pub use ring::{Entry, History, PICKER_DEPTH, RECENT_DEPTH, RETENTION};
pub use store::Store;

/// Where the history database lives, unless `BAASH_HISTORY_DB` says otherwise.
///
/// `None` means there is nowhere to put it (no home directory, so no data
/// directory either); the caller then runs without persistence rather than
/// guessing at a location.
pub fn default_path() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("BAASH_HISTORY_DB") {
        return if value.is_empty() {
            None
        } else {
            Some(PathBuf::from(value))
        };
    }
    let root = baash_sys::dirs::data_home()?;
    Some(root.join("baash").join("history.db"))
}
