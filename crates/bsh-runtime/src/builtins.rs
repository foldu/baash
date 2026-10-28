//! The builtin registry: POSIX's three tiers plus bash's extensions.
//!
//! POSIX 2024 §2.15 names fifteen *special* built-ins and §1.7 adds a new
//! *intrinsic utilities* tier (`alias bg cd command fc fg getopts hash jobs
//! kill read type ulimit umask unalias wait`). Both lists are fixed by the
//! standard; the table below is the authority for them. `printf`, `test`,
//! `[`, `echo`, `pwd`, `true` and `false` are deliberately [`Tier::Regular`]
//! and therefore exec-able: POSIX §2 says they are standard utilities, not
//! intrinsic.
//!
//! Search order (POSIX §2.9.1.4, DESIGN.md §3.3): special built-ins before
//! functions, functions before intrinsic utilities, intrinsic utilities before
//! `PATH`; regular built-ins sit where `PATH` would be and bash extensions
//! behave like regular built-ins for search purposes.
//!
//! A builtin whose body is not written yet is still in the table: [`BuiltinDef::run`]
//! emits a `BSH-E0508` diagnostic and returns status 2. Nothing panics.

use crate::errors::{Error, ShellError, codes, display};

mod complete;
mod getopts;
mod meta;
use crate::opts::OptionKind;
use crate::vars::{ArrayShape, CreateScope, Vars};
use crate::{Opts, ShellHost, Status};
use bsh_diag::Diagnostic;

/// Which class a builtin belongs to. The tier decides both search order and
/// assignment persistence, which are the two places it changes behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// POSIX special built-in: searched before functions; failures can exit a
    /// non-interactive shell; preceding assignments persist.
    Special,
    /// POSIX 2024 intrinsic utility: a shell builtin that need not exist as an
    /// executable, and that functions may override.
    Intrinsic,
    /// A standard utility implemented in-process for convenience. It must
    /// still be reachable through `exec`, so it is never intrinsic.
    Regular,
    /// A bash extension (`local`, `declare`, `shopt`, …). Present so the
    /// search order can be implemented, not yet implemented.
    Extension,
}

impl Tier {
    /// POSIX §2.9.1.4: assignments preceding a *special* built-in persist in
    /// the shell; for every other tier they are scoped to the command.
    ///
    /// The VM passes the tier of the command name it resolved. That means
    /// `x=1 command :` uses [`Tier::Intrinsic`] and is temporary even though
    /// `:` is special, which is what bash does.
    pub fn assignment_persistence(self) -> AssignPersistence {
        match self {
            Tier::Special => AssignPersistence::Persist,
            _ => AssignPersistence::Temporary,
        }
    }

    /// Search rank: special built-ins come first, then function lookup (not a
    /// tier), then intrinsic utilities, then regular built-ins and `PATH`.
    /// Extensions sit with regular built-ins.
    pub fn search_priority(self) -> u8 {
        match self {
            Tier::Special => 0,
            Tier::Intrinsic => 1,
            Tier::Regular | Tier::Extension => 2,
        }
    }

    /// Whether a shell function may shadow this tier. POSIX: functions
    /// override intrinsic utilities but never special built-ins.
    pub fn overridable_by_function(self) -> bool {
        !matches!(self, Tier::Special)
    }

    /// Name used in diagnostics and listings.
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Special => "special",
            Tier::Intrinsic => "intrinsic",
            Tier::Regular => "regular",
            Tier::Extension => "extension",
        }
    }
}

/// What happens to `NAME=VALUE` prefixes in front of a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignPersistence {
    /// The assignments become shell variables (special built-ins only).
    Persist,
    /// The assignments are visible to the command and then discarded.
    Temporary,
}

impl AssignPersistence {
    /// Human-readable form, used by tests and documentation.
    pub fn as_str(self) -> &'static str {
        match self {
            AssignPersistence::Persist => "persist",
            AssignPersistence::Temporary => "temporary",
        }
    }
}

/// A builtin body. `argv[0]` is the command name as written.
pub type Handler = fn(&mut dyn ShellHost, &[&[u8]]) -> Status;

/// One entry of the builtin registry.
#[derive(Clone, Copy)]
pub struct BuiltinDef {
    /// Name as typed.
    pub name: &'static str,
    /// POSIX/bourne tier.
    pub tier: Tier,
    /// The body, or `None` when the builtin is registered but not written.
    pub handler: Option<Handler>,
}

impl std::fmt::Debug for BuiltinDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuiltinDef")
            .field("name", &self.name)
            .field("tier", &self.tier)
            .field("implemented", &self.handler.is_some())
            .finish()
    }
}

impl BuiltinDef {
    /// Whether a body exists.
    pub fn is_implemented(&self) -> bool {
        self.handler.is_some()
    }

    /// Run the builtin, or report that it is not written yet.
    ///
    /// The dispatch point records a [`ShellError`] for a failing builtin when
    /// the handler did not record a more precise class itself, so the VM can
    /// apply the POSIX §2.8.1 table with [`crate::consequence`].
    pub fn run(&self, host: &mut dyn ShellHost, argv: &[&[u8]]) -> Status {
        let status = match self.handler {
            Some(handler) => handler(host, argv),
            None => self.not_implemented(host),
        };
        if !status.is_success() && host.state().error().is_none() {
            host.state()
                .record_error(ShellError::from_builtin(self.tier, status));
        }
        status
    }

    fn not_implemented(&self, host: &mut dyn ShellHost) -> Status {
        host.diagnose(&Diagnostic::error(
            codes::NOT_IMPLEMENTED,
            0..0,
            format!("{}: not implemented yet", self.name),
        ));
        Status(2)
    }
}

/// `shopt [-s|-u] [-q] [-p] [name…]`.
///
/// Every name is a `shopt`-table option; the table is the single source of
/// truth, so an unknown name is an error rather than a silently ignored word.
fn bi_shopt(host: &mut dyn ShellHost, argv: &[&[u8]]) -> Status {
    let mut on: Option<bool> = None;
    let mut quiet = false;
    let mut printable = false;
    let mut names: Vec<&[u8]> = Vec::new();

    for argument in &argv[1..] {
        match *argument {
            b"-s" => on = Some(true),
            b"-u" => on = Some(false),
            b"-q" => quiet = true,
            b"-p" => printable = true,
            b"--" => {}
            other if other.starts_with(b"-") && other.len() > 1 => {
                // `-su` and friends.
                for letter in &other[1..] {
                    match letter {
                        b's' => on = Some(true),
                        b'u' => on = Some(false),
                        b'q' => quiet = true,
                        b'p' => printable = true,
                        _ => {
                            host.diagnose(&Diagnostic::error(
                                codes::USAGE,
                                0..0,
                                format!("shopt: -{}: invalid option", char::from(*letter)),
                            ));
                            return Status(2);
                        }
                    }
                }
            }
            other => names.push(other),
        }
    }

    let opts = &host.state().opts;
    let known: Vec<&'static str> = opts.names(OptionKind::Shopt).collect();

    if names.is_empty() {
        if quiet {
            return Status::SUCCESS;
        }
        let mut out = Vec::new();
        for option in opts.options(OptionKind::Shopt) {
            if printable {
                out.extend_from_slice(b"shopt -");
                out.push(if option.on { b's' } else { b'u' });
                out.push(b' ');
                out.extend_from_slice(option.name.as_bytes());
                out.push(b'\n');
            } else {
                out.extend_from_slice(option.name.as_bytes());
                out.extend_from_slice(if option.on { b"\ton\n" } else { b"\toff\n" });
            }
        }
        host.write_out(&out);
        return Status::SUCCESS;
    }

    let mut status = Status::SUCCESS;
    for name in names {
        let Some(_option) = known.iter().find(|known| known.as_bytes() == name) else {
            host.diagnose(&Diagnostic::error(
                codes::USAGE,
                0..0,
                format!(
                    "shopt: {}: invalid shell option name",
                    String::from_utf8_lossy(name)
                ),
            ));
            status = Status::FAILURE;
            continue;
        };
        match on {
            Some(value) => {
                if let Err(error) = host
                    .state()
                    .opts
                    .set_by_name_in(OptionKind::Shopt, name, value)
                {
                    host.diagnose(&Diagnostic::error(
                        codes::USAGE,
                        0..0,
                        format!("shopt: {error}"),
                    ));
                    status = Status::FAILURE;
                    continue;
                }
                if printable {
                    // `-p` with `-s` prints the resulting state.
                    let mut out = b"shopt -".to_vec();
                    out.push(if value { b's' } else { b'u' });
                    out.push(b' ');
                    out.extend_from_slice(name);
                    out.push(b'\n');
                    host.write_out(&out);
                }
            }
            None => {
                let current = host.state().opts.get_by_name(name).unwrap_or(false);
                if printable {
                    let mut out = b"shopt -".to_vec();
                    out.push(if current { b's' } else { b'u' });
                    out.push(b' ');
                    out.extend_from_slice(name);
                    out.push(b'\n');
                    host.write_out(&out);
                } else if quiet && !current {
                    status = Status::FAILURE;
                }
            }
        }
    }
    status
}

/// The registry. Fixed lists are asserted by tests; order within a tier is
/// alphabetical for predictable listings.
pub const BUILTINS: &[BuiltinDef] = &[
    // POSIX §2.15 special built-ins (15), alphabetical with `:` first.
    def(":", Tier::Special, Some(bi_colon)),
    def("break", Tier::Special, None),
    def("continue", Tier::Special, None),
    def(".", Tier::Special, None),
    def("eval", Tier::Special, None),
    def("exec", Tier::Special, None),
    def("exit", Tier::Special, Some(bi_exit)),
    def("export", Tier::Special, Some(bi_export)),
    def("readonly", Tier::Special, Some(bi_readonly)),
    def("return", Tier::Special, None),
    def("set", Tier::Special, Some(bi_set)),
    def("shift", Tier::Special, Some(bi_shift)),
    def("times", Tier::Special, None),
    def("trap", Tier::Special, Some(bi_trap)),
    def("unset", Tier::Special, Some(bi_unset)),
    // POSIX 2024 §1.7 intrinsic utilities (16).
    def("alias", Tier::Intrinsic, Some(meta::bi_alias)),
    def("bg", Tier::Intrinsic, None),
    def("cd", Tier::Intrinsic, None),
    def("command", Tier::Intrinsic, Some(meta::bi_command)),
    def("fc", Tier::Intrinsic, None),
    def("fg", Tier::Intrinsic, None),
    def("getopts", Tier::Intrinsic, Some(getopts::bi_getopts)),
    def("hash", Tier::Intrinsic, None),
    def("jobs", Tier::Intrinsic, None),
    def("kill", Tier::Intrinsic, None),
    def("read", Tier::Intrinsic, None),
    def("type", Tier::Intrinsic, Some(meta::bi_type)),
    def("ulimit", Tier::Intrinsic, None),
    def("umask", Tier::Intrinsic, None),
    def("unalias", Tier::Intrinsic, Some(meta::bi_unalias)),
    def("wait", Tier::Intrinsic, None),
    // Standard utilities that must remain exec-able, so never intrinsic.
    def("[", Tier::Regular, None),
    def("echo", Tier::Regular, None),
    def("false", Tier::Regular, Some(bi_false)),
    def("printf", Tier::Regular, None),
    def("pwd", Tier::Regular, None),
    def("test", Tier::Regular, None),
    def("true", Tier::Regular, Some(bi_true)),
    // Bash extensions (DESIGN.md §3.4); registered so search order and
    // "command not found" diagnostics can distinguish them.
    def("builtin", Tier::Extension, Some(meta::bi_builtin)),
    def("compgen", Tier::Extension, Some(complete::bi_compgen)),
    def("complete", Tier::Extension, Some(complete::bi_complete)),
    def("compopt", Tier::Extension, None),
    def("declare", Tier::Extension, None),
    def("dirs", Tier::Extension, None),
    def("disown", Tier::Extension, None),
    def("enable", Tier::Extension, None),
    def("history", Tier::Extension, None),
    def("let", Tier::Extension, None),
    def("local", Tier::Extension, None),
    def("mapfile", Tier::Extension, None),
    def("popd", Tier::Extension, None),
    def("pushd", Tier::Extension, None),
    def("readarray", Tier::Extension, None),
    def("shopt", Tier::Extension, Some(bi_shopt)),
    def("source", Tier::Extension, None),
    def("typeset", Tier::Extension, None),
];

const fn def(name: &'static str, tier: Tier, handler: Option<Handler>) -> BuiltinDef {
    BuiltinDef {
        name,
        tier,
        handler,
    }
}

/// Find a builtin by name, as bytes.
pub fn lookup(name: &[u8]) -> Option<&'static BuiltinDef> {
    BUILTINS
        .iter()
        .find(|builtin| builtin.name.as_bytes() == name)
}

/// Tier of a builtin name, if it is one.
pub fn tier_of(name: &[u8]) -> Option<Tier> {
    lookup(name).map(|builtin| builtin.tier)
}

/// Run a builtin by name. `None` means "not a builtin" — the VM then tries a
/// function or `PATH`; it does not mean "failed".
pub fn run_builtin(name: &[u8], host: &mut dyn ShellHost, argv: &[&[u8]]) -> Option<Status> {
    lookup(name).map(|builtin| builtin.run(host, argv))
}

/// The names in `tier`'s group, in table order.
pub fn names_in(tier: Tier) -> impl Iterator<Item = &'static str> {
    BUILTINS
        .iter()
        .filter(move |builtin| builtin.tier == tier)
        .map(|builtin| builtin.name)
}

/// Whether at least one builtin of `tier` is still a stub.
pub fn stubs_in(tier: Tier) -> usize {
    BUILTINS
        .iter()
        .filter(|builtin| builtin.tier == tier && !builtin.is_implemented())
        .count()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn arg<'a>(argv: &[&'a [u8]], index: usize) -> Option<&'a [u8]> {
    argv.get(index).copied()
}

fn diagnose(host: &mut dyn ShellHost, code: bsh_diag::ErrCode, message: impl Into<String>) {
    host.diagnose(&Diagnostic::error(code, 0..0, message));
}

/// Builtin name for messages, falling back to a caller-supplied spelling when
/// the VM invokes the handler with an empty argv.
fn program_name(argv: &[&[u8]], fallback: &str) -> String {
    argv.first()
        .map(|name| display(name))
        .unwrap_or_else(|| fallback.to_owned())
}

fn usage(host: &mut dyn ShellHost, argv: &[&[u8]], message: &str) -> Status {
    diagnose(
        host,
        codes::USAGE,
        format!("{}: {message}", program_name(argv, "builtin")),
    );
    Status(2)
}

fn report_error(host: &mut dyn ShellHost, argv: &[&[u8]], error: &Error) -> Status {
    diagnose(
        host,
        error.code(),
        format!("{}: {}", program_name(argv, "builtin"), error.message()),
    );
    Status(1)
}

fn parse_count(bytes: &[u8]) -> Option<usize> {
    let digits = bytes.strip_prefix(b"+").unwrap_or(bytes);
    if digits.is_empty() {
        return None;
    }
    let mut value: usize = 0;
    for &byte in digits {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value
            .checked_mul(10)?
            .checked_add(usize::from(byte - b'0'))?;
    }
    Some(value)
}

/// One operand of `export`/`readonly`/`local`: `name`, `name=value` or
/// `name+=value`.
struct AssignmentOperand<'a> {
    name: &'a [u8],
    value: Option<&'a [u8]>,
    append: bool,
}

fn split_assignment(operand: &[u8]) -> AssignmentOperand<'_> {
    let Some(equals) = operand.iter().position(|&byte| byte == b'=') else {
        return AssignmentOperand {
            name: operand,
            value: None,
            append: false,
        };
    };
    let raw_name = &operand[..equals];
    let (name, append) = match raw_name.strip_suffix(b"+") {
        Some(stripped) => (stripped, true),
        None => (raw_name, false),
    };
    AssignmentOperand {
        name,
        value: Some(&operand[equals + 1..]),
        append,
    }
}

fn parse_exit_code(bytes: &[u8]) -> Option<i32> {
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
        value = value.checked_mul(10)?;
        value = value.checked_add(i64::from(byte - b'0'))?;
    }
    let value = if negative { -value } else { value };
    i32::try_from(value).ok()
}

/// Quote one byte string so the result can be re-read by the shell. Values
/// with no special characters are emitted raw; everything else is
/// single-quoted, with embedded `'` written as `'\''`.
fn quote_into(out: &mut Vec<u8>, bytes: &[u8]) {
    let bare = !bytes.is_empty()
        && bytes.iter().all(|&byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'_' | b'-' | b'.' | b'/' | b':' | b'@' | b'%' | b'+' | b','
                )
        });
    if bare {
        out.extend_from_slice(bytes);
        return;
    }
    out.push(b'\'');
    for &byte in bytes {
        if byte == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(byte);
        }
    }
    out.push(b'\'');
}

fn push_assignment(
    out: &mut Vec<u8>,
    keyword: Option<&[u8]>,
    name: &[u8],
    value: &crate::VarValue,
) {
    if let Some(keyword) = keyword {
        out.extend_from_slice(keyword);
        out.push(b' ');
    }
    out.extend_from_slice(name);
    match value {
        crate::VarValue::Unset => {
            if keyword.is_none() {
                out.push(b'=');
            }
        }
        crate::VarValue::Str(bytes) => {
            out.push(b'=');
            quote_into(out, bytes);
        }
        crate::VarValue::Indexed(elements) => {
            out.extend_from_slice(b"=(");
            for (position, (index, element)) in elements.iter().enumerate() {
                if position > 0 {
                    out.push(b' ');
                }
                out.extend_from_slice(format!("[{index}]=").as_bytes());
                quote_into(out, element);
            }
            out.push(b')');
        }
        crate::VarValue::Assoc(pairs) => {
            out.extend_from_slice(b"=(");
            for (index, (key, value)) in pairs.iter().enumerate() {
                if index > 0 {
                    out.push(b' ');
                }
                out.push(b'[');
                quote_into(out, key);
                out.extend_from_slice(b"]=");
                quote_into(out, value);
            }
            out.push(b')');
        }
    }
    out.push(b'\n');
}

fn list_variables(
    host: &mut dyn ShellHost,
    keyword: Option<&[u8]>,
    filter: fn(&crate::Var) -> bool,
) -> Status {
    let mut out = Vec::new();
    {
        let state = host.state();
        for (name, var) in state.vars.visible() {
            if filter(var) {
                push_assignment(&mut out, keyword, name, &var.value);
            }
        }
    }
    host.write_out(&out)
}

// ---------------------------------------------------------------------------
// Implemented special builtins
// ---------------------------------------------------------------------------

fn bi_colon(_host: &mut dyn ShellHost, _argv: &[&[u8]]) -> Status {
    Status::SUCCESS
}

fn bi_true(_host: &mut dyn ShellHost, _argv: &[&[u8]]) -> Status {
    Status::SUCCESS
}

fn bi_false(_host: &mut dyn ShellHost, _argv: &[&[u8]]) -> Status {
    Status(1)
}

fn bi_exit(host: &mut dyn ShellHost, argv: &[&[u8]]) -> Status {
    if argv.len() > 2 {
        diagnose(
            host,
            codes::USAGE,
            format!("{}: too many arguments", program_name(argv, "exit")),
        );
        if !host.is_interactive() {
            host.state().request_exit(Status(1));
        }
        return Status(1);
    }

    enum Request {
        Last,
        Code(i32),
        Invalid(Vec<u8>),
    }
    let request = match arg(argv, 1) {
        None => Request::Last,
        Some(bytes) => match parse_exit_code(bytes) {
            Some(code) => Request::Code(code),
            None => Request::Invalid(bytes.to_vec()),
        },
    };
    match request {
        Request::Last => {
            // Inside the EXIT trap a bare `exit` reuses the status the shell
            // was already exiting with, not the handler's last command.
            let status = host
                .trap_saved_status()
                .unwrap_or_else(|| host.state().vars.last_status());
            host.state().request_exit(status);
            status
        }
        Request::Code(code) => {
            let status = Status(code);
            host.state().request_exit(status);
            status
        }
        Request::Invalid(bytes) => {
            diagnose(
                host,
                codes::BAD_NUMBER,
                format!(
                    "{}: {}: numeric argument required",
                    program_name(argv, "exit"),
                    display(&bytes)
                ),
            );
            host.state().request_exit(Status(2));
            Status(2)
        }
    }
}

fn bi_shift(host: &mut dyn ShellHost, argv: &[&[u8]]) -> Status {
    let requested = match arg(argv, 1) {
        None => Ok(1usize),
        Some(bytes) => {
            if bytes.first() == Some(&b'-') && bytes.len() > 1 {
                Err(true)
            } else if let Some(count) = parse_count(bytes) {
                Ok(count)
            } else {
                Err(false)
            }
        }
    };
    let count = match requested {
        Ok(count) => count,
        Err(negative) => {
            let code = if negative {
                codes::SHIFT_UNDERFLOW
            } else {
                codes::BAD_NUMBER
            };
            diagnose(
                host,
                code,
                format!(
                    "{}: {}: {}",
                    program_name(argv, "shift"),
                    display(arg(argv, 1).unwrap_or_default()),
                    if negative {
                        "shift count out of range"
                    } else {
                        "numeric argument required"
                    }
                ),
            );
            return Status(1);
        }
    };
    match host.state().vars.shift(count) {
        Ok(()) => Status::SUCCESS,
        Err(error) => report_error(host, argv, &error),
    }
}

fn bi_export(host: &mut dyn ShellHost, argv: &[&[u8]]) -> Status {
    let mut print = false;
    let mut unexport = false;
    let mut index = 1;
    while index < argv.len() {
        match argv[index] {
            b"--" => {
                index += 1;
                break;
            }
            b"-p" => {
                print = true;
                index += 1;
            }
            b"-n" => {
                unexport = true;
                index += 1;
            }
            operand if operand.len() > 1 && operand[0] == b'-' => {
                return usage(host, argv, "unknown option");
            }
            _ => break,
        }
    }

    if print || index >= argv.len() {
        return list_variables(host, Some(b"export"), |var| var.attrs.exported);
    }

    let mut status = Status::SUCCESS;
    for &operand in &argv[index..] {
        let assignment = split_assignment(operand);
        let name = assignment.name;
        if !Vars::is_name(name) {
            let error = Error::InvalidName {
                name: name.to_vec(),
            };
            let _ = report_error(host, argv, &error);
            status = Status(1);
            continue;
        }
        let mut failed = false;
        if let Some(value) = assignment.value {
            let result = if assignment.append {
                host.state().vars.append(name, value)
            } else {
                host.state().vars.assign_scalar(name, value)
            };
            if let Err(error) = result {
                let _ = report_error(host, argv, &error);
                // A readonly write behind `export x=…` is a *variable
                // assignment* error, not a special-builtin utility error
                // (§2.8.1).
                host.state().record_error(ShellError::VariableAssignment);
                failed = true;
            }
        }
        if failed {
            status = Status(1);
            continue;
        }
        let attribute = if unexport {
            host.state().vars.unmark_exported(name)
        } else {
            host.state().vars.mark_exported(name)
        };
        if let Err(error) = attribute {
            let _ = report_error(host, argv, &error);
            status = Status(1);
        }
    }
    status
}

fn bi_readonly(host: &mut dyn ShellHost, argv: &[&[u8]]) -> Status {
    let mut print = false;
    let (mut indexed, mut associative) = (false, false);
    let mut index = 1;
    while index < argv.len() {
        match argv[index] {
            b"--" => {
                index += 1;
                break;
            }
            b"-p" => {
                print = true;
                index += 1;
            }
            // `readonly -a`/`-A` declare the shape before making it readonly,
            // so `readonly -a arr` on an existing array keeps its elements.
            b"-a" => {
                indexed = true;
                index += 1;
            }
            b"-A" => {
                associative = true;
                index += 1;
            }
            option if option.len() > 1 && option[0] == b'-' => {
                return usage(host, argv, "unknown option");
            }
            _ => break,
        }
    }

    if print || index >= argv.len() {
        return list_variables(host, Some(b"readonly"), |var| var.attrs.readonly);
    }

    let mut status = Status::SUCCESS;
    for &operand in &argv[index..] {
        let assignment = split_assignment(operand);
        let name = assignment.name;
        if !Vars::is_name(name) {
            let error = Error::InvalidName {
                name: name.to_vec(),
            };
            let _ = report_error(host, argv, &error);
            status = Status(1);
            continue;
        }
        if indexed || associative {
            if let Err(error) = declare_array_from_flag(&mut host.state().vars, name, associative) {
                let _ = report_error(host, argv, &error);
                status = Status(1);
                continue;
            }
        }
        if let Some(value) = assignment.value {
            let result = if assignment.append {
                host.state().vars.append(name, value)
            } else {
                host.state().vars.assign_scalar(name, value)
            };
            if let Err(error) = result {
                let _ = report_error(host, argv, &error);
                host.state().record_error(ShellError::VariableAssignment);
                status = Status(1);
                continue;
            }
        }
        if let Err(error) = host.state().vars.mark_readonly(name) {
            let _ = report_error(host, argv, &error);
            status = Status(1);
        }
    }
    status
}

fn bi_unset(host: &mut dyn ShellHost, argv: &[&[u8]]) -> Status {
    /// Which namespace `unset` was pointed at.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mode {
        /// Neither `-f` nor `-v`: the variable if it exists, else the function.
        Default,
        Function,
        Variable,
    }

    let mut mode = Mode::Default;
    let mut index = 1;
    let mut nameref = false;
    while index < argv.len() {
        match argv[index] {
            b"--" => {
                index += 1;
                break;
            }
            b"-v" => {
                mode = Mode::Variable;
                index += 1;
            }
            b"-f" => {
                mode = Mode::Function;
                index += 1;
            }
            b"-n" => {
                nameref = true;
                index += 1;
            }
            option if option.len() > 1 && option[0] == b'-' => {
                return usage(host, argv, "unknown option");
            }
            _ => break,
        }
    }
    if nameref {
        // `-n` removes the nameref itself rather than its target; namerefs are
        // deferred (DESIGN.md §3.4), so the honest answer is that this is not
        // implemented yet.
        diagnose(
            host,
            codes::NOT_IMPLEMENTED,
            format!(
                "{}: nameref removal is not implemented yet",
                program_name(argv, "unset")
            ),
        );
        return Status(2);
    }

    let mut status = Status::SUCCESS;
    for &name in &argv[index..] {
        if mode != Mode::Function && name.contains(&b'[') {
            if let Some(element_status) = host.unset_array_element(name) {
                if !element_status.is_success() {
                    status = element_status;
                }
                continue;
            }
            diagnose(
                host,
                codes::NOT_IMPLEMENTED,
                format!(
                    "{}: {}: array element removal is not implemented yet",
                    program_name(argv, "unset"),
                    display(name)
                ),
            );
            status = Status::FAILURE;
            continue;
        }
        match mode {
            Mode::Function => {
                // A missing function is not an error: `unset -f nope` is 0.
                host.unset_function(name);
            }
            Mode::Variable => {
                if let Err(error) = host.state().vars.unset(name) {
                    let _ = report_error(host, argv, &error);
                    status = Status::FAILURE;
                }
            }
            Mode::Default => {
                // bash: the variable wins when it exists, and only then is the
                // function considered, so `f=x; f() { …; }; unset f` keeps the
                // function.
                if host.state().vars.contains(name) {
                    if let Err(error) = host.state().vars.unset(name) {
                        let _ = report_error(host, argv, &error);
                        status = Status::FAILURE;
                    }
                } else {
                    host.unset_function(name);
                }
            }
        }
    }
    status
}

// ---------------------------------------------------------------------------
// `trap`
// ---------------------------------------------------------------------------

/// A trap failure must not be fatal even though `trap` is a special built-in.
///
/// POSIX §2.8.1 would exit a non-interactive shell when a special built-in
/// fails; every shell in the suite continues after a bad `trap` operand
/// (`trap 'x' SIGINVALID; echo ok` prints `ok`), so the handler records a
/// *utility* failure — the one class whose consequence is `Continue` — instead
/// of letting the dispatch point classify it by tier. `set -e` still stops a
/// script, which is how `trap -1 …` becomes fatal.
fn trap_failed(host: &mut dyn ShellHost, status: Status) {
    host.state().record_error(crate::ShellError::Utility {
        status,
        redirection: false,
    });
}

fn trap_usage(host: &mut dyn ShellHost, argv: &[&[u8]], message: &str) -> Status {
    let status = usage(host, argv, message);
    trap_failed(host, status);
    status
}

fn bi_trap(host: &mut dyn ShellHost, argv: &[&[u8]]) -> Status {
    let mut print = false;
    let mut list = false;
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
                    b'p' | b'P' => print = true,
                    b'l' | b'L' => list = true,
                    _ => {
                        return trap_usage(host, argv, "invalid option");
                    }
                }
            }
            index += 1;
            continue;
        }
        break;
    }

    if list {
        // `trap -l` lists the signal names; other operands are ignored, as in
        // bash, which makes `trap -l 2` a listing rather than a reset.
        let table = crate::signal_table();
        return host.write_out(&table);
    }

    let operands = &argv[index..];

    if print {
        // `trap -p` (or `trap -p SIG…`) prints in re-input-safe form. A signal
        // with no trap prints nothing; an invalid one is an error.
        let mut only = Vec::new();
        for spec in operands {
            match crate::TrapKey::parse(spec) {
                Some(key) => only.push(key),
                None => {
                    diagnose(
                        host,
                        codes::USAGE,
                        format!(
                            "{}: {}: invalid signal specification",
                            program_name(argv, "trap"),
                            display(spec)
                        ),
                    );
                    trap_failed(host, Status(1));
                    return Status(1);
                }
            }
        }
        return print_traps(host, &only);
    }

    if operands.is_empty() {
        return print_traps(host, &[]);
    }

    // bash's rule, and the one place the `trap` surface is surprising: if the
    // *first* operand is itself a signal specification, there is no action and
    // every operand is a signal to reset. So `trap 0 2` resets EXIT and INT,
    // and `trap 2 EXIT` does too; `trap - SIG` spells the same thing out, and
    // `trap EXIT` (one operand) is the common shorthand.
    if crate::TrapKey::parse(operands[0]).is_some() {
        let mut status = Status::SUCCESS;
        for spec in operands {
            match crate::TrapKey::parse(spec) {
                Some(key) => {
                    host.state().traps.remove(key);
                }
                None => {
                    diagnose(
                        host,
                        codes::USAGE,
                        format!(
                            "{}: {}: invalid signal specification",
                            program_name(argv, "trap"),
                            display(spec)
                        ),
                    );
                    status = Status::FAILURE;
                }
            }
        }
        if !status.is_success() {
            trap_failed(host, status);
        }
        return status;
    }

    let action = operands[0];
    let signals = &operands[1..];
    if signals.is_empty() {
        return trap_usage(host, argv, "invalid signal specification");
    }
    let reset = action == b"-";
    if !reset && !host.check_syntax(action) {
        diagnose(
            host,
            codes::USAGE,
            format!(
                "{}: {}: syntax error in trap action",
                program_name(argv, "trap"),
                display(action)
            ),
        );
        trap_failed(host, Status::FAILURE);
        return Status::FAILURE;
    }

    let mut status = Status::SUCCESS;
    for &spec in signals {
        let Some(key) = crate::TrapKey::parse(spec) else {
            diagnose(
                host,
                codes::USAGE,
                format!(
                    "{}: {}: invalid signal specification",
                    program_name(argv, "trap"),
                    display(spec)
                ),
            );
            status = Status::FAILURE;
            continue;
        };
        if reset {
            host.state().traps.remove(key);
        } else if action.is_empty() {
            host.state().traps.set(key, crate::TrapAction::Ignore);
        } else {
            host.state()
                .traps
                .set(key, crate::TrapAction::Command(action.to_vec()));
        }
    }
    if !status.is_success() {
        trap_failed(host, status);
    }
    status
}

/// Print the traps in `only` (all of them when empty), re-input-safe.
fn print_traps(host: &mut dyn ShellHost, only: &[crate::TrapKey]) -> Status {
    let mut out = Vec::new();
    {
        let table = &host.state().traps;
        for (key, action) in table.entries() {
            if !only.is_empty() && !only.contains(key) {
                continue;
            }
            out.extend_from_slice(b"trap -- ");
            quote_into(&mut out, action.code());
            out.push(b' ');
            out.extend_from_slice(&key.as_spec());
            out.push(b'\n');
        }
    }
    host.write_out(&out)
}

// ---------------------------------------------------------------------------
// `set`
// ---------------------------------------------------------------------------

fn print_set_options(host: &mut dyn ShellHost, reinput: bool) -> Status {
    let mut out = Vec::new();
    {
        let state = host.state();
        for option in state.opts.options(OptionKind::SetO) {
            if reinput {
                out.extend_from_slice(if option.on { b"set -o " } else { b"set +o " });
                out.extend_from_slice(option.name.as_bytes());
                out.push(b'\n');
            } else {
                out.extend_from_slice(option.name.as_bytes());
                out.extend_from_slice(if option.on { b"\ton\n" } else { b"\toff\n" });
            }
        }
    }
    host.write_out(&out)
}

fn bi_set(host: &mut dyn ShellHost, argv: &[&[u8]]) -> Status {
    let mut index = 1;
    let mut positional: Option<Vec<Vec<u8>>> = None;
    let mut status = Status::SUCCESS;

    while index < argv.len() {
        let argument = argv[index];
        if argument == b"--" {
            index += 1;
            positional = Some(argv[index..].iter().map(|word| word.to_vec()).collect());
            break;
        }
        if argument == b"-" {
            // Historical: `set -` turns off -v and -x.
            {
                let opts = &mut host.state().opts;
                opts.verbose = false;
                opts.xtrace = false;
            }
            index += 1;
            continue;
        }
        let is_flag = matches!(argument.first(), Some(b'-' | b'+')) && argument.len() > 1;
        if !is_flag {
            positional = Some(argv[index..].iter().map(|word| word.to_vec()).collect());
            break;
        }

        let on = argument[0] == b'-';
        let flags = &argument[1..];
        let mut consumed_name = false;
        for &flag in flags {
            if flag == b'o' {
                // Bash lets `-o` end a bundle and take the next word:
                // `set -euo pipefail`. Characters after `o` in the same word
                // are ignored, matching bash.
                if let Some(name) = arg(argv, index + 1) {
                    let result = host.state().opts.set_by_name_in(OptionKind::SetO, name, on);
                    if let Err(error) = result {
                        let _ = report_error(host, argv, &error);
                        status = Status(2);
                    }
                    index += 2;
                } else {
                    // `set -o` / `set +o` with no name: print. POSIX leaves
                    // the plain form's format unspecified; `+o` must be
                    // reinput-safe.
                    let print_status = print_set_options(host, !on);
                    if !print_status.is_success() {
                        status = print_status;
                    }
                    index += 1;
                }
                consumed_name = true;
                break;
            }
            if let Err(error) = host.state().opts.set_by_short(flag, on) {
                let _ = report_error(host, argv, &error);
                status = Status(2);
            }
        }
        if !consumed_name {
            index += 1;
        }
    }

    if let Some(words) = positional {
        host.state().vars.set_positional(words);
    } else if argv.len() == 1 {
        // `set` with no options or arguments lists the shell variables.
        let mut out = Vec::new();
        {
            let state = host.state();
            for (name, var) in state.vars.visible() {
                push_assignment(&mut out, None, name, &var.value);
            }
        }
        let write_status = host.write_out(&out);
        if !write_status.is_success() {
            status = write_status;
        }
    }
    status
}

// ---------------------------------------------------------------------------
// `local`/`declare` helpers used by the future extension builtins
// ---------------------------------------------------------------------------

/// The scope `local`/`declare` writes to: local inside a function, global
/// otherwise (bash's rule). Kept here so the builtin bodies, when written,
/// cannot choose differently.
pub fn declaration_scope(vars: &Vars) -> CreateScope {
    if vars.frame_depth() > 0 {
        CreateScope::Local
    } else {
        CreateScope::Global
    }
}

/// Declare an array shape from `-a`/`-A` (`declare`, `readonly`, `local`).
pub fn declare_array_from_flag(
    vars: &mut Vars,
    name: &[u8],
    associative: bool,
) -> Result<(), Error> {
    vars.declare_shape(
        name,
        if associative {
            ArrayShape::Associative
        } else {
            ArrayShape::Indexed
        },
    )
}

/// The options a `set -o` printer would show, for callers that want them
/// without the builtin.
pub fn set_option_table(opts: &Opts) -> Vec<(&'static str, bool)> {
    opts.options(OptionKind::SetO)
        .map(|option| (option.name, option.on))
        .collect()
}

/// Snapshot of the builtin table, useful for `type`/completion later.
pub fn builtin_names() -> impl Iterator<Item = (&'static str, Tier)> {
    BUILTINS.iter().map(|builtin| (builtin.name, builtin.tier))
}
