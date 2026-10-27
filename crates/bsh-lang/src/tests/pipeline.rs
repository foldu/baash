//! The pipeline seams: tilde, globbing, command substitution, arith, and the
//! assignment mode.

use crate::fixture::{parse_word, parse_words};
use crate::{
    CmdSubStyle, CommandSub, ErrorKind, Field, Mode, Part, Passthrough, ProcSubDir, ProcessSub,
    UnsupportedOp, Word, expand_assignment, expand_word, expand_word_with, expand_words,
};

use super::{argv, assert_argv, expand_line, expand_one, try_expand_line};

#[test]
fn tilde_expands_the_home_directory() {
    let mut shell = TestShell::new().var(b"HOME", b"/home/u");
    assert_argv(&one("~/src", &mut shell), &["/home/u/src"], "~/src");
    assert_argv(&one("~", &mut shell), &["/home/u"], "~");
    assert_argv(&one("~/", &mut shell), &["/home/u/"], "~/");
    // Not word-initial: no expansion.
    assert_argv(&one("a~", &mut shell), &["a~"], "a~");
    // Quoted: no expansion.
    assert_argv(&one("\"~\"", &mut shell), &["~"], "\"~\"");
    // Unknown login: left alone, as bash does.
    assert_argv(
        &one("~nosuch/x", &mut shell),
        &["~nosuch/x"],
        "unknown user",
    );
}

#[test]
fn tilde_users_and_pwd() {
    let mut shell = TestShell::new().home(b"bob", b"/home/bob");
    assert_argv(&one("~bob/x", &mut shell), &["/home/bob/x"], "~bob/x");
    assert_argv(&one("~bob", &mut shell), &["/home/bob"], "~bob");

    let mut shell = TestShell::new()
        .var(b"PWD", b"/here")
        .var(b"OLDPWD", b"/there");
    assert_argv(&one("~+", &mut shell), &["/here"], "~+");
    assert_argv(&one("~-", &mut shell), &["/there"], "~-");
    // bash's directory-stack forms are deliberately left alone.
    assert_argv(&one("~+2", &mut shell), &["~+2"], "~+2");
    // Without PWD, `~+` stays literal.
    let mut shell = TestShell::new();
    assert_argv(&one("~+", &mut shell), &["~+"], "~+ with no PWD");
}

#[test]
fn tilde_result_is_quoted() {
    // A home directory with a space is one field.
    let mut shell = TestShell::new().var(b"HOME", b"/home/a b");
    assert_argv(&one("~", &mut shell), &["/home/a b"], "space in HOME");

    // A home directory with glob metacharacters is not a pattern.
    let mut shell = TestShell::new()
        .var(b"HOME", b"/home/a*")
        .glob(b"/home/a*", &[b"/home/a1"]);
    assert_argv(&one("~", &mut shell), &["/home/a*"], "glob chars in HOME");
    assert!(
        shell.glob_calls.is_empty(),
        "tilde expansion results are treated as quoted"
    );
}

#[test]
fn tilde_in_parameter_replacement_words() {
    let mut shell = TestShell::new().var(b"HOME", b"/home/u");
    assert_argv(&one("${x:-~}", &mut shell), &["/home/u"], "${x:-~}");
    assert_argv(&one("\"${x:-~}\"", &mut shell), &["~"], "quoted");
    assert_argv(
        &one("${x:-a~/b}", &mut shell),
        &["a~/b"],
        "not word-initial",
    );
}

#[test]
fn pathname_expansion_uses_the_callback() {
    let mut shell = TestShell::new().glob(b"*.rs", &[b"a.rs", b"b.rs"]);
    assert_argv(&one("*.rs", &mut shell), &["a.rs", "b.rs"], "*.rs");
    assert_eq!(shell.glob_calls, vec![b"*.rs".to_vec()]);

    // Quoted and escaped metacharacters are inert.
    let mut shell = TestShell::new().glob(b"*.rs", &[b"a.rs"]);
    assert_argv(&one("\"*.rs\"", &mut shell), &["*.rs"], "quoted");
    assert_argv(&one("\\*.rs", &mut shell), &["*.rs"], "escaped");
    assert!(shell.glob_calls.is_empty());

    // No match: the word is left unchanged (POSIX default, bash without
    // nullglob).
    let mut shell = TestShell::new();
    assert_argv(&one("*.nope", &mut shell), &["*.nope"], "no match");

    // A partly quoted pattern still globs, with a quoting mask.
    let mut shell = TestShell::new().glob(b"a*.rs", &[b"ab.rs"]);
    assert_argv(&one("\"a\"*.rs", &mut shell), &["ab.rs"], "partly quoted");
    assert_eq!(shell.glob_calls, vec![b"a*.rs".to_vec()]);
}

#[test]
fn pathname_expansion_does_not_recurse() {
    // The callback's matches are marked quoted, so they are never globbed
    // again even if they contain metacharacters.
    let mut shell = TestShell::new().glob(b"*.x", &[b"a*.x"]);
    assert_argv(&one("*.x", &mut shell), &["a*.x"], "match with a star");
    assert_eq!(shell.glob_calls.len(), 1);
}

#[test]
fn quoting_mask_controls_globbing() {
    assert!(Field::plain(b"*.rs".to_vec()).has_active_glob());
    assert!(!Field::matched(b"*.rs".to_vec()).has_active_glob());
    assert!(!Field::new(b"*.rs".to_vec(), vec![true, false, false, false]).has_active_glob());
    assert!(!Field::plain(b"plain".to_vec()).has_active_glob());
    assert!(Field::plain(b"a?b".to_vec()).has_active_glob());
    assert!(Field::plain(b"[ab]".to_vec()).has_active_glob());
}

#[test]
fn command_substitution_is_split_unless_quoted() {
    let mut shell = TestShell::new().cmdsub(b"echo hi", b"hi there");
    assert_argv(&one("$(echo hi)", &mut shell), &["hi", "there"], "unquoted");
    assert_argv(&one("\"$(echo hi)\"", &mut shell), &["hi there"], "quoted");
    assert_argv(
        &one("x$(echo hi)y", &mut shell),
        &["xhi", "therey"],
        "inside a word",
    );
    assert_eq!(shell.cmdsub_calls.len(), 3);
}

#[test]
fn backtick_command_substitution() {
    let mut shell = TestShell::new().cmdsub(b"echo hi", b"out");
    assert_argv(&one("`echo hi`", &mut shell), &["out"], "backticks");
    assert_eq!(shell.cmdsub_calls, vec![b"echo hi".to_vec()]);
}

#[test]
fn arithmetic_expansion_uses_the_callback() {
    let mut shell = TestShell::new().arith(b"1+2", b"3");
    assert_argv(&one("$((1+2))", &mut shell), &["3"], "$((1+2))");
    assert_eq!(shell.arith_calls, vec![b"1+2".to_vec()]);
}

#[test]
fn host_errors_propagate() {
    let mut shell = TestShell::new();
    let err = try_expand_line("$(missing)", &mut shell).unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Host { .. }), "got {err:?}");
    assert_eq!(err.code(), crate::codes::HOST);
}

#[test]
fn process_substitution_is_unsupported() {
    let word = Word::new(vec![Part::ProcSub(ProcessSub {
        source: b"cmd".to_vec(),
        dir: ProcSubDir::Input,
        span: 4..9,
    })]);
    let mut shell = TestShell::new();
    let err = expand_word(&word, &mut shell).unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::Unsupported(UnsupportedOp::ProcessSubstitution)
    );
    assert_eq!(err.span, 4..9);
}

#[test]
fn passthrough_stub_leaves_unimplemented_stages_unchanged() {
    let mut shell = Passthrough::empty();
    // `$(cmd --flag)` comes back as its source text, which is still an
    // unquoted expansion result, so the pipeline splits it: the stub is
    // visible, not silently authoritative.
    let words = parse_words(b"$(cmd --flag) *.rs");
    let got = expand_words(&words, &mut shell).expect("stub expansion");
    assert_eq!(got, argv(&["cmd", "--flag", "*.rs"]));
}

#[test]
fn assignment_mode_does_not_split_or_glob() {
    let mut shell = TestShell::new().var(b"x", b"a b");
    let word = parse_word(b"$x");
    assert_eq!(
        expand_assignment(&word, &mut shell).expect("assignment"),
        b"a b"
    );
    assert_argv(
        &expand_word(&word, &mut shell).expect("command"),
        &["a", "b"],
        "the same word in command mode",
    );

    // No pathname expansion in an assignment value.
    let mut shell = TestShell::new().glob(b"*", &[b"a"]);
    let word = parse_word(b"*");
    assert_eq!(
        expand_assignment(&word, &mut shell).expect("assignment"),
        b"*"
    );
    assert!(shell.glob_calls.is_empty());

    // Tilde expansion still happens, and a glob in the result stays literal.
    let mut shell = TestShell::new().var(b"HOME", b"/home/u");
    let word = parse_word(b"~/src");
    assert_eq!(
        expand_assignment(&word, &mut shell).expect("assignment"),
        b"/home/u/src"
    );
}

#[test]
fn assignment_of_positional_parameters() {
    let mut shell = TestShell::new().ifs(b":").positionals(&[b"a", b"b"]);
    let cases: &[(&str, &[u8])] = &[
        ("$@", b"a b"),
        ("\"$@\"", b"a b"),
        ("$*", b"a:b"),
        ("\"$*\"", b"a:b"),
    ];
    for (source, want) in cases {
        let word = parse_word(source.as_bytes());
        let got = expand_assignment(&word, &mut shell).expect("assignment");
        assert_eq!(got.as_slice(), *want, "{source}");
    }

    // Empty parameters survive into the scalar form: `$@` joins with a space
    // (ignoring IFS), `$*` joins with the first IFS character.
    let mut shell = TestShell::new().ifs(b":").positionals(&[b"a", b"", b"b"]);
    let cases: &[(&str, &[u8])] = &[
        ("$@", b"a  b"),
        ("\"$@\"", b"a  b"),
        ("$*", b"a::b"),
        ("\"$*\"", b"a::b"),
    ];
    for (source, want) in cases {
        let word = parse_word(source.as_bytes());
        let got = expand_assignment(&word, &mut shell).expect("assignment");
        assert_eq!(got.as_slice(), *want, "{source}");
    }

    // No parameters is the empty string, not "nothing": an assignment always
    // produces exactly one value.
    let mut shell = TestShell::new();
    let word = parse_word(b"$@");
    assert_eq!(
        expand_assignment(&word, &mut shell).expect("assignment"),
        b""
    );
}

#[test]
fn words_expand_independently() {
    let mut shell = TestShell::new().positionals(&[b"a b", b"c"]);
    let got = expand_line("pre\"$@\"post $unset", &mut shell);
    assert_argv(&got, &["prea b", "cpost"], "adjacent fields stay per word");
}

#[test]
fn expand_word_with_assignment_mode_returns_one_value() {
    let mut shell = TestShell::new().var(b"x", b"a b");
    let word = parse_word(b"$x");
    let got = expand_word_with(&word, &mut shell, Mode::Assignment).expect("assignment");
    assert_eq!(got, vec![b"a b".to_vec()]);
}

#[test]
fn command_substitution_uses_the_declared_style() {
    let word = Word::new(vec![Part::CmdSub(CommandSub::dollar_paren(b"cmd"))]);
    let mut shell = TestShell::new().cmdsub(b"cmd", b"out");
    assert_argv(
        &expand_word(&word, &mut shell).expect("command substitution"),
        &["out"],
        "$(cmd)",
    );
    assert_eq!(
        word.parts,
        vec![Part::CmdSub(CommandSub {
            source: b"cmd".to_vec(),
            style: CmdSubStyle::DollarParen,
            span: 0..0,
        })]
    );
}

// Keep the test helper import path visible to the reader of the module.
use crate::fixture::TestShell;

fn one(src: &str, shell: &mut TestShell) -> Vec<Vec<u8>> {
    expand_one(src, shell)
}
