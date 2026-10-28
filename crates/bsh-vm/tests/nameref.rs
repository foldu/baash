//! End-to-end `nameref` (`declare -n`) and declaration-inspection behaviour.
//!
//! The interesting part of a nameref is that *every* operation has to follow
//! it while `declare -p` must still show the reference cell, so these tests go
//! through the parser, expansion and assignment executor rather than calling
//! `Vars` directly.

use bsh_lang::{ArrayValue, Vars};
use bsh_vm::Interp;

fn shell() -> Interp {
    let mut interp = Interp::new(false);
    interp.set_arg0("bsh");
    interp
}

fn run(interp: &mut Interp, script: &str) {
    let status = interp.run_named("<nameref-test>", script.as_bytes());
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

/// Nested references must resolve transitively for both reads and writes.
///
/// `r1 -> r2 -> x`: writing `r1` reaches `x`, and `declare -p` on each link
/// still shows that link's own reference text.
#[test]
fn nested_references_resolve_transitively() {
    let mut interp = shell();
    run(
        &mut interp,
        "x=one
         declare -n r1=r2
         declare -n r2=x
         through_r1=$r1
         r1=two
         through_r2=$r2
         x=three
         through_r2_again=$r2",
    );
    assert_eq!(text(&interp, b"x"), "three");
    assert_eq!(text(&interp, b"through_r1"), "one");
    assert_eq!(text(&interp, b"through_r2"), "two");
    assert_eq!(text(&interp, b"through_r2_again"), "three");
    // The reference text is what the cell holds; resolution happens on use.
    run(&mut interp, "r1_decl=$(declare -p r1)");
    assert_eq!(text(&interp, b"r1_decl"), "declare -n r1=r2");
}

/// `declare -p ref` shows the reference, `declare -p target` the value.
#[test]
fn declare_p_prints_the_reference_not_the_target() {
    let mut interp = shell();
    run(
        &mut interp,
        "x=value
         declare -n ref=x
         ref_decl=$(declare -p ref)
         x_decl=$(declare -p x)
         both=$(declare -p ref x)",
    );
    assert_eq!(text(&interp, b"ref_decl"), "declare -n ref=x");
    assert_eq!(text(&interp, b"x_decl"), "declare -- x=value");
    assert_eq!(
        text(&interp, b"both"),
        "declare -n ref=x\ndeclare -- x=value"
    );
    // Reading the reference yields the target's value.
    run(&mut interp, "read_through=$ref");
    assert_eq!(text(&interp, b"read_through"), "value");
}

/// Element references: `-n r='a[1]'` reads and writes one element, and a
/// further subscript is refused instead of being applied to the target.
#[test]
fn element_references_read_and_write_one_element() {
    let mut interp = shell();
    run(
        &mut interp,
        "a=(zero one two)
         declare -n elem='a[1]'
         got=$elem
         elem=ONE
         decayed=$a",
    );
    assert_eq!(text(&interp, b"got"), "one");
    assert_eq!(values(&interp, b"a"), ["zero", "ONE", "two"]);
    assert_eq!(text(&interp, b"decayed"), "zero");
}

/// A reference with an associative key works for reads and writes.
#[test]
fn associative_key_reference() {
    let mut interp = shell();
    run(
        &mut interp,
        "declare -A m
         m[k]=v
         key=k
         declare -n r='m[$key]'
         before=$r
         r=changed
         after=$r
         direct=${m[k]}",
    );
    assert_eq!(text(&interp, b"before"), "v");
    assert_eq!(text(&interp, b"after"), "changed");
    assert_eq!(text(&interp, b"direct"), "changed");
}

/// A nameref inside a function resolves against the caller's local, which is
/// bash's dynamic scoping.
#[test]
fn dynamic_scope_reaches_the_callers_local() {
    let mut interp = shell();
    run(
        &mut interp,
        "inner() { local -n ref=$1; ref=changed; }
         outer() { local mine=original; inner mine; result=$mine; }
         outer",
    );
    assert_eq!(text(&interp, b"result"), "changed");
}

/// `unset ref` removes the target and leaves the reference dangling.
#[test]
fn unset_follows_the_reference() {
    let mut interp = shell();
    run(
        &mut interp,
        "x=X
         declare -n ref=x
         unset ref
         after=$ref
         target=${x-unset}",
    );
    assert_eq!(text(&interp, b"after"), "");
    assert_eq!(text(&interp, b"target"), "unset");
}

/// `declare -n ref=ref` is rejected and leaves the variable alone; a cycle of
/// two references is accepted at definition time and fatal when read or
/// written, so a script cannot silently compute with a name that never
/// resolves.
#[test]
fn self_reference_is_rejected_and_cycles_are_fatal_on_use() {
    let mut interp = shell();
    run(
        &mut interp,
        "keep=old
         declare -n keep=keep || self_status=$?
         kept=$keep",
    );
    assert_eq!(text(&interp, b"self_status"), "1");
    // The rejected declaration did not turn `keep` into a reference.
    assert_eq!(text(&interp, b"kept"), "old");

    let mut interp = shell();
    run(&mut interp, "declare -n a=b; declare -n b=a");
    let status = interp.run_named("<cycle-read>", b"echo $a");
    assert_eq!(status, bsh_runtime::Status::FAILURE);
    let status = interp.run_named("<cycle-write>", b"a=1");
    assert_eq!(status, bsh_runtime::Status::FAILURE);
}

/// `typeset +n` turns the reference back into a plain string holding the last
/// reference text; `declare -n` with no value promotes the current value.
#[test]
fn toggling_the_nameref_attribute() {
    let mut interp = shell();
    run(
        &mut interp,
        "x=foo
         ref=x
         typeset -n ref
         while_ref=$ref
         typeset +n ref
         after=$ref
         typeset -n ref
         again=$ref
         ref_decl=$(declare -p ref)",
    );
    assert_eq!(text(&interp, b"while_ref"), "foo");
    assert_eq!(text(&interp, b"after"), "x");
    assert_eq!(text(&interp, b"again"), "foo");
    assert_eq!(text(&interp, b"ref_decl"), "declare -n ref=x");
}

/// The listing forms that the declaration builtins share.
#[test]
fn declaration_listing_forms() {
    let mut interp = shell();
    run(
        &mut interp,
        "a=(1 2 3)
         b=()
         b[3]=x
         declare -A m=([k]=v)
         declare -r ro=1
         declare -n ref=a
         a_decl=$(declare -p a)
         b_decl=$(declare -p b)
         m_decl=$(declare -p m)
         ro_decl=$(declare -p ro)
         filtered=$(declare -pn)",
    );
    assert_eq!(
        text(&interp, b"a_decl"),
        "declare -a a=([0]=\"1\" [1]=\"2\" [2]=\"3\")"
    );
    assert_eq!(text(&interp, b"b_decl"), "declare -a b=([3]=\"x\")");
    assert_eq!(text(&interp, b"m_decl"), "declare -A m=([k]=\"v\" )");
    assert_eq!(text(&interp, b"ro_decl"), "declare -r ro=1");
    assert_eq!(text(&interp, b"filtered"), "declare -n ref=a");
}
