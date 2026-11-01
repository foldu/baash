//! The command-introspection built-ins: `type`, `command`, `builtin`, and the
//! alias pair.
//!
//! None of them knows how a name resolves. That is [`crate::ShellHost`]'s job
//! (`resolve_command`), implemented once in `baash-vm` by the same function the
//! executor uses, so `type ll` and running `ll` agree by construction. What
//! lives here is the *formatting* — which is the part the suites pin — and the
//! alias table, which is shell state.

use crate::command::{CommandInfo, CommandKind, ResolveQuery};
use crate::{ShellHost, Status, Tier};

use super::{arg, codes, diagnose, display, program_name, usage};

/// The flags a `type` invocation can carry.
#[derive(Clone, Copy, Debug, Default)]
struct TypeFlags {
    /// `-a`: report every match, not just the first.
    all: bool,
    /// `-t`: print the one-word class only.
    type_only: bool,
    /// `-p`: print only the file that would be executed, if any.
    path_only: bool,
    /// `-P`: force a `PATH` search, ignoring everything else.
    force_path: bool,
    /// `-f`: do not look up shell functions.
    skip_functions: bool,
}

impl TypeFlags {
    /// Whether a diagnostic should be written for a missing name.
    ///
    /// `type -t`, `-p` and `-P` are query modes: they answer through the status,
    /// so they stay silent (the suite pins an empty stderr for `-t`).
    fn quiet(self) -> bool {
        self.type_only || self.path_only || self.force_path
    }

    fn query(self) -> ResolveQuery {
        if self.force_path {
            return ResolveQuery {
                all: self.all,
                files_only: true,
                fresh: true,
                ..ResolveQuery::default()
            };
        }
        ResolveQuery {
            all: self.all,
            skip_functions: self.skip_functions,
            files_only: false,
            classify: true,
            default_path: false,
            fresh: false,
        }
    }
}

/// One line of `type`/`command -V` output: `name is …`.
fn describe_line(out: &mut Vec<u8>, name: &[u8], info: &CommandInfo) {
    out.extend_from_slice(name);
    match info.kind {
        CommandKind::Alias => {
            out.extend_from_slice(b" is an alias for ");
            quote_double(out, &info.target);
        }
        CommandKind::Keyword => out.extend_from_slice(b" is a shell keyword"),
        CommandKind::Function => out.extend_from_slice(b" is a shell function"),
        CommandKind::Builtin(Tier::Special) => {
            out.extend_from_slice(b" is a special shell builtin");
        }
        CommandKind::Builtin(_) => out.extend_from_slice(b" is a shell builtin"),
        CommandKind::External => {
            out.extend_from_slice(b" is ");
            out.extend_from_slice(&info.target);
        }
    }
    out.push(b'\n');
}

/// Quote a value the way `type` prints it: always double-quoted, with the two
/// characters that would end or corrupt the quote escaped.
fn quote_double(out: &mut Vec<u8>, bytes: &[u8]) {
    out.push(b'"');
    for &byte in bytes {
        if matches!(byte, b'"' | b'\\') {
            out.push(b'\\');
        }
        out.push(byte);
    }
    out.push(b'"');
}

/// `alias name='value'`, always single-quoted so the output re-parses.
fn definition_line(out: &mut Vec<u8>, name: &[u8], value: &[u8]) {
    out.extend_from_slice(b"alias ");
    out.extend_from_slice(name);
    out.push(b'=');
    out.push(b'\'');
    for &byte in value {
        if byte == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(byte);
        }
    }
    out.push(b'\'');
    out.push(b'\n');
}

/// `type NAME …`.
pub(super) fn bi_type(host: &mut dyn ShellHost, argv: &[&[u8]]) -> Status {
    let mut flags = TypeFlags::default();
    let mut index = 1;
    while index < argv.len() {
        let argument = argv[index];
        if argument == b"--" {
            index += 1;
            break;
        }
        if argument.len() > 1 && argument[0] == b'-' {
            for &flag in &argument[1..] {
                match flag {
                    b'a' => flags.all = true,
                    b't' => flags.type_only = true,
                    b'p' => flags.path_only = true,
                    b'P' => flags.force_path = true,
                    b'f' => flags.skip_functions = true,
                    _ => return usage(host, argv, "invalid option"),
                }
            }
            index += 1;
            continue;
        }
        break;
    }

    let query = flags.query();
    let mut out = Vec::new();
    let mut status = Status::SUCCESS;
    for &name in &argv[index..] {
        let infos = host.resolve_command(name, query);
        if flags.type_only {
            match infos.first() {
                Some(info) => {
                    out.extend_from_slice(info.kind.type_word().as_bytes());
                    out.push(b'\n');
                }
                None => status = Status::FAILURE,
            }
            continue;
        }
        if flags.path_only || flags.force_path {
            let mut found = false;
            for info in &infos {
                if info.kind.is_external() {
                    out.extend_from_slice(&info.target);
                    out.push(b'\n');
                    found = true;
                }
            }
            // `-p` asks "what would run?" — a built-in answers "nothing" but
            // still exists, so it is not an error. `-P` asks only about files.
            let known = if flags.force_path {
                found
            } else {
                !infos.is_empty()
            };
            if !known {
                status = Status::FAILURE;
            }
            continue;
        }
        let Some(first) = infos.first() else {
            status = Status::FAILURE;
            if !flags.quiet() {
                diagnose(
                    host,
                    codes::NOT_FOUND,
                    format!(
                        "{}: {}: not found",
                        program_name(argv, "type"),
                        display(name)
                    ),
                );
            }
            continue;
        };
        describe_line(&mut out, name, first);
        if matches!(first.kind, CommandKind::Function)
            && let Some(source) = host.function_source(name)
        {
            out.extend_from_slice(name);
            out.extend_from_slice(b" () ");
            out.extend_from_slice(&source);
            out.push(b'\n');
        }
        if flags.all {
            for info in infos.iter().skip(1) {
                describe_line(&mut out, name, info);
            }
        }
    }
    let write_status = host.write_out(&out);
    if !write_status.is_success() {
        return write_status;
    }
    status
}

/// The flags a `command` invocation can carry.
#[derive(Clone, Copy, Debug, Default)]
struct CommandFlags {
    /// `-v`: print the name or path, one line, machine-readable.
    short: bool,
    /// `-V`: print the verbose description `type` uses.
    verbose: bool,
    /// `-p`: run (or search) with the standard utility path.
    default_path: bool,
}

/// `command [-p] [-v|-V] NAME …`.
pub(super) fn bi_command(host: &mut dyn ShellHost, argv: &[&[u8]]) -> Status {
    let mut flags = CommandFlags::default();
    let mut index = 1;
    while index < argv.len() {
        let argument = argv[index];
        if argument == b"--" {
            index += 1;
            break;
        }
        if argument.len() > 1 && argument[0] == b'-' {
            for &flag in &argument[1..] {
                match flag {
                    b'v' => flags.short = true,
                    b'V' => flags.verbose = true,
                    b'p' => flags.default_path = true,
                    _ => return usage(host, argv, "invalid option"),
                }
            }
            index += 1;
            continue;
        }
        break;
    }

    let words = &argv[index..];
    if flags.short || flags.verbose {
        let query = ResolveQuery {
            classify: true,
            default_path: flags.default_path,
            ..ResolveQuery::default()
        };
        let mut out = Vec::new();
        let mut status = Status::SUCCESS;
        let mut missing = Vec::new();
        for &name in words {
            let infos = host.resolve_command(name, query);
            let Some(info) = infos.first() else {
                status = Status::FAILURE;
                missing.push(name.to_vec());
                continue;
            };
            if flags.short {
                match info.kind {
                    CommandKind::Alias => definition_line(&mut out, name, &info.target),
                    CommandKind::External => {
                        out.extend_from_slice(&info.target);
                        out.push(b'\n');
                    }
                    _ => {
                        out.extend_from_slice(name);
                        out.push(b'\n');
                    }
                }
            } else {
                describe_line(&mut out, name, info);
            }
        }
        let write_status = host.write_out(&out);
        // `-v` is the silent query (`command -v missing` prints nothing);
        // `-V` is the verbose one and says why.
        for name in missing.into_iter().filter(|_| flags.verbose) {
            diagnose(
                host,
                codes::NOT_FOUND,
                format!(
                    "{}: {}: not found",
                    program_name(argv, "command"),
                    display(&name)
                ),
            );
        }
        if !write_status.is_success() {
            return write_status;
        }
        return status;
    }

    if words.is_empty() {
        // `command` alone is a no-op, so `command` in a `&&` chain does not
        // change the status.
        return Status::SUCCESS;
    }
    let words: Vec<Vec<u8>> = words.iter().map(|word| word.to_vec()).collect();
    host.run_command_direct(&words, flags.default_path)
}

/// `builtin NAME …`: run a built-in, bypassing functions, aliases and `PATH`.
pub(super) fn bi_builtin(host: &mut dyn ShellHost, argv: &[&[u8]]) -> Status {
    let mut index = 1;
    if arg(argv, 1) == Some(b"--") {
        index = 2;
    }
    let words: Vec<Vec<u8>> = argv[index..].iter().map(|word| word.to_vec()).collect();
    if words.is_empty() {
        return Status::SUCCESS;
    }
    match host.run_builtin_direct(&words) {
        Some(status) => status,
        None => {
            let name = display(&words[0]);
            diagnose(
                host,
                codes::NOT_FOUND,
                format!("builtin: {name}: not a shell builtin"),
            );
            Status::FAILURE
        }
    }
}

/// `alias [--] [NAME[=VALUE] …]`.
pub(super) fn bi_alias(host: &mut dyn ShellHost, argv: &[&[u8]]) -> Status {
    let mut index = 1;
    while index < argv.len() {
        match argv[index] {
            b"--" => {
                index += 1;
                break;
            }
            b"-p" => index += 1,
            argument if argument.len() > 1 && argument[0] == b'-' => {
                return usage(host, argv, "invalid option");
            }
            _ => break,
        }
    }

    let mut out = Vec::new();
    let mut status = Status::SUCCESS;
    if index >= argv.len() {
        for (name, value) in host.state().aliases.iter() {
            definition_line(&mut out, name, value);
        }
        let write_status = host.write_out(&out);
        return if write_status.is_success() {
            status
        } else {
            write_status
        };
    }

    for &operand in &argv[index..] {
        match operand.iter().position(|&byte| byte == b'=') {
            Some(equals) => {
                let name = &operand[..equals];
                if name.is_empty() {
                    diagnose(
                        host,
                        codes::USAGE,
                        format!(
                            "{}: {}: invalid alias name",
                            program_name(argv, "alias"),
                            display(operand)
                        ),
                    );
                    status = Status::FAILURE;
                    continue;
                }
                host.state()
                    .aliases
                    .set(name, operand[equals + 1..].to_vec());
            }
            None => match host.state().aliases.get(operand) {
                Some(value) => {
                    let value = value.to_vec();
                    definition_line(&mut out, operand, &value);
                }
                None => {
                    diagnose(
                        host,
                        codes::NOT_FOUND,
                        format!(
                            "{}: {}: not found",
                            program_name(argv, "alias"),
                            display(operand)
                        ),
                    );
                    status = Status::FAILURE;
                }
            },
        }
    }
    let write_status = host.write_out(&out);
    if !write_status.is_success() {
        return write_status;
    }
    status
}

/// `unalias [-a] NAME …`.
pub(super) fn bi_unalias(host: &mut dyn ShellHost, argv: &[&[u8]]) -> Status {
    let mut index = 1;
    let mut all = false;
    while index < argv.len() {
        match argv[index] {
            b"--" => {
                index += 1;
                break;
            }
            b"-a" => {
                all = true;
                index += 1;
            }
            argument if argument.len() > 1 && argument[0] == b'-' => {
                return usage(host, argv, "invalid option");
            }
            _ => break,
        }
    }

    if all {
        host.state().aliases.clear();
    }
    if index >= argv.len() {
        // `unalias -a` alone is complete; a bare `unalias` is a usage error.
        if all {
            return Status::SUCCESS;
        }
        return usage(host, argv, "usage: unalias [-a] name [name ...]");
    }

    let mut status = Status::SUCCESS;
    for &name in &argv[index..] {
        if !host.state().aliases.remove(name) {
            diagnose(
                host,
                codes::NOT_FOUND,
                format!(
                    "{}: {}: not found",
                    program_name(argv, "unalias"),
                    display(name)
                ),
            );
            status = Status::FAILURE;
        }
    }
    status
}
