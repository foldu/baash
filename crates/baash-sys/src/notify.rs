//! A wake-up channel from a worker to whatever is waiting on its result.
//!
//! The line editor can wait on the keyboard, a directory watch and `SIGWINCH`,
//! but a result a worker computed is none of those. Without a descriptor of its
//! own, a finished background refresh had nothing to wake the editor with, and
//! the git counters it produced only went up at the editor's next one-second
//! backstop: the prompt was a whole second behind on every `cd` into a
//! repository (`docs/prompt-watch.md` §5).
//!
//! A pipe is the portable shape of that channel: the worker writes, and a
//! reactor can poll the read end (`compio`'s `PollFd` takes any descriptor).
//! Both ends are non-blocking and close-on-exec. Non-blocking matters twice:
//! the worker must never wait on a full pipe, and a write that cannot be
//! delivered is not an error here — what the byte announces is *state* (the
//! prompt's cache), so a reader that missed the count still sees the current
//! state. That is also why the signals coalesce: a hundred writes are one
//! drain, and a full pipe already means "there is something to read".
//!
//! This is a mechanism, not a policy: it says "something happened", never what,
//! and it holds no state of its own.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

/// A pipe pair used only to wake a poller.
///
/// [`Notifier::notify`] is the writing side, [`Notifier::as_fd`] the descriptor
/// to await for readability, and [`Notifier::drain`] clears what has been
/// signalled. The pair is created together because a half of it is useless on
/// its own, and it is cheap: two descriptors per shell session.
#[derive(Debug)]
pub struct Notifier {
    read: OwnedFd,
    write: OwnedFd,
}

impl Notifier {
    /// Create the pair, both ends close-on-exec and non-blocking.
    pub fn new() -> io::Result<Self> {
        use rustix::pipe::{PipeFlags, pipe_with};
        // `NONBLOCK` on both ends: the reader must not block the reactor's
        // thread if it is woken twice, and the writer must not block a worker
        // whose reader has gone away or stopped draining.
        let (read, write) = pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK)?;
        Ok(Self { read, write })
    }

    /// Announce that there is something new to look at.
    ///
    /// Never blocks and never reports: a failed write means the reader is
    /// already due to wake up (the pipe is full) or gone, and neither is worth
    /// interrupting the work that produced the news.
    pub fn notify(&self) {
        let _ = rustix::io::write(&self.write, b"!");
    }

    /// Consume every notification queued so far; `true` if there was one.
    ///
    /// A caller has to drain before it waits again: a pipe with a byte in it
    /// stays readable, so a poller that only reads the state behind the signal
    /// would spin. Reading one buffer per queued write is not needed — the
    /// signal is level-triggered, so one drain per wake-up is the whole
    /// contract.
    pub fn drain(&self) -> bool {
        let mut buffer = [0u8; 64];
        let mut signalled = false;
        loop {
            match rustix::io::read(&self.read, &mut buffer) {
                // Writers all closed: nobody can signal any more, and the
                // reader has nothing left to clear.
                Ok(0) => return signalled,
                Ok(_) => signalled = true,
                // `EWOULDBLOCK`: the queue is empty, which is the normal end.
                // `EINTR` is a short read with no bytes, not a state change.
                Err(rustix::io::Errno::WOULDBLOCK | rustix::io::Errno::INTR) => return signalled,
                // Nothing else is expected from a pipe we own; report no news
                // rather than looping on the same error.
                Err(_) => return signalled,
            }
        }
    }

    /// The descriptor to await for readability.
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.read.as_fd()
    }

    /// An owned duplicate of the read end, for a reactor that takes ownership.
    ///
    /// Both descriptors name the same pipe, so a notification written for one
    /// is seen by the other and either can drain it.
    pub fn reader(&self) -> io::Result<OwnedFd> {
        self.read.try_clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_signalled_is_nothing_to_drain() {
        let notifier = Notifier::new().expect("a pipe");
        assert!(!notifier.drain());
    }

    #[test]
    fn one_notification_is_one_drain() {
        let notifier = Notifier::new().expect("a pipe");
        notifier.notify();
        assert!(notifier.drain(), "the notification is queued");
        assert!(!notifier.drain(), "and draining it clears the channel");
    }

    #[test]
    fn notifications_coalesce_into_one_drain() {
        // A burst of worker results is one wake-up and one repaint: the bytes
        // are a count of "something happened", not one event per byte.
        let notifier = Notifier::new().expect("a pipe");
        for _ in 0..100 {
            notifier.notify();
        }
        assert!(notifier.drain());
        assert!(!notifier.drain());
    }

    #[test]
    fn a_polled_duplicate_is_the_same_channel() {
        // This is how the editor and the prompt share the descriptor: the
        // editor awaits the duplicate, and the prompt's owner drains the
        // original. A notification in either view is visible in both.
        let notifier = Notifier::new().expect("a pipe");
        let reader = notifier.reader().expect("a duplicate");
        assert!(!crate::poll_readable(std::os::fd::AsRawFd::as_raw_fd(&reader), 0).expect("poll"));
        notifier.notify();
        assert!(
            crate::poll_readable(std::os::fd::AsRawFd::as_raw_fd(&reader), 0).expect("poll"),
            "the duplicate becomes readable"
        );
        assert!(notifier.drain());
        assert!(
            !crate::poll_readable(std::os::fd::AsRawFd::as_raw_fd(&reader), 0).expect("poll"),
            "draining the original clears the duplicate's view too"
        );
    }
}
