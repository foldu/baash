//! Where the history database goes when `BAASH_HISTORY_DB` is not set.
//!
//! The default is the platform's per-user data directory, which is XDG on
//! Unix. The environment is process-global, so this file holds one test: no
//! other test runs in this binary, and the other test binaries are separate
//! processes.

use baash_history::default_path;

/// Set an environment variable for this process.
///
/// `std::env::set_var` is `unsafe` in edition 2024 because a concurrent reader
/// would race; the test binary runs this as its only test, so nothing else
/// reads the environment while it changes.
fn set(name: &str, value: &str) {
    unsafe { std::env::set_var(name, value) };
}

fn remove(name: &str) {
    unsafe { std::env::remove_var(name) };
}

#[test]
fn the_history_database_follows_the_xdg_data_directory() {
    let root = std::env::temp_dir().join(format!("baash-history-dir-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("create the test directory");
    let home = root.join("home");
    let data = root.join("data");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&data).expect("create data");

    remove("BAASH_HISTORY_DB");
    set("HOME", &home.to_string_lossy());

    // `$XDG_DATA_HOME` wins over the default under `$HOME`.
    set("XDG_DATA_HOME", &data.to_string_lossy());
    assert_eq!(
        default_path(),
        Some(data.join("baash").join("history.db")),
        "an absolute XDG_DATA_HOME is the base directory"
    );

    // The specification only honours an absolute path: a relative one is the
    // same as unset, so the database cannot land in the working directory.
    set("XDG_DATA_HOME", "relative/data");
    assert_eq!(
        default_path(),
        Some(
            home.join(".local")
                .join("share")
                .join("baash")
                .join("history.db")
        ),
        "a relative XDG_DATA_HOME is ignored"
    );

    // An empty variable is not a base directory either.
    set("XDG_DATA_HOME", "");
    assert_eq!(
        default_path(),
        Some(
            home.join(".local")
                .join("share")
                .join("baash")
                .join("history.db")
        )
    );

    // The override replaces the whole path, and an empty one turns
    // persistence off rather than falling back to a location the user did not
    // ask for.
    set("XDG_DATA_HOME", &data.to_string_lossy());
    let explicit = root.join("elsewhere").join("history.db");
    set("BAASH_HISTORY_DB", &explicit.to_string_lossy());
    assert_eq!(default_path(), Some(explicit));
    set("BAASH_HISTORY_DB", "");
    assert_eq!(default_path(), None, "an empty override disables history");

    std::fs::remove_dir_all(&root).ok();
}
