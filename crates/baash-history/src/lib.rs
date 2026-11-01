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
//! * [`Store`] is the embedded [Turso] database behind it, written from its
//!   own thread so a slow disk never reaches the keystroke path.
//!
//! The database lives in `baash/history.db` under the platform's per-user data
//! directory — `$XDG_DATA_HOME` or `~/.local/share` on Unix,
//! `%LOCALAPPDATA%` on Windows, both resolved by [`baash_sys::dirs`].
//! [`default_path`] applies that rule, and `BAASH_HISTORY_DB` overrides it (an
//! empty value disables persistence).
//!
//! [Turso]: https://turso.tech

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod ring;
mod store;

use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

pub use ring::{Entry, History, RECENT_DEPTH, RETENTION};
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

/// Drive a future that does its own I/O, on this thread.
///
/// Turso's futures schedule their own completions and only need a waker that
/// wakes this thread; there is no executor to run here. A few spins cover an
/// operation that is already finished, and then the thread parks — the waker
/// unparks it, so the park costs nothing but the wait.
fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWaker {
        thread: std::thread::current(),
    }));
    let mut context = Context::from_waker(&waker);
    let mut future = pin!(future);
    let mut spins = 0u8;
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => {
                if spins < 32 {
                    spins += 1;
                    std::thread::yield_now();
                } else {
                    std::thread::park_timeout(Duration::from_millis(1));
                }
            }
        }
    }
}

struct ThreadWaker {
    thread: std::thread::Thread,
}

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.thread.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.thread.unpark();
    }
}
