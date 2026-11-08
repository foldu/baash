//! The bash-completion library the binary carries.
//!
//! `-i -c` is an interactive shell without a terminal: it reads the startup
//! files, which is where the library is unpacked and offered, and then runs
//! one command. That makes it the cheapest place to prove the embedding works
//! on any platform — the Tab path itself needs a pty and is
//! `interactive_complete.rs`.
//!
//! The state is a fresh `HOME`, cache and configuration, and an empty
//! `XDG_CONFIG_DIRS`, so the run cannot pick up the machine's startup file (a
//! developer on a machine with an installed `baash` would otherwise test that
//! copy, which is a different `config.sh`).

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

/// The unpacked library's directory: the cache has one tree, named by the
/// digest of its provenance.
fn unpacked(cache: &Path) -> Option<PathBuf> {
    std::fs::read_dir(cache.join("baash").join("bash-completion"))
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.join("bash_completion").is_file())
}

#[test]
fn an_interactive_shell_unpacks_and_sources_the_library() {
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
. "$BAASH_EMBEDDED_BASH_COMPLETION"
printf 'status=%s\n' "$?"
declare -F _comp_load >/dev/null && echo loader
declare -F _comp_initialize >/dev/null && echo initialize
"#,
    );
    let text = stdout(&output);
    if text.contains("skipped") {
        // A build without `vendor/bash-completion/` carries no library; that
        // is a supported build, not a failure (the module doc says so).
        eprintln!("skipped: this binary carries no bash-completion tree");
        return;
    }
    assert_eq!(
        output.status.code(),
        Some(0),
        "the shell must exit cleanly; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
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

    let tree = unpacked(&cache).unwrap_or_else(|| {
        panic!(
            "the library was not unpacked into {}: {text}",
            cache.display()
        )
    });
    assert!(
        tree.join("completions-core").is_dir(),
        "the completions come with the framework: {}",
        tree.display()
    );
    assert!(
        tree.join("PROVENANCE").is_file(),
        "the tree records where it came from: {}",
        tree.display()
    );

    // A second start finds the marker and leaves the tree alone: a file put
    // there by hand (or by the shell itself) stays.
    let sentinel = tree.join("kept-by-the-test");
    std::fs::write(&sentinel, b"keep me").expect("write into the tree");
    let again = interactive(&home, &cache, &config.join("absent"), "echo second");
    assert_eq!(again.status.code(), Some(0));
    assert!(
        sentinel.is_file(),
        "an existing extraction must not be overwritten"
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
