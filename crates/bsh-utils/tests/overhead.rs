//! Timing harness for the embedded-utility dispatch.
//!
//! Ignored by default because it measures rather than asserts; run it with
//! `cargo test -p bsh-utils --release --test overhead -- --ignored --nocapture`.

use std::time::Instant;

fn argv(items: &[&str]) -> Vec<Vec<u8>> {
    items.iter().map(|item| item.as_bytes().to_vec()).collect()
}

fn call() -> Option<i32> {
    bsh_utils::run(&argv(&["test", "1", "=", "1"]))
}

#[test]
#[ignore = "timing measurement; run with --release --ignored --nocapture"]
fn dispatch_overhead() {
    const N: u32 = 20_000;

    let start = Instant::now();
    assert_eq!(call(), Some(0));
    println!(
        "cold first call (thread spawn + catalog setup): {:?}",
        start.elapsed()
    );

    let start = Instant::now();
    for _ in 0..N {
        assert_eq!(call(), Some(0));
    }
    let per = start.elapsed() / N;
    println!("dedicated worker, warm: {per:?}/call");

    // A per-call thread is the alternative the pool replaces. An empty body
    // understates it — the real one also parses that utility's catalog — so
    // this is the *floor* for `thread::spawn` + join.
    let start = Instant::now();
    for _ in 0..N {
        std::thread::spawn(|| {}).join().unwrap();
    }
    let per = start.elapsed() / N;
    println!("thread spawn+join, floor: {per:?}/call");

    // Cold start for every other utility, so the cost of one thread each is
    // visible rather than guessed at.
    for name in [
        "printf", "[", "cat", "mkdir", "rm", "ls", "wc", "head", "tail", "tr", "seq", "sleep",
    ] {
        let start = Instant::now();
        let _ = bsh_utils::run(&argv(&[name, "--help"]));
        println!("cold {name}: {:?}", start.elapsed());
    }
}
