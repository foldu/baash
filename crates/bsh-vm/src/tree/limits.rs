//! The `ulimit` built-in.
//!
//! One resource is selected by a letter (`-f` is the default), and no operand
//! prints the soft limit. Setting updates both limits, which is what bash does
//! and what the conformance cases pin; an unknown letter is an invalid flag
//! rather than a number, so `ulimit -f -42` is a usage error and
//! `ulimit -f -- -42` is a negative value.

use bsh_diag::Diagnostic;
use bsh_runtime::{ShellHost, Status};

use super::Interp;

struct Resource {
    letter: u8,
    limit: bsh_sys::Limit,
    name: &'static str,
    unit: &'static str,
    /// Bytes per reported unit: `-f`/`-c` count 512-byte blocks, the kbyte
    /// resources 1024, and the plain counts one.
    block: u64,
}

const RESOURCES: &[Resource] = &[
    Resource {
        letter: b'c',
        limit: bsh_sys::Limit::Core,
        name: "core file size",
        unit: "blocks",
        block: 512,
    },
    Resource {
        letter: b'd',
        limit: bsh_sys::Limit::Data,
        name: "data seg size",
        unit: "kbytes",
        block: 1024,
    },
    Resource {
        letter: b'f',
        limit: bsh_sys::Limit::Fsize,
        name: "file size",
        unit: "blocks",
        block: 512,
    },
    Resource {
        letter: b'l',
        limit: bsh_sys::Limit::Memlock,
        name: "max locked memory",
        unit: "kbytes",
        block: 1024,
    },
    Resource {
        letter: b'm',
        limit: bsh_sys::Limit::Rss,
        name: "max memory size",
        unit: "kbytes",
        block: 1024,
    },
    Resource {
        letter: b'n',
        limit: bsh_sys::Limit::Nofile,
        name: "open files",
        unit: "",
        block: 1,
    },
    Resource {
        letter: b's',
        limit: bsh_sys::Limit::Stack,
        name: "stack size",
        unit: "kbytes",
        block: 1024,
    },
    Resource {
        letter: b't',
        limit: bsh_sys::Limit::Cpu,
        name: "cpu time",
        unit: "seconds",
        block: 1,
    },
    Resource {
        letter: b'u',
        limit: bsh_sys::Limit::Processes,
        name: "max user processes",
        unit: "",
        block: 1,
    },
    Resource {
        letter: b'v',
        limit: bsh_sys::Limit::AddressSpace,
        name: "virtual memory",
        unit: "kbytes",
        block: 1024,
    },
];

fn fail(interp: &mut Interp, message: String) -> Status {
    interp.diagnose(&Diagnostic::error(
        crate::codes::DEFERRED,
        0..0,
        format!("ulimit: {message}"),
    ));
    Status::FAILURE
}

fn report(limit: Option<u64>, resource: &Resource) -> String {
    match limit {
        Some(value) => (value / resource.block).to_string(),
        None => "unlimited".to_owned(),
    }
}

pub(crate) fn bi_ulimit(interp: &mut Interp, argv: &[Vec<u8>]) -> Status {
    let mut all = false;
    let mut selected = &RESOURCES[2]; // `-f`
    let mut selected_changes = 0;
    let mut which_hard = false;
    let mut which_soft = false;
    let mut operand: Option<Vec<u8>> = None;
    let mut options = true;
    for argument in &argv[1..] {
        if options && argument == b"--" {
            options = false;
            continue;
        }
        if options && argument == b"--all" {
            all = true;
            continue;
        }
        if options && argument.len() > 1 && argument[0] == b'-' {
            for letter in &argument[1..] {
                match letter {
                    b'a' => all = true,
                    b'H' => which_hard = true,
                    b'S' => which_soft = true,
                    other => match RESOURCES.iter().find(|resource| resource.letter == *other) {
                        Some(resource) => {
                            selected = resource;
                            selected_changes += 1;
                        }
                        None => {
                            return fail(
                                interp,
                                format!("-{}: invalid option", char::from(*other)),
                            );
                        }
                    },
                }
            }
            continue;
        }
        if operand.is_some() {
            return fail(interp, "too many arguments".to_owned());
        }
        operand = Some(argument.clone());
    }

    // `-a` is exclusive, and only one resource may be named: those are usage
    // errors (status 2), not limit failures.
    if all && (operand.is_some() || selected_changes > 0) {
        interp.diagnose(&Diagnostic::error(
            crate::codes::DEFERRED,
            0..0,
            "ulimit: -a cannot be combined with a limit".to_owned(),
        ));
        return Status(2);
    }
    if selected_changes > 1 {
        interp.diagnose(&Diagnostic::error(
            crate::codes::DEFERRED,
            0..0,
            "ulimit: only one resource may be named".to_owned(),
        ));
        return Status(2);
    }
    if all {
        let mut out = Vec::new();
        // The reference shell's `-a` lists these eight; `-m` and `-u` still
        // work as flags but are not part of the summary.
        for resource in RESOURCES
            .iter()
            .filter(|resource| !matches!(resource.letter, b'm' | b'u'))
        {
            let current = bsh_sys::get_limit(resource.limit)
                .and_then(|(soft, _)| soft)
                .map(|value| (value / resource.block).to_string())
                .unwrap_or_else(|| "unlimited".to_owned());
            let unit = if resource.unit.is_empty() {
                String::new()
            } else {
                format!("({}, -{})", resource.unit, char::from(resource.letter))
            };
            out.extend_from_slice(
                format!(
                    "{:<24} {:<12} {current}\n",
                    resource.name,
                    unit,
                    current = current
                )
                .as_bytes(),
            );
        }
        return interp.write_out(&out);
    }

    let Some(operand) = operand else {
        let Some((soft, hard)) = bsh_sys::get_limit(selected.limit) else {
            return fail(interp, "not supported on this platform".to_owned());
        };
        let shown = if which_hard { hard } else { soft };
        let text = format!("{}\n", report(shown, selected));
        return interp.write_out(text.as_bytes());
    };

    let soft = if operand == b"unlimited" {
        None
    } else {
        match std::str::from_utf8(&operand)
            .ok()
            .and_then(|text| text.parse::<u64>().ok())
            .and_then(|value| value.checked_mul(selected.block))
        {
            Some(bytes) => Some(bytes),
            None => {
                return fail(
                    interp,
                    format!("{}: invalid number", String::from_utf8_lossy(&operand)),
                );
            }
        }
    };
    // `-S` and `-H` choose one side and leave the other alone; without either,
    // bash sets both.
    let (soft, hard) = match (which_soft, which_hard) {
        (true, false) => (
            soft,
            bsh_sys::get_limit(selected.limit).and_then(|(_, hard)| hard),
        ),
        (false, true) => (
            bsh_sys::get_limit(selected.limit).and_then(|(soft, _)| soft),
            soft,
        ),
        _ => (soft, soft),
    };
    match bsh_sys::set_limit(selected.limit, soft, hard) {
        Ok(()) => Status::SUCCESS,
        Err(error) => fail(interp, format!("cannot set limit: {error}")),
    }
}
