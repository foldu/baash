//! The permission questions `test -r` / `-w` / `-x` ask.
//!
//! bash asks with the *effective* ids (it uses `eaccess` where the platform has
//! it), which is why `access` lives here rather than in the built-in: `std` has
//! no equivalent, and the answer for root differs from a plain mode-bit test.

/// Which permission to ask about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// `-r`
    Read,
    /// `-w`
    Write,
    /// `-x`
    Execute,
}

/// Whether this process may use `path` in the requested way.
///
/// `false` covers both "no" and "the path does not exist"; callers that need
/// the difference ask about existence first.
#[cfg(unix)]
pub fn access(path: &[u8], wanted: Access) -> bool {
    use rustix::fs::{Access as Wanted, AtFlags, CWD, accessat};

    let wanted = match wanted {
        Access::Read => Wanted::READ_OK,
        Access::Write => Wanted::WRITE_OK,
        Access::Execute => Wanted::EXEC_OK,
    };
    // `AT_EACCESS` asks with the effective ids, which is bash's question; the
    // default (`0`) would ask with the real ones and answer differently under
    // `sudo` for a setuid-aware program.
    accessat(
        CWD,
        crate::os::path_from_bytes(path),
        wanted,
        AtFlags::EACCESS,
    )
    .is_ok()
}

/// Whether this process may use `path` in the requested way.
///
/// Windows has no `access(2)`. A read-only attribute is the only permission
/// information a `std::fs::Metadata` exposes, so `-w` reports the attribute and
/// `-r`/`-x` report existence; `test -x` on Windows therefore means "exists",
/// which is the same approximation `std`'s `permissions()` forces. The gap is
/// recorded in `docs/portability.md`.
#[cfg(windows)]
pub fn access(path: &[u8], wanted: Access) -> bool {
    let path = crate::os::path_from_bytes(path);
    let Ok(metadata) = std::fs::metadata(&path) else {
        return false;
    };
    match wanted {
        Access::Write => !metadata.permissions().readonly(),
        Access::Read | Access::Execute => true,
    }
}
