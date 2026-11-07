//! Bash surface syntax that only shows up in real scripts.
//!
//! Each case here came out of parsing the installed shell scripts (the
//! `scripts/parse_corpus.py` run): bash accepted the file and baash did not.
//! They are deliberately about the *grammar* around a construct, not its
//! execution, so every assertion is a parse and, where useful, a shape.

mod common;

use baash_syntax::NodeKind;

#[test]
fn empty_array_literal_may_contain_layout() {
    // Multiline arrays still use the newline as an element separator.
    common::ast(b"a=(\n  one\n  two\n)");
    // `a=( )` is an empty array; the space is layout, not an element. This is
    // how Nix's wrapped shell scripts write an empty accumulator.
    let src = b"a=( )";
    let ast = common::ast(src);
    let Some(NodeKind::Simple(cmd)) =
        common::find_node(&ast, |kind| matches!(kind, NodeKind::Simple(_)))
    else {
        panic!("no simple command");
    };
    assert_eq!(cmd.assignments.len(), 1);
    assert!(
        cmd.assignments[0]
            .array
            .as_ref()
            .is_some_and(|elements| elements.is_empty())
    );
}

#[test]
fn a_reserved_word_may_follow_a_compound_command_without_a_semicolon() {
    // A compound command is self-delimiting. bash accepts all of these; only
    // after a simple command (`if true then`) is the separator required.
    for src in [
        &b"if [[ x ]] then :; fi"[..],
        b"if ((1)) then :; fi",
        b"if { :; } then :; fi",
        b"if (:) then :; fi",
        b"while :; do case x in esac done",
        b"while :; do if :; then :; fi done",
    ] {
        common::ast(src);
    }
    common::fail(b"if true then :; fi");
}

#[test]
fn a_case_pattern_may_be_a_brace() {
    // `{` and `}` are reserved words everywhere else, but in pattern position
    // bash reads them as ordinary patterns. git-completion.bash relies on it.
    let ast = common::ast(b"case $x in {) :;; }) :;; esac");
    let Some(NodeKind::Case { arms, .. }) =
        common::find_node(&ast, |kind| matches!(kind, NodeKind::Case { .. }))
    else {
        panic!("no case");
    };
    assert_eq!(arms.len(), 2);
    assert_eq!(arms[0].patterns.len(), 1);
    assert_eq!(arms[1].patterns.len(), 1);
}

#[test]
fn an_extglob_pattern_may_start_a_case_line() {
    // After `in` + newline the lexer is back at statement start, where a
    // leading `!(` normally means pipeline negation. In pattern position it is
    // the extglob group: bash-completion's ssh.bash writes it this way.
    common::ast(b"case $cur in\n!(*:*)/* | [.~]*) ;;\nesac");
}

#[test]
fn a_brace_group_may_replace_do_done_in_a_for_loop() {
    // swtpm's test script writes `for ((…)) { … }`; bash accepts the brace
    // form for both the arithmetic and the word-list loop.
    common::ast(b"for ((i = 0; i < 2; i++)) { echo $i; }");
    common::ast(b"for i in a b; { echo $i; }");
    common::ast(b"for i; { echo $i; }");
}

#[test]
fn legacy_dollar_bracket_arithmetic() {
    // drbd's OCF agent still uses `$[1 << 16]`, the pre-POSIX spelling of
    // `$((1 << 16))`.
    let ast = common::ast(b"x=$[1 << 16]");
    assert!(common::find_node(&ast, |kind| matches!(kind, NodeKind::Simple(_))).is_some());
    common::ast(b"echo $[1 + 2 * 3]");
}

#[test]
fn a_backslash_newline_cannot_glue_a_reserved_word_to_the_pipeline_bar() {
    // POSIX deletes `\`+newline before tokenising, so `echo | \` ⏎ `while`
    // still has `while` in command position.
    let ast = common::ast(b"echo a | \\\nwhile read x; do :; done");
    let Some(NodeKind::While { .. }) =
        common::find_node(&ast, |kind| matches!(kind, NodeKind::While { .. }))
    else {
        panic!("no while");
    };
}

#[test]
fn a_regex_operand_is_not_arithmetic() {
    // `=~ ` operands are arbitrary ERE text. Two unescaped `((` in the regex
    // used to be lexed as an arithmetic command, swallowing the closing `]]`.
    for src in [
        &br"[[ $line =~ ^#define\ +([A-Z0-9_]+k?)\ +\(\(([A-Z]+)\)((0x)?[0-9A-Fa-f]+)L?\) ]]"[..],
        br"[[ $line =~ ^[[:blank:]]{1,10}(((,[[:blank:]])?(--?([\]\[a-zA-Z0-9?=-]+))(,[[:space:]])?)+).*$ ]]",
        br"[[ $x =~ ((a|b)+) ]]",
    ] {
        let ast = common::ast(src);
        assert!(common::find_node(&ast, |kind| matches!(kind, NodeKind::TestCommand { .. })).is_some());
    }
}

// The remaining store cases are language features baash does not implement
// yet. They stay as ignored tests so the corpus has a named home for each one
// and enabling it is a visible change.

#[test]
#[ignore = "known gap: `select` is not implemented"]
fn select_command() {
    common::ast(b"select x in a b; do echo $x; done");
}

#[test]
#[ignore = "known gap: `coproc` is not implemented"]
fn coproc_command() {
    common::ast(b"coproc watcher { cat; }");
}

#[test]
#[ignore = "known deviation: `((` always starts arithmetic (DESIGN.md 3.3)"]
fn parens_that_bash_reparses_as_nested_subshells() {
    // zdiff and init-script-builder write `((cmd)` / `$((cmd) | …)`, which
    // bash tries as arithmetic and then re-reads as nested subshells.
    common::ast(b"x=$((echo a) | cat)");
    common::ast(b"((echo a) | cat)");
}
