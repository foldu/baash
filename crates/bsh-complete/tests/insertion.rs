//! Insertion correctness: quoting, round trips, hostile bytes.
//!
//! These tests insert a completion into a line exactly as the editor would
//! (`Menu::apply`), then re-parse and re-expand the line with `bsh-syntax` and
//! `bsh-lang` and assert the original bytes came back as one argument.

mod common;

use bsh_complete::{Completer, CompletionEngine, QuoteCtx};
use bsh_lang::quote;
use common::{TempTree, expand_single_argument};

fn quoted_line(mut line: Vec<u8>, quoted: &[u8], ctx: QuoteCtx) -> Vec<u8> {
    match ctx {
        QuoteCtx::Unquoted => line.extend_from_slice(quoted),
        QuoteCtx::DoubleQuoted => {
            line.push(b'"');
            line.extend_from_slice(quoted);
            line.push(b'"');
        }
        QuoteCtx::SingleQuoted => {
            line.push(b'\'');
            line.extend_from_slice(quoted);
            line.push(b'\'');
        }
    }
    line
}

fn assert_round_trip(original: &[u8], ctx: QuoteCtx) {
    let quoted = quote(original, ctx);
    let line = quoted_line(b"cat ".to_vec(), &quoted, ctx);
    assert_eq!(
        expand_single_argument(&line),
        original,
        "round trip failed for {original:?} in {ctx:?}: line {:?}",
        String::from_utf8_lossy(&line)
    );
}

const HOSTILE: &[&[u8]] = &[
    b"plain",
    b"with space",
    b"dollar$var",
    b"quo\"te",
    b"single'quote",
    b"back\\slash",
    b"glob*star",
    b"semi;colon",
    b"new\nline",
    b"tab\there",
    b"*",
];

#[test]
fn unquoted_round_trip() {
    for original in HOSTILE {
        assert_round_trip(original, QuoteCtx::Unquoted);
    }
}

#[test]
fn double_quoted_round_trip() {
    for original in HOSTILE {
        assert_round_trip(original, QuoteCtx::DoubleQuoted);
    }
}

#[test]
fn single_quoted_round_trip() {
    for original in HOSTILE {
        assert_round_trip(original, QuoteCtx::SingleQuoted);
    }
}

#[test]
fn invalid_utf8_round_trip() {
    for ctx in [
        QuoteCtx::Unquoted,
        QuoteCtx::DoubleQuoted,
        QuoteCtx::SingleQuoted,
    ] {
        assert_round_trip(b"bad\xffbytes", ctx);
        assert_round_trip(b"\xff\xfe", ctx);
    }
}

#[test]
fn dollar_is_literal_unquoted_and_inside_single_quotes() {
    // Unquoted, `$x` would expand; the quoted form must prevent that.
    assert_round_trip(b"$HOME", QuoteCtx::Unquoted);
    assert_round_trip(b"$HOME", QuoteCtx::SingleQuoted);
    // Inside double quotes the dollar *is* escaped by `quote`.
    assert_round_trip(b"$HOME", QuoteCtx::DoubleQuoted);
}

#[test]
fn newline_unquoted_becomes_ansi_c_quoted() {
    let quoted = quote(b"a\nb", QuoteCtx::Unquoted);
    assert_eq!(quoted, b"a$'\\n'b");
    assert_round_trip(b"a\nb", QuoteCtx::Unquoted);
}

#[test]
fn undo_quoted_newline_is_not_a_line_continuation() {
    // A bare backslash-newline in a shell word disappears; the round trip
    // proves `quote` used `$'\n'` instead.
    assert_round_trip(b"x\n", QuoteCtx::Unquoted);
}

/// Complete a single hostile file through the real engine and apply the menu.
fn complete_hostile_file(name: &[u8]) {
    let tree = TempTree::new("hostile");
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = tree.root.join(std::ffi::OsStr::from_bytes(name));
        std::fs::write(path, b"x").expect("write");
    }
    let engine = CompletionEngine::new().with_root(&tree.root);

    let line = b"cat ";
    let menu = engine.complete(line, line.len());
    assert_eq!(menu.quote, QuoteCtx::Unquoted);
    let index = menu
        .candidates
        .iter()
        .position(|candidate| candidate.insert == name)
        .unwrap_or_else(|| {
            panic!(
                "no candidate for {name:?}; got {:?}",
                menu.candidates
                    .iter()
                    .map(|c| c.insert.clone())
                    .collect::<Vec<_>>()
            )
        });
    let candidate = &menu.candidates[index];
    let mut line = line.to_vec();
    let span = menu.apply(&mut line, index).expect("insertion");
    assert_eq!(
        &line[span.clone()],
        menu.insertion(index).unwrap().as_slice()
    );
    assert_eq!(expand_single_argument(&line), name);
    let _ = candidate;
}

#[cfg(unix)]
#[test]
fn engine_inserts_hostile_filenames_exactly() {
    for name in [
        &b"space name"[..],
        b"dollar$var",
        b"double\"quote",
        b"single'quote",
        b"new\nline",
        b"star*glob",
        b"bad\xffbytes",
    ] {
        complete_hostile_file(name);
    }
}

#[test]
fn directory_completion_keeps_the_slash_and_adds_no_space() {
    let tree = TempTree::new("dirinsert");
    tree.dir("my directory");
    let engine = CompletionEngine::new().with_root(&tree.root);

    let line = b"cat my";
    let menu = engine.complete(line, line.len());
    let index = menu
        .candidates
        .iter()
        .position(|candidate| candidate.insert == b"my directory/")
        .expect("directory candidate");
    let mut line = line.to_vec();
    let span = menu.apply(&mut line, index).expect("insertion");
    assert_eq!(&line[span], b"my\\ directory/");
    assert_eq!(line, b"cat my\\ directory/");
}

#[test]
fn a_unique_file_gets_a_trailing_space() {
    let tree = TempTree::new("space");
    tree.file("unique-name", b"");
    let engine = CompletionEngine::new().with_root(&tree.root);

    let line = b"cat unique";
    let menu = engine.complete(line, line.len());
    assert_eq!(menu.candidates.len(), 1);
    let mut line = line.to_vec();
    menu.apply(&mut line, 0).expect("insertion");
    assert_eq!(line, b"cat unique-name ");
}

#[test]
fn an_unterminated_single_quote_inserts_inside_the_quote() {
    let tree = TempTree::new("openquote");
    tree.file("a file", b"");
    let engine = CompletionEngine::new().with_root(&tree.root);

    let line = b"cat 'a";
    let menu = engine.complete(line, line.len());
    assert_eq!(menu.quote, QuoteCtx::SingleQuoted);
    let index = menu
        .candidates
        .iter()
        .position(|candidate| candidate.insert == b"a file")
        .expect("candidate");
    let mut line = line.to_vec();
    menu.apply(&mut line, index).expect("insertion");
    // The opening quote is preserved and the inserted text is still inside it.
    assert_eq!(line, b"cat 'a file");
    // The inserted text is still inside the open quote: closing it explicitly
    // yields one argument with the right bytes.
    line.push(b'\'');
    assert_eq!(expand_single_argument(&line), b"a file");
}

#[test]
fn an_unterminated_double_quote_inserts_inside_the_quote() {
    let tree = TempTree::new("openquote2");
    tree.file("dollar$name", b"");
    let engine = CompletionEngine::new().with_root(&tree.root);

    let line = b"cat \"dollar";
    let menu = engine.complete(line, line.len());
    assert_eq!(menu.quote, QuoteCtx::DoubleQuoted);
    let index = menu
        .candidates
        .iter()
        .position(|candidate| candidate.insert == b"dollar$name")
        .expect("candidate");
    let mut line = line.to_vec();
    menu.apply(&mut line, index).expect("insertion");
    assert_eq!(line, b"cat \"dollar\\$name");
    line.push(b'"');
    assert_eq!(expand_single_argument(&line), b"dollar$name");
}
