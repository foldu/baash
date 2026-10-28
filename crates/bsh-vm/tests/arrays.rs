//! End-to-end array behaviour through the tree evaluator.
//!
//! Results are read back through `bsh_lang::Vars` (and `declare -p` through a
//! command substitution), so the test exercises the parser, the expansion
//! pipeline and the assignment executor together instead of re-testing any one
//! layer's API.

use bsh_lang::{ArrayValue, Vars};
use bsh_vm::Interp;

fn run(interp: &mut Interp, script: &str) {
    let status = interp.run_named("<array-test>", script.as_bytes());
    assert!(status.is_success(), "script failed: {script}");
}

fn text(interp: &Interp, name: &[u8]) -> String {
    String::from_utf8_lossy(Vars::get(interp, name).unwrap_or_default()).into_owned()
}

fn values(interp: &Interp, name: &[u8]) -> Vec<String> {
    match Vars::array(interp, name) {
        Some(ArrayValue::Indexed(elements)) => elements
            .values()
            .map(|value| String::from_utf8_lossy(value).into_owned())
            .collect(),
        Some(ArrayValue::Assoc(pairs)) => pairs
            .iter()
            .map(|(_, value)| String::from_utf8_lossy(value).into_owned())
            .collect(),
        None => panic!("{} is not an array", String::from_utf8_lossy(name)),
    }
}

fn shell() -> Interp {
    let mut interp = Interp::new(false);
    interp.set_arg0("bsh");
    interp
}

#[test]
fn literals_expansion_and_counts() {
    let mut interp = shell();
    run(
        &mut interp,
        "a=(1 '2 3')
         n=${#a[@]}
         first=${a[0]}
         second=\"${a[1]}\"
         joined=\"${a[*]}\"
         decayed=\"${a[@]}\"",
    );
    assert_eq!(text(&interp, b"n"), "2");
    assert_eq!(text(&interp, b"first"), "1");
    assert_eq!(text(&interp, b"second"), "2 3");
    assert_eq!(text(&interp, b"joined"), "1 2 3");
    // An assignment is a scalar context, so a quoted `[@]` decays by joining.
    assert_eq!(text(&interp, b"decayed"), "1 2 3");
    assert_eq!(values(&interp, b"a"), ["1", "2 3"]);
}

#[test]
fn sparse_storage_and_index_lists() {
    let mut interp = shell();
    run(
        &mut interp,
        "a=()
         (( a[99]=1 ))
         n=${#a[@]}
         indices=\"${!a[@]}\"
         value=${a[99]}
         missing=${a[33]}",
    );
    assert_eq!(text(&interp, b"n"), "1");
    assert_eq!(text(&interp, b"indices"), "99");
    assert_eq!(text(&interp, b"value"), "1");
    assert_eq!(text(&interp, b"missing"), "");
    assert_eq!(values(&interp, b"a"), ["1"]);
}

#[test]
fn element_assignment_append_and_unset() {
    let mut interp = shell();
    run(
        &mut interp,
        "a=(1 2 3)
         a[1]=TWO
         a[4]+=four
         a+=(5 6)
         unset 'a[0]'
         n=${#a[@]}
         indices=\"${!a[@]}\"",
    );
    assert_eq!(text(&interp, b"n"), "5");
    assert_eq!(text(&interp, b"indices"), "1 2 4 5 6");
    assert_eq!(values(&interp, b"a"), ["TWO", "3", "four", "5", "6"]);

    // `unset a` removes the whole variable.
    run(&mut interp, "unset a");
    assert!(Vars::array(&interp, b"a").is_none());

    // Unsetting one element leaves a hole that iteration skips.
    run(
        &mut interp,
        "a=(x y z)
         unset 'a[1]'
         n=${#a[@]}
         indices=\"${!a[@]}\"",
    );
    assert_eq!(text(&interp, b"n"), "2");
    assert_eq!(text(&interp, b"indices"), "0 2");
}

#[test]
fn associative_literals_lookup_and_order() {
    let mut interp = shell();
    run(
        &mut interp,
        "declare -A m
         m=([foo]=bar ['a+1']='c d')
         got=${m[foo]}
         quot=${m[a+1]}
         keys=\"${!m[*]}\"
         vals=\"${m[*]}\"",
    );
    assert_eq!(text(&interp, b"got"), "bar");
    assert_eq!(text(&interp, b"quot"), "c d");
    assert_eq!(text(&interp, b"keys"), "foo a+1");
    assert_eq!(text(&interp, b"vals"), "bar c d");
}

#[test]
fn local_arrays_are_scope_local() {
    let mut interp = shell();
    run(
        &mut interp,
        "a=(outer)
         f() {
           local a=(inner 'two words')
           n=${#a[@]}
           first=\"${a[0]}\"
         }
         f
         after=${a[0]}",
    );
    assert_eq!(text(&interp, b"n"), "2");
    assert_eq!(text(&interp, b"first"), "inner");
    assert_eq!(text(&interp, b"after"), "outer");
}

#[test]
fn slices_use_arithmetic_bounds() {
    let mut interp = shell();
    run(
        &mut interp,
        "a=(0 1 2 3 4)
         i=3
         middle=\"${a[@]:1:2}\"
         tail=\"${a[@]:i-2}\"
         neg=\"${a[*]: -2}\"",
    );
    assert_eq!(text(&interp, b"middle"), "1 2");
    assert_eq!(text(&interp, b"tail"), "1 2 3 4");
    assert_eq!(text(&interp, b"neg"), "3 4");
}

#[test]
fn arithmetic_subscripts_read_and_write() {
    let mut interp = shell();
    run(
        &mut interp,
        "a=(10 20 30)
         sum=$((a[0] + a[2]))
         (( a[5] = 60 ))
         (( a[5] += 1 ))
         n=${#a[@]}
         sum2=$((a[5]))
         i=1
         shifted=$((a[i+1]))",
    );
    assert_eq!(text(&interp, b"sum"), "40");
    assert_eq!(text(&interp, b"n"), "4");
    assert_eq!(text(&interp, b"sum2"), "61");
    assert_eq!(text(&interp, b"shifted"), "30");
}

// Everything below was checked against bash 5.3.20 before it was written; the
// trailing comments give the exact shell snippet that establishes the rule.

#[test]
fn sparse_holes_survive_every_reader() {
    // bash 5.3.20:
    //   a=(one two three); unset 'a[1]'; a[5]=five
    //   "${a[@]}" -> "one three five", "${a[*]}" joins the same elements,
    //   ${#a[@]} -> 3, ${!a[@]} -> "0 2 5", ${a[1]} -> "".
    let mut interp = shell();
    run(
        &mut interp,
        "a=(one two three)
         unset 'a[1]'
         a[5]=five
         at=\"${a[@]}\"
         star=\"${a[*]}\"
         n=${#a[@]}
         keys=\"${!a[@]}\"
         hole=\"${a[1]}\"
         last=\"${a[5]}\"",
    );
    assert_eq!(text(&interp, b"at"), "one three five");
    assert_eq!(text(&interp, b"star"), "one three five");
    assert_eq!(text(&interp, b"n"), "3");
    assert_eq!(text(&interp, b"keys"), "0 2 5");
    assert_eq!(text(&interp, b"hole"), "");
    assert_eq!(text(&interp, b"last"), "five");
    assert_eq!(values(&interp, b"a"), ["one", "three", "five"]);

    // Appending starts after the *greatest* subscript, not after the count:
    // bash 5.3.20 gives ${!a[@]} = "0 2 5 6".
    run(&mut interp, "a+=(six)\n n=${#a[@]}\n keys=\"${!a[@]}\"");
    assert_eq!(text(&interp, b"keys"), "0 2 5 6");
}

#[test]
fn assignment_forms_write_the_intended_subscript() {
    // bash 5.3.20:
    //   a=(1 2 3); a[1]+=X          -> "1 2X 3"
    //   a[5]+=Y on a missing element -> creates it
    //   i=1; a[i+1]=Z                -> writes a[2]
    //   declare -a d=([3]=x y)       -> [3]=x, [4]=y
    //   s=hi; s[2]+=x                -> element 0 stays "hi"
    let mut interp = shell();
    run(
        &mut interp,
        "a=(1 2 3)
         a[1]+=X
         a[5]+=Y
         i=1
         a[i+1]=Z
         append=\"${a[@]}\"
         keys=\"${!a[@]}\"
         declare -a d=([3]=x y)
         d_at=\"${d[@]}\"
         d_keys=\"${!d[@]}\"
         s=hi
         s[2]+=x
         s_at=\"${s[@]}\"
         s_keys=\"${!s[@]}\"",
    );
    assert_eq!(text(&interp, b"append"), "1 2X Z Y");
    assert_eq!(text(&interp, b"keys"), "0 1 2 5");
    assert_eq!(text(&interp, b"d_at"), "x y");
    assert_eq!(text(&interp, b"d_keys"), "3 4");
    assert_eq!(text(&interp, b"s_at"), "hi x");
    assert_eq!(text(&interp, b"s_keys"), "0 2");
}

#[test]
fn local_arrays_and_read_a_keep_the_shape() {
    // bash 5.3.20: `local -a la; la[4]=z` keeps the hole, and `read -a`
    // clears the target array and fills it densely from zero.
    let mut interp = shell();
    run(
        &mut interp,
        "f() {
           local -a la=(one)
           la[4]=five
           local_at=\"${la[@]}\"
           local_keys=\"${!la[@]}\"
         }
         f
         read -r -a ra <<< 'x y z'
         ra[5]=w
         read_at=\"${ra[@]}\"
         read_keys=\"${!ra[@]}\"",
    );
    assert_eq!(text(&interp, b"local_at"), "one five");
    assert_eq!(text(&interp, b"local_keys"), "0 4");
    assert_eq!(text(&interp, b"read_at"), "x y z w");
    assert_eq!(text(&interp, b"read_keys"), "0 1 2 5");
}

#[test]
fn subscripts_are_arithmetic_for_indexed_and_text_for_associative() {
    // bash 5.3.20: an indexed subscript is an arithmetic expression, so an
    // unset name is 0 and `i+1` is evaluated; an associative subscript is the
    // expanded *text*, so `1+1` is the literal key "1+1".
    let mut interp = shell();
    run(
        &mut interp,
        "a=(zero one two)
         unset i
         unset_value=\"${a[i]}\"
         i=1
         shifted=\"${a[i+1]}\"
         paren=\"${a[$((i+1))]}\"
         negative=\"${a[-1]}\"
         declare -A m
         k='1+1'
         m[$k]=literal
         m[a b]='spaced key'
         assoc_keys=\"${!m[@]}\"
         literal=${m[1+1]}",
    );
    assert_eq!(text(&interp, b"unset_value"), "zero");
    assert_eq!(text(&interp, b"shifted"), "two");
    assert_eq!(text(&interp, b"paren"), "two");
    assert_eq!(text(&interp, b"negative"), "two");
    assert_eq!(text(&interp, b"assoc_keys"), "1+1 a b");
    assert_eq!(text(&interp, b"literal"), "literal");
}
#[test]
fn declaration_builtins_apply_array_literals() {
    // bash 5.3.20: `readonly -a ra=([3]=x y)` is the array [3]=x [4]=y, and
    // `readonly -A rb=([k]=v)` keeps its key. The parser records these
    // operands as array literals, which the builtins must consume instead of
    // assigning the textual value "(…)".
    let mut interp = shell();
    run(
        &mut interp,
        "readonly -a ra=([3]=x y)
         ra_at=\"${ra[@]}\"
         ra_keys=\"${!ra[@]}\"
         readonly -A rb=([k]=v)
         rb_value=\"${rb[k]}\"
         rb_keys=\"${!rb[@]}\"
         export ea=(1 2)
         ea_at=\"${ea[@]}\"
         ea_keys=\"${!ea[@]}\"",
    );
    assert_eq!(text(&interp, b"ra_at"), "x y");
    assert_eq!(text(&interp, b"ra_keys"), "3 4");
    assert_eq!(text(&interp, b"rb_value"), "v");
    assert_eq!(text(&interp, b"rb_keys"), "k");
    assert_eq!(text(&interp, b"ea_at"), "1 2");
    assert_eq!(text(&interp, b"ea_keys"), "0 1");
    // The attribute is real: the same shell refuses a later element write.
    let status = interp.run_named("<array-test>", b"ra[0]=changed\n");
    assert!(!status.is_success());
    assert_eq!(values(&interp, b"ra"), ["x", "y"]);
}

#[test]
fn empty_literal_subscript_is_rejected_but_quoted_empty_is_zero() {
    // bash 5.3.20: `a[]=x` is `a[]: bad array subscript` and leaves the array
    // alone; `a[""]=x` writes element zero because the quoted empty word is
    // arithmetic zero. `declare a[]=x` also leaves an empty indexed array.
    let mut interp = shell();
    run(
        &mut interp,
        "a=(1 2 3)
         a[]=x
         rejected=$?
         a[\"\"]=zero
         keys=\"${!a[@]}\"
         first=\"${a[0]}\"
         declare e[]=v
         declared=$?
         declared_len=${#e[@]}",
    );
    assert_eq!(text(&interp, b"rejected"), "1");
    assert_eq!(text(&interp, b"first"), "zero");
    assert_eq!(text(&interp, b"keys"), "0 1 2");
    assert_eq!(text(&interp, b"declared"), "1");
    assert_eq!(text(&interp, b"declared_len"), "0");
    assert_eq!(values(&interp, b"a"), ["zero", "2", "3"]);
}
