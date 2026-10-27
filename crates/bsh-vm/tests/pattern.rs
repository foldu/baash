//! `case` and `[[ … == … ]]` on the shared pattern engine.
//!
//! The two consumers share one matcher but not one dialect: `[[ … == … ]]`
//! forces extglob on (bash does, even with `shopt -u extglob`) while `case`
//! honours the option. Both honour `nocasematch`, and neither may affect the
//! four strip operators.

use bsh_lang::Vars;
use bsh_runtime::Status;
use bsh_vm::Interp;

fn interp() -> Interp {
    let mut interp = Interp::new(false);
    interp.set_arg0("bsh");
    interp
}

fn var(interp: &Interp, name: &[u8]) -> Option<String> {
    Vars::get(interp, name).map(|bytes| String::from_utf8_lossy(bytes).into_owned())
}

#[test]
fn case_honours_quoting_inside_the_pattern() {
    let mut interp = interp();
    // `a\*b` is a literal star, `a*b` is a pattern.
    assert_eq!(
        interp.run_named("-c", b"case 'a*b' in a\\*b) r=quoted;; *) r=miss;; esac"),
        Status(0)
    );
    assert_eq!(var(&interp, b"r").as_deref(), Some("quoted"));

    assert_eq!(
        interp.run_named("-c", b"case axb in a\\*b) r=quoted;; *) r=miss;; esac"),
        Status(0)
    );
    assert_eq!(var(&interp, b"r").as_deref(), Some("miss"));

    assert_eq!(
        interp.run_named("-c", b"case axb in a*b) r=pattern;; *) r=miss;; esac"),
        Status(0)
    );
    assert_eq!(var(&interp, b"r").as_deref(), Some("pattern"));
}

#[test]
fn conditional_forces_extglob_even_when_shopt_is_off() {
    let mut interp = interp();
    assert_eq!(interp.run_named("-c", b"shopt -u extglob"), Status(0));
    assert_eq!(interp.run_named("-c", b"[[ cc == @(cc) ]]"), Status(0));
    assert_eq!(interp.run_named("-c", b"[[ cc == @(dd) ]]"), Status(1));
    assert_eq!(interp.run_named("-c", b"[[ cc != @(dd) ]]"), Status(0));
    assert_eq!(interp.run_named("-c", b"[[ ac == a@(b|c) ]]"), Status(0));

    // `case` still honours the option: the same pattern from a variable.
    assert_eq!(
        interp.run_named("-c", b"p='@(cc)'; case cc in $p) r=hit;; *) r=miss;; esac"),
        Status(0)
    );
    assert_eq!(var(&interp, b"r").as_deref(), Some("miss"));
    assert_eq!(
        interp.run_named(
            "-c",
            b"shopt -s extglob; case cc in $p) r=hit;; *) r=miss;; esac"
        ),
        Status(0)
    );
    assert_eq!(var(&interp, b"r").as_deref(), Some("hit"));
}

#[test]
fn conditional_keeps_bang_and_grouping() {
    let mut interp = interp();
    // `!(` at the start of an expression is negation of a group in bash, not
    // the extglob operator; the pattern-reading context must not leak.
    assert_eq!(interp.run_named("-c", b"[[ !(a) ]]"), Status(1));
    assert_eq!(interp.run_named("-c", b"[[ ! (a) ]]"), Status(1));
    assert_eq!(interp.run_named("-c", b"[[ ! a == b ]]"), Status(0));
    assert_eq!(interp.run_named("-c", b"[[ ( a == a ) ]]"), Status(0));
}

#[test]
fn nocasematch_folds_case_but_strips_do_not() {
    let mut interp = interp();
    assert_eq!(interp.run_named("-c", b"shopt -s nocasematch"), Status(0));

    assert_eq!(
        interp.run_named("-c", b"case a in A) r=case;; *) r=miss;; esac"),
        Status(0)
    );
    assert_eq!(var(&interp, b"r").as_deref(), Some("case"));
    assert_eq!(
        interp.run_named("-c", b"case A in [a]) r=bracket;; *) r=miss;; esac"),
        Status(0)
    );
    assert_eq!(var(&interp, b"r").as_deref(), Some("bracket"));

    assert_eq!(interp.run_named("-c", b"[[ a == A ]]"), Status(0));
    assert_eq!(interp.run_named("-c", b"[[ A == [a] ]]"), Status(0));

    // `nocasematch` does not reach the four strip operators.
    assert_eq!(interp.run_named("-c", b"x=ABC; y=${x#a}"), Status(0));
    assert_eq!(var(&interp, b"y").as_deref(), Some("ABC"));
    assert_eq!(interp.run_named("-c", b"x=ABC; y=${x%[c]}"), Status(0));
    assert_eq!(var(&interp, b"y").as_deref(), Some("ABC"));

    // And off again, the fold is gone.
    assert_eq!(interp.run_named("-c", b"shopt -u nocasematch"), Status(0));
    assert_eq!(interp.run_named("-c", b"[[ a == A ]]"), Status(1));
}

#[test]
fn an_unmatched_glob_leaves_the_word_alone() {
    let mut interp = interp();
    assert_eq!(
        interp.run_named("-c", b"r=; for x in zz-no-such-file-*; do r=$x; done"),
        Status(0)
    );
    assert_eq!(var(&interp, b"r").as_deref(), Some("zz-no-such-file-*"));

    // `nullglob` is the caller's policy and removes it instead.
    assert_eq!(interp.run_named("-c", b"shopt -s nullglob"), Status(0));
    assert_eq!(
        interp.run_named("-c", b"r=; for x in zz-no-such-file-*; do r=$x; done"),
        Status(0)
    );
    assert_eq!(var(&interp, b"r").as_deref(), Some(""));
}
