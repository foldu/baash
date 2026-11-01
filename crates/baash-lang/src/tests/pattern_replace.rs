//! `${x/pat/rep}`, `${x//pat/rep}`, `${x/#pat/rep}`, `${x/%pat/rep}`.
//!
//! Every expected value was checked against bash 5.3.20 under `LC_ALL=C`.

use crate::fixture::TestShell;

use super::{assert_argv, expand_one};

fn one(src: &str, shell: &mut TestShell) -> Vec<Vec<u8>> {
    expand_one(src, shell)
}

#[test]
fn replace_first_all_and_anchors() {
    let mut shell = TestShell::new().var(b"v", b"abcabc");
    assert_argv(&one("\"${v/b/X}\"", &mut shell), &["aXcabc"], "${v/b/X}");
    assert_argv(&one("\"${v//b/X}\"", &mut shell), &["aXcaXc"], "${v//b/X}");
    assert_argv(
        &one("\"${v/#abc/X}\"", &mut shell),
        &["Xabc"],
        "${v/#abc/X}",
    );
    assert_argv(
        &one("\"${v/%abc/X}\"", &mut shell),
        &["abcX"],
        "${v/%abc/X}",
    );
    // An anchored pattern must touch the anchor.
    assert_argv(&one("\"${v/#b/X}\"", &mut shell), &["abcabc"], "prefix");
    assert_argv(&one("\"${v/%b/X}\"", &mut shell), &["abcabc"], "suffix");
    // `${v/a}` is `${v/a/}`: no replacement text.
    let mut shell = TestShell::new().var(b"v", b"aabb");
    assert_argv(&one("\"${v/a}\"", &mut shell), &["abb"], "${v/a}");
    assert_argv(&one("\"${v//a}\"", &mut shell), &["bb"], "${v//a}");
    // Everything after the second `/` is the replacement, slashes included.
    let mut shell = TestShell::new().var(b"v", b"abc");
    assert_argv(&one("\"${v/b/x/y}\"", &mut shell), &["ax/yc"], "slashes");
}

#[test]
fn replace_takes_the_longest_match_at_the_leftmost_position() {
    let mut shell = TestShell::new().var(b"s", b"begin <html></html> end");
    assert_argv(
        &one("\"${s/<*>/[]}\"", &mut shell),
        &["begin [] end"],
        "longest",
    );
    // The anchors take the longest match at their end of the value too.
    let mut shell = TestShell::new().var(b"v", b"abcabc");
    assert_argv(&one("\"${v/#a*c/X}\"", &mut shell), &["X"], "${v/#a*c/X}");
    assert_argv(&one("\"${v/%a*c/X}\"", &mut shell), &["X"], "${v/%a*c/X}");
}

#[test]
fn replace_and_empty_matches() {
    let mut shell = TestShell::new().var(b"v", b"abc");
    // The scanning loop never replaces an empty match: it copies the byte and
    // advances.
    for source in [
        "\"${v/\"\"/X}\"",
        "\"${v//\"\"/X}\"",
        "\"${v/x*/X}\"",
        "\"${v//x*/X}\"",
        "\"${v/x?/X}\"",
    ] {
        assert_argv(&one(source, &mut shell), &["abc"], source);
    }
    // ...but an empty *pattern* is honoured by the anchored forms, which is
    // how `/#/` prepends and `/%/` appends.
    assert_argv(&one("\"${v/#/X}\"", &mut shell), &["Xabc"], "${v/#/X}");
    assert_argv(&one("\"${v/%/X}\"", &mut shell), &["abcX"], "${v/%/X}");
    assert_argv(
        &one("\"${v/#\"\"/X}\"", &mut shell),
        &["Xabc"],
        "${v/#\"\"/X}",
    );
    // An empty match at the anchor is still not a replacement.
    assert_argv(&one("\"${v/#x*/X}\"", &mut shell), &["abc"], "${v/#x*/X}");
    assert_argv(&one("\"${v/%x*/X}\"", &mut shell), &["abc"], "${v/%x*/X}");
    // Whole-value matches.
    assert_argv(&one("\"${v/*/X}\"", &mut shell), &["X"], "${v/*/X}");
    assert_argv(&one("\"${v//*/X}\"", &mut shell), &["X"], "${v//*/X}");
    // An empty value stays empty.
    let mut shell = TestShell::new().var(b"v", b"");
    assert_argv(&one("\"${v//X/}\"", &mut shell), &[""], "empty value");
    assert_argv(&one("\"${v/#/X}\"", &mut shell), &["X"], "prepend to empty");
}

#[test]
fn replace_parses_the_separator_like_bash() {
    // The global form may not split at its first byte: `${x////c}` replaces
    // `/` with `c`.
    let mut shell = TestShell::new().var(b"x", b"/_/");
    assert_argv(&one("\"${x////c}\"", &mut shell), &["c_c"], "${x////c}");
    assert_argv(&one("\"${x///}\"", &mut shell), &["_"], "${x///}");
    // `${x///c}` has no separator, so the pattern is `/c` and nothing matches.
    assert_argv(&one("\"${x///c}\"", &mut shell), &["/_/"], "${x///c}");
    // A quoted `/` is a pattern, not a separator.
    assert_argv(
        &one("\"${x//'/'/c}\"", &mut shell),
        &["c_c"],
        "quoted slash",
    );
    assert_argv(
        &one("\"${x//\\//c}\"", &mut shell),
        &["c_c"],
        "escaped slash",
    );
    // `${x/}` and `${x//}` search for the empty pattern and change nothing.
    assert_argv(&one("\"${x/}\"", &mut shell), &["/_/"], "${x/}");
    assert_argv(&one("\"${x//}\"", &mut shell), &["/_/"], "${x//}");

    // A `#` is an anchor only unquoted, and only in the single-slash form.
    let mut shell = TestShell::new().var(b"v", b"#ab");
    assert_argv(&one("\"${v/#/P}\"", &mut shell), &["P#ab"], "anchor");
    assert_argv(&one("\"${v/'#'/X}\"", &mut shell), &["Xab"], "quoted hash");
    // A backslash-quoted `#` is a literal pattern — which still matches at the
    // anchor, because the value starts with it.
    assert_argv(
        &one("\"${v/#\\#/X}\"", &mut shell),
        &["Xab"],
        "escaped hash",
    );
    assert_argv(&one("\"${v//#/X}\"", &mut shell), &["Xab"], "global hash");
    // A `%` is an anchor only unquoted.
    let mut shell = TestShell::new().var(b"v", b"ab%");
    assert_argv(&one("\"${v/%/S}\"", &mut shell), &["ab%S"], "suffix anchor");
    assert_argv(
        &one("\"${v/'%'/X}\"", &mut shell),
        &["abX"],
        "quoted percent",
    );
}

#[test]
fn replace_applies_to_arrays_element_wise() {
    let mut shell = TestShell::new().indexed(b"a", &[(0, b"xaxa"), (1, b"ya")]);
    assert_argv(
        &one("\"${a[@]//x/y}\"", &mut shell),
        &["yaya", "ya"],
        "quoted ${a[@]//x/y}",
    );
    assert_argv(
        &one("\"${a[*]//x/y}\"", &mut shell),
        &["yaya ya"],
        "quoted ${a[*]//x/y}",
    );

    // The Kubernetes idiom: prepend to every element, including an empty one.
    let mut shell = TestShell::new().indexed(b"a", &[(0, b"aa"), (1, b"bb"), (2, b"")]);
    assert_argv(
        &one("\"${a[@]/#/p-}\"", &mut shell),
        &["p-aa", "p-bb", "p-"],
        "${a[@]/#/p-}",
    );
    assert_argv(
        &one("\"${a[@]/%/-s}\"", &mut shell),
        &["aa-s", "bb-s", "-s"],
        "${a[@]/%/-s}",
    );

    let mut shell = TestShell::new().positionals(&[b"xa", b"ya"]);
    assert_argv(&one("\"${@/a/b}\"", &mut shell), &["xb", "yb"], "${@/a/b}");
}
