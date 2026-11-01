//! Special variables and the seams added for the bash variable operators.
//!
//! Values are read back through the shell state rather than stdout, so the
//! test does not depend on which utilities the test `PATH` happens to find.

#![forbid(unsafe_code)]

use baash_runtime::{ShellHost, Status};
use baash_vm::Interp;

fn shell() -> Interp {
    let mut interp = Interp::new(false);
    interp.set_arg0("baash");
    interp
}

fn run(interp: &mut Interp, script: &str) -> Status {
    interp.run_named("<specials-test>", script.as_bytes())
}

fn scalar(interp: &mut Interp, name: &[u8]) -> Vec<u8> {
    interp
        .state()
        .vars
        .scalar(name)
        .unwrap_or_default()
        .to_vec()
}

#[test]
fn underscore_is_the_last_argument_of_the_previous_command() {
    let mut interp = shell();
    assert!(run(&mut interp, ": 42").is_success());
    assert_eq!(scalar(&mut interp, b"_"), b"42");
    assert!(run(&mut interp, "x='with spaces'; : $x").is_success());
    assert_eq!(scalar(&mut interp, b"_"), b"spaces");
}

#[test]
fn identity_variables_exist_and_are_readonly() {
    let mut interp = shell();
    let uid = scalar(&mut interp, b"UID");
    assert!(!uid.is_empty());
    assert_eq!(uid, scalar(&mut interp, b"EUID"));
    assert!(!scalar(&mut interp, b"PPID").is_empty());
    assert!(!scalar(&mut interp, b"OSTYPE").is_empty());
    assert!(!scalar(&mut interp, b"HOSTNAME").is_empty());
    // The assignment fails and leaves the value alone.
    assert!(!run(&mut interp, "UID=xx").is_success());
    assert_eq!(scalar(&mut interp, b"UID"), uid);
}

#[test]
fn random_is_a_number_and_changes() {
    let mut interp = shell();
    assert!(run(&mut interp, "first=$RANDOM").is_success());
    assert!(run(&mut interp, "second=$RANDOM").is_success());
    let first = String::from_utf8(scalar(&mut interp, b"first")).unwrap();
    let second = String::from_utf8(scalar(&mut interp, b"second")).unwrap();
    assert!(first.parse::<u32>().unwrap() < 32768);
    assert!(second.parse::<u32>().unwrap() < 32768);
}

#[test]
fn name_listing_and_attributes() {
    let mut interp = shell();
    assert!(run(&mut interp, "Z=1; ZIP=2; ZOO=3; v=\"${!Z@}\"").is_success());
    assert_eq!(scalar(&mut interp, b"v"), b"Z ZIP ZOO");
    assert!(run(&mut interp, "v=\"${!Z*}\"; v=\"${v}\"").is_success());
    assert!(run(&mut interp, "declare -a a=(1 2); v=\"${a@a}\"").is_success());
    assert_eq!(scalar(&mut interp, b"v"), b"a");
    assert!(run(&mut interp, "declare -A m=([k]=v); v=\"${m@a}\"").is_success());
    assert_eq!(scalar(&mut interp, b"v"), b"A");
    assert!(run(&mut interp, "declare -rx X=1; v=\"${X@a}\"").is_success());
    assert_eq!(scalar(&mut interp, b"v"), b"rx");
}
