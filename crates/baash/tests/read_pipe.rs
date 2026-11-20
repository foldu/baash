//! `read` sees every line of a pipe, including one longer than its buffer.
//!
//! `read` pulls a chunk and gives the bytes it did not consume back: on a
//! seekable descriptor by rewinding, and on a pipe by reading a byte at a time,
//! because a pipe cannot return them. Which of the two it does is decided by
//! asking `baash_sys::lseek` whether the descriptor can be rewound.
//!
//! On Windows the CRT cannot answer that question: `_lseeki64(fd, 0, SEEK_CUR)`
//! on a pipe returns 0 instead of failing, so every `read` took the chunked
//! path and dropped the rest of its 4096-byte chunk (and `read -t` never timed
//! out on a pipe, because a "seekable" descriptor is documented not to block).
//! bash-completion's own functions read a command's `--help` this way, so on
//! Windows a long-option menu was empty — `ls --<TAB>` completed nothing.
//!
//! The fixture is deliberately larger than one chunk: a pipe that fits in a
//! single read cannot show the bug, which is why the small-pipe cases passed.

#![forbid(unsafe_code)]

use std::process::Command;

/// 2000 lines of `seq` is about 11 KiB: three chunks, and every line after the
/// first has to come out of the surplus the previous `read` kept.
#[test]
fn read_sees_every_line_of_a_pipe_larger_than_one_chunk() {
    let output = Command::new(env!("CARGO_BIN_EXE_baash"))
        .args([
            "-c",
            "seq 1 2000 | { while IFS= read -r line; do n=$((n+1)); done; printf '%s\\n' \"$n\"; }",
        ])
        .output()
        .expect("spawn baash");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "status {:?}: {stderr}",
        output.status.code()
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "2000\n");
}
