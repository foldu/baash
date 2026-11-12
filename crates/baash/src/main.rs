//! The baash binary.
//!
//! Argument dispatch plus the three ways to give a shell some text: `-c`, a
//! script file, and standard input. Parsing, execution and diagnostics all
//! live in the crates; this file decides *which* bytes get run, sets `$0` and
//! the positional parameters, and turns the final status into an exit code.
//!
//! `-n` is the one mode that never executes: it parses and renders
//! `baash_diag::render_report`, exiting 2 on a syntax error, because a syntax
//! error must be visible *before* any side effect (DESIGN.md §5.6).

#![forbid(unsafe_code)]

/// Windows builds use mimalloc instead of the system heap.
///
/// A shell allocates small byte vectors constantly — every word, every variable
/// value, every argument — and `HeapAlloc` is the slow part of that on Windows.
/// The Unix builds keep the system allocator: measured against it, mimalloc
/// trades startup and small loops for large-array throughput, which is not a
/// trade a shell should make by default (`docs/performance.md`).
#[cfg(feature = "mimalloc")]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::ffi::OsString;
use std::io::{BufRead, Read, Write};
use std::process::ExitCode;
use std::time::Duration;

use baash_diag::{Diagnostic, SourceFile};
use baash_lang::histexpand;
use baash_repl::Theme;
use baash_runtime::ShellHost;
use baash_syntax::{ParseOpts, ParseOutcome};
use baash_vm::Interp;

mod bash_completion;
mod carapace_ext;
mod complete;
mod git_prompt;
mod inputrc;
mod prompt_state;

const USAGE: &str = "\
baash — a POSIX- and bash-compatible shell

USAGE
    baash [OPTIONS] [SCRIPT [ARGUMENT...]]
    baash -c COMMAND [NAME [ARGUMENT...]]

OPTIONS
    -c <COMMAND>    Run COMMAND.
    -i              Interactive: read commands from a terminal with a prompt.
    -n              Parse SCRIPT (or stdin) without executing.
    --highlight     Syntax-highlight stdin to stdout and exit.
    --regex-engine <NAME>
                    Engine for `[[ x =~ y ]]`: `system` (the platform engine,
                    which is what bash uses; the default) or `minrx` (portable;
                    the only engine on Windows). `glibc` is accepted as an
                    alias for `system`. The environment variable
                    `BAASH_REGEX_ENGINE` is read when the flag is absent.
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
    /// `--rcfile FILE`: the startup file to read instead of the default
    /// (`$XDG_CONFIG_HOME/baash/config.sh`, or `~/.bashrc` without one).
    rcfile: Option<std::path::PathBuf>,
    /// `--rcdir DIR`: a directory of startup files, read in name order after
    /// `--rcfile` (an osh extension; bash has no equivalent).
    rcdir: Option<std::path::PathBuf>,
    /// `--norc`: read no startup file at all.
    norc: bool,
    /// `--regex-engine NAME`: which POSIX ERE engine `[[ =~ ]]` uses.
    /// `BAASH_REGEX_ENGINE` stands in when the flag is absent.
    regex_engine: Option<Vec<u8>>,
    /// `--eval FILE`: run these files, in order, before the command or script.
    /// The positional parameters are the ones the command text will get, so
    /// `baash --eval setup.sh -c 'echo "$@"' name a b` sees `a b` in both.
    eval: Vec<std::path::PathBuf>,
}

impl ShellFlags {
    fn apply(&self, opts: &mut baash_runtime::Opts) -> Result<(), String> {
        for name in &self.shopt_set {
            baash_runtime::Opts::set_shopt_by_name(opts, name, true)
                .map_err(|error| format!("-O {}: {error}", String::from_utf8_lossy(name)))?;
        }
        for name in &self.shopt_clear {
            baash_runtime::Opts::set_shopt_by_name(opts, name, false)
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
    fn apply(&self, opts: &mut baash_runtime::Opts, on: bool) -> Result<(), String> {
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
            eprintln!("baash: {message}");
            eprintln!("Try 'baash --help' for more information.");
            ExitCode::from(2)
        }
    }
}

fn bytes(value: &OsString) -> Vec<u8> {
    baash_sys::os::bytes_from_os(value)
}

fn parse_args(args: &[OsString]) -> Result<(Mode, bool, ShellFlags), String> {
    let mut index = 0;
    let mut command: Option<Vec<u8>> = None;
    let mut parse_only = false;
    let mut highlight = false;
    let mut interactive = false;
    let mut flags = ShellFlags::default();
    let mut positional: Vec<Vec<u8>> = Vec::new();
    // `--`, `-`, the script name and the command text all end option
    // processing: everything after them is a positional argument, even when it
    // looks like a flag (`baash script.sh --help` must pass `--help` on).
    let mut options_done = false;
    // `-c` does not take the text it will run from the next word directly:
    // bash keeps parsing options after it, so `-c -x 'echo hi'` traces the
    // command and `-c -z` is an invalid option. The first word that is not an
    // option becomes the command text once `-c` has been seen.
    let mut command_pending = false;

    while index < args.len() {
        let argument = bytes(&args[index]);
        if options_done || command.is_some() || !positional.is_empty() {
            if command_pending && command.is_none() && positional.is_empty() {
                command = Some(argument);
            } else {
                positional.push(argument);
            }
            index += 1;
            continue;
        }
        match argument.as_slice() {
            b"-h" | b"--help" => return Ok((Mode::Help, false, ShellFlags::default())),
            b"-V" | b"--version" => return Ok((Mode::Version, false, ShellFlags::default())),
            b"--highlight" => highlight = true,
            b"--interactive" => interactive = true,
            b"--lint" => {
                return Ok((
                    Mode::Lint(source_from(&positional, &mut None)?),
                    false,
                    ShellFlags::default(),
                ));
            }
            // `-` stops flag processing too, and is not a script name: the
            // next argument is. A lone `-` with nothing after it reads stdin.
            b"--" => options_done = true,
            b"-" => options_done = true,
            b"--rcfile" => {
                index += 1;
                let Some(path) = args.get(index) else {
                    return Err("--rcfile requires a path".to_string());
                };
                flags.rcfile = Some(std::path::PathBuf::from(path));
            }
            b"--rcdir" => {
                index += 1;
                let Some(path) = args.get(index) else {
                    return Err("--rcdir requires a path".to_string());
                };
                flags.rcdir = Some(std::path::PathBuf::from(path));
            }
            b"--norc" => flags.norc = true,
            b"--regex-engine" => {
                index += 1;
                let Some(name) = args.get(index) else {
                    return Err("--regex-engine requires a name".to_string());
                };
                flags.regex_engine = Some(bytes(name));
            }
            b"--eval" => {
                index += 1;
                let Some(path) = args.get(index) else {
                    return Err("--eval requires a path".to_string());
                };
                flags.eval.push(std::path::PathBuf::from(path));
            }
            // A login shell reads profile files this shell does not have;
            // accepting the flag keeps `baash -l` (and `env -i baash -lc …`)
            // working instead of failing on an unknown option.
            b"--login" => {}
            other if other.starts_with(b"--") => {
                // Every long option this shell has is matched above; the rest
                // are errors rather than a bundle of `set` letters.
                return Err(format!(
                    "unknown option `{}`",
                    String::from_utf8_lossy(other)
                ));
            }
            other if other.len() > 1 && (other[0] == b'-' || other[0] == b'+') => {
                // A bundle of letters: `-eu`, `+x`, `-oo errexit`. Each `o`/`O`
                // takes the *next argument* as its name, even when it is
                // attached in the same word, which is why `-oo errexit noglob`
                // sets both options.
                let on = other[0] == b'-';
                for letter in &other[1..] {
                    match letter {
                        // Accepted and ignored: login, and `-s` (read stdin,
                        // which is already the default without a script name).
                        b'l' | b's' => {}
                        b'c' => command_pending = true,
                        b'i' => interactive = true,
                        b'n' => parse_only = true,
                        b'o' | b'O' => {
                            index += 1;
                            let Some(name) = args.get(index) else {
                                return Err(format!(
                                    "{} requires an argument",
                                    char::from(*letter)
                                ));
                            };
                            let name = bytes(name);
                            if *letter == b'O' {
                                // `-O extglob` is how bash lets a *script* be
                                // parsed with extglob on: the decision is
                                // parse-time, so it has to be a command-line
                                // flag rather than a `shopt` the file runs.
                                if !baash_runtime::Opts::is_shopt_name(&name) {
                                    return Err(format!(
                                        "unknown shopt option `{}`",
                                        String::from_utf8_lossy(&name)
                                    ));
                                }
                                if on {
                                    flags.shopt_set.push(name);
                                } else {
                                    flags.shopt_clear.push(name);
                                }
                            } else if on {
                                flags.set.push(FlagSpec::Named(name));
                            } else {
                                flags.clear.push(FlagSpec::Named(name));
                            }
                        }
                        short => {
                            if on {
                                flags.set.push(FlagSpec::Short(*short));
                            } else {
                                flags.clear.push(FlagSpec::Short(*short));
                            }
                        }
                    }
                }
            }
            _ => {
                if command_pending && command.is_none() {
                    command = Some(argument);
                } else {
                    positional.push(argument);
                }
            }
        }
        index += 1;
    }

    if command_pending && command.is_none() {
        return Err("-c requires an argument".to_string());
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
            println!("baash {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Mode::Highlight => highlight_stdin(),
        Mode::Lint(source) => {
            eprintln!("baash: `--lint` is not implemented yet");
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
            let stdin_tty = baash_sys::is_tty(0);
            let from_stdin = matches!(source, Source::Stdin);
            let repl = from_stdin && stdin_tty;
            let interactive = interactive_flag || repl;
            // `--rcfile` is an interactive-shell option. bash silently ignores
            // it otherwise, which hides a typo'd wrapper script; say so, but
            // only when the file is really not read.
            if !interactive && flags.rcfile.is_some() {
                eprintln!("baash: warning: --rcfile is only read by interactive shells");
            }
            // An interactive shell survives `Ctrl-C`: the console gives the
            // event to the foreground child too, so the child dies and the
            // prompt comes back, as a Unix shell ignores `SIGINT` while it
            // waits for a foreground job. A script keeps the default handler.
            #[cfg(windows)]
            if interactive {
                let _ = baash_sys::windows::install_console_control_handler();
            }
            let mut shell = shell(interactive, &flags);
            // bash reads a startup file only for an interactive shell, and
            // `--rcfile` replaces the default rather than adding to it.
            if interactive {
                read_startup_file(&mut shell, &flags);
                // After the rc file, so anything it changed is already in
                // place. Only the terminal REPL opens the history database; a
                // piped `-i` must not depend on (or write to) the user's data
                // directory, which is what keeps conformance deterministic.
                shell.initialize_history(repl);
            }
            if !flags.eval.is_empty() {
                // The eval files run in the same shell, so `$?` carries the
                // last one's status into the command; `execute` then resets
                // `$0`/`$@` for the command itself.
                let arguments = match &source {
                    // `argv[0]` is the name the command text gets, not a
                    // positional parameter.
                    Source::Command(_, argv) => argv.get(1..).unwrap_or(&[]).to_vec(),
                    Source::File(_, argv) => argv.clone(),
                    Source::Stdin => Vec::new(),
                };
                for path in &flags.eval {
                    let _ = execute(
                        &mut shell,
                        Source::File(baash_sys::os::bytes_from_os(path), arguments.clone()),
                    );
                }
            }
            if repl {
                run_repl_on_runtime(&mut shell)
            } else {
                let code = execute(&mut shell, source);
                if interactive {
                    // `-i -c` and interactive stdin that is not a terminal:
                    // the editor is not there to drain the write queue.
                    shell.flush_history();
                }
                code
            }
        }
    }
}

/// The default configuration shipped inside the binary.
///
/// `config/config.sh` in the source tree, compiled in so a plain `cargo build`
/// behaves like an installed shell. A distributor can override it by
/// installing `$XDG_CONFIG_DIRS/baash/config.sh`, which is read instead.
const DEFAULT_CONFIG: &[u8] = include_bytes!("../../../config/config.sh");

/// The name the built-in default is reported under in diagnostics.
const DEFAULT_CONFIG_NAME: &str = "(built-in config)";

/// The `PATH` a shell gives itself when the environment has none.
///
/// Its own value, rather than `baash_sys::DEFAULT_PATH`: this one also names
/// the `sbin` directories, which is what a login shell on Unix wants, while
/// the command search only needs the three. Windows keeps its system
/// directories, which is where the platform puts its own tools.
#[cfg(not(windows))]
const SEEDED_PATH: &[u8] = b"/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin";
/// The `PATH` a shell gives itself when the environment has none.
#[cfg(windows)]
const SEEDED_PATH: &[u8] = b"C:\\Windows\\system32;C:\\Windows";

/// Read the interactive startup files.
///
/// `--norc` skips them and `--rcfile FILE` replaces them. Otherwise there are
/// two layers, in the order an interactive shell wants them:
///
/// 1. **the shell's defaults** — `$XDG_CONFIG_DIRS/baash/config.sh` if a
///    distributor installed one, else the copy built into the binary
///    ([`DEFAULT_CONFIG`]);
/// 2. **the user's own** — `$XDG_CONFIG_HOME/baash/config.sh`
///    (`~/.config/baash/config.sh`), or `~/.bashrc` when there is none.
///
/// A missing file is silently ignored at every step, which is what keeps
/// `--rcfile /dev/null` the portable way to ask for no startup file. The user
/// file is the XDG one and lives under the configuration directory rather than
/// in `$HOME`: a bash rc full of `bind`/`complete -D` half-applies in this
/// shell, so a user who wants a baash-specific file gets one without renaming
/// the bash file.
fn read_startup_file(shell: &mut Interp, flags: &ShellFlags) {
    if flags.norc {
        return;
    }
    // The bash-completion library the binary carries, mounted in memory and
    // named for the startup files: a machine with none installed still gets
    // one, and `config/config.sh` prefers a copy the system has. A value
    // already in the environment is the user's and is left alone.
    if shell
        .state()
        .vars
        .scalar(b"BAASH_EMBEDDED_BASH_COMPLETION")
        .is_none()
    {
        if let Some(path) = bash_completion::install(shell) {
            let _ = shell
                .state()
                .vars
                .assign_scalar(b"BAASH_EMBEDDED_BASH_COMPLETION", &path);
        }
    }
    read_rc_files(shell, flags);
    read_rcdir(shell, flags);
}

/// The startup files the flags name: `--rcfile`, or the default layers.
fn read_rc_files(shell: &mut Interp, flags: &ShellFlags) {
    if let Some(path) = &flags.rcfile {
        run_startup_file(shell, path);
        return;
    }
    match system_config() {
        Some(path) => run_startup_file(shell, &path),
        None => {
            shell.run_named(DEFAULT_CONFIG_NAME, DEFAULT_CONFIG);
        }
    }
    if let Some(path) = user_config() {
        run_startup_file(shell, &path);
    }
}

/// Read and run one startup file; a file that cannot be read is not an error.
fn run_startup_file(shell: &mut Interp, path: &std::path::Path) {
    let Ok(bytes) = std::fs::read(path) else {
        return;
    };
    shell.run_named(&path.to_string_lossy(), &bytes);
}

/// The distributor's configuration file, if one is installed.
///
/// The first `$XDG_CONFIG_DIRS` entry that has a `baash/config.sh` wins, and
/// nothing after it is consulted: the search path is an override list, not a
/// set of fragments to concatenate.
fn system_config() -> Option<std::path::PathBuf> {
    baash_sys::dirs::config_dirs()
        .into_iter()
        .map(|dir| dir.join("baash").join("config.sh"))
        .find(|path| path.is_file())
}

/// The user's own startup file: the baash one, or a bash rc as a fallback.
fn user_config() -> Option<std::path::PathBuf> {
    let ours = baash_sys::dirs::config_home().map(|dir| dir.join("baash").join("config.sh"));
    if let Some(path) = ours.filter(|path| path.is_file()) {
        return Some(path);
    }
    baash_sys::dirs::home_dir()
        .map(|home| home.join(".bashrc"))
        .filter(|path| path.is_file())
}

/// The engine `[[ =~ ]]` compiles with: `--regex-engine`, then
/// `BAASH_REGEX_ENGINE`, then the platform default.
///
/// The name is resolved in the binary, not in `baash-sys`, because an engine
/// this platform does not have is a usage error the user can act on, not a
/// compile failure that surfaces at the first `=~`.
fn regex_engine(flags: &ShellFlags) -> Result<baash_sys::Engine, String> {
    let name = match &flags.regex_engine {
        Some(name) => Some(String::from_utf8_lossy(name).into_owned()),
        None => std::env::var_os("BAASH_REGEX_ENGINE").map(|value| {
            String::from_utf8_lossy(&baash_sys::os::bytes_from_os(&value)).into_owned()
        }),
    };
    let engine = match name {
        None => baash_sys::Engine::default(),
        Some(name) => baash_sys::Engine::from_name(&name).ok_or_else(|| {
            format!("unknown regular-expression engine `{name}` (expected `system` or `minrx`)")
        })?,
    };
    if !engine.is_available() {
        return Err(format!(
            "the `{engine}` regular-expression engine is not available on this platform; use `minrx`"
        ));
    }
    Ok(engine)
}

/// `--rcdir DIR`: every regular file in the directory, in name order.
///
/// A missing or unreadable directory is ignored, exactly like a missing
/// `--rcfile`: a wrapper script that points at a directory this machine does
/// not have must not stop the shell from starting.
fn read_rcdir(shell: &mut Interp, flags: &ShellFlags) {
    let Some(dir) = &flags.rcdir else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    paths.sort();
    for path in paths {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        shell.run_named(&path.to_string_lossy(), &bytes);
    }
}

fn shell(interactive: bool, flags: &ShellFlags) -> Interp {
    let mut shell = Interp::new(interactive);
    // bash enables history expansion for every interactive shell: `-H` is in
    // `$-` even for `-i -c`, which never consults it because a `-c` string is
    // not a history line. Command-line flags are applied below, so `baash +H -i`
    // still wins.
    shell.state().opts.histexpand = interactive;
    // The one place that knows it is running for a user rather than a test:
    // character widths come from the environment here and nowhere else.
    shell.detect_locale();
    // The regex engine is a usage decision, so it is resolved here and a bad
    // name exits like any other bad option rather than failing at the first
    // `[[ x =~ y ]]` the user writes.
    match regex_engine(flags) {
        Ok(engine) => shell.set_regex_engine(engine),
        Err(message) => {
            eprintln!("baash: {message}");
            eprintln!("Try 'baash --help' for more information.");
            std::process::exit(2);
        }
    }
    // bash and osh warn once when the environment names a locale the C library
    // does not know; without the warning the fallback to the C locale is
    // invisible, and globbing quietly changes behaviour.
    let locale = std::env::var_os("LC_ALL").or_else(|| std::env::var_os("LC_CTYPE"));
    if let Some(locale) = locale {
        let name = baash_sys::os::bytes_from_os(&locale);
        if !name.is_empty() && !baash_sys::locale_exists(&name) {
            eprintln!(
                "baash: warning: {}: invalid locale",
                String::from_utf8_lossy(&name)
            );
        }
    }
    if let Err(message) = flags.apply(&mut shell.state().opts) {
        eprintln!("baash: {message}");
        eprintln!("Try 'baash --help' for more information.");
        std::process::exit(2);
    }
    shell.import_environment();
    // `$0` is the shell's own name until a script or `-c` name replaces it;
    // a shell reading stdin still has one, which is what makes `${@:0}`
    // include it.
    if let Some(name) = std::env::args_os().next() {
        shell.set_arg0(baash_sys::os::bytes_from_os(name));
    }
    // A shell always has a `PATH`, even when the environment does not: a
    // script that runs with `env -i` must still find `/bin/sh` and friends.
    if shell.state().vars.scalar(b"PATH").is_none() {
        let _ = shell.state().vars.assign_scalar(b"PATH", SEEDED_PATH);
        let _ = shell.state().vars.mark_exported(b"PATH");
    }
    // `PWD` is an *assertion* about the process's directory, and an inherited
    // one can be stale (`su`, a chroot, a wrapper script). bash replaces it
    // with the real path in that case; otherwise `$PWD`, `cd ..` and `\w` all
    // lie. A valid logical path (a symlinked `$PWD` that still names this
    // directory) is kept, which is why the comparison is by device and inode
    // rather than by string.
    if let Ok(directory) = std::env::current_dir() {
        let names_cwd = shell
            .state()
            .vars
            .scalar(b"PWD")
            .is_some_and(|pwd| same_directory(pwd, &directory));
        if !names_cwd {
            let _ = shell
                .state()
                .vars
                .assign_scalar(b"PWD", &baash_sys::os::bytes_from_os(&directory));
            let _ = shell.state().vars.mark_exported(b"PWD");
        }
    }
    shell
}

/// Whether `pwd` names the same directory as `directory`.
fn same_directory(pwd: &[u8], directory: &std::path::Path) -> bool {
    let env_path = baash_sys::os::path_from_bytes(pwd);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(from_env) = std::fs::metadata(env_path) else {
            return false;
        };
        let Ok(actual) = std::fs::metadata(directory) else {
            return false;
        };
        from_env.dev() == actual.dev() && from_env.ino() == actual.ino()
    }
    #[cfg(not(unix))]
    {
        // There is no device/inode pair; canonical paths answer the same
        // question for a directory that exists under both spellings.
        match (
            std::fs::canonicalize(env_path),
            std::fs::canonicalize(directory),
        ) {
            (Ok(env), Ok(actual)) => env == actual,
            _ => false,
        }
    }
}

fn execute(shell: &mut Interp, source: Source) -> ExitCode {
    match source {
        Source::Stdin => run_stdin(shell),
        other => {
            // A script *file* is bash's `main` frame: `${FUNCNAME[@]}` in it is
            // `main`, while a `-c` string or stdin has no frame at all (which
            // is why `set -u` makes `$FUNCNAME` an error there).
            let script = match &other {
                Source::File(path, _) => Some(path.clone()),
                _ => None,
            };
            match read_source(shell, other) {
                Ok((name, bytes)) => {
                    let display = String::from_utf8_lossy(&name).into_owned();
                    let framed = script.is_some();
                    if let Some(path) = &script {
                        shell.push_call(b"main", path, 0);
                    }
                    let status = shell.run_named(&display, &bytes);
                    if framed {
                        shell.pop_call();
                    }
                    ExitCode::from(status.exit_code() as u8)
                }
                Err(code) => code,
            }
        }
    }
}

/// Read the bytes a [`Source`] names, and set `$0`/positional for it.
///
/// The text is decoded once here rather than in the parser: a script written
/// on Windows carries a byte order mark and `\r\n` line endings, and both are
/// settled before anything looks at tokens (`baash_syntax::decode`). That also
/// keeps the diagnostics pointing at the text the user sees, because the
/// `SourceFile` is built from these same bytes.
fn read_source(shell: &mut Interp, source: Source) -> Result<(Vec<u8>, Vec<u8>), ExitCode> {
    match source {
        Source::Command(text, argv) => {
            let (name, arguments) = match argv.split_first() {
                Some((name, rest)) => (name.clone(), rest.to_vec()),
                None => (b"baash".to_vec(), Vec::new()),
            };
            shell.set_arg0(name.clone());
            shell.set_positional(arguments);
            Ok((name, baash_syntax::decode(&text).into_owned()))
        }
        Source::File(path, argv) => match std::fs::read(baash_sys::os::path_from_bytes(&path)) {
            Ok(bytes) => {
                shell.set_arg0(path.clone());
                shell.set_positional(argv);
                Ok((path, baash_syntax::decode(&bytes).into_owned()))
            }
            Err(error) => {
                eprintln!("baash: {}: {error}", String::from_utf8_lossy(&path));
                Err(ExitCode::from(127))
            }
        },
        Source::Stdin => {
            let mut bytes = Vec::new();
            if let Err(error) = std::io::stdin().read_to_end(&mut bytes) {
                eprintln!("baash: cannot read stdin: {error}");
                return Err(ExitCode::from(1));
            }
            shell.set_arg0("baash");
            Ok((
                b"<stdin>".to_vec(),
                baash_syntax::decode(&bytes).into_owned(),
            ))
        }
    }
}

/// Write to stderr without panicking when the reader has gone away.
///
/// `eprint!` panics on a failed write, and a diagnostic report can be far
/// larger than the pipe buffer, so `baash -n … | head` must not turn a syntax
/// error into exit status 101.
fn write_stderr(text: &str) {
    let mut stderr = std::io::stderr().lock();
    let _ = stderr
        .write_all(text.as_bytes())
        .and_then(|()| stderr.flush());
}

fn parse_only(name: &[u8], bytes: &[u8]) -> ExitCode {
    let file = SourceFile::new(String::from_utf8_lossy(name).into_owned(), bytes);
    match baash_syntax::parse_with(&file, &ParseOpts::DEFAULT) {
        ParseOutcome::Ok(_) => ExitCode::SUCCESS,
        ParseOutcome::NeedMoreInput { open, .. } => {
            let diagnostic = Diagnostic::error(
                baash_syntax::codes::NEED_MORE_INPUT,
                open,
                "unexpected end of file: the input is incomplete",
            );
            write_stderr(&baash_diag::render_report(&[diagnostic], &file));
            ExitCode::from(2)
        }
        ParseOutcome::Failed(diagnostics) => {
            write_stderr(&baash_diag::render_report(&diagnostics, &file));
            ExitCode::from(2)
        }
    }
}

/// The parser's "is this command finished?" answer, as the editor needs it.
struct ShellSyntax;

impl baash_repl::Syntax for ShellSyntax {
    fn needs_more_input(&self, buffer: &[u8]) -> bool {
        matches!(
            baash_syntax::parse_bytes(buffer, &ParseOpts::DEFAULT),
            ParseOutcome::NeedMoreInput { .. }
        )
    }

    /// A rejected parse, for the hint under the line.
    ///
    /// Only `Failed` is reported: `NeedMoreInput` is a line still being typed —
    /// an open quote, an `if` without its `fi` — and painting an error for it
    /// would flash on every keystroke of a block the user is about to close.
    /// The first diagnostic is the one at the front of the line, which is the
    /// one a prompt-sized message should name; its help, if any, is appended
    /// because the span alone does not say what to do.
    fn check(&self, buffer: &[u8]) -> Option<baash_repl::LineError> {
        let ParseOutcome::Failed(diagnostics) =
            baash_syntax::parse_bytes(buffer, &ParseOpts::DEFAULT)
        else {
            return None;
        };
        let first = diagnostics.first()?;
        let mut message = first.message.clone();
        if let Some(help) = &first.help {
            message.push_str(" — ");
            message.push_str(help);
        }
        Some(baash_repl::LineError {
            span: first.span.clone(),
            message,
        })
    }
}

/// The `PROMPT_COMMAND` commands, in bash's order.
///
/// bash 5 accepts a scalar (one command string, `;` still separates commands
/// inside it) or an indexed array (each element is one command). Both are
/// cheap here because [`baash_runtime::Vars`] already stores arrays.
fn prompt_commands(shell: &mut Interp) -> Vec<Vec<u8>> {
    match shell.state().vars.value(b"PROMPT_COMMAND") {
        Some(baash_runtime::VarValue::Str(command)) if !command.is_empty() => {
            vec![command.clone()]
        }
        Some(baash_runtime::VarValue::Indexed(elements)) => elements.values().cloned().collect(),
        _ => Vec::new(),
    }
}

/// A prompt template from the shell's variable table.
///
/// The environment is not the table: an rc file, a `PS1=` typed at the prompt
/// and an exported `PS1` all end up here, and only the shell's copy is current.
fn prompt_template(shell: &mut Interp, name: &[u8]) -> Option<Vec<u8>> {
    shell.state().vars.scalar(name).map(<[u8]>::to_vec)
}

/// Run `PROMPT_COMMAND`, whose stdout appears above the prompt.
///
/// `$?` is restored afterwards: bash's prompt command sees the status of the
/// last command, and the next command sees it too (measured against 5.3.20).
/// A failure is not fatal here even under `set -e`; bash exits in that case,
/// which is recorded as a known difference in `docs/state.md`.
fn run_prompt_command(shell: &mut Interp) {
    let commands = prompt_commands(shell);
    if commands.is_empty() {
        return;
    }
    let status = shell.state().vars.last_status();
    for command in commands {
        run_prompt_command_line(shell, &command);
    }
    shell.restore_status(status);
}

/// Run one `PROMPT_COMMAND` in this shell with standard output captured, then
/// write what it produced above the prompt.
///
/// bash leaves its stdout on the terminal, so a command whose output does not
/// end in a newline leaves the prompt on the same line. The editor paints from
/// column zero because it cannot see what a command wrote, so the output is
/// collected and re-emitted with a final newline when it needs one: the prompt
/// then always starts on a fresh line. A write to `/dev/tty` bypasses the
/// capture, exactly as it bypasses bash's column tracking.
fn run_prompt_command_line(shell: &mut Interp, command: &[u8]) {
    let Some(output) = capture_stdout(shell, command) else {
        // The capture failed (no descriptors, no temporary file). Running the
        // command with the terminal as its stdout is worse for the screen than
        // not running it at all, which is what bash would do if it could not
        // fork; stay silent and keep the shell usable.
        return;
    };
    if output.is_empty() {
        return;
    }
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(&output);
    if !output.ends_with(b"\n") {
        let _ = stdout.write_all(b"\n");
    }
    let _ = stdout.flush();
}

/// Run `command` in this shell and return everything it wrote to stdout.
///
/// A seekable temporary file, not a pipe: the command runs *in this process*
/// (so `PROMPT_COMMAND='PS1=…'` takes effect), and a pipe could fill while the
/// command is still writing, deadlocking the shell. `stderr` is left alone.
fn capture_stdout(shell: &mut Interp, command: &[u8]) -> Option<Vec<u8>> {
    use baash_sys::AsRawFd;

    let saved = baash_sys::dup(1).ok()?;
    let temp = baash_sys::create_unlinked_temp(0).ok()?;
    if baash_sys::dup2(temp.as_raw_fd(), 1).is_err() {
        return None;
    }
    // Rust's stdout is line-buffered: without this, a command that printed no
    // newline would leave its bytes in the buffer and they would surface, in
    // the wrong order, after the descriptor was restored.
    let _ = std::io::stdout().flush();
    shell.run_named("<PROMPT_COMMAND>", command);
    let _ = std::io::stdout().flush();
    baash_sys::dup2(saved.as_raw_fd(), 1).ok()?;

    // `SEEK_SET`, which `baash-sys` names only as the raw `whence` argument.
    const SEEK_SET: i32 = 0;
    baash_sys::lseek(temp.as_raw_fd(), 0, SEEK_SET).ok()?;
    let mut output = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        match baash_sys::read_fd(temp.as_raw_fd(), &mut buffer) {
            Ok(0) => break,
            Ok(count) => output.extend_from_slice(&buffer[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    Some(output)
}

/// Run `PROMPT_COMMAND`, then hand the freshly rendered `PS1`/`PS2` to the
/// editor.
///
/// Returns whether the stock prompt was used. That can differ from
/// `stock_prompt` when `PROMPT_COMMAND` itself assigns `PS1`: the assignment
/// wins at this prompt, exactly as it would from a `PS1=` typed at the prompt.
///
/// Whether `PROMPT_ICONS` asks for the stock prompt's Nerd Font glyphs.
///
/// The folder and branch icons are private-use codepoints (U+F115 and U+F418):
/// a font that has them draws a picture, and a font without them draws a box.
/// A shell cannot see the font — no terminal protocol has a question for it —
/// so the choice is the user's, and it is off by default. `1`, `on`, `true` and
/// `yes` in any ASCII case turn it on; anything else, an unset variable
/// included, leaves it off. A shell variable rather than an environment
/// variable, so an rc file and the command line both work.
fn prompt_icons(shell: &mut Interp) -> bool {
    const ON: [&[u8]; 4] = [b"1", b"on", b"true", b"yes"];
    shell
        .state()
        .vars
        .scalar(b"PROMPT_ICONS")
        .is_some_and(|value| ON.iter().any(|word| value.eq_ignore_ascii_case(word)))
}

/// How long the last command took, as the stock prompt should paint it.
///
/// `PROMPT_DURATION` picks the policy: `0`/`off`/`false`/`no` never paints, a
/// truthy word always does, and unset — or anything else — paints only from
/// [`baash_lang::prompt::DURATION_FLOOR`]. The default is the middle one
/// because a prompt that
/// reports every 20 ms `grep` is noise, and one that reports nothing is a
/// feature nobody discovers. It is read at the next prompt, so an assignment
/// typed at the prompt takes effect immediately (`docs/contracts.md`).
fn prompt_duration(shell: &mut Interp, elapsed: Duration) -> Option<Duration> {
    let setting = shell
        .state()
        .vars
        .scalar(b"PROMPT_DURATION")
        .map(<[u8]>::to_vec);
    baash_lang::prompt::show_duration(setting.as_deref(), elapsed)
}

/// The shortest command worth a desktop notification, from `BAASH_NOTIFY`.
///
/// The value is a number of seconds; unset, empty, `0` or a false word turns
/// notifications off, which is the default. A notification is an interruption,
/// so it is opted into rather than sprung on a user who is already watching
/// the screen.
fn notify_minimum(shell: &mut Interp) -> Option<Duration> {
    const OFF: [&[u8]; 4] = [b"0", b"off", b"false", b"no"];
    let value = shell.state().vars.scalar(b"BAASH_NOTIFY")?;
    if value.is_empty() || OFF.iter().any(|word| value.eq_ignore_ascii_case(word)) {
        return None;
    }
    let seconds: u64 = std::str::from_utf8(value).ok()?.trim().parse().ok()?;
    (seconds > 0).then(|| Duration::from_secs(seconds))
}

/// Ask the terminal to notify the user, if the command outlived
/// `BAASH_NOTIFY`.
///
/// The shell cannot ask whether the terminal has focus while a child runs:
/// focus reporting is off for the duration of a command, so that a child
/// reading stdin is never handed `CSI I`/`CSI O`. The policy is therefore a
/// threshold and not a focus test — a terminal that raises its own
/// notification from `OSC 133 D` knows its focus and is doing the better
/// version (`docs/terminal.md` §13). The body names the command and never its
/// output, because an output line can be a secret.
fn notify_long_command(
    editor: &mut baash_repl::Editor<'_>,
    shell: &mut Interp,
    command: &[u8],
    elapsed: Duration,
    status: i32,
) {
    let Some(minimum) = notify_minimum(shell) else {
        return;
    };
    if elapsed < minimum {
        return;
    }
    let mut body = Vec::with_capacity(command.len() + 32);
    body.extend_from_slice(b"baash: ");
    body.extend_from_slice(&baash_lang::prompt::format_duration(elapsed));
    if status != 0 {
        body.extend_from_slice(format!(", exit {status}").as_bytes());
    }
    body.extend_from_slice(" \u{2014} ".as_bytes());
    body.extend_from_slice(notification_command(command));
    editor.notify(&body);
}

/// What a notification shows of a command: its first line, bounded.
///
/// A multi-line command — a pasted snippet, a `while` loop typed over several
/// lines — would otherwise arrive as one long line or as several, and a
/// notification is one line by design. The cut lands on a character boundary
/// so a UTF-8 command does not end in a replacement glyph.
fn notification_command(command: &[u8]) -> &[u8] {
    let end = command
        .iter()
        .position(|&byte| byte == b'\n' || byte == b'\r')
        .unwrap_or(command.len());
    let mut end = end.min(120);
    while end > 0 && end < command.len() && command[end] & 0xc0 == 0x80 {
        end -= 1;
    }
    &command[..end]
}

/// The ring moves to the shell first so `PROMPT_COMMAND` sees the same
/// history the user does, and because `\!` is the ring's next number, which is
/// what the line about to be typed will get.
fn refresh_prompt(
    shell: &mut Interp,
    editor: &mut baash_repl::Editor<'_>,
    engine: &baash_complete::CompletionEngine,
    prompt: &mut prompt_state::PromptState,
    stock_prompt: bool,
    light: bool,
    duration: Option<Duration>,
) -> bool {
    std::mem::swap(editor.history_mut(), &mut shell.state().history);
    run_prompt_command(shell);
    // Decided after `PROMPT_COMMAND`, because that is shell code and may have
    // assigned `PS1`.
    let stock = stock_prompt && shell.uses_default_prompt();
    // Read once, after `PROMPT_COMMAND`: that command may have `cd`ed, and
    // both the stock prompt and the terminal's cwd report have to name the
    // directory the next command will run in, even when the prompt is the
    // user's own `PS1`.
    let context = shell.repl_prompt_context();
    let secondary = prompt_template(shell, b"PS2").unwrap_or_else(|| b"> ".to_vec());
    let secondary = shell.render_prompt(&secondary);
    let primary = if stock {
        // Arm here rather than in the loop: `PROMPT_COMMAND` may have `cd`ed
        // or changed `HOME`, and the prompt has to describe the directory the
        // next command will run in. The composer itself is cache-only: one
        // `.git/HEAD` read and no subprocess, so a slow repository cannot hold
        // the line up (`docs/prompt-watch.md` §2).
        let color = std::env::var_os("NO_COLOR").is_none() && baash_sys::is_tty(0);
        prompt.arm(
            &context,
            baash_lang::prompt::Paint {
                color,
                light,
                icons: prompt_icons(shell),
            },
            duration,
        );
        prompt.primary()
    } else {
        // The legacy `[branch ~N]` suffix is fed by the same worker cache, so
        // it needs the directory even when the stock prompt is not in use.
        prompt.tick(&baash_sys::os::path_from_bytes(&context.cwd));
        let template = prompt_template(shell, b"PS1").unwrap_or_default();
        let primary = shell.render_prompt(&template);
        // The stock prompt carries the branch inline; the legacy bracketed
        // suffix is for a user-defined `PS1` only.
        match prompt.legacy_segment() {
            Some(segment) => {
                let mut bytes = primary.into_bytes();
                bytes.extend_from_slice(&segment);
                baash_repl::ExpandedPrompt::from_bytes(bytes)
            }
            None => primary,
        }
    };
    // The completion engine reads the same state the prompt does, so it is
    // refreshed here: `PROMPT_COMMAND` may have defined a function or a
    // `complete` rule, `cd` may have moved the working directory, and the
    // history ring is in the shell only for this stretch of the loop.
    complete::refresh(shell, engine);
    std::mem::swap(editor.history_mut(), &mut shell.state().history);
    editor.set_prompt(baash_repl::Prompt::from_expanded(primary, secondary));
    // Tell the terminal where the prompt is and what to call the window,
    // once per prompt. The terminal's copy of the directory is not the
    // shell's: a child (`ssh`, a container) can leave it stale while `PWD` is
    // right, so the report is re-sent rather than only after a `cd` — the same
    // choice fish made after issue #12191. A dumb terminal can interpret
    // neither, and a path with no URI form (a Windows UNC path) is not guessed
    // at.
    let preamble = if baash_sys::is_dumb_terminal() {
        Vec::new()
    } else {
        let ssh = baash_lang::prompt::ssh_context(&context.user, &context.host);
        let ssh = ssh
            .as_ref()
            .map(|(user, host)| (user.as_slice(), host.as_slice()));
        let mut bytes = baash_sys::osc::title(&baash_lang::prompt::window_title(
            &context.cwd,
            &context.home,
            ssh,
        ));
        if let Some(report) = baash_sys::osc::cwd_report(
            &context.cwd,
            &baash_sys::hostname(),
            baash_sys::paths::Flavour::host(),
        ) {
            bytes.extend_from_slice(&report);
        }
        bytes
    };
    editor.set_preamble(&preamble);
    stock
}

/// The interactive shell: prompt, line editor, completion, history.
///
/// `$?` consequences, exit requests and diagnostics are the VM's; this loop
/// only decides which bytes are one command. A failed command stays silent
/// because the command itself already reported its error, `exit N` leaves with
/// `N`, and `Ctrl-D` on an empty line leaves with 0.
/// Drive the interactive loop from the I/O runtime.
///
/// One thread owns the shell (`docs/direction.md` §2): the runtime exists to
/// give that thread completion-based wakeups — stdin, child processes, file
/// scans — instead of a hand-rolled `poll`. The loop itself is still
/// synchronous, so this wrapper changes nothing yet; it is the seam the rest of
/// W3 moves onto.
fn run_repl_on_runtime(shell: &mut Interp) -> ExitCode {
    let runtime = match compio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("baash: cannot start the I/O runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    // `enter` rather than `block_on`: the loop itself is synchronous and calls
    // `block_on` for individual operations (a keystroke read, a prompt's `git
    // status`). Nesting a `block_on` inside another would run the executor
    // re-entrantly for no gain.
    runtime.enter(|| run_repl(shell))
}

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
                .map(baash_sys::os::bytes_from_os)
        });
    let root = cwd
        .as_deref()
        .map(baash_sys::os::path_from_bytes)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let engine = baash_complete::CompletionEngine::new().with_root(root);
    // readline's `completion-ignore-case`, the one inputrc setting the shell
    // honours: bash's `compgen` honours it inside a completion, and a
    // filename completion that differs only in case otherwise finds nothing.
    let ignore_case = inputrc::completion_ignore_case();
    engine.set_ignore_case(ignore_case);
    shell.state().completions.set_ignore_case(ignore_case);
    // There is one ring. The shell state owns it between commands (so the
    // built-ins see it); the editor holds it while a line is being read, and
    // the loop swaps it across at exactly those two points.
    let history = std::mem::take(&mut shell.state().history);
    let config = baash_repl::Config {
        // The editor paints on the controlling terminal, so colour is decided
        // by the terminal rather than by stdout (which may be redirected).
        color: baash_repl::color_supported(0),
        // Replaced by `refresh_prompt` before the first line is read.
        prompt: baash_repl::Prompt::default(),
        history,
        ..baash_repl::Config::default()
    };
    let mut terminal = baash_repl::RealTerminal::new();
    // The terminal knows whether its background is light; the answer picks the
    // syntax theme and, later, the stock prompt's accent colours. A terminal
    // that does not answer (the Windows console, `TERM=dumb`, a terminal that
    // ignores OSC 11) keeps the dark palette.
    let light = terminal.background_is_light().unwrap_or(false);
    let config = baash_repl::Config {
        theme: if light {
            baash_repl::Theme::LIGHT
        } else {
            baash_repl::Theme::DARK
        },
        ..config
    };
    let mut editor = baash_repl::Editor::with_config(&mut terminal, &engine, &syntax, config);
    // The first command completion would otherwise walk every `PATH` directory
    // on the thread that reads keystrokes. Build the index on a worker and
    // install it when it lands (checked once per prompt), so TAB is a lookup
    // and typing stays responsive while the scan runs.
    let commands = engine.commands();
    let (index_tx, index_rx) = std::sync::mpsc::channel();
    {
        let commands = std::sync::Arc::clone(&commands);
        std::thread::Builder::new()
            .name("baash-completion-index".to_owned())
            .spawn(move || {
                let _ = index_tx.send(commands.build_index());
            })
            .ok();
    }
    // One session per REPL: `:&` and `!%` remember facts across lines, and the
    // quote/here-document state must survive `PS2` continuation lines.
    let mut expansion = histexpand::Session::new();
    // The stock prompt's state: the abbreviated directory, the cached git
    // facts and the working-tree watcher. It holds no shell state, which is
    // what lets the editor borrow it while a line is being read
    // (`docs/prompt-watch.md` §3).
    let mut prompt = prompt_state::PromptState::new();
    // The last command's wall time, kept across the loop because the prompt
    // that shows it is painted at the top of the *next* iteration.
    let mut last_duration: Option<Duration> = None;

    let status = loop {
        // `PROMPT_COMMAND` runs and the prompt is rendered here, not at
        // startup: the command just executed may have changed `PS1`, the
        // working directory or `promptvars`, and `\!` must name the line
        // about to be typed.
        // A finished background scan is installed before the prompt, never
        // while a key is being read.
        if let Ok(index) = index_rx.try_recv() {
            commands.install_index(index);
        }
        // The stock prompt carries the branch and counters inline. A
        // user-defined `PS1` gets the legacy bracketed `[branch]` suffix
        // instead; both are fed by the same worker.
        let stock_prompt = shell.uses_default_prompt();
        let stock = refresh_prompt(
            shell,
            &mut editor,
            &engine,
            &mut prompt,
            stock_prompt,
            light,
            last_duration,
        );
        if stock {
            // Only the built-in prompt is live. A user's `PS1` is the result of
            // `PROMPT_COMMAND` and every prompt escape, so re-rendering it from
            // a watcher would run shell code while the user is typing.
            prompt.adopt(&mut editor);
        } else {
            prompt.disarm(&mut editor);
        }
        // `PROMPT_COMMAND='exit'` leaves the shell before the prompt is drawn.
        if let Some(requested) = shell.state().take_exit_request() {
            break requested;
        }
        match read_command(shell, &mut editor, &mut expansion, &mut prompt, &engine) {
            Ok(NextCommand::Run(bytes)) => {
                // Hand the ring to the shell for the duration of the command,
                // so `history`/`fc` mutate the very ring the editor navigates.
                std::mem::swap(editor.history_mut(), &mut shell.state().history);
                let before = shell.state().vars.scalar(b"PWD").map(<[u8]>::to_vec);
                // `\#` counts the lines the interactive loop reads, so the
                // counter advances here rather than in the evaluator (which
                // sees a whole chunk and cannot tell a REPL line from a
                // script).
                shell.state().begin_command();
                // The command pair brackets execution for the terminal: `C`
                // carries the text, `D` the status. They are the loop's job
                // because the editor never sees a command run.
                // The buffer ends with the newline that submitted it; that
                // terminator is not part of the command the terminal should
                // show or replay.
                let command = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
                editor.command_started(command);
                let started = std::time::Instant::now();
                let status = shell.run_named("<stdin>", &bytes);
                let elapsed = started.elapsed();
                last_duration = prompt_duration(shell, elapsed);
                editor.command_finished(status.0);
                notify_long_command(&mut editor, shell, command, elapsed, status.0);
                // `cd` changes the shell's `PWD`; the completion engine and
                // `PS1`'s `\w` must follow it (`docs/contracts.md`).
                let after = shell.state().vars.scalar(b"PWD").map(<[u8]>::to_vec);
                // Record `$?` and the wall time against the line's row, then
                // give the ring back to the editor.
                shell
                    .state()
                    .history
                    .finish(Some(status.0), Some(duration_ms(elapsed)));
                std::mem::swap(editor.history_mut(), &mut shell.state().history);
                if after != before
                    && let Some(pwd) = &after
                {
                    engine.set_root(baash_sys::os::os_from_bytes(pwd));
                }
                if let Some(requested) = shell.state().take_exit_request() {
                    break requested;
                }
            }
            // A blank line, a failed expansion, or a `:p` consumed the read.
            Ok(NextCommand::Skip) => continue,
            Ok(NextCommand::Interrupted) => {
                // `Ctrl-C` abandons the line and leaves `$?` at 130, the way a
                // shell reports an interrupted foreground command.
                shell
                    .state()
                    .vars
                    .set_last_status(baash_runtime::Status(130));
            }
            Ok(NextCommand::Eof) => break baash_runtime::Status::SUCCESS,
            Err(error) => {
                eprintln!("baash: {error}");
                editor.finish();
                return ExitCode::from(1);
            }
        }
    };
    editor.finish();
    ExitCode::from(status.exit_code() as u8)
}

/// What the interactive reader should do after one `read_line`.
enum NextCommand {
    /// Run this text.
    Run(Vec<u8>),
    /// The line was consumed: blank, a failed expansion, or `:p`.
    Skip,
    /// `Ctrl-C`.
    Interrupted,
    /// End of input.
    Eof,
}

/// The events a reader may expand: every recorded line except the one the
/// editor has just recorded and marked current. `!!` must mean the previous
/// *complete* command, never the line being typed.
fn history_entries(history: &baash_runtime::History) -> Vec<histexpand::Entry<'_>> {
    let count = history.entries().len();
    let current = usize::from(history.is_current() && count > 0);
    history
        .entries()
        .iter()
        .take(count - current)
        .map(|entry| histexpand::Entry::new(entry.number(), entry.text()))
        .collect()
}

/// A wall time as whole milliseconds, saturating rather than wrapping.
fn duration_ms(elapsed: std::time::Duration) -> i64 {
    i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
}

fn write_stderr_bytes(bytes: &[u8]) {
    let mut stderr = std::io::stderr().lock();
    let _ = stderr.write_all(bytes);
}

/// Read one line, expand it, and say what to do with it.
///
/// History expansion rewrites the typed line before the parser sees it. The
/// editor has already recorded the typed line, so when an expansion changes it
/// the typed entry is taken back out and the expanded line is recorded in its
/// place — that is what bash's `history` shows. `histverify` turns the next
/// read into a review of the expanded line instead of a run, without running
/// `PROMPT_COMMAND` again (measured).
fn read_command(
    shell: &mut Interp,
    editor: &mut baash_repl::Editor<'_>,
    session: &mut histexpand::Session,
    live: &mut dyn baash_repl::LivePrompt,
    engine: &baash_complete::CompletionEngine,
) -> std::io::Result<NextCommand> {
    loop {
        // The `complete -F` seam: a rule whose source is a shell function runs
        // it here, with the interpreter, while the editor owns the terminal.
        let bytes = {
            let mut completer = complete::ShellCompleter::new(shell, engine);
            let result = editor.read_line_with_shell(live, Some(&mut completer))?;
            match result {
                baash_repl::ReadResult::Line(bytes) => bytes,
                baash_repl::ReadResult::Interrupted => return Ok(NextCommand::Interrupted),
                baash_repl::ReadResult::Eof => return Ok(NextCommand::Eof),
            }
        };
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(NextCommand::Skip);
        }
        // bash expands only when both options are on: `set +H` and
        // `set +o history` each disable it, and a non-interactive shell never
        // reaches this function.
        let expanding =
            shell.is_interactive() && shell.state().opts.histexpand && shell.state().opts.history;
        if !expanding {
            if !shell.state().opts.history {
                editor.history_mut().remove_current();
            }
            return Ok(NextCommand::Run(bytes));
        }
        // The editor hands back the line with the newline that submitted it
        // (and, for a continuation, the earlier lines too). Expansion is
        // defined on the typed text, and `histverify` needs a buffer without
        // the newline, so work on the body and restore the newline only for
        // execution.
        let (body, had_newline) = match bytes.strip_suffix(b"\n") {
            Some(body) => (body, true),
            None => (bytes.as_slice(), false),
        };
        let events = history_entries(editor.history());
        let expansion = match session.expand(body, &events) {
            Ok(expansion) => expansion,
            Err(error) => {
                // A `!` that begins no valid event abandons the line: bash
                // reports it, does not record it, and leaves `$?` alone.
                eprintln!("baash: {error}");
                session.abandon_command();
                editor.history_mut().remove_current();
                return Ok(NextCommand::Skip);
            }
        };
        if !expansion.changed() {
            return Ok(NextCommand::Run(bytes));
        }
        let expanded = expansion.text().to_vec();
        // bash writes the expanded command to stderr, where the prompt lives;
        // stdout belongs to the command. `histverify` is the exception: the
        // line goes back to the editing buffer instead of being echoed.
        if !shell.state().opts.histverify && !expanded.is_empty() {
            write_stderr_bytes(&expanded);
            write_stderr_bytes(b"\n");
        }
        if expansion.print_only() || shell.state().opts.histverify {
            editor.history_mut().remove_current();
        } else {
            editor.history_mut().remove_current();
            editor.history_mut().push(&expanded);
        }
        if shell.state().opts.histverify && !expansion.print_only() {
            editor.set_initial_line(&expanded);
            continue;
        }
        if expansion.print_only() {
            // `:p` prints and still records the expanded line (measured).
            editor.history_mut().push(&expanded);
            return Ok(NextCommand::Skip);
        }
        let mut command = expanded;
        if had_newline {
            command.push(b'\n');
        }
        return Ok(NextCommand::Run(command));
    }
}

/// Non-interactive stdin is one chunk; a terminal gets a line-at-a-time loop
/// so a half-typed compound command is not executed early.
///
/// The terminal path above ([`run_repl`]) uses `baash-repl`'s editor; this is the
/// fallback for a pipe or a redirected file, which must keep the script
/// semantics of executing the whole input as one chunk.
fn run_stdin(shell: &mut Interp) -> ExitCode {
    // A non-interactive pipe is one chunk (script semantics). An interactive
    // shell reading a pipe is line-at-a-time, because bash history-expands
    // each line as it is read (`baash -i < file`, the shape the conformance
    // suite's interactive cases use); a whole-chunk pass would expand every
    // line before the previous ones were recorded.
    if !baash_sys::is_tty(0) && !shell.is_interactive() {
        let mut bytes = Vec::new();
        if let Err(error) = std::io::stdin().read_to_end(&mut bytes) {
            eprintln!("baash: cannot read stdin: {error}");
            return ExitCode::from(1);
        }
        let bytes = baash_syntax::decode(&bytes);
        let status = shell.run_named("<stdin>", &bytes);
        return ExitCode::from(status.exit_code() as u8);
    }

    let mut pending: Vec<u8> = Vec::new();
    let mut status = baash_runtime::Status::SUCCESS;
    let mut expansion = histexpand::Session::new();
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    loop {
        // An interactive shell runs `PROMPT_COMMAND` once per prompt, and a
        // piped stdin gets the same treatment: the prompt itself is not
        // painted (there is no terminal), but its command still runs. The
        // first iteration is a no-op unless the environment set it.
        if shell.is_interactive() {
            run_prompt_command(shell);
            if shell.state().take_exit_request().is_some() {
                break;
            }
        }
        let mut line = Vec::new();
        let read = stdin.read_until(b'\n', &mut line);
        match read {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => {
                eprintln!("baash: cannot read stdin: {error}");
                return ExitCode::from(1);
            }
        }
        // The line keeps the `\r` of a Windows file; decoding settles it here,
        // where a whole script is settled too, so a CRLF script reads the same
        // whether it is piped, redirected or named.
        let line = baash_syntax::decode(&line).into_owned();
        // bash expands an interactive shell that reads a pipe too (`baash -i <
        // file`); only a `-c` string is exempt, and that never comes here.
        let expanding =
            shell.is_interactive() && shell.state().opts.histexpand && shell.state().opts.history;
        if expanding {
            let (body, newline) = match line.strip_suffix(b"\n") {
                Some(body) => (body, true),
                None => (line.as_slice(), false),
            };
            let events = history_entries(&shell.state().history);
            match expansion.expand_line(body, &events) {
                Ok(expanded) => {
                    if expanded.print_only() {
                        // `:p` prints and records, but is not executed.
                        write_stderr_bytes(expanded.text());
                        write_stderr_bytes(b"\n");
                        shell.state().history.push(expanded.text());
                        continue;
                    }
                    pending.extend_from_slice(expanded.text());
                    if newline {
                        pending.push(b'\n');
                    }
                }
                Err(error) => {
                    eprintln!("baash: {error}");
                    expansion.abandon_command();
                    pending.clear();
                    continue;
                }
            }
        } else {
            pending.extend_from_slice(&line);
        }
        match baash_syntax::parse_bytes(&pending, &ParseOpts::DEFAULT) {
            ParseOutcome::NeedMoreInput { .. } => continue,
            ParseOutcome::Failed(diagnostics) => {
                let file = SourceFile::new("<stdin>", pending.as_slice());
                for diagnostic in &diagnostics {
                    write_stderr(&baash_diag::render_compact(diagnostic, &file));
                }
                pending.clear();
                status = baash_runtime::Status(2);
                continue;
            }
            ParseOutcome::Ok(_) => {}
        }
        let chunk = std::mem::take(&mut pending);
        // An interactive shell records what it reads even when stdin is not a
        // terminal (`baash -i < file`, which is how the conformance suite drives
        // the interactive cases). Non-interactive runs record nothing.
        if shell.is_interactive() && shell.state().opts.history {
            shell.state().history.push(&chunk);
        }
        // The status of the last chunk is the shell's exit status; an `exit`
        // inside it already requested the unwind, and the request survives in
        // the shell state, so the loop stops here too.
        let started = std::time::Instant::now();
        status = shell.run_named("<stdin>", &chunk);
        let elapsed = started.elapsed();
        if shell.is_interactive() {
            shell
                .state()
                .history
                .finish(Some(status.0), Some(duration_ms(elapsed)));
        }
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
        eprintln!("baash: cannot read stdin: {err}");
        return ExitCode::from(1);
    }

    let color = baash_repl::color_supported(1);
    let painted = baash_repl::paint(&input, &Theme::DARK, color);

    let mut stdout = std::io::stdout().lock();
    if let Err(err) = stdout.write_all(&painted).and_then(|()| stdout.flush()) {
        // EPIPE just means the reader went away; a shell must not treat that as
        // its own failure.
        if err.kind() != std::io::ErrorKind::BrokenPipe {
            eprintln!("baash: cannot write stdout: {err}");
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
