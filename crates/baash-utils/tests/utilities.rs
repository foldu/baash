//! Every embedded utility answers a smoke test.
//!
//! The table is a curated set that grows, and a utility that links but panics
//! on its first argument is worse than one that is absent: the shell would
//! report a crash instead of "command not found". `--help` is the cheapest
//! argument every uutils binary accepts without touching the file system, so
//! it runs for each entry, once per utility, on its own worker thread (which
//! is what a real call does too).

use std::collections::HashSet;

fn argv(items: &[&str]) -> Vec<Vec<u8>> {
    items.iter().map(|item| item.as_bytes().to_vec()).collect()
}

#[test]
fn every_embedded_utility_answers_help() {
    // The isolated four need a host executable to spawn, and a test harness is
    // not one (`baash_utils::is_isolated`); `crates/baash/tests/
    // embedded_isolated.rs` drives those through the real binary instead.
    for name in baash_utils::names().filter(|name| !baash_utils::is_isolated(name.as_bytes())) {
        let status = baash_utils::run(&argv(&[name, "--help"]));
        assert_eq!(
            status,
            Some(0),
            "`{name} --help` must succeed: an embedded utility that cannot answer its own \
             usage is one the shell would crash in instead of running"
        );
    }
}

#[test]
fn no_utility_is_listed_twice() {
    let names: Vec<&str> = baash_utils::names().collect();
    let unique: HashSet<&str> = names.iter().copied().collect();
    assert_eq!(names.len(), unique.len(), "duplicate name in the table");
}

#[test]
fn the_set_is_the_one_the_docs_describe() {
    // `docs/portability.md` §6 lists the set by name, and the point of this
    // test is that the two cannot drift: adding a utility means editing both.
    let documented = "base64 basename cat cksum comm cp cut date dirname du expr head ln ls \
md5sum mkdir mktemp mv nl nproc od paste readlink realpath rm rmdir seq sha1sum sha256sum \
shuf sleep sort split tac tail tee touch tr truncate uniq wc clear \
awk find grep sed xargs";
    let documented: HashSet<&str> = documented.split_whitespace().collect();
    let actual: HashSet<&str> = baash_utils::names().collect();
    let missing: Vec<&&str> = documented.difference(&actual).collect();
    let undocumented: Vec<&&str> = actual.difference(&documented).collect();
    assert!(
        missing.is_empty() && undocumented.is_empty(),
        "table and docs disagree: documented but not embedded {missing:?}, embedded but not \
         documented {undocumented:?}"
    );
}
