//! `getopts`: POSIX option parsing with bash's exact scan position.
//!
//! The observable contract is four variables: `OPTIND` (which argument),
//! `OPTARG` (the option's argument) and the *variable named by the second
//! operand* (the option letter, `?`, or `:` in silent mode). The subtle parts,
//! all verified against bash 5.3.20:
//!
//! * **Clusters advance lazily.** `-ab` parses `a` with `OPTIND` still `1`;
//!   it only becomes `2` when the cluster is exhausted. `-ab hi` parses `b`
//!   with `OPTIND=3` because `b`'s argument was the next word.
//! * **The scan position inside the cluster is out of band.** `OPTIND` alone
//!   cannot express it, so it is kept in [`crate::ShellState`] and discarded
//!   whenever `OPTIND` is *assigned* — including `OPTIND=1` when it is already
//!   `1`, which is how a script resets the scan.
//! * **`--` terminates and is consumed**: after it, `OPTIND` is one past it so
//!   `shift $((OPTIND-1))` leaves the operands.
//! * **A non-option argument terminates without advancing**, which is why
//!   `getopts` is only useful for a leading run of options.
//! * **A leading `:` in the option string selects silent mode**: the result is
//!   `:` for a missing argument and `?` for an unknown option, with `OPTARG`
//!   carrying the offending character and no diagnostic.
//! * **`?` is a success.** Only the end of the arguments returns non-zero; an
//!   invalid option or a missing argument returns 0 with `?` in the variable.

use crate::{ShellHost, Status, Vars};

use super::{arg, codes, diagnose, display, program_name, usage};

/// The operands of one `getopts` call, after the option string is decoded.
struct Invocation {
    /// The option string, with a leading `:` removed.
    spec: Vec<u8>,
    /// Silent mode: a leading `:` was present.
    silent: bool,
    /// The variable that receives the option letter.
    name: Vec<u8>,
    /// The arguments to parse: the explicit list, or the positional ones.
    args: Vec<Vec<u8>>,
}

/// `getopts OPTSTRING NAME [ARG …]`.
pub(super) fn bi_getopts(host: &mut dyn ShellHost, argv: &[&[u8]]) -> Status {
    let Some(raw_spec) = arg(argv, 1) else {
        return usage(host, argv, "usage: getopts optstring name [arg ...]");
    };
    let Some(name) = arg(argv, 2) else {
        return usage(host, argv, "usage: getopts optstring name [arg ...]");
    };
    let silent = raw_spec.first() == Some(&b':');
    let spec = if silent {
        raw_spec[1..].to_vec()
    } else {
        raw_spec.to_vec()
    };
    let args: Vec<Vec<u8>> = if argv.len() > 3 {
        argv[3..].iter().map(|word| word.to_vec()).collect()
    } else {
        host.state().vars.positional().argv.clone()
    };

    parse(
        host,
        argv,
        &Invocation {
            spec,
            silent,
            name: name.to_vec(),
            args,
        },
    )
}

/// Publish one parse step: the two globals, the scan position, then the
/// option variable.
///
/// Returns `Status(2)` when the option variable's name is not a valid
/// identifier, which is `getopts`'s own usage error; the parse has already
/// happened by then, which is observable through `OPTIND`/`OPTARG`.
fn publish(
    host: &mut dyn ShellHost,
    optind: i64,
    offset: usize,
    optarg: &[u8],
    name: &[u8],
    value: &[u8],
) -> Status {
    let text = optind.to_string();
    let _ = host.state().vars.assign_scalar(b"OPTIND", text.as_bytes());
    let _ = host.state().vars.assign_scalar(b"OPTARG", optarg);
    // Written after ours, so a script's own `OPTIND=` is what invalidates it.
    let generation = host.state().vars.optind_writes();
    host.state()
        .set_getopts_position(optind, offset, generation);
    if !Vars::is_name(name) {
        return Status(2);
    }
    let _ = host.state().vars.assign_scalar(name, value);
    Status::SUCCESS
}

fn parse(host: &mut dyn ShellHost, argv: &[&[u8]], invocation: &Invocation) -> Status {
    let (name, args, spec) = (&invocation.name, &invocation.args, &invocation.spec);

    let generation = host.state().vars.optind_writes();
    let raw_optind = host
        .state()
        .vars
        .scalar(b"OPTIND")
        .and_then(parse_index)
        .unwrap_or(1);
    let mut optind = raw_optind.max(1);
    let (last_optind, last_offset, last_generation) = host.state().getopts_position();
    // The offset survives only while `OPTIND` and its write counter agree: an
    // assignment is the script's way of saying "start over".
    let mut offset = if last_generation == generation && last_optind == optind {
        last_offset
    } else {
        0
    };

    // Past the end: park `OPTIND` at `$# + 1` and report the end of options.
    // The variable is set to `?` even here, which the suite pins.
    if optind > args.len() as i64 {
        optind = args.len() as i64 + 1;
        let status = publish(host, optind, 0, b"", name, b"?");
        return if status.is_success() {
            Status::FAILURE
        } else {
            status
        };
    }

    let argument = &args[(optind - 1) as usize];
    if offset == 0 {
        if argument == b"--" {
            let status = publish(host, optind + 1, 0, b"", name, b"?");
            return if status.is_success() {
                Status::FAILURE
            } else {
                status
            };
        }
        // `-` alone and anything without a leading `-` is an operand, not an
        // option: stop without consuming it.
        if argument.len() < 2 || argument[0] != b'-' {
            let status = publish(host, optind, 0, b"", name, b"?");
            return if status.is_success() {
                Status::FAILURE
            } else {
                status
            };
        }
        offset = 1;
    }

    let flag = argument[offset];
    let (known, takes_argument) = lookup(spec, flag);

    if !known {
        offset += 1;
        let mut next = optind;
        if offset >= argument.len() {
            next += 1;
            offset = 0;
        }
        if !invocation.silent {
            diagnose(
                host,
                codes::USAGE,
                format!(
                    "{}: illegal option -- {}",
                    program_name(argv, "getopts"),
                    display(&[flag])
                ),
            );
        }
        let optarg: &[u8] = if invocation.silent { &[flag] } else { b"" };
        let status = publish(host, next, offset, optarg, name, b"?");
        return if status.is_success() {
            Status::SUCCESS
        } else {
            status
        };
    }

    if !takes_argument {
        offset += 1;
        let mut next = optind;
        if offset >= argument.len() {
            next += 1;
            offset = 0;
        }
        let status = publish(host, next, offset, b"", name, &[flag]);
        return if status.is_success() {
            Status::SUCCESS
        } else {
            status
        };
    }

    // `-c10`: the rest of the cluster is the argument.
    if offset + 1 < argument.len() {
        let optarg = argument[offset + 1..].to_vec();
        let status = publish(host, optind + 1, 0, &optarg, name, &[flag]);
        return if status.is_success() {
            Status::SUCCESS
        } else {
            status
        };
    }
    // `-c 10`: the next word is the argument, and both are consumed.
    if (optind as usize) < args.len() {
        let optarg = args[optind as usize].clone();
        let status = publish(host, optind + 2, 0, &optarg, name, &[flag]);
        return if status.is_success() {
            Status::SUCCESS
        } else {
            status
        };
    }
    // Missing required argument: advance past the option, report `?` (or `:`).
    if invocation.silent {
        let status = publish(host, optind + 1, 0, &[flag], name, b":");
        return if status.is_success() {
            Status::SUCCESS
        } else {
            status
        };
    }
    diagnose(
        host,
        codes::USAGE,
        format!(
            "{}: option requires an argument -- {}",
            program_name(argv, "getopts"),
            display(&[flag])
        ),
    );
    let status = publish(host, optind + 1, 0, b"", name, b"?");
    if status.is_success() {
        Status::SUCCESS
    } else {
        status
    }
}

/// Whether `spec` has `flag`, and whether it is followed by `:`.
fn lookup(spec: &[u8], flag: u8) -> (bool, bool) {
    let mut index = 0;
    while index < spec.len() {
        if spec[index] == b':' {
            // A `:` only has meaning right after the letter it qualifies.
            index += 1;
            continue;
        }
        if spec[index] == flag {
            return (true, spec.get(index + 1) == Some(&b':'));
        }
        index += 1;
    }
    (false, false)
}

/// A decimal `OPTIND`.
fn parse_index(bytes: &[u8]) -> Option<i64> {
    let (negative, digits) = match bytes.split_first() {
        Some((&b'-', rest)) => (true, rest),
        Some((&b'+', rest)) => (false, rest),
        _ => (false, bytes),
    };
    if digits.is_empty() {
        return None;
    }
    let mut value: i64 = 0;
    for &byte in digits {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(i64::from(byte - b'0'))?;
    }
    Some(if negative { -value } else { value })
}
