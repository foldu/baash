//! `read_fd`/`write_all`/`poll_readable`: the byte plumbing the `read`
//! built-in needs, including the property that it must not over-read.

use baash_sys::{Pipe, fd::seek, lseek, poll_readable, read_fd, write_all};
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

#[test]
fn lseek_rewinds_surplus_bytes_on_a_file_and_fails_on_a_pipe() {
    // This is how `read` gives back what it buffered but did not consume: a
    // chunked read followed by a rewind must leave the *next* reader at the
    // right offset.
    let path = std::env::temp_dir().join(format!("baash-sys-lseek-{}", std::process::id()));
    std::fs::write(&path, b"first\nsecond\n").expect("write fixture");
    let file = std::fs::File::open(&path).expect("open fixture");
    let fd = file.as_raw_fd();

    assert_eq!(lseek(fd, 0, seek::CUR).expect("tell"), 0);
    let mut buffer = [0u8; 16];
    let n = read_fd(fd, &mut buffer).expect("read");
    let read_all = n;
    assert_eq!(read_all, 13, "one chunk covers the file");
    // Consume six bytes' worth and give the rest back.
    assert_eq!(lseek(fd, -7, seek::CUR).expect("rewind"), 6);
    assert_eq!(lseek(fd, 0, seek::CUR).expect("tell"), 6);
    let n = read_fd(fd, &mut buffer).expect("read after rewind");
    assert_eq!(&buffer[..n], b"second\n");
    drop(file);
    std::fs::remove_file(&path).ok();

    let pipe = Pipe::new().expect("pipe");
    let error = lseek(pipe.read_fd(), 0, seek::CUR).expect_err("pipes are not seekable");
    assert_eq!(error.raw_os_error(), Some(libc::ESPIPE));
}
