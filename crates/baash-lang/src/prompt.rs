//! Prompt strings: `PS1`/`PS2`, and the `${x@P}` parameter transformation.
//!
//! One expander serves both callers so they cannot drift: an interactive
//! shell renders `PS1`/`PS2` with [`expand_with_variables`], and
//! [`crate::expand`] produces `${x@P}` with the same function. Both decode the
//! backslash escapes and then — when [`PromptContext::prompt_vars`] is on,
//! which is bash's `shopt promptvars` default — expand the result a second
//! time as if it were inside double quotes. That second pass needs the
//! shell's [`Shell`] callbacks: it expands parameters, arithmetic and command
//! substitutions, so `PS1='$(date +%H:%M) $ '` runs `date` at prompt time.
//! [`expand`] is the single-pass form, exactly bash's `shopt -u promptvars`
//! rendering, and is what a caller with no interpreter can do.
//!
//! The second pass runs **per non-printing stretch**: a prompt whose
//! `\[`…`\]` coloured prefix also contains a variable keeps the width
//! bookkeeping [`ExpandedPrompt::non_printing`] carries, so the editor can
//! still compute the cursor column. A construct that straddles a `\[`/`\]`
//! boundary (`${x` in one stretch, `}` in another) is expanded one stretch at
//! a time, which is the only reading that keeps the spans meaningful.
//!
//! # What is measured, not assumed
//!
//! The escape table follows bash 5.3.20's `decode_prompt_string`, measured
//! under `LC_ALL=C`:
//!
//! * An unknown escape keeps its backslash (`\z` is `\z`); a trailing lone
//!   backslash stays one backslash.
//! * `\nnn` consumes **up to three octal digits, and fails as a whole** when
//!   any of the up-to-three bytes that follow is not an octal digit (`\1x`
//!   stays `\1x`). The value wraps modulo 256, and a zero byte disappears
//!   (`\400` is empty), because the C implementation stores it in a string.
//! * `\$` is not a literal dollar: it is *the prompt character*, `#` for root
//!   and `$` otherwise. When variables are expanded afterwards the non-root
//!   form is written `\$` so the second pass removes the backslash; that is
//!   why `PS1='\\$'` and `PS1='\$'` both print one `$`, while `PS1='\\\$'`
//!   prints `\$`.
//! * `\[` and `\]` are removed from the output. Their contents are recorded in
//!   [`ExpandedPrompt::non_printing`] so a caller can keep them out of the
//!   display width. An unmatched `\[` hides the rest of the string; a stray
//!   `\]` is ignored; a second `\[` while one is open changes nothing.
//! * `\W` is `~` when the working directory *is* `$HOME` (bash abbreviates
//!   before taking the basename) and the basename otherwise; `\w` abbreviates
//!   a `$HOME` prefix only at a path boundary.
//! * `\l` is `tty` when standard input is not a terminal.
//!
//! # Values
//!
//! [`PromptContext`] is plain data. It is not a [`crate::Shell`] method: the
//! editor has no interpreter and the interpreter's expansion has no terminal.
//! Callers build the context from whatever they own and pass it in, which keeps
//! the expander pure and lets tests freeze the clock. The one exception is
//! [`Shell::prompt_context`], which is how `${x@P}` asks a shell for the
//! facts; an interactive shell builds the prompt's context the same way and
//! then overrides the few values a prompt needs (`\!`, the terminal).
//!
//! Local time is computed here, without `libc` or a new dependency: the epoch
//! is converted with the civil-calendar algorithm and the UTC offset is read
//! from the `TZ` file (or `/etc/localtime`) in the TZif format. A `TZ` value
//! that is a POSIX rule string rather than a file falls back to
//! `/etc/localtime`; when nothing is readable the offset is zero.

use crate::Span;
use crate::host::Shell;

/// One local calendar time, as the prompt escapes need it.
///
/// The fields are the local civil time (`year` 1970, `month` 1–12, `day` 1–31,
/// 24-hour `hour`) plus the two facts `strftime` needs and calendars do not
/// supply: the zero-based weekday (Sunday is 0) and the one-based day of year.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PromptTime {
    /// Local calendar year.
    pub year: i64,
    /// Month, 1–12.
    pub month: u8,
    /// Day of month, 1–31.
    pub day: u8,
    /// Hour, 0–23.
    pub hour: u8,
    /// Minute, 0–59.
    pub minute: u8,
    /// Second, 0–59.
    pub second: u8,
    /// Weekday, 0 = Sunday.
    pub weekday: u8,
    /// Day of year, 1–366.
    pub yearday: u16,
    /// Seconds since the Unix epoch, for `%s`.
    pub epoch: i64,
    /// Seconds east of UTC in effect at [`PromptTime::epoch`], for `%z`.
    pub utc_offset: i32,
    /// Whether daylight saving time was in effect.
    pub is_dst: bool,
}

impl PromptTime {
    /// The current local time.
    pub fn now() -> Self {
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        Self::from_unix(epoch)
    }

    /// The local time at `epoch` seconds, using the process's time zone.
    pub fn from_unix(epoch: i64) -> Self {
        let (offset, is_dst) = local_offset(epoch);
        Self::from_unix_offset(epoch, offset, is_dst)
    }

    /// The UTC time at `epoch` seconds.
    pub fn from_unix_utc(epoch: i64) -> Self {
        Self::from_unix_offset(epoch, 0, false)
    }

    fn from_unix_offset(epoch: i64, offset: i32, is_dst: bool) -> Self {
        let local = epoch.saturating_add(i64::from(offset));
        let days = local.div_euclid(86_400);
        let seconds = local.rem_euclid(86_400);
        let (year, month, day, yearday) = civil_from_days(days);
        Self {
            year,
            month,
            day,
            hour: u8::try_from(seconds / 3_600).unwrap_or(0),
            minute: u8::try_from(seconds % 3_600 / 60).unwrap_or(0),
            second: u8::try_from(seconds % 60).unwrap_or(0),
            weekday: u8::try_from((days + 4).rem_euclid(7)).unwrap_or(0),
            yearday,
            epoch,
            utc_offset: offset,
            is_dst,
        }
    }

    /// Format this time with the `strftime` subset the prompt uses.
    ///
    /// `LC_ALL=C` names are used: the shell is byte-oriented and does not
    /// track a locale for month and weekday names. Unknown conversions are
    /// copied through unchanged (`%Q` is `%Q`), which is what `strftime` does.
    /// An empty format is bash's `%X`.
    pub fn format(&self, format: &[u8]) -> Vec<u8> {
        let format: &[u8] = if format.is_empty() { b"%X" } else { format };
        let mut out = Vec::with_capacity(format.len() + 16);
        let mut at = 0;
        while at < format.len() {
            if format[at] != b'%' {
                out.push(format[at]);
                at += 1;
                continue;
            }
            let Some(&conversion) = format.get(at + 1) else {
                out.push(b'%');
                break;
            };
            at += 2;
            match conversion {
                b'%' => out.push(b'%'),
                b'n' => out.push(b'\n'),
                b't' => out.push(b'\t'),
                b'a' => out.extend_from_slice(weekday_name(self.weekday, true)),
                b'A' => out.extend_from_slice(weekday_name(self.weekday, false)),
                b'b' | b'h' => out.extend_from_slice(month_name(self.month, true)),
                b'B' => out.extend_from_slice(month_name(self.month, false)),
                b'c' => out.extend_from_slice(&self.format(b"%a %b %e %H:%M:%S %Y")),
                b'C' => push_padded(&mut out, self.year.div_euclid(100), 2),
                b'd' => push_padded(&mut out, i64::from(self.day), 2),
                b'e' => push_space_padded(&mut out, i64::from(self.day), 2),
                b'D' => out.extend_from_slice(&self.format(b"%m/%d/%y")),
                b'F' => out.extend_from_slice(&self.format(b"%Y-%m-%d")),
                b'H' => push_padded(&mut out, i64::from(self.hour), 2),
                b'I' => push_padded(&mut out, i64::from(hour12(self.hour)), 2),
                b'j' => push_padded(&mut out, i64::from(self.yearday), 3),
                b'm' => push_padded(&mut out, i64::from(self.month), 2),
                b'M' => push_padded(&mut out, i64::from(self.minute), 2),
                b'p' => out.extend_from_slice(if self.hour < 12 { b"AM" } else { b"PM" }),
                b'r' => out.extend_from_slice(&self.format(b"%I:%M:%S %p")),
                b'R' => out.extend_from_slice(&self.format(b"%H:%M")),
                b's' => out.extend_from_slice(self.epoch.to_string().as_bytes()),
                b'S' => push_padded(&mut out, i64::from(self.second), 2),
                b'T' => out.extend_from_slice(&self.format(b"%H:%M:%S")),
                b'u' => {
                    if self.weekday == 0 {
                        out.push(b'7');
                    } else {
                        out.push(b'0' + self.weekday);
                    }
                }
                b'U' => push_padded(
                    &mut out,
                    i64::from((u32::from(self.yearday) + 6 - u32::from(self.weekday)) / 7),
                    2,
                ),
                b'w' => out.push(b'0' + self.weekday),
                b'W' => push_padded(
                    &mut out,
                    i64::from(
                        (u32::from(self.yearday) + 6 - (u32::from(self.weekday) + 6) % 7) / 7,
                    ),
                    2,
                ),
                b'x' => out.extend_from_slice(&self.format(b"%m/%d/%y")),
                b'X' => out.extend_from_slice(&self.format(b"%H:%M:%S")),
                b'y' => push_padded(&mut out, self.year.rem_euclid(100), 2),
                b'Y' => out.extend_from_slice(self.year.to_string().as_bytes()),
                b'z' => push_offset(&mut out, self.utc_offset),
                b'Z' => out.extend_from_slice(if self.is_dst { b"DST" } else { b"UTC" }),
                other => {
                    out.push(b'%');
                    out.push(other);
                }
            }
        }
        out
    }
}

fn hour12(hour: u8) -> u8 {
    match hour % 12 {
        0 => 12,
        other => other,
    }
}

fn weekday_name(weekday: u8, short: bool) -> &'static [u8] {
    const SHORT: [&[u8]; 7] = [b"Sun", b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat"];
    const LONG: [&[u8]; 7] = [
        b"Sunday",
        b"Monday",
        b"Tuesday",
        b"Wednesday",
        b"Thursday",
        b"Friday",
        b"Saturday",
    ];
    let index = usize::from(weekday).min(6);
    if short { SHORT[index] } else { LONG[index] }
}

fn month_name(month: u8, short: bool) -> &'static [u8] {
    const SHORT: [&[u8]; 12] = [
        b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov",
        b"Dec",
    ];
    const LONG: [&[u8]; 12] = [
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
    let index = usize::from(month.clamp(1, 12)) - 1;
    if short { SHORT[index] } else { LONG[index] }
}

fn push_padded(out: &mut Vec<u8>, value: i64, width: usize) {
    let text = value.abs().to_string();
    if value < 0 {
        out.push(b'-');
    }
    for _ in text.len()..width {
        out.push(b'0');
    }
    out.extend_from_slice(text.as_bytes());
}

fn push_space_padded(out: &mut Vec<u8>, value: i64, width: usize) {
    let text = value.abs().to_string();
    if value < 0 {
        out.push(b'-');
    }
    for _ in text.len()..width {
        out.push(b' ');
    }
    out.extend_from_slice(text.as_bytes());
}

fn push_offset(out: &mut Vec<u8>, offset: i32) {
    let sign = if offset < 0 { b'-' } else { b'+' };
    let offset = offset.unsigned_abs();
    out.push(sign);
    push_padded(out, i64::from(offset / 3_600), 2);
    push_padded(out, i64::from(offset % 3_600 / 60), 2);
}

/// Civil date from days since 1970-01-01, and the one-based day of year.
///
/// Howard Hinnant's `civil_from_days`: exact for the whole `i64` range the
/// epoch arithmetic can represent, with no table and no `libc`.
fn civil_from_days(days: i64) -> (i64, u8, u8, u16) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (
        year,
        u8::try_from(m).unwrap_or(0),
        u8::try_from(d).unwrap_or(0),
        day_of_year(year, m, d),
    )
}

/// The one-based calendar day of year, from the civil date.
fn day_of_year(year: i64, month: i64, day: i64) -> u16 {
    const CUMULATIVE: [i64; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let index = usize::try_from(month.clamp(1, 12) - 1).unwrap_or(0);
    let extra = i64::from(leap && month > 2);
    u16::try_from(CUMULATIVE[index] + day + extra).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Time zone
// ---------------------------------------------------------------------------

/// The UTC offset and DST flag in effect at `epoch`, from the process's zone.
///
/// `TZ` is resolved the way `tzset` does for the common cases: an empty value
/// is UTC, a path is read directly, and a zone name (`Europe/Amsterdam`,
/// `UTC`) is looked up under `TZDIR` (default `/usr/share/zoneinfo`). A POSIX
/// rule string is not parsed; it falls back to `/etc/localtime`, and an
/// unreadable zone is UTC.
fn local_offset(epoch: i64) -> (i32, bool) {
    if let Some(tz) = std::env::var_os("TZ") {
        let tz = tz.as_encoded_bytes();
        if tz.is_empty() {
            return (0, false);
        }
        let name = tz.strip_prefix(b":").unwrap_or(tz);
        if let Some(found) = zone_file(name).and_then(|data| tzif_offset(&data, epoch)) {
            return found;
        }
    }
    std::fs::read("/etc/localtime")
        .ok()
        .and_then(|data| tzif_offset(&data, epoch))
        .unwrap_or((0, false))
}

/// Read a TZif file for a `TZ` value: an explicit path, or a name under the
/// system zoneinfo directory.
fn zone_file(name: &[u8]) -> Option<Vec<u8>> {
    let path = os_path(name);
    if let Ok(data) = std::fs::read(path) {
        return Some(data);
    }
    if name.starts_with(b"/") {
        return None;
    }
    let mut absolute = env_bytes("TZDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| b"/usr/share/zoneinfo".to_vec());
    if !absolute.ends_with(b"/") {
        absolute.push(b'/');
    }
    absolute.extend_from_slice(name);
    let path = os_path(&absolute);
    std::fs::read(path).ok()
}

/// One TZif file's offset at `epoch`, version 1 through 3.
///
/// The format is RFC 8536: a 44-byte header, a 32-bit block, and (for versions
/// 2 and 3) a second header and a 64-bit block. Only the transition table is
/// read; leap seconds, the designation strings and the POSIX footer rule are
/// not needed to answer "what offset applied at this instant".
fn tzif_offset(data: &[u8], epoch: i64) -> Option<(i32, bool)> {
    let header = TZifHeader::parse(data, 0)?;
    if header.version >= b'2' {
        let second = 44 + header.block_len(4)?;
        let header = TZifHeader::parse(data, second)?;
        let body = second + 44;
        return parse_block(data, body, &header, 8, epoch);
    }
    parse_block(data, 44, &header, 4, epoch)
}

struct TZifHeader {
    version: u8,
    isutcnt: usize,
    isstdcnt: usize,
    leapcnt: usize,
    timecnt: usize,
    typecnt: usize,
    charcnt: usize,
}

impl TZifHeader {
    fn parse(data: &[u8], at: usize) -> Option<Self> {
        let bytes = data.get(at..at + 44)?;
        if &bytes[..4] != b"TZif" {
            return None;
        }
        let count = |index: usize| -> Option<usize> {
            let start = 20 + index * 4;
            Some(u32::from_be_bytes(bytes[start..start + 4].try_into().ok()?) as usize)
        };
        Some(Self {
            version: bytes[4],
            isutcnt: count(0)?,
            isstdcnt: count(1)?,
            leapcnt: count(2)?,
            timecnt: count(3)?,
            typecnt: count(4)?,
            charcnt: count(5)?,
        })
    }

    fn block_len(&self, time_size: usize) -> Option<usize> {
        let times = self.timecnt.checked_mul(time_size)?;
        let indices = self.timecnt;
        let types = self.typecnt.checked_mul(6)?;
        let leaps = self.leapcnt.checked_mul(time_size + 4)?;
        times
            .checked_add(indices)?
            .checked_add(types)?
            .checked_add(self.charcnt)?
            .checked_add(leaps)?
            .checked_add(self.isstdcnt)?
            .checked_add(self.isutcnt)
    }
}

fn parse_block(
    data: &[u8],
    at: usize,
    header: &TZifHeader,
    time_size: usize,
    epoch: i64,
) -> Option<(i32, bool)> {
    let times_len = header.timecnt.checked_mul(time_size)?;
    let times = data.get(at..at + times_len)?;
    let indices = data.get(at + times_len..at + times_len + header.timecnt)?;
    let types_at = at + times_len + header.timecnt;
    let types = data.get(types_at..types_at + header.typecnt.checked_mul(6)?)?;

    let type_at = |index: usize| -> Option<(i32, bool)> {
        let entry = types.get(index.checked_mul(6)?..index.checked_mul(6)? + 6)?;
        Some((
            i32::from_be_bytes(entry[..4].try_into().ok()?),
            entry[4] != 0,
        ))
    };
    // RFC 8536: the default type, for instants before the first transition,
    // is the first standard-time type; type 0 when every type is DST.
    let default = (0..header.typecnt)
        .find(|&index| type_at(index).is_some_and(|(_, is_dst)| !is_dst))
        .unwrap_or(0);

    if header.timecnt == 0 {
        return type_at(default);
    }
    let transition = |index: usize| -> Option<i64> {
        let entry =
            times.get(index.checked_mul(time_size)?..index.checked_mul(time_size)? + time_size)?;
        Some(if time_size == 8 {
            i64::from_be_bytes(entry.try_into().ok()?)
        } else {
            i64::from(i32::from_be_bytes(entry.try_into().ok()?))
        })
    };
    let mut selected = None;
    for index in 0..header.timecnt {
        if transition(index)? > epoch {
            break;
        }
        selected = Some(index);
    }
    match selected {
        Some(index) => type_at(usize::from(*indices.get(index)?)),
        None => type_at(default),
    }
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

/// The facts a prompt escape reads.
///
/// It is deliberately plain data: the expander stays pure, tests freeze the
/// clock, and a caller with no interpreter (the line editor) can supply the
/// same values as one with a variable table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptContext {
    /// `\u`: the user name.
    pub user: Vec<u8>,
    /// `\h` (up to the first dot) and `\H` (whole): the host name.
    pub host: Vec<u8>,
    /// `\w` and `\W`: the current working directory.
    pub cwd: Vec<u8>,
    /// The previous working directory. No bash escape prints it today; it is
    /// carried because the shell state has it and a future `\w` variant will.
    pub previous_cwd: Vec<u8>,
    /// `\w` abbreviates this prefix to `~`.
    pub home: Vec<u8>,
    /// `\s`: the name the shell was invoked as.
    pub shell: Vec<u8>,
    /// `\v`: the `major.minor` version.
    pub version: Vec<u8>,
    /// `\V`: the full version.
    pub release: Vec<u8>,
    /// `\!`: the history number of the current command.
    pub history_number: u32,
    /// `\#`: the number this command would get. `${x@P}` prints one less than
    /// the interactive prompt because bash decrements for a non-prompt
    /// decoding; the caller decides which value belongs here.
    pub command_number: u32,
    /// `\j`: the number of jobs.
    pub job_count: u32,
    /// `\$` is `#` for root and `$` otherwise.
    pub root: bool,
    /// Whether the previous command failed. Bash's prompt escapes do not print
    /// it (there is no `\?`); it is carried for a caller that wants to build a
    /// prompt template and for future escapes.
    pub last_failed: bool,
    /// The terminal width. No escape needs it, but a caller that truncates a
    /// prompt does; carried here so one context describes the whole prompt.
    pub columns: u16,
    /// `\l`: the basename of the terminal, or `tty` when input is not one.
    pub tty: Vec<u8>,
    /// `\t`, `\T`, `\@`, `\A`, `\d`, `\D{…}`.
    pub time: PromptTime,
    /// Whether the value is expanded a second time (bash's `promptvars`, on by
    /// default). When false, inserted values are not double-quote escaped and
    /// `\$` is the final `$`; that is what the line editor wants, because it
    /// has no interpreter to run the second pass.
    pub prompt_vars: bool,
}

impl Default for PromptContext {
    fn default() -> Self {
        Self {
            user: Vec::new(),
            host: Vec::new(),
            cwd: Vec::new(),
            previous_cwd: Vec::new(),
            home: Vec::new(),
            shell: b"baash".to_vec(),
            version: env!("CARGO_PKG_VERSION")
                .split('.')
                .take(2)
                .collect::<Vec<_>>()
                .join(".")
                .into_bytes(),
            release: env!("CARGO_PKG_VERSION").as_bytes().to_vec(),
            history_number: 1,
            command_number: 1,
            job_count: 0,
            root: false,
            last_failed: false,
            columns: 80,
            tty: b"tty".to_vec(),
            time: PromptTime::default(),
            prompt_vars: true,
        }
    }
}

impl PromptContext {
    /// Build a context from the process environment.
    ///
    /// This is what the line editor uses, and the default a [`Shell`] gets
    /// before it overrides the parts it owns (the logical `PWD`, the history
    /// number, the job count). The working directory comes from
    /// [`std::env::current_dir`] rather than `$PWD`: a shell that has just run
    /// `cd` changes the process directory but not its inherited environment.
    pub fn from_env() -> Self {
        let cwd = std::env::current_dir()
            .map(|path| os_bytes(path.as_os_str()))
            .unwrap_or_default();
        Self {
            user: current_user(),
            host: env_bytes("HOSTNAME")
                .or_else(read_hostname)
                .unwrap_or_default(),
            cwd,
            previous_cwd: env_bytes("OLDPWD").unwrap_or_default(),
            home: env_bytes("HOME").unwrap_or_default(),
            history_number: 1,
            time: PromptTime::now(),
            ..Self::default()
        }
    }
}

fn env_bytes(name: &str) -> Option<Vec<u8>> {
    std::env::var_os(name).map(|value| os_bytes(&value))
}

// The prompt reports paths and users, and both are byte strings on Unix and
// UTF-16 on Windows. These three helpers are the only platform-specific code in
// this file (`baash-lang` deliberately does not depend on `baash-sys`).

/// A shell byte string as a path.
#[cfg(unix)]
fn os_path(bytes: &[u8]) -> std::path::PathBuf {
    use std::os::unix::ffi::OsStrExt;
    std::path::PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

/// A shell byte string as a path.
#[cfg(windows)]
fn os_path(bytes: &[u8]) -> std::path::PathBuf {
    std::path::PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

/// An OS string as shell bytes.
#[cfg(unix)]
fn os_bytes(value: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

/// An OS string as shell bytes.
#[cfg(windows)]
fn os_bytes(value: &std::ffi::OsStr) -> Vec<u8> {
    value.to_string_lossy().into_owned().into_bytes()
}

/// This process's effective uid, where the platform has one.
#[cfg(unix)]
fn own_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self")
        .ok()
        .map(|metadata| metadata.uid())
}

/// This process's effective uid; Windows has none.
#[cfg(windows)]
fn own_uid() -> Option<u32> {
    None
}

/// The effective user's login name.
///
/// bash reads it from `getpwuid`, not from `$USER`, and a login shell's
/// environment may have neither `USER` nor `LOGNAME`; `/etc/passwd` is the
/// same source `getpwuid` reads, keyed by the effective uid `/proc/self`
/// reports. The environment is the fallback for a container whose uid has no
/// passwd entry.
fn current_user() -> Vec<u8> {
    if let Some(uid) = own_uid()
        && let Some(name) = passwd_name(uid)
    {
        return name;
    }
    env_bytes("USER")
        .or_else(|| env_bytes("LOGNAME"))
        .unwrap_or_default()
}

fn passwd_name(uid: u32) -> Option<Vec<u8>> {
    let wanted = uid.to_string();
    let data = std::fs::read("/etc/passwd").ok()?;
    for line in data.split(|&byte| byte == b'\n') {
        let mut fields = line.split(|&byte| byte == b':');
        let (Some(name), Some(_password), Some(id)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if id == wanted.as_bytes() {
            return Some(name.to_vec());
        }
    }
    None
}

fn read_hostname() -> Option<Vec<u8>> {
    let host = std::fs::read("/proc/sys/kernel/hostname").ok()?;
    let trimmed = host.trim_ascii();
    (!trimmed.is_empty()).then(|| trimmed.to_vec())
}

/// Whether this process is running as root, as far as we can tell.
///
/// `/proc/<pid>` is owned by the process's effective uid on Linux; reading the
/// owner is a `stat` the standard library can do without `libc`.
pub fn is_root() -> bool {
    if let Some(uid) = own_uid() {
        return uid == 0;
    }
    env_bytes("USER").is_some_and(|user| user == b"root")
}

// ---------------------------------------------------------------------------
// Expansion
// ---------------------------------------------------------------------------

/// A decoded prompt: bytes plus the stretches that must not count as width.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExpandedPrompt {
    bytes: Vec<u8>,
    non_printing: Vec<Span>,
}

impl ExpandedPrompt {
    /// The prompt bytes, with `\[` and `\]` removed.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consume the value and take its bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// A prompt with no non-printing stretches.
    ///
    /// For a caller that has plain text to paint — the editor's
    /// `(reverse-i-search)` prompt, for instance — and needs the same type the
    /// renderer takes.
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            bytes: bytes.into(),
            non_printing: Vec::new(),
        }
    }

    /// Byte ranges of [`ExpandedPrompt::bytes`] that came from between `\[`
    /// and `\]` and must not count toward the display width.
    ///
    /// The ranges are in output order and do not overlap. An unmatched `\[`
    /// yields one range from the opening to the end of the bytes.
    pub fn non_printing(&self) -> &[Span] {
        &self.non_printing
    }

    /// Whether a byte offset is inside a non-printing stretch.
    pub fn is_non_printing(&self, offset: usize) -> bool {
        self.non_printing
            .iter()
            .any(|range| range.contains(&offset))
    }
}

/// Expand the backslash escapes in `template` without a second expansion.
///
/// This is the rendering the line editor uses and the first pass of
/// `${x@P}`. Inserted values are not double-quote escaped, and `\$` is the
/// final prompt character.
pub fn expand(template: &[u8], ctx: &PromptContext) -> ExpandedPrompt {
    decode(template, ctx, false)
}

/// Expand `template` the way a shell draws a prompt: decode the escapes, then
/// expand the result as if it were double-quoted when `promptvars` is on.
///
/// This is the form both callers want. `shopt promptvars` (bash's default, and
/// [`PromptContext::prompt_vars`] here) selects the second pass; with it off
/// the value is [`expand`]'s single pass, which is what `${x@P}` prints when
/// the shopt is unset. Values are double-quote escaped during the first pass
/// when the second pass will run, so a working directory containing `$` or a
/// backslash survives unchanged.
///
/// The second pass applies double-quote backslash removal, expands parameter,
/// arithmetic and command substitutions, and leaves forms it does not
/// implement (`${x:-word}`) exactly as written rather than guessing. The
/// non-printing stretches are recomputed over the *output*, so a prompt with a
/// coloured `\[`…`\]` prefix that also contains a variable keeps its width
/// bookkeeping.
pub fn expand_with_variables<S: Shell>(
    template: &[u8],
    ctx: &PromptContext,
    shell: &mut S,
) -> ExpandedPrompt {
    let decoded = decode(template, ctx, ctx.prompt_vars);
    if !ctx.prompt_vars {
        return decoded;
    }
    second_pass(&decoded, shell)
}

/// Run the double-quoted expansion over each stretch of `decoded` separately,
/// rebuilding the non-printing ranges around the results.
fn second_pass<S: Shell>(decoded: &ExpandedPrompt, shell: &mut S) -> ExpandedPrompt {
    let mut out = ExpandedPrompt::default();
    let mut at = 0;
    for hidden in decoded.non_printing() {
        if hidden.start > at {
            out.bytes.extend(expand_double_quoted(
                &decoded.bytes[at..hidden.start],
                shell,
            ));
        }
        let start = out.bytes.len();
        out.bytes
            .extend(expand_double_quoted(&decoded.bytes[hidden.clone()], shell));
        out.non_printing.push(start..out.bytes.len());
        at = hidden.end.max(at);
    }
    if at < decoded.bytes.len() {
        out.bytes
            .extend(expand_double_quoted(&decoded.bytes[at..], shell));
    }
    out
}

fn decode(template: &[u8], ctx: &PromptContext, quote: bool) -> ExpandedPrompt {
    let mut out = ExpandedPrompt::default();
    let mut hidden_from: Option<usize> = None;
    let mut at = 0;
    while at < template.len() {
        if template[at] != b'\\' {
            out.bytes.push(template[at]);
            at += 1;
            continue;
        }
        let Some(&escape) = template.get(at + 1) else {
            // A trailing lone backslash is one literal backslash.
            out.bytes.push(b'\\');
            break;
        };
        at += 2;
        match escape {
            b'0'..=b'7' => {
                // bash copies three bytes and rejects the escape when any of
                // them is not octal; the value is then truncated to a byte.
                let window = &template[at - 1..(at + 2).min(template.len())];
                let valid =
                    !window.is_empty() && window.iter().all(|&byte| (b'0'..=b'7').contains(&byte));
                if valid {
                    let mut value: u32 = 0;
                    for &digit in window {
                        value = value * 8 + u32::from(digit - b'0');
                    }
                    let byte = value & 0xff;
                    // A NUL ends the C string bash builds, so it contributes
                    // no bytes to the prompt.
                    if byte != 0 {
                        out.bytes.push(u8::try_from(byte).unwrap_or(0));
                    }
                    at += window.len() - 1;
                } else {
                    // bash leaves the string pointer on the byte after the
                    // backslash when the octal number is bad, so the digits
                    // are copied literally: `\08` is `\08`, not `\8`.
                    out.bytes.push(b'\\');
                    at -= 1;
                }
            }
            b'a' => out.bytes.push(0x07),
            b'e' => out.bytes.push(0x1b),
            b'r' => out.bytes.push(b'\r'),
            b'n' => out.bytes.push(b'\n'),
            b'\\' => out.bytes.push(b'\\'),
            b'$' => {
                if ctx.root {
                    out.bytes.push(b'#');
                } else if quote {
                    // The second pass removes the backslash.
                    out.bytes.extend_from_slice(b"\\$");
                } else {
                    out.bytes.push(b'$');
                }
            }
            b'u' => push_value(&mut out.bytes, &ctx.user, quote),
            b'h' => push_value(&mut out.bytes, short_host(&ctx.host), quote),
            b'H' => push_value(&mut out.bytes, &ctx.host, quote),
            b'w' => {
                let expanded = abbreviate_home(&ctx.cwd, &ctx.home);
                push_value(&mut out.bytes, &expanded, quote);
            }
            b'W' => {
                let expanded = last_component_for_wide(&ctx.cwd, &ctx.home);
                push_value(&mut out.bytes, expanded, quote);
            }
            b's' => push_value(&mut out.bytes, &ctx.shell, quote),
            b'v' => push_value(&mut out.bytes, &ctx.version, quote),
            b'V' => push_value(&mut out.bytes, &ctx.release, quote),
            b'!' => push_value(
                &mut out.bytes,
                ctx.history_number.to_string().as_bytes(),
                quote,
            ),
            b'#' => push_value(
                &mut out.bytes,
                ctx.command_number.to_string().as_bytes(),
                quote,
            ),
            b'j' => push_value(&mut out.bytes, ctx.job_count.to_string().as_bytes(), quote),
            b'l' => push_value(&mut out.bytes, &ctx.tty, quote),
            b'd' | b't' | b'T' | b'@' | b'A' => {
                let format: &[u8] = match escape {
                    b'd' => b"%a %b %d",
                    b't' => b"%H:%M:%S",
                    b'T' => b"%I:%M:%S",
                    b'@' => b"%I:%M %p",
                    _ => b"%H:%M",
                };
                let formatted = ctx.time.format(format);
                push_value(&mut out.bytes, &formatted, quote);
            }
            b'D' => {
                if template.get(at) == Some(&b'{') {
                    let start = at + 1;
                    let end = template[start..]
                        .iter()
                        .position(|&byte| byte == b'}')
                        .map(|offset| start + offset);
                    match end {
                        Some(end) => {
                            let formatted = ctx.time.format(&template[start..end]);
                            push_value(&mut out.bytes, &formatted, quote);
                            at = end + 1;
                        }
                        None => {
                            out.bytes.extend_from_slice(b"\\D");
                        }
                    }
                } else {
                    out.bytes.extend_from_slice(b"\\D");
                }
            }
            b'[' => {
                // `\[` hides what follows; a second one while hidden does not
                // open a nested stretch (readline's marker is a flag, not a
                // stack).
                if hidden_from.is_none() {
                    hidden_from = Some(out.bytes.len());
                }
            }
            b']' => {
                if let Some(start) = hidden_from.take() {
                    out.non_printing.push(start..out.bytes.len());
                }
            }
            other => {
                // Unknown escapes keep their backslash.
                out.bytes.push(b'\\');
                out.bytes.push(other);
            }
        }
    }
    if let Some(start) = hidden_from {
        // An unmatched `\[` hides the rest of the string. The empty range is
        // recorded too, so a caller asking at the end boundary gets a
        // consistent answer.
        out.non_printing.push(start..out.bytes.len());
    }
    out
}

/// Append `value`, escaping the characters that are special inside double
/// quotes when the caller will run a second expansion.
fn push_value(out: &mut Vec<u8>, value: &[u8], quote: bool) {
    if !quote {
        out.extend_from_slice(value);
        return;
    }
    for &byte in value {
        if matches!(byte, b'\\' | b'"' | b'$' | b'`') {
            out.push(b'\\');
        }
        out.push(byte);
    }
}

fn short_host(host: &[u8]) -> &[u8] {
    match host.iter().position(|&byte| byte == b'.') {
        Some(dot) => &host[..dot],
        None => host,
    }
}

// ---------------------------------------------------------------------------
// The default prompt
// ---------------------------------------------------------------------------

/// The fish-like prompt an interactive shell draws when `PS1` is untouched.
///
/// The shell's default `PS1` stays bash's `\s-\v\$ ` (the conformance suite
/// reads it, and `${PS1@P}` decodes it), but the *rendered* prompt of an
/// unmodified `PS1` is this instead:
///
/// ```text
/// ~/src/bshell main !4 ?3
/// ```
///
/// in this order, with a component's leading space omitted together with the
/// component:
///
/// * `cwd`, with a leading `$HOME` abbreviated to `~.  The bytes are kept as
///   bytes: a path that is not UTF-8 renders exactly, and nothing here is
///   expanded a second time, so a `$` or `\` in a directory name stays literal.
/// * the git branch from [`git_branch`], plain and unbracketed.
/// * `!N` for `job_count` background children, only when `N > 0`.
/// * `?N` for a non-zero `last_status`.
///
/// The result ends with one space, so typing starts after the prompt rather
/// than against it. It is built directly rather than through `PS1` escapes:
/// the paths and branch names are data, and the escape decoder (or the
/// `promptvars` second pass) would reinterpret them.
pub fn fish_prompt(
    cwd: &[u8],
    home: &[u8],
    branch: Option<&[u8]>,
    job_count: u32,
    last_status: i32,
) -> ExpandedPrompt {
    let mut bytes = abbreviate_home(cwd, home);
    if let Some(branch) = branch {
        bytes.push(b' ');
        bytes.extend_from_slice(branch);
    }
    if job_count > 0 {
        bytes.extend_from_slice(format!(" !{job_count}").as_bytes());
    }
    if last_status != 0 {
        bytes.extend_from_slice(format!(" ?{last_status}").as_bytes());
    }
    bytes.push(b' ');
    ExpandedPrompt::from_bytes(bytes)
}

/// The branch the repository containing `cwd` has checked out, if any.
///
/// This is the default prompt's git segment and it never forks: walking up to
/// find `.git` is `stat`, and the branch is a small file read, which is why the
/// prompt can be exact on every line (`docs/prompt-watch.md` §2). A `.git`
/// *file* is followed, because that is how a linked worktree and a submodule
/// point at their real repository.
///
/// A symbolic `HEAD` outside `refs/heads/` is resolved through the loose ref,
/// the worktree's `commondir`, and `packed-refs`, in that order; whatever it
/// resolves to is shown as an abbreviated commit id. A detached `HEAD` is a
/// raw object id and is abbreviated directly. Nothing in a file we cannot read
/// is guessed at: the answer is `None`, and the prompt omits the branch.
pub fn git_branch(cwd: &[u8]) -> Option<Vec<u8>> {
    let git_dir = find_git_dir(&os_path(cwd))?;
    let head = read_trimmed(&git_dir.join("HEAD"))?;
    if let Some(name) = head.strip_prefix(b"ref: refs/heads/") {
        return (!name.is_empty()).then(|| name.to_vec());
    }
    if let Some(reference) = head.strip_prefix(b"ref: ") {
        return resolve_ref(&git_dir, reference).map(|commit| abbreviate_commit(&commit));
    }
    is_object_id(&head).then(|| abbreviate_commit(&head))
}

/// Walk up from `start` to the repository root, following a `.git` file.
fn find_git_dir(start: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut at = Some(start);
    while let Some(directory) = at {
        let dot_git = directory.join(".git");
        if dot_git.is_dir() {
            return Some(dot_git);
        }
        if dot_git.is_file()
            && let Some(target) = gitdir_pointer(&dot_git)
            && target.is_dir()
        {
            return Some(target);
        }
        at = directory.parent();
    }
    None
}

/// The directory a `.git` file names: `gitdir: <path>`, relative to the file.
fn gitdir_pointer(dot_git: &std::path::Path) -> Option<std::path::PathBuf> {
    let contents = std::fs::read(dot_git).ok()?;
    let contents = trim_ascii_end(&contents);
    let target = contents.strip_prefix(b"gitdir:")?.trim_ascii_start();
    if target.is_empty() {
        return None;
    }
    let target = os_path(target);
    if target.is_absolute() {
        Some(target)
    } else {
        dot_git.parent().map(|parent| parent.join(target))
    }
}

/// The object id a symbolic ref names.
fn resolve_ref(git_dir: &std::path::Path, reference: &[u8]) -> Option<Vec<u8>> {
    if let Some(commit) = read_loose_ref(git_dir, reference) {
        return Some(commit);
    }
    let common = common_dir(git_dir);
    if common != git_dir
        && let Some(commit) = read_loose_ref(&common, reference)
    {
        return Some(commit);
    }
    for directory in [git_dir, common.as_path()] {
        if let Some(commit) = read_packed_ref(directory, reference) {
            return Some(commit);
        }
    }
    None
}

/// `<git_dir>/<ref>`, when the ref is stored loose.
fn read_loose_ref(git_dir: &std::path::Path, reference: &[u8]) -> Option<Vec<u8>> {
    let relative = os_path(reference);
    if relative.is_absolute() {
        return None;
    }
    let contents = read_trimmed(&git_dir.join(relative))?;
    is_object_id(&contents).then_some(contents)
}

/// A linked worktree's shared git directory, from `commondir`.
fn common_dir(git_dir: &std::path::Path) -> std::path::PathBuf {
    let Some(contents) = read_trimmed(&git_dir.join("commondir")) else {
        return git_dir.to_path_buf();
    };
    if contents.is_empty() {
        return git_dir.to_path_buf();
    }
    let path = os_path(&contents);
    if path.is_absolute() {
        path
    } else {
        git_dir.join(path)
    }
}

/// A ref stored in `packed-refs`, by exact name.
fn read_packed_ref(git_dir: &std::path::Path, reference: &[u8]) -> Option<Vec<u8>> {
    let contents = std::fs::read(git_dir.join("packed-refs")).ok()?;
    for line in contents.split(|&byte| byte == b'\n') {
        if line.is_empty() || line[0] == b'#' || line[0] == b'^' {
            continue;
        }
        let Some(space) = line.iter().position(|&byte| byte == b' ') else {
            continue;
        };
        if line[space + 1..].trim_ascii() == reference {
            let commit = trim_ascii_end(&line[..space]);
            if is_object_id(commit) {
                return Some(commit.to_vec());
            }
        }
    }
    None
}

/// A whole file read and stripped of the newline the ref formats end with.
fn read_trimmed(path: &std::path::Path) -> Option<Vec<u8>> {
    let contents = std::fs::read(path).ok()?;
    let contents = trim_ascii_end(&contents);
    (!contents.is_empty()).then(|| contents.to_vec())
}

fn trim_ascii_end(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    while end > 0 && matches!(bytes[end - 1], b'\n' | b'\r') {
        end -= 1;
    }
    &bytes[..end]
}

/// Whether `bytes` is a complete object id (SHA-1, or SHA-256 in a newer repo).
fn is_object_id(bytes: &[u8]) -> bool {
    matches!(bytes.len(), 40 | 64) && bytes.iter().all(u8::is_ascii_hexdigit)
}

/// The short form git prints by default.
fn abbreviate_commit(commit: &[u8]) -> Vec<u8> {
    commit[..commit.len().min(7)].to_vec()
}

fn abbreviate_home(cwd: &[u8], home: &[u8]) -> Vec<u8> {
    if home.len() <= 1 || !cwd.starts_with(home) {
        return cwd.to_vec();
    }
    let rest = &cwd[home.len()..];
    if rest.is_empty() {
        return b"~".to_vec();
    }
    if rest[0] == b'/' {
        let mut out = Vec::with_capacity(rest.len() + 1);
        out.push(b'~');
        out.extend_from_slice(rest);
        return out;
    }
    cwd.to_vec()
}

/// `\W`: bash abbreviates a working directory that *is* `$HOME` to `~` before
/// considering the basename, so `~` is the answer there too.
fn last_component_for_wide<'a>(cwd: &'a [u8], home: &[u8]) -> &'a [u8] {
    if !home.is_empty() && cwd == home {
        return b"~";
    }
    match cwd.iter().rposition(|&byte| byte == b'/') {
        Some(0) => &cwd[..1],
        Some(slash) => &cwd[slash + 1..],
        None => cwd,
    }
}

// ---------------------------------------------------------------------------
// The second pass
// ---------------------------------------------------------------------------

/// Expand parameter, arithmetic and command substitutions as if the text were
/// inside double quotes, removing double-quote backslash escapes.
///
/// This is not the word pipeline and does not pretend to be: it sees bytes
/// that a prompt template produced, where the only live syntax is `$…` and
/// `` `…` ``, and it leaves every construct it does not implement exactly as
/// written. The full pipeline needs a parser, which is the front end's job; a
/// prompt that contains an unsupported form renders it literally rather than
/// silently dropping it.
fn expand_double_quoted<S: Shell>(text: &[u8], shell: &mut S) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut at = 0;
    while at < text.len() {
        match text[at] {
            b'\\' => {
                let Some(&next) = text.get(at + 1) else {
                    out.push(b'\\');
                    break;
                };
                match next {
                    b'$' | b'`' | b'"' | b'\\' => {
                        out.push(next);
                        at += 2;
                    }
                    b'\n' => at += 2,
                    _ => {
                        out.push(b'\\');
                        at += 1;
                    }
                }
            }
            b'$' => {
                let (bytes, next) = expand_dollar(text, at, shell);
                out.extend_from_slice(&bytes);
                at = next;
            }
            b'`' => {
                let (bytes, next) = expand_backtick(text, at, shell);
                out.extend_from_slice(&bytes);
                at = next;
            }
            byte => {
                out.push(byte);
                at += 1;
            }
        }
    }
    out
}

/// Expand the `$…` construct at `at`, returning its bytes and the offset after
/// it. Constructs this pass does not implement are copied through verbatim.
fn expand_dollar<S: Shell>(text: &[u8], at: usize, shell: &mut S) -> (Vec<u8>, usize) {
    let after = at + 1;
    let Some(&first) = text.get(after) else {
        return (vec![b'$'], after);
    };
    if first == b'{' {
        let Some(close) = matching_brace(text, after) else {
            return (vec![b'$'], after);
        };
        let body = &text[after + 1..close];
        let value = simple_parameter(body, shell);
        return match value {
            Some(bytes) => (bytes, close + 1),
            None => (text[at..=close].to_vec(), close + 1),
        };
    }
    if first == b'(' {
        // `$((…))` is arithmetic; `$(…)` is a command substitution. Both are
        // executed: bash runs them while expanding a prompt, and a prompt that
        // says `$(date)` means it.
        if text.get(after + 1) == Some(&b'(') {
            return match arithmetic_close(text, after) {
                Some((start, end, next)) => {
                    match shell.arithmetic(&crate::word::Arithmetic::new(&text[start..end])) {
                        // bash prints the error and leaves the unexpanded text.
                        Ok(bytes) => (bytes, next),
                        Err(_) => (text[at..next].to_vec(), next),
                    }
                }
                // No closing `))`: what bash leaves behind is not defined
                // enough to guess, so the text stays as written.
                None => (text[at..].to_vec(), text.len()),
            };
        }
        return match matching_paren(text, after) {
            Some(close) => {
                let source = &text[after + 1..close];
                match shell.command_substitution(&crate::word::CommandSub::dollar_paren(source)) {
                    Ok(bytes) => (bytes, close + 1),
                    // bash prints the substitution's error and substitutes
                    // nothing; the status is not the prompt's.
                    Err(_) => (Vec::new(), close + 1),
                }
            }
            None => (text[at..].to_vec(), text.len()),
        };
    }
    if is_name_start(first) {
        let mut end = after + 1;
        while text.get(end).is_some_and(|&byte| is_name(byte)) {
            end += 1;
        }
        let value = shell
            .get(&text[after..end])
            .map(<[u8]>::to_vec)
            .unwrap_or_default();
        return (value, end);
    }
    if first.is_ascii_digit() {
        let value = if first == b'0' {
            shell.get(b"0").map(<[u8]>::to_vec).unwrap_or_default()
        } else {
            shell
                .positional()
                .get(usize::from(first - b'1'))
                .cloned()
                .unwrap_or_default()
        };
        return (value, after + 1);
    }
    match first {
        b'?' | b'-' | b'!' | b'$' => {
            let value = shell.get(&[first]).map(<[u8]>::to_vec).unwrap_or_default();
            (value, after + 1)
        }
        b'#' => (shell.positional().len().to_string().into_bytes(), after + 1),
        b'@' | b'*' => {
            let separator = if first == b'*' {
                shell
                    .get(b"IFS")
                    .and_then(<[u8]>::first)
                    .copied()
                    .unwrap_or(b' ')
            } else {
                b' '
            };
            let mut joined = Vec::new();
            for (index, value) in shell.positional().iter().enumerate() {
                if index > 0 {
                    joined.push(separator);
                }
                joined.extend_from_slice(value);
            }
            (joined, after + 1)
        }
        _ => (vec![b'$'], after),
    }
}

/// Expand the `` `…` `` substitution at `at`.
///
/// An unterminated backtick is copied through, like every other construct this
/// pass cannot delimit.
fn expand_backtick<S: Shell>(text: &[u8], at: usize, shell: &mut S) -> (Vec<u8>, usize) {
    let Some((source, next)) = backtick_body(text, at) else {
        return (vec![b'`'], at + 1);
    };
    match shell.command_substitution(&crate::word::CommandSub::backtick(source)) {
        Ok(bytes) => (bytes, next),
        Err(_) => (Vec::new(), next),
    }
}

/// The command text of the backtick substitution starting at `at`, and the
/// offset after the closing backtick.
///
/// Inside backticks, the documented escapes are `\``, `\\`, `\$` and a
/// backslash before a newline; the text handed to the runtime is
/// unescaped, which is the contract [`crate::word::CommandSub`] documents for
/// the backtick form.
fn backtick_body(text: &[u8], at: usize) -> Option<(Vec<u8>, usize)> {
    let mut source = Vec::new();
    let mut index = at + 1;
    while index < text.len() {
        match text[index] {
            b'`' => return Some((source, index + 1)),
            b'\\' => {
                let next = *text.get(index + 1)?;
                match next {
                    b'`' | b'\\' | b'$' => {
                        source.push(next);
                        index += 2;
                    }
                    b'\n' => index += 2,
                    _ => {
                        source.push(b'\\');
                        index += 1;
                    }
                }
            }
            byte => {
                source.push(byte);
                index += 1;
            }
        }
    }
    None
}

/// The body of `$((…))` starting at the `(` at `open`, as
/// `(expression start, expression end, offset after)`.
///
/// The second `(` opens a parenthesis group inside the expression, so its
/// depth starts at one and the expression ends at the `)` that closes it —
/// the first byte of the `))`.
fn arithmetic_close(text: &[u8], open: usize) -> Option<(usize, usize, usize)> {
    let start = open + 2;
    let mut depth = 1usize;
    let mut at = start;
    while at < text.len() {
        match text[at] {
            b'\\' => at += 1,
            b'(' => depth += 1,
            b')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    if text.get(at + 1) == Some(&b')') {
                        return Some((start, at, at + 2));
                    }
                    return None;
                }
            }
            _ => {}
        }
        at += 1;
    }
    None
}

/// `${body}` when `body` is a plain name, a special parameter or `#name`;
/// `None` means the form has an operator this pass does not implement.
fn simple_parameter<S: Shell>(body: &[u8], shell: &mut S) -> Option<Vec<u8>> {
    let (length, body) = match body.split_first() {
        Some((&b'#', rest)) if !rest.is_empty() => (true, rest),
        _ => (false, body),
    };
    let value = match body {
        [] => return None,
        [special @ (b'?' | b'-' | b'$' | b'!' | b'0')] => shell
            .get(&[*special])
            .map(<[u8]>::to_vec)
            .unwrap_or_default(),
        [b'#'] if !length => shell.positional().len().to_string().into_bytes(),
        [b'@'] | [b'*'] => {
            let mut joined = Vec::new();
            for (index, value) in shell.positional().iter().enumerate() {
                if index > 0 {
                    joined.push(b' ');
                }
                joined.extend_from_slice(value);
            }
            joined
        }
        digits if digits.iter().all(u8::is_ascii_digit) => {
            match std::str::from_utf8(digits)
                .ok()
                .and_then(|text| text.parse::<usize>().ok())
            {
                Some(0) => shell.get(b"0").map(<[u8]>::to_vec).unwrap_or_default(),
                Some(index) => shell
                    .positional()
                    .get(index - 1)
                    .cloned()
                    .unwrap_or_default(),
                None => return None,
            }
        }
        name if is_name_start(name[0]) && name.iter().all(|&byte| is_name(byte)) => {
            shell.get(name).map(<[u8]>::to_vec).unwrap_or_default()
        }
        _ => return None,
    };
    Some(if length {
        value.len().to_string().into_bytes()
    } else {
        value
    })
}

fn matching_brace(text: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut at = open;
    while at < text.len() {
        match text[at] {
            b'\\' => at += 1,
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
        at += 1;
    }
    None
}

fn matching_paren(text: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut at = open;
    while at < text.len() {
        match text[at] {
            b'\\' => at += 1,
            b'(' => depth += 1,
            b')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
        at += 1;
    }
    None
}

fn is_name_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_name(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;
    use crate::fixture::TestShell;

    fn context() -> PromptContext {
        PromptContext {
            user: b"ada".to_vec(),
            host: b"lab.example.com".to_vec(),
            cwd: b"/home/ada/work".to_vec(),
            previous_cwd: b"/home/ada".to_vec(),
            home: b"/home/ada".to_vec(),
            shell: b"baash".to_vec(),
            version: b"1.2".to_vec(),
            release: b"1.2.3".to_vec(),
            history_number: 42,
            command_number: 7,
            job_count: 3,
            root: false,
            last_failed: false,
            columns: 80,
            tty: b"pts/3".to_vec(),
            time: PromptTime {
                year: 2026,
                month: 3,
                day: 9,
                hour: 14,
                minute: 5,
                second: 7,
                weekday: 1,
                yearday: 68,
                epoch: 1_773_065_107,
                utc_offset: 3_600,
                is_dst: false,
            },
            prompt_vars: true,
        }
    }

    fn rendered(template: &[u8]) -> Vec<u8> {
        expand(template, &context()).into_bytes()
    }

    /// Rendering with the two-pass `${x@P}` path.
    fn transformed(template: &[u8], shell: &mut TestShell) -> Vec<u8> {
        let context = context();
        expand_with_variables(template, &context, shell).into_bytes()
    }

    #[test]
    fn control_escapes() {
        assert_eq!(
            rendered(b"a\\a\\e\\r\\n\\\\b"),
            b"a\x07\x1b\r\n\\b".to_vec()
        );
    }

    #[test]
    fn time_and_date_escapes() {
        assert_eq!(rendered(b"\\d"), b"Mon Mar 09");
        assert_eq!(rendered(b"\\t"), b"14:05:07");
        assert_eq!(rendered(b"\\T"), b"02:05:07");
        assert_eq!(rendered(b"\\@"), b"02:05 PM");
        assert_eq!(rendered(b"\\A"), b"14:05");
        assert_eq!(rendered(b"\\D{%H:%M:%S}"), b"14:05:07");
        // An empty format is bash's `%X`, the locale time.
        assert_eq!(rendered(b"\\D{}"), b"14:05:07");
        assert_eq!(rendered(b"\\D{%Y-%m-%d %I %p %j}"), b"2026-03-09 02 PM 068");
        // A `\D` without a braced format is not an escape.
        assert_eq!(rendered(b"\\D"), b"\\D");
        assert_eq!(rendered(b"\\D%H"), b"\\D%H");
    }

    #[test]
    fn strftime_subset() {
        let time = context().time;
        let format = |text: &[u8]| time.format(text);
        assert_eq!(format(b"%a %A"), b"Mon Monday");
        assert_eq!(format(b"%b %B"), b"Mar March");
        assert_eq!(format(b"%e"), b" 9");
        assert_eq!(format(b"%d"), b"09");
        assert_eq!(format(b"%m/%d/%y"), b"03/09/26");
        assert_eq!(format(b"%F %T %R"), b"2026-03-09 14:05:07 14:05");
        assert_eq!(format(b"%r"), b"02:05:07 PM");
        assert_eq!(format(b"%C"), b"20");
        assert_eq!(format(b"%s"), b"1773065107");
        assert_eq!(format(b"%z"), b"+0100");
        assert_eq!(format(b"%n%t%%"), b"\n\t%");
        assert_eq!(format(b"%q"), b"%q");
        assert_eq!(format(b"%"), b"%");
    }

    #[test]
    fn identity_escapes() {
        assert_eq!(rendered(b"\\u"), b"ada");
        assert_eq!(rendered(b"\\h"), b"lab");
        assert_eq!(rendered(b"\\H"), b"lab.example.com");
        assert_eq!(rendered(b"\\s"), b"baash");
        assert_eq!(rendered(b"\\v"), b"1.2");
        assert_eq!(rendered(b"\\V"), b"1.2.3");
        assert_eq!(rendered(b"\\!"), b"42");
        assert_eq!(rendered(b"\\#"), b"7");
        assert_eq!(rendered(b"\\j"), b"3");
        assert_eq!(rendered(b"\\l"), b"pts/3");
        assert_eq!(rendered(b"\\$"), b"$");

        let mut root = context();
        root.root = true;
        assert_eq!(expand(b"\\$", &root).into_bytes(), b"#");
    }

    #[test]
    fn working_directory_abbreviation() {
        assert_eq!(rendered(b"\\w"), b"~/work");
        assert_eq!(rendered(b"\\W"), b"work");

        let mut ctx = context();
        ctx.cwd = b"/home/ada".to_vec();
        assert_eq!(expand(b"\\w", &ctx).into_bytes(), b"~");
        // `\W` abbreviates before taking the basename, so home is `~` too.
        assert_eq!(expand(b"\\W", &ctx).into_bytes(), b"~");

        let mut ctx = context();
        ctx.cwd = b"/home/adam".to_vec();
        assert_eq!(expand(b"\\w", &ctx).into_bytes(), b"/home/adam");

        let mut ctx = context();
        ctx.cwd = b"/".to_vec();
        assert_eq!(expand(b"\\w", &ctx).into_bytes(), b"/");
        assert_eq!(expand(b"\\W", &ctx).into_bytes(), b"/");

        // A home of `/` is not abbreviated (bash requires length > 1).
        let mut ctx = context();
        ctx.home = b"/".to_vec();
        ctx.cwd = b"/work".to_vec();
        assert_eq!(expand(b"\\w", &ctx).into_bytes(), b"/work");

        // A space is an ordinary byte in the path, in both forms.
        let mut ctx = context();
        ctx.cwd = b"/home/ada/my work".to_vec();
        assert_eq!(expand(b"\\w", &ctx).into_bytes(), b"~/my work");
        assert_eq!(expand(b"\\W", &ctx).into_bytes(), b"my work");
    }

    #[test]
    fn trailing_backslash_and_unknown_escapes() {
        assert_eq!(rendered(b"trailing\\"), b"trailing\\");
        assert_eq!(rendered(b"\\z"), b"\\z");
        assert_eq!(rendered(b"\\x55"), b"\\x55");
        assert_eq!(rendered(b"[\\u0001]"), b"[ada0001]");
        assert_eq!(rendered(b"\\8"), b"\\8");
        assert_eq!(rendered(b"\\08"), b"\\08");
    }

    #[test]
    fn octal_escapes() {
        // Up to three digits, wrapping modulo 256; the escape fails as a whole
        // when the three-byte window contains a non-octal byte.
        assert_eq!(rendered(b"\\1004$"), b"@4$");
        assert_eq!(rendered(b"[\\045]"), b"[%]");
        assert_eq!(rendered(b"[\\0455]"), b"[%5]");
        assert_eq!(rendered(b"\\555$"), b"m$");
        assert_eq!(rendered(b"\\401"), vec![1]);
        assert_eq!(rendered(b"\\377"), vec![0xff]);
        assert_eq!(rendered(b"\\1"), vec![1]);
        assert_eq!(rendered(b"\\12"), vec![0x0a]);
        assert_eq!(rendered(b"\\123"), b"S");
        assert_eq!(rendered(b"\\1x"), b"\\1x");
        assert_eq!(rendered(b"\\12x"), b"\\12x");
        // A NUL would end bash's C string, so it contributes nothing.
        assert_eq!(rendered(b"\\0"), b"");
        assert_eq!(rendered(b"\\400"), b"");
        assert_eq!(rendered(b"\\0000"), b"0");
    }

    #[test]
    fn non_printing_stretches_are_removed_and_recorded() {
        let prompt = expand(b"\\[foo\\]\\$", &context());
        assert_eq!(prompt.bytes(), b"foo$");
        assert_eq!(prompt.non_printing(), &[0..3]);
        assert!(prompt.is_non_printing(0));
        assert!(prompt.is_non_printing(2));
        assert!(!prompt.is_non_printing(3));

        // A coloured prompt: the ANSI sequence is hidden, the rest is not.
        let prompt = expand(b"\\[\\e[31m\\]$\\[\\e[0m\\] ", &context());
        assert_eq!(prompt.bytes(), b"\x1b[31m$\x1b[0m ");
        assert_eq!(prompt.non_printing(), &[0..5, 6..10]);
    }

    #[test]
    fn stray_and_repeated_non_printing_marks() {
        // A stray end marker is just removed.
        let prompt = expand(b"a\\]b", &context());
        assert_eq!(prompt.bytes(), b"ab");
        assert!(prompt.non_printing().is_empty());

        // An unmatched start hides the rest of the string.
        let prompt = expand(b"\\[a", &context());
        assert_eq!(prompt.bytes(), b"a");
        assert_eq!(prompt.non_printing(), &[0..1]);

        // A second start while one is open does not nest; the first end
        // closes the stretch.
        let prompt = expand(b"\\[a\\[b\\]c\\]d", &context());
        assert_eq!(prompt.bytes(), b"abcd");
        assert_eq!(prompt.non_printing(), &[0..2]);

        // Repeated complete stretches stay separate.
        let prompt = expand(b"\\[a\\]b\\[c\\]d", &context());
        assert_eq!(prompt.bytes(), b"abcd");
        assert_eq!(prompt.non_printing(), &[0..1, 2..3]);

        // An empty stretch is recorded at its position.
        let prompt = expand(b"\\[\\]x", &context());
        assert_eq!(prompt.bytes(), b"x");
        assert_eq!(prompt.non_printing(), &[0..0]);
    }

    /// The five `$` combinations the conformance suite pins down. They are the
    /// visible consequence of `\$` writing a backslash that the second pass
    /// removes.
    #[test]
    fn dollar_special_cases_match_bash() {
        let cases: [(&[u8], &[u8]); 5] = [
            (b"$", b"$"),
            (b"\\$", b"$"),
            (b"\\\\$", b"$"),
            (b"\\\\\\$", b"\\$"),
            (b"\\\\\\\\$", b"\\$"),
        ];
        for (template, expected) in cases {
            let mut shell = TestShell::new();
            assert_eq!(
                transformed(template, &mut shell),
                expected,
                "PS1={:?}",
                String::from_utf8_lossy(template)
            );
        }
    }

    #[test]
    fn the_second_pass_expands_variables_but_does_not_redecode() {
        // `$x$y` expands to `\h`, and the inserted bytes are not prompt text.
        let mut shell = TestShell::new().var(b"x", b"\\").var(b"y", b"h");
        assert_eq!(transformed(b"$x$y", &mut shell), b"\\h");

        let mut shell = TestShell::new()
            .var(b"foo", b"foo_value")
            .var(b"USER", b"zed");
        assert_eq!(transformed(b"$foo", &mut shell), b"foo_value");
        assert_eq!(transformed(b"${USER}", &mut shell), b"zed");
        assert_eq!(transformed(b"${#USER}", &mut shell), b"3");
        assert_eq!(transformed(b"$$", &mut shell), b"");
        assert_eq!(transformed(b"a b", &mut shell), b"a b");
    }

    #[test]
    fn the_second_pass_removes_double_quote_backslashes() {
        let mut shell = TestShell::new();
        assert_eq!(transformed(b"\\$", &mut shell), b"$");
        assert_eq!(transformed(b"\\\\", &mut shell), b"\\");
        assert_eq!(transformed(b"\\x", &mut shell), b"\\x");
        assert_eq!(transformed(b"a\\\nb", &mut shell), b"ab");
    }

    #[test]
    fn inserted_values_are_quoted_for_the_second_pass() {
        // A `$` in the working directory must survive as a literal.
        let mut ctx = context();
        ctx.cwd = b"/tmp/$HOME\\x\"y".to_vec();
        let mut shell = TestShell::new();
        let rendered = expand_with_variables(b"\\w", &ctx, &mut shell);
        assert_eq!(rendered.into_bytes(), b"/tmp/$HOME\\x\"y");
    }

    #[test]
    fn prompt_vars_off_skips_quoting_and_the_second_pass() {
        let mut ctx = context();
        ctx.prompt_vars = false;
        ctx.cwd = b"/tmp/$HOME".to_vec();
        let mut shell = TestShell::new().var(b"HOME", b"expanded");
        assert_eq!(
            expand_with_variables(b"\\w \\$", &ctx, &mut shell).into_bytes(),
            b"/tmp/$HOME $"
        );
    }

    #[test]
    fn unsupported_second_pass_forms_are_copied_through() {
        let mut shell = TestShell::new().var(b"x", b"value");
        // Operators the minimal second pass does not implement stay visible
        // rather than being silently emptied.
        assert_eq!(transformed(b"${x:-d}", &mut shell), b"${x:-d}");
        assert_eq!(transformed(b"${a[0]}", &mut shell), b"${a[0]}");
    }

    #[test]
    fn the_second_pass_runs_command_substitution() {
        // bash runs `$(…)` while expanding a prompt (measured against
        // 5.3.20: `PS1='$(echo hi)'` shows `hi`), so the shared expander does
        // the same for `${x@P}` and for the line editor.
        let mut shell = TestShell::new().cmdsub(b"date +%H", b"14");
        assert_eq!(transformed(b"$(date +%H):$ ", &mut shell), b"14:$ ");
        assert_eq!(shell.cmdsub_calls, vec![b"date +%H".to_vec()]);

        // Backticks are the other spelling of the same construct.
        let mut shell = TestShell::new().cmdsub(b"echo bt", b"bt");
        assert_eq!(transformed(b"`echo bt`", &mut shell), b"bt");

        // A substitution that fails contributes nothing, like bash.
        let mut shell = TestShell::new();
        assert_eq!(transformed(b"a$(boom)b", &mut shell), b"ab");

        // An unterminated form has no delimiter to execute.
        assert_eq!(transformed(b"a$(boom", &mut shell), b"a$(boom");
        assert_eq!(transformed(b"a`boom", &mut shell), b"a`boom");
    }

    #[test]
    fn the_second_pass_evaluates_arithmetic() {
        let mut shell = TestShell::new().arith(b"1+2", b"3");
        assert_eq!(transformed(b"$((1+2)) $ ", &mut shell), b"3 $ ");
        // Nested parentheses belong to the expression, not to the delimiter.
        let mut shell = TestShell::new().arith(b"(1+2)*3", b"9");
        assert_eq!(transformed(b"$(((1+2)*3))", &mut shell), b"9");
        // A failing expression keeps its text, as bash does for `$((1/0))`.
        let mut shell = TestShell::new();
        assert_eq!(transformed(b"a$((1/0))b", &mut shell), b"a$((1/0))b");
    }

    #[test]
    fn escapes_use_real_history_and_job_numbers() {
        let mut ctx = context();
        ctx.history_number = 501;
        ctx.command_number = 12;
        ctx.job_count = 2;
        assert_eq!(expand(b"\\![\\#][\\j]", &ctx).into_bytes(), b"501[12][2]");
        // The numbers still expand when the second pass runs.
        let mut shell = TestShell::new();
        assert_eq!(
            expand_with_variables(b"\\![\\#][\\j]", &ctx, &mut shell).into_bytes(),
            b"501[12][2]"
        );
        // ... and when it does not.
        ctx.prompt_vars = false;
        let mut shell = TestShell::new();
        assert_eq!(
            expand_with_variables(b"\\![\\#][\\j]", &ctx, &mut shell).into_bytes(),
            b"501[12][2]"
        );
    }

    #[test]
    fn non_printing_spans_survive_the_second_pass() {
        // The most common real-world PS1: a coloured prefix with `\u@\h`, an
        // ordinary `\w\$`, all of it followed by promptvars expansion.
        let mut ctx = context();
        ctx.prompt_vars = true;
        let mut shell = TestShell::new();
        let prompt = expand_with_variables(
            b"\\[\\e[1;32m\\]\\u@\\h\\[\\e[0m\\] \\w\\$ ",
            &ctx,
            &mut shell,
        );
        assert_eq!(prompt.bytes(), b"\x1b[1;32mada@lab\x1b[0m ~/work$ ");
        assert_eq!(prompt.non_printing(), &[0..7, 14..18]);

        // The same template with a variable in it: the spans must still be
        // exactly the escape sequences, not the whole prompt.
        let mut shell = TestShell::new().var(b"TAG", b"T");
        let prompt = expand_with_variables(
            b"\\[\\e[1;32m\\]${TAG}\\u@\\h\\[\\e[0m\\] \\w\\$ ",
            &ctx,
            &mut shell,
        );
        assert_eq!(prompt.bytes(), b"\x1b[1;32mTada@lab\x1b[0m ~/work$ ");
        assert_eq!(prompt.non_printing(), &[0..7, 15..19]);
        assert!(prompt.is_non_printing(0));
        assert!(!prompt.is_non_printing(7));

        // A hidden stretch that contains a variable stays hidden.
        let mut shell = TestShell::new().var(b"COLOUR", b"\x1b[31m");
        let prompt = expand_with_variables(b"\\[$COLOUR\\]x", &ctx, &mut shell);
        assert_eq!(prompt.bytes(), b"\x1b[31mx");
        assert_eq!(prompt.non_printing(), &[0..5]);
    }

    #[test]
    fn prompt_vars_off_leaves_the_decoded_text_alone() {
        let mut ctx = context();
        ctx.prompt_vars = false;
        let mut shell = TestShell::new()
            .var(b"PWD", b"/tmp")
            .cmdsub(b"date", b"never");
        assert_eq!(
            expand_with_variables(b"$PWD $(date) \\! $ ", &ctx, &mut shell).into_bytes(),
            b"$PWD $(date) 42 $ "
        );
        assert!(shell.cmdsub_calls.is_empty(), "no command may run");
        // `expand` is the same single pass.
        assert_eq!(expand(b"$PWD \\! $ ", &ctx).into_bytes(), b"$PWD 42 $ ");
    }

    #[test]
    fn prompt_vars_on_expands_variables_and_commands() {
        let mut ctx = context();
        ctx.prompt_vars = true;
        let mut shell = TestShell::new()
            .var(b"PWD", b"/tmp")
            .cmdsub(b"date +%H:%M", b"14:05");
        assert_eq!(
            expand_with_variables(b"$PWD $(date +%H:%M) \\! $ ", &ctx, &mut shell).into_bytes(),
            b"/tmp 14:05 42 $ "
        );
    }

    #[test]
    fn local_time_conversion_is_exact_for_known_instants() {
        // 2026-03-09T14:05:07Z.
        let time = PromptTime::from_unix_utc(1_773_065_107);
        assert_eq!(time.year, 2026);
        assert_eq!(time.month, 3);
        assert_eq!(time.day, 9);
        assert_eq!(time.hour, 14);
        assert_eq!(time.minute, 5);
        assert_eq!(time.second, 7);
        assert_eq!(time.weekday, 1);
        assert_eq!(time.yearday, 68);

        let epoch = PromptTime::from_unix_utc(0);
        assert_eq!((epoch.year, epoch.month, epoch.day), (1970, 1, 1));
        assert_eq!(epoch.weekday, 4);
    }

    #[test]
    fn zone_names_resolve_through_tzdir() {
        // Guarded: a minimal container may ship no zoneinfo at all.
        if let Some(data) = zone_file(b"UTC") {
            assert_eq!(tzif_offset(&data, 0), Some((0, false)));
        }
        if let Some(data) = zone_file(b"Europe/Amsterdam") {
            // 2026-07-01T00:00:00Z is CEST, two hours ahead of UTC.
            assert_eq!(
                tzif_offset(&data, 1_782_864_000).map(|(offset, _)| offset),
                Some(7_200)
            );
            // ... and 2026-01-01T00:00:00Z is CET, one hour ahead.
            assert_eq!(
                tzif_offset(&data, 1_767_225_600).map(|(offset, _)| offset),
                Some(3_600)
            );
        }
    }

    #[test]
    fn tzif_transition_selection() {
        // A minimal TZif v1 block with two types: UTC and UTC+1 (DST).
        let mut data = Vec::new();
        data.extend_from_slice(b"TZif");
        data.push(b'1');
        data.extend_from_slice(&[0; 15]);
        // isutcnt, isstdcnt, leapcnt, timecnt, typecnt, charcnt.
        for count in [0u32, 0, 0, 2, 2, 4] {
            data.extend_from_slice(&count.to_be_bytes());
        }
        // Transitions at 1000 and 2000.
        data.extend_from_slice(&1000i32.to_be_bytes());
        data.extend_from_slice(&2000i32.to_be_bytes());
        // Type indices: 0 before 2000, 1 after.
        data.extend_from_slice(&[0, 1]);
        // Type 0: +0, standard. Type 1: +3600, DST.
        data.extend_from_slice(&0i32.to_be_bytes());
        data.extend_from_slice(&[0, 0]);
        data.extend_from_slice(&3600i32.to_be_bytes());
        data.extend_from_slice(&[1, 0]);
        data.extend_from_slice(b"UTC\0");

        assert_eq!(tzif_offset(&data, 500), Some((0, false)));
        assert_eq!(tzif_offset(&data, 1000), Some((0, false)));
        assert_eq!(tzif_offset(&data, 1999), Some((0, false)));
        assert_eq!(tzif_offset(&data, 2000), Some((3600, true)));
        assert_eq!(tzif_offset(&data, 10_000), Some((3600, true)));
    }

    // -----------------------------------------------------------------------
    // The default prompt and its git branch
    // -----------------------------------------------------------------------

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fresh directory under the system temporary directory.
    fn scratch(label: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "baash-prompt-{}-{}-{label}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create scratch directory");
        path
    }

    fn as_bytes(path: &std::path::Path) -> Vec<u8> {
        os_bytes(path.as_os_str())
    }

    fn render(cwd: &[u8], home: &[u8], branch: Option<&[u8]>, jobs: u32, status: i32) -> Vec<u8> {
        fish_prompt(cwd, home, branch, jobs, status).into_bytes()
    }

    #[test]
    fn the_default_prompt_abbreviates_home_and_orders_the_segments() {
        assert_eq!(
            render(b"/home/ada/src/bshell", b"/home/ada", Some(b"main"), 4, 3),
            b"~/src/bshell main !4 ?3 "
        );
        assert_eq!(
            render(b"/home/ada", b"/home/ada", Some(b"main"), 0, 0),
            b"~ main "
        );
    }

    #[test]
    fn a_path_outside_home_stays_absolute() {
        assert_eq!(
            render(b"/opt/bshell", b"/home/ada", None, 0, 0),
            b"/opt/bshell "
        );
        // A `$HOME` prefix that is not at a path boundary is not abbreviated.
        assert_eq!(
            render(b"/home/adam/x", b"/home/ada", None, 0, 0),
            b"/home/adam/x "
        );
    }

    #[test]
    fn counters_appear_only_when_non_zero() {
        // Each component carries its own space, so no component leaves a
        // double space behind.
        assert_eq!(render(b"/x", b"/h", None, 2, 0), b"/x !2 ");
        assert_eq!(render(b"/x", b"/h", None, 0, 1), b"/x ?1 ");
        assert_eq!(render(b"/x", b"/h", Some(b"dev"), 0, 0), b"/x dev ");
        assert_eq!(render(b"/x", b"/h", None, 0, 0), b"/x ");
    }

    #[test]
    fn a_non_utf8_path_is_byte_exact() {
        // Paths are bytes. Nothing in the default prompt decodes them, so an
        // undecodable byte survives exactly and cannot become U+FFFD.
        let cwd = b"/tmp/caf\xe9/\xff";
        assert_eq!(render(cwd, b"/tmp", None, 0, 0), b"~/caf\xe9/\xff ");
        assert_eq!(
            render(b"/home/ada/\xff", b"/home/ada", Some(b"ma\xefn"), 0, 0),
            b"~/\xff ma\xefn "
        );
    }

    #[test]
    fn branch_from_a_git_directory() {
        let root = scratch("git-dir");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        assert_eq!(git_branch(&as_bytes(&root)), Some(b"main".to_vec()));

        // A nested directory finds the repository above it.
        let nested = root.join("src/nested");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(git_branch(&as_bytes(&nested)), Some(b"main".to_vec()));
    }

    #[test]
    fn branch_from_a_worktree_git_file() {
        // A linked worktree has a `.git` *file* naming the real git directory,
        // which is where HEAD lives.
        let root = scratch("worktree");
        let git_dir = root.join("repo/.git/worktrees/wt");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), b"ref: refs/heads/feature/x\n").unwrap();
        let worktree = root.join("checkout");
        std::fs::create_dir_all(worktree.join("sub")).unwrap();
        std::fs::write(
            worktree.join(".git"),
            b"gitdir: ../repo/.git/worktrees/wt\n",
        )
        .unwrap();

        assert_eq!(
            git_branch(&as_bytes(&worktree.join("sub"))),
            Some(b"feature/x".to_vec())
        );
    }

    #[test]
    fn a_detached_head_is_the_abbreviated_commit() {
        let root = scratch("detached");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(
            root.join(".git/HEAD"),
            b"2f4a9c1d0e5b6a7c8d9e0f1a2b3c4d5e6f708192\n",
        )
        .unwrap();
        assert_eq!(
            git_branch(&as_bytes(&root)),
            Some(b"2f4a9c1".to_vec()),
            "a detached HEAD shows git's seven-character abbreviation"
        );
    }

    #[test]
    fn no_repository_has_no_branch() {
        let root = scratch("no-repo");
        assert_eq!(git_branch(&as_bytes(&root)), None);
        assert_eq!(render(&as_bytes(&root), b"/h", None, 0, 0), {
            let mut expected = as_bytes(&root);
            expected.push(b' ');
            expected
        });
    }

    #[test]
    fn a_symbolic_ref_outside_heads_resolves_through_packed_refs() {
        // `HEAD` is allowed to name a tag or another ref; the prompt then shows
        // the commit, which may only exist in `packed-refs` after `git gc`.
        let root = scratch("packed");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), b"ref: refs/tags/v1\n").unwrap();
        std::fs::write(
            root.join(".git/packed-refs"),
            b"# pack-refs with: peeled fully-peeled sorted\n\
              a1b2c3d4e5f60718293a4b5c6d7e8f9012345678 refs/tags/v1\n\
              ^a1b2c3d4e5f60718293a4b5c6d7e8f9012345678\n",
        )
        .unwrap();
        assert_eq!(git_branch(&as_bytes(&root)), Some(b"a1b2c3d".to_vec()));
    }

    #[test]
    fn a_loose_ref_wins_over_packed_refs() {
        let root = scratch("loose-ref");
        std::fs::create_dir_all(root.join(".git/refs/tags")).unwrap();
        std::fs::write(root.join(".git/HEAD"), b"ref: refs/tags/current\n").unwrap();
        std::fs::write(
            root.join(".git/refs/tags/current"),
            b"1111111111111111111111111111111111111111\n",
        )
        .unwrap();
        std::fs::write(
            root.join(".git/packed-refs"),
            b"2222222222222222222222222222222222222222 refs/tags/current\n",
        )
        .unwrap();
        assert_eq!(git_branch(&as_bytes(&root)), Some(b"1111111".to_vec()));
    }
}
