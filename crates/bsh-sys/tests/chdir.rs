//! `chdir` takes shell bytes and rejects embedded NULs.
//!
//! The working directory is process-wide state, so the single test that
//! changes it lives in its own test binary. The NUL case never calls `chdir`,
//! so it cannot disturb anything either.

use std::io;
use std::os::unix::ffi::OsStrExt;

use bsh_sys::{NulByte, chdir};

#[test]
fn chdir_round_trips_and_rejects_embedded_nul() {
    let original = std::env::current_dir().expect("a current directory");
    let target = std::env::temp_dir();

    chdir(target.as_os_str().as_bytes()).expect("chdir into the temp directory");
    assert_eq!(std::env::current_dir().unwrap(), target);

    chdir(original.as_os_str().as_bytes()).expect("chdir back");
    assert_eq!(std::env::current_dir().unwrap(), original);

    // A NUL is rejected as InvalidInput rather than truncating the path, and
    // it leaves the working directory untouched.
    let error = chdir(b"/tmp/bsh\0hidden").expect_err("a NUL cannot name a directory");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(
        error.get_ref().is_some_and(|inner| inner.is::<NulByte>()),
        "the concrete NulByte error should be preserved"
    );
    assert_eq!(std::env::current_dir().unwrap(), original);

    let missing = chdir(b"/nonexistent-bsh/chdir-target").expect_err("the directory is absent");
    assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    assert_eq!(std::env::current_dir().unwrap(), original);
}
