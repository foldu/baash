//! `history` and `fc` end to end through the evaluator.
//!
//! The built-ins write through the VM's descriptor table. These tests capture
//! that output with command substitution, or inspect the ring through
//! `ShellHost::state()`, rather than redirecting the process's stdout: the
//! whole test binary shares one process, and a `> file` here would capture
//! libtest's own output from another thread. The exact formats are bash
//! 5.3.20's, measured under `bash --norc`; see
//! `crates/bsh-runtime/src/builtins/history.rs`.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use bsh_lang::Vars;
use bsh_runtime::{ShellHost, Status};
use bsh_vm::Interp;

fn shell() -> Interp {
    let mut interp = Interp::new(false);
    interp.set_arg0("bsh");
    interp
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bsh-vm-history-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn run(interp: &mut Interp, script: &str) -> Status {
    interp.run_named("<history-test>", script.as_bytes())
}

fn text(interp: &Interp, name: &[u8]) -> String {
    String::from_utf8_lossy(Vars::get(interp, name).unwrap_or_default()).into_owned()
}

fn read(path: &std::path::Path) -> String {
    String::from_utf8_lossy(&std::fs::read(path).expect("read output")).into_owned()
}

/// Capture a command's stdout in the shell variable `out`.
///
/// Command substitution strips trailing newlines, so expectations omit the
/// final `\n`. A sentinel distinguishes "printed nothing" from a lost capture.
fn capture(interp: &mut Interp, command: &str) -> String {
    let status = run(interp, &format!("out=$({command}; echo .)"));
    assert!(
        status.is_success(),
        "capturing `{command}` failed: {status:?}"
    );
    let captured = text(interp, b"out");
    if captured == "." {
        return String::new();
    }
    let captured = captured.strip_suffix("\n.").unwrap_or_else(|| {
        panic!("captured output of `{command}` did not end with the sentinel: {captured:?}")
    });
    captured.to_owned()
}

fn texts(interp: &mut Interp) -> Vec<String> {
    interp
        .state()
        .history
        .commands()
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect()
}

fn numbers(interp: &mut Interp) -> Vec<u32> {
    interp
        .state()
        .history
        .entries()
        .iter()
        .map(|entry| entry.number())
        .collect()
}

#[test]
fn history_without_entries_prints_nothing_non_interactively() {
    let mut interp = shell();
    assert_eq!(capture(&mut interp, "history"), "");
    // A plain shell does not record the commands it runs.
    assert_eq!(capture(&mut interp, "echo one; history"), "one");
}

#[test]
fn history_lists_five_wide_numbers_and_two_spaces() {
    let mut interp = shell();
    run(
        &mut interp,
        "history -s echo a; history -s echo b; history -s echo c",
    );
    assert_eq!(
        capture(&mut interp, "history 2"),
        "    2  echo b\n    3  echo c"
    );
    assert_eq!(
        capture(&mut interp, "history"),
        "    1  echo a\n    2  echo b\n    3  echo c"
    );
}

#[test]
fn history_c_clears_the_list() {
    let mut interp = shell();
    run(&mut interp, "history -s a; history -s b");
    assert_eq!(run(&mut interp, "history -c"), Status(0));
    assert!(texts(&mut interp).is_empty());
    assert_eq!(interp.state().history.next_number(), 1);
    assert_eq!(run(&mut interp, "history"), Status(0));
}

#[test]
fn history_d_deletes_by_number_and_negative_offset() {
    let mut interp = shell();
    run(&mut interp, "history -s a; history -s b; history -s c");
    assert_eq!(run(&mut interp, "history -d 2"), Status(0));
    assert_eq!(texts(&mut interp), ["a", "c"]);
    assert_eq!(numbers(&mut interp), [1, 2]);

    let mut interp = shell();
    run(&mut interp, "history -s a; history -s b; history -s c");
    assert_eq!(run(&mut interp, "history -d -1"), Status(0));
    assert_eq!(texts(&mut interp), ["a", "b"]);

    assert_eq!(run(&mut interp, "history -d 99"), Status(1));
    assert_eq!(run(&mut interp, "history -d 0"), Status(1));
    assert_eq!(run(&mut interp, "history -d zz"), Status(1));
}

#[test]
fn history_w_writes_and_history_r_reads_the_file() {
    let dir = temp_dir("rw");
    let file = dir.join("hist");
    let mut interp = shell();
    run(&mut interp, "history -s a; history -s b");
    run(
        &mut interp,
        &format!("HISTFILE='{}' history -w", file.display()),
    );
    assert_eq!(read(&file), "a\nb\n");

    assert_eq!(run(&mut interp, "history -c"), Status(0));
    run(
        &mut interp,
        &format!("HISTFILE='{}' history -r", file.display()),
    );
    assert_eq!(texts(&mut interp), ["a", "b"]);
    assert_eq!(capture(&mut interp, "history"), "    1  a\n    2  b");
}

#[test]
fn history_a_appends_only_new_lines() {
    let dir = temp_dir("append");
    let file = dir.join("hist");
    std::fs::write(&file, "old\n").unwrap();
    let mut interp = shell();
    run(
        &mut interp,
        &format!(
            "HISTFILE='{}' history -s a; HISTFILE='{}' history -a",
            file.display(),
            file.display()
        ),
    );
    assert_eq!(read(&file), "old\na\n");
}

#[test]
fn history_n_reads_only_lines_not_read_before() {
    let dir = temp_dir("read-new");
    let first = dir.join("one");
    let second = dir.join("two");
    std::fs::write(&first, "a\nb\n").unwrap();
    std::fs::write(&second, "a\nb\nc\nd\n").unwrap();
    let mut interp = shell();
    run(
        &mut interp,
        &format!(
            "HISTFILE='{}' history -r; HISTFILE='{}' history -n",
            first.display(),
            second.display()
        ),
    );
    assert_eq!(texts(&mut interp), ["a", "b", "c", "d"]);
}

#[test]
fn history_rejects_bad_operands_with_status_2() {
    let mut interp = shell();
    assert_eq!(run(&mut interp, "history -z"), Status(2));
    assert_eq!(run(&mut interp, "history not-a-number"), Status(2));
    assert_eq!(run(&mut interp, "history 1 2"), Status(2));
    assert_eq!(run(&mut interp, "history -d"), Status(2));
}

#[test]
fn history_s_stores_joined_arguments_and_p_prints_them() {
    let mut interp = shell();
    run(&mut interp, "history -s echo hi");
    assert_eq!(texts(&mut interp), ["echo hi"]);
    assert_eq!(capture(&mut interp, "history"), "    1  echo hi");

    assert_eq!(capture(&mut interp, "history -p 'a  b' c"), "a  b\nc");
    // `-p` does not store.
    assert_eq!(texts(&mut interp), ["echo hi"]);
}

#[test]
fn histcontrol_ignoredups_applies_to_history_s() {
    let mut interp = shell();
    run(
        &mut interp,
        "HISTCONTROL=ignoredups; history -s x; history -s x; history -s y",
    );
    assert_eq!(texts(&mut interp), ["x", "y"]);
}

#[test]
fn histignore_skips_matching_lines() {
    let mut interp = shell();
    run(
        &mut interp,
        "HISTIGNORE='echo*'; history -s echo a; history -s ls",
    );
    assert_eq!(texts(&mut interp), ["ls"]);
}

#[test]
fn histsize_shrinks_the_list_on_the_next_command() {
    let mut interp = shell();
    run(&mut interp, "history -s a; history -s b; history -s c");
    run(&mut interp, "HISTSIZE=2");
    // There is no REPL loop in this test to sync at a command boundary, so
    // stand in for it the way the interactive path does.
    interp.state().sync_history();
    assert_eq!(texts(&mut interp), ["b", "c"]);
    assert_eq!(numbers(&mut interp), [2, 3]);
}

#[test]
fn fc_l_prints_number_tab_space_and_honours_n_and_r() {
    let mut interp = shell();
    run(&mut interp, "history -s echo a; history -s echo b");

    assert_eq!(capture(&mut interp, "fc -l"), "1\t echo a\n2\t echo b");
    assert_eq!(capture(&mut interp, "fc -ln"), "\t echo a\n\t echo b");
    assert_eq!(capture(&mut interp, "fc -lr"), "2\t echo b\n1\t echo a");
}

#[test]
fn fc_l_ranges_are_inclusive_and_reverse_when_reversed() {
    let mut interp = shell();
    run(
        &mut interp,
        "history -s echo a; history -s echo b; history -s echo c",
    );
    assert_eq!(capture(&mut interp, "fc -l 2 3"), "2\t echo b\n3\t echo c");
    assert_eq!(capture(&mut interp, "fc -l 3 2"), "3\t echo c\n2\t echo b");
    assert_eq!(capture(&mut interp, "fc -l -2"), "2\t echo b\n3\t echo c");
}

#[test]
fn fc_l_empty_history_is_success_and_unknown_prefix_fails() {
    let mut interp = shell();
    assert_eq!(run(&mut interp, "fc -l"), Status(0));
    run(&mut interp, "history -s echo a");
    assert_eq!(run(&mut interp, "fc -l zzz"), Status(1));
}

#[test]
fn fc_s_substitutes_prints_and_reexecutes() {
    let mut interp = shell();
    run(&mut interp, "history -s 'v=1'");
    // The command substitution runs the whole sequence in a subshell, which is
    // what makes `$v` and the ring observable together without touching the
    // test process's stdout.
    assert_eq!(
        capture(
            &mut interp,
            "history -s 'v=1'; fc -s 1=2 1; echo \"v=$v\"; history"
        ),
        // `history -s` adds a second copy of `v=1`; `fc` removes the last entry
        // and stores the re-executed command there.
        "v=2\nv=2\n    1  v=1\n    2  v=2"
    );
    // The parent ring was not touched by the subshell.
    assert_eq!(texts(&mut interp), ["v=1"]);
}

#[test]
fn fc_s_without_history_fails_and_plain_fc_is_not_implemented() {
    let mut interp = shell();
    assert_eq!(run(&mut interp, "fc -s"), Status(1));
    assert_eq!(run(&mut interp, "fc"), Status(2));
    assert_eq!(run(&mut interp, "fc -l -s"), Status(2));
}

#[test]
fn executed_commands_are_unrecorded_but_history_s_is_not() {
    let mut interp = shell();
    run(&mut interp, "echo one > /dev/null; history -s explicit");
    assert_eq!(texts(&mut interp), ["explicit"]);
    assert_eq!(numbers(&mut interp), [1]);
}
