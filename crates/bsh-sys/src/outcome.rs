//! What happened to a child, shared by both platform backends.

/// What happened to a child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    /// The child exited normally with this status.
    Exited(i32),
    /// The child was killed by this signal.
    Signaled(i32),
}

impl WaitOutcome {
    /// The status a shell stores in `$?`.
    ///
    /// A signalled process yields `128 + signal`, which is what POSIX shells
    /// report. An ordinary exit keeps its status (the VM masks it to 8 bits
    /// when expanding `$?`).
    pub fn exit_status(self) -> i32 {
        match self {
            Self::Exited(status) => status,
            Self::Signaled(signal) => 128 + signal,
        }
    }
}
