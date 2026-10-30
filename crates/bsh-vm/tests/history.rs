//! `history` end to end through the evaluator.
//!
//! These tests only look at status and ring state: the built-in's *output* is
//! covered by unit tests in `bsh-runtime`, with a host that captures bytes in
//! memory. Capturing here would go through `$( … )`, which `dup2`s the
//! process's descriptor 1, and libtest's own progress lines would land in the
//! capture.
//!
//! `fc` and the `-a`/`-n`/`-r`/`-w`/`-p`/`-s` options are deliberately absent:
//! they existed to move lines between the ring and a text file. The last two
//! tests pin that removal down, so a future "compatibility" patch has to argue
//! with them.

#![forbid(unsafe_code)]

use bsh_runtime::{ShellHost, Status};
use bsh_vm::Interp;

fn shell() -> Interp {
    let mut interp = Interp::new(false);
    interp.set_arg0("bsh");
    interp
}

fn run(interp: &mut Interp, script: &str) -> Status {
    interp.run_named("<history-test>", script.as_bytes())
}

/// Record lines the way the interactive loop does, without a database.
fn seed(interp: &mut Interp, lines: &[&str]) {
    for line in lines {
        interp.state().history.push(line.as_bytes());
    }
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
fn a_plain_shell_records_nothing() {
    let mut interp = shell();
    assert_eq!(run(&mut interp, "echo one"), Status(0));
    assert!(texts(&mut interp).is_empty());
    assert_eq!(run(&mut interp, "history"), Status(0));
}

#[test]
fn history_c_clears_the_list() {
    let mut interp = shell();
    seed(&mut interp, &["a", "b"]);
    assert_eq!(run(&mut interp, "history -c"), Status(0));
    assert!(texts(&mut interp).is_empty());
    assert_eq!(interp.state().history.next_number(), 1);
}

#[test]
fn history_d_deletes_by_number_and_negative_offset() {
    let mut interp = shell();
    seed(&mut interp, &["a", "b", "c"]);
    assert_eq!(run(&mut interp, "history -d 2"), Status(0));
    assert_eq!(texts(&mut interp), ["a", "c"]);
    // Numbers are not reassigned: the survivor keeps the number it printed.
    assert_eq!(numbers(&mut interp), [1, 3]);

    let mut interp = shell();
    seed(&mut interp, &["a", "b", "c"]);
    assert_eq!(run(&mut interp, "history -d -1"), Status(0));
    assert_eq!(texts(&mut interp), ["a", "b"]);

    assert_eq!(run(&mut interp, "history -d 99"), Status(1));
    assert_eq!(run(&mut interp, "history -d 0"), Status(1));
    assert_eq!(run(&mut interp, "history -d zz"), Status(1));
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
fn a_submitted_line_is_numbered_and_current() {
    let mut interp = shell();
    interp.state().history.push(b"echo one\n");
    assert!(interp.state().history.is_current());
    assert_eq!(numbers(&mut interp), [1]);
    interp.state().history.finish(Some(0), Some(1));
    assert!(!interp.state().history.is_current());
    interp.state().history.push(b"echo two");
    assert_eq!(numbers(&mut interp), [1, 2]);
}

#[test]
fn the_file_options_are_gone() {
    for option in ["-a", "-n", "-r", "-w", "-p", "-s"] {
        let mut interp = shell();
        seed(&mut interp, &["echo a"]);
        assert_eq!(
            run(&mut interp, &format!("history {option}")),
            Status(2),
            "`history {option}` must not be accepted"
        );
        assert_eq!(
            texts(&mut interp),
            ["echo a"],
            "`history {option}` must not change the list"
        );
    }
}

#[test]
fn fc_is_not_a_builtin_any_more() {
    let mut interp = shell();
    seed(&mut interp, &["echo a"]);
    assert_eq!(run(&mut interp, "fc -l"), Status(127));
    assert_eq!(run(&mut interp, "type fc"), Status(1));
}
