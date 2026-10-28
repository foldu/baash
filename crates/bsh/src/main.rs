//! The ßhell binary.
//!
//! Argument dispatch plus the three ways to give a shell some text: `-c`, a
//! script file, and standard input. Parsing, execution and diagnostics all
//! live in the crates; this file decides *which* bytes get run, sets `$0` and
//! the positional parameters, and turns the final status into an exit code.
//!
//! `-n` is the one mode that never executes: it parses and renders
//! `bsh_diag::render_report`, exiting 2 on a syntax error, because a syntax
//! error must be visible *before* any side effect (DESIGN.md §5.6).

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::io::{BufRead, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::process::ExitCode;

use bsh_diag::{Diagnostic, SourceFile};
use bsh_repl::Theme;
use bsh_runtime::ShellHost;
use bsh_syntax::{ParseOpts, ParseOutcome};
use bsh_vm::Interp;

const USAGE: &str = "\
ßhell — a POSIX- and bash-compatible shell

USAGE
    bsh [OPTIONS] [SCRIPT [ARGUMENT...]]
    bsh -c COMMAND [NAME [ARGUMENT...]]

OPTIONS
    -c <COMMAND>    Run COMMAND.
    -i              Interactive: read commands from a terminal with a prompt.
    -n              Parse SCRIPT (or stdin) without executing.
    --highlight     Syntax-highlight stdin to stdout and exit.
    -h, --help      Print this help.
    -V, --version   Print the version.

Exit status: the status of the last command, or 2 for a usage or syntax error.
";

/// Exit status for "this is not implemented yet", mirroring the shell
/// convention of refusing to pretend.
const UNIMPLEMENTED: u8 = 2;

/// What the invocation asked for.
enum Mode {
    Help,
    Version,
    Highlight,
    /// `-n`: parse, report, do not execute.
    ParseOnly(Source),
    /// Run `-c`, a file, or stdin.
    Execute(Source),
    Lint(Source),
}

/// Shell options given on the command line: `-e`, `+u`, `-o pipefail`, `+o`.
///
/// The POSIX letters are applied through `Opts::set_by_short` and `-o` names
/// through `Opts::set_by_name`, so the CLI needs no table of its own and an
/// unknown option is reported by the same code that reports `set -o junk`.
#[derive(Debug, Default)]
struct ShellFlags {
    set: Vec<FlagSpec>,
    clear: Vec<FlagSpec>,
    /// `-O name`: shopt options to set before the first parse.
    shopt_set: Vec<Vec<u8>>,
    /// `+O name`: shopt options to unset before the first parse.
    shopt_clear: Vec<Vec<u8>>,
}

impl ShellFlags {
    fn apply(&self, opts: &mut bsh_runtime::Opts) -> Result<(), String> {
        for name in &self.shopt_set {
            bsh_runtime::Opts::set_shopt_by_name(opts, name, true)
                .map_err(|error| format!("-O {}: {error}", String::from_utf8_lossy(name)))?;
        }
        for name in &self.shopt_clear {
            bsh_runtime::Opts::set_shopt_by_name(opts, name, false)
                .map_err(|error| format!("+O {}: {error}", String::from_utf8_lossy(name)))?;
        }
        for spec in &self.set {
            spec.apply(opts, true)?;
        }
        for spec in &self.clear {
            spec.apply(opts, false)?;
        }
        Ok(())
    }
}

#[derive(Debug)]
enum FlagSpec {
    /// A `set` letter, e.g. `-e`.
    Short(u8),
    /// An `-o` name, e.g. `-o pipefail`.
    Named(Vec<u8>),
}

impl FlagSpec {
    fn apply(&self, opts: &mut bsh_runtime::Opts, on: bool) -> Result<(), String> {
        let result = match self {
            FlagSpec::Short(letter) => opts.set_by_short(*letter, on),
            FlagSpec::Named(name) => opts.set_by_name(name, on),
        };
        result.map_err(|_error| match self {
            FlagSpec::Short(letter) => format!("unknown option `-{}`", char::from(*letter)),
            FlagSpec::Named(name) => {
                format!("unknown option name `{}`", String::from_utf8_lossy(name))
            }
        })
    }
}

/// Where the shell text comes from.
enum Source {
    /// `-c COMMAND`, with an optional `$0` and positional parameters.
    Command(Vec<u8>, Vec<Vec<u8>>),
    /// A script file and its arguments.
    File(Vec<u8>, Vec<Vec<u8>>),
    /// Standard input.
    Stdin,
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match parse_args(&args) {
        Ok((mode, interactive, flags)) => dispatch(mode, interactive, flags),
        Err(message) => {
            eprintln!("bsh: {message}");
            eprintln!("Try 'bsh --help' for more information.");
            ExitCode::from(2)
        }
    }
}

fn bytes(value: &OsString) -> Vec<u8> {
    value.as_os_str().as_bytes().to_vec()
}

fn parse_args(args: &[OsString]) -> Result<(Mode, bool, ShellFlags), String> {
    let mut index = 0;
    let mut command: Option<Vec<u8>> = None;
    let mut parse_only = false;
    let mut highlight = false;
    let mut interactive = false;
    let mut flags = ShellFlags::default();
    let mut positional: Vec<Vec<u8>> = Vec::new();

    while index < args.len() {
        let argument = bytes(&args[index]);
        match argument.as_slice() {
            b"-h" | b"--help" => return Ok((Mode::Help, false, ShellFlags::default())),
            b"-V" | b"--version" => return Ok((Mode::Version, false, ShellFlags::default())),
            b"--highlight" => highlight = true,
            b"--lint" => {
                return Ok((
                    Mode::Lint(source_from(&positional, &mut None)?),
                    false,
                    ShellFlags::default(),
                ));
            }
            b"-i" | b"--interactive" => interactive = true,
            b"-n" => parse_only = true,
            b"-c" => {
                index += 1;
                let Some(text) = args.get(index) else {
                    return Err("-c requires an argument".to_owned());
                };
                command = Some(bytes(text));
            }
            b"-s" => {}
            b"-o" | b"+o" => {
                let on = argument[0] == b'-';
                index += 1;
                let Some(name) = args.get(index) else {
                    return Err(format!("{}o requires an argument", char::from(argument[0])));
                };
                let name = bytes(name);
                if on {
                    flags.set.push(FlagSpec::Named(name));
                } else {
                    flags.clear.push(FlagSpec::Named(name));
                }
            }
            b"--" => {
                index += 1;
                while index < args.len() {
                    positional.push(bytes(&args[index]));
                    index += 1;
                }
                break;
            }
            other if other.starts_with(b"--") => {
                // Every long option this shell has is matched above; the rest
                // are errors rather than a bundle of `set` letters.
                return Err(format!(
                    "unknown option `{}`",
                    String::from_utf8_lossy(other)
                ));
            }
            other @ (b"-O" | b"+O") => {
                // `-O extglob` is how bash lets a *script* be parsed with
                // extglob on. The decision is parse-time, so it has to be a
                // command-line flag, not a `shopt` the file itself runs.
                index += 1;
                let Some(name) = args.get(index) else {
                    return Err(format!(
                        "{} requires an option name",
                        String::from_utf8_lossy(other)
                    ));
                };
                let name = bytes(name);
                if !bsh_runtime::Opts::is_shopt_name(&name) {
                    return Err(format!(
                        "unknown shopt option `{}`",
                        String::from_utf8_lossy(&name)
                    ));
                }
                if other == b"-O" {
                    flags.shopt_set.push(name);
                } else {
                    flags.shopt_clear.push(name);
                }
            }
            other if other.len() > 1 && (other[0] == b'-' || other[0] == b'+') => {
                // A bundle of `set` letters: `-eu`, `+x`. The letters this CLI
                // handles itself were matched above, so anything here is a
                // shell option and is validated by `Opts`.
                let on = other[0] == b'-';
                for letter in &other[1..] {
                    if on {
                        flags.set.push(FlagSpec::Short(*letter));
                    } else {
                        flags.clear.push(FlagSpec::Short(*letter));
                    }
                }
            }
            _ => positional.push(argument),
        }
        index += 1;
    }

    if highlight {
        return Ok((Mode::Highlight, false, flags));
    }
    let source = source_from(&positional, &mut command)?;
    Ok((
        if parse_only {
            Mode::ParseOnly(source)
        } else {
            Mode::Execute(source)
        },
        interactive,
        flags,
    ))
}

/// Turn the trailing operands (plus any `-c` command) into a [`Source`].
fn source_from(positional: &[Vec<u8>], command: &mut Option<Vec<u8>>) -> Result<Source, String> {
    if let Some(text) = command.take() {
        let (name, arguments) = match positional.split_first() {
            Some((name, rest)) => (Some(name.clone()), rest.to_vec()),
            None => (None, Vec::new()),
        };
        let mut argv = Vec::with_capacity(arguments.len() + usize::from(name.is_some()));
        if let Some(name) = name {
            argv.push(name);
        }
        argv.extend(arguments);
        return Ok(Source::Command(text, argv));
    }
    match positional.split_first() {
        Some((path, arguments)) => Ok(Source::File(path.clone(), arguments.to_vec())),
        None => Ok(Source::Stdin),
    }
}

fn dispatch(mode: Mode, interactive_flag: bool, flags: ShellFlags) -> ExitCode {
    match mode {
        Mode::Help => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Mode::Version => {
            println!("bsh {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Mode::Highlight => highlight_stdin(),
        Mode::Lint(source) => {
            eprintln!("bsh: `--lint` is not implemented yet");
            let _ = source;
            ExitCode::from(UNIMPLEMENTED)
        }
        Mode::ParseOnly(source) => {
            let mut shell = shell(false, &flags);
            match read_source(&mut shell, source) {
                Ok((name, bytes)) => parse_only(&name, &bytes),
                Err(code) => code,
            }
        }
        Mode::Execute(source) => {
            // Interactive by default when the command text comes from a
            // terminal; `-i` asks for interactive semantics even when it does
            // not (a pipe, a script). Only a terminal can be driven by the
            // line editor, so a pipe keeps the script path.
            let stdin_tty = bsh_sys::is_tty(0);
            let from_stdin = matches!(source, Source::Stdin);
            let repl = from_stdin && stdin_tty;
            let interactive = interactive_flag || repl;
            let mut shell = shell(interactive, &flags);
            if repl {
                run_repl(&mut shell)
            } else {
                execute(&mut shell, source)
            }
        }
    }
}

/// A shell with the environment imported and `PWD` sane.
fn shell(interactive: bool, flags: &ShellFlags) -> Interp {
    let mut shell = Interp::new(interactive);
    // The one place that knows it is running for a user rather than a test:
    // character widths come from the environment here and nowhere else.
    shell.detect_locale();
    if let Err(message) = flags.apply(&mut shell.state().opts) {
        eprintln!("bsh: {message}");
        eprintln!("Try 'bsh --help' for more information.");
        std::process::exit(2);
    }
    shell.import_environment();
    if shell.state().vars.scalar(b"PWD").is_none() {
        if let Ok(directory) = std::env::current_dir() {
            let _ = shell
                .state()
                .vars
                .assign_scalar(b"PWD", directory.as_os_str().as_bytes());
            let _ = shell.state().vars.mark_exported(b"PWD");
        }
    }
    shell
}

fn execute(shell: &mut Interp, source: Source) -> ExitCode {
    match source {
        Source::Stdin => run_stdin(shell),
        other => match read_source(shell, other) {
            Ok((name, bytes)) => {
                let display = String::from_utf8_lossy(&name).into_owned();
                let status = shell.run_named(&display, &bytes);
                ExitCode::from(status.exit_code() as u8)
            }
            Err(code) => code,
        },
    }
}

/// Read the bytes a [`Source`] names, and set `$0`/positional for it.
fn read_source(shell: &mut Interp, source: Source) -> Result<(Vec<u8>, Vec<u8>), ExitCode> {
    match source {
        Source::Command(text, argv) => {
            let (name, arguments) = match argv.split_first() {
                Some((name, rest)) => (name.clone(), rest.to_vec()),
                None => (b"bsh".to_vec(), Vec::new()),
            };
            shell.set_arg0(name.clone());
            shell.set_positional(arguments);
            Ok((name, text))
        }
        Source::File(path, argv) => {
            match std::fs::read(std::path::Path::new(std::ffi::OsStr::from_bytes(&path))) {
                Ok(bytes) => {
                    shell.set_arg0(path.clone());
                    shell.set_positional(argv);
                    Ok((path, bytes))
                }
                Err(error) => {
                    eprintln!("bsh: {}: {error}", String::from_utf8_lossy(&path));
                    Err(ExitCode::from(127))
                }
            }
        }
        Source::Stdin => {
            let mut bytes = Vec::new();
            if let Err(error) = std::io::stdin().read_to_end(&mut bytes) {
                eprintln!("bsh: cannot read stdin: {error}");
                return Err(ExitCode::from(1));
            }
            shell.set_arg0("bsh");
            Ok((b"<stdin>".to_vec(), bytes))
        }
    }
}

/// Write to stderr without panicking when the reader has gone away.
///
/// `eprint!` panics on a failed write, and a diagnostic report can be far
/// larger than the pipe buffer, so `bsh -n … | head` must not turn a syntax
/// error into exit status 101.
fn write_stderr(text: &str) {
    let mut stderr = std::io::stderr().lock();
    let _ = stderr
        .write_all(text.as_bytes())
        .and_then(|()| stderr.flush());
}

fn parse_only(name: &[u8], bytes: &[u8]) -> ExitCode {
    let file = SourceFile::new(String::from_utf8_lossy(name).into_owned(), bytes);
    match bsh_syntax::parse_with(&file, &ParseOpts::DEFAULT) {
        ParseOutcome::Ok(_) => ExitCode::SUCCESS,
        ParseOutcome::NeedMoreInput { open, .. } => {
            let diagnostic = Diagnostic::error(
                bsh_syntax::codes::NEED_MORE_INPUT,
                open,
                "unexpected end of file: the input is incomplete",
            );
            write_stderr(&bsh_diag::render_report(&[diagnostic], &file));
            ExitCode::from(2)
        }
        ParseOutcome::Failed(diagnostics) => {
            write_stderr(&bsh_diag::render_report(&diagnostics, &file));
            ExitCode::from(2)
        }
    }
}

/// The parser's "is this command finished?" answer, as the editor needs it.
struct ShellSyntax;

impl bsh_repl::Syntax for ShellSyntax {
    fn needs_more_input(&self, buffer: &[u8]) -> bool {
        matches!(
            bsh_syntax::parse_bytes(buffer, &ParseOpts::DEFAULT),
            ParseOutcome::NeedMoreInput { .. }
        )
    }
}

/// The interactive shell: prompt, line editor, completion, history.
///
/// `$?` consequences, exit requests and diagnostics are the VM's; this loop
/// only decides which bytes are one command. A failed command stays silent
/// because the command itself already reported its error, `exit N` leaves with
/// `N`, and `Ctrl-D` on an empty line leaves with 0.
fn run_repl(shell: &mut Interp) -> ExitCode {
    let syntax = ShellSyntax;
    let cwd = shell
        .state()
        .vars
        .scalar(b"PWD")
        .map(<[u8]>::to_vec)
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|path| path.as_os_str().as_bytes().to_vec())
        });
    let root = cwd
        .as_deref()
        .map(|bytes| std::path::PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let engine = bsh_complete::CompletionEngine::new().with_root(root);
    let config = bsh_repl::Config {
        // The editor paints on the controlling terminal, so colour is decided
        // by the terminal rather than by stdout (which may be redirected).
        color: bsh_repl::color_supported(0),
        prompt: bsh_repl::Prompt::from_env(),
        history: bsh_repl::History::from_env(),
        ..bsh_repl::Config::default()
    };
    let mut terminal = bsh_repl::RealTerminal::new();
    let mut editor = bsh_repl::Editor::with_config(&mut terminal, &engine, &syntax, config);
    if let Some(cwd) = &cwd {
        editor.set_cwd(cwd);
    }

    let status = loop {
        match editor.read_line() {
            Ok(bsh_repl::ReadResult::Line(bytes)) => {
                if bytes.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let before = shell.state().vars.scalar(b"PWD").map(<[u8]>::to_vec);
                shell.run_named("<stdin>", &bytes);
                // `cd` changes the shell's `PWD`; the completion engine and
                // `PS1`'s `\w` must follow it (`docs/contracts.md`).
                let after = shell.state().vars.scalar(b"PWD").map(<[u8]>::to_vec);
                if after != before
                    && let Some(pwd) = &after
                {
                    engine.set_root(std::ffi::OsStr::from_bytes(pwd));
                    editor.set_cwd(pwd);
                }
                if let Some(requested) = shell.state().take_exit_request() {
                    break requested;
                }
            }
            Ok(bsh_repl::ReadResult::Interrupted) => {
                // `Ctrl-C` abandons the line and leaves `$?` at 130, the way a
                // shell reports an interrupted foreground command.
                shell.state().vars.set_last_status(bsh_runtime::Status(130));
            }
            Ok(bsh_repl::ReadResult::Eof) => break bsh_runtime::Status::SUCCESS,
            Err(error) => {
                eprintln!("bsh: {error}");
                let _ = editor.finish();
                return ExitCode::from(1);
            }
        }
    };
    let _ = editor.finish();
    ExitCode::from(status.exit_code() as u8)
}

/// Non-interactive stdin is one chunk; a terminal gets a line-at-a-time loop
/// so a half-typed compound command is not executed early.
///
/// The terminal path above ([`run_repl`]) uses `bsh-repl`'s editor; this is the
/// fallback for a pipe or a redirected file, which must keep the script
/// semantics of executing the whole input as one chunk.
fn run_stdin(shell: &mut Interp) -> ExitCode {
    if !bsh_sys::is_tty(0) {
        let mut bytes = Vec::new();
        if let Err(error) = std::io::stdin().read_to_end(&mut bytes) {
            eprintln!("bsh: cannot read stdin: {error}");
            return ExitCode::from(1);
        }
        let status = shell.run_named("<stdin>", &bytes);
        return ExitCode::from(status.exit_code() as u8);
    }

    let mut pending: Vec<u8> = Vec::new();
    let mut status = bsh_runtime::Status::SUCCESS;
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    loop {
        let mut line = Vec::new();
        let read = stdin.read_until(b'\n', &mut line);
        match read {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => {
                eprintln!("bsh: cannot read stdin: {error}");
                return ExitCode::from(1);
            }
        }
        pending.extend_from_slice(&line);
        match bsh_syntax::parse_bytes(&pending, &ParseOpts::DEFAULT) {
            ParseOutcome::NeedMoreInput { .. } => continue,
            ParseOutcome::Failed(diagnostics) => {
                let file = SourceFile::new("<stdin>", pending.as_slice());
                for diagnostic in &diagnostics {
                    write_stderr(&bsh_diag::render_compact(diagnostic, &file));
                }
                pending.clear();
                status = bsh_runtime::Status(2);
                continue;
            }
            ParseOutcome::Ok(_) => {}
        }
        let chunk = std::mem::take(&mut pending);
        // The status of the last chunk is the shell's exit status; an `exit`
        // inside it already requested the unwind, and the request survives in
        // the shell state, so the loop stops here too.
        status = shell.run_named("<stdin>", &chunk);
        if shell.state().take_exit_request().is_some() {
            break;
        }
    }
    ExitCode::from(status.exit_code() as u8)
}

/// Read stdin, paint it, write it back.
fn highlight_stdin() -> ExitCode {
    let mut input = Vec::new();
    if let Err(err) = std::io::stdin().read_to_end(&mut input) {
        eprintln!("bsh: cannot read stdin: {err}");
        return ExitCode::from(1);
    }

    let color = bsh_repl::color_supported(1);
    let painted = bsh_repl::paint(&input, &Theme::DARK, color);

    let mut stdout = std::io::stdout().lock();
    if let Err(err) = stdout.write_all(&painted).and_then(|()| stdout.flush()) {
        // EPIPE just means the reader went away; a shell must not treat that as
        // its own failure.
        if err.kind() != std::io::ErrorKind::BrokenPipe {
            eprintln!("bsh: cannot write stdout: {err}");
            return ExitCode::from(1);
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn dash_i_forces_interactive_semantics() {
        let (mode, interactive, _) = parse_args(&args(&["-i"])).unwrap();
        assert!(interactive);
        assert!(matches!(mode, Mode::Execute(Source::Stdin)));

        let (_, interactive, _) = parse_args(&args(&["--interactive", "-c", "x"])).unwrap();
        assert!(interactive);
    }

    #[test]
    fn a_command_is_not_interactive_by_default() {
        let (mode, interactive, _) = parse_args(&args(&["-c", "echo hi"])).unwrap();
        assert!(!interactive);
        match mode {
            Mode::Execute(Source::Command(text, argv)) => {
                assert_eq!(text, b"echo hi");
                assert!(argv.is_empty());
            }
            _ => panic!("expected -c to select a command"),
        }
    }

    #[test]
    fn a_script_file_carries_its_arguments() {
        let (mode, _, _) = parse_args(&args(&["script.sh", "a", "b"])).unwrap();
        match mode {
            Mode::Execute(Source::File(path, argv)) => {
                assert_eq!(path, b"script.sh");
                assert_eq!(argv, vec![b"a".to_vec(), b"b".to_vec()]);
            }
            _ => panic!("expected a script file"),
        }
    }

    #[test]
    fn parse_only_wins_over_execution() {
        let (mode, _, _) = parse_args(&args(&["-n", "-c", "x"])).unwrap();
        assert!(matches!(mode, Mode::ParseOnly(Source::Command(..))));
    }

    #[test]
    fn an_unknown_option_is_an_error() {
        assert!(parse_args(&args(&["--nope"])).is_err());
    }
}
