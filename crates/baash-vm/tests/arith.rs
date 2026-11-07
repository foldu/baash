//! `(( … ))` and `$(( … ))` through the real evaluator.
//!
//! Arithmetic is word-expanded before it is evaluated, in both spellings: the
//! operators come from `arith`, but `$#`, `${#a[@]}` and `$n` are the word
//! layer's job. The command form used to skip that step, so `(( $# == 0 ))`
//! reported "expected a name" — the sort of line bash-completion and every
//! loop-counter script writes.

use baash_lang::Vars;
use baash_vm::Interp;

fn run(interp: &mut Interp, script: &str) {
    let status = interp.run_named("<arith-test>", script.as_bytes());
    assert!(status.is_success(), "script failed: {script}");
}

fn text(interp: &Interp, name: &[u8]) -> String {
    String::from_utf8_lossy(Vars::get(interp, name).unwrap_or_default()).into_owned()
}

#[test]
fn the_command_form_expands_parameters() {
    let mut interp = Interp::new(false);
    interp.set_arg0("baash");
    run(
        &mut interp,
        "set -- a b\n(( $# == 2 )); positional=$?\na=(x y)\n(( ${#a[@]} == 2 )); count=$?\nn=3\n(( $n > 2 )); number=$?\n",
    );
    assert_eq!(text(&interp, b"positional"), "0");
    assert_eq!(text(&interp, b"count"), "0");
    assert_eq!(text(&interp, b"number"), "0");
}

#[test]
fn the_expansion_form_still_expands_parameters() {
    let mut interp = Interp::new(false);
    interp.set_arg0("baash");
    run(
        &mut interp,
        "set -- a b\nsum=$(( $# + 1 ))\nn=6\nproduct=$(( $n * 7 ))\n",
    );
    assert_eq!(text(&interp, b"sum"), "3");
    assert_eq!(text(&interp, b"product"), "42");
}

#[test]
fn plain_arithmetic_is_unchanged() {
    let mut interp = Interp::new(false);
    interp.set_arg0("baash");
    run(
        &mut interp,
        "(( x = 2 + 3 ))\n(( x++ ))\n(( y = x ** 2 ))\n",
    );
    assert_eq!(text(&interp, b"x"), "6");
    assert_eq!(text(&interp, b"y"), "36");
}
