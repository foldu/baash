//! Byte-string to C-string conversion.
//!
//! Shell text is bytes (DESIGN.md §4.2): filenames, arguments and environment
//! entries are arbitrary non-NUL byte sequences. `exec` and every other C API
//! nevertheless take NUL-terminated strings, so the boundary has to either
//! convert or reject — and a shell may not silently truncate a filename at its
//! first NUL. Conversion therefore fails with [`NulByte`], which converts into
//! an [`io::Error`] with [`io::ErrorKind::InvalidInput`] for the many syscall
//! wrappers that return `io::Result`.

use std::ffi::CString;
use std::fmt;
use std::io;

/// A byte string contained a NUL and cannot be passed across the syscall
/// boundary.
///
/// `execve(2)` cannot represent such a string: the kernel would see a shorter
/// argument. Rejecting is the only behaviour that preserves the shell's data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NulByte;

impl fmt::Display for NulByte {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("embedded NUL byte in a syscall argument")
    }
}

impl std::error::Error for NulByte {}

impl From<NulByte> for io::Error {
    fn from(error: NulByte) -> Self {
        io::Error::new(io::ErrorKind::InvalidInput, error)
    }
}

/// Convert one shell byte string into a `CString`.
///
/// Reports the concrete [`NulByte`] error; `?` in an `io::Result` function
/// converts it through `From<NulByte> for io::Error`.
pub fn cstring(bytes: &[u8]) -> Result<CString, NulByte> {
    CString::new(bytes).map_err(|_| NulByte)
}

/// Convert an ordered sequence of shell byte strings, stopping at the first
/// embedded NUL.
pub fn cstrings<I, B>(items: I) -> Result<Vec<CString>, NulByte>
where
    I: IntoIterator<Item = B>,
    B: AsRef<[u8]>,
{
    items
        .into_iter()
        .map(|item| cstring(item.as_ref()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_bytes_round_trip() {
        assert_eq!(cstring(b"echo").unwrap().as_bytes(), b"echo");
        assert_eq!(cstring(b"").unwrap().as_bytes(), b"");
    }

    #[test]
    fn embedded_nul_is_rejected() {
        assert_eq!(cstring(b"a\0b").unwrap_err(), NulByte);
    }

    #[test]
    fn a_nul_in_any_element_rejects_the_list() {
        assert_eq!(
            cstrings([b"ok".as_slice(), b"bad\0".as_slice()]).unwrap_err(),
            NulByte
        );
    }

    #[test]
    fn nul_becomes_invalid_input_at_the_syscall_boundary() {
        let error = io::Error::from(NulByte);
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            error.get_ref().unwrap().downcast_ref::<NulByte>(),
            Some(&NulByte)
        );
    }
}
