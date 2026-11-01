//! `strftime` for `printf '%(format)T'`.
//!
//! The conversion is bash's: the format is handed to the C library's
//! `strftime` with bash's 128-byte result buffer and the shell's *exported*
//! `TZ`. Two observable behaviours follow from that, and both are pinned by the
//! conformance suite:
//!
//! * a result that does not fit in 128 bytes is empty (`strftime` returns 0),
//!   not truncated;
//! * a `TZ` shell variable that is not exported is invisible to the
//!   conversion, while a `TZ` that is exported applies immediately — bash keeps
//!   the process environment in step with the exported shell variables, so the
//!   same sync happens here before the call.
//!
//! The window between the environment update and the conversion is not
//! reentrant: `printf %T` runs on the interpreter thread, like every other
//! built-in, and no other thread in the process reads `TZ`.

/// The size of bash's `strftime` buffer, including the terminating NUL.
#[cfg(unix)]
const BUFFER: usize = 128;

// `libc` declares `tzset` for Windows only, so the Unix declaration lives
// here. POSIX defines it, and every Unix libc the shell targets exports it.
#[cfg(unix)]
unsafe extern "C" {
    /// Synchronise the `TZ`-derived state the C library caches.
    fn tzset();
}

/// Format `seconds` since the Unix epoch with `format`.
///
/// `timezone` is `Some` exactly when the shell exports `TZ`; `None` means the
/// system default (and removes a stale `TZ` the process environment still
/// carries). The result is empty when `strftime` could not fit it into
/// [`BUFFER`] bytes.
#[cfg(unix)]
pub fn strftime_local(seconds: i64, format: &[u8], timezone: Option<&[u8]>) -> Vec<u8> {
    use std::ffi::CString;

    ensure_zoneinfo();
    let format = CString::new(trim_at_nul(format)).unwrap_or_default();
    // SAFETY: every pointer below is valid for the call. `setenv`/`unsetenv`
    // receive NUL-terminated strings (an interior NUL was trimmed above);
    // `localtime_r` writes into `broken_down`; `strftime` writes at most
    // `BUFFER` bytes into `buffer` and returns the count it wrote, which is
    // sliced back out. No pointer outlives the call.
    unsafe {
        let name = c"TZ";
        match timezone {
            Some(timezone) => {
                let timezone = CString::new(trim_at_nul(timezone)).unwrap_or_default();
                libc::setenv(name.as_ptr(), timezone.as_ptr(), 1);
            }
            None => {
                libc::unsetenv(name.as_ptr());
            }
        }
        tzset();
        let when = seconds as libc::time_t;
        let mut broken_down: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&when, &mut broken_down).is_null() {
            return Vec::new();
        }
        let mut buffer = [0 as libc::c_char; BUFFER];
        let written = libc::strftime(
            buffer.as_mut_ptr(),
            buffer.len(),
            format.as_ptr(),
            &broken_down,
        );
        buffer[..written].iter().map(|&byte| byte as u8).collect()
    }
}

/// Format `seconds` since the Unix epoch with `format`, in UTC.
///
/// Windows has no IANA timezone database behind the C library, so `%T` is a
/// UTC-only conversion here: the specifiers scripts actually use (`%Y %m %d
/// %H %M %S %F %T %s` and friends) are implemented, `%Z`/`%z` report `UTC`,
/// and an unknown specifier is copied through unchanged. `TZ` is ignored.
/// `docs/portability.md` records the gap; there is no pretence of matching
/// bash's local time here.
#[cfg(not(unix))]
pub fn strftime_local(seconds: i64, format: &[u8], timezone: Option<&[u8]>) -> Vec<u8> {
    let _ = timezone;
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = second_of_day / 3600;
    let minute = (second_of_day % 3600) / 60;
    let second = second_of_day % 60;
    // 1970-01-01 was a Thursday, so `days % 7 == 0` is a Thursday.
    let weekday = (days + 4).rem_euclid(7);
    let year_day = day_of_year(year, month, day);

    let mut out = Vec::with_capacity(format.len() + 16);
    let mut index = 0;
    while index < format.len() {
        if format[index] != b'%' {
            out.push(format[index]);
            index += 1;
            continue;
        }
        index += 1;
        let Some(&directive) = format.get(index) else {
            out.push(b'%');
            break;
        };
        index += 1;
        match directive {
            b'%' => out.push(b'%'),
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'Y' => out.extend_from_slice(year.to_string().as_bytes()),
            b'y' => push_padded(&mut out, year.rem_euclid(100) as u64, 2),
            b'm' => push_padded(&mut out, month as u64, 2),
            b'd' => push_padded(&mut out, day as u64, 2),
            b'e' => {
                if day < 10 {
                    out.push(b' ');
                }
                out.extend_from_slice(day.to_string().as_bytes());
            }
            b'H' => push_padded(&mut out, hour as u64, 2),
            b'I' => {
                let twelve = if hour % 12 == 0 { 12 } else { hour % 12 };
                push_padded(&mut out, twelve as u64, 2);
            }
            b'M' => push_padded(&mut out, minute as u64, 2),
            b'S' => push_padded(&mut out, second as u64, 2),
            b'p' => out.extend_from_slice(if hour < 12 { b"AM" } else { b"PM" }),
            b'j' => push_padded(&mut out, year_day as u64, 3),
            b'a' => out.extend_from_slice(WEEKDAYS_SHORT[weekday as usize]),
            b'A' => out.extend_from_slice(WEEKDAYS[weekday as usize]),
            b'b' | b'h' => out.extend_from_slice(MONTHS_SHORT[(month - 1) as usize]),
            b'B' => out.extend_from_slice(MONTHS[(month - 1) as usize]),
            b'F' => {
                out.extend_from_slice(year.to_string().as_bytes());
                out.push(b'-');
                push_padded(&mut out, month as u64, 2);
                out.push(b'-');
                push_padded(&mut out, day as u64, 2);
            }
            b'T' => {
                push_padded(&mut out, hour as u64, 2);
                out.push(b':');
                push_padded(&mut out, minute as u64, 2);
                out.push(b':');
                push_padded(&mut out, second as u64, 2);
            }
            b'R' => {
                push_padded(&mut out, hour as u64, 2);
                out.push(b':');
                push_padded(&mut out, minute as u64, 2);
            }
            b'D' => {
                push_padded(&mut out, month as u64, 2);
                out.push(b'/');
                push_padded(&mut out, day as u64, 2);
                out.push(b'/');
                push_padded(&mut out, year.rem_euclid(100) as u64, 2);
            }
            b's' => out.extend_from_slice(seconds.to_string().as_bytes()),
            b'Z' => out.extend_from_slice(b"UTC"),
            b'z' => out.extend_from_slice(b"+0000"),
            other => {
                out.push(b'%');
                out.push(other);
            }
        }
    }
    out
}

/// Point the C library at a zoneinfo database when the environment has none.
///
/// bash hands `strftime` the exported `TZ` and the C library looks the zone up
/// in its compiled-in database directory. A shell linked against a Nix glibc
/// inherits a *store path* for that directory which need not exist when the
/// store entry was not part of the closure, and a minimal environment (the
/// conformance harness clears the environment) has no `TZDIR` to correct it.
/// When `TZDIR` is absent, use the first conventional database directory that
/// exists; a POSIX `TZ` string such as `UTC0` is unaffected because the C
/// library only consults the directory for zone names.
#[cfg(unix)]
fn ensure_zoneinfo() {
    if std::env::var_os("TZDIR").is_some() {
        return;
    }
    for candidate in [
        "/etc/zoneinfo",
        "/usr/share/zoneinfo",
        "/usr/lib/zoneinfo",
        "/run/current-system/sw/share/zoneinfo",
    ] {
        if !std::path::Path::new(candidate).is_dir() {
            continue;
        }
        let Ok(value) = std::ffi::CString::new(candidate) else {
            continue;
        };
        // SAFETY: `value` is a NUL-terminated string and the `NAME` literal is
        // a valid NUL-terminated key; `overwrite` is 0, so an existing value is
        // never replaced (the check above already established there is none).
        unsafe {
            libc::setenv(c"TZDIR".as_ptr(), value.as_ptr(), 0);
        }
        return;
    }
}

/// Truncate at an interior NUL: the C library takes a C string.
#[cfg(unix)]
fn trim_at_nul(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|&byte| byte == 0) {
        Some(end) => &bytes[..end],
        None => bytes,
    }
}

#[cfg(not(unix))]
const WEEKDAYS: [&[u8]; 7] = [
    b"Sunday",
    b"Monday",
    b"Tuesday",
    b"Wednesday",
    b"Thursday",
    b"Friday",
    b"Saturday",
];

#[cfg(not(unix))]
const WEEKDAYS_SHORT: [&[u8]; 7] = [b"Sun", b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat"];

#[cfg(not(unix))]
const MONTHS: [&[u8]; 12] = [
    b"January",
    b"February",
    b"March",
    b"April",
    b"May",
    b"June",
    b"July",
    b"August",
    b"September",
    b"October",
    b"November",
    b"December",
];

#[cfg(not(unix))]
const MONTHS_SHORT: [&[u8]; 12] = [
    b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov", b"Dec",
];

#[cfg(not(unix))]
fn push_padded(out: &mut Vec<u8>, value: u64, width: usize) {
    let text = value.to_string();
    for _ in text.len()..width {
        out.push(b'0');
    }
    out.extend_from_slice(text.as_bytes());
}

/// Days since the Unix epoch as a proleptic Gregorian date.
///
/// Howard Hinnant's `civil_from_days`: `days` is shifted to an era that starts
/// in March so leap days land at the end of the year, which makes the month
/// arithmetic branch-free.
#[cfg(not(unix))]
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// The one-based day of the year.
#[cfg(not(unix))]
fn day_of_year(year: i64, month: u32, day: u32) -> u32 {
    const BEFORE: [u32; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    BEFORE[(month - 1) as usize] + day + if leap && month > 2 { 1 } else { 0 }
}
