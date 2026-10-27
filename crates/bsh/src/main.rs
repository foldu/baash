//! The ßhell binary.
//!
//! Scope right now is the scaffold: argument dispatch, and one feature that is
//! genuinely finished, `--highlight`, which exercises the `bsh-syntax` →
//! `bsh-highlight` → `bsh-repl` chain end to end.

#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::process::ExitCode;

use bsh_repl::Theme;

const USAGE: &str = "\
ßhell — a POSIX- and bash-compatible shell

USAGE
    bsh [OPTIONS] [SCRIPT]

OPTIONS
    -c <COMMAND>    Run COMMAND.
    -n              Parse SCRIPT (or stdin) without executing.
    --lint          Report non-POSIX constructs.
    --highlight     Syntax-highlight stdin to stdout and exit.
    -h, --help      Print this help.
    -V, --version   Print the version.

Only --highlight is implemented so far; the rest exits with status 2.
";

/// What the invocation asked for.
enum Mode {
    Help,
    Version,
    Highlight,
    ParseOnly(Option<String>),
    Lint(Option<String>),
    Execute(Option<String>),
    Run(Option<String>),
}

/// Exit status for "this is not implemented yet", mirroring the shell
/// convention of refusing to pretend.
const UNIMPLEMENTED: u8 = 2;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&args) {
        Ok(mode) => dispatch(mode),
        Err(message) => {
            eprintln!("bsh: {message}");
            eprintln!("Try 'bsh --help' for more information.");
            ExitCode::from(2)
        }
    }
}

fn parse_args(args: &[String]) -> Result<Mode, String> {
    let mut i = 0;
    let mut mode: Option<Mode> = None;
    let mut positional: Option<String> = None;

    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "-h" | "--help" => mode = Some(Mode::Help),
            "-V" | "--version" => mode = Some(Mode::Version),
            "--highlight" => mode = Some(Mode::Highlight),
            "--lint" => mode = Some(Mode::Lint(None)),
            "-n" => mode = Some(Mode::ParseOnly(None)),
            "-c" => {
                i += 1;
                let command = args
                    .get(i)
                    .ok_or_else(|| "-c requires an argument".to_string())?;
                mode = Some(Mode::Execute(Some(command.clone())));
            }
            _ if arg.starts_with('-') && arg.len() > 1 => {
                return Err(format!("unknown option `{arg}`"));
            }
            _ => {
                if positional.replace(arg.to_string()).is_some() {
                    return Err(format!("unexpected extra argument `{arg}`"));
                }
            }
        }
        i += 1;
    }

    Ok(match mode {
        Some(Mode::Lint(_)) => Mode::Lint(positional),
        Some(Mode::ParseOnly(_)) => Mode::ParseOnly(positional),
        Some(other) => other,
        None => Mode::Run(positional),
    })
}

fn dispatch(mode: Mode) -> ExitCode {
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
        Mode::Run(input) => not_yet("execution and the interactive editor", input.as_deref()),
        Mode::Execute(command) => not_yet("`-c` execution", command.as_deref()),
        Mode::ParseOnly(input) => not_yet("parsing", input.as_deref()),
        Mode::Lint(input) => not_yet("`--lint`", input.as_deref()),
    }
}

fn not_yet(what: &str, subject: Option<&str>) -> ExitCode {
    match subject {
        Some(subject) => eprintln!("bsh: {what} is not implemented yet ({subject})"),
        None => eprintln!("bsh: {what} is not implemented yet"),
    }
    ExitCode::from(UNIMPLEMENTED)
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
