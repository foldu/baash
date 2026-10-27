//! Insertion quoting (DESIGN.md §6.9) and the lexer round-trip.

use crate::fixture::{TestShell, parse_words};
use crate::{QuoteCtx, expand_word, expand_words, quote};

use super::{expand_line, expand_one, show};

/// A candidate made of everything the shell treats specially.
const NASTY: &[u8] = b"a b'c\"d$e`f\\g\nh\ti*?[x]~#!;|&()<>{} \xff\x80";

/// Wrap `inserted` the way a completion would appear on the line.
fn line_for(inserted: &[u8], ctx: QuoteCtx) -> Vec<u8> {
    match ctx {
        QuoteCtx::Unquoted => inserted.to_vec(),
        QuoteCtx::DoubleQuoted => {
            let mut line = vec![b'"'];
            line.extend_from_slice(inserted);
            line.push(b'"');
            line
        }
        QuoteCtx::SingleQuoted => {
            let mut line = vec![b'\''];
            line.extend_from_slice(inserted);
            line.push(b'\'');
            line
        }
    }
}

fn assert_round_trip(candidate: &[u8], ctx: QuoteCtx) {
    let inserted = quote(candidate, ctx);
    let line = line_for(&inserted, ctx);
    let words = parse_words(&line);
    assert_eq!(
        words.len(),
        1,
        "ctx={ctx:?} line=<{}> parsed as {} words",
        String::from_utf8_lossy(&line),
        words.len()
    );
    let mut shell = TestShell::new();
    let argv = expand_word(&words[0], &mut shell).expect("re-expansion must succeed");
    assert_eq!(
        argv,
        vec![candidate.to_vec()],
        "ctx={ctx:?} line=<{}> re-expanded to {}",
        String::from_utf8_lossy(&line),
        show(&argv)
    );
}

#[test]
fn bare_words_are_untouched() {
    assert_eq!(quote(b"src/main.rs", QuoteCtx::Unquoted), b"src/main.rs");
}

#[test]
fn spaces_and_globs_are_escaped() {
    assert_eq!(
        quote(b"My File*.txt", QuoteCtx::Unquoted),
        br"My\ File\*.txt"
    );
}

#[test]
fn double_quoted_escapes_only_specials() {
    assert_eq!(quote(b"a b$c", QuoteCtx::DoubleQuoted), br"a b\$c");
}

#[test]
fn single_quoted_closes_and_reopens() {
    assert_eq!(quote(b"a'b", QuoteCtx::SingleQuoted), b"a'\\''b");
    assert_eq!(quote(b"plain path", QuoteCtx::SingleQuoted), b"plain path");
}

#[test]
fn empty_input_stays_empty() {
    assert!(quote(b"", QuoteCtx::Unquoted).is_empty());
    assert!(quote(b"", QuoteCtx::DoubleQuoted).is_empty());
    assert!(quote(b"", QuoteCtx::SingleQuoted).is_empty());
}

#[test]
fn unquoted_newline_is_not_a_line_continuation() {
    let inserted = quote(b"a\nb", QuoteCtx::Unquoted);
    assert_eq!(inserted, b"a$'\\n'b");
    assert!(
        !inserted.windows(2).any(|pair| pair == b"\\\n"),
        "a backslash-newline would be swallowed by line continuation"
    );
}

#[test]
fn every_candidate_round_trips_through_the_lexer() {
    let candidates: &[&[u8]] = &[
        NASTY,
        b"a b",
        b" leading and trailing ",
        b"~tilde",
        b"new\nline",
        b"\xff\x80binary\x01",
        b"'\"$`\\",
        b"-rf",
        b"x=y",
    ];
    for candidate in candidates {
        for ctx in [
            QuoteCtx::Unquoted,
            QuoteCtx::DoubleQuoted,
            QuoteCtx::SingleQuoted,
        ] {
            assert_round_trip(candidate, ctx);
        }
    }
}

/// The full loop the completer depends on: expand a line, re-quote every
/// resulting argv element, expand again, and get the same argv.
#[test]
fn expand_requote_expand_is_stable() {
    let mut shell = TestShell::new()
        .var(b"HOME", b"/home/a b")
        .var(b"x", b"a b'c\"d")
        .positionals(&[b"p q", b"r"]);
    let source = "~/src \"$x\" pre\"$@\"post *.rs $unset";
    let first = expand_line(source, &mut shell);
    assert_eq!(first.len(), 5, "{}", show(&first));

    let mut line = Vec::new();
    for (index, argument) in first.iter().enumerate() {
        if index > 0 {
            line.push(b' ');
        }
        line.extend_from_slice(&quote(argument, QuoteCtx::Unquoted));
    }
    let words = parse_words(&line);
    let second = expand_words(&words, &mut shell).expect("re-expansion");
    assert_eq!(
        second,
        first,
        "requoted line <{}> expanded to {}",
        String::from_utf8_lossy(&line),
        show(&second)
    );
}

#[test]
fn quoted_empty_candidates_stay_one_argument() {
    assert_eq!(
        expand_one("\"\"", &mut TestShell::new()),
        vec![Vec::<u8>::new()]
    );
    assert_eq!(
        expand_one("''", &mut TestShell::new()),
        vec![Vec::<u8>::new()]
    );
}

#[test]
fn argv_round_trips_through_requoting() {
    let args: &[&[u8]] = &[
        b"plain",
        b"two words",
        b"'single'",
        b"\"double\"",
        b"$HOME",
        b"`cmd`",
        b"back\\slash",
        b"new\nline",
        b"*?[glob]",
        b"~user",
        b"\xff",
    ];
    let mut line = Vec::new();
    for (index, arg) in args.iter().enumerate() {
        if index > 0 {
            line.push(b' ');
        }
        line.extend_from_slice(&quote(arg, QuoteCtx::Unquoted));
    }
    let words = parse_words(&line);
    assert_eq!(words.len(), args.len());
    let mut shell = TestShell::new();
    let mut got = Vec::new();
    for word in &words {
        got.extend(expand_word(word, &mut shell).expect("re-expansion must succeed"));
    }
    assert_eq!(
        got,
        crate::tests::values(args),
        "line=<{}> produced {}",
        String::from_utf8_lossy(&line),
        show(&got)
    );
}
