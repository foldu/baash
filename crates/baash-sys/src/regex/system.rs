//! The platform engine: glibc's `regcomp`/`regexec`.
//!
//! This is what bash links, so it is the fidelity reference for the other
//! backends. It takes C strings, so a pattern or subject with an embedded NUL
//! cannot be expressed, and every search copies the subject into a `CString`.
//!
//! # `unsafe`
//!
//! This module owns an opaque C allocation. [`Raw`] exists only after a
//! successful `regcomp`, and `Drop` calls `regfree` exactly once.

use std::mem::MaybeUninit;

use super::{CaptureRanges, RegexError};
use crate::bytes::cstring;

/// A compiled pattern owned by glibc.
pub(super) struct Raw {
    compiled: libc::regex_t,
}

impl Raw {
    /// `regcomp` with `REG_EXTENDED`, plus `REG_ICASE` when folding.
    pub(super) fn compile(pattern: &[u8], icase: bool) -> Result<Self, RegexError> {
        let pattern = cstring(pattern).map_err(|_| RegexError::NulInPattern)?;
        let mut flags = libc::REG_EXTENDED;
        if icase {
            flags |= libc::REG_ICASE;
        }
        let mut compiled = MaybeUninit::<libc::regex_t>::uninit();
        // SAFETY: `pattern` is NUL-terminated and lives for the call. On
        // success `regcomp` initialises the storage behind `compiled`; on
        // failure the storage stays uninitialised and is never turned into a
        // `Raw`, so `regfree` cannot observe it.
        let code = unsafe { libc::regcomp(compiled.as_mut_ptr(), pattern.as_ptr(), flags) };
        if code != 0 {
            // SAFETY: this is the canonical C usage after a failed `regcomp`:
            // `regerror` is given the same `preg` and the `REG_*` code, and
            // for compile failures it reads the code, never the undefined
            // compiled form.
            return Err(RegexError::Engine {
                code,
                message: regerror_message(code, compiled.as_ptr()),
            });
        }
        // SAFETY: `regcomp` returned 0, so it initialised the value. Moving it
        // out of `MaybeUninit` transfers ownership to `Raw`.
        let compiled = unsafe { compiled.assume_init() };
        Ok(Self { compiled })
    }

    /// `regexec` over the first `slots` capture slots.
    pub(super) fn exec(
        &self,
        text: &[u8],
        slots: usize,
    ) -> Result<Option<CaptureRanges>, RegexError> {
        let text = cstring(text).map_err(|_| RegexError::NulInText)?;
        let mut matches = vec![
            libc::regmatch_t {
                rm_so: -1,
                rm_eo: -1,
            };
            slots
        ];
        // SAFETY: `text` is NUL-terminated for the call; `matches` has `slots`
        // initialised entries and `regexec` writes at most that many; neither
        // pointer is retained after the call.
        let code = unsafe {
            libc::regexec(
                &self.compiled,
                text.as_ptr(),
                matches.len(),
                matches.as_mut_ptr(),
                0,
            )
        };
        if code == libc::REG_NOMATCH {
            return Ok(None);
        }
        if code != 0 {
            // SAFETY: `self.compiled` is live and initialised, and `code` is a
            // `regexec` failure code, which `regerror` accepts.
            return Err(RegexError::Engine {
                code,
                message: regerror_message(code, &self.compiled),
            });
        }
        Ok(Some(
            matches
                .into_iter()
                .map(|matched| {
                    // POSIX leaves unused entries at -1. A broken engine could
                    // also report an inverted range; treat both as "did not
                    // participate" rather than handing a caller an offset that
                    // would panic when used to slice.
                    if matched.rm_so < 0 || matched.rm_eo < matched.rm_so {
                        None
                    } else {
                        Some((matched.rm_so as usize, matched.rm_eo as usize))
                    }
                })
                .collect(),
        ))
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        // SAFETY: the type-level invariant says `compiled` was initialised by
        // a successful `regcomp` and has not been freed. `drop` runs at most
        // once for a value, and `Raw` cannot be copied, so this `regfree` is
        // the exactly-once release of that allocation. Values from a failed
        // `regcomp` never became a `Raw` and are never passed here.
        unsafe { libc::regfree(&mut self.compiled) };
    }
}

/// Read `regerror`'s message for `code` into an owned `String`.
///
/// The two-call `regerror` protocol (ask for the size, then fill a buffer) is
/// what POSIX specifies; the result includes the terminating NUL, so at least
/// one byte is always written on success.
fn regerror_message(code: libc::c_int, compiled: *const libc::regex_t) -> String {
    // SAFETY: `regerror` reads `code` and, depending on the code, the compiled
    // expression. A null buffer with size 0 is the documented way to ask how
    // much space the message needs; nothing is written.
    let needed = unsafe { libc::regerror(code, compiled, std::ptr::null_mut(), 0) };
    let mut message = if needed == 0 {
        String::new()
    } else {
        let mut buffer = vec![0u8; needed];
        // SAFETY: `buffer` is writable for its full length and `regerror`
        // writes at most `needed` bytes, including the terminating NUL.
        let written =
            unsafe { libc::regerror(code, compiled, buffer.as_mut_ptr().cast(), buffer.len()) };
        let limit = written.min(buffer.len());
        let end = buffer.iter().position(|&byte| byte == 0).unwrap_or(limit);
        // Message text comes from the locale's message catalogue and is not
        // required to be UTF-8; rendering it lossily keeps `Error: Display`.
        String::from_utf8_lossy(&buffer[..end]).into_owned()
    };
    if message.is_empty() {
        // The engine must not produce an empty explanation (a user typo is the
        // common case), so fall back to something still actionable.
        message = format!("regular expression error (REG_* code {code})");
    }
    message
}
