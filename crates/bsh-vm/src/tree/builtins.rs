//! Executor-side built-ins.
//!
//! `bsh-runtime` owns the builtin registry, the tiers and every body that only
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
//! (`BuiltinDef::is_implemented()` is false), so when `bsh-runtime` fills one
//! in this module goes dormant instead of shadowing it. That rule is what keeps
//! the ownership boundary honest.
//!
//! `echo` is here because a shell that forks `/bin/echo` for the most common
//! command in existence is not an interactive shell, and because `echo -e` /
//! `echo -n` are *bash* behaviour, not the coreutils behaviour a `PATH` hit
//! would give. It stops being used the moment `bsh-runtime` writes its body.
//!
//! Three more bodies are here because their effect *is* executor state, not
//! shell state: `hash` (which path a later command word runs), and
//! `mapfile`/`readarray` (the descriptor table and the per-descriptor read
//! buffer that `read` already owns). Their registry entries stay stubs, which
//! is the documented mechanism for "this body belongs to the executor".
//!
//! Deliberately *not* here: `printf`, `test`, `[` and `pwd` are regular
//! utilities in POSIX terms — they must remain exec-able, their program forms
//! are correct, and they are `bsh-runtime`'s bodies to write. Until it writes
//! them, the `PATH` position serves them, which is exactly where POSIX puts a
//! regular built-in.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use bsh_diag::Diagnostic;
use bsh_runtime::{CreateScope, ShellError, ShellHost, Status};

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
        name: b"hash",
        run: bi_hash,
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

fn program(argv: &[Vec<u8>], fallback: &str) -> String {
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
    for (index, argument) in argv[1..].iter().enumerate() {
        if index > 0 {
            source.push(b' ');
        }
        source.extend_from_slice(argument);
    }
    interp.run_named("<eval>", &source)
}

fn bi_source(interp: &mut Interp, argv: &[Vec<u8>]) -> Status {
    let name = program(argv, "source");
    let Some(file) = argv.get(1).cloned() else {
        diagnose(interp, format!("{name}: filename argument required"));
        return Status(2);
    };
    let path = if file.contains(&b'/') {
        file.clone()
    } else {
        interp.search_path(&file).unwrap_or_else(|| file.clone())
    };
    let bytes = match std::fs::read(std::path::Path::new(OsStr::from_bytes(&path))) {
        Ok(bytes) => bytes,
        Err(error) => {
            diagnose(
                interp,
                format!("{name}: {}: {error}", String::from_utf8_lossy(&path)),
            );
            return Status::FAILURE;
        }
    };
    let display = String::from_utf8_lossy(&path).into_owned();
    let status = if argv.len() > 2 {
        // `source file a b` gives the sourced file its own positional
        // parameters; a frame is the cheapest way to scope them.
        interp.state.vars.push_frame();
        interp.state.vars.set_positional(argv[2..].to_vec());
        let status = interp.run_named(&display, &bytes);
        let _ = interp.state.vars.pop_frame();
        status
    } else {
        interp.run_named(&display, &bytes)
    };
    // `RETURN` runs when a script executed by `.`/`source` finishes; its status
    // does not replace the script's (bash keeps `return 42` as the status).
    if let Some(exit) = interp.run_return_trap() {
        interp.state.request_exit(exit);
    }
    status
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
    for argument in &argv[1..] {
        match argument.as_slice() {
            b"-L" | b"-P" | b"--" => {}
            b"-" => target = Some(b"-".to_vec()),
            other if other.first() == Some(&b'-') && other.len() > 1 => {
                diagnose(interp, format!("{name}: {other:?}: invalid option"));
                return Status(2);
            }
            other => {
                if target.is_some() {
                    diagnose(interp, format!("{name}: too many arguments"));
                    return Status::FAILURE;
                }
                target = Some(other.to_vec());
            }
        }
    }
    let mut print = false;
    let target = match target {
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

    if let Err(error) = bsh_sys::chdir(&target) {
        diagnose(
            interp,
            format!("{name}: {}: {error}", String::from_utf8_lossy(&target)),
        );
        return Status::FAILURE;
    }
    let previous = interp.state.vars.scalar(b"PWD").map(<[u8]>::to_vec);
    let current = std::env::current_dir()
        .map(|dir| dir.as_os_str().as_bytes().to_vec())
        .unwrap_or_else(|_| target.clone());
    if let Some(previous) = previous {
        let _ = interp.state.vars.assign_scalar(b"OLDPWD", &previous);
        let _ = interp.state.vars.mark_exported(b"OLDPWD");
    }
    let _ = interp.state.vars.assign_scalar(b"PWD", &current);
    let _ = interp.state.vars.mark_exported(b"PWD");
    if print {
        let mut out = current;
        out.push(b'\n');
        let _ = interp.write_out(&out);
    }
    Status::SUCCESS
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
/// Supported flags: `-a -A -x -r -i -p` and `--`. `-f`/`-g` are diagnosed as
/// deferred rather than silently ignored.
fn declare(interp: &mut Interp, argv: &[Vec<u8>], local_only: bool) -> Status {
    let name = program(argv, "declare");
    let (mut indexed, mut associative) = (false, false);
    let (mut export, mut readonly, mut integer) = (false, false, false);
    let mut print = false;
    let mut index = 1;
    while index < argv.len() {
        let argument = &argv[index];
        if argument == b"--" {
            index += 1;
            break;
        }
        let Some(flags) = argument.strip_prefix(b"-") else {
            break;
        };
        if flags.is_empty() {
            break;
        }
        for &flag in flags {
            match flag {
                b'a' => indexed = true,
                b'A' => associative = true,
                b'x' => export = true,
                b'r' => readonly = true,
                b'i' => integer = true,
                b'p' => print = true,
                b'f' | b'g' => {
                    diagnose(
                        interp,
                        format!("{name}: -{}: not implemented yet", flag as char),
                    );
                    return Status(2);
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

    if index >= argv.len() {
        // Listing forms: `declare -p` (everything), `declare -a` (indexed
        // arrays), `declare -A` (associative arrays).
        if print || indexed || associative {
            let mut out = Vec::new();
            for (variable, var) in interp.state.vars.visible() {
                let keep = if print {
                    true
                } else if associative {
                    matches!(var.value, bsh_runtime::VarValue::Assoc(_))
                } else {
                    matches!(var.value, bsh_runtime::VarValue::Indexed(_))
                };
                if keep {
                    render_declaration(&mut out, variable, var);
                }
            }
            return interp.write_out(&out);
        }
        diagnose(interp, format!("{name}: no names given"));
        return Status::FAILURE;
    }

    let scope = if local_only {
        if interp.state.vars.frame_depth() == 0 {
            diagnose(interp, format!("{name}: can only be used in a function"));
            return Status::FAILURE;
        }
        CreateScope::Local
    } else {
        bsh_runtime::declaration_scope(&interp.state.vars)
    };

    let mut status = Status::SUCCESS;
    for operand in &argv[index..] {
        // `declare -p name` prints; it never assigns.
        if print {
            match interp.state.vars.lookup(operand) {
                Some(var) => {
                    let mut out = Vec::new();
                    render_declaration(&mut out, operand, var);
                    let _ = interp.write_out(&out);
                }
                None => {
                    diagnose(
                        interp,
                        format!("{name}: {}: not found", String::from_utf8_lossy(operand)),
                    );
                    status = Status::FAILURE;
                }
            }
            continue;
        }

        // `declare -a a=(1 2)`: the structured literal arrives on the side
        // channel, keyed by the raw text of this operand.
        if let Some(literal) = interp.take_array_literal(operand) {
            let variable = literal.name.clone();
            let shape = if associative {
                Some(bsh_runtime::ArrayShape::Associative)
            } else if indexed {
                Some(bsh_runtime::ArrayShape::Indexed)
            } else {
                interp.state.vars.shape(&variable)
            };
            if scope == CreateScope::Local {
                if let Err(error) = interp.state.vars.declare_local(&variable) {
                    diagnose(interp, format!("{name}: {}", error.message()));
                    status = Status::FAILURE;
                    continue;
                }
            }
            if let Some(shape) = shape {
                if let Err(error) = interp.state.vars.declare_shape(&variable, shape) {
                    diagnose(interp, format!("{name}: {}", error.message()));
                    status = Status::FAILURE;
                    continue;
                }
            }
            if let Err(message) =
                interp.assign_array_literal(&variable, &literal.elements, literal.append, shape)
            {
                diagnose(interp, format!("{name}: {message}"));
                status = Status::FAILURE;
                continue;
            }
            if let Err(error) =
                apply_declaration_attrs(interp, &variable, export, readonly, integer)
            {
                diagnose(interp, format!("{name}: {}", error.message()));
                status = Status::FAILURE;
            }
            continue;
        }

        // `a[sub]=value`: a subscripted operand, which `declare` allows where a
        // plain assignment word would not be split.
        if let Some((variable, subscript, value, append)) = split_element_operand(operand) {
            if !bsh_runtime::Vars::is_name(variable) {
                diagnose(
                    interp,
                    format!(
                        "{name}: {}: not a valid identifier",
                        String::from_utf8_lossy(variable)
                    ),
                );
                status = Status::FAILURE;
                continue;
            }
            if scope == CreateScope::Local {
                if let Err(error) = interp.state.vars.declare_local(variable) {
                    diagnose(interp, format!("{name}: {}", error.message()));
                    status = Status::FAILURE;
                    continue;
                }
            }
            // `declare a[]=x`: bash reports `a[]: bad array subscript`, but it
            // has already turned the name into an (empty) indexed array. The
            // quoted spelling `a[""]=x` is a legal write to element zero.
            if subscript.is_empty() {
                let shape = if associative {
                    bsh_runtime::ArrayShape::Associative
                } else {
                    bsh_runtime::ArrayShape::Indexed
                };
                let _ = interp.state.vars.declare_shape(variable, shape);
                diagnose(
                    interp,
                    format!(
                        "{name}: {}[]: bad array subscript",
                        String::from_utf8_lossy(variable)
                    ),
                );
                status = Status::FAILURE;
                continue;
            }
            if let Err(message) = interp.assign_element(variable, subscript, append, value) {
                diagnose(interp, format!("{name}: {message}"));
                status = Status::FAILURE;
                continue;
            }
            if let Err(error) = apply_declaration_attrs(interp, variable, export, readonly, integer)
            {
                diagnose(interp, format!("{name}: {}", error.message()));
                status = Status::FAILURE;
            }
            continue;
        }

        let (variable, value, append) = split_assignment(operand);
        if !bsh_runtime::Vars::is_name(variable) {
            diagnose(
                interp,
                format!(
                    "{name}: {}: not a valid identifier",
                    String::from_utf8_lossy(variable)
                ),
            );
            status = Status::FAILURE;
            continue;
        }
        if scope == CreateScope::Local {
            if let Err(error) = interp.state.vars.declare_local(variable) {
                diagnose(interp, format!("{name}: {}", error.message()));
                status = Status::FAILURE;
                continue;
            }
        }
        if indexed || associative {
            if let Err(error) = interp.state.vars.declare_shape(
                variable,
                if associative {
                    bsh_runtime::ArrayShape::Associative
                } else {
                    bsh_runtime::ArrayShape::Indexed
                },
            ) {
                diagnose(interp, format!("{name}: {}", error.message()));
                status = Status::FAILURE;
                continue;
            }
        }
        if let Some(value) = value {
            let result = if append {
                interp.state.vars.append(variable, value)
            } else {
                interp.state.vars.assign_scalar_in(variable, value, scope)
            };
            if let Err(error) = result {
                diagnose(interp, format!("{name}: {}", error.message()));
                status = Status::FAILURE;
                continue;
            }
        }
        if let Err(error) = apply_declaration_attrs(interp, variable, export, readonly, integer) {
            diagnose(interp, format!("{name}: {}", error.message()));
            status = Status::FAILURE;
        }
    }
    status
}

/// Apply `-x`/`-r`/`-i` after a value has been written, so
/// `readonly -a a=(1 2)` can still fill the array first.
fn apply_declaration_attrs(
    interp: &mut Interp,
    variable: &[u8],
    export: bool,
    readonly: bool,
    integer: bool,
) -> Result<(), bsh_runtime::Error> {
    if export {
        interp.state.vars.mark_exported(variable)?;
    }
    if readonly {
        interp.state.vars.mark_readonly(variable)?;
    }
    if integer {
        interp.state.vars.mark_integer(variable)?;
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

/// Render one `declare -… name=value` line, re-input safe.
///
/// Array values are double-quoted and subscripts are printed explicitly, so
/// the output can be fed back to `declare` (bash's format). Scalar values stay
/// bare when they are safe, which is the format the suite expects for
/// `declare -- x=1`.
fn render_declaration(out: &mut Vec<u8>, name: &[u8], var: &bsh_runtime::Var) {
    out.extend_from_slice(b"declare -");
    let mut any = false;
    if var.attrs.readonly {
        out.push(b'r');
        any = true;
    }
    if var.attrs.exported {
        out.push(b'x');
        any = true;
    }
    if var.attrs.integer {
        out.push(b'i');
        any = true;
    }
    match &var.value {
        bsh_runtime::VarValue::Indexed(_) => {
            out.push(b'a');
            any = true;
        }
        bsh_runtime::VarValue::Assoc(_) => {
            out.push(b'A');
            any = true;
        }
        _ => {}
    }
    if !any {
        out.push(b'-');
    }
    out.push(b' ');
    out.extend_from_slice(name);
    match &var.value {
        bsh_runtime::VarValue::Unset => {}
        bsh_runtime::VarValue::Str(bytes) => {
            out.push(b'=');
            quote_scalar(out, bytes);
        }
        bsh_runtime::VarValue::Indexed(elements) => {
            out.extend_from_slice(b"=(");
            for (position, (index, value)) in elements.iter().enumerate() {
                if position > 0 {
                    out.push(b' ');
                }
                out.extend_from_slice(format!("[{index}]=").as_bytes());
                quote_double(out, value);
            }
            out.push(b')');
        }
        bsh_runtime::VarValue::Assoc(pairs) => {
            if pairs.is_empty() {
                out.extend_from_slice(b"=()");
                out.push(b'\n');
                return;
            }
            out.extend_from_slice(b"=(");
            for (position, (key, value)) in pairs.iter().enumerate() {
                if position > 0 {
                    out.push(b' ');
                }
                if is_bare_word(key) && !key.is_empty() {
                    out.push(b'[');
                    out.extend_from_slice(key);
                    out.extend_from_slice(b"]=");
                } else {
                    out.extend_from_slice(b"[");
                    quote_double(out, key);
                    out.extend_from_slice(b"]=");
                }
                quote_double(out, value);
            }
            // bash puts a space before the closing parenthesis for assoc
            // arrays; keeping it makes the output byte-identical to bash's.
            out.extend_from_slice(b" )");
        }
    }
    out.push(b'\n');
}

/// Double-quote bytes with the escaping a re-input-safe `declare -p` needs.
fn quote_double(out: &mut Vec<u8>, bytes: &[u8]) {
    out.push(b'"');
    for &byte in bytes {
        match byte {
            b'"' | b'\\' | b'$' | b'`' => {
                out.push(b'\\');
                out.push(byte);
            }
            _ => out.push(byte),
        }
    }
    out.push(b'"');
}

/// Quote a scalar only when it needs it (the suite's `declare -- x=1` form).
fn quote_scalar(out: &mut Vec<u8>, bytes: &[u8]) {
    if is_bare_word(bytes) || bytes.is_empty() {
        out.extend_from_slice(bytes);
    } else {
        quote_double(out, bytes);
    }
}

/// Bytes that need no quoting in `declare -p` output.
fn is_bare_word(bytes: &[u8]) -> bool {
    bytes.iter().all(|&byte| {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'_' | b'-' | b'.' | b'/' | b':' | b'@' | b'%' | b'+' | b',' | b'='
            )
    })
}

/// `hash`: the `PATH` table the executor resolves commands against.
///
/// The table itself is [`Interp`]'s (`path_cache`), which is why this body is
/// here and not in `bsh-runtime`: `hash` exists to mutate executor state, and
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
        let query = bsh_runtime::ResolveQuery {
            fresh: true,
            ..bsh_runtime::ResolveQuery::EXECUTION
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
    use bsh_runtime::{Attrs, Var, VarValue};

    use super::render_declaration;

    fn render(var: &Var) -> String {
        let mut out = Vec::new();
        render_declaration(&mut out, b"v", var);
        String::from_utf8(out).expect("ASCII output")
    }

    #[test]
    fn declare_p_renders_reinput_safe_arrays() {
        let indexed = Var {
            value: VarValue::Indexed(
                [(0, b"1".to_vec()), (2, b"2 3".to_vec())]
                    .into_iter()
                    .collect(),
            ),
            attrs: Attrs::default(),
        };
        assert_eq!(render(&indexed), "declare -a v=([0]=\"1\" [2]=\"2 3\")\n");

        let assoc = Var {
            value: VarValue::Assoc(vec![
                (b"k".to_vec(), b"v".to_vec()),
                (b"x y".to_vec(), b"a\"b".to_vec()),
            ]),
            attrs: Attrs::default(),
        };
        assert_eq!(
            render(&assoc),
            "declare -A v=([k]=\"v\" [\"x y\"]=\"a\\\"b\" )\n"
        );

        // Empty arrays and attribute order.
        let empty = Var {
            value: VarValue::Assoc(Vec::new()),
            attrs: Attrs {
                readonly: true,
                ..Attrs::default()
            },
        };
        assert_eq!(render(&empty), "declare -rA v=()\n");

        let scalar = Var {
            value: VarValue::Str(b"111".to_vec()),
            attrs: Attrs {
                exported: true,
                ..Attrs::default()
            },
        };
        assert_eq!(render(&scalar), "declare -x v=111\n");
    }
}
