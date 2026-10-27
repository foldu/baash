//! The variable source expansion reads and (for `${x:=w}`) writes.

/// Everything expansion needs to know about shell variables.
///
/// `bsh-runtime` implements this over its scope stack. Keeping it this small
/// means the word model and the expansion pipeline never see the runtime, and
/// tests can use a `HashMap`.
///
/// # What the implementation must provide
///
/// * Every ordinary variable, addressed by its name bytes.
/// * The special parameters the runtime owns: `?`, `-`, `$`, `!` and `0`.
///   They are ordinary [`Vars::get`] lookups; expansion never special-cases
///   them.
/// * `IFS`, so field splitting can see whether it is unset (`None`) or set to
///   an empty string (`Some(b"")`) — the two behave differently (POSIX XCU
///   2.5.3).
/// * The positional parameters through [`Vars::positional`]. `$#` is computed
///   from the length; `$1` is index 0, and so on.
///
/// Values are bytes, never `str`: filenames are arbitrary non-NUL bytes and
/// `$'\xff'` exists (DESIGN.md §4.2).
pub trait Vars {
    /// Look up a variable. `None` means *unset*; `Some(b"")` means set to the
    /// empty string. The difference matters for `${x-word}` versus
    /// `${x:-word}`.
    fn get(&self, name: &[u8]) -> Option<&[u8]>;

    /// The positional parameters, starting at `$1`.
    fn positional(&self) -> &[Vec<u8>];

    /// Assign a variable, for `${x:=word}` and `${x=word}`.
    ///
    /// Read-only variables and invalid names are reported with [`SetError`];
    /// expansion turns that into a diagnostic. The default implementation
    /// refuses every assignment, which is the right behaviour for a
    /// read-only view of the environment (e.g. completion).
    fn set(&mut self, name: &[u8], value: Vec<u8>) -> Result<(), SetError> {
        let _ = (name, value);
        Err(SetError::ReadOnly)
    }
}

/// Why [`Vars::set`] refused an assignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetError {
    /// The variable exists but is read-only, or the source does not support
    /// assignment at all.
    ReadOnly,
    /// The name cannot be assigned (for example `$1` or `${x?}`).
    InvalidName,
}
