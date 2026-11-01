//! History expansion is interactive-only.
//!
//! Bash never expands a `-c` string, and a script does not expand even with
//! `set -H` (measured: `bash -H script` and `set -H` inside a script both
//! print `!!`). These tests pin the byte-for-byte script behaviour the
//! conformance suite relies on.

#![forbid(unsafe_code)]

use std::process::Command;

fn run(args: &[&str]) -> (String, String, i32) {
    let output = Command::new(env!("CARGO_BIN_EXE_baash"))
        .args(args)
        .env("LC_ALL", "C")
        .env_remove("HISTFILE")
        .output()
        .expect("run baash");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code().unwrap_or(-1),
    )
}

#[test]
fn a_command_string_does_not_expand_bang() {
    let (stdout, stderr, status) = run(&["--norc", "-c", "echo !!"]);
    assert_eq!(stdout, "!!\n", "stderr: {stderr}");
    assert_eq!(status, 0);
}

#[test]
fn set_h_does_not_enable_expansion_in_a_command_string() {
    let (stdout, stderr, status) = run(&["--norc", "-c", "set -H; echo !!"]);
    assert_eq!(stdout, "!!\n", "stderr: {stderr}");
    assert_eq!(status, 0);
}

#[test]
fn set_h_does_not_enable_expansion_in_a_script() {
    let path =
        std::env::temp_dir().join(format!("baash-histexpand-script-{}.sh", std::process::id()));
    std::fs::write(&path, "set -H\necho !!\n").expect("write script");
    let (stdout, stderr, status) = run(&["--norc", path.to_str().unwrap()]);
    let _ = std::fs::remove_file(&path);
    assert_eq!(stdout, "!!\n", "stderr: {stderr}");
    assert_eq!(status, 0);
}

#[test]
fn interactive_flag_with_command_string_does_not_expand_bang() {
    // bash: `-i -c` is interactive for prompts and `$-`, but a `-c` string is
    // not a history line. Measured against 5.3.20.
    let (stdout, stderr, status) = run(&["--norc", "-i", "-c", "echo !!"]);
    assert_eq!(stdout, "!!\n", "stderr: {stderr}");
    assert_eq!(status, 0);
}
