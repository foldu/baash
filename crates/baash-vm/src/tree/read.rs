//! `read`: line input, `IFS` field splitting, and the option set.
//!
//! The semantics are bash's, deliberately traced from `builtins/read.def`, so
//! the suite's edge cases (trailing `IFS` whitespace, the last variable getting
//! "the rest including delimiters", backslash continuation across physical
//! lines) behave the same way. Two rules drive the shape of this module:
//!
//! * **A read consumes exactly what it needs.** The bytes a read pulls from the
//!   descriptor but does not consume are given back, so `read a; read b` and
//!   `read x; head -1` see the right input (DESIGN.md §4.4). On a seekable
//!   descriptor the surplus is read in one chunk and then rewound with
//!   `lseek`; a pipe has no way to return bytes, so it is read one byte at a
//!   time and nothing is ever stolen from a later reader. The buffer in
//!   [`Interp`] is only a fallback for the case where a chunked read could not
//!   be rewound.
//! * **`read` never kills the shell.** It is an intrinsic utility, so a failure
//!   is a status, not a `ShellError` class. Even a read error from a directory
//!   descriptor is status 1 with a diagnostic.

use baash_sys::RawFd;
use std::time::{Duration, Instant};

use baash_runtime::{ShellHost, Status, Vars};

use super::Interp;

/// What a single `read` call was asked to do.
struct Options {
    /// `-r`: backslashes are ordinary characters.
    raw: bool,
    /// `-d`: the record delimiter (`0` for `-d ''`).
    delim: u8,
    /// `-N`: the delimiter is not recognised at all.
    ignore_delim: bool,
    /// `-n`/`-N`: stop after this many characters.
    nchars: Option<usize>,
    /// `-p`: prompt text, used only when the descriptor is a terminal.
    prompt: Option<Vec<u8>>,
    /// `-s`: turn echo off while reading a terminal.
    silent: bool,
    /// `-u`: the descriptor to read.
    fd: RawFd,
    /// `-t`: (seconds, microseconds), or `None` for no timeout.
    timeout: Option<(u64, u32)>,
    /// `-a`: assign the fields to this array instead of to names.
    array: Option<Vec<u8>>,
    /// The variable names (`REPLY` when empty).
    names: Vec<Vec<u8>>,
}

impl Options {
    fn new() -> Self {
        Self {
            raw: false,
            delim: b'\n',
            ignore_delim: false,
            nchars: None,
            prompt: None,
            silent: false,
            fd: 0,
            timeout: None,
            array: None,
            names: Vec::new(),
        }
    }
}

/// Why the read loop stopped.
enum Stop {
    /// The delimiter was read.
    Delimiter,
    /// The character count was reached.
    Count,
    /// End of file (`0` bytes from the descriptor).
    Eof,
    /// `-t` expired.
    Timeout,
    /// The descriptor could not be read.
    Error(std::io::Error),
}

/// A line as read, with the quoting bits the splitter needs.
///
/// `quoted[i]` means byte `i` was escaped by a backslash, so it is a literal
/// character even when it appears in `IFS`. bash stores this in-band with
/// `CTLESC` bytes; a parallel mask is the same information without a reserved
/// byte that shell input could legitimately contain.
struct Line {
    bytes: Vec<u8>,
    quoted: Vec<bool>,
}

/// `read`'s parse of its argv, or a status to return.
enum Parsed {
    Options(Options),
    Status(Status),
}

/// The `read` built-in.
pub(crate) fn bi_read(interp: &mut Interp, argv: &[Vec<u8>]) -> Status {
    let options = match parse(interp, argv) {
        Parsed::Options(options) => options,
        Parsed::Status(status) => return status,
    };
    read_line(interp, &options)
}

fn parse(interp: &mut Interp, argv: &[Vec<u8>]) -> Parsed {
    let mut options = Options::new();
    let mut index = 1;
    while index < argv.len() {
        let argument = &argv[index];
        if argument == b"--" {
            index += 1;
            break;
        }
        if argument.len() < 2 || argument[0] != b'-' {
            // A bare `-` is an operand, as it is for bash's getopt.
            break;
        }
        let mut position = 1;
        while position < argument.len() {
            let flag = argument[position];
            if flag == b'r' {
                options.raw = true;
                position += 1;
                continue;
            }
            if flag == b's' {
                options.silent = true;
                position += 1;
                continue;
            }
            if flag == b'e' {
                // `-e` would use readline; without it bash reads normally.
                position += 1;
                continue;
            }
            if !matches!(flag, b'a' | b'd' | b'i' | b'n' | b'N' | b'p' | b't' | b'u') {
                return Parsed::Status(usage(interp, argv, "invalid option"));
            }
            let value: Vec<u8> = if position + 1 < argument.len() {
                let value = argument[position + 1..].to_vec();
                position = argument.len();
                value
            } else {
                index += 1;
                let value = match argv.get(index) {
                    Some(value) => value.clone(),
                    None => {
                        return Parsed::Status(usage(interp, argv, "option requires an argument"));
                    }
                };
                position = argument.len();
                value
            };
            match flag {
                b'd' => {
                    // bash uses the first byte; `-d ''` is the NUL record
                    // separator.
                    options.delim = value.first().copied().unwrap_or(0);
                }
                b'a' => options.array = Some(value),
                b'i' => {
                    // `-i` only seeds readline, which is not implemented.
                }
                b'n' | b'N' => {
                    if flag == b'N' {
                        options.ignore_delim = true;
                    }
                    match parse_count(&value) {
                        Some(count) => options.nchars = Some(count),
                        None => {
                            return Parsed::Status(usage(interp, argv, "invalid character count"));
                        }
                    }
                }
                b'p' => options.prompt = Some(value),
                b't' => match parse_timeout(&value) {
                    Some(timeout) => options.timeout = Some(timeout),
                    None => {
                        return Parsed::Status(usage(
                            interp,
                            argv,
                            "invalid timeout specification",
                        ));
                    }
                },
                b'u' => match parse_fd(&value) {
                    Some(fd) => options.fd = fd,
                    None => {
                        return Parsed::Status(usage(interp, argv, "invalid file descriptor"));
                    }
                },
                _ => unreachable!("the flag set is checked above"),
            }
        }
        index += 1;
    }
    options.names = argv[index..].to_vec();
    Parsed::Options(options)
}

fn usage(interp: &mut Interp, argv: &[Vec<u8>], message: &str) -> Status {
    let name = argv
        .first()
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .unwrap_or_else(|| "read".to_owned());
    interp.report(crate::codes::DEFERRED, 0..0, format!("{name}: {message}"));
    Status(2)
}

/// A non-negative decimal count, as `-n`/`-N` accept.
fn parse_count(bytes: &[u8]) -> Option<usize> {
    if bytes.is_empty() {
        return None;
    }
    let mut value: usize = 0;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value
            .checked_mul(10)?
            .checked_add(usize::from(byte - b'0'))?;
    }
    Some(value)
}

/// A non-negative decimal descriptor.
fn parse_fd(bytes: &[u8]) -> Option<RawFd> {
    let value = parse_count(bytes)?;
    RawFd::try_from(value).ok()
}

/// `seconds[.fraction]`, rejecting anything negative or non-numeric.
fn parse_timeout(bytes: &[u8]) -> Option<(u64, u32)> {
    let (seconds, fraction) = match bytes.iter().position(|&byte| byte == b'.') {
        Some(dot) => (&bytes[..dot], &bytes[dot + 1..]),
        None => (bytes, &b""[..]),
    };
    if seconds.is_empty() || fraction.len() > 6 {
        return None;
    }
    let seconds: u64 = parse_count(seconds)? as u64;
    let mut micros: u32 = 0;
    if !fraction.is_empty() {
        let value = parse_count(fraction)?;
        let scale = 10u32.pow(u32::try_from(6 - fraction.len()).ok()?);
        micros = (value as u32).checked_mul(scale)?;
    }
    Some((seconds, micros))
}

fn read_line(interp: &mut Interp, options: &Options) -> Status {
    // `poll_readable` stays here rather than moving to compio (`docs/direction.md`
    // §6, W3): this is one `poll(2)` with a deadline on one descriptor, not an
    // event loop, and `baash -c 'read -t 1 x'` deliberately starts no runtime —
    // giving it one would put a driver on the fastest path the shell has.
    // `-t 0` only asks whether input is available; it never reads or assigns.
    if let Some((0, 0)) = options.timeout {
        return match baash_sys::poll_readable(options.fd, 0) {
            Ok(true) => Status::SUCCESS,
            Ok(false) => Status::FAILURE,
            Err(_) => Status::FAILURE,
        };
    }

    if !baash_sys::fd_is_open(options.fd) {
        interp.report(
            crate::codes::DEFERRED,
            0..0,
            format!("read: {}: invalid file descriptor", options.fd),
        );
        return Status(2);
    }

    // `-p` and `-s` are terminal features; off a terminal they are silently
    // ignored rather than being an error (bash does the same).
    let terminal = baash_sys::is_tty(options.fd);
    if terminal && let Some(prompt) = &options.prompt {
        let _ = interp.write_err(prompt);
    }
    let _echo = if terminal && options.silent {
        baash_sys::NoEcho::enable(options.fd).ok()
    } else {
        None
    };

    let (line, stop) = interp.read_bytes(options);
    drop(_echo);

    let status = match &stop {
        Stop::Error(error) => {
            interp.report(
                crate::codes::DEFERRED,
                0..0,
                format!("read: {}: {error}", options.fd),
            );
            Status::FAILURE
        }
        Stop::Timeout => Status(128 + baash_sys::SIGALRM),
        Stop::Eof => Status::FAILURE,
        Stop::Delimiter | Stop::Count => Status::SUCCESS,
    };

    // Assign even after EOF or a timeout: bash delivers the partial line and
    // reports the failure through the status (the suite pins both halves).
    let mut assign_status = assign(interp, options, &line);
    if !status.is_success() {
        assign_status = status;
    }
    assign_status
}

/// Read one record from the descriptor, honouring `-r`, `-d`, `-n`/`-N` and
/// `-t`.
impl Interp {
    fn read_bytes(&mut self, options: &Options) -> (Line, Stop) {
        let fd = options.fd;
        // A seekable descriptor can be read in a chunk and rewound; a pipe
        // cannot give bytes back, so it is read one byte at a time. That is
        // why a pipe never loses data to a later reader (DESIGN.md §4.4).
        let seekable = baash_sys::lseek(fd, 0, baash_sys::seek::CUR).is_ok();
        let chunk = if seekable { 4096 } else { 1 };
        // bash disables `-t` for a regular file: the read cannot block.
        let deadline = match options.timeout {
            Some(timeout) if !seekable => {
                let (seconds, micros) = timeout;
                Some(Instant::now() + Duration::new(seconds, micros.saturating_mul(1000)))
            }
            _ => None,
        };

        let mut buffer = self.read_buffers.remove(&fd).unwrap_or_default();
        let mut consumed = 0usize;
        let mut line = Line {
            bytes: Vec::new(),
            quoted: Vec::new(),
        };
        let mut pass_next = false;
        let mut count = 0usize;
        let stop;

        loop {
            if consumed >= buffer.len() {
                if let Some(deadline) = deadline {
                    let now = Instant::now();
                    if now >= deadline {
                        stop = Stop::Timeout;
                        break;
                    }
                    let remaining = deadline.saturating_duration_since(now);
                    let millis = remaining.as_millis().max(1).min(i32::MAX as u128) as i32;
                    match baash_sys::poll_readable(fd, millis) {
                        Ok(true) => {}
                        Ok(false) => {
                            stop = Stop::Timeout;
                            break;
                        }
                        Err(error) => {
                            stop = Stop::Error(error);
                            break;
                        }
                    }
                }
                let mut chunk_bytes = [0u8; 4096];
                let want = chunk.min(chunk_bytes.len());
                match baash_sys::read_fd(fd, &mut chunk_bytes[..want]) {
                    Ok(0) => {
                        stop = Stop::Eof;
                        break;
                    }
                    Ok(read) => {
                        buffer.clear();
                        consumed = 0;
                        buffer.extend_from_slice(&chunk_bytes[..read]);
                    }
                    Err(error) => {
                        stop = Stop::Error(error);
                        break;
                    }
                }
            }
            let byte = buffer[consumed];
            consumed += 1;

            // A backslash-newline pair disappears regardless of the delimiter
            // (`read -d ,` still continues a line), and an escaped character
            // is a literal: it is never a delimiter and never an `IFS` split.
            if pass_next {
                pass_next = false;
                if byte == b'\n' {
                    continue;
                }
                line.bytes.push(byte);
                line.quoted.push(true);
                count += 1;
                if Some(count) == options.nchars {
                    stop = Stop::Count;
                    break;
                }
                continue;
            }
            if byte == b'\\' && !options.raw {
                pass_next = true;
                continue;
            }
            if !options.ignore_delim && byte == options.delim {
                stop = Stop::Delimiter;
                break;
            }
            if byte == 0 && options.delim != 0 {
                // NUL is not data for a newline-terminated record; `-d ''`
                // makes it the delimiter instead.
                continue;
            }
            line.bytes.push(byte);
            line.quoted.push(false);
            count += 1;
            if Some(count) == options.nchars {
                stop = Stop::Count;
                break;
            }
        }

        // Give back the bytes that were read but not consumed.
        let leftover = buffer.len().saturating_sub(consumed);
        if leftover > 0 {
            let surplus = buffer[consumed..].to_vec();
            let rewound =
                seekable && baash_sys::lseek(fd, -(leftover as i64), baash_sys::seek::CUR).is_ok();
            if !rewound {
                self.read_buffers.insert(fd, surplus);
            }
        }
        (line, stop)
    }
}

/// One record read by `mapfile`.
pub(crate) struct Record {
    /// The bytes, including the delimiter when one was seen.
    pub(crate) bytes: Vec<u8>,
    /// Whether the delimiter ended the record (false: end of file did).
    pub(crate) terminated: bool,
}

/// Read one delimited record from `fd`, for `mapfile`.
///
/// This is `read`'s no-over-read discipline without the shell-quoting rules:
/// mapfile takes bytes literally, keeps the delimiter, and must not steal the
/// bytes a later reader needs. The buffer and the seek-or-buffer decision are
/// shared with [`Interp::read_bytes`]; only the interpretation differs.
pub(crate) fn read_record(
    interp: &mut Interp,
    fd: RawFd,
    delim: u8,
) -> Result<Option<Record>, std::io::Error> {
    let seekable = baash_sys::lseek(fd, 0, baash_sys::seek::CUR).is_ok();
    let want = if seekable { 4096 } else { 1 };
    let mut buffer = interp.read_buffers.remove(&fd).unwrap_or_default();
    let mut consumed = 0usize;
    let mut bytes = Vec::new();
    let mut terminated = false;
    let mut eof = false;
    loop {
        if consumed >= buffer.len() {
            let mut chunk = [0u8; 4096];
            match baash_sys::read_fd(fd, &mut chunk[..want]) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(read) => {
                    buffer.clear();
                    consumed = 0;
                    buffer.extend_from_slice(&chunk[..read]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        let byte = buffer[consumed];
        consumed += 1;
        bytes.push(byte);
        if byte == delim {
            terminated = true;
            break;
        }
    }
    let leftover = buffer.len().saturating_sub(consumed);
    if leftover > 0 {
        let surplus = buffer[consumed..].to_vec();
        let rewound =
            seekable && baash_sys::lseek(fd, -(leftover as i64), baash_sys::seek::CUR).is_ok();
        if !rewound {
            interp.read_buffers.insert(fd, surplus);
        }
    }
    if eof && bytes.is_empty() {
        return Ok(None);
    }
    Ok(Some(Record { bytes, terminated }))
}

/// Assign the read line to the destination variables, bash's `assign_vars`.
fn assign(interp: &mut Interp, options: &Options, line: &Line) -> Status {
    let ifs = effective_ifs(interp);

    if let Some(array) = &options.array {
        return assign_array(interp, array, line, &ifs);
    }
    if options.names.is_empty() {
        return match interp.state.vars.assign_scalar(b"REPLY", &line.bytes) {
            Ok(()) => Status::SUCCESS,
            Err(error) => {
                interp.report(
                    crate::codes::DEFERRED,
                    0..0,
                    format!("read: {}", error.message()),
                );
                Status::FAILURE
            }
        };
    }

    for name in &options.names {
        if !Vars::is_name(name) {
            interp.report(
                crate::codes::DEFERRED,
                0..0,
                format!(
                    "read: {}: not a valid identifier",
                    String::from_utf8_lossy(name)
                ),
            );
            return Status::FAILURE;
        }
    }

    let splitter = Splitter {
        bytes: &line.bytes,
        quoted: &line.quoted,
        // `read -N` does no field splitting at all: it hands the characters to
        // the first variable (`read.def` clears `ifs_chars` for it).
        ifs: if options.ignore_delim { &b""[..] } else { &ifs },
    };
    let fields = splitter.read_fields(options.names.len());
    let mut status = Status::SUCCESS;
    for (index, name) in options.names.iter().enumerate() {
        let value = fields.get(index).cloned().unwrap_or_default();
        if let Err(error) = interp.state.vars.assign_scalar(name, &value) {
            interp.report(
                crate::codes::DEFERRED,
                0..0,
                format!(
                    "read: {}: {}",
                    String::from_utf8_lossy(name),
                    error.message()
                ),
            );
            status = Status::FAILURE;
        }
    }
    status
}

/// `read -a`: split the line into words and replace the array's contents.
fn assign_array(interp: &mut Interp, array: &[u8], line: &Line, ifs: &[u8]) -> Status {
    if !Vars::is_name(array) {
        interp.report(
            crate::codes::DEFERRED,
            0..0,
            format!(
                "read: {}: not a valid identifier",
                String::from_utf8_lossy(array)
            ),
        );
        return Status::FAILURE;
    }
    let splitter = Splitter {
        bytes: &line.bytes,
        quoted: &line.quoted,
        ifs,
    };
    let words = splitter.words();
    if let Err(error) = interp
        .state
        .vars
        .declare_shape(array, baash_runtime::ArrayShape::Indexed)
    {
        interp.report(
            crate::codes::DEFERRED,
            0..0,
            format!(
                "read: {}: {}",
                String::from_utf8_lossy(array),
                error.message()
            ),
        );
        return Status::FAILURE;
    }
    if let Err(error) = interp
        .state
        .vars
        .clear_array(array, baash_runtime::ArrayShape::Indexed)
    {
        interp.report(
            crate::codes::DEFERRED,
            0..0,
            format!(
                "read: {}: {}",
                String::from_utf8_lossy(array),
                error.message()
            ),
        );
        return Status::FAILURE;
    }
    let mut status = Status::SUCCESS;
    for (index, word) in words.iter().enumerate() {
        if let Err(error) = interp
            .state
            .vars
            .set_indexed(array, index as i64, word.as_slice())
        {
            interp.report(
                crate::codes::DEFERRED,
                0..0,
                format!(
                    "read: {}: {}",
                    String::from_utf8_lossy(array),
                    error.message()
                ),
            );
            status = Status::FAILURE;
            break;
        }
    }
    status
}

/// The `IFS` in force: unset means the default, empty means "no splitting".
fn effective_ifs(interp: &Interp) -> Vec<u8> {
    match interp.state.vars.scalar(b"IFS") {
        Some(value) => value.to_vec(),
        None => b" \t\n".to_vec(),
    }
}

/// `IFS` field splitting, with bash's exact rules (traced from `subst.c`).
struct Splitter<'a> {
    bytes: &'a [u8],
    quoted: &'a [bool],
    ifs: &'a [u8],
}

impl<'a> Splitter<'a> {
    /// bash's `sh_style_split`: the default `IFS`, where runs of *spaces, tabs
    /// and newlines* are one delimiter and each non-whitespace `IFS` character
    /// is a delimiter of its own.
    fn sh_style(&self) -> bool {
        self.ifs == b" \t\n"
    }

    fn is_sep(&self, index: usize) -> bool {
        index < self.bytes.len() && !self.quoted[index] && self.ifs.contains(&self.bytes[index])
    }

    /// Whether the separator at `index` is "IFS whitespace" (space, tab or
    /// newline, and in `IFS`).
    fn is_sep_white(&self, index: usize) -> bool {
        index < self.bytes.len()
            && !self.quoted[index]
            && self.ifs.contains(&self.bytes[index])
            && (if self.sh_style() {
                is_spctabnl(self.bytes[index])
            } else {
                is_space(self.bytes[index])
            })
    }

    /// `get_word_from_string`: skip leading `IFS` whitespace, extract one field
    /// and leave `position` just past its delimiters.
    ///
    /// Returns `None` when nothing but `IFS` whitespace was left.
    fn next_word(&self, position: &mut usize) -> Option<(usize, usize)> {
        let len = self.bytes.len();
        let mut index = *position;
        while index < len && !self.quoted[index] && self.ifs.contains(&self.bytes[index]) {
            let space = if self.sh_style() {
                is_spctabnl(self.bytes[index])
            } else {
                is_space(self.bytes[index])
            };
            if !space {
                break;
            }
            index += 1;
        }
        if index >= len {
            *position = index;
            return None;
        }
        let start = index;
        while index < len {
            if self.quoted[index] {
                index += 1;
                continue;
            }
            if self.ifs.contains(&self.bytes[index]) {
                break;
            }
            index += 1;
        }
        let end = index;
        let white_sep = self.is_sep_white(index);
        // Move past the separator character, then an adjacent run of `IFS`
        // whitespace.
        if index < len {
            index += 1;
        }
        while index < len
            && !self.quoted[index]
            && is_spctabnl(self.bytes[index])
            && self.ifs.contains(&self.bytes[index])
        {
            index += 1;
        }
        // A non-whitespace `IFS` character *adjacent* to `IFS` whitespace
        // belongs to the same delimiter (POSIX 3.6.5 (3)(b)), so it does not
        // create an empty field.
        if index < len && white_sep && self.is_sep(index) && !self.is_sep_white(index) {
            index += 1;
            while index < len && self.is_sep_white(index) {
                index += 1;
            }
        }
        *position = index;
        Some((start, end))
    }

    /// The `read` maximum-split rule: each name takes one field and the last
    /// name takes the rest of the line, delimiters included, with trailing
    /// `IFS` whitespace removed.
    fn read_fields(&self, names: usize) -> Vec<Vec<u8>> {
        let len = self.bytes.len();
        let mut fields = Vec::with_capacity(names);
        let mut position = 0usize;
        // Lead-in stripping happens once, before any name is assigned.
        while position < len && !self.quoted[position] && self.ifs.contains(&self.bytes[position]) {
            let space = if self.sh_style() {
                is_spctabnl(self.bytes[position])
            } else {
                is_space(self.bytes[position])
            };
            if !space {
                break;
            }
            position += 1;
        }

        for _ in 1..names {
            if position >= len {
                fields.push(Vec::new());
                continue;
            }
            match self.next_word(&mut position) {
                Some((start, end)) => fields.push(self.bytes[start..end].to_vec()),
                None => fields.push(Vec::new()),
            }
        }

        if names == 0 {
            return fields;
        }
        // The last variable: if consuming one more field exhausts the input,
        // the value is that field; otherwise it is everything that is left
        // (delimiters included), without trailing `IFS` whitespace.
        if position >= len {
            fields.push(Vec::new());
            return fields;
        }
        let start = position;
        let word = self.next_word(&mut position);
        let value = if position >= len {
            match word {
                Some((word_start, word_end)) => self.bytes[word_start..word_end].to_vec(),
                None => Vec::new(),
            }
        } else {
            let end = self.strip_trailing(start);
            self.bytes[start..end].to_vec()
        };
        fields.push(value);
        fields
    }

    /// bash's `strip_trailing_ifs_whitespace`: the end index after removing
    /// trailing `IFS` whitespace, never removing the string's first byte (an
    /// odd but observable bash rule).
    fn strip_trailing(&self, start: usize) -> usize {
        let mut end = self.bytes.len();
        loop {
            if end <= start + 1 {
                break;
            }
            let last = end - 1;
            if self.quoted[last] || !self.ifs.contains(&self.bytes[last]) {
                break;
            }
            if !is_spctabnl(self.bytes[last]) {
                break;
            }
            end -= 1;
        }
        end
    }

    /// `list_string`: ordinary unlimited field splitting, as `read -a` uses.
    fn words(&self) -> Vec<Vec<u8>> {
        if self.bytes.is_empty() {
            return Vec::new();
        }
        let len = self.bytes.len();
        let mut words = Vec::new();
        let mut index = 0usize;
        // Leading `IFS` whitespace is ignored.
        while index < len && !self.quoted[index] && self.ifs.contains(&self.bytes[index]) {
            let space = if self.sh_style() {
                is_spctabnl(self.bytes[index])
            } else {
                is_space(self.bytes[index])
            };
            if !space {
                break;
            }
            index += 1;
        }
        while index < len {
            let start = index;
            while index < len {
                if self.quoted[index] {
                    index += 1;
                    continue;
                }
                if self.ifs.contains(&self.bytes[index]) {
                    break;
                }
                index += 1;
            }
            let word = &self.bytes[start..index];
            if !word.is_empty() {
                words.push(word.to_vec());
            } else if !self.sh_style()
                && index < len
                && !self.quoted[index]
                && !is_space(self.bytes[index])
            {
                // Two adjacent non-whitespace delimiters separate an empty
                // field: `IFS=,; echo a,,b` gives three fields.
                words.push(Vec::new());
            }
            let white_sep = self.is_sep_white(index);
            if index < len {
                index += 1;
            }
            while index < len
                && !self.quoted[index]
                && is_spctabnl(self.bytes[index])
                && self.ifs.contains(&self.bytes[index])
            {
                index += 1;
            }
            if index < len && white_sep && self.is_sep(index) && !self.is_sep_white(index) {
                index += 1;
                while index < len && self.is_sep_white(index) {
                    index += 1;
                }
            }
        }
        words
    }
}

/// bash's `spctabnl`: the characters POSIX calls IFS whitespace.
fn is_spctabnl(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n')
}

/// The wider `ISSPACE` class bash uses for "adjacent" delimiter runs.
fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}
