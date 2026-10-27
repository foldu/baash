//! `read_fd`/`write_all`/`poll_readable`: the byte plumbing the `read`
//! built-in needs, including the property that it must not over-read.

use bsh_sys::{Pipe, poll_readable, read_fd, write_all};
use std::os::fd::AsRawFd;

#[test]
fn reads_exactly_what_was_written_then_eof() {
    let Pipe { read, write } = Pipe::new().expect("pipe");
    let message = b"hello\n";
    assert_eq!(
        write_all(write.as_raw_fd(), message).expect("write"),
        message.len()
    );

    let mut buf = [0u8; 32];
    let n = read_fd(read.as_raw_fd(), &mut buf).expect("read");
    assert_eq!(&buf[..n], message);

    drop(write);
    let n = read_fd(read.as_raw_fd(), &mut buf).expect("read at eof");
    assert_eq!(n, 0, "end of file reports zero bytes");
}

#[test]
fn a_read_does_not_consume_more_than_it_returns() {
    // The `read` built-in must leave the rest of the input for the next
    // reader; a buffered reader that swallowed the remainder would make
    // `read a; read b` lose data.
    let Pipe { read, write } = Pipe::new().expect("pipe");
    write_all(write.as_raw_fd(), b"one two three\n").expect("write");

    let mut first = [0u8; 4];
    let n = read_fd(read.as_raw_fd(), &mut first).expect("read");
    assert_eq!(n, 4);
    assert_eq!(&first[..n], b"one ");

    let mut second = [0u8; 32];
    let n = read_fd(read.as_raw_fd(), &mut second).expect("read");
    assert_eq!(&second[..n], b"two three\n");
}

#[test]
fn poll_reports_readiness() {
    let Pipe { read, write } = Pipe::new().expect("pipe");
    assert!(
        !poll_readable(read.as_raw_fd(), 0).expect("poll"),
        "an empty pipe is not readable"
    );
    write_all(write.as_raw_fd(), b"x").expect("write");
    assert!(
        poll_readable(read.as_raw_fd(), 0).expect("poll"),
        "a written pipe is readable"
    );
}

#[test]
fn poll_times_out_instead_of_blocking() {
    // The writer must stay alive: dropping it gives the reader EOF, which
    // *is* readable, and the test would assert the wrong thing.
    let Pipe { read, write } = Pipe::new().expect("pipe");
    let started = std::time::Instant::now();
    assert!(!poll_readable(read.as_raw_fd(), 20).expect("poll"));
    assert!(started.elapsed() >= std::time::Duration::from_millis(15));
    drop(write);
}
