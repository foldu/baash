//! Per-user directories: XDG on Unix, the platform's own folders on Windows.
//!
//! Only the two bases the shell itself needs are here. Both follow the [XDG
//! Base Directory Specification][xdg] on Unix:
//!
//! * an environment variable names the base when it holds a non-empty
//!   **absolute** path — a relative value is ignored, which is the rule the
//!   specification states and the one a hand-rolled `$XDG_DATA_HOME` check
//!   usually gets wrong, because it would put a database in the current
//!   directory;
//! * otherwise the specification's default under `$HOME` applies.
//!
//! A machine with no `$HOME` yields `None` rather than a path relative to
//! wherever the shell happens to be running; callers read that as "nowhere to
//! store this" (no config file, no history persistence).
//!
//! Windows has no XDG: configuration is the roaming profile and data is the
//! local one, which is the same split XDG draws between `config` and `data`.
//!
//! [xdg]: https://specifications.freedesktop.org/basedir-spec/latest/

use std::path::PathBuf;

/// The current user's home directory.
///
/// `$HOME` on Unix, `%USERPROFILE%` on Windows. An empty value counts as
/// unset: the shell must not turn it into a relative path.
#[cfg(unix)]
pub fn home_dir() -> Option<PathBuf> {
    non_empty("HOME").map(PathBuf::from)
}

/// The current user's home directory.
///
/// `%USERPROFILE%` on Windows; `$HOME` is a Cygwin/MSYS convention that a
/// native program should not read.
#[cfg(windows)]
pub fn home_dir() -> Option<PathBuf> {
    non_empty("USERPROFILE").map(PathBuf::from)
}

/// The home directory to use when the shell's own `HOME` variable is unset.
///
/// A POSIX shell has no fallback: with no `HOME`, `cd` fails and `~` stays
/// literal, which is what bash does. A native Windows process, on the other
/// hand, is started with no `HOME` at all — the profile lives in
/// `%USERPROFILE%` — so there the profile directory stands in for it.
#[cfg(unix)]
pub fn fallback_home() -> Option<Vec<u8>> {
    None
}

/// `%USERPROFILE%`, which is what a shell should use when `HOME` is unset.
#[cfg(windows)]
pub fn fallback_home() -> Option<Vec<u8>> {
    crate::os::own_home()
}

/// `$XDG_CONFIG_HOME`, or `~/.config`.
#[cfg(unix)]
pub fn config_home() -> Option<PathBuf> {
    xdg("XDG_CONFIG_HOME").or_else(|| home_dir().map(|home| home.join(".config")))
}

/// `$XDG_CONFIG_DIRS`, or `/etc/xdg`.
///
/// The search path for system-wide configuration, in order. Entries are
/// `:`-separated and only absolute paths count, as the specification says; an
/// empty result means the specification's default, not "no system
/// configuration".
#[cfg(unix)]
pub fn config_dirs() -> Vec<PathBuf> {
    let Some(value) = non_empty("XDG_CONFIG_DIRS") else {
        return vec![PathBuf::from("/etc/xdg")];
    };
    let dirs: Vec<PathBuf> = std::env::split_paths(&value)
        .filter(|dir| dir.is_absolute())
        .collect();
    if dirs.is_empty() {
        vec![PathBuf::from("/etc/xdg")]
    } else {
        dirs
    }
}

/// No system-wide configuration directory on Windows.
///
/// There is no `%XDG_CONFIG_DIRS%` equivalent, and the shell's defaults are
/// built in, so a distributor has nothing to install here.
#[cfg(windows)]
pub fn config_dirs() -> Vec<PathBuf> {
    Vec::new()
}

/// `%APPDATA%`, or `~/AppData/Roaming`.
#[cfg(windows)]
pub fn config_home() -> Option<PathBuf> {
    non_empty("APPDATA")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join("AppData").join("Roaming")))
}

/// `$XDG_DATA_HOME`, or `~/.local/share`.
#[cfg(unix)]
pub fn data_home() -> Option<PathBuf> {
    xdg("XDG_DATA_HOME").or_else(|| home_dir().map(|home| home.join(".local").join("share")))
}

/// `$XDG_CACHE_HOME`, or `~/.cache`.
///
/// For data the shell can regenerate: the unpacked bash-completion library
/// lives here rather than under [`data_home`], because losing a cache costs one
/// extraction and losing data costs a user their history.
#[cfg(unix)]
pub fn cache_home() -> Option<PathBuf> {
    xdg("XDG_CACHE_HOME").or_else(|| home_dir().map(|home| home.join(".cache")))
}

/// `%LOCALAPPDATA%`, or `~/AppData/Local`.
#[cfg(windows)]
pub fn data_home() -> Option<PathBuf> {
    non_empty("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join("AppData").join("Local")))
}

/// `%LOCALAPPDATA%`, or `~/AppData/Local`.
///
/// Windows has one local directory for data and caches both, so this is the
/// same place [`data_home`] names; the XDG split has no second half there.
#[cfg(windows)]
pub fn cache_home() -> Option<PathBuf> {
    data_home()
}

/// An XDG base directory variable, when the spec says it applies.
///
/// Unset, empty and relative values are all "not set": the spec reserves the
/// defaults for exactly that case, and honouring a relative path would put the
/// shell's files in its working directory.
#[cfg(unix)]
fn xdg(name: &str) -> Option<PathBuf> {
    non_empty(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

fn non_empty(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}
