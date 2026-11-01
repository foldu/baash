//! IFS field splitting: POSIX XCU 2.5.3 and 2.6.5.
//!
//! Field splitting is applied to the *results* of parameter expansion,
//! command substitution and arithmetic expansion that are not inside double
//! quotes (POSIX XCU 2.6.5). Literal source text is not split, even when it is
//! unquoted: `IFS=:; printf '<%s>' a:b` prints one field. That distinction is
//! carried by [`crate::Chunk::splittable`].
//!
//! The rules, quoted from POSIX XCU 2.6.5:
//!
//! > If the value of IFS is a `<space>`, `<tab>`, and `<newline>`, or if IFS
//! > is unset, sequences of `<space>`, `<tab>`, and `<newline>` at the
//! > beginning and end of the input shall be ignored and sequences of these
//! > characters shall delimit fields.
//! >
//! > If the value of IFS is not a `<space>`, `<tab>`, and `<newline>`:
//! >
//! > 1. IFS white space, if any, at the beginning and end of the input shall
//! >    be ignored.
//! > 2. Each occurrence in the input of an IFS character that is not IFS white
//! >    space, along with any adjacent IFS white space, shall delimit a field.
//! > 3. Non-zero-length sequences of IFS white space characters shall delimit
//! >    a field.
//!
//! In particular, a run of IFS whitespace is one delimiter (never producing
//! empty fields), a non-whitespace IFS character is a delimiter even with
//! whitespace around it, and a `:` at the end of the input does not produce a
//! trailing empty field while a `:` at the beginning does produce a leading
//! one. [`crate::split_fields`] implements this as a small state machine.

use smallvec::SmallVec;

use crate::vars::Vars;

/// The current IFS value, or the default when IFS is unset.
///
/// An IFS set to the empty string is *not* the default: it disables field
/// splitting entirely. `Ifs::new(b"")` therefore has no delimiters and no
/// join separator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ifs {
    bytes: SmallVec<[u8; 3]>,
}

impl Ifs {
    /// The POSIX default, used when IFS is unset.
    pub const DEFAULT: &'static [u8] = b" \t\n";

    /// The default IFS (`space`, `tab`, `newline`).
    pub fn default_posix() -> Self {
        Self::new(Self::DEFAULT)
    }

    /// Build from an explicit value. This is the IFS-*set* case; pass
    /// [`Ifs::default_posix`] for unset.
    pub fn new(bytes: &[u8]) -> Self {
        Self {
            bytes: SmallVec::from_slice(bytes),
        }
    }

    /// Build from a raw `Vars::get(b"IFS")` result: `None` (unset) uses the
    /// default, `Some(bytes)` is used verbatim.
    pub fn from_value(value: Option<&[u8]>) -> Self {
        match value {
            None => Self::default_posix(),
            Some(bytes) => Self::new(bytes),
        }
    }

    /// Read IFS from a variable source.
    pub fn from_vars<V: Vars + ?Sized>(vars: &V) -> Self {
        Self::from_value(vars.get(b"IFS"))
    }

    /// The raw IFS bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Whether `b` is an IFS character. Always false when IFS is empty.
    pub fn contains(&self, b: u8) -> bool {
        self.bytes.contains(&b)
    }

    /// POSIX's three "IFS white space" characters, regardless of IFS.
    pub fn is_whitespace(b: u8) -> bool {
        matches!(b, b' ' | b'\t' | b'\n')
    }

    /// Whether `b` is both an IFS character and IFS white space.
    pub fn is_ifs_whitespace(&self, b: u8) -> bool {
        Self::is_whitespace(b) && self.contains(b)
    }

    /// The separator used to join `$*` (and unquoted `$@`) in a command word:
    /// the first IFS character, or `None` when IFS is set but empty.
    ///
    /// POSIX XCU 2.5.3: "$* ... separated by the first character of the IFS
    /// variable". bash semantics with IFS unset is a space, which is exactly
    /// the first byte of the default.
    pub fn join_separator(&self) -> Option<u8> {
        self.bytes.first().copied()
    }
}

impl Default for Ifs {
    fn default() -> Self {
        Self::default_posix()
    }
}
