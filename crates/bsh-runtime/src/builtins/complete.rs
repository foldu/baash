//! Programmable completion: `compgen`, `complete` and `compopt`.
//!
//! Three pieces meet here. What a candidate *is* (which action means which
//! list, how a prefix filters it) lives in [`crate::complete`]; the shell state
//! that produces candidates arrives through [`ShellHost`]'s completion seams;
//! and this module is the bash option grammar plus the order the generators run
//! in — the part the conformance suite pins, and the part that cannot be
//! decided anywhere else because it is about the command line.
//!
//! # The generator order
//!
//! bash's order, measured against 5.3.20 rather than guessed:
//!
//! ```text
//! -A actions (in flag order) → -G glob → -W words → -F function/-C command
//! → implicit file generators (-o dirnames/default/plusdirs)
//! → -X filter → -P prefix → -S suffix
//! ```
//!
//! Actions come first in *flag* order, which is why `compgen -A function
//! -A variable -A file PA` prints a function, then a variable, then a file.
//! `-W` words are filtered by the word being completed; `-F` and `-C`
//! candidates are not (bash prints whatever `COMPREPLY` holds, and the suite
//! relies on that). Everything is subject to `-X`.
//!
//! # Statuses
//!
//! * `2` — a usage error: unknown option, unknown `-A` action, unknown `-o`
//!   name, a `-W` list that does not parse.
//! * `1` — no candidates were generated, or a `-W`/`-F` expansion failed.
//! * `0` — at least one candidate was printed. A `compgen` with no generator
//!   at all is also `0` and prints nothing, which is what bash does.

use crate::complete::{
    CompletionAction, CompletionSource, CompletionSpec, CompletionTable, Request, WordList,
    candidates, filter_prefix, has_option, is_completion_option, parse_complete,
};
use crate::{Opts, ShellHost, Status, VarValue};

use super::{codes, diagnose, display, usage};

/// The pattern dialect `-X` and `-G` match with: the shell's `shopt` switches,
/// as everywhere else in the shell.
fn pattern_flags(opts: &Opts) -> bsh_pattern::Flags {
    bsh_pattern::Flags {
        extglob: opts.extglob,
        nocasematch: opts.nocasematch,
        pathname: false,
        utf8: false,
    }
}

/// Run one candidate query against the host's source, or answer nothing when
/// the host cannot enumerate candidates at all.
fn with_source(
    host: &mut dyn ShellHost,
    query: impl FnOnce(&mut dyn CompletionSource) -> Vec<Vec<u8>>,
) -> Vec<Vec<u8>> {
    match host.completion_source() {
        Some(source) => query(source),
        None => Vec::new(),
    }
}

/// `-X`: remove the candidates matching `pattern`.
///
/// A leading `!` inverts the test, which is how `compgen -X '!*.py'` keeps only
/// Python files. The pattern is matched against the candidate *before* `-P`
/// and `-S` decorate it.
fn apply_filter(candidates: &mut Vec<Vec<u8>>, pattern: &[u8], opts: &Opts) {
    let (negated, pattern) = match pattern.strip_prefix(b"!") {
        Some(rest) => (true, rest),
        None => (false, pattern),
    };
    let flags = pattern_flags(opts);
    candidates.retain(|candidate| bsh_pattern::matches(pattern, candidate, flags) == negated);
}

/// `-G glob`: the files matching `pattern`.
///
/// The directory part of the pattern drives the listing and the whole pattern
/// filters it, which is what makes `subdir/*` and `o*` both work. A directory
/// part that is itself a pattern is not expanded — bash would glob it — and
/// that is the documented limit of doing this with the file source rather than
/// the pathname expander.
fn glob_candidates(host: &mut dyn ShellHost, pattern: &[u8]) -> Vec<Vec<u8>> {
    let directory = match pattern.iter().rposition(|&byte| byte == b'/') {
        Some(at) => pattern[..=at].to_vec(),
        None => Vec::new(),
    };
    let mut entries = with_source(host, |source| source.files(&directory, false));
    let flags = pattern_flags(&host.state().opts);
    entries.retain(|entry| bsh_pattern::matches(pattern, entry, flags));
    entries
}

/// Split captured command output into candidate lines.
///
/// Trailing newlines are dropped like a command substitution's, and empty
/// lines are not candidates.
fn output_lines(bytes: &[u8]) -> Vec<Vec<u8>> {
    bytes
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

/// Prepare the variables a completion function reads.
///
/// Measured against bash 5.3.20 with `compgen -F`: the line is empty, the
/// cursor is at 0, the word index is -1 and `COMP_WORDS` is empty. The suite
/// prints all four from inside the function.
fn prepare_completion_vars(host: &mut dyn ShellHost) {
    let state = host.state();
    let _ = state.vars.assign_scalar(b"COMP_LINE", b"");
    let _ = state.vars.assign_scalar(b"COMP_POINT", b"0");
    let _ = state.vars.assign_scalar(b"COMP_CWORD", b"-1");
    let _ = state.vars.unset(b"COMP_WORDS");
    let _ = state.vars.unset(b"COMPREPLY");
}

/// The candidates a completion function left in `COMPREPLY`.
///
/// bash reads it as an array and also accepts the scalar form
/// (`COMPREPLY=hello`), which the suite tests on its own.
fn completion_replies(host: &mut dyn ShellHost) -> Vec<Vec<u8>> {
    let vars = &host.state().vars;
    match vars.value(b"COMPREPLY") {
        Some(VarValue::Indexed(elements)) => elements.values().cloned().collect(),
        Some(VarValue::Assoc(pairs)) => pairs.iter().map(|(_, value)| value.clone()).collect(),
        Some(VarValue::Str(value)) => vec![value.clone()],
        Some(VarValue::Unset) | None => Vec::new(),
    }
}

/// Call a `-F` completion function and read `COMPREPLY` back.
///
/// bash calls it with `$1` `compgen`, `$2` the word being completed and `$3`
/// the previous word — for `compgen` there is no previous word, so it is
/// empty. The function runs in *this* shell, not a subshell, because
/// `COMPREPLY` is how it answers.
fn completion_function(
    host: &mut dyn ShellHost,
    function: &[u8],
    word: &[u8],
) -> Result<Vec<Vec<u8>>, Status> {
    let spec_options = host.state().completion.options().to_vec();
    prepare_completion_vars(host);
    host.state().completion.begin(&spec_options);
    // `argv[0]` is the callee's own name (the callee's `$@` is `argv[1..]`),
    // so the three arguments bash passes are the command name, the word being
    // completed and the previous word.
    let argv = vec![
        function.to_vec(),
        b"compgen".to_vec(),
        word.to_vec(),
        Vec::new(),
    ];
    let status = host.call_function(function, &argv);
    host.state().completion.end();
    let Some(status) = status else {
        diagnose(
            host,
            codes::NOT_FOUND,
            format!("compgen: {}: no such function", display(function)),
        );
        return Err(Status::FAILURE);
    };
    if !status.is_success() {
        // The suite pins this: a completion function that fails part-way
        // prints nothing, not even the replies it had already assigned, and
        // `compgen` reports the failure as 1 rather than passing the
        // function's own status through (measured against bash).
        return Err(Status::FAILURE);
    }
    Ok(completion_replies(host))
}

/// One `compgen` invocation's operands.
#[derive(Debug, Default)]
struct Compgen {
    actions: Vec<CompletionAction>,
    wordlist: Option<Vec<u8>>,
    glob: Option<Vec<u8>>,
    prefix: Option<Vec<u8>>,
    suffix: Option<Vec<u8>>,
    filter: Option<Vec<u8>>,
    function: Option<Vec<u8>>,
    command: Option<Vec<u8>>,
    options: Vec<Vec<u8>>,
    word: Vec<u8>,
}

/// The action a single-letter `compgen` flag selects.
///
/// `-b` is *builtin* here: `compgen` has no `-b` for bindings (that action is
/// only reachable as `-A binding`), unlike the printer in
/// [`crate::complete`].
fn short_action(letter: u8) -> Option<CompletionAction> {
    Some(match letter {
        b'a' => CompletionAction::Alias,
        b'b' => CompletionAction::Builtin,
        b'c' => CompletionAction::Command,
        b'd' => CompletionAction::Directory,
        b'e' => CompletionAction::Export,
        b'f' => CompletionAction::File,
        b'g' => CompletionAction::Group,
        b'j' => CompletionAction::Job,
        b'k' => CompletionAction::Keyword,
        b's' => CompletionAction::Service,
        b'u' => CompletionAction::User,
        b'v' => CompletionAction::Variable,
        _ => return None,
    })
}

/// The option letter an argument is, if it takes a value.
fn value_option(argument: &[u8]) -> Option<u8> {
    Some(match argument {
        b"-A" => b'A',
        b"-W" => b'W',
        b"-G" => b'G',
        b"-P" => b'P',
        b"-S" => b'S',
        b"-X" => b'X',
        b"-F" => b'F',
        b"-C" => b'C',
        b"-o" => b'o',
        _ => return None,
    })
}

/// Parse `compgen`'s arguments, without the command name.
fn parse_compgen(host: &mut dyn ShellHost, argv: &[&[u8]]) -> Result<Compgen, Status> {
    let mut parsed = Compgen::default();
    let mut operands: Vec<Vec<u8>> = Vec::new();
    let mut index = 1;
    while index < argv.len() {
        let argument = argv[index];
        if let Some(letter) = value_option(argument) {
            let Some(value) = argv.get(index + 1) else {
                return Err(usage(
                    host,
                    argv,
                    &format!("-{}: option requires an argument", char::from(letter)),
                ));
            };
            let value = value.to_vec();
            index += 2;
            apply_value_option(host, &mut parsed, letter, value)?;
            continue;
        }
        if argument == b"--" {
            operands.extend(argv[index + 1..].iter().map(|word| word.to_vec()));
            break;
        }
        if argument.len() > 1 && argument[0] == b'-' {
            for &letter in &argument[1..] {
                match short_action(letter) {
                    Some(action) => parsed.actions.push(action),
                    None => {
                        return Err(usage(
                            host,
                            argv,
                            &format!("-{}: invalid option", char::from(letter)),
                        ));
                    }
                }
            }
            index += 1;
            continue;
        }
        operands.push(argument.to_vec());
        index += 1;
    }
    parsed.word = operands.first().cloned().unwrap_or_default();
    Ok(parsed)
}

/// Apply one value-taking option. `Err` carries the status to return.
fn apply_value_option(
    host: &mut dyn ShellHost,
    parsed: &mut Compgen,
    letter: u8,
    value: Vec<u8>,
) -> Result<(), Status> {
    match letter {
        b'A' => match CompletionAction::from_name(&value) {
            Some(action) => parsed.actions.push(action),
            None => {
                diagnose(
                    host,
                    codes::USAGE,
                    format!("compgen: {}: invalid action name", display(&value)),
                );
                return Err(Status(2));
            }
        },
        b'W' => parsed.wordlist = Some(value),
        b'G' => parsed.glob = Some(value),
        b'P' => parsed.prefix = Some(value),
        b'S' => parsed.suffix = Some(value),
        b'X' => parsed.filter = Some(value),
        b'F' => parsed.function = Some(value),
        b'C' => parsed.command = Some(value),
        b'o' => {
            if !is_completion_option(&value) {
                diagnose(
                    host,
                    codes::USAGE,
                    format!("compgen: {}: invalid option name", display(&value)),
                );
                return Err(Status(2));
            }
            parsed.options.push(value);
        }
        _ => unreachable!("all value options are handled"),
    }
    Ok(())
}

/// `compgen [-A action] [-G glob] [-W wordlist] [-F function] [-C command] …`.
pub(super) fn bi_compgen(host: &mut dyn ShellHost, argv: &[&[u8]]) -> Status {
    let parsed = match parse_compgen(host, argv) {
        Ok(parsed) => parsed,
        Err(status) => return status,
    };
    let word = parsed.word.clone();
    let dirnames = has_option(&parsed.options, b"dirnames");
    let default = has_option(&parsed.options, b"default");
    let plusdirs = has_option(&parsed.options, b"plusdirs");

    let mut generated: Vec<Vec<u8>> = Vec::new();
    let mut file_action = false;
    for action in &parsed.actions {
        let batch = match action {
            CompletionAction::File => {
                file_action = true;
                with_source(host, |source| source.files(&word, dirnames))
            }
            CompletionAction::Directory => {
                file_action = true;
                with_source(host, |source| source.files(&word, true))
            }
            other => with_source(host, |source| candidates(*other, &word, source)),
        };
        generated.extend(batch);
    }

    if let Some(pattern) = &parsed.glob {
        generated.extend(glob_candidates(host, pattern));
    }

    if let Some(wordlist) = &parsed.wordlist {
        match host.completion_words(wordlist) {
            WordList::Words(mut words) => {
                filter_prefix(&mut words, &word);
                generated.extend(words);
            }
            WordList::SyntaxError => {
                diagnose(host, codes::USAGE, "compgen: -W: invalid word list");
                return Status(2);
            }
            WordList::RuntimeError => return Status::FAILURE,
        }
    }

    if let Some(function) = &parsed.function {
        match completion_function(host, function, &word) {
            Ok(replies) => generated.extend(replies),
            Err(status) => return status,
        }
    }

    if let Some(command) = &parsed.command {
        let argv = vec![command.clone(), word.clone()];
        match host.capture_command(&argv) {
            Some((bytes, _status)) => generated.extend(output_lines(&bytes)),
            None => {
                diagnose(
                    host,
                    codes::NOT_FOUND,
                    format!(
                        "compgen: {}: cannot capture command output",
                        display(command)
                    ),
                );
                return Status::FAILURE;
            }
        }
    }

    // The implicit file generators. `-o default` and `-o plusdirs` add files
    // to whatever was generated; `-o dirnames` restricts the file source to
    // directories, which `-A file` above already honoured.
    if dirnames && !file_action {
        generated.extend(with_source(host, |source| source.files(&word, true)));
    }
    if default && !file_action {
        generated.extend(with_source(host, |source| source.files(&word, false)));
    }
    if plusdirs {
        generated.extend(with_source(host, |source| source.files(&word, true)));
    }

    if let Some(filter) = &parsed.filter {
        apply_filter(&mut generated, filter, &host.state().opts);
    }
    if let Some(prefix) = &parsed.prefix {
        for candidate in &mut generated {
            let mut decorated = prefix.clone();
            decorated.append(candidate);
            *candidate = decorated;
        }
    }
    if let Some(suffix) = &parsed.suffix {
        for candidate in &mut generated {
            candidate.extend_from_slice(suffix);
        }
    }

    let mut out = Vec::new();
    for candidate in &generated {
        out.extend_from_slice(candidate);
        out.push(b'\n');
    }
    let write_status = host.write_out(&out);
    if !write_status.is_success() {
        return write_status;
    }

    let have_generator = !parsed.actions.is_empty()
        || parsed.glob.is_some()
        || parsed.wordlist.is_some()
        || parsed.function.is_some()
        || parsed.command.is_some()
        || dirnames
        || default
        || plusdirs;
    if !have_generator {
        // `compgen` on its own is not a usage error in bash; it simply has
        // nothing to offer.
        return Status::SUCCESS;
    }
    if generated.is_empty() {
        Status::FAILURE
    } else {
        Status::SUCCESS
    }
}

/// Write the lines `complete`/`complete -p` produce.
fn write_lines(host: &mut dyn ShellHost, lines: &[Vec<u8>]) -> Status {
    let mut out = Vec::new();
    for line in lines {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    host.write_out(&out)
}

/// Check the parts of a spec that can be wrong when it is *defined*.
///
/// bash warns about a missing `-F` function and keeps going, and it does not
/// look at a `-W` word list until a completion runs. The suite pins status 2
/// for both at definition time, which is the more useful answer: a typo in
/// `.bashrc` should be reported when it is typed.
fn validate_spec(
    host: &mut dyn ShellHost,
    spec: &CompletionSpec,
    unnamed: bool,
) -> Result<(), Status> {
    for option in &spec.options {
        if !is_completion_option(option) {
            diagnose(
                host,
                codes::USAGE,
                format!("complete: {}: invalid option name", display(option)),
            );
            return Err(Status(2));
        }
    }
    // A `-D`/`-E`/`-I` spec names no command, so the function is the only
    // thing that can be wrong and a missing one is reported now. A named spec
    // is allowed to mention a function that a later file defines, which is
    // what the suite's `complete -F myfunc other` requires.
    if let Some(function) = &spec.function
        && unnamed
        && !host.is_function(function)
    {
        diagnose(
            host,
            codes::NOT_FOUND,
            format!("complete: {}: no such function", display(function)),
        );
        return Err(Status(2));
    }
    if let Some(wordlist) = &spec.wordlist
        && !host.check_completion_words(wordlist)
    {
        diagnose(host, codes::USAGE, "complete: -W: invalid word list");
        return Err(Status(2));
    }
    Ok(())
}

/// `complete [-p|-r] [-D|-E|-I] [-A action] [-W wordlist] … [name …]`.
pub(super) fn bi_complete(host: &mut dyn ShellHost, argv: &[&[u8]]) -> Status {
    let arguments: Vec<&[u8]> = argv[1..].to_vec();
    let request = match parse_complete(&arguments) {
        Ok(request) => request,
        Err(message) => {
            diagnose(host, codes::USAGE, message);
            return Status(2);
        }
    };
    if let Request::Set {
        spec,
        default,
        empty,
        initial,
        ..
    } = &request
        && let Err(status) = validate_spec(host, spec, *default || *empty || *initial)
    {
        return status;
    }
    let table: &mut CompletionTable = &mut host.state().completions;
    match table.apply(&request) {
        Ok(lines) => write_lines(host, &lines),
        Err(message) => {
            diagnose(host, codes::USAGE, message);
            Status(2)
        }
    }
}

/// `compopt [-o option] [+o option] …`.
///
/// It edits only the completion *in progress*: outside one there is nothing to
/// change and bash returns 1, which the suite pins. The options it sets are
/// recorded in [`crate::complete::CompletionRun`] for the engine that is
/// generating the completion.
pub(super) fn bi_compopt(host: &mut dyn ShellHost, argv: &[&[u8]]) -> Status {
    let mut changes: Vec<(Vec<u8>, bool)> = Vec::new();
    let mut index = 1;
    while index < argv.len() {
        let argument = argv[index];
        if argument == b"--" {
            break;
        }
        let sign = argument.first().copied().unwrap_or(b'-');
        if argument.len() > 1 && matches!(sign, b'-' | b'+') {
            if &argument[1..] != b"o" {
                return usage(host, argv, "usage: compopt [-o option] [+o option]");
            }
            let Some(value) = argv.get(index + 1) else {
                return usage(host, argv, "-o: option requires an argument");
            };
            if !is_completion_option(value) {
                diagnose(
                    host,
                    codes::USAGE,
                    format!("compopt: {}: invalid option name", display(value)),
                );
                return Status(2);
            }
            changes.push((value.to_vec(), sign == b'-'));
            index += 2;
            continue;
        }
        return usage(host, argv, "usage: compopt [-o option] [+o option]");
    }

    if !host.state().completion.is_active() {
        diagnose(
            host,
            codes::USAGE,
            "compopt: not currently executing completion function",
        );
        return Status::FAILURE;
    }
    for (name, on) in changes {
        host.state().completion.set_option(&name, on);
    }
    Status::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_short_action_letter_is_a_bash_action() {
        for letter in b"abcdefgjksuv" {
            assert!(
                short_action(*letter).is_some(),
                "-{} should name an action",
                char::from(*letter)
            );
        }
        assert_eq!(short_action(b'z'), None);
        assert_eq!(short_action(b'b'), Some(CompletionAction::Builtin));
    }

    #[test]
    fn value_options_are_exactly_the_ones_that_take_values() {
        for argument in [
            &b"-A"[..],
            b"-W",
            b"-G",
            b"-P",
            b"-S",
            b"-X",
            b"-F",
            b"-C",
            b"-o",
        ] {
            assert!(value_option(argument).is_some(), "{argument:?}");
        }
        assert_eq!(value_option(b"-p"), None);
        assert_eq!(value_option(b"-a"), None);
    }

    #[test]
    fn option_names_are_the_bash_set() {
        assert_eq!(crate::complete::COMPLETION_OPTIONS.len(), 9);
        assert!(is_completion_option(b"nospace"));
        assert!(is_completion_option(b"dirnames"));
        assert!(!is_completion_option(b"invalid"));
    }

    #[test]
    fn output_lines_drops_the_command_substitution_trailer() {
        assert_eq!(
            output_lines(b"foo\nbar\n\n"),
            vec![b"foo".to_vec(), b"bar".to_vec()]
        );
        assert_eq!(output_lines(b""), Vec::<Vec<u8>>::new());
    }
}
