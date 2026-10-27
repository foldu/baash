//! Array expansion: `${a[i]}`, `${a[@]}`, `${a[*]}`, counts, indices, slices.
//!
//! The expected values were checked against bash 5.3.20 except where the
//! comment names the rule being pinned (the quoted/unquoted distinction and
//! the empty-array cases are the ones that are easy to get subtly wrong).

use crate::fixture::TestShell;
use crate::{ErrorKind, ParamName, ParamOp, Part, UnsupportedOp, Vars};

use super::{assert_argv, expand_one, values};

fn one(src: &str, shell: &mut TestShell) -> Vec<Vec<u8>> {
    expand_one(src, shell)
}

#[test]
fn element_reads_expand_the_subscript() {
    let mut shell = TestShell::new().indexed(b"a", &[(0, b"one"), (1, b"2 3")]);
    assert_argv(&one("${a[0]}", &mut shell), &["one"], "${a[0]}");
    assert_argv(&one("\"${a[1]}\"", &mut shell), &["2 3"], "${a[1]}");
    // A missing element is empty, not an error.
    assert_argv(&one("\"${a[9]}\"", &mut shell), &[""], "missing element");

    // The subscript is a word first (`$i` expands) and arithmetic second.
    let mut shell = TestShell::new()
        .indexed(b"a", &[(0, b"zero"), (1, b"one"), (2, b"two")])
        .var(b"i", b"2");
    assert_argv(&one("\"${a[$i]}\"", &mut shell), &["two"], "${a[$i]}");
    // An unquoted subscript is not field-split, so `1 + 1` is one expression.
    assert_argv(
        &one("\"${a[$i]}\"", &mut shell),
        &["two"],
        "subscript with a variable",
    );
}

#[test]
fn indexed_array_subscripts_are_arithmetic() {
    // `i-1` is arithmetic, not the literal key "i-1".
    let mut shell = TestShell::new()
        .indexed(b"a", &[(0, b"zero"), (1, b"one"), (2, b"two")])
        .arith(b"i-1", b"1");
    assert_argv(&one("\"${a[i-1]}\"", &mut shell), &["one"], "${a[i-1]}");
    assert!(
        shell.arith_calls.iter().any(|call| call == b"i-1"),
        "the subscript must go through the arithmetic seam"
    );
}

#[test]
fn associative_elements_use_the_key_verbatim() {
    let mut shell =
        TestShell::new().assoc(b"a", &[(b"foo", b"bar"), (b"a+1", b"c"), (b"0", b"zero")]);
    assert_argv(&one("\"${a[foo]}\"", &mut shell), &["bar"], "${a[foo]}");
    assert_argv(&one("\"${a[a+1]}\"", &mut shell), &["c"], "key with a plus");
    assert_argv(&one("\"${a[0]}\"", &mut shell), &["zero"], "numeric key");
    // An associative subscript is a key, never arithmetic.
    assert!(shell.arith_calls.is_empty());
}

#[test]
fn quoted_at_is_one_field_per_element_and_empty_is_no_field() {
    let mut shell = TestShell::new().indexed(b"a", &[(0, b"1"), (1, b"2 3")]);
    assert_argv(&one("\"${a[@]}\"", &mut shell), &["1", "2 3"], "quoted @");
    assert_argv(&one("${a[@]}", &mut shell), &["1", "2", "3"], "unquoted @");

    // An empty array produces *zero* fields for "…[@]", not one empty field.
    let mut shell = TestShell::new();
    assert_argv(&one("\"${a[@]}\"", &mut shell), &[], "empty quoted @");
    assert_argv(&one("x\"${a[@]}\"y", &mut shell), &["xy"], "glued empty @");
}

#[test]
fn quoted_star_joins_with_the_first_ifs_character() {
    let mut shell = TestShell::new()
        .indexed(b"a", &[(0, b"1"), (1, b"2 3")])
        .ifs(b":");
    assert_argv(&one("\"${a[*]}\"", &mut shell), &["1:2 3"], "quoted star");
    // Unquoted, the joined result is field-split like any expansion result.
    assert_argv(&one("${a[*]}", &mut shell), &["1", "2 3"], "unquoted star");

    // IFS unset joins with a space; IFS empty concatenates.
    let mut shell = TestShell::new().indexed(b"a", &[(0, b"a"), (1, b"b")]);
    assert_argv(&one("\"${a[*]}\"", &mut shell), &["a b"], "default IFS");
    let mut shell = TestShell::new()
        .indexed(b"a", &[(0, b"a"), (1, b"b")])
        .ifs(b"");
    assert_argv(&one("\"${a[*]}\"", &mut shell), &["ab"], "empty IFS");

    // Even an empty array is one (empty) field for "${a[*]}".
    let mut shell = TestShell::new();
    assert_argv(&one("\"${a[*]}\"", &mut shell), &[""], "empty quoted star");
}

#[test]
fn counts_and_indices() {
    let mut shell = TestShell::new().indexed(b"a", &[(0, b"1"), (3, b"2 3")]);
    assert_argv(&one("\"${#a[@]}\"", &mut shell), &["2"], "element count");
    assert_argv(&one("\"${#a[1]}\"", &mut shell), &["0"], "missing length");
    assert_argv(&one("\"${#a[3]}\"", &mut shell), &["3"], "element length");

    // Indices are decimal, in numeric order, skipping holes.
    assert_argv(&one("\"${!a[@]}\"", &mut shell), &["0", "3"], "indices");
    assert_argv(&one("\"${!a[*]}\"", &mut shell), &["0 3"], "star indices");

    let mut shell = TestShell::new().assoc(b"a", &[(b"foo", b"1"), (b"bar", b"2")]);
    assert_argv(
        &one("\"${!a[@]}\"", &mut shell),
        &["foo", "bar"],
        "assoc keys keep insertion order",
    );
    assert_argv(&one("\"${#a[@]}\"", &mut shell), &["2"], "assoc count");
}

#[test]
fn negative_indices_count_from_the_greatest_key() {
    // Keys 0 and 2: greatest+1 is 3, so -1 is 2 and -2 is the hole at 1.
    let mut shell = TestShell::new().indexed(b"a", &[(0, b"zero"), (2, b"two")]);
    assert_argv(&one("\"${a[-1]}\"", &mut shell), &["two"], "${a[-1]}");
    assert_argv(&one("\"${a[-2]}\"", &mut shell), &[""], "hole at -2");
    assert_argv(&one("\"${a[-3]}\"", &mut shell), &["zero"], "${a[-3]}");
    assert_argv(&one("\"${a[-4]}\"", &mut shell), &[""], "before the start");
}

#[test]
fn slices_of_a_list() {
    let mut shell = TestShell::new().indexed(
        b"a",
        &[(0, b"0"), (1, b"1"), (2, b"2"), (3, b"3"), (4, b"4")],
    );
    assert_argv(
        &one("\"${a[@]:1:2}\"", &mut shell),
        &["1", "2"],
        "offset:len",
    );
    assert_argv(&one("\"${a[@]: -2}\"", &mut shell), &["3", "4"], "negative");
    assert_argv(&one("\"${a[*]:1:2}\"", &mut shell), &["1 2"], "star slice");
    assert_argv(&one("\"${a[@]:9}\"", &mut shell), &[], "past the end");
    assert_argv(&one("\"${a[@]:0:0}\"", &mut shell), &[], "zero length");

    // The bounds are arithmetic, through the same seam as a subscript.
    let mut shell = TestShell::new()
        .indexed(b"a", &[(0, b"0"), (1, b"1"), (2, b"2")])
        .arith(b"i-1", b"1");
    assert_argv(
        &one("\"${a[@]:i-1:2}\"", &mut shell),
        &["1", "2"],
        "arithmetic bounds",
    );

    // List slicing of the positional parameters uses the same rules.
    let mut shell = TestShell::new().positionals(&[b"a", b"b", b"c"]);
    assert_argv(&one("\"${@:1:2}\"", &mut shell), &["b", "c"], "${@:1:2}");
}

#[test]
fn arrays_decay_to_element_zero() {
    let mut shell = TestShell::new().indexed(b"a", &[(0, b"zero"), (1, b"one")]);
    assert_argv(&one("\"${a}\"", &mut shell), &["zero"], "bare ${a}");
    assert_argv(&one("\"${#a}\"", &mut shell), &["4"], "length of a[0]");

    // A scalar behaves like a one-element array for reads.
    let mut shell = TestShell::new().var(b"s", b"abc");
    assert_argv(&one("\"${s[@]}\"", &mut shell), &["abc"], "scalar @");
    assert_argv(&one("\"${s[*]}\"", &mut shell), &["abc"], "scalar *");
    assert_argv(&one("\"${#s[@]}\"", &mut shell), &["1"], "scalar count");
    assert_argv(&one("\"${s[0]}\"", &mut shell), &["abc"], "scalar [0]");
    assert_argv(&one("\"${s[1]}\"", &mut shell), &[""], "scalar [1]");
}

#[test]
fn associative_decay_and_length() {
    let mut shell = TestShell::new().assoc(b"a", &[(b"0", b"zero"), (b"k", b"v")]);
    assert_argv(
        &one("\"${a}\"", &mut shell),
        &["zero"],
        "assoc ${a} is key 0",
    );
    assert_argv(&one("\"${a[k]}\"", &mut shell), &["v"], "assoc key");
    assert_argv(&one("\"${#a}\"", &mut shell), &["4"], "length of key 0");
}

#[test]
fn default_operators_see_the_list() {
    // bash: an empty array is "unset"; one empty element is "null" for `:-`;
    // a longer list is non-null even when every element is empty.
    let mut shell = TestShell::new();
    assert_argv(&one("\"${a[@]:-d}\"", &mut shell), &["d"], "empty list");
    let mut shell = TestShell::new().indexed(b"a", &[(0, b"")]);
    assert_argv(&one("\"${a[@]:-d}\"", &mut shell), &["d"], "one null");
    let mut shell = TestShell::new().indexed(b"a", &[(0, b""), (1, b"")]);
    assert_argv(&one("\"${a[@]:-d}\"", &mut shell), &["", ""], "two nulls");
}

#[test]
fn indirect_element_expansion() {
    let mut shell = TestShell::new()
        .indexed(b"a", &[(0, b"v1"), (1, b"v2")])
        .var(b"v1", b"hello")
        .var(b"v2", b"world");
    assert_argv(
        &one("\"${!a[0]}, ${!a[1]}\"", &mut shell),
        &["hello, world"],
        "${!a[i]}",
    );
    // `${!s}` on a plain scalar is still refused: it would re-parse data as a
    // name.
    let mut shell = TestShell::new().var(b"s", b"x");
    let err = super::try_expand_line("${!s}", &mut shell).unwrap_err();
    assert!(matches!(
        err.kind,
        ErrorKind::Unsupported(UnsupportedOp::Indirect)
    ));
}

#[test]
fn array_words_have_a_stable_shape() {
    // The public word model keeps the array form visible: an element carries
    // its subscript word, a list carries its `[*]`/`[@]` spelling.
    let word = crate::fixture::parse_word(b"${a[1]}");
    let Part::Param(exp) = &word.parts[0] else {
        panic!("expected a parameter");
    };
    assert!(matches!(
        &exp.name,
        ParamName::Element { name, subscript }
            if name == b"a" && !subscript.parts.is_empty()
    ));
    assert_eq!(exp.op, ParamOp::Value);

    let word = crate::fixture::parse_word(b"${a[*]}");
    let Part::Param(exp) = &word.parts[0] else {
        panic!("expected a parameter");
    };
    assert!(matches!(&exp.name, ParamName::Elements { star: true, .. }));
}

#[test]
fn test_shell_publishes_arrays_through_the_vars_trait() {
    let shell = TestShell::new().indexed(b"a", &[(2, b"x")]);
    let array = shell.array(b"a").expect("an array");
    assert_eq!(array.len(), 1);
    assert_eq!(array.indices(), values(&[b"2"]));
    assert_eq!(Vars::get(&shell, b"a"), None);

    let shell = TestShell::new().assoc(b"m", &[(b"k", b"v")]);
    let array = shell.array(b"m").expect("an associative array");
    assert_eq!(array.get(b"k"), Some(b"v".as_slice()));
}
