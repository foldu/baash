//! `read` semantics that are easy to get subtly wrong.
//!
//! The spec suite covers the broad surface; these pin the properties the
//! implementation is built around, one per test: the `IFS` maximum-split rule,
//! backslash continuation, quoting an `IFS` character, the no-over-read
//! guarantee, and `REPLY`. They run through `run_named`, so they also exercise
//! the redirect + built-in path rather than calling the body directly.
//!
//! Deliberately no pipelines: this crate's tests run on several threads, and
//! `fork()` is only sound in the single-threaded shell process (DESIGN.md
//! §4.3). Everything here uses file redirection, which needs no fork.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use baash_lang::Vars;
use baash_vm::Interp;

/// Tests that redirect a descriptor must not run concurrently with each other.
///
/// Redirections are process-wide (`dup2` on fd 0, 1, 3, …), and this test
/// binary runs its tests on several threads in one process, so two `read`
/// tests at once would fight over the same descriptor. A process mutex is the
/// honest way to say "these tests touch the process".
static FDS: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    FDS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn interp() -> Interp {
    let mut interp = Interp::new(false);
    interp.set_arg0("baash");
    interp
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("baash-read-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> String {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write fixture");
    path.display().to_string()
}

fn value(interp: &Interp, name: &[u8]) -> Option<String> {
    Vars::get(interp, name).map(|bytes| String::from_utf8_lossy(bytes).into_owned())
}

#[test]
fn ifs_splits_into_names_and_the_last_name_takes_the_rest() {
    let _serial = serial();
    let dir = temp_dir("split");
    let file = write_file(&dir, "in.txt", b"A\t\tB C D E \nFG\n");
    let mut interp = interp();
    let status = interp.run_named(
        "<test>",
        format!("read x y z < '{file}'; read x2 y2 < '{file}'").as_bytes(),
    );
    assert_eq!(status.exit_code(), 0);
    assert_eq!(value(&interp, b"x").as_deref(), Some("A"));
    assert_eq!(value(&interp, b"y").as_deref(), Some("B"));
    // The last name takes the remainder, delimiters included, with trailing
    // IFS whitespace removed.
    assert_eq!(value(&interp, b"z").as_deref(), Some("C D E"));
    assert_eq!(value(&interp, b"x2").as_deref(), Some("A"));
    assert_eq!(value(&interp, b"y2").as_deref(), Some("B C D E"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ifs_non_whitespace_delimiters_produce_empty_fields() {
    let _serial = serial();
    let dir = temp_dir("ifs-delims");
    // bash: `IFS=, ; read a b` on "a,,b" leaves the second field empty, while a
    // single trailing delimiter is not a field of its own.
    let file = write_file(&dir, "in.txt", b"a,,b\nx,y,\n");
    let mut interp = interp();
    interp.run_named("<test>", b"IFS=,");
    interp.run_named(
        "<test>",
        format!("{{ read a b c; read d e; }} < '{file}'").as_bytes(),
    );
    assert_eq!(value(&interp, b"a").as_deref(), Some("a"));
    assert_eq!(value(&interp, b"b").as_deref(), Some(""));
    assert_eq!(value(&interp, b"c").as_deref(), Some("b"));
    assert_eq!(value(&interp, b"d").as_deref(), Some("x"));
    assert_eq!(value(&interp, b"e").as_deref(), Some("y"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ifs_edge_case_from_the_bash_algorithm() {
    let _serial = serial();
    let dir = temp_dir("ifs-edge");
    // `IFS='x '` and an all-delimiter line: the *first* name takes an empty
    // field and the last takes the remainder. bash gives '' and 'xx' here.
    let file = write_file(&dir, "in.txt", b"xxx\nx x\nx\na xx\n");
    let mut interp = interp();
    interp.run_named("<test>", b"IFS='x '");
    interp.run_named(
        "<test>",
        format!("{{ read a b; read c d; read e f; read g h; }} < '{file}'").as_bytes(),
    );
    assert_eq!(value(&interp, b"a").as_deref(), Some(""));
    assert_eq!(value(&interp, b"b").as_deref(), Some("xx"));
    assert_eq!(value(&interp, b"c").as_deref(), Some(""));
    assert_eq!(value(&interp, b"d").as_deref(), Some(""));
    assert_eq!(value(&interp, b"e").as_deref(), Some(""));
    assert_eq!(value(&interp, b"f").as_deref(), Some(""));
    assert_eq!(value(&interp, b"g").as_deref(), Some("a"));
    assert_eq!(value(&interp, b"h").as_deref(), Some(""));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn backslash_continuation_joins_physical_lines() {
    let _serial = serial();
    let dir = temp_dir("continuation");
    let file = write_file(&dir, "in.txt", b"one-\\\ntwo three-\\\nfour five-\\\nsix\n");
    let mut interp = interp();
    interp.run_named("<test>", format!("read x y < '{file}'").as_bytes());
    assert_eq!(value(&interp, b"x").as_deref(), Some("one-two"));
    assert_eq!(
        value(&interp, b"y").as_deref(),
        Some("three-four five-six"),
        "the continuation swallows the newline and keeps splitting"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn backslash_quotes_an_ifs_character_and_r_disables_it() {
    let _serial = serial();
    let dir = temp_dir("quoting");
    let file = write_file(&dir, "in.txt", b"b\\: c\n");
    let mut interp = interp();
    interp.run_named("<test>", b"IFS=:");
    interp.run_named("<test>", format!("read a b < '{file}'").as_bytes());
    assert_eq!(value(&interp, b"a").as_deref(), Some("b: c"));
    assert_eq!(value(&interp, b"b").as_deref(), Some(""));

    // With -r the backslash is literal, so the colon splits and the backslash
    // stays on the first field.
    interp.run_named("<test>", format!("read -r c d < '{file}'").as_bytes());
    assert_eq!(value(&interp, b"c").as_deref(), Some("b\\"));
    assert_eq!(value(&interp, b"d").as_deref(), Some(" c"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_seekable_read_gives_back_the_bytes_it_did_not_consume() {
    let _serial = serial();
    let dir = temp_dir("no-over-read");
    let file = write_file(&dir, "in.txt", b"one\ntwo\nthree\n");
    let out = dir.join("out.txt");
    let mut interp = interp();
    // Two reads through one descriptor, then an external command on the same
    // descriptor: `head -1` must see line three, not the start of the file and
    // not a truncated file.
    // The external command needs the test process's `PATH` (this suite runs on
    // NixOS, where `/usr/bin/head` does not exist).
    let path = std::env::var("PATH").unwrap_or_default();
    interp.run_named("<test>", format!("PATH={path}").as_bytes());
    let script = format!(
        "{{ read a; read b; head -1; }} < '{file}' > '{out}'",
        out = out.display()
    );
    let status = interp.run_named("<test>", script.as_bytes());
    assert_eq!(status.exit_code(), 0);
    assert_eq!(value(&interp, b"a").as_deref(), Some("one"));
    assert_eq!(value(&interp, b"b").as_deref(), Some("two"));
    let seen = std::fs::read_to_string(&out).expect("head output");
    assert_eq!(seen, "three\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_second_read_continues_where_the_first_stopped() {
    let _serial = serial();
    let dir = temp_dir("second-read");
    let file = write_file(&dir, "in.txt", b"first\nsecond\nthird\n");
    let mut interp = interp();
    interp.run_named(
        "<test>",
        format!("read a < '{file}'; read b < '{file}'").as_bytes(),
    );
    // Each command opens its own descriptor, so both see the first line: the
    // point of this test is the *single*-descriptor case below.
    assert_eq!(value(&interp, b"a").as_deref(), Some("first"));
    assert_eq!(value(&interp, b"b").as_deref(), Some("first"));

    interp.run_named(
        "<test>",
        format!("{{ read c; read d; }} < '{file}'").as_bytes(),
    );
    assert_eq!(value(&interp, b"c").as_deref(), Some("first"));
    assert_eq!(value(&interp, b"d").as_deref(), Some("second"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn eof_with_partial_data_assigns_it_and_returns_one() {
    let _serial = serial();
    let dir = temp_dir("eof-partial");
    let file = write_file(&dir, "in.txt", b"ZZZ");
    let mut interp = interp();
    let status = interp.run_named("<test>", format!("read x < '{file}'").as_bytes());
    assert_eq!(status.exit_code(), 1);
    assert_eq!(value(&interp, b"x").as_deref(), Some("ZZZ"));

    // An empty file assigns an empty value and still fails.
    let empty = write_file(&dir, "empty.txt", b"");
    let status = interp.run_named(
        "<test>",
        format!("read reply_holder < '{empty}'").as_bytes(),
    );
    assert_eq!(status.exit_code(), 1);
    assert_eq!(value(&interp, b"reply_holder").as_deref(), Some(""));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn no_names_assigns_reply_without_splitting() {
    let _serial = serial();
    let dir = temp_dir("reply");
    let file = write_file(&dir, "in.txt", b"  a b  \n");
    let mut interp = interp();
    interp.run_named("<test>", format!("read < '{file}'").as_bytes());
    assert_eq!(
        value(&interp, b"REPLY").as_deref(),
        Some("  a b  "),
        "REPLY keeps leading and trailing IFS whitespace"
    );

    // A name, in contrast, is split (and the leading whitespace goes).
    interp.run_named("<test>", format!("read myvar < '{file}'").as_bytes());
    assert_eq!(value(&interp, b"myvar").as_deref(), Some("a b"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn d_n_and_capital_n_pick_their_endings() {
    let _serial = serial();
    let dir = temp_dir("flags");
    let file = write_file(&dir, "in.txt", b"a b c\n");
    let mut interp = interp();
    // -n stops at the delimiter when it arrives first.
    interp.run_named("<test>", format!("read -n 5 A B C < '{file}'").as_bytes());
    assert_eq!(value(&interp, b"A").as_deref(), Some("a"));
    assert_eq!(value(&interp, b"B").as_deref(), Some("b"));
    assert_eq!(value(&interp, b"C").as_deref(), Some("c"));
    // -N ignores the delimiter: one variable gets all five characters.
    interp.run_named("<test>", format!("read -N 5 D E < '{file}'").as_bytes());
    assert_eq!(value(&interp, b"D").as_deref(), Some("a b c"));
    assert_eq!(value(&interp, b"E").as_deref(), Some(""));

    let records = write_file(&dir, "records.txt", b"a,b,c:d,e\n");
    interp.run_named("<test>", format!("read -d : v1 < '{records}'").as_bytes());
    assert_eq!(value(&interp, b"v1").as_deref(), Some("a,b,c"));
    interp.run_named("<test>", format!("read -d : v2 < '{records}'").as_bytes());
    assert_eq!(value(&interp, b"v2").as_deref(), Some("a,b,c"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_reads_into_an_indexed_array() {
    let _serial = serial();
    let dir = temp_dir("array");
    let file = write_file(&dir, "in.txt", b"a b c\\ d\n");
    let mut interp = interp();
    let status = interp.run_named(
        "<test>",
        format!("read -a list < '{file}'; read -r -a raw extra < '{file}'").as_bytes(),
    );
    assert_eq!(status.exit_code(), 0);
    let list: Vec<String> = match Vars::array(&interp, b"list") {
        Some(baash_lang::ArrayValue::Indexed(elements)) => elements
            .values()
            .map(|value| String::from_utf8_lossy(value).into_owned())
            .collect(),
        other => panic!("expected an indexed array, got {other:?}"),
    };
    assert_eq!(list, vec!["a", "b", "c d"]);
    // `-r` keeps the backslash, so the escaped space splits and leaves the
    // backslash on the field.
    let raw: Vec<String> = match Vars::array(&interp, b"raw") {
        Some(baash_lang::ArrayValue::Indexed(elements)) => elements
            .values()
            .map(|value| String::from_utf8_lossy(value).into_owned())
            .collect(),
        other => panic!("expected an indexed array, got {other:?}"),
    };
    assert_eq!(raw, vec!["a", "b", "c\\", "d"]);
    // Extra names after -a are ignored, not assigned.
    assert_eq!(value(&interp, b"extra"), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn t_zero_only_asks_whether_input_is_available() {
    let _serial = serial();
    let mut interp = interp();
    // /dev/null is readable (EOF counts), so `-t 0` succeeds without assigning.
    let status = interp.run_named("<test>", b"read -t 0 < /dev/null");
    assert_eq!(status.exit_code(), 0);
    assert_eq!(value(&interp, b"REPLY"), None);
}

#[test]
fn u_reads_the_named_descriptor() {
    let _serial = serial();
    let dir = temp_dir("u-fd");
    let file = write_file(&dir, "in.txt", b"hi\n");
    let mut interp = interp();
    // A here-document on fd 3, as the suite writes it.
    let status = interp.run_named("<test>", b"read -u 3 3<<'EOF'\nhi\nEOF");
    assert_eq!(status.exit_code(), 0);
    assert_eq!(value(&interp, b"REPLY").as_deref(), Some("hi"));
    // A real file works the same way.
    interp.run_named("<test>", format!("read -u 3 v 3< '{file}'").as_bytes());
    assert_eq!(value(&interp, b"v").as_deref(), Some("hi"));
    let _ = std::fs::remove_dir_all(&dir);
}
