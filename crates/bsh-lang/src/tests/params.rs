//! Parameter expansion operators that need no pattern matching.

use crate::fixture::TestShell;
use crate::{
    ErrorKind, ParamExp, ParamName, ParamOp, Part, UnsupportedOp, Vars, Word, expand_word,
};

use super::{argv, assert_argv, expand_one, expand_one_bytes, try_expand_line, values};

#[test]
fn default_and_alternate_operators() {
    // Unset.
    let mut shell = TestShell::new();
    assert_argv(&one("${x:-d}", &mut shell), &["d"], "${x:-d}");
    assert_argv(&one("${x-d}", &mut shell), &["d"], "${x-d}");
    assert_argv(&one("${x:+d}", &mut shell), &[], "${x:+d}");
    assert_argv(&one("${x+d}", &mut shell), &[], "${x+d}");
    assert_argv(&one("\"${x:+d}\"", &mut shell), &[""], "\"${x:+d}\"");

    // Null: the colon forms treat null like unset; the others do not.
    let mut shell = TestShell::new().var(b"x", b"");
    assert_argv(&one("${x:-d}", &mut shell), &["d"], "null ${x:-d}");
    assert_argv(&one("${x-d}", &mut shell), &[], "null ${x-d}");
    assert_argv(&one("\"${x-d}\"", &mut shell), &[""], "null \"${x-d}\"");
    assert_argv(&one("${x:+d}", &mut shell), &[], "null ${x:+d}");
    assert_argv(&one("${x+d}", &mut shell), &["d"], "null ${x+d}");

    // Set and non-null.
    let mut shell = TestShell::new().var(b"x", b"v");
    assert_argv(&one("${x:-d}", &mut shell), &["v"], "set ${x:-d}");
    assert_argv(&one("${x-d}", &mut shell), &["v"], "set ${x-d}");
    assert_argv(&one("${x:+d}", &mut shell), &["d"], "set ${x:+d}");
    assert_argv(&one("${x+d}", &mut shell), &["d"], "set ${x+d}");
    assert_argv(&one("\"${x}\"", &mut shell), &["v"], "set \"${x}\"");
}

#[test]
fn empty_replacement_words() {
    let mut shell = TestShell::new();
    assert_argv(&one("${x:-}", &mut shell), &[], "unquoted empty");
    assert_argv(&one("\"${x:-}\"", &mut shell), &[""], "quoted empty");
    assert_argv(&one("\"${x:+}\"", &mut shell), &[""], "alternate empty");
}

#[test]
fn assign_default_sets_the_variable() {
    let mut shell = TestShell::new();
    assert_argv(&one("${x:=v}", &mut shell), &["v"], "${x:=v}");
    assert_eq!(shell.get(b"x"), Some(&b"v"[..]));
    assert_eq!(shell.set_calls, vec![(b"x".to_vec(), b"v".to_vec())]);

    // `=`, unlike `:=`, does not assign over a null value.
    let mut shell = TestShell::new().var(b"x", b"");
    assert_argv(&one("${x=v}", &mut shell), &[], "null ${x=v}");
    assert_eq!(shell.get(b"x"), Some(&b""[..]));
    assert!(shell.set_calls.is_empty());

    let mut shell = TestShell::new().var(b"x", b"");
    assert_argv(&one("${x:=v}", &mut shell), &["v"], "null ${x:=v}");
    assert_eq!(shell.get(b"x"), Some(&b"v"[..]));

    // A set, non-null value is left alone and nothing is assigned.
    let mut shell = TestShell::new().var(b"x", b"keep");
    assert_argv(&one("${x:=v}", &mut shell), &["keep"], "set ${x:=v}");
    assert!(shell.set_calls.is_empty());

    let mut shell = TestShell::new();
    assert_argv(&one("${x:=}", &mut shell), &[], "empty assignment");
    assert_eq!(shell.get(b"x"), Some(&b""[..]));
}

#[test]
fn assignment_to_something_unassignable_is_an_error() {
    let mut shell = TestShell::new();
    let err = try_expand_line("${1:=v}", &mut shell).unwrap_err();
    assert!(
        matches!(&err.kind, ErrorKind::CannotAssign { name } if name == b"1"),
        "got {err:?}"
    );

    let mut shell = TestShell::new().read_only();
    let err = try_expand_line("${x:=v}", &mut shell).unwrap_err();
    assert!(matches!(err.kind, ErrorKind::CannotAssign { .. }));
    assert_eq!(err.code(), crate::codes::CANNOT_ASSIGN);
}

#[test]
fn error_operator_reports_like_bash() {
    let mut shell = TestShell::new();
    let err = try_expand_line("${x:?}", &mut shell).unwrap_err();
    match &err.kind {
        ErrorKind::ParameterNull { name, message } => {
            assert_eq!(name, b"x");
            assert!(message.is_none());
        }
        other => panic!("unexpected kind {other:?}"),
    }
    assert_eq!(err.to_string(), "x: parameter null or not set");

    let mut shell = TestShell::new();
    let err = try_expand_line("${x?}", &mut shell).unwrap_err();
    assert_eq!(err.to_string(), "x: parameter not set");

    let mut shell = TestShell::new();
    let err = try_expand_line("${x:?boom}", &mut shell).unwrap_err();
    assert_eq!(err.to_string(), "x: boom");

    // The message word is expanded.
    let mut shell = TestShell::new().var(b"y", b"hello");
    let err = try_expand_line("${x:?$y}", &mut shell).unwrap_err();
    assert_eq!(err.to_string(), "x: hello");

    // `?` without a colon only tests unset; a null value passes.
    let mut shell = TestShell::new().var(b"x", b"");
    assert_argv(&one("${x?}", &mut shell), &[], "null ${x?}");

    // A set value is substituted and no message is produced.
    let mut shell = TestShell::new().var(b"x", b"v");
    assert_argv(&one("${x:?boom}", &mut shell), &["v"], "set ${x:?}");

    let mut shell = TestShell::new();
    let err = try_expand_line("${x:?}", &mut shell).unwrap_err();
    assert_eq!(err.code(), crate::codes::PARAMETER_NULL);
    assert_eq!(err.diagnostic().code, crate::codes::PARAMETER_NULL);
}

#[test]
fn length_counts_bytes() {
    let mut shell = TestShell::new();
    assert_argv(&one("${#x}", &mut shell), &["0"], "unset");
    shell = TestShell::new().var(b"x", b"");
    assert_argv(&one("${#x}", &mut shell), &["0"], "null");
    shell = TestShell::new().var(b"x", b"aBc");
    assert_argv(&one("${#x}", &mut shell), &["3"], "set");

    // Byte-oriented, i.e. LC_ALL=C: two-byte UTF-8 and invalid bytes count as
    // bytes. bash in a UTF-8 locale counts characters; see the crate docs.
    let mut shell = TestShell::new().var(b"x", b"a\xff\xfe");
    assert_eq!(expand_one_bytes(b"${#x}", &mut shell), argv(&["3"]));

    let mut shell = TestShell::new().positionals(&[b"a", b"b", b"c"]);
    assert_argv(&one("${#1}", &mut shell), &["1"], "${#1}");
    assert_argv(&one("${#4}", &mut shell), &["0"], "${#4}");
}

#[test]
fn substring_offsets_and_lengths() {
    let mut shell = TestShell::new().var(b"x", b"abcdef");
    assert_argv(&one("${x:2}", &mut shell), &["cdef"], "${x:2}");
    assert_argv(&one("${x:2:3}", &mut shell), &["cde"], "${x:2:3}");
    assert_argv(&one("${x: -2}", &mut shell), &["ef"], "${x: -2}");
    assert_argv(&one("${x: -3:2}", &mut shell), &["de"], "${x: -3:2}");
    assert_argv(&one("${x:1:-1}", &mut shell), &["bcde"], "${x:1:-1}");
    assert_argv(&one("${x:2:-2}", &mut shell), &["cd"], "${x:2:-2}");
    assert_argv(&one("\"${x:0:0}\"", &mut shell), &[""], "empty quoted");
    assert_argv(&one("${x:0:0}", &mut shell), &[], "empty unquoted");
    assert_argv(&one("${x:20}", &mut shell), &[], "past the end");
    assert_argv(&one("${x: -20}", &mut shell), &[], "far before the start");
    assert_argv(&one("${x:6}", &mut shell), &[], "exactly at the end");

    // Unset is empty, not an error.
    let mut shell = TestShell::new();
    assert_argv(&one("\"${x:2}\"", &mut shell), &[""], "unset");

    // Positional parameters work too.
    let mut shell = TestShell::new().positionals(&[b"abc"]);
    assert_argv(&one("${1:1}", &mut shell), &["bc"], "${1:1}");
    assert_argv(&one("${1: -1}", &mut shell), &["c"], "${1: -1}");

    // A negative length that reaches before the start is an error in bash.
    let mut shell = TestShell::new().var(b"x", b"abc");
    let err = try_expand_line("${x:2:-2}", &mut shell).unwrap_err();
    assert!(matches!(err.kind, ErrorKind::SubstringRange { value: -2 }));
    assert_eq!(err.to_string(), "-2: substring expression < 0");
}

#[test]
fn case_conversion_is_ascii() {
    let mut shell = TestShell::new().var(b"x", b"aBc DeF");
    assert_argv(
        &one("\"${x^^}\"", &mut shell),
        &["ABC DEF"],
        "quoted ${x^^}",
    );
    assert_argv(
        &one("\"${x,,}\"", &mut shell),
        &["abc def"],
        "quoted ${x,,}",
    );
    assert_argv(&one("\"${x^}\"", &mut shell), &["ABc DeF"], "quoted ${x^}");
    assert_argv(&one("\"${x,}\"", &mut shell), &["aBc DeF"], "quoted ${x,}");
    // Unquoted, the converted value is an expansion result and splits.
    assert_argv(
        &one("${x^^}", &mut shell),
        &["ABC", "DEF"],
        "unquoted ${x^^}",
    );

    let mut shell = TestShell::new();
    assert_argv(&one("${x^^}", &mut shell), &[], "unset");

    // ASCII only: 0xff has no upper-case form and is left alone.
    let mut shell = TestShell::new().var(b"x", b"\xffz");
    assert_eq!(expand_one_bytes(b"${x^^}", &mut shell), values(&[b"\xffZ"]));
}

#[test]
fn unsupported_operators_fail_instead_of_guessing() {
    let cases = [
        ("${x#p}", UnsupportedOp::RemovePrefix),
        ("${x##p}", UnsupportedOp::RemovePrefix),
        ("${x%p}", UnsupportedOp::RemoveSuffix),
        ("${x%%p}", UnsupportedOp::RemoveSuffix),
        ("${x/a/b}", UnsupportedOp::Replace),
        ("${!x}", UnsupportedOp::Indirect),
        ("${x@Q}", UnsupportedOp::Transform),
        ("${x@P}", UnsupportedOp::Transform),
        ("${@^^}", UnsupportedOp::CaseOfList),
        ("${a[@]^^}", UnsupportedOp::CaseOfList),
    ];
    for (source, expected) in cases {
        let mut shell = TestShell::new().var(b"x", b"abc").positionals(&[b"a"]);
        let err = try_expand_line(source, &mut shell)
            .err()
            .unwrap_or_else(|| panic!("{source} should not expand"));
        assert_eq!(err.kind, ErrorKind::Unsupported(expected), "word {source}");
        assert_eq!(err.code(), crate::codes::UNSUPPORTED);
    }
}

/// The unsupported error must not leave a partial result behind, and it must
/// carry the span of the expansion.
#[test]
fn unsupported_error_has_the_expansion_span() {
    let word = Word::new(vec![
        Part::lit(b"pre"),
        Part::Param(ParamExp::new(
            ParamName::var(b"x"),
            ParamOp::Unsupported(UnsupportedOp::Transform),
            3..10,
        )),
        Part::lit(b"post"),
    ]);
    let mut shell = TestShell::new().var(b"x", b"abc");
    let err = expand_word(&word, &mut shell).unwrap_err();
    assert_eq!(err.span, 3..10);
}

#[test]
fn unused_default_words_are_not_expanded() {
    let mut shell = TestShell::new().var(b"x", b"set").cmdsub(b"cmd", b"out");
    let got = expand_one("${x:-$(cmd)}", &mut shell);
    assert_argv(&got, &["set"], "${x:-$(cmd)} with x set");
    assert!(
        shell.cmdsub_calls.is_empty(),
        "a replacement word that is not used must not run"
    );

    let mut shell = TestShell::new().cmdsub(b"cmd", b"out");
    let got = expand_one("${x:-$(cmd)}", &mut shell);
    assert_argv(&got, &["out"], "${x:-$(cmd)} with x unset");
    assert_eq!(shell.cmdsub_calls, vec![b"cmd".to_vec()]);
}

#[test]
fn expansion_is_left_to_right() {
    let mut shell = TestShell::new().cmdsub(b"one", b"1").cmdsub(b"two", b"2");
    let got = expand_one("$(one)$(two)", &mut shell);
    assert_argv(&got, &["12"], "adjacent substitutions");
    assert_eq!(shell.cmdsub_calls, vec![b"one".to_vec(), b"two".to_vec()]);
}

#[test]
fn special_parameters_are_ordinary_lookups() {
    let mut shell = TestShell::new().var(b"?", b"42").var(b"0", b"prog");
    assert_argv(&one("$?", &mut shell), &["42"], "$?");
    assert_argv(&one("${?}", &mut shell), &["42"], "${?}");
    assert_argv(&one("$0", &mut shell), &["prog"], "$0");
    assert_argv(&one("\"$9\"", &mut shell), &[""], "out-of-range positional");
}

#[test]
fn assignment_operator_flattens_positionals() {
    let mut shell = TestShell::new().positionals(&[b"a", b"b"]);
    let got = expand_one("\"${x:=$@}\"", &mut shell);
    assert_argv(&got, &["a b"], "\"${x:=$@}\"");
    assert_eq!(shell.get(b"x"), Some(&b"a b"[..]));
}

fn one(src: &str, shell: &mut TestShell) -> Vec<Vec<u8>> {
    expand_one(src, shell)
}
