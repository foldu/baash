//! The `history` and `fc` built-ins.
//!
//! Both are thin front ends over [`crate::History`]. The formats are bash
//! 5.3.20's, measured under `bash --norc`:
//!
//! ```text
//! history      →  "    1  echo a"     (five-wide number, two spaces)
//! history 2    →  the last two entries
//! fc -l        →  "1\t echo a"        (number, TAB, space)
//! fc -ln       →  "\t echo a"         (no number)
//! fc -l 1 2    →  entries 1 through 2; 2 1 lists them descending
//! ```
//!
//! `fc` deliberately excludes the command that invoked it: bash adds a typed
//! line to history before dispatch, so `fc -l` hides the last entry in an
//! interactive shell. `fc -s` goes further and *replaces* that entry with the
//! command it re-executes (measured even non-interactively, where it removes
//! the last entry regardless).
//!
//! # Scope
//!
//! `fc` without `-l`/`-s` opens `$FCEDIT`/`$EDITOR` on a temporary file. That
//! is not implemented here: it reports status 2 with a clear message instead of
//! pretending. History expansion (`!$`, `!!`) is a separate missing feature,
//! so `history -p` prints its arguments as they are, which is what bash prints
//! when the expansion produces nothing.

use super::{codes, diagnose, display, usage};
use crate::{HistoryEntry, ShellHost, Status};

const HISTORY_USAGE: &str =
    "usage: history [-c] [-d offset] [n] or history -anrw [filename] or history -ps arg [arg...]";

const FC_USAGE: &str = "usage: fc [-e ename] [-lnr] [first] [last] or fc -s [pat=rep] [command]";

/// How many entries `fc -l` shows when no range is given (bash's default).
const FC_DEFAULT_PAGE: usize = 16;

/// Write one `history` line: a five-wide number, two spaces, the command.
fn write_history_line(out: &mut Vec<u8>, entry: &HistoryEntry) {
    out.extend_from_slice(format!("{:>5}", entry.number()).as_bytes());
    out.extend_from_slice(b"  ");
    out.extend_from_slice(entry.text());
    out.push(b'\n');
}

/// Write one `fc -l` line: number, TAB, space, command; `suppress` drops the
/// number but keeps the TAB and space.
fn write_fc_line(out: &mut Vec<u8>, entry: &HistoryEntry, suppress: bool) {
    if !suppress {
        out.extend_from_slice(entry.number().to_string().as_bytes());
    }
    out.extend_from_slice(b"\t ");
    out.extend_from_slice(entry.text());
    out.push(b'\n');
}

/// `history [-c] [-d offset] [n]`, `-a`, `-n`, `-r`, `-w`, `-p`, `-s`.
pub(super) fn bi_history(host: &mut dyn ShellHost, argv: &[&[u8]]) -> Status {
    host.state().sync_history();

    let mut clear = false;
    let mut delete: Option<Vec<u8>> = None;
    let mut append = false;
    let mut read_new = false;
    let mut read = false;
    let mut write = false;
    let mut print = false;
    let mut store = false;
    let mut operands: Vec<&[u8]> = Vec::new();
    let mut options_done = false;

    let mut index = 1;
    while index < argv.len() {
        let argument = argv[index];
        index += 1;
        if options_done || argument == b"-" || !argument.starts_with(b"-") || argument.len() == 1 {
            operands.push(argument);
            continue;
        }
        if argument == b"--" {
            options_done = true;
            continue;
        }
        let letters = &argument[1..];
        let mut position = 0;
        while position < letters.len() {
            match letters[position] {
                b'c' => clear = true,
                b'a' => append = true,
                b'n' => read_new = true,
                b'r' => read = true,
                b'w' => write = true,
                b'p' => print = true,
                b's' => store = true,
                b'd' => {
                    let attached = &letters[position + 1..];
                    if attached.is_empty() {
                        match argv.get(index) {
                            Some(value) => {
                                delete = Some(value.to_vec());
                                index += 1;
                            }
                            None => {
                                diagnose(
                                    host,
                                    codes::USAGE,
                                    "history: -d: option requires an argument",
                                );
                                diagnose(host, codes::USAGE, format!("history: {HISTORY_USAGE}"));
                                return Status::USAGE;
                            }
                        }
                    } else {
                        delete = Some(attached.to_vec());
                    }
                    position = letters.len();
                    continue;
                }
                other => {
                    diagnose(
                        host,
                        codes::USAGE,
                        format!("history: -{}: invalid option", char::from(other)),
                    );
                    diagnose(host, codes::USAGE, format!("history: {HISTORY_USAGE}"));
                    return Status::USAGE;
                }
            }
            position += 1;
        }
    }

    if clear {
        host.state().history.clear();
    }

    if let Some(offset) = &delete {
        let Some(reference) = parse_reference(offset) else {
            diagnose(
                host,
                codes::NOT_FOUND,
                format!("history: {}: invalid number", display(offset)),
            );
            return Status::FAILURE;
        };
        if !host.state().history.delete(reference) {
            diagnose(
                host,
                codes::NOT_FOUND,
                format!(
                    "history: {}: history position out of range",
                    display(offset)
                ),
            );
            return Status::FAILURE;
        }
    }

    let mut status = Status::SUCCESS;
    if append || read_new || read || write {
        // `-anrw` accept an optional filename in bash; this wave always uses
        // `HISTFILE`, and a filename operand would be silently meaningless,
        // so reject it rather than lie about having used it.
        if !operands.is_empty() && !print && !store {
            return usage(host, argv, "too many arguments");
        }
        let result = if append {
            host.state().history.append_file()
        } else if read_new {
            host.state().history.read_new().map(|_| ())
        } else if read {
            host.state().history.read_file().map(|_| ())
        } else {
            host.state().history.write_file()
        };
        if let Err(error) = result {
            diagnose(host, codes::NOT_FOUND, format!("history: {error}"));
            status = Status::FAILURE;
        }
    }

    if print && !store {
        if !operands.is_empty() {
            let mut out = Vec::new();
            for operand in &operands {
                out.extend_from_slice(operand);
                out.push(b'\n');
            }
            let _ = host.write_out(&out);
        }
        return status;
    }

    if store {
        // bash records the typed line before dispatch, then `history -s`
        // removes it and stores its arguments instead. A non-interactive shell
        // never recorded the line in the first place.
        if host.is_interactive() {
            host.state().history.remove_last();
        }
        let mut line = Vec::new();
        for (position, operand) in operands.iter().enumerate() {
            if position > 0 {
                line.push(b' ');
            }
            line.extend_from_slice(operand);
        }
        if !line.is_empty() {
            host.state().history.add(&line);
        }
        return status;
    }

    let any_action = clear || delete.is_some() || append || read_new || read || write;
    if any_action && operands.is_empty() {
        return status;
    }

    let limit = match operands.as_slice() {
        [] => None,
        [one] => match parse_count(one) {
            Some(limit) => Some(limit),
            None => {
                diagnose(
                    host,
                    codes::USAGE,
                    format!("history: {}: numeric argument required", display(one)),
                );
                return Status::USAGE;
            }
        },
        _ => return usage(host, argv, "too many arguments"),
    };

    let mut out = Vec::new();
    for entry in host.state().history.list(limit) {
        write_history_line(&mut out, entry);
    }
    let _ = host.write_out(&out);
    status
}

/// `fc [-e ename] [-lnr] [first [last]]` and `fc -s [pat=rep] [command]`.
pub(super) fn bi_fc(host: &mut dyn ShellHost, argv: &[&[u8]]) -> Status {
    host.state().sync_history();

    let mut list = false;
    let mut suppress = false;
    let mut reverse = false;
    let mut substitute = false;
    let mut operands: Vec<&[u8]> = Vec::new();
    let mut options_done = false;

    let mut index = 1;
    while index < argv.len() {
        let argument = argv[index];
        index += 1;
        if options_done || !argument.starts_with(b"-") || argument.len() == 1 {
            operands.push(argument);
            continue;
        }
        if argument == b"--" {
            options_done = true;
            continue;
        }
        // `-3` is a negative history offset, not an option.
        if argument[1..].iter().all(u8::is_ascii_digit) {
            operands.push(argument);
            continue;
        }
        let letters = &argument[1..];
        let mut position = 0;
        while position < letters.len() {
            match letters[position] {
                b'l' => list = true,
                b'n' => suppress = true,
                b'r' => reverse = true,
                b's' => substitute = true,
                b'e' => {
                    let attached = &letters[position + 1..];
                    let editor = if attached.is_empty() {
                        argv.get(index).copied().inspect(|_| index += 1)
                    } else {
                        Some(attached)
                    };
                    let Some(editor) = editor else {
                        diagnose(host, codes::USAGE, "fc: -e: option requires an argument");
                        diagnose(host, codes::USAGE, format!("fc: {FC_USAGE}"));
                        return Status::USAGE;
                    };
                    // bash's `fc -e -` is `fc -s`.
                    if editor == b"-" {
                        substitute = true;
                    }
                    position = letters.len();
                    continue;
                }
                other => {
                    diagnose(
                        host,
                        codes::USAGE,
                        format!("fc: -{}: invalid option", char::from(other)),
                    );
                    diagnose(host, codes::USAGE, format!("fc: {FC_USAGE}"));
                    return Status::USAGE;
                }
            }
            position += 1;
        }
    }

    if list && substitute {
        return usage(host, argv, "-l and -s are mutually exclusive");
    }

    if !list && !substitute {
        diagnose(
            host,
            codes::USAGE,
            "fc: editing history is not implemented yet; use `fc -l` or `fc -s`",
        );
        return Status::USAGE;
    }

    // A typed command is in the ring, but `fc` never lists or selects itself.
    let eligible = eligible_len(host);
    if eligible == 0 {
        return if substitute {
            diagnose(host, codes::NOT_FOUND, "fc: no command found");
            Status::FAILURE
        } else {
            Status::SUCCESS
        };
    }

    if list {
        let (first, last) = match operands.as_slice() {
            [] => (eligible.saturating_sub(FC_DEFAULT_PAGE), eligible - 1),
            [first] => match resolve(host, first, eligible, Role::First) {
                Ok(index) => (index, eligible - 1),
                Err(status) => return status,
            },
            [first, last] => {
                let first = match resolve(host, first, eligible, Role::First) {
                    Ok(index) => index,
                    Err(status) => return status,
                };
                let last = match resolve(host, last, eligible, Role::Last) {
                    Ok(index) => index,
                    Err(status) => return status,
                };
                (first, last)
            }
            // bash ignores extra arguments here (`fc -l 0 1 2` is status 0).
            [..] => {
                let first = match resolve(host, operands[0], eligible, Role::First) {
                    Ok(index) => index,
                    Err(status) => return status,
                };
                let last = match resolve(host, operands[1], eligible, Role::Last) {
                    Ok(index) => index,
                    Err(status) => return status,
                };
                (first, last)
            }
        };

        let mut indices: Vec<usize> = if first <= last {
            (first..=last).collect()
        } else {
            (last..=first).rev().collect()
        };
        // `-r` reverses a normal ascending page; bash leaves an explicitly
        // reversed range alone (`fc -lr 3 2` is still 3 2).
        if reverse && first <= last {
            indices.reverse();
        }
        let entries = host.state().history.entries();
        let mut out = Vec::new();
        for index in indices {
            if let Some(entry) = entries.get(index) {
                write_fc_line(&mut out, entry, suppress);
            }
        }
        let _ = host.write_out(&out);
        return Status::SUCCESS;
    }

    // `fc -s [old=new] [first]`
    let mut replacement: Option<(&[u8], &[u8])> = None;
    let mut command_reference: Option<&[u8]> = None;
    for operand in &operands {
        if command_reference.is_none() && replacement.is_none() {
            if let Some(equals) = operand.iter().position(|byte| *byte == b'=') {
                replacement = Some((&operand[..equals], &operand[equals + 1..]));
                continue;
            }
        }
        if command_reference.is_none() {
            command_reference = Some(operand);
        }
    }

    let selected = match command_reference {
        Some(reference) => match resolve(host, reference, eligible, Role::First) {
            Ok(index) => index,
            Err(status) => return status,
        },
        None => eligible - 1,
    };
    let mut command = host
        .state()
        .history
        .entries()
        .get(selected)
        .map(|entry| entry.text().to_vec())
        .unwrap_or_default();
    if let Some((old, new)) = replacement {
        command = substitute_word(&command, old, new);
    }

    let mut out = command.clone();
    out.push(b'\n');
    let _ = host.write_out(&out);

    // Measured: bash removes the last entry and stores the re-executed
    // command in its place, in an interactive shell (where the last entry is
    // the `fc` line itself) and non-interactively alike.
    host.state().history.remove_last();
    host.state().history.add(&command);

    host.run_source(&command)
}

/// Whether the last entry is the command currently running.
fn eligible_len(host: &mut dyn ShellHost) -> usize {
    let length = host.state().history.len();
    if host.is_interactive() && length > 0 {
        length - 1
    } else {
        length
    }
}

/// Which end an out-of-range reference clamps to.
#[derive(Clone, Copy)]
enum Role {
    /// bash clamps a first reference above the list to the oldest entry.
    First,
    /// A last reference above the list clamps to the newest entry.
    Last,
}

/// Resolve an `fc` event reference against the first `eligible` entries.
fn resolve(
    host: &mut dyn ShellHost,
    reference: &[u8],
    eligible: usize,
    role: Role,
) -> Result<usize, Status> {
    let entries = &host.state().history.entries()[..eligible];
    let not_found = |host: &mut dyn ShellHost| {
        diagnose(host, codes::NOT_FOUND, "fc: no command found");
        Status::FAILURE
    };
    if let Some(number) = parse_reference(reference) {
        if number == 0 {
            return Ok(eligible.saturating_sub(1));
        }
        if number > 0 {
            let target = u32::try_from(number).unwrap_or(u32::MAX);
            if let Some(index) = entries.iter().position(|entry| entry.number() == target) {
                return Ok(index);
            }
            // Out of range: bash clamps a high first to the oldest entry (so
            // `fc -l 100` prints everything) and a high last to the newest.
            return Ok(match role {
                Role::First => 0,
                Role::Last => eligible - 1,
            });
        }
        let offset = usize::try_from(number.unsigned_abs()).unwrap_or(usize::MAX);
        if offset >= eligible {
            return Ok(0);
        }
        return Ok(eligible - offset);
    }
    match entries
        .iter()
        .rposition(|entry| entry.text().starts_with(reference))
    {
        Some(index) => Ok(index),
        None => Err(not_found(host)),
    }
}

/// Parse an integer history reference, accepting a leading `-`.
fn parse_reference(value: &[u8]) -> Option<i64> {
    let (negative, digits) = match value.first() {
        Some(b'-') => (true, &value[1..]),
        _ => (false, value),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let magnitude: i64 = std::str::from_utf8(digits).ok()?.parse().ok()?;
    Some(if negative { -magnitude } else { magnitude })
}

/// Parse the `history n` count: a non-negative decimal.
fn parse_count(value: &[u8]) -> Option<usize> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(value).ok()?.parse().ok()
}

/// Replace the first occurrence of `old` with `new`; an empty `old` prepends.
fn substitute_word(command: &[u8], old: &[u8], new: &[u8]) -> Vec<u8> {
    if old.is_empty() {
        let mut out = new.to_vec();
        out.extend_from_slice(command);
        return out;
    }
    if let Some(position) = command.windows(old.len()).position(|window| window == old) {
        let mut out = Vec::with_capacity(command.len() + new.len());
        out.extend_from_slice(&command[..position]);
        out.extend_from_slice(new);
        out.extend_from_slice(&command[position + old.len()..]);
        return out;
    }
    command.to_vec()
}
