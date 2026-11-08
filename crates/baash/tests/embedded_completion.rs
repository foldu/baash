//! The bash-completion library the binary can carry, end to end.
//!
//! `-i -c` is an interactive shell without a terminal: it reads the startup
//! files, which is where the library is mounted and offered, and then runs one
//! command. That makes it the cheapest place to prove the whole chain —
//! `source`, the file predicates and pathname expansion all reach into the
//! mounted tree, and nothing is written to disk. The Tab path itself needs a
//! pty and is `interactive_complete.rs`.
//!
//! A binary built without `vendor/bash-completion/` (or without the feature,
//! outside Windows) carries no tree; the tests report the skip, the same way
//! the pty tests do.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A fresh directory under the system temporary directory.
fn dir(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("baash-embedded-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("create the test directory");
    path
}

/// An interactive shell with a clean environment, running `script`.
fn interactive(home: &Path, cache: &Path, config: &Path, script: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_baash"))
        .args(["-i", "-c", script])
        .env_clear()
        .env("HOME", home)
        .env("XDG_CACHE_HOME", cache)
        .env("XDG_CONFIG_DIRS", config)
        .output()
        .expect("spawn baash")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn an_interactive_shell_mounts_and_sources_the_library() {
    let home = dir("home");
    let cache = dir("cache");
    let config = dir("config");
    let output = interactive(
        &home,
        &cache,
        &config.join("absent"),
        r#"printf 'var=%s\n' "${BAASH_EMBEDDED_BASH_COMPLETION-}"
if [[ -z ${BAASH_EMBEDDED_BASH_COMPLETION-} ]]; then
    echo skipped
    exit 0
fi
library=$BAASH_EMBEDDED_BASH_COMPLETION
[[ -e $library ]] && echo exists
[[ -d ${library%/*} ]] && echo directory
[[ -r $library ]] && echo readable
for startup in "${library%/*}"/startup-core/*.bash; do
    printf 'startup:%s\n' "${startup##*/}"
done
. "$library"
printf 'status=%s\n' "$?"
declare -F _comp_load >/dev/null && echo loader
declare -F _comp_initialize >/dev/null && echo initialize
"#,
    );
    let text = stdout(&output);
    if text.contains("skipped") {
        eprintln!("skipped: this binary carries no bash-completion tree");
        return;
    }
    assert_eq!(
        output.status.code(),
        Some(0),
        "the shell must exit cleanly; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains("var=/__baash__/bash-completion/bash_completion"),
        "the mount is named for the startup files: {text}"
    );
    // The three questions the library asks of its own location, answered from
    // the table rather than the file system.
    assert!(
        text.contains("exists"),
        "-e must see the mounted file: {text}"
    );
    assert!(
        text.contains("directory"),
        "-d must see the mounted directory: {text}"
    );
    assert!(
        text.contains("readable"),
        "-r must see the mounted file: {text}"
    );
    // Pathname expansion lists the mounted directory.
    assert!(
        text.contains("startup:000_bash_completion_compat.bash"),
        "the startup glob must find the mounted file: {text}"
    );
    assert!(text.contains("status=0"), "sourcing failed: {text}");
    assert!(
        text.contains("loader"),
        "the 2.18 loader must be defined: {text}"
    );
    assert!(
        text.contains("initialize"),
        "the v2 compatibility layer must be sourced too: {text}"
    );
    // The point of the mount: the library never becomes files on disk.
    assert!(
        !cache.join("baash").exists(),
        "the shell wrote a cache directory: {}",
        cache.display()
    );
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&cache);
    let _ = std::fs::remove_dir_all(&config);
}

#[test]
fn the_opt_out_leaves_completion_as_it_was() {
    let home = dir("home-off");
    let cache = dir("cache-off");
    let config = dir("config-off");
    let script = r#"declare -F _comp_load >/dev/null && echo sourced || echo not-sourced"#;
    let output = Command::new(env!("CARGO_BIN_EXE_baash"))
        .args(["-i", "-c", script])
        .env_clear()
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", &cache)
        .env("XDG_CONFIG_DIRS", config.join("absent"))
        .env("BAASH_NO_BASH_COMPLETION", "1")
        .output()
        .expect("spawn baash");
    let text = stdout(&output);
    assert_eq!(output.status.code(), Some(0));
    assert!(
        text.contains("not-sourced"),
        "`BAASH_NO_BASH_COMPLETION=1` must source nothing: {text}"
    );
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&cache);
    let _ = std::fs::remove_dir_all(&config);
}
