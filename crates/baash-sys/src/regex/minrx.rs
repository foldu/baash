//! MinRX: a small POSIX ERE engine (BSD-2-Clause) vendored by `minrx-sys`.
//!
//! Byte-oriented and length-based: `minrx_regncomp`/`minrx_regnexec` take an
//! explicit length, so a shell pattern or subject may contain an embedded NUL
//! and no per-search copy is needed. GNU `\b \w \s` and BSD `\< \>` are enabled
//! because glibc accepts them and bash-completion uses `\b`.
//!
//! # `unsafe`
//!
//! [`Raw`] owns the engine's compiled expression. `minrx_regexec` takes it by
//! mutable pointer because the engine caches per-search state inside it, so
//! the cell is `UnsafeCell`; `Raw` is consequently neither `Send` nor `Sync`,
//! which matches its use (one shell thread). `Drop` calls `minrx_regfree`
//! exactly once.

use std::cell::UnsafeCell;

use super::{CaptureRanges, RegexError};

/// `MINRX_REG_EXTENDED`, the POSIX extended syntax.
const EXTENDED: libc::c_int = minrx_sys::minrx_regcomp_flags_t_MINRX_REG_EXTENDED as libc::c_int;
/// `MINRX_REG_ICASE`, case folding for `nocasematch`.
const ICASE: libc::c_int = minrx_sys::minrx_regcomp_flags_t_MINRX_REG_ICASE as libc::c_int;
/// `MINRX_REG_EXTENSIONS_BSD`: `\<` and `\>`.
const EXTENSIONS_BSD: libc::c_int =
    minrx_sys::minrx_regcomp_flags_t_MINRX_REG_EXTENSIONS_BSD as libc::c_int;
/// `MINRX_REG_EXTENSIONS_GNU`: `\b \B \s \S \w \W`.
const EXTENSIONS_GNU: libc::c_int =
    minrx_sys::minrx_regcomp_flags_t_MINRX_REG_EXTENSIONS_GNU as libc::c_int;

/// `MINRX_REG_SUCCESS`.
const SUCCESS: libc::c_int = minrx_sys::minrx_result_t_MINRX_REG_SUCCESS as libc::c_int;
/// `MINRX_REG_NOMATCH`: the pattern did not match, which is not an error.
const NOMATCH: libc::c_int = minrx_sys::minrx_result_t_MINRX_REG_NOMATCH as libc::c_int;

/// A compiled pattern owned by MinRX.
pub(super) struct Raw {
    /// See the module docs: the engine mutates this during a search.
    re: UnsafeCell<minrx_sys::minrx_regex_t>,
}

impl Raw {
    /// `minrx_regncomp` with the ERE and extension flags.
    pub(super) fn compile(pattern: &[u8], icase: bool) -> Result<Self, RegexError> {
        let mut flags = EXTENDED | EXTENSIONS_GNU | EXTENSIONS_BSD;
        if icase {
            flags |= ICASE;
        }
        let mut re = minrx_sys::minrx_regex_t {
            re_regexp: std::ptr::null_mut(),
            re_nsub: 0,
            re_compflags: 0,
        };
        // SAFETY: `re` is writable storage and `pattern` is readable for
        // `pattern.len()` bytes; the engine copies what it needs and retains
        // neither pointer.
        let code = unsafe {
            minrx_sys::minrx_regncomp(&mut re, pattern.len(), pattern.as_ptr().cast(), flags)
        };
        if code != SUCCESS {
            // SAFETY: `re` is the storage that the failed `minrx_regncomp` was
            // given; `minrx_regerror` reads it (and the code) to format.
            return Err(RegexError::Engine {
                code,
                message: error_message(code, &re),
            });
        }
        Ok(Self {
            re: UnsafeCell::new(re),
        })
    }

    /// `minrx_regnexec` over the first `slots` capture slots.
    pub(super) fn exec(
        &self,
        text: &[u8],
        slots: usize,
    ) -> Result<Option<CaptureRanges>, RegexError> {
        let mut matches = vec![
            minrx_sys::minrx_regmatch_t {
                rm_so: -1,
                rm_eo: -1,
            };
            slots
        ];
        // SAFETY: the cell is not aliased during the call (one shell thread,
        // and `exec` takes `&self` without handing the pointer out), `text` is
        // readable for `text.len()` bytes, and `matches` is writable for
        // `slots` entries, which is the count passed.
        let code = unsafe {
            minrx_sys::minrx_regnexec(
                self.re.get(),
                text.len(),
                text.as_ptr().cast(),
                slots,
                matches.as_mut_ptr(),
                0,
            )
        };
        match code {
            SUCCESS => {}
            NOMATCH => return Ok(None),
            code => {
                // SAFETY: the cell was compiled successfully and is live; the
                // engine only reads it to format the message.
                let re = unsafe { &*self.re.get() };
                return Err(RegexError::Engine {
                    code,
                    message: error_message(code, re),
                });
            }
        }
        Ok(Some(
            matches
                .into_iter()
                .map(|matched| {
                    // Unused slots stay at -1. An inverted range is treated as
                    // "did not participate" rather than handed to a caller
                    // that would slice with it.
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
        // SAFETY: the engine compiled this expression successfully and it has
        // not been freed; `drop` runs once and `Raw` is not copyable.
        unsafe { minrx_sys::minrx_regfree(self.re.get()) };
    }
}

/// Ask MinRX for the text belonging to `code`.
fn error_message(code: libc::c_int, re: &minrx_sys::minrx_regex_t) -> String {
    // The two-call protocol: a null buffer asks for the required size.
    // SAFETY: `re` is the storage the failed call was given; nothing is
    // written when the buffer is null and the size is zero.
    let needed = unsafe { minrx_sys::minrx_regerror(code, re, std::ptr::null_mut(), 0) };
    let mut message = if needed == 0 {
        String::new()
    } else {
        let mut buffer = vec![0u8; needed];
        // SAFETY: `buffer` is writable for `needed` bytes and MinRX writes at
        // most that many, including a terminating NUL when there is room.
        let written =
            unsafe { minrx_sys::minrx_regerror(code, re, buffer.as_mut_ptr().cast(), needed) };
        let end = buffer
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(written.min(buffer.len()));
        String::from_utf8_lossy(&buffer[..end]).into_owned()
    };
    if message.is_empty() {
        message = format!("regular expression error (MinRX code {code})");
    }
    message
}
