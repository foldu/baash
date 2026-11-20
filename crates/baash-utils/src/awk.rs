//! `awk`, in the child the shell spawned for it.
//!
//! The body is [`awk-rs`](https://github.com/quinnjr/rawk) (MIT OR Apache-2.0),
//! a POSIX awk with gawk extensions. It exists here for one measured reason:
//! bash-completion's scripts call `awk` through `_comp_awk` in 150 of their
//! files, and on Windows there is no `awk` to run — so `make`, `python` and
//! `curl` complete nothing (`docs/portability.md` §6).
//!
//! # Why a child, and not a body in the shell
//!
//! The same two reasons `sed` has, plus one of its own:
//!
//! * an awk program can `exit`, and an in-process `exit` ends the *shell*;
//! * awk programs open files, write through `print > file` and can run
//!   `system()`, which is a utility's business and not the shell's loop;
//! * awk-rs is a young implementation (0.2.0, one release on crates.io): an
//!   `unwrap()` on an input it has not seen would, in-process, be a panic in
//!   the middle of the session. A child contains it to one completion.
//!
//! # The command line this driver accepts
//!
//! awk's own, not `awk-rs`'s binary's: `-F fs` (or `-Ffs`), `-v var=val` (or
//! `-vvar=val`), `-f progfile` (repeatable; the texts are concatenated, as
//! POSIX says), `--`, `-` for standard input, the program text, and file
//! operands. `-P`/`--posix` asks awk-rs for strict POSIX mode.
//!
//! Two operand shapes are **refused** with a diagnostic instead of
//! approximated:
//!
//! * **Assignment operands** (`awk '{…}' a=1 file`). POSIX applies those when
//!   the operand is reached — between files — and awk-rs runs one program over
//!   all inputs with `BEGIN`/`END` once, so the only way to accept them would
//!   be to move the assignment before `BEGIN`, which is silently wrong. bash-
//!   completion never uses one.
//! * **Text that is not UTF-8.** awk-rs takes `&str`, and the shell's own
//!   contract is that a name which is not UTF-8 is a refusal, never a lossy
//!   conversion that would name a different file.
//!
//! Everything else is a diagnostic and status 2, the way awk reports a fatal
//! error: a bad option, a missing program file, a file that cannot be opened.

use std::fs;
use std::io::{self, BufRead, BufReader, Write};

use awk_rs::{Interpreter, Lexer, Parser};

/// The status awk uses for a fatal error.
const FATAL: i32 = 2;

/// `awk`, in the child [`crate::run_isolated`] spawned.
pub(crate) fn main(argv: &[Vec<u8>]) -> i32 {
    match run(argv) {
        Ok(status) => status,
        Err(message) => {
            eprintln!("awk: {message}");
            FATAL
        }
    }
}

fn run(argv: &[Vec<u8>]) -> Result<i32, String> {
    let mut field_separator = " ".to_owned();
    let mut variables: Vec<(String, String)> = Vec::new();
    let mut program_files: Vec<String> = Vec::new();
    let mut program: Option<String> = None;
    let mut files: Vec<String> = Vec::new();
    let mut posix = false;

    let mut index = 1;
    while index < argv.len() {
        let argument = text(&argv[index])?;
        match argument.as_str() {
            "--" => {
                index += 1;
                while index < argv.len() {
                    operand(&text(&argv[index])?, &mut files)?;
                    index += 1;
                }
                break;
            }
            "--version" => {
                println!("awk (embedded awk-rs)");
                return Ok(0);
            }
            "--help" => {
                print!("{USAGE}");
                return Ok(0);
            }
            "--posix" | "-P" => posix = true,
            "-F" => {
                index += 1;
                field_separator = option_argument(argv, index, "-F")?;
            }
            "-v" => {
                index += 1;
                let assignment = option_argument(argv, index, "-v")?;
                variables.push(assignment_of(&assignment, "-v")?);
            }
            "-f" => {
                index += 1;
                program_files.push(option_argument(argv, index, "-f")?);
            }
            "-" => operand("-", &mut files)?,
            other if other.starts_with("-F") => field_separator = other[2..].to_owned(),
            other if other.starts_with("-v") => {
                variables.push(assignment_of(&other[2..], "-v")?);
            }
            other if other.starts_with("-f") => program_files.push(other[2..].to_owned()),
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown option: {other}"));
            }
            other => {
                // awk takes a bare `name=value` operand as a variable
                // assignment made when its turn comes; see the module docs.
                if (program.is_some() || !program_files.is_empty()) && is_assignment(other) {
                    return Err(format!(
                        "assignment operands are not supported by the embedded awk: {other} \
                         (use -v, which is applied before BEGIN)"
                    ));
                }
                if program.is_none() && program_files.is_empty() {
                    program = Some(other.to_owned());
                } else {
                    files.push(other.to_owned());
                }
            }
        }
        index += 1;
    }

    // POSIX: with `-f` the operands are all files, and several `-f` texts are
    // concatenated in the order given.
    let source = if program_files.is_empty() {
        program.ok_or_else(|| "no program text".to_owned())?
    } else {
        let mut source = String::new();
        for path in &program_files {
            let text = fs::read_to_string(path)
                .map_err(|error| format!("cannot open program file {path}: {error}"))?;
            source.push_str(&text);
            if !source.ends_with('\n') {
                source.push('\n');
            }
        }
        source
    };

    let tokens = Lexer::new(&source)
        .tokenize()
        .map_err(|error| error.to_string())?;
    let program = Parser::new(tokens)
        .parse()
        .map_err(|error| error.to_string())?;
    let mut interpreter = Interpreter::new(&program);
    interpreter.set_posix_mode(posix);
    interpreter.set_fs(&field_separator);
    let mut awk_argv = vec!["awk".to_owned()];
    awk_argv.extend(files.iter().cloned());
    interpreter.set_args(awk_argv);
    for (name, value) in &variables {
        interpreter.set_variable(name, value);
    }

    let mut inputs: Vec<Box<dyn BufRead>> = Vec::new();
    let mut filenames: Vec<String> = Vec::new();
    if files.is_empty() {
        inputs.push(Box::new(BufReader::new(io::stdin())));
        filenames.push(String::new());
    } else {
        for path in &files {
            if path == "-" {
                inputs.push(Box::new(BufReader::new(io::stdin())));
            } else {
                let file =
                    fs::File::open(path).map_err(|error| format!("cannot open {path}: {error}"))?;
                inputs.push(Box::new(BufReader::new(file)));
            }
            filenames.push(path.clone());
        }
    }
    interpreter.set_filenames(filenames);

    let stdout = io::stdout();
    let mut output = stdout.lock();
    let status = interpreter
        .run(inputs, &mut output)
        .map_err(|error| error.to_string())?;
    output
        .flush()
        .map_err(|error| format!("write error: {error}"))?;
    Ok(status)
}

/// One argument as text: UTF-8 or a refusal, never a lossy conversion.
fn text(argument: &[u8]) -> Result<String, String> {
    std::str::from_utf8(argument)
        .map(str::to_owned)
        .map_err(|_| "argument is not valid UTF-8".to_owned())
}

/// The argument after an option that takes one.
fn option_argument(argv: &[Vec<u8>], index: usize, option: &str) -> Result<String, String> {
    argv.get(index)
        .ok_or_else(|| format!("option {option} requires an argument"))
        .and_then(|argument| text(argument))
}

/// A file operand, or the refusal for an assignment operand.
fn operand(operand: &str, files: &mut Vec<String>) -> Result<(), String> {
    if is_assignment(operand) {
        return Err(format!(
            "assignment operands are not supported by the embedded awk: {operand} \
             (use -v, which is applied before BEGIN)"
        ));
    }
    files.push(operand.to_owned());
    Ok(())
}

/// Whether `operand` has the shape awk reads as `name=value`.
fn is_assignment(operand: &str) -> bool {
    let Some((name, _)) = operand.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Split `name=value` for a `-v` option, decoding the escapes POSIX gives it.
fn assignment_of(assignment: &str, option: &str) -> Result<(String, String), String> {
    let Some((name, value)) = assignment.split_once('=') else {
        return Err(format!(
            "invalid assignment for {option}: {assignment} (expected name=value)"
        ));
    };
    if !is_assignment(assignment) {
        return Err(format!("invalid variable name for {option}: {name}"));
    }
    Ok((name.to_owned(), unescape(value)))
}

/// Decode the escapes in a `-v` value, exactly as awk reads a string literal.
///
/// POSIX: "the escape sequences shall be processed in the same way as in a
/// string literal", which is what makes `-v x='a\tb'` mean a tab. An unknown
/// escape keeps the character itself, as awk does.
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match characters.peek().copied() {
            Some('n') => {
                characters.next();
                out.push('\n');
            }
            Some('t') => {
                characters.next();
                out.push('\t');
            }
            Some('r') => {
                characters.next();
                out.push('\r');
            }
            Some('b') => {
                characters.next();
                out.push('\u{8}');
            }
            Some('f') => {
                characters.next();
                out.push('\u{c}');
            }
            Some('a') => {
                characters.next();
                out.push('\u{7}');
            }
            Some('v') => {
                characters.next();
                out.push('\u{b}');
            }
            Some(digit) if digit.is_digit(8) => {
                let mut octal = String::new();
                while octal.len() < 3 {
                    match characters.peek().copied() {
                        Some(next) if next.is_digit(8) => {
                            octal.push(next);
                            characters.next();
                        }
                        _ => break,
                    }
                }
                let byte = u32::from_str_radix(&octal, 8)
                    .ok()
                    .and_then(|value| u8::try_from(value).ok())
                    .map(char::from);
                match byte {
                    Some(character) => out.push(character),
                    None => {
                        out.push('\\');
                        out.push_str(&octal);
                    }
                }
            }
            Some('x') => {
                characters.next();
                let mut hex = String::new();
                while hex.len() < 2 {
                    match characters.peek().copied() {
                        Some(next) if next.is_ascii_hexdigit() => {
                            hex.push(next);
                            characters.next();
                        }
                        _ => break,
                    }
                }
                let byte = u32::from_str_radix(&hex, 16)
                    .ok()
                    .and_then(|value| u8::try_from(value).ok())
                    .map(char::from);
                match byte {
                    Some(character) => out.push(character),
                    None => {
                        out.push('x');
                        out.push_str(&hex);
                    }
                }
            }
            Some(other) => {
                characters.next();
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Printed by `awk --help`; awk has no `--help`, so this is a courtesy.
const USAGE: &str = "\
usage: awk [-F fs] [-v var=value] ['program' | -f progfile] [file ...]

  -F fs            set the field separator to fs
  -v var=value     assign a variable before the program runs
  -f progfile      read the program from progfile (repeatable)
  --posix, -P      strict POSIX mode, no gawk extensions
  --version        print version information
  --help           print this message

The body is awk-rs; `docs/portability.md` §6 records what it does not do yet.
";
