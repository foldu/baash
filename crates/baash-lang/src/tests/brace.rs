//! Brace expansion: the stage that runs before every other expansion.
//!
//! The expectations were measured against bash 5.3.20 under `LC_ALL=C`, except
//! where a comment records a deliberate deviation. The stage is a text
//! transformation over a word's *parts*, so the tests also pin that it keeps
//! the parts it did not touch: `x{a,$y}b` must leave `$y` unexpanded for the
//! parameter stage, and `{a,b}''` must keep the empty quoted suffix that makes
//! the empty alternatives visible as fields.

use crate::fixture::{TestShell, parse_word};
use crate::{Part, expand_assignment, expand_braces};

use super::{assert_argv, expand_one};

#[test]
fn comma_alternatives_are_a_cartesian_product() {
    let mut shell = TestShell::new();
    assert_argv(
        &expand_one("a{b,c}d", &mut shell),
        &["abd", "acd"],
        "a{b,c}d",
    );
    assert_argv(&expand_one("{foo,bar}", &mut shell), &["foo", "bar"], "bar");
    assert_argv(
        &expand_one("{a,b}{1,2}", &mut shell),
        &["a1", "a2", "b1", "b2"],
        "the leftmost group varies slowest",
    );
    // No comma is not a brace expansion; the second group still is.
    assert_argv(
        &expand_one("{foo}{a,b}", &mut shell),
        &["{foo}a", "{foo}b"],
        "an unusable group does not stop the scan",
    );
    // Two empty alternatives: unquoted empty words vanish, but the word is
    // still produced twice.
    assert_argv(&expand_one("x{,}y", &mut shell), &["xy", "xy"], "x{,}y");
}

#[test]
fn groups_nest_to_any_depth() {
    let mut shell = TestShell::new();
    assert_argv(
        &expand_one("pre{a,{b,c}}post", &mut shell),
        &["preapost", "prebpost", "precpost"],
        "one level",
    );
    assert_argv(
        &expand_one("-{A,={a,.{x,y}.,b}=,B}-", &mut shell),
        &["-A-", "-=a=-", "-=.x.=-", "-=.y.=-", "-=b=-", "-B-"],
        "three levels",
    );
    assert_argv(
        &expand_one("-{A,={a,b}{c,d}=,B}-", &mut shell),
        &["-A-", "-=ac=-", "-=ad=-", "-=bc=-", "-=bd=-", "-B-"],
        "a product inside an alternative",
    );
    // An unusable outer group is skipped, not removed: bash expands the inner
    // group and keeps the unmatched brace as literal text.
    assert_argv(
        &expand_one("{{a,b}}", &mut shell),
        &["{a}", "{b}"],
        "inner group wins",
    );
    // An unbalanced `{` stops the scan: from that byte on the word is literal,
    // so the inner group is not expanded through it.
    assert_argv(
        &expand_one("{a,{b,c}", &mut shell),
        &["{a,{b,c}"],
        "unbalanced outer group",
    );
    assert_argv(
        &expand_one("{{a,b}", &mut shell),
        &["{{a,b}"],
        "leading brace",
    );
    // A group that is complete *before* the unbalanced brace still expands.
    assert_argv(
        &expand_one("{a,b}_{", &mut shell),
        &["a_{", "b_{"],
        "trailing brace",
    );
    assert_argv(
        &expand_one("{a,b}{", &mut shell),
        &["a{", "b{"],
        "trailing open",
    );
}

#[test]
fn empty_alternatives_and_elision() {
    let mut shell = TestShell::new();
    assert_argv(
        &expand_one("a{X,,Y}b", &mut shell),
        &["aXb", "ab", "aYb"],
        "an empty alternative is an empty word",
    );
    assert_argv(
        &expand_one("{X,Y,}", &mut shell),
        &["X", "Y"],
        "trailing empty",
    );
    // A quoted empty suffix keeps every alternative a real field.
    assert_argv(
        &expand_one("{X,,Y,}''", &mut shell),
        &["X", "", "Y", ""],
        "empty fields survive with a quoted suffix",
    );
    assert_argv(
        &expand_one("{X,,Y,}\"\"", &mut shell),
        &["X", "", "Y", ""],
        "the double-quoted spelling",
    );
}

#[test]
fn quoting_decides_what_is_structural() {
    let mut shell = TestShell::new();
    // Quoted braces are literal, whichever quoting produced them.
    assert_argv(&expand_one("\"{a,b}\"", &mut shell), &["{a,b}"], "double");
    assert_argv(&expand_one("'{a,b}'", &mut shell), &["{a,b}"], "single");
    assert_argv(&expand_one("\\{a,b\\}", &mut shell), &["{a,b}"], "escaped");
    // The escaped `{` is literal, the group after it is not.
    assert_argv(
        &expand_one("\\{{a,b}", &mut shell),
        &["{a", "{b"],
        "escaped open",
    );
    // A quoted comma is text, not a separator.
    assert_argv(
        &expand_one("{a,'b,c'}", &mut shell),
        &["a", "b,c"],
        "quoted comma",
    );
    // Alternatives keep their own quoting: `\X` and `"b"` stay quoted bytes.
    assert_argv(
        &expand_one("-{\\X\"b\",'cd'}-", &mut shell),
        &["-Xb-", "-cd-"],
        "mixed quoting",
    );
    assert_argv(
        &expand_one("{'a',b}_{c,\"d\"}", &mut shell),
        &["a_c", "a_d", "b_c", "b_d"],
        "quoting per alternative",
    );
}

#[test]
fn non_expansions_stay_exactly_as_written() {
    let mut shell = TestShell::new();
    for source in [
        "{foo}", "x{}y", "{a}", "{1.3}", "{1...3}", "{1__3}", "{1..a}", "{z..3}", "{a,b", "a,b}",
        "{,}x", // a group, actually: both alternatives empty, leaving `x` twice
    ] {
        let got = expand_one(source, &mut shell);
        if source == "{,}x" {
            assert_argv(
                &got,
                &["x", "x"],
                "an empty-alternative group still expands",
            );
        } else {
            assert_argv(&got, &[source], source);
        }
    }
}

#[test]
fn invalid_ranges_are_not_ranges() {
    let mut shell = TestShell::new();
    // Overflow and non-decimal spellings are not endpoints.
    for source in [
        "{9223372036854775807..9223372036854775809}",
        "{0x1..0x3}",
        "{10#1..10#3}",
        "{1..3..}",
        "{..3}",
        "{1..3..2..4}",
        "{!..$}",
    ] {
        assert_argv(&expand_one(source, &mut shell), &[source], source);
    }
    // Solely-quoted endpoints disqualify the range (bash leaves both alone).
    assert_argv(
        &expand_one("{1..\"3\"}", &mut shell),
        &["{1..3}"],
        "quoted endpoint",
    );
}

#[test]
fn numeric_ranges_walk_in_the_endpoints_direction() {
    let mut shell = TestShell::new();
    let cases: &[(&str, &[&str])] = &[
        ("{1..3}", &["1", "2", "3"]),
        ("{3..1}", &["3", "2", "1"]),
        ("{1..10..3}", &["1", "4", "7", "10"]),
        ("{1..8..3}", &["1", "4", "7"]),
        // A step whose sign disagrees with the direction keeps its magnitude;
        // this is what bash 5.3.20 does (`{1..8..-3}` is `1 4 7`).
        ("{8..1..-3}", &["8", "5", "2"]),
        ("{-3..3..2}", &["-3", "-1", "1", "3"]),
        ("{-9..-11..2}", &["-9", "-11"]),
        // A zero step counts as one, and a step past the end yields `start`.
        ("{1..4..0}", &["1", "2", "3", "4"]),
        ("{11..9..3}", &["11"]),
        // Singletons are ranges of one, whatever the step says.
        ("{1..1}", &["1"]),
        ("{-9..-9..-3}", &["-9"]),
    ];
    for (source, want) in cases {
        assert_argv(&expand_one(source, &mut shell), want, source);
    }
}

#[test]
fn numeric_ranges_keep_bashs_zero_padding() {
    let mut shell = TestShell::new();
    let cases: &[(&str, &[&str])] = &[
        ("{01..03}", &["01", "02", "03"]),
        ("{01..3}", &["01", "02", "03"]),
        ("{001..3}", &["001", "002", "003"]),
        ("{1..003}", &["001", "002", "003"]),
        ("{09..12}", &["09", "10", "11", "12"]),
        ("{12..07}", &["12", "11", "10", "09", "08", "07"]),
        // Width includes the sign, and `0` is padded like any other value.
        ("{-01..3}", &["-01", "000", "001", "002", "003"]),
        ("{01..-3}", &["01", "00", "-1", "-2", "-3"]),
        // A leading `+` suppresses padding, exactly as bash does.
        ("{+01..3}", &["1", "2", "3"]),
        ("{-0..3}", &["0", "1", "2", "3"]),
        ("{-00..3}", &["000", "001", "002", "003"]),
    ];
    for (source, want) in cases {
        assert_argv(&expand_one(source, &mut shell), want, source);
    }
}

#[test]
fn character_ranges_step_over_their_letters() {
    let mut shell = TestShell::new();
    let cases: &[(&str, &[&str])] = &[
        ("{a..c}", &["a", "b", "c"]),
        ("{a..e..2}", &["a", "c", "e"]),
        ("{a..e..-2}", &["a", "c", "e"]),
        ("{e..a..-2}", &["e", "c", "a"]),
        ("{c..a..2}", &["c", "a"]),
        ("{A..C..2}", &["A", "C"]),
        ("{a..a}", &["a"]),
        ("{a..a..2}", &["a"]),
    ];
    for (source, want) in cases {
        assert_argv(&expand_one(source, &mut shell), want, source);
    }
}

#[test]
fn parameters_are_preserved_and_expanded_afterwards() {
    let mut shell = TestShell::new().var(b"a", b"A").var(b"y", b",b");
    assert_argv(&expand_one("-{$a,b}-", &mut shell), &["-A-", "-b-"], "$a");
    assert_argv(
        &expand_one("{${a},b}_{c,d}", &mut shell),
        &["A_c", "A_d", "b_c", "b_d"],
        "${a}",
    );
    assert_argv(
        &expand_one("{_$a,b}_{c,d}", &mut shell),
        &["_A_c", "_A_d", "b_c", "b_d"],
        "literal and variable",
    );
    // `$y` is `,b`: expanding the parameter *before* the brace stage would
    // create a comma-separated group, so this pins the order.
    assert_argv(&expand_one("x{$y,z}", &mut shell), &["x,b", "xz"], "order");

    // The `$y` part is still a parameter expansion in the produced word.
    let word = parse_word(b"x{a,$y}b");
    let words = expand_braces(&word);
    assert_eq!(words.len(), 2, "one word per alternative");
    assert!(
        !words[0]
            .parts
            .iter()
            .any(|part| matches!(part, Part::Param(_))),
        "the first alternative does not contain $y"
    );
    assert!(
        words[1]
            .parts
            .iter()
            .any(|part| matches!(part, Part::Param(_))),
        "the second alternative keeps $y as a part"
    );
}

#[test]
fn a_parameter_replacement_word_is_its_own_expansion_unit() {
    // Brace expansion runs on the outer word before the parameter is expanded,
    // so the replacement word is never brace-expanded.
    let mut shell = TestShell::new().var(b"x", b"");
    assert_argv(&expand_one("${x:-{a,b}}", &mut shell), &["{a,b}"], "null x");
    let mut shell = TestShell::new().var(b"x", b"v");
    assert_argv(&expand_one("${x:-{a,b}}", &mut shell), &["v"], "set x");
    let mut shell = TestShell::new();
    assert_argv(&expand_one("${x-{a,b}}", &mut shell), &["{a,b}"], "unset x");
}

#[test]
fn assignment_values_are_never_brace_expanded() {
    let mut shell = TestShell::new();
    let word = parse_word(b"{X,Y}");
    assert_eq!(
        expand_assignment(&word, &mut shell).expect("assignment"),
        b"{X,Y}",
        "bash leaves `v={{X,Y}}` literal"
    );
}

#[test]
fn the_braceexpand_option_turns_the_stage_off() {
    let mut shell = TestShell::new().without_brace_expansion();
    assert_argv(&expand_one("a{b,c}d", &mut shell), &["a{b,c}d"], "set +B");
    assert_argv(&expand_one("{1..3}", &mut shell), &["{1..3}"], "ranges too");
    assert_argv(
        &expand_one("{a,b}{1,2}", &mut shell),
        &["{a,b}{1,2}"],
        "nothing at all",
    );
}

#[test]
fn tilde_expansion_sees_the_expanded_word() {
    let mut shell = TestShell::new().var(b"HOME", b"/home/bob");
    // The brace stage runs first, so the second word starts with `~` and the
    // first does not.
    assert_argv(
        &expand_one("{foo~,~}/bar", &mut shell),
        &["foo~/bar", "/home/bob/bar"],
        "{foo~,~}/bar",
    );
    let mut shell = TestShell::new()
        .var(b"HOME", b"/home/bob")
        .home(b"root", b"/root");
    assert_argv(
        &expand_one("~{/src,root}", &mut shell),
        &["/home/bob/src", "/root"],
        "a tilde prefix split by the brace stage",
    );
}

#[test]
fn brace_alternatives_are_glob_active() {
    let mut shell = TestShell::new().glob(b"a*", &[b"ab"]);
    assert_argv(&expand_one("{a,b}*", &mut shell), &["ab", "b*"], "{a,b}*");
    assert_eq!(shell.glob_calls, vec![b"a*".to_vec(), b"b*".to_vec()]);
}
