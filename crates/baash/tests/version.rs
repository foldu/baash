//! `--version` names the build, not just the package.
//!
//! Every build of this tree is `0.0.0`, so the parenthesised label is the only
//! thing that tells two binaries apart, and it is what a bug report quotes.
//! `crates/baash/build.rs` computes it: `BAASH_GIT_VERSION` inside the Nix
//! sandbox, which has no `.git`, and `git describe` for a `cargo build` in a
//! checkout. Neither is reachable from a test, so the second case re-runs the
//! same command and compares — which is also the only way to notice that the
//! label went stale.

use std::process::{Command, Output};

/// `baash --version`, required to succeed.
fn version() -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_baash"))
        .arg("--version")
        .output()
        .expect("spawn baash");
    assert_success(&output);
    String::from_utf8(output.stdout).expect("--version prints text")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "--version exits 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The parenthesised label on a `--version` line.
fn label(line: &str) -> &str {
    let line = line.trim_end();
    let Some((head, tail)) = line.rsplit_once(" (") else {
        panic!("--version has no parenthesised label: {line:?}");
    };
    assert!(
        head.starts_with("baash "),
        "--version does not start with the program name: {line:?}"
    );
    let Some(label) = tail.strip_suffix(')') else {
        panic!("--version has an unterminated label: {line:?}");
    };
    assert!(!label.is_empty(), "--version has an empty label: {line:?}");
    assert!(
        !label.contains(char::is_whitespace),
        "--version has a label with a space in it: {line:?}"
    );
    label
}

/// The same label minus the dirty marker, which describes the tree at build
/// time and may legitimately differ from the tree the test runs in: a document
/// edited after the build is not watched (`build.rs` says why).
fn committed(label: &str) -> &str {
    label.strip_suffix("-dirty").unwrap_or(label)
}

#[test]
fn version_is_one_line_with_a_commit_on_it() {
    let line = version();
    assert_eq!(line.lines().count(), 1, "--version is one line: {line:?}");
    // Whatever else it says, it names a commit or admits it cannot.
    let _ = label(&line);
}

#[test]
fn version_names_the_commit_this_checkout_is_on() {
    let line = version();
    let label = label(&line);

    let described = Command::new("git")
        .arg("-C")
        .arg(env!("CARGO_MANIFEST_DIR"))
        .args(["describe", "--always", "--dirty", "--abbrev=7"])
        .output();
    let Ok(described) = described else {
        // No `git` to compare against; `version_is_one_line_with_a_commit_on_it`
        // still holds the line's shape.
        return;
    };
    if !described.status.success() {
        return;
    }
    let expected = String::from_utf8_lossy(&described.stdout)
        .trim()
        .to_string();
    assert_eq!(
        committed(label),
        committed(&expected),
        "the label names a different commit than the checkout: the binary is \
         stale, or `crates/baash/build.rs` took BAASH_GIT_VERSION from the \
         environment"
    );
}
