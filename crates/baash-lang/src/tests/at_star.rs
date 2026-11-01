//! `$@` and `$*`, quoted and unquoted, including `"$@"` vanishing when there
//! are no positional parameters.
//!
//! Expected results were checked against bash 5.3.20.

use crate::fixture::TestShell;

use super::{argv, assert_argv, expand_one_bytes, show};

fn one(word: &str, shell: &mut TestShell) -> Vec<Vec<u8>> {
    expand_one_bytes(word.as_bytes(), shell)
}

#[test]
fn quoted_at_with_no_parameters_vanishes_entirely() {
    let mut shell = TestShell::new();
    assert_argv(&one("\"$@\"", &mut shell), &[], "\"$@\"");
    // ... unlike "", which is one empty field.
    assert_argv(&one("\"\"", &mut shell), &[""], "\"\"");
    assert_argv(&one("x\"$@\"y", &mut shell), &["xy"], "x\"$@\"y");
}

#[test]
fn unquoted_at_with_no_parameters_vanishes() {
    let mut shell = TestShell::new();
    assert_argv(&one("$@", &mut shell), &[], "$@");
    assert_argv(&one("$*", &mut shell), &[], "$*");
    assert_argv(&one("x$@y", &mut shell), &["xy"], "x$@y");
}

#[test]
fn quoted_star_with_no_parameters_is_one_empty_field() {
    let mut shell = TestShell::new();
    assert_argv(&one("\"$*\"", &mut shell), &[""], "\"$*\"");
}

#[test]
fn quoted_at_glues_prefix_to_first_parameter_and_suffix_to_last() {
    let mut shell = TestShell::new();
    assert_argv(&one("x\"$@\"y", &mut shell), &["xy"], "no params");
    shell = shell.positionals(&[b"a"]);
    assert_argv(&one("x\"$@\"y", &mut shell), &["xay"], "one param");
    shell = shell.positionals(&[b"a", b"b"]);
    assert_argv(&one("x\"$@\"y", &mut shell), &["xa", "by"], "two params");
    shell = shell.positionals(&[b"a", b"b", b"c"]);
    assert_argv(
        &one("x\"$@\"y", &mut shell),
        &["xa", "b", "cy"],
        "three params",
    );
}

#[test]
fn quoted_at_does_not_split_parameters() {
    let mut shell = TestShell::new().positionals(&[b"a b", b"c"]);
    assert_argv(&one("\"$@\"", &mut shell), &["a b", "c"], "\"$@\"");
    // Fields can still be empty.
    shell = shell.positionals(&[b"", b"x"]);
    assert_argv(&one("\"$@\"", &mut shell), &["", "x"], "empty parameter");
}

#[test]
fn unquoted_at_and_star_are_identical_outside_quotes() {
    for word in ["$@", "$*"] {
        let mut shell = TestShell::new().positionals(&[b"a b", b"c"]);
        assert_argv(&one(word, &mut shell), &["a", "b", "c"], word);
    }
}

#[test]
fn star_joins_with_the_first_ifs_character() {
    let mut shell = TestShell::new().ifs(b":").positionals(&[b"a", b"b", b"c"]);
    assert_argv(&one("\"$*\"", &mut shell), &["a:b:c"], "quoted $*");
    assert_argv(&one("$*", &mut shell), &["a", "b", "c"], "unquoted $*");
    assert_argv(&one("\"$@\"", &mut shell), &["a", "b", "c"], "quoted $@");
    assert_argv(&one("$@", &mut shell), &["a", "b", "c"], "unquoted $@");
}

#[test]
fn star_with_default_ifs_joins_with_a_space() {
    let mut shell = TestShell::new().positionals(&[b"a", b"b", b"c"]);
    assert_argv(&one("\"$*\"", &mut shell), &["a b c"], "quoted $*");
    assert_argv(&one("$*", &mut shell), &["a", "b", "c"], "unquoted $*");
}

#[test]
fn star_with_empty_ifs_concatenates_when_quoted() {
    let mut shell = TestShell::new().ifs(b"").positionals(&[b"a", b"b"]);
    assert_argv(&one("\"$*\"", &mut shell), &["ab"], "quoted $*");
    // Unquoted, an empty IFS cannot join: each parameter stays its own field
    // (verified against bash).
    assert_argv(&one("$*", &mut shell), &["a", "b"], "unquoted $*");
    assert_argv(&one("$@", &mut shell), &["a", "b"], "unquoted $@");
    assert_argv(&one("1$@2", &mut shell), &["1a", "b2"], "1$@2");
    assert_argv(&one("1$*2", &mut shell), &["1a", "b2"], "1$*2");
}

#[test]
fn unquoted_at_joins_with_ifs_then_splits() {
    // bash joins unquoted $@ with the first IFS character in a command word,
    // which is why an empty middle parameter survives with IFS=: ...
    let mut shell = TestShell::new().ifs(b":").positionals(&[b"a", b"", b"b"]);
    assert_argv(&one("$@", &mut shell), &["a", "", "b"], "IFS=:");
    assert_argv(&one("1$@2", &mut shell), &["1a", "", "b2"], "1$@2 IFS=:");
    // ... but not with the default whitespace IFS, where runs collapse.
    shell = TestShell::new().ifs(b" ").positionals(&[b"a", b"", b"b"]);
    assert_argv(&one("$@", &mut shell), &["a", "b"], "IFS=space");
    shell = TestShell::new().positionals(&[b"a", b"", b"b"]);
    assert_argv(&one("$@", &mut shell), &["a", "b"], "IFS unset");
}

#[test]
fn unquoted_at_splits_each_parameter_with_ifs() {
    let mut shell = TestShell::new().ifs(b":").positionals(&[b"a:b", b"c"]);
    assert_argv(&one("$@", &mut shell), &["a", "b", "c"], "$@");
    assert_argv(&one("$*", &mut shell), &["a", "b", "c"], "$*");
}

#[test]
fn empty_unquoted_at_with_empty_ifs_produces_nothing() {
    let mut shell = TestShell::new().ifs(b"").positionals(&[b"", b""]);
    assert_argv(&one("$@", &mut shell), &[], "$@");
    // Two empty parameters still make two field boundaries, so the prefix and
    // suffix land in different fields (verified against bash).
    assert_argv(&one("x$@y", &mut shell), &["x", "y"], "x$@y");
    // A single empty parameter is one (empty) field, which then vanishes;
    // with surrounding text there is no boundary to split at.
    shell = TestShell::new().ifs(b"").positionals(&[b""]);
    assert_argv(&one("$@", &mut shell), &[], "single empty parameter");
    assert_argv(&one("x$@y", &mut shell), &["xy"], "single empty parameter");
}

#[test]
fn quoted_empty_parameters_are_kept() {
    let mut shell = TestShell::new().positionals(&[b"", b"", b"x"]);
    assert_argv(&one("\"$@\"", &mut shell), &["", "", "x"], "\"$@\"");
    shell = TestShell::new().ifs(b"").positionals(&[b"", b""]);
    assert_argv(&one("\"$@\"", &mut shell), &["", ""], "\"$@\" IFS=''");
}

#[test]
fn adjacent_quoted_at_lists_concatenate_at_the_seam() {
    let mut shell = TestShell::new().ifs(b":").positionals(&[b"a", b"b"]);
    assert_argv(&one("\"$@\"$@", &mut shell), &["a", "ba", "b"], "\"$@\"$@");
    assert_argv(
        &one("\"$@\"x\"$@\"", &mut shell),
        &["a", "bxa", "b"],
        "\"$@\"x\"$@\"",
    );
}

/// DESIGN.md §3.3 Tier C: POSIX would not split the explicitly quoted `"$@"`
/// inside an unquoted `${x:-"$@"}`, but bash does, and we match bash.
#[test]
fn tier_c_quoted_at_inside_default_word_splits_like_bash() {
    let mut shell = TestShell::new().positionals(&[b"a b", b"c"]);
    assert_argv(
        &one("${x:-\"$@\"}", &mut shell),
        &["a b", "c"],
        "${x:-\"$@\"}",
    );
    assert_argv(
        &one("1${x:-\"$@\"}2", &mut shell),
        &["1a b", "c2"],
        "with prefix and suffix",
    );
    assert_argv(
        &one("\"${x:-\"$@\"}\"", &mut shell),
        &["a b", "c"],
        "outer quotes too",
    );
}

#[test]
fn quoted_default_word_inherits_quoting_for_unquoted_at() {
    // Inside `"${x:-…}"` an unquoted `$@` is quoted by the outer quotes, so it
    // yields one field per parameter instead of joining.
    let mut shell = TestShell::new().positionals(&[b"a b", b"c"]);
    assert_argv(
        &one("\"${x:-$@}\"", &mut shell),
        &["a b", "c"],
        "\"${x:-$@}\"",
    );
    assert_argv(&one("\"${x:-$*}\"", &mut shell), &["a b c"], "\"${x:-$*}\"");
}

#[test]
fn unquoted_at_inside_default_word_uses_the_ordinary_rules() {
    // Documented deviation: bash joins an unquoted `$@` inside `${…}` with
    // spaces and then suppresses splitting in some IFS configurations (e.g.
    // `IFS=:; set -- a:b c; printf '<%s>' ${x:-$@}` is one field in bash).
    // Here the ordinary unquoted rules apply, which is easier to explain and
    // matches POSIX's "expand the word, then split the result".
    let mut shell = TestShell::new().ifs(b":").positionals(&[b"a:b", b"c"]);
    assert_argv(&one("${x:-$@}", &mut shell), &["a", "b", "c"], "${x:-$@}");
    // With the default IFS the two models agree.
    let mut shell = TestShell::new().positionals(&[b"a b", b"c"]);
    let got = one("${x:-$@}", &mut shell);
    assert_eq!(got, argv(&["a", "b", "c"]), "got {}", show(&got));
}

#[test]
fn quoted_default_word_with_no_parameters_is_one_empty_field() {
    // `${x:-…}` substitutes an empty word, which vanishes unquoted but is an
    // empty field when quoted — even when the word is `$@` with no parameters.
    let mut shell = TestShell::new();
    assert_argv(&one("\"${x:-$@}\"", &mut shell), &[""], "\"${x:-$@}\"");
    assert_argv(
        &one("\"${x:-\"$@\"}\"", &mut shell),
        &[""],
        "\"${x:-\"$@\"}\"",
    );
    assert_argv(&one("${x:-$@}", &mut shell), &[], "unquoted");
    assert_argv(&one("\"${x:-}\"", &mut shell), &[""], "\"${x:-}\"");
}

#[test]
fn quoted_at_inside_alternate_word() {
    let mut shell = TestShell::new().ifs(b":").positionals(&[b"a:b", b"c"]);
    // x is set and non-null, so the alternate word is used.
    shell = shell.var(b"x", b"set");
    assert_argv(
        &one("\"${x:+$@}\"", &mut shell),
        &["a:b", "c"],
        "\"${x:+$@}\"",
    );
    // x is null, so the alternate word is not expanded: one empty quoted field.
    shell = shell.var(b"x", b"");
    assert_argv(&one("\"${x:+$@}\"", &mut shell), &[""], "null x");
}

#[test]
fn dollar_hash_and_counts() {
    let mut shell = TestShell::new().positionals(&[b"a", b"b", b"c"]);
    assert_argv(&one("\"$#\"", &mut shell), &["3"], "\"$#\"");
    assert_argv(&one("$#", &mut shell), &["3"], "$#");
    assert_argv(&one("\"${#}\"", &mut shell), &["3"], "\"${#}\"");
    assert_argv(&one("\"${#@}\"", &mut shell), &["3"], "\"${#@}\"");
    assert_argv(&one("\"${#*}\"", &mut shell), &["3"], "\"${#*}\"");

    let mut empty = TestShell::new();
    assert_argv(&one("\"$#\"", &mut empty), &["0"], "no parameters");
    assert_argv(&one("\"$@\"", &mut empty), &[], "no parameters, $@");
}
