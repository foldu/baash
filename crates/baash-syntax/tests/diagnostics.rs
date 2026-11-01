//! Golden diagnostics.
//!
//! These pin the *rendered* text, so rewording is a deliberate act. The stable
//! part is the code (`BAASH-E####`); everything else is allowed to change, but
//! only on purpose (`DESIGN.md` §4.10).

mod common;

use baash_syntax::codes;

fn fail_one(src: &[u8]) -> (String, baash_diag::ErrCode) {
    let ds = common::fail(src);
    let first = ds.first().expect("at least one diagnostic");
    (common::render(first, "t.sh", src), first.code)
}

#[test]
fn golden_orphan_fi() {
    let (text, code) = fail_one(b"fi");
    assert_eq!(code, codes::UNEXPECTED);
    assert_eq!(
        text,
        "t.sh:1:1: error[BAASH-E0003]: unexpected `fi`\n  help: there is no `if` waiting for `fi`"
    );
}

#[test]
fn golden_orphan_paren() {
    let (text, code) = fail_one(b")");
    assert_eq!(code, codes::UNEXPECTED);
    assert_eq!(
        text,
        "t.sh:1:1: error[BAASH-E0003]: unexpected `)`\n  help: there is no `(` waiting for `)`"
    );
}

#[test]
fn golden_missing_then_points_at_the_if() {
    let (text, code) = fail_one(b"if true; fi");
    assert_eq!(code, codes::UNEXPECTED);
    assert_eq!(
        text,
        "t.sh:1:10: error[BAASH-E0003]: expected `then`, found `fi`\n  --> t.sh:1:1: `if` opened here\n  help: expected `then`"
    );
}

#[test]
fn golden_missing_do_points_at_the_loop() {
    let (text, _) = fail_one(b"while true; done");
    assert_eq!(
        text,
        "t.sh:1:13: error[BAASH-E0003]: expected `do`, found `done`\n  --> t.sh:1:1: loop opened here\n  help: expected `do`"
    );
}

#[test]
fn golden_pipeline_cannot_start_with_pipe() {
    let (text, _) = fail_one(b"echo hi | | wc");
    assert_eq!(
        text,
        "t.sh:1:11: error[BAASH-E0003]: unexpected `|`\n  help: a pipeline cannot start with `|`"
    );
}

#[test]
fn array_subscript_may_contain_spaces() {
    // bash accepts `a[1 + 2]=v`; the parser stitches the tokens back together
    // rather than diagnosing the white space.
    let ast = common::ast(b"a[1 + 2]=v");
    let command = common::first_command(&ast);
    let baash_syntax::NodeKind::Simple(command) = &ast.node(command).kind else {
        panic!("expected a simple command");
    };
    assert_eq!(command.assignments.len(), 1);
    assert!(command.assignments[0].index.is_some());
}

#[test]
fn golden_array_subscript_with_command_substitution() {
    let (text, code) = fail_one(b"a[$(x)]=v");
    assert_eq!(code, codes::ARRAY_SUBSCRIPT);
    assert_eq!(
        text,
        "t.sh:1:1: error[BAASH-E0005]: a subscript containing a command substitution is refused\n  help: the subscript would be re-parsed as code; assign to a variable first"
    );
}

#[test]
fn golden_empty_brace_group() {
    let (text, _) = fail_one(b"{ }");
    assert_eq!(
        text,
        "t.sh:1:3: error[BAASH-E0003]: `{ }` needs a command before the closing brace\n  --> t.sh:1:1: `{` opened here\n  help: write `{ :; }` for a do-nothing group"
    );
}

#[test]
fn golden_arithmetic_command_as_argument_says_how_to_nest() {
    let (text, _) = fail_one(b"echo ((1+2))");
    assert_eq!(
        text,
        "t.sh:1:6: error[BAASH-E0003]: `((` starts an arithmetic command and cannot be an argument\n  help: `((` begins arithmetic in bash; write `( (` for a nested subshell"
    );
}

#[test]
fn a_layout_separated_paren_pair_is_a_function_definition() {
    // bash allows `f ( ) { …; }`: layout may separate the name from `(`, and
    // `(` from `)`. Only something *between* the parens makes it not a
    // definition.
    common::ast(b"f ( ) { :; }");
    common::ast(b"f ()\n{ :; }");
    assert!(!common::fail(b"f (x) { :; }").is_empty());
}

#[test]
fn golden_for_variable_must_be_a_name() {
    let (text, _) = fail_one(b"for 1 in a; do :; done");
    assert_eq!(
        text,
        "t.sh:1:5: error[BAASH-E0003]: the `for` variable must be a valid name\n  help: names start with a letter or `_` and contain only letters, digits and `_`"
    );
}

#[test]
fn golden_here_document_without_delimiter() {
    let (text, code) = fail_one(b"cat <<\nbody");
    assert_eq!(code, codes::HERE_DOC);
    assert_eq!(
        text,
        "t.sh:1:5: error[BAASH-E0007]: here-document has no delimiter word\n  help: write `<<EOF` (or any word) and repeat it on a line of its own"
    );
}

#[test]
fn golden_posix_rejects_arithmetic_command() {
    let src = b"((x))";
    let ds = match common::parse_posix(src) {
        baash_syntax::ParseOutcome::Failed(ds) => ds,
        other => panic!("expected POSIX rejection, got {other:?}"),
    };
    let d = ds
        .iter()
        .find(|d| d.code == codes::BASHISM)
        .expect("a bashism diagnostic");
    assert_eq!(
        common::render(d, "t.sh", src),
        "t.sh:1:1: error[BAASH-E0004]: `((` starts an arithmetic command in bash\n  --> t.sh:1:1: not valid in POSIX mode\n  help: POSIX: write `( (` for a nested subshell"
    );
}

#[test]
fn every_diagnostic_has_a_stable_code_and_a_nonempty_span() {
    for src in [
        &b"fi"[..],
        &b"a[$(x)]=v"[..],
        &b"echo (x)"[..],
        &b"echo hi | | wc"[..],
        &b"cat <<\nbody"[..],
        &b"f (x) { :; }"[..],
    ] {
        for d in common::fail(src) {
            assert!(d.span.start <= d.span.end);
            assert!(!d.message.is_empty());
            assert!(!d.code.to_string().is_empty());
        }
    }
}
