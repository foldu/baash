//! Executor-side built-ins.
//!
//! `baash-runtime` owns the builtin registry, the tiers and every body that only
//! touches shell state (`export`, `readonly`, `set`, `shift`, `unset`, `exit`,
//! `:`, `true`, `false`). Three kinds of body cannot live there:
//!
//! * bodies that *unwind* (`break`, `continue`, `return`), because unwinding is
//!   the evaluator's control-flow channel;
//! * bodies that need the syscall layer or the interpreter (`cd`, `exec`,
//!   `eval`, `.`/`source`);
//! * `local`/`declare`, which need the frame stack that only the evaluator
//!   opens.
//!
//! A VM built-in is consulted **only when the registry entry has no body**
//! (`BuiltinDef::is_implemented()` is false), so when `baash-runtime` fills one
//! in this module goes dormant instead of shadowing it. That rule is what keeps
//! the ownership boundary honest.
//!
//! `echo` is here because a shell that forks `/bin/echo` for the most common
//! command in existence is not an interactive shell, and because `echo -e` /
//! `echo -n` are *bash* behaviour, not the coreutils behaviour a `PATH` hit
//! would give. It stops being used the moment `baash-runtime` writes its body.
//!
//! Three more bodies are here because their effect *is* executor state, not
//! shell state: `hash` (which path a later command word runs), and
//! `mapfile`/`readarray` (the descriptor table and the per-descriptor read
//! buffer that `read` already owns). Their registry entries stay stubs, which
//! is the documented mechanism for "this body belongs to the executor".
//!
//! `printf` and `test`/`[` are *not* here: they are regular built-ins in POSIX
//! terms, so they stay exec-able, and their bodies live in `baash-runtime`
//! (`builtins/printf.rs`, `builtins/test.rs`), which owns the format language,
//! the expression grammar and the shell state they read. `pwd` is still
//! unwritten, so the `PATH` position serves it — exactly where POSIX puts a
//! regular built-in.

use baash_diag::Diagnostic;
use baash_runtime::{CreateScope, ShellError, ShellHost, Status};

use super::{Interp, Unwind};

/// One executor-side built-in.
pub(crate) struct VmBuiltin {
    /// Name as typed.
    pub(crate) name: &'static [u8],
    /// The body. `argv[0]` is the name as written.
    pub(crate) run: fn(&mut Interp, &[Vec<u8>]) -> Status,
}

pub(crate) fn lookup(name: &[u8]) -> Option<&'static VmBuiltin> {
    VM_BUILTINS.iter().find(|builtin| builtin.name == name)
}

static VM_BUILTINS: &[VmBuiltin] = &[
    VmBuiltin {
        name: b"break",
        run: bi_break,
    },
    VmBuiltin {
        name: b"pwd",
        run: super::dirs::bi_pwd,
    },
    VmBuiltin {
        name: b"continue",
        run: bi_continue,
    },
    VmBuiltin {
        name: b"return",
        run: bi_return,
    },
    VmBuiltin {
        name: b"cd",
        run: bi_cd,
    },
    VmBuiltin {
        name: b"echo",
        run: bi_echo,
    },
    VmBuiltin {
        name: b"eval",
        run: bi_eval,
    },
    VmBuiltin {
        name: b".",
        run: bi_source,
    },
    VmBuiltin {
        name: b"source",
        run: bi_source,
    },
    VmBuiltin {
        name: b"local",
        run: bi_local,
    },
    VmBuiltin {
        name: b"declare",
        run: bi_declare,
    },
    VmBuiltin {
        name: b"typeset",
        run: bi_declare,
    },
    VmBuiltin {
        name: b"read",
        run: super::read::bi_read,
    },
    VmBuiltin {
        name: b"wait",
        run: super::Interp::bi_wait,
    },
    VmBuiltin {
        name: b"kill",
        run: super::Interp::bi_kill,
    },
    VmBuiltin {
        name: b"jobs",
        run: super::Interp::bi_job_control_unavailable,
    },
    VmBuiltin {
        name: b"fg",
        run: super::Interp::bi_job_control_unavailable,
    },
    VmBuiltin {
        name: b"bg",
        run: super::Interp::bi_job_control_unavailable,
    },
    VmBuiltin {
        name: b"disown",
        run: super::Interp::bi_job_control_unavailable,
    },
    VmBuiltin {
        name: b"hash",
        run: bi_hash,
    },
    VmBuiltin {
        name: b"umask",
        run: super::umask::bi_umask,
    },
    VmBuiltin {
        name: b"ulimit",
        run: super::limits::bi_ulimit,
    },
    VmBuiltin {
        name: b"dirs",
        run: super::dirs::bi_dirs,
    },
    VmBuiltin {
        name: b"pushd",
        run: super::dirs::bi_pushd,
    },
    VmBuiltin {
        name: b"popd",
        run: super::dirs::bi_popd,
    },
    VmBuiltin {
        name: b"mapfile",
        run: super::mapfile::bi_mapfile,
    },
    VmBuiltin {
        name: b"readarray",
        run: super::mapfile::bi_mapfile,
    },
];

fn diagnose(interp: &mut Interp, message: impl Into<String>) {
    interp.diagnose(&Diagnostic::error(
        crate::codes::DEFERRED,
        0..0,
        message.into(),
    ));
}

pub(super) fn program(argv: &[Vec<u8>], fallback: &str) -> String {
    argv.first()
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .unwrap_or_else(|| fallback.to_owned())
}

/// A small decimal count, for `break n`, `continue n`, `return n`.
fn count(argv: &[Vec<u8>], index: usize) -> Option<u32> {
    let argument = argv.get(index)?;
    if argument.is_empty() || !argument.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(argument).ok()?.parse().ok()
}

fn loop_control(interp: &mut Interp, argv: &[Vec<u8>], break_it: bool) -> Status {
    if argv.len() > 2 {
        // bash warns "too many arguments". Reading the extra words as a count
        // made `continue 1 2 3` loop forever, so the extra arguments are a
        // usage error for the built-in.
        diagnose(
            interp,
            format!(
                "{}: too many arguments",
                program(argv, if break_it { "break" } else { "continue" })
            ),
        );
        interp.state.record_error(ShellError::SpecialBuiltin {
            status: Status(2),
            via_command: false,
        });
        return Status(2);
    }
    let requested = match argv.get(1) {
        None => 1,
        Some(_) => match count(argv, 1) {
            Some(0) | None => {
                diagnose(
                    interp,
                    format!(
                        "{}: {}: loop count out of range",
                        program(argv, "loop"),
                        String::from_utf8_lossy(argv.get(1).map_or(&b""[..], Vec::as_slice))
                    ),
                );
                // Record the failure so the §2.8.1 table decides what happens
                // next. Without this the diagnostic was printed, the status
                // was non-zero, and nothing acted on it: `while true; do break
                // oops; done` printed forever instead of exiting.
                interp.state.record_error(ShellError::SpecialBuiltin {
                    status: Status::FAILURE,
                    via_command: false,
                });
                return Status::FAILURE;
            }
            Some(count) => count.min(u32::from(u8::MAX)),
        },
    };
    let level = requested.clamp(1, u32::from(u8::MAX)) as u8;
    interp.pending = Some(if break_it {
        Unwind::Break(level)
    } else {
        Unwind::Continue(level)
    });
    Status::SUCCESS
}

fn bi_break(interp: &mut Interp, argv: &[Vec<u8>]) -> Status {
    loop_control(interp, argv, true)
}

fn bi_continue(interp: &mut Interp, argv: &[Vec<u8>]) -> Status {
    loop_control(interp, argv, false)
}

fn bi_return(interp: &mut Interp, argv: &[Vec<u8>]) -> Status {
    let status = match argv.get(1) {
        None => interp.state.vars.last_status(),
        Some(_) => match count(argv, 1) {
            Some(value) => Status(value as i32 & 0xff),
            None => {
                diagnose(
                    interp,
                    format!(
                        "return: {}: numeric argument required",
                        String::from_utf8_lossy(argv.get(1).map_or(&b""[..], Vec::as_slice))
                    ),
                );
                Status(2)
            }
        },
    };
    interp.pending = Some(Unwind::Return(status));
    Status::SUCCESS
}

fn bi_eval(interp: &mut Interp, argv: &[Vec<u8>]) -> Status {
    let mut source = Vec::new();
    // `eval -- string` is the same as `eval string`: `--` ends option parsing
    // and is not part of the code.
    let arguments = if argv.get(1).map(Vec::as_slice) == Some(b"--") {
        &argv[2..]
    } else {
        &argv[1..]
    };
    for (index, argument) in arguments.iter().enumerate() {
        if index > 0 {
            source.push(b' ');
        }
        source.extend_from_slice(argument);
    }
    let unwind = interp.run_chunk_unwind("<eval>", &source);
    propagate(interp, unwind, false)
}

/// Hand a nested chunk's unwind to its caller.
///
/// The command dispatcher takes `pending` right after a built-in returns, so
/// setting it is how a `return` inside `eval` returns from the enclosing
/// function and how `break`/`continue` reach the enclosing loop. `source`
/// absorbs `return` itself (`absorb_return`): a `return` in a sourced file ends
/// the file, not the caller's function.
fn propagate(interp: &mut Interp, unwind: Unwind, absorb_return: bool) -> Status {
    match unwind {
        Unwind::Status(status) => status,
        Unwind::Return(status) if absorb_return => status,
        Unwind::Break(_) | Unwind::Continue(_) | Unwind::Return(_) => {
            let status = unwind.status();
            interp.pending = Some(unwind);
            status
        }
        other => interp.absorb(other),
    }
}

fn bi_source(interp: &mut Interp, argv: &[Vec<u8>]) -> Status {
    let name = program(argv, "source");
    // `source -- file`: bash runs its built-ins through a getopt pass, so a
    // `--` ends the (empty) option list and is not a filename.
    let mut first = 1;
    if argv.get(first).map(Vec::as_slice) == Some(b"--") {
        first += 1;
    }
    let Some(file) = argv.get(first).cloned() else {
        diagnose(interp, format!("{name}: filename argument required"));
        return Status(2);
    };
    let path = source_path(interp, &file).unwrap_or(file.clone());
    let bytes = match std::fs::read(baash_sys::os::path_from_bytes(&path)) {
        Ok(bytes) => bytes,
        Err(error) => {
            diagnose(
                interp,
                format!("{name}: {}: {error}", String::from_utf8_lossy(&path)),
            );
            return Status::FAILURE;
        }
    };
    // A sourced file is script text like any other: a byte order mark or
    // `\r\n` line endings are the file's, not the parser's, and are settled
    // the same way `baash script` settles them (`baash_syntax::decode`).
    let bytes = baash_syntax::decode(&bytes).into_owned();
    let display = String::from_utf8_lossy(&path).into_owned();
    // A sourced file is a stack frame named `source`, and `BASH_LINENO`
    // records where the `.` command ran (`Locations when sourcing`).
    let line = interp.line_number().max(0) as usize;
    interp.push_call(b"source", &path, line);
    let unwind = if argv.len() > first + 1 {
        // `source file a b` gives the sourced file its own positional
        // parameters; a frame is the cheapest way to scope them.
        interp.state.vars.push_frame();
        interp.state.vars.set_positional(argv[first + 1..].to_vec());
        let unwind = interp.run_chunk_unwind(&display, &bytes);
        let _ = interp.state.vars.pop_frame();
        unwind
    } else {
        interp.run_chunk_unwind(&display, &bytes)
    };
    let status = propagate(interp, unwind, true);
    interp.pop_call();
    // `RETURN` runs when a script executed by `.`/`source` finishes; its status
    // does not replace the script's (bash keeps `return 42` as the status).
    if let Some(exit) = interp.run_return_trap() {
        interp.state.request_exit(exit);
    }
    status
}

/// The file `.`/`source` reads for `file`.
///
/// POSIX XCU 2.14.1: a name containing a slash is used as it stands, and
/// otherwise `PATH` is searched in order. A `PATH` element is a *directory*,
/// so an entry that happens to be a directory (`_tmp/dir/hello.sh` in the
/// suite) does not count as the file, and the empty element is the current
/// directory. bash then tries the current directory itself outside posix mode,
/// which is why `. cmd` finds `./cmd` when `PATH` has no match.
fn source_path(interp: &Interp, file: &[u8]) -> Option<Vec<u8>> {
    if file.contains(&b'/') {
        return regular_file(file).then(|| file.to_vec());
    }
    if let Some(path) = interp.state.vars.scalar(b"PATH") {
        for element in path.split(|&byte| byte == baash_sys::PATH_SEPARATOR) {
            let mut candidate = element.to_vec();
            if !element.is_empty() {
                candidate.push(baash_sys::PATH_JOIN);
            }
            candidate.extend_from_slice(file);
            if regular_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    regular_file(file).then(|| file.to_vec())
}

/// Whether `path` names a readable regular file (never a directory).
fn regular_file(path: &[u8]) -> bool {
    std::fs::metadata(baash_sys::os::path_from_bytes(path)).is_ok_and(|meta| meta.is_file())
}

/// `echo`, with bash's `-n`/`-e`/`-E` handling.
fn bi_echo(interp: &mut Interp, argv: &[Vec<u8>]) -> Status {
    let mut newline = true;
    let mut escapes = false;
    let mut index = 1;
    while let Some(option) = argv.get(index) {
        let flags = option.strip_prefix(b"-").unwrap_or_default();
        if flags.is_empty() || !flags.iter().all(|flag| matches!(flag, b'n' | b'e' | b'E')) {
            break;
        }
        for &flag in flags {
            match flag {
                b'n' => newline = false,
                b'e' => escapes = true,
                _ => escapes = false,
            }
        }
        index += 1;
    }

    let mut out = Vec::new();
    for (position, argument) in argv[index..].iter().enumerate() {
        if position > 0 {
            out.push(b' ');
        }
        if escapes && !interpret_escapes(argument, &mut out) {
            // `\c` drops the rest of the output, including the newline.
            return interp.write_out(&out);
        } else if !escapes {
            out.extend_from_slice(argument);
        }
    }
    if newline {
        out.push(b'\n');
    }
    interp.write_out(&out)
}

/// Decode `echo -e` escapes into `out`. Returns false on `\c`.
fn interpret_escapes(bytes: &[u8], out: &mut Vec<u8>) -> bool {
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' || index + 1 >= bytes.len() {
            out.push(bytes[index]);
            index += 1;
            continue;
        }
        index += 1;
        match bytes[index] {
            b'a' => {
                out.push(7);
                index += 1;
            }
            b'b' => {
                out.push(8);
                index += 1;
            }
            b'c' => return false,
            b'e' | b'E' => {
                out.push(27);
                index += 1;
            }
            b'f' => {
                out.push(12);
                index += 1;
            }
            b'n' => {
                out.push(b'\n');
                index += 1;
            }
            b'r' => {
                out.push(b'\r');
                index += 1;
            }
            b't' => {
                out.push(b'\t');
                index += 1;
            }
            b'v' => {
                out.push(11);
                index += 1;
            }
            b'\\' => {
                out.push(b'\\');
                index += 1;
            }
            b'0' => {
                index += 1;
                let start = index;
                let limit = (start + 3).min(bytes.len());
                let mut value: u32 = 0;
                while index < limit && (b'0'..=b'7').contains(&bytes[index]) {
                    value = value * 8 + u32::from(bytes[index] - b'0');
                    index += 1;
                }
                out.push((value & 0xff) as u8);
            }
            b'x' => {
                index += 1;
                let start = index;
                let limit = (start + 2).min(bytes.len());
                let mut value: u32 = 0;
                while index < limit {
                    let Some(digit) = (bytes[index] as char).to_digit(16) else {
                        break;
                    };
                    value = value * 16 + digit;
                    index += 1;
                }
                if index == start {
                    out.extend_from_slice(b"\\x");
                } else {
                    out.push((value & 0xff) as u8);
                }
            }
            // `\uHHHH` and `\UHHHHHHHH`: up to four or eight hex digits,
            // encoded as UTF-8. A value that is not a Unicode scalar (a
            // surrogate, or past `U+10FFFF`) and an escape with no digits both
            // stay literal, as in bash.
            b'u' | b'U' => {
                let max = if bytes[index] == b'u' { 4 } else { 8 };
                let start = index + 1;
                let limit = (start + max).min(bytes.len());
                let mut end = start;
                while end < limit && bytes[end].is_ascii_hexdigit() {
                    end += 1;
                }
                let value = if end == start {
                    None
                } else {
                    std::str::from_utf8(&bytes[start..end])
                        .ok()
                        .and_then(|digits| u32::from_str_radix(digits, 16).ok())
                        .and_then(char::from_u32)
                };
                match value {
                    Some(character) => {
                        let mut buffer = [0u8; 4];
                        out.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
                        index = end;
                    }
                    None => {
                        out.push(b'\\');
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            other => {
                out.push(b'\\');
                out.push(other);
                index += 1;
            }
        }
    }
    true
}

fn bi_cd(interp: &mut Interp, argv: &[Vec<u8>]) -> Status {
    let name = program(argv, "cd");
    let mut target: Option<Vec<u8>> = None;
    let mut physical = false;
    for argument in &argv[1..] {
        match argument.as_slice() {
            b"-L" => physical = false,
            b"-P" => physical = true,
            // bash's `-e` (and `-@`) only matter with `-P`, where they forbid
            // falling back to a guess when `getcwd()` fails; they are accepted
            // so a script using them does not stop here.
            b"-e" | b"-@" | b"--" => {}
            b"-" => target = Some(b"-".to_vec()),
            other if other.first() == Some(&b'-') && other.len() > 1 => {
                diagnose(interp, format!("{name}: {other:?}: invalid option"));
                return Status(2);
            }
            other => {
                if target.is_some() {
                    // bash reports a usage error, not a failure of the change.
                    diagnose(interp, format!("{name}: too many arguments"));
                    return Status(2);
                }
                target = Some(other.to_vec());
            }
        }
    }
    let mut print = false;
    let mut target = match target {
        None => match interp.state.vars.scalar(b"HOME") {
            Some(home) => home.to_vec(),
            None => {
                diagnose(interp, format!("{name}: HOME not set"));
                return Status::FAILURE;
            }
        },
        Some(path) if path == b"-" => {
            print = true;
            match interp.state.vars.scalar(b"OLDPWD") {
                Some(previous) => previous.to_vec(),
                None => {
                    diagnose(interp, format!("{name}: OLDPWD not set"));
                    return Status::FAILURE;
                }
            }
        }
        Some(path) => path,
    };

    // `CDPATH` is a search path for a bare relative name: bash tries each
    // element in order, and prints the new directory when the match came from
    // a non-empty element (the empty one is the current directory, which the
    // user already sees).
    if let Some((found, from_search)) = search_cdpath(interp, &target) {
        target = found;
        print |= from_search;
    }

    // A logical `cd` validates the components it names before entering them;
    // `-P` leaves that to the kernel's own path walk.
    if !physical {
        match super::dirs::check_components(interp, &target) {
            Ok(()) => {}
            Err(error) => {
                diagnose(
                    interp,
                    format!("{name}: {}: {error}", String::from_utf8_lossy(&target)),
                );
                return Status::FAILURE;
            }
        }
    }
    let current = match super::dirs::enter(interp, &target, physical) {
        Ok(current) => current,
        Err(error) => {
            diagnose(
                interp,
                format!("{name}: {}: {error}", String::from_utf8_lossy(&target)),
            );
            return Status::FAILURE;
        }
    };
    // `cd` replaces the current directory on the stack; the pushed entries
    // stay where they are.
    super::dirs::sync(interp, &current);
    if print {
        let mut out = current;
        out.push(b'\n');
        let _ = interp.write_out(&out);
    }
    Status::SUCCESS
}

/// The `CDPATH` match for `target`, and whether it came from a non-empty
/// element (which is what makes `cd` print the new directory).
///
/// bash only searches for a name that is neither absolute nor anchored to the
/// current directory (`./`/`../`), and falls back to the original operand when
/// nothing matches.
fn search_cdpath(interp: &Interp, target: &[u8]) -> Option<(Vec<u8>, bool)> {
    if target.first() == Some(&b'/') || target.starts_with(b"./") || target.starts_with(b"../") {
        return None;
    }
    let path = interp.state.vars.scalar(b"CDPATH")?;
    if path.is_empty() {
        return None;
    }
    for element in path.split(|&byte| byte == baash_sys::PATH_SEPARATOR) {
        let mut candidate = Vec::with_capacity(element.len() + target.len() + 1);
        candidate.extend_from_slice(element);
        if !element.is_empty() {
            candidate.push(baash_sys::PATH_JOIN);
        }
        candidate.extend_from_slice(target);
        let probe = baash_sys::os::path_from_bytes(&candidate);
        if probe.is_dir() {
            return Some((candidate, !element.is_empty()));
        }
    }
    None
}

/// Split `name=value` / `name+=value` into its parts.
fn split_assignment(operand: &[u8]) -> (&[u8], Option<&[u8]>, bool) {
    let Some(equals) = operand.iter().position(|&byte| byte == b'=') else {
        return (operand, None, false);
    };
    let raw = &operand[..equals];
    match raw.strip_suffix(b"+") {
        Some(name) => (name, Some(&operand[equals + 1..]), true),
        None => (raw, Some(&operand[equals + 1..]), false),
    }
}

/// `local`, `declare` and `typeset`.
///
/// Flags: `-a -A -i -n -r -x -p -f -F -g` and their `+` forms (`+n`, `+r`,
/// `+x`, `+i` clear the attribute). `-p` prints declarations, `-f` prints
/// function definitions, `-F` lists function names, `-g` looks in the global
/// scope, and a bare `declare`/`local` lists `name=value`.
///
/// `local` and `declare` differ only in the scope a *new* variable gets: local
/// always writes the innermost frame, declare writes the innermost frame when
/// one is open (bash's dynamic scope) and the global scope otherwise.
fn declare(interp: &mut Interp, argv: &[Vec<u8>], local_only: bool) -> Status {
    let name = program(argv, "declare");
    let mut flags = DeclFlags::default();
    let mut index = 1;
    while index < argv.len() {
        let argument = &argv[index];
        if argument == b"--" {
            index += 1;
            break;
        }
        let (on, flags_text) = match argument.first() {
            Some(b'-') => (true, &argument[1..]),
            Some(b'+') => (false, &argument[1..]),
            _ => break,
        };
        if flags_text.is_empty() {
            break;
        }
        for &flag in flags_text {
            let ok = match flag {
                b'a' if on => {
                    flags.indexed = Some(true);
                    true
                }
                b'A' if on => {
                    flags.associative = Some(true);
                    true
                }
                b'x' => {
                    flags.export = Some(on);
                    true
                }
                b'r' => {
                    flags.readonly = Some(on);
                    true
                }
                b'i' => {
                    flags.integer = Some(on);
                    true
                }
                b'n' => {
                    flags.nameref = Some(on);
                    true
                }
                b'p' if on => {
                    flags.print = true;
                    true
                }
                b'f' if on => {
                    flags.functions = true;
                    true
                }
                b'F' if on => {
                    flags.function_names = true;
                    true
                }
                b'g' if on => {
                    flags.global = true;
                    true
                }
                _ => false,
            };
            if !ok {
                // `+a`, `-A` with `+` and the like are not declaration
                // operations this shell defines; bash reports an invalid
                // option too.
                diagnose(
                    interp,
                    format!(
                        "{name}: {}{}: invalid option",
                        if on { '-' } else { '+' },
                        flag as char
                    ),
                );
                return Status(2);
            }
        }
        index += 1;
    }
    let operands = &argv[index..];

    if flags.functions || flags.function_names {
        if operands.is_empty() {
            return list_functions(interp, flags.function_names);
        }
        return print_functions(interp, operands, flags.function_names);
    }

    if operands.is_empty() {
        return list_declarations(interp, &flags, local_only);
    }

    let strict = interp.state.opts.strict_nameref;
    let mut status = Status::SUCCESS;
    for operand in operands {
        if flags.print {
            if print_one_declaration(interp, operand, &flags, local_only) != Status::SUCCESS {
                status = Status::FAILURE;
            }
            continue;
        }
        if let Err(failure) = declare_operand(interp, operand, &flags, local_only, &name) {
            if failure.fatal || (strict && failure.nameref) {
                interp.state.request_exit(Status::FAILURE);
            }
            status = Status::FAILURE;
        }
    }
    status
}

/// How a declaration operand failed.
///
/// Two independent distinctions matter:
///
/// * `fatal` — an explicit invalid nameref value (`typeset -n ref='@'`) is a
///   hard error that stops the shell, while an *inherited* invalid value
///   (`ref=1; typeset -n ref`) is only reported and the shell keeps going.
///   bash treats both as status 1 without exiting; the suite pins the OSH
///   reading for the explicit form.
/// * `nameref` — any nameref validation failure is fatal under
///   `shopt -s strict_nameref`, which is that option's whole purpose.
struct DeclFailure {
    fatal: bool,
    nameref: bool,
}
impl DeclFailure {
    fn plain() -> Self {
        Self {
            fatal: false,
            nameref: false,
        }
    }
    fn nameref(fatal: bool) -> Self {
        Self {
            fatal,
            nameref: true,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct DeclFlags {
    indexed: Option<bool>,
    associative: Option<bool>,
    export: Option<bool>,
    readonly: Option<bool>,
    integer: Option<bool>,
    nameref: Option<bool>,
    print: bool,
    functions: bool,
    function_names: bool,
    global: bool,
}

impl DeclFlags {
    /// Whether a variable passes the attribute filter the flags describe.
    ///
    /// `declare -p` with no names lists everything; with `-n`/`-r`/`-x`/`-a`/
    /// `-A` it lists only matching variables, and it does that with names too
    /// (that is the OSH reading the suite pins for `declare -pn name`).
    fn keeps(&self, var: &baash_runtime::Var) -> bool {
        let check = |requested: Option<bool>, present: bool| match requested {
            Some(on) => on == present,
            None => true,
        };
        check(self.nameref, var.attrs.nameref)
            && check(self.readonly, var.attrs.readonly)
            && check(self.export, var.attrs.exported)
            && check(self.integer, var.attrs.integer)
            && check(
                self.indexed,
                matches!(var.value, baash_runtime::VarValue::Indexed(_)),
            )
            && check(
                self.associative,
                matches!(var.value, baash_runtime::VarValue::Assoc(_)),
            )
    }

    fn has_attribute_filter(&self) -> bool {
        self.nameref.is_some()
            || self.readonly.is_some()
            || self.export.is_some()
            || self.integer.is_some()
            || self.indexed.is_some()
            || self.associative.is_some()
    }
}

/// The listing forms: `declare`, `declare -p`, `local`, `local -p`, and the
/// attribute filters (`declare -pn`, `-pa`, …).
fn list_declarations(interp: &mut Interp, flags: &DeclFlags, local_only: bool) -> Status {
    let declarations = flags.print || flags.has_attribute_filter();
    let mut out = Vec::new();
    {
        let vars = &interp.state.vars;
        let rows: Vec<(&[u8], &baash_runtime::Var)> = if local_only {
            vars.locals()
        } else if flags.global {
            vars.globals()
        } else {
            vars.visible()
        };
        for (variable, var) in rows {
            if !flags.keeps(var) {
                continue;
            }
            if declarations {
                baash_runtime::declare::render_declaration(&mut out, variable, var);
            } else {
                baash_runtime::declare::render_assignment(&mut out, variable, var);
            }
        }
    }
    interp.write_out(&out)
}

/// `declare -p name…`: print each declaration, and fail on a name that is not
/// a variable (or does not pass the attribute filter).
fn print_one_declaration(
    interp: &mut Interp,
    operand: &[u8],
    flags: &DeclFlags,
    local_only: bool,
) -> Status {
    // `declare -p foo=bar` is not a declaration and not a name; bash rejects
    // the whole operand.
    if operand.contains(&b'=') {
        diagnose(
            interp,
            format!(
                "declare: {}: not a valid identifier",
                String::from_utf8_lossy(operand)
            ),
        );
        return Status::FAILURE;
    }
    let var = if local_only {
        interp.state.vars.lookup_local(operand)
    } else if flags.global {
        interp.state.vars.lookup_global(operand)
    } else {
        interp.state.vars.lookup(operand)
    };
    let Some(var) = var else {
        diagnose(
            interp,
            format!("declare: {}: not found", String::from_utf8_lossy(operand)),
        );
        return Status::FAILURE;
    };
    if !flags.keeps(var) {
        return Status::FAILURE;
    }
    let mut out = Vec::new();
    baash_runtime::declare::render_declaration(&mut out, operand, var);
    let _ = interp.write_out(&out);
    Status::SUCCESS
}

/// `declare -f` (definitions) and `declare -F` (names).
fn print_functions(interp: &mut Interp, operands: &[Vec<u8>], names_only: bool) -> Status {
    let extdebug = interp.state.opts.extdebug;
    let mut status = Status::SUCCESS;
    let mut out = Vec::new();
    for operand in operands {
        let name = std::str::from_utf8(operand)
            .map(str::to_owned)
            .unwrap_or_else(|_| String::from_utf8_lossy(operand).into_owned());
        let Some(definition) = function_definition(interp, operand) else {
            diagnose(interp, format!("declare: {name}: not found"));
            status = Status::FAILURE;
            continue;
        };
        if names_only {
            if extdebug {
                let (line, file) = function_location(interp, operand).unwrap_or((0, String::new()));
                out.extend_from_slice(format!("{name} {line} {file}\n").as_bytes());
            } else {
                // `declare -F name` prints the bare name; the `declare -f`
                // prefix belongs to the listing form with no operands.
                out.extend_from_slice(operand);
                out.push(b'\n');
            }
        } else {
            out.extend_from_slice(&definition);
        }
    }
    if !out.is_empty() {
        let _ = interp.write_out(&out);
    }
    status
}

/// `declare -F`/`declare -f` with no operands: every function, sorted by name.
///
/// Only the bare name form is printed here — bash's `extdebug` line/file
/// column applies to `declare -F name`, not to the listing.
fn list_functions(interp: &mut Interp, names_only: bool) -> Status {
    let mut names: Vec<Vec<u8>> = interp
        .functions
        .iter()
        .map(|function| function.name.clone())
        .collect();
    names.sort();
    names.dedup();
    let mut out = Vec::new();
    for name in names {
        if names_only {
            out.extend_from_slice(b"declare -f ");
            out.extend_from_slice(&name);
            out.push(b'\n');
        } else if let Some(definition) = function_definition(interp, &name) {
            out.extend_from_slice(&definition);
        }
    }
    if out.is_empty() {
        return Status::SUCCESS;
    }
    interp.write_out(&out)
}

/// The source text of a function definition, re-input safe: `name ()` followed
/// by the body exactly as it was parsed.
fn function_definition(interp: &Interp, name: &[u8]) -> Option<Vec<u8>> {
    let index = interp.function_index(name)?;
    let function = interp.functions.get(index)?;
    let body = &function.chunk.source.bytes()[function.chunk.ast.node(function.body).span.clone()];
    let mut out = name.to_vec();
    out.extend_from_slice(b" ()\n");
    out.extend_from_slice(body);
    out.push(b'\n');
    Some(out)
}

/// The line and file of a function's definition, for `declare -F` under
/// `shopt -s extdebug`.
///
/// bash reports the line of the definition (the name, or the `{` of a
/// multi-line form) and the file it was parsed from, with `main` for standard
/// input — the suite compares that word, so stdin is reported as `main` the
/// way bash's debugger view does.
fn function_location(interp: &Interp, name: &[u8]) -> Option<(u32, String)> {
    let index = interp.function_index(name)?;
    let function = interp.functions.get(index)?;
    let source = &function.chunk.source;
    let bytes = source.bytes();
    let offset = definition_offset(bytes, function.chunk.ast.node(function.body).span.start);
    let (line, _) = source.line_col(offset);
    let file = match source.name() {
        "<stdin>" => "main".to_owned(),
        other => other.to_owned(),
    };
    Some((line, file))
}

/// Start offset of the line a function definition begins on.
///
/// The body node starts at `{`; the definition begins on the same line for
/// `f() { … }` and on the previous non-blank line for the common
/// `f()\n{ … }` layout.
fn definition_offset(bytes: &[u8], body_start: usize) -> usize {
    let line_start = bytes[..body_start]
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |newline| newline + 1);
    if bytes[line_start..body_start]
        .iter()
        .all(|byte| byte.is_ascii_whitespace())
    {
        return bytes[..line_start]
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |newline| newline + 1);
    }
    line_start
}

/// One operand of a declaration builtin.
fn declare_operand(
    interp: &mut Interp,
    operand: &[u8],
    flags: &DeclFlags,
    local_only: bool,
    name: &str,
) -> Result<(), DeclFailure> {
    let scope = if local_only {
        if interp.state.vars.frame_depth() == 0 {
            diagnose(interp, format!("{name}: can only be used in a function"));
            return Err(DeclFailure::plain());
        }
        CreateScope::Local
    } else if flags.global {
        CreateScope::Global
    } else {
        baash_runtime::declaration_scope(&interp.state.vars)
    };

    // `declare -a a=(1 2)`: the structured literal arrives on the side
    // channel, keyed by the raw text of this operand.
    if let Some(literal) = interp.take_array_literal(operand) {
        let variable = literal.name.clone();
        if !baash_runtime::Vars::is_name(&variable) {
            diagnose(
                interp,
                format!(
                    "{name}: {}: not a valid identifier",
                    String::from_utf8_lossy(&variable)
                ),
            );
            return Err(DeclFailure::plain());
        }
        // A nameref holds a *name*; assigning it an array would have to mean
        // "the target becomes an array", which bash refuses as a hard error.
        if interp
            .state
            .vars
            .lookup(&variable)
            .is_some_and(|var| var.attrs.nameref)
        {
            diagnose(
                interp,
                format!(
                    "{name}: {}: cannot assign array to nameref",
                    String::from_utf8_lossy(&variable)
                ),
            );
            return Err(DeclFailure::nameref(true));
        }
        let shape = if flags.associative == Some(true) {
            Some(baash_runtime::ArrayShape::Associative)
        } else if flags.indexed == Some(true) {
            Some(baash_runtime::ArrayShape::Indexed)
        } else {
            interp.state.vars.shape(&variable)
        };
        if scope == CreateScope::Local {
            if let Err(error) = interp.state.vars.declare_local(&variable) {
                diagnose(interp, format!("{name}: {}", error.message()));
                return Err(DeclFailure::plain());
            }
        }
        if let Some(shape) = shape {
            if let Err(error) = interp.state.vars.declare_shape(&variable, shape) {
                diagnose(interp, format!("{name}: {}", error.message()));
                return Err(DeclFailure::plain());
            }
        }
        if let Err(message) =
            interp.assign_array_literal(&variable, &literal.elements, literal.append, shape)
        {
            diagnose(interp, format!("{name}: {message}"));
            return Err(DeclFailure::plain());
        }
        apply_declaration_attrs(interp, &variable, flags).map_err(|_| DeclFailure::plain())?;
        return Ok(());
    }

    // `a[sub]=value`: a subscripted operand, which `declare` allows where a
    // plain assignment word would not be split.
    if let Some((variable, subscript, value, append)) = split_element_operand(operand) {
        if !baash_runtime::Vars::is_name(variable) {
            diagnose(
                interp,
                format!(
                    "{name}: {}: not a valid identifier",
                    String::from_utf8_lossy(variable)
                ),
            );
            return Err(DeclFailure::plain());
        }
        if scope == CreateScope::Local {
            if let Err(error) = interp.state.vars.declare_local(variable) {
                diagnose(interp, format!("{name}: {}", error.message()));
                return Err(DeclFailure::plain());
            }
        }
        // `declare a[]=x`: bash reports `a[]: bad array subscript`, but it has
        // already turned the name into an (empty) indexed array. The quoted
        // spelling `a[""]=x` is a legal write to element zero.
        if subscript.is_empty() {
            let shape = if flags.associative == Some(true) {
                baash_runtime::ArrayShape::Associative
            } else {
                baash_runtime::ArrayShape::Indexed
            };
            let _ = interp.state.vars.declare_shape(variable, shape);
            diagnose(
                interp,
                format!(
                    "{name}: {}[]: bad array subscript",
                    String::from_utf8_lossy(variable)
                ),
            );
            return Err(DeclFailure::plain());
        }
        if let Err(failure) = interp.assign_element(variable, subscript, append, value) {
            diagnose(interp, format!("{name}: {}", failure.message()));
            return Err(DeclFailure::plain());
        }
        apply_declaration_attrs(interp, variable, flags).map_err(|_| DeclFailure::plain())?;
        return Ok(());
    }

    let (variable, value, append) = split_assignment(operand);
    if !baash_runtime::Vars::is_name(variable) {
        diagnose(
            interp,
            format!(
                "{name}: {}: not a valid identifier",
                String::from_utf8_lossy(variable)
            ),
        );
        return Err(DeclFailure::plain());
    }
    if scope == CreateScope::Local {
        if let Err(error) = interp.state.vars.declare_local(variable) {
            diagnose(interp, format!("{name}: {}", error.message()));
            return Err(DeclFailure::plain());
        }
    }

    // `declare -n ref=target` sets a reference; every other operation on a
    // nameref cell must not follow it, or `declare -n r=y` would rewrite `x`.
    if flags.nameref == Some(true) {
        if append {
            diagnose(
                interp,
                format!(
                    "{name}: {}: cannot append to a name reference",
                    String::from_utf8_lossy(variable)
                ),
            );
            return Err(DeclFailure::nameref(true));
        }
        if let Err(error) = interp.state.vars.declare_nameref(variable, value, scope) {
            diagnose(interp, format!("{name}: {}", error.message()));
            // An explicit reference that cannot name a variable is a hard
            // error, except for a direct self-reference, which bash reports
            // and ignores. A bad value inherited from the cell is reported but
            // the shell keeps running too (bash returns 1 for all of these).
            let fatal =
                value.is_some() && !matches!(error, baash_runtime::Error::NamerefSelf { .. });
            return Err(DeclFailure::nameref(fatal));
        }
        apply_declaration_attrs(interp, variable, flags).map_err(|_| DeclFailure::plain())?;
        return Ok(());
    }
    if flags.nameref == Some(false) {
        if let Err(error) = interp.state.vars.unmark_nameref(variable) {
            diagnose(interp, format!("{name}: {}", error.message()));
            return Err(DeclFailure::plain());
        }
    }

    if flags.indexed == Some(true) || flags.associative == Some(true) {
        let shape = if flags.associative == Some(true) {
            baash_runtime::ArrayShape::Associative
        } else {
            baash_runtime::ArrayShape::Indexed
        };
        if let Err(error) = interp.state.vars.declare_shape(variable, shape) {
            diagnose(interp, format!("{name}: {}", error.message()));
            return Err(DeclFailure::plain());
        }
    }

    // `typeset +r r=2` clears the attribute *before* the assignment, which is
    // the only reading under which the assignment can succeed.
    if flags.readonly == Some(false) {
        if let Err(error) = interp.state.vars.unmark_readonly(variable) {
            diagnose(interp, format!("{name}: {}", error.message()));
            return Err(DeclFailure::plain());
        }
    }
    if flags.export == Some(false) {
        if let Err(error) = interp.state.vars.unmark_exported(variable) {
            diagnose(interp, format!("{name}: {}", error.message()));
            return Err(DeclFailure::plain());
        }
    }

    if let Some(value) = value {
        let integer = flags.integer == Some(true)
            || interp
                .state
                .vars
                .lookup(variable)
                .is_some_and(|var| var.attrs.integer);
        let result = if append {
            interp.state.vars.append(variable, value)
        } else if integer {
            match crate::arith::eval(value, &mut *interp) {
                Ok(number) => interp.state.vars.assign_scalar_in(
                    variable,
                    number.to_string().as_bytes(),
                    scope,
                ),
                Err(message) => {
                    diagnose(interp, format!("{name}: {message}"));
                    return Err(DeclFailure::plain());
                }
            }
        } else {
            interp.state.vars.assign_scalar_in(variable, value, scope)
        };
        if let Err(error) = result {
            diagnose(interp, format!("{name}: {}", error.message()));
            if matches!(error, baash_runtime::Error::Readonly { .. }) {
                interp
                    .state
                    .record_error(baash_runtime::ShellError::VariableAssignment);
            }
            return Err(DeclFailure::plain());
        }
    } else if flags.indexed.is_none()
        && flags.associative.is_none()
        && flags.nameref.is_none()
        && flags.export.is_none()
        && flags.readonly.is_none()
        && flags.integer.is_none()
    {
        // `declare x` declares an unset variable; that is observable through
        // `declare -p x` and `${x+set}`.
        if let Err(error) = interp.state.vars.declare_empty(variable, scope) {
            diagnose(interp, format!("{name}: {}", error.message()));
            return Err(DeclFailure::plain());
        }
    } else if let Err(error) = interp.state.vars.declare_empty(variable, scope) {
        // `declare -x x` and friends also create the variable.
        diagnose(interp, format!("{name}: {}", error.message()));
        return Err(DeclFailure::plain());
    }
    apply_declaration_attrs(interp, variable, flags).map_err(|_| DeclFailure::plain())
}

/// Apply the attribute flags, including their `+` forms.
fn apply_declaration_attrs(
    interp: &mut Interp,
    variable: &[u8],
    flags: &DeclFlags,
) -> Result<(), baash_runtime::Error> {
    if let Some(export) = flags.export {
        if export {
            interp.state.vars.mark_exported(variable)?;
        } else {
            interp.state.vars.unmark_exported(variable)?;
        }
    } else if interp.state.opts.allexport {
        // `set -a` exports everything assigned, including `local` and
        // `declare` variables; an explicit `+x` above still wins.
        interp.state.vars.mark_exported(variable)?;
    }
    if let Some(readonly) = flags.readonly {
        if readonly {
            interp.state.vars.mark_readonly(variable)?;
        } else {
            interp.state.vars.unmark_readonly(variable)?;
        }
    }
    if let Some(integer) = flags.integer {
        if integer {
            interp.state.vars.mark_integer(variable)?;
        } else {
            interp.state.vars.unmark_integer(variable)?;
        }
    }
    Ok(())
}

/// `(name, subscript, value, append)` from a `name[sub]=value` operand.
type ElementOperand<'a> = (&'a [u8], &'a [u8], &'a [u8], bool);

/// Split `name[sub]=value` / `name[sub]+=value` for a declaration operand.
fn split_element_operand(operand: &[u8]) -> Option<ElementOperand<'_>> {
    let open = operand.iter().position(|&byte| byte == b'[')?;
    let mut depth = 0usize;
    let mut close = None;
    for (index, &byte) in operand.iter().enumerate().skip(open) {
        match byte {
            b'[' => depth += 1,
            b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    close = Some(index);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;
    let append = operand.get(close + 1) == Some(&b'+');
    let equals = if append { close + 2 } else { close + 1 };
    if operand.get(equals) != Some(&b'=') {
        return None;
    }
    Some((
        &operand[..open],
        &operand[open + 1..close],
        &operand[equals + 1..],
        append,
    ))
}

/// `hash`: the `PATH` table the executor resolves commands against.
///
/// The table itself is [`Interp`]'s (`path_cache`), which is why this body is
/// here and not in `baash-runtime`: `hash` exists to mutate executor state, and
/// its whole observable effect is which path a later command word runs.
fn bi_hash(interp: &mut Interp, argv: &[Vec<u8>]) -> Status {
    let name = program(argv, "hash");
    let mut clear = false;
    let mut reusable = false;
    let mut forget: Vec<Vec<u8>> = Vec::new();
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
                b'r' => {
                    clear = true;
                    position += 1;
                }
                b'l' => {
                    reusable = true;
                    position += 1;
                }
                b'd' | b'p' => {
                    // `-dname` / `-d name`, and `-p pathname name`.
                    let value = if position + 1 < argument.len() {
                        argument[position + 1..].to_vec()
                    } else {
                        index += 1;
                        match argv.get(index) {
                            Some(value) => value.clone(),
                            None => {
                                diagnose(
                                    interp,
                                    format!(
                                        "{name}: -{}: option requires an argument",
                                        flag as char
                                    ),
                                );
                                return Status(2);
                            }
                        }
                    };
                    if flag == b'd' {
                        forget.push(value);
                    } else {
                        index += 1;
                        match argv.get(index) {
                            Some(target) => {
                                interp.path_cache.insert(target.clone(), value);
                            }
                            None => {
                                diagnose(interp, format!("{name}: -p: name required"));
                                return Status(2);
                            }
                        }
                    }
                    position = argument.len();
                }
                other => {
                    diagnose(
                        interp,
                        format!("{name}: -{}: invalid option", other as char),
                    );
                    return Status(2);
                }
            }
        }
        index += 1;
    }

    let operands: Vec<Vec<u8>> = argv[index.min(argv.len())..].to_vec();
    if clear {
        if !operands.is_empty() {
            // bash refuses `hash -r name`; POSIX has no say, and the suite
            // accepts either status, so this reports the usage error.
            diagnose(interp, format!("{name}: -r: cannot be used with names"));
            return Status::FAILURE;
        }
        interp.path_cache.clear();
    }
    for target in forget {
        interp.path_cache.remove(&target);
    }

    if operands.is_empty() {
        let mut out = Vec::new();
        for (target, path) in &interp.path_cache {
            if reusable {
                out.extend_from_slice(b"builtin hash -p ");
                out.extend_from_slice(path);
                out.push(b' ');
                out.extend_from_slice(target);
                out.push(b'\n');
            } else {
                out.extend_from_slice(path);
                out.push(b'\n');
            }
        }
        return interp.write_out(&out);
    }

    let mut status = Status::SUCCESS;
    for target in operands {
        // `hash name` is a *search*: it must bypass the table, or a stale entry
        // would make it a no-op after `PATH` changed.
        let query = baash_runtime::ResolveQuery {
            fresh: true,
            ..baash_runtime::ResolveQuery::EXECUTION
        };
        match interp.path_hits_for_hash(&target, query) {
            Some(path) => {
                interp.path_cache.insert(target, path);
            }
            None => {
                diagnose(
                    interp,
                    format!("{name}: {}: not found", String::from_utf8_lossy(&target)),
                );
                status = Status::FAILURE;
            }
        }
    }
    status
}

fn bi_local(interp: &mut Interp, argv: &[Vec<u8>]) -> Status {
    declare(interp, argv, true)
}

fn bi_declare(interp: &mut Interp, argv: &[Vec<u8>]) -> Status {
    declare(interp, argv, false)
}

#[cfg(test)]
mod tests {
    use baash_runtime::{Attrs, Var, VarValue};

    use super::split_element_operand;

    #[test]
    fn element_operand_splits_only_assignment_shapes() {
        let (name, subscript, value, append) =
            split_element_operand(b"a[i+1]=v").expect("an element operand");
        assert_eq!(name, b"a");
        assert_eq!(subscript, b"i+1");
        assert_eq!(value, b"v");
        assert!(!append);

        let (_, _, value, append) = split_element_operand(b"a[k]+=v").expect("append");
        assert_eq!(value, b"v");
        assert!(append);

        assert!(split_element_operand(b"a=1").is_none());
        assert!(split_element_operand(b"a[0]").is_none());
        assert_eq!(
            split_element_operand(b"m[a[b]]=v").map(|parts| parts.1.to_vec()),
            Some(b"a[b]".to_vec())
        );
    }

    #[test]
    fn rendered_flags_cover_every_attribute() {
        let var = Var {
            value: VarValue::Str(b"1".to_vec()),
            attrs: Attrs {
                nameref: true,
                readonly: true,
                exported: true,
                integer: true,
            },
        };
        let mut out = Vec::new();
        baash_runtime::declare::render_flags(&mut out, &var);
        assert_eq!(String::from_utf8(out).expect("ASCII"), "-nrxi");
    }
}
