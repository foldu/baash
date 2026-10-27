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
