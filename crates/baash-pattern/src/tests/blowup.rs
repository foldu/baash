//! The hard requirement: no exponential blow-up.
//!
//! A backtracking matcher explores every way of splitting every `*`; with 30
//! stars that is 200³⁰ possibilities for a 200-character subject. The NFA
//! simulation visits each (instruction, position) pair once. Each test runs
//! under a watchdog so a regression fails instead of hanging CI.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::{Flags, matches, prefix_end};

const LIMIT: Duration = Duration::from_secs(5);

/// Run `f` on a worker thread and fail if it takes longer than [`LIMIT`].
fn within<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(LIMIT) {
        Ok(value) => value,
        Err(_) => panic!("{what} did not finish within {LIMIT:?}; the matcher is backtracking"),
    }
}

#[test]
fn thirty_stars_against_two_hundred_as() {
    let pattern = "a*".repeat(30) + "b";
    let text = "a".repeat(200);
    let pattern = pattern.into_bytes();
    let text = text.into_bytes();
    let hit = within("30 `a*` groups vs 200 a's", move || {
        matches(&pattern, &text, Flags::default())
    });
    assert!(!hit, "there is no `b`, so the pattern cannot match");
}

#[test]
fn thirty_stars_prefix_shortest_and_longest() {
    // Each `a*` must consume at least one `a`, so the shortest prefix is 30
    // characters; the longest is the whole string.
    let pattern = "a*".repeat(30).into_bytes();
    let text = "a".repeat(200).into_bytes();
    let (short, long) = within("30 `a*` groups prefix", move || {
        (
            prefix_end(&pattern, &text, false, Flags::default()),
            prefix_end(&pattern, &text, true, Flags::default()),
        )
    });
    assert_eq!(short, Some(30));
    assert_eq!(long, Some(200));
}

#[test]
fn repeated_extglob_against_two_hundred_as() {
    let pattern = "*(a)".repeat(20) + "b";
    let text = "a".repeat(200);
    let pattern = pattern.into_bytes();
    let text = text.into_bytes();
    let hit = within("20 `*(a)` groups vs 200 a's", move || {
        matches(&pattern, &text, Flags::default())
    });
    assert!(!hit);
}

#[test]
fn negated_group_inside_a_star_terminates() {
    // `!(a)` can match empty, so the star has an epsilon cycle; the closure
    // must reach a fixed point rather than loop or branch forever.
    let flags = Flags {
        extglob: true,
        ..Flags::default()
    };
    assert!(matches(b"*(!(a))", b"aaa", flags));
    assert!(matches(b"*(!(a))", b"", flags));
    assert!(matches(b"!(!(!(!(a))))", b"a", flags));
    let text = "a".repeat(200).into_bytes();
    let pattern = b"*(!(a))";
    let hit = within("`*(!(a))` vs 200 a's", move || {
        matches(pattern, &text, flags)
    });
    assert!(hit);
}
