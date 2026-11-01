//! The `[[ … ]]` conditional, at the level of the operators it shares with
//! `set -o`.
//!
//! Pattern matching has its own file (`pattern.rs`); this one covers `-o`,
//! which reads the shell's option table and is what startup files (zoxide's
//! hook among them) use to decide whether the shell is interactive enough to
//! record anything.

use baash_runtime::Status;
use baash_vm::Interp;

fn interp() -> Interp {
    let mut interp = Interp::new(false);
    interp.set_arg0("baash");
    interp
}

fn status(interp: &mut Interp, script: &[u8]) -> Status {
    interp.run_named("-c", script)
}

#[test]
fn the_o_test_reads_the_shell_options() {
    let mut interp = interp();
    // Off by default, and true once `set -o` turns it on.
    assert_eq!(status(&mut interp, b"[[ -o pipefail ]]"), Status(1));
    assert_eq!(
        status(&mut interp, b"set -o pipefail; [[ -o pipefail ]]"),
        Status(0)
    );
    // Negation works like any other unary test.
    assert_eq!(
        status(&mut interp, b"set +o pipefail; [[ ! -o pipefail ]]"),
        Status(0)
    );
    // The name is an ordinary operand, so it expands like one.
    assert_eq!(
        status(&mut interp, b"set -o pipefail; o=pipefail; [[ -o $o ]]"),
        Status(0)
    );
    // A name split across a quoted and an unquoted piece is still one word.
    assert_eq!(
        status(&mut interp, b"set -o pipefail; [[ -o \"pipe\"fail ]]"),
        Status(0)
    );
    // An unknown name is false, not an error: a typo in a startup file must
    // not turn a conditional into a syntax error.
    assert_eq!(status(&mut interp, b"[[ -o nosuchoption ]]"), Status(1));
    // A missing operand is a syntax error, as bash makes it.
    assert_eq!(status(&mut interp, b"[[ -o ]]"), Status(2));
}

#[test]
fn a_malformed_conditional_is_rejected_before_anything_runs() {
    let mut interp = interp();
    // The whole chunk is rejected, as bash's parser does; `touch` never runs.
    assert_eq!(
        status(&mut interp, b"echo ran > /dev/null; [[ a b ]]"),
        Status(2)
    );
    assert_eq!(status(&mut interp, b"[[ a == ]]"), Status(2));
    assert_eq!(status(&mut interp, b"[[ a -o b ]]"), Status(2));
    // A well-formed one is unaffected.
    assert_eq!(status(&mut interp, b"[[ a == a && -n b ]]"), Status(0));
}
