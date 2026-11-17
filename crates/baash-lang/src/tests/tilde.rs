//! Tilde expansion borrows a word that has nothing to expand.
//!
//! `expand_one` runs stage 1 for every word of every command, and the owned
//! form copies the `Vec<Part>` and each literal's bytes. Most words hold no
//! `~`, so they are borrowed instead. Two things have to stay true for that to
//! be safe: the borrowing path must agree with the owned one on every word, and
//! a word that does have a tilde must still be expanded to the same bytes.

use std::borrow::Cow;

use crate::fixture::{TestShell, parse_word};
use crate::{
    Mode, Word, expand_assignment_tilde, expand_tilde, expand_tilde_cow, expand_word_with,
};

fn fixture() -> TestShell {
    TestShell::new()
        .home(b"", b"/home/user")
        .home(b"root", b"/root")
        .var(b"x", b"~/sub")
        .var(b"h", b"/home/user/sub")
        .var(b"v", b"a/b/c")
}

fn owned(word: &Word, shell: &mut TestShell, mode: Mode) -> Word {
    match mode {
        Mode::Command => expand_tilde(word, shell).expect("tilde expansion"),
        Mode::Assignment => expand_assignment_tilde(word, shell).expect("tilde expansion"),
    }
}

/// Words that cover the quoting shapes a tilde can hide in, plus a few that
/// only look like they might.
const CORPUS: &[&str] = &[
    "plain",
    "a~",
    "a:b:c",
    "~",
    "~/x",
    "~root",
    "~nobody",
    "x=~",
    "~:~/bin",
    "\"~\"",
    "'~'",
    "a\"~\"b",
    "a~b",
    "${x}",
    "${x:-~}",
    "\"${x:-~}\"",
    "${x#~}",
    "${x/~/y}",
    "${v/a/~/}",
    "${!x}",
    "$((1+2))",
    "$(echo ~)",
    "`echo ~`",
    "~${x}",
    "~/$x",
    "[a]~",
    "*~",
];

#[test]
fn the_borrowing_path_agrees_with_the_owned_expander() {
    // The dangerous direction is a word that is borrowed and *would* have been
    // rewritten: it would leave a `~` unexpanded. Every borrowed word is
    // therefore compared with what the owned expander does to it.
    for src in CORPUS {
        let word = parse_word(src.as_bytes());
        for mode in [Mode::Command, Mode::Assignment] {
            let mut shell = fixture();
            let want = owned(&word, &mut shell, mode);
            let mut shell = fixture();
            match expand_tilde_cow(&word, &mut shell, mode).expect("tilde expansion") {
                Cow::Borrowed(borrowed) => assert_eq!(
                    borrowed, &want,
                    "{src:?} was borrowed in {mode:?} but tilde expansion rewrites it"
                ),
                Cow::Owned(got) => assert_eq!(
                    got, want,
                    "{src:?} in {mode:?}: copied and expanded differently"
                ),
            }
        }
    }
}

#[test]
fn a_word_with_a_tilde_is_expanded_not_borrowed() {
    for (src, mode, expected) in [
        ("~", Mode::Command, "/home/user"),
        ("~/x", Mode::Command, "/home/user/x"),
        ("~root", Mode::Command, "/root"),
        // `PATH=~:~/bin` expands after every unquoted `:`, which a command word
        // does not.
        ("~:~/bin", Mode::Assignment, "/home/user:/home/user/bin"),
        // A replacement word is expanded inside double quotes when the operator
        // is a pattern one, and left alone when it is not.
        // The replacement word of a non-pattern operator keeps the quotes; a
        // pattern operator's does not.
        ("${unset:-~}", Mode::Command, "/home/user"),
        ("\"${unset:-~}\"", Mode::Command, "~"),
        ("\"${h#~}\"", Mode::Command, "/sub"),
    ] {
        let word = parse_word(src.as_bytes());
        let mut shell = fixture();
        let cow = expand_tilde_cow(&word, &mut shell, mode).expect("tilde expansion");
        assert!(
            matches!(cow, Cow::Owned(_)),
            "{src:?} has a tilde but was borrowed ({mode:?})"
        );
        let mut shell = fixture();
        let bytes = expand_word_with(&parse_word(src.as_bytes()), &mut shell, mode)
            .expect("expand")
            .into_iter()
            .next()
            .expect("one word");
        assert_eq!(String::from_utf8_lossy(&bytes), expected, "{src:?}");
    }
}

#[test]
fn a_word_without_a_tilde_is_left_alone() {
    for src in ["plain", "\"~\"", "'~'", "${x}", "$((1+2))"] {
        let word = parse_word(src.as_bytes());
        for mode in [Mode::Command, Mode::Assignment] {
            let mut shell = fixture();
            assert!(
                matches!(
                    expand_tilde_cow(&word, &mut shell, mode).expect("tilde expansion"),
                    Cow::Borrowed(_)
                ),
                "{src:?} has no tilde to expand ({mode:?})"
            );
        }
    }
    // Assignment mode expands after every unquoted `:`, so a `~` that is not
    // at the start of the word is still worth the copy there; a command word
    // only ever looks at the first byte.
    let word = parse_word(b"a~b");
    let mut shell = fixture();
    assert!(matches!(
        expand_tilde_cow(&word, &mut shell, Mode::Command).expect("tilde expansion"),
        Cow::Borrowed(_)
    ));
    let mut shell = fixture();
    assert!(matches!(
        expand_tilde_cow(&word, &mut shell, Mode::Assignment).expect("tilde expansion"),
        Cow::Owned(_)
    ));
}
