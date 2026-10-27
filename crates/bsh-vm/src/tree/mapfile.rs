//! `mapfile` and its synonym `readarray`: read records into an array.
//!
//! The descriptor work is `read`'s: [`super::read::read_record`] reuses the
//! per-descriptor buffer and the seek-or-buffer rule, so a `mapfile` in the
//! middle of a pipeline never steals bytes from the command after it, and
//! `read` never sees a stale buffer either. What differs is interpretation:
//! `mapfile` takes bytes literally (no backslash processing, no `IFS`), keeps
//! the delimiter unless `-t` was given, and writes an indexed array.
//!
//! `-C`/`-c` (the callback and its quantum) are deliberately out of scope: they
//! execute shell code per record, which is a different feature.

use std::os::fd::RawFd;

use bsh_runtime::{ArrayShape, Status, Vars};

use super::{Interp, read};

/// One `mapfile` invocation.
struct Options {
    /// `-d`: the record delimiter (`0` for `-d ''`).
    delim: u8,
    /// `-n`: at most this many records (`0` means all).
    count: usize,
    /// `-O`: the first index to write, keeping the array's existing elements.
    origin: Option<usize>,
    /// `-s`: records to discard before the first assignment.
    skip: usize,
    /// `-t`: drop the delimiter from each element.
    strip: bool,
    /// `-u`: the descriptor to read.
    fd: RawFd,
    /// The destination array (`MAPFILE` by default).
    array: Vec<u8>,
}

/// `mapfile [-d d] [-n n] [-O o] [-s s] [-t] [-u fd] [array]`.
pub(crate) fn bi_mapfile(interp: &mut Interp, argv: &[Vec<u8>]) -> Status {
    let name = argv
        .first()
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .unwrap_or_else(|| "mapfile".to_owned());
    let mut options = Options {
        delim: b'\n',
        count: 0,
        origin: None,
        skip: 0,
        strip: false,
        fd: 0,
        array: b"MAPFILE".to_vec(),
    };
    let mut index = 1;
    while index < argv.len() {
        let argument = &argv[index];
        if argument == b"--" {
            index += 1;
            break;
        }
        if argument.len() < 2 || argument[0] != b'-' {
            break;
        }
        let mut position = 1;
        while position < argument.len() {
            let flag = argument[position];
            match flag {
                b't' => {
                    options.strip = true;
                    position += 1;
                    continue;
                }
                b'd' | b'n' | b'O' | b's' | b'u' => {}
                b'C' | b'c' => {
                    interp.report(
                        crate::codes::DEFERRED,
                        0..0,
                        format!("{name}: -{}: the callback is not implemented", flag as char),
                    );
                    return Status(2);
                }
                other => {
                    interp.report(
                        crate::codes::DEFERRED,
                        0..0,
                        format!("{name}: -{}: invalid option", other as char),
                    );
                    return Status(2);
                }
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
                        interp.report(
                            crate::codes::DEFERRED,
                            0..0,
                            format!("{name}: -{}: option requires an argument", flag as char),
                        );
                        return Status(2);
                    }
                };
                position = argument.len();
                value
            };
            match flag {
                b'd' => options.delim = value.first().copied().unwrap_or(0),
                b'n' => match parse_count(&value) {
                    Some(count) => options.count = count,
                    None => return bad_number(interp, &name, flag, &value),
                },
                b'O' => match parse_count(&value) {
                    Some(origin) => options.origin = Some(origin),
                    None => return bad_number(interp, &name, flag, &value),
                },
                b's' => match parse_count(&value) {
                    Some(skip) => options.skip = skip,
                    None => return bad_number(interp, &name, flag, &value),
                },
                b'u' => match RawFd::try_from(parse_count(&value).unwrap_or(usize::MAX)) {
                    Ok(fd) => options.fd = fd,
                    Err(_) => return bad_number(interp, &name, flag, &value),
                },
                _ => unreachable!("the flag set is checked above"),
            }
        }
        index += 1;
    }
    if let Some(array) = argv.get(index) {
        options.array = array.clone();
    }
    if !Vars::is_name(&options.array) {
        interp.report(
            crate::codes::DEFERRED,
            0..0,
            format!(
                "{name}: {}: not a valid identifier",
                String::from_utf8_lossy(&options.array)
            ),
        );
        return Status(2);
    }
    fill(interp, &name, &options)
}

fn bad_number(interp: &mut Interp, name: &str, flag: u8, value: &[u8]) -> Status {
    interp.report(
        crate::codes::DEFERRED,
        0..0,
        format!(
            "{name}: -{}: {}: invalid number",
            flag as char,
            String::from_utf8_lossy(value)
        ),
    );
    Status(2)
}

fn fill(interp: &mut Interp, name: &str, options: &Options) -> Status {
    if !bsh_sys::fd_is_open(options.fd) {
        interp.report(
            crate::codes::DEFERRED,
            0..0,
            format!("{name}: {}: invalid file descriptor", options.fd),
        );
        return Status(2);
    }

    // `-s` discards whole records, and a short input simply yields nothing.
    for _ in 0..options.skip {
        match read::read_record(interp, options.fd, options.delim) {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(error) => return read_error(interp, name, options.fd, &error),
        }
    }

    let mut lines: Vec<Vec<u8>> = Vec::new();
    while options.count == 0 || lines.len() < options.count {
        match read::read_record(interp, options.fd, options.delim) {
            Ok(Some(record)) => {
                let mut bytes = record.bytes;
                // `-t` drops the delimiter; a NUL delimiter is dropped even
                // without it, because bash stores records as C strings and the
                // NUL cannot survive. Matching that keeps `mapfile -d ''`
                // reversible instead of leaving a stray byte on every element.
                if (options.strip || options.delim == 0) && record.terminated {
                    bytes.pop();
                }
                lines.push(bytes);
            }
            Ok(None) => break,
            Err(error) => return read_error(interp, name, options.fd, &error),
        }
    }

    let array = &options.array;
    if let Err(error) = interp.state.vars.declare_shape(array, ArrayShape::Indexed) {
        interp.report(
            crate::codes::DEFERRED,
            0..0,
            format!("{name}: {}", error.message()),
        );
        return Status::FAILURE;
    }
    if options.origin.is_none()
        && let Err(error) = interp.state.vars.clear_array(array, ArrayShape::Indexed)
    {
        interp.report(
            crate::codes::DEFERRED,
            0..0,
            format!("{name}: {}", error.message()),
        );
        return Status::FAILURE;
    }
    let base = options.origin.unwrap_or(0) as i64;
    for (offset, line) in lines.iter().enumerate() {
        if let Err(error) = interp
            .state
            .vars
            .set_indexed(array, base + offset as i64, line)
        {
            interp.report(
                crate::codes::DEFERRED,
                0..0,
                format!("{name}: {}", error.message()),
            );
            return Status::FAILURE;
        }
    }
    Status::SUCCESS
}

fn read_error(interp: &mut Interp, name: &str, fd: RawFd, error: &std::io::Error) -> Status {
    interp.report(
        crate::codes::DEFERRED,
        0..0,
        format!("{name}: {fd}: {error}"),
    );
    Status::FAILURE
}

/// A non-negative decimal count.
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
