//! `${x#pat}`, `${x##pat}`, `${x%pat}`, `${x%%pat}`.
//!
//! Every expected value was checked against bash 5.3.20 under `LC_ALL=C`,
//! which is the unit policy the whole crate uses: `?` matches one byte.

use crate::ParamOp;
use crate::fixture::TestShell;

use super::{assert_argv, expand_one, expand_one_bytes, values};

fn one(src: &str, shell: &mut TestShell) -> Vec<Vec<u8>> {
    expand_one(src, shell)
}

#[test]
fn strip_prefix_shortest_and_longest() {
    let mut shell = TestShell::new().var(b"v", b"aabbccdd");
    assert_argv(&one("\"${v#*b}\"", &mut shell), &["bccdd"], "${v#*b}");
    assert_argv(&one("\"${v##*b}\"", &mut shell), &["ccdd"], "${v##*b}");

    let mut shell = TestShell::new().var(b"v", b"abcd");
    assert_argv(&one("\"${v#a}\"", &mut shell), &["bcd"], "${v#a}");
    assert_argv(&one("\"${v#ab}\"", &mut shell), &["cd"], "${v#ab}");
    assert_argv(&one("\"${v##ab}\"", &mut shell), &["cd"], "${v##ab}");
    // No match leaves the value alone.
    assert_argv(&one("\"${v#xyz}\"", &mut shell), &["abcd"], "${v#xyz}");
    assert_argv(&one("\"${v##xyz}\"", &mut shell), &["abcd"], "${v##xyz}");
    // A match of the whole value.
    assert_argv(&one("\"${v##abcd}\"", &mut shell), &[""], "whole prefix");
    assert_argv(&one("\"${v%%abcd}\"", &mut shell), &[""], "whole suffix");
    // `*` always matches, so the shortest prefix is empty and the longest is
    // everything.
    assert_argv(&one("\"${v#*}\"", &mut shell), &["abcd"], "${v#*}");
    assert_argv(&one("\"${v##*}\"", &mut shell), &[""], "${v##*}");
    // `${x#?}` is "drop one byte"; `${x##?}` is the same, because `?` cannot
    // match more than one.
    assert_argv(&one("\"${v#?}\"", &mut shell), &["bcd"], "${v#?}");
    assert_argv(&one("\"${v##?}\"", &mut shell), &["bcd"], "${v##?}");
    // An invalid UTF-8 byte is one byte for `?`, like `LC_ALL=C` bash.
    let mut shell = TestShell::new().var(b"v", b"\xffab");
    assert_eq!(
        expand_one_bytes(b"\"${v#?}\"", &mut shell),
        values(&[b"ab"])
    );
}

#[test]
fn strip_suffix_shortest_and_longest() {
    let mut shell = TestShell::new().var(b"v", b"aabbccdd");
    assert_argv(&one("\"${v%c*}\"", &mut shell), &["aabbc"], "${v%c*}");
    assert_argv(&one("\"${v%%c*}\"", &mut shell), &["aabb"], "${v%%c*}");

    let mut shell = TestShell::new().var(b"v", b"abc");
    // `?` matches exactly one byte, so `%` and `%%` agree on it.
    assert_argv(&one("\"${v%?}\"", &mut shell), &["ab"], "${v%?}");
    assert_argv(&one("\"${v%%?}\"", &mut shell), &["ab"], "${v%%?}");
    // `*` matches the empty suffix (shortest) and the whole value (longest).
    assert_argv(&one("\"${v%*}\"", &mut shell), &["abc"], "${v%*}");
    assert_argv(&one("\"${v%%*}\"", &mut shell), &[""], "${v%%*}");
    // The suffix operators scan from the end: the *last* `b` is matched.
    let mut shell = TestShell::new().var(b"v", b"abcb");
    assert_argv(&one("\"${v%b}\"", &mut shell), &["abc"], "${v%b}");
    assert_argv(&one("\"${v%%b}\"", &mut shell), &["abc"], "${v%%b}");
    // Only one suffix can match here, so `%` and `%%` agree again.
    assert_argv(&one("\"${v%c*b}\"", &mut shell), &["ab"], "${v%c*b}");
    assert_argv(&one("\"${v%%c*b}\"", &mut shell), &["ab"], "${v%%c*b}");
    // Shortest versus longest with a real choice: `.*` is `.z` or `.y.z`.
    let mut shell = TestShell::new().var(b"v", b"x.y.z");
    assert_argv(&one("\"${v%.*}\"", &mut shell), &["x.y"], "${v%.*}");
    assert_argv(&one("\"${v%%.*}\"", &mut shell), &["x"], "${v%%.*}");
}

#[test]
fn strip_with_an_empty_pattern_does_nothing() {
    // bash: `${x##}` is `${x##""}` — nothing matched, so nothing is stripped.
    // The empty pattern matches every position, so a naive "longest" rule
    // would delete the whole value.
    let mut shell = TestShell::new().var(b"v", b"abc");
    for source in [
        "\"${v#}\"",
        "\"${v##}\"",
        "\"${v%}\"",
        "\"${v%%}\"",
        "\"${v#\"\"}\"",
        "\"${v%%\"\"}\"",
    ] {
        assert_argv(&one(source, &mut shell), &["abc"], source);
    }
    // The same when the emptiness comes from an expansion.
    let mut shell = TestShell::new().var(b"v", b"abc").var(b"e", b"");
    assert_argv(&one("\"${v##$e}\"", &mut shell), &["abc"], "${v##$e}");
}

#[test]
fn strip_honours_quoting_inside_the_pattern() {
    // A quoted `*` is a literal `*`, whether it was escaped, single-quoted or
    // double-quoted.
    let mut shell = TestShell::new().var(b"v", b"*abc");
    assert_argv(&one("\"${v#\\*}\"", &mut shell), &["abc"], "escaped `*`");
    assert_argv(&one("\"${v#\"*\"}\"", &mut shell), &["abc"], "quoted `*`");
    assert_argv(
        &one("\"${v#'*'}\"", &mut shell),
        &["abc"],
        "single-quoted `*`",
    );
    assert_argv(&one("\"${v##*}\"", &mut shell), &[""], "unquoted `*`");

    // A bracket expression that comes from a *quoted* expansion is text.
    let mut shell = TestShell::new().var(b"v", b"[a]bc").var(b"p", b"[a]");
    assert_argv(
        &one("\"${v#\"$p\"}\"", &mut shell),
        &["bc"],
        "quoted expansion",
    );
    // `[a]` is a bracket expression, so it matches a single `a` — not the
    // three bytes `[a]` that the value starts with.
    assert_argv(
        &one("\"${v##$p}\"", &mut shell),
        &["[a]bc"],
        "unquoted class",
    );

    // The enclosing double quotes do not quote the pattern: bash expands
    // `"${v##$p}"` exactly like the unquoted form. Only quoting written inside
    // the pattern counts.
    let mut shell = TestShell::new().var(b"v", b"abc").var(b"p", b"a*");
    assert_argv(
        &one("\"${v##$p}\"", &mut shell),
        &[""],
        "pattern from a variable",
    );
    assert_argv(
        &one("\"${v##\"$p\"}\"", &mut shell),
        &["abc"],
        "quoted pattern from a variable",
    );

    // A quoted backslash is a literal backslash, not an escape for the byte
    // after it.
    let mut shell = TestShell::new().var(b"v", b"\\*");
    assert_argv(
        &one("\"${v#\\\\}\"", &mut shell),
        &["*"],
        "escaped backslash",
    );
    assert_argv(&one("\"${v#\\*}\"", &mut shell), &["\\*"], "escaped star");
}

#[test]
fn strip_applies_to_arrays_element_wise() {
    let mut shell = TestShell::new().indexed(b"a", &[(0, b"xaxa"), (1, b"ya")]);
    // Quoted `[@]` keeps one field per element.
    assert_argv(
        &one("\"${a[@]#x}\"", &mut shell),
        &["axa", "ya"],
        "quoted ${a[@]#x}",
    );
    // Quoted `[*]` joins after the per-element strip.
    assert_argv(
        &one("\"${a[*]#x}\"", &mut shell),
        &["axa ya"],
        "quoted ${a[*]#x}",
    );
    // Unquoted, the list is an expansion result and splits.
    assert_argv(&one("${a[@]#x}", &mut shell), &["axa", "ya"], "unquoted");

    let mut shell = TestShell::new().positionals(&[b"1a", b"2a", b"3a"]);
    assert_argv(&one("\"${@%a}\"", &mut shell), &["1", "2", "3"], "${@%a}");
    assert_argv(&one("\"${*%a}\"", &mut shell), &["1 2 3"], "${*%a}");
    assert_argv(
        &one("${@%a}", &mut shell),
        &["1", "2", "3"],
        "unquoted ${@%a}",
    );

    // A single element is a scalar, not a list.
    let mut shell = TestShell::new().indexed(b"a", &[(0, b"xa"), (1, b"xya")]);
    assert_argv(&one("\"${a[1]#x}\"", &mut shell), &["ya"], "${a[1]#x}");
    // An empty array produces no fields.
    let mut shell = TestShell::new().indexed(b"a", &[]);
    assert_argv(&one("\"${a[@]#x}\"", &mut shell), &[], "empty array");
}

#[test]
fn the_operator_shapes_are_public() {
    // The parser's output is the expansion's input; this pins the shapes the
    // front end produces for the forms that need a word.
    let mut shell = TestShell::new().var(b"x", b"abc");
    let word = crate::fixture::parse_word(b"${x##p}");
    let expected = ParamOp::RemovePrefix {
        pattern: crate::Word::lit(b"p").with_span(0..1),
        longest: true,
    };
    let crate::Part::Param(exp) = &word.parts[0] else {
        panic!("expected a parameter");
    };
    assert_eq!(exp.op, expected);
    let _ = expand_one("\"${x##p}\"", &mut shell);
    // `${@^^}` used to be refused; the shell-level test in `crates/bsh`
    // covers its element-wise expansion.
}
