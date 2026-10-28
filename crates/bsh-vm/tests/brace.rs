//! Brace expansion end to end, through the real front end and executor.
//!
//! `bsh-lang`'s unit tests pin the forms; these pin that the stage is actually
//! wired in at the front of the pipeline and honours `set +B`, and that the
//! words it produces go through the rest of the pipeline — parameter
//! expansion, arithmetic side effects, field splitting and globbing — exactly
//! as bash's do.
//!
//! Everything is observed through shell state (`set --` and the positional
//! parameters, or a variable) rather than by redirecting this process's
//! stdout, because these tests run in one process on several threads.

use std::path::PathBuf;

use bsh_lang::Vars;
use bsh_runtime::Status;
use bsh_vm::Interp;

fn shell() -> Interp {
    let mut interp = Interp::new(false);
    interp.set_arg0("bsh");
    interp
}

fn run(interp: &mut Interp, script: &str) {
    assert_eq!(
        interp.run_named("-c", script.as_bytes()),
        Status(0),
        "script: {script}"
    );
}

fn positional(interp: &Interp) -> Vec<String> {
    Vars::positional(interp)
        .iter()
        .map(|word| String::from_utf8_lossy(word).into_owned())
        .collect()
}

/// The words `script` leaves in the positional parameters.
fn words(script: &str) -> Vec<String> {
    let mut interp = shell();
    run(&mut interp, script);
    positional(&interp)
}

fn text(interp: &Interp, name: &[u8]) -> String {
    String::from_utf8_lossy(Vars::get(interp, name).unwrap_or_default()).into_owned()
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bsh-brace-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn one_source_word_becomes_several_arguments() {
    assert_eq!(words("set -- a{b,c}d"), ["abd", "acd"]);
    assert_eq!(
        words("set -- pre{a,{b,c}}post"),
        ["preapost", "prebpost", "precpost"]
    );
    assert_eq!(words("set -- {a,b}{1,2}"), ["a1", "a2", "b1", "b2"]);
    assert_eq!(words("set -- {1..3}"), ["1", "2", "3"]);
    assert_eq!(words("set -- {a..e..2}"), ["a", "c", "e"]);
    // The word is expanded by the front end as one word, so the splitting the
    // stage implies happens before globbing and quote removal.
    assert_eq!(words("set -- a{X,,Y}b"), ["aXb", "ab", "aYb"]);
}

#[test]
fn quoted_and_parameter_brace_text_stays_literal() {
    assert_eq!(words("set -- \"{a,b}\""), ["{a,b}"]);
    assert_eq!(words("set -- '{a,b}'"), ["{a,b}"]);
    assert_eq!(words("x=; set -- ${x:-{a,b}}"), ["{a,b}"]);
    // An assignment value is not a command word: bash leaves it alone.
    let mut interp = shell();
    run(&mut interp, "v={X,Y}; set -- $v");
    assert_eq!(positional(&interp), ["{X,Y}"]);
}

#[test]
fn the_braceexpand_option_switches_the_stage() {
    assert_eq!(words("set +B; set -- a{b,c}d"), ["a{b,c}d"]);
    assert_eq!(words("set +B; set -B; set -- a{b,c}d"), ["abd", "acd"]);
    // The short flag is reported in `$-` only while the stage is on.
    let mut interp = shell();
    run(&mut interp, "set -B");
    assert!(text(&interp, b"-").contains('B'));
    run(&mut interp, "set +B");
    assert!(!text(&interp, b"-").contains('B'));
}

#[test]
fn parameters_expand_after_the_stage_runs() {
    assert_eq!(words("x=A; set -- -{$x,b}-"), ["-A-", "-b-"]);
    assert_eq!(
        words("x=A; set -- {${x},b}_{c,d}"),
        ["A_c", "A_d", "b_c", "b_d"]
    );
    // `$y` is `,b`: expanding it first would create a third alternative.
    assert_eq!(words("y=,b; set -- x{$y,z}"), ["x,b", "xz"]);
}

#[test]
fn each_produced_word_runs_the_later_stages_once() {
    // The arithmetic side effect happens once per alternative, in order:
    // bash expands the brace group first and then expands each word.
    let mut interp = shell();
    run(&mut interp, "i=0; set -- {a,b,c}-$((i++))");
    assert_eq!(positional(&interp), ["a-0", "b-1", "c-2"]);
    assert_eq!(text(&interp, b"i"), "3");

    // `for`'s word list goes through the same pipeline.
    let mut interp = shell();
    run(&mut interp, "out=; for x in {1..3}; do out=$out$x; done");
    assert_eq!(text(&interp, b"out"), "123");
}

#[test]
fn tilde_expansion_sees_the_words_the_stage_produced() {
    let mut interp = shell();
    run(&mut interp, "HOME=/home/bob; set -- {foo~,~}/bar");
    assert_eq!(positional(&interp), ["foo~/bar", "/home/bob/bar"]);
}

#[test]
fn brace_alternatives_feed_pathname_expansion() {
    let dir = temp_dir("glob");
    for name in ["a1.txt", "b1.txt", "c2.md"] {
        std::fs::write(dir.join(name), "").unwrap();
    }
    let prefix = dir.display().to_string();

    // Each alternative becomes its own pattern; the quoted braces form matches
    // nothing, so it is left alone.
    let expanded = words(&format!("set -- {prefix}/{{a,b}}*.txt"));
    assert_eq!(expanded.len(), 2, "{expanded:?}");
    assert!(expanded[0].ends_with("a1.txt"), "{expanded:?}");
    assert!(expanded[1].ends_with("b1.txt"), "{expanded:?}");
    assert_eq!(
        words(&format!("set -- {prefix}/'{{a,b}}'*.txt")),
        [format!("{prefix}/{{a,b}}*.txt")]
    );

    let _ = std::fs::remove_dir_all(&dir);
}
