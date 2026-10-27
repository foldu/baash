//! IFS field splitting: POSIX XCU 2.5.3 and 2.6.5.
//!
//! Expected results were checked against bash 5.3.20. The interesting
//! rows are annotated: they are the ones that a naive "split on every IFS
//! character" implementation gets wrong.

use crate::fixture::TestShell;
use crate::{Ifs, Mode, Vars, expand_parameters, split_fields};

use super::{argv, assert_argv, expand_one_bytes, show};

struct Case {
    name: &'static str,
    /// `None` means IFS is unset (the default). `Some("")` means IFS is set to
    /// the empty string, which disables splitting entirely.
    ifs: Option<&'static str>,
    vars: &'static [(&'static str, &'static str)],
    word: &'static str,
    want: &'static [&'static str],
}

const CASES: &[Case] = &[
    // Default IFS: runs of space/tab/newline collapse; leading and trailing
    // runs are ignored.
    Case {
        name: "default IFS trims and collapses",
        ifs: None,
        vars: &[("x", "  a \t b\n c  ")],
        word: "$x",
        want: &["a", "b", "c"],
    },
    Case {
        name: "default IFS: a whitespace-only value produces no field",
        ifs: None,
        vars: &[("x", "   ")],
        word: "$x",
        want: &[],
    },
    Case {
        name: "quoted expansion is never split",
        ifs: Some(":"),
        vars: &[("x", "a:b::c")],
        word: "\"$x\"",
        want: &["a:b::c"],
    },
    // IFS=: (no whitespace at all): spaces are ordinary characters.
    Case {
        name: "IFS=: double delimiter produces an empty field",
        ifs: Some(":"),
        vars: &[("x", "a::b")],
        word: "$x",
        want: &["a", "", "b"],
    },
    Case {
        name: "IFS=: leading delimiter produces an empty field",
        ifs: Some(":"),
        vars: &[("x", ":a:")],
        word: "$x",
        want: &["", "a"],
    },
    Case {
        name: "IFS=: trailing delimiter does not produce a trailing field",
        ifs: Some(":"),
        vars: &[("x", "a:")],
        word: "$x",
        want: &["a"],
    },
    Case {
        name: "IFS=: a lone delimiter is one empty field",
        ifs: Some(":"),
        vars: &[("x", ":")],
        word: "$x",
        want: &[""],
    },
    Case {
        name: "IFS=: two delimiters are two empty fields",
        ifs: Some(":"),
        vars: &[("x", "::")],
        word: "$x",
        want: &["", ""],
    },
    Case {
        name: "IFS=: three delimiters are three empty fields",
        ifs: Some(":"),
        vars: &[("x", ":::")],
        word: "$x",
        want: &["", "", ""],
    },
    Case {
        name: "IFS=: an empty value produces no fields",
        ifs: Some(":"),
        vars: &[("x", "")],
        word: "$x",
        want: &[],
    },
    Case {
        name: "IFS=: spaces are ordinary characters",
        ifs: Some(":"),
        vars: &[("x", "a b:c d")],
        word: "$x",
        want: &["a b", "c d"],
    },
    // IFS with both whitespace and a non-whitespace character.
    Case {
        name: "IFS=' :' ignores whitespace around the delimiter",
        ifs: Some(" :"),
        vars: &[("x", " a : b ")],
        word: "$x",
        want: &["a", "b"],
    },
    Case {
        name: "IFS=' :' keeps empty fields between delimiters",
        ifs: Some(" :"),
        vars: &[("x", "a::b")],
        word: "$x",
        want: &["a", "", "b"],
    },
    Case {
        name: "IFS=' :' a whitespace-and-delimiter value is one empty field",
        ifs: Some(" :"),
        vars: &[("x", " : ")],
        word: "$x",
        want: &[""],
    },
    // POSIX XCU 2.6.5 rule 2: "each occurrence ... along with any adjacent
    // IFS white space, shall delimit a field" -- so ` :: ` is two empty
    // fields, not three, and not one.
    Case {
        name: "IFS=' :' two adjacent delimiters after whitespace",
        ifs: Some(" :"),
        vars: &[("x", " :: ")],
        word: "$x",
        want: &["", ""],
    },
    Case {
        name: "IFS=' :' whitespace around adjacent delimiters collapses",
        ifs: Some(" :"),
        vars: &[("x", "a : : b")],
        word: "$x",
        want: &["a", "", "b"],
    },
    Case {
        name: "IFS=' :' long whitespace runs collapse",
        ifs: Some(" :"),
        vars: &[("x", "a  :  b")],
        word: "$x",
        want: &["a", "b"],
    },
    Case {
        name: "IFS=' :' leading and trailing whitespace runs are ignored",
        ifs: Some(" :"),
        vars: &[("x", "  a  b  ")],
        word: "$x",
        want: &["a", "b"],
    },
    Case {
        name: "IFS=' :' a delimiter followed by a value",
        ifs: Some(" :"),
        vars: &[("x", ":a")],
        word: "$x",
        want: &["", "a"],
    },
    // IFS default when it contains only tab: tab is IFS whitespace, so runs
    // collapse even though the value is not the default triplet.
    Case {
        name: "IFS=tab",
        ifs: Some("\t"),
        vars: &[("x", "a\t\tb\t")],
        word: "$x",
        want: &["a", "b"],
    },
    // IFS set but empty: no splitting at all, not even the default.
    Case {
        name: "IFS='' disables splitting",
        ifs: Some(""),
        vars: &[("x", "a b:c")],
        word: "$x",
        want: &["a b:c"],
    },
    // Literal text is NOT field-split (POSIX XCU 2.6.5 applies to expansion
    // results only), but literal text is still glob-active.
    Case {
        name: "literal colon is not a delimiter",
        ifs: Some(":"),
        vars: &[],
        word: "a:b",
        want: &["a:b"],
    },
    Case {
        name: "literal text between two expansions is not a delimiter",
        ifs: Some(":"),
        vars: &[("x", "a"), ("y", "b")],
        word: "$x:$y",
        want: &["a:b"],
    },
    Case {
        name: "literal colon after an expansion joins the last field",
        ifs: Some(":"),
        vars: &[("x", "a:")],
        word: "$x:",
        want: &["a", ":"],
    },
    Case {
        name: "literal prefix joins the first field",
        ifs: Some(":"),
        vars: &[("x", "a:")],
        word: "b$x",
        want: &["ba"],
    },
    Case {
        name: "literal prefix before a leading delimiter",
        ifs: Some(":"),
        vars: &[("x", ":a")],
        word: "b$x",
        want: &["b", "a"],
    },
    Case {
        name: "a colon from an expansion is a delimiter",
        ifs: Some(":"),
        vars: &[("x", ":")],
        word: "a${x}b",
        want: &["a", "b"],
    },
    // Adjacent expansion results concatenate before splitting (the input to
    // field splitting is the whole word's expansions).
    Case {
        name: "adjacent expansions concatenate",
        ifs: Some(":"),
        vars: &[("x", "a:"), ("y", ":b")],
        word: "$x$y",
        want: &["a", "", "b"],
    },
    // Quoted parts confirm a pending delimiter but are never split.
    Case {
        name: "quoted text after trailing unquoted whitespace starts a field",
        ifs: Some(" "),
        vars: &[("x", "a ")],
        word: "$x\"b\"",
        want: &["a", "b"],
    },
    Case {
        name: "quoted empty after trailing whitespace is an empty field",
        ifs: Some(" "),
        vars: &[("x", "a ")],
        word: "$x\"\"",
        want: &["a", ""],
    },
    Case {
        name: "leading whitespace and a quoted empty is one empty field",
        ifs: Some(" "),
        vars: &[("x", " ")],
        word: "$x\"\"",
        want: &[""],
    },
    Case {
        name: "leading whitespace and quoted text",
        ifs: Some(" "),
        vars: &[("x", " ")],
        word: "$x\"b\"",
        want: &["b"],
    },
    Case {
        name: "quoted colon is not a delimiter",
        ifs: Some(":"),
        vars: &[("x", "a:b")],
        word: "$x\"c:d\"",
        want: &["a", "bc:d"],
    },
    Case {
        name: "quoted empty after a trailing delimiter is an empty field",
        ifs: Some(":"),
        vars: &[("x", "a:")],
        word: "$x\"\"",
        want: &["a", ""],
    },
    Case {
        name: "quoted empty before a leading delimiter",
        ifs: Some(":"),
        vars: &[("x", ":a:")],
        word: "\"\"$x",
        want: &["", "a"],
    },
    Case {
        name: "quoted empty after a trailing delimiter is the last field",
        ifs: Some(":"),
        vars: &[("x", ":a:")],
        word: "$x\"\"",
        want: &["", "a", ""],
    },
    // Parameter-expansion replacement words: their unquoted literal text is
    // part of the expansion result, so it IS split. bash 5.3.20 agrees.
    Case {
        name: "default word literal text is split",
        ifs: None,
        vars: &[],
        word: "${x:-a b}",
        want: &["a", "b"],
    },
    Case {
        name: "default word literal colon is split",
        ifs: Some(":"),
        vars: &[],
        word: "${x:-a:b}",
        want: &["a", "b"],
    },
    Case {
        name: "quoted text in a default word is not split",
        ifs: None,
        vars: &[],
        word: "${x:-a\"b c\"d}",
        want: &["ab cd"],
    },
    Case {
        name: "two default words concatenate before splitting",
        ifs: None,
        vars: &[],
        word: "${x:-a b}${y:-c d}",
        want: &["a", "bc", "d"],
    },
    Case {
        name: "assignment operator substitutes and splits",
        ifs: Some(":"),
        vars: &[],
        word: "${x:=a:b}",
        want: &["a", "b"],
    },
];

#[test]
fn ifs_splitting_table() {
    for case in CASES {
        let mut shell = TestShell::new();
        if let Some(ifs) = case.ifs {
            shell = shell.ifs(ifs.as_bytes());
        }
        for (name, value) in case.vars {
            shell = shell.var(name.as_bytes(), value.as_bytes());
        }
        let got = expand_one_bytes(case.word.as_bytes(), &mut shell);
        assert_eq!(
            got,
            argv(case.want),
            "case `{}` (IFS={:?}, word={:?}): got {}",
            case.name,
            case.ifs,
            case.word,
            show(&got)
        );
    }
}

#[test]
fn ifs_assigned_during_expansion_is_used_for_the_rest_of_the_word() {
    // bash: `unset IFS; set -- ${IFS:=:}a:b` is `"" a:b` -- the `:` that came
    // from the assignment is split, the literal `a:b` is not, and the split
    // uses the newly assigned IFS.
    let mut shell = TestShell::new();
    assert_argv(
        &expand_one_bytes(b"${IFS:=:}a:b", &mut shell),
        &["", "a:b"],
        "assignment then literal",
    );

    // The same value is also used to join a later `$*`, matching bash:
    // `unset IFS; set -- a b; set -- ${IFS:=:}$*` is `"" a b`.
    let mut shell = TestShell::new().positionals(&[b"a", b"b"]);
    assert_argv(
        &expand_one_bytes(b"${IFS:=:}$*", &mut shell),
        &["", "a", "b"],
        "star join uses the new IFS",
    );
    assert_eq!(shell.get(b"IFS"), Some(&b":"[..]));
}

#[test]
fn assignment_operator_also_assigns_the_unsplit_value() {
    let mut shell = TestShell::new().ifs(b":");
    let got = expand_one_bytes(b"${x:=a:b}", &mut shell);
    assert_eq!(got, argv(&["a", "b"]));
    assert_eq!(shell.get(b"x"), Some(&b"a:b"[..]));
}

/// The stages are separately callable, which is what `bsh explain` needs.
#[test]
fn stages_are_inspectable() {
    let mut shell = TestShell::new().ifs(b":");
    shell = shell.var(b"x", b"c:d");
    let word = crate::fixture::parse_word(b"a$x\"b\"");
    let expanded = expand_parameters(&word, &mut shell, Mode::Command).expect("stage 2");
    let ifs = Ifs::from_vars(&shell);
    assert_eq!(expanded.chunks.len(), 3, "three chunks: a, $x, \"b\"");
    assert!(!expanded.chunks[0].splittable, "literal text is not split");
    assert!(expanded.chunks[1].splittable, "parameter values are split");
    assert_eq!(expanded.chunks[1].bytes, b"c:d".to_vec());
    assert!(expanded.chunks[2].quoted, "the \"b\" group is quoted");
    let fields = split_fields(&expanded, &ifs);
    let bytes: Vec<Vec<u8>> = fields.iter().map(|field| field.bytes.clone()).collect();
    assert_eq!(bytes, argv(&["ac", "db"]));
}
