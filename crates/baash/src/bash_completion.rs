//! The bash-completion library the binary can carry.
//!
//! A `complete -F` function is only half a completion: the useful ones are
//! written against bash-completion's helpers (`_filedir`, `_init_completion`,
//! `_comp_compgen_*`), and that library is distribution data. Windows has no
//! package manager that installs it where a shell can find it, so a bare
//! `baash.exe` would complete nothing those scripts recognise.
//!
//! * The tree is compiled into the binary and **mounted in memory** at
//!   [`MOUNT`], so `source`, `test` and pathname expansion read it without
//!   touching the disk (`baash_runtime::vfs`). Writing it out and reading it
//!   back was the first design; on Windows it cost 475 ms of the first start
//!   (1106 files for Defender to scan) and a cache directory to version and
//!   trust.
//! * The path is handed to the startup file as `BAASH_EMBEDDED_BASH_COMPLETION`
//!   before the startup files run; `config/config.sh` tries a copy the system
//!   installed first and this one last, so a distribution's library wins.
//! * The tree is **not vendored**. `flake.nix` copies the pinned nixpkgs
//!   `bash-completion` into `vendor/bash-completion/` before the build (the dev
//!   shell does the same for `cargo test`), and a build without the folder is
//!   not an error: `allow_missing` makes it a binary that carries no library.
//! * It is compiled in only when the feature is on. The feature is on for
//!   Windows — where the executable is the whole installation — and off
//!   elsewhere, where a package manager installs bash-completion and a smaller
//!   binary is the better default. `--features embedded-bash-completion` is for
//!   a static or self-contained build elsewhere.

use std::borrow::Cow;

use baash_runtime::ShellHost;
use baash_vm::Interp;

/// The path the tree is mounted at inside the shell.
///
/// A path, not a scheme: the library does path arithmetic on its own location
/// (`${BASH_SOURCE%/*}`, `/completions-core`, globbing `startup-core/*.bash`),
/// and a path is the one shape that arithmetic already understands.
pub const MOUNT: &[u8] = b"/__baash__/bash-completion";

/// The file `config.sh` sources; its directory is the library's home.
const LIBRARY: &str = "bash_completion";

// Windows gets the tree without a flag because there is nothing else to
// install it with; everywhere else it is the `embedded-bash-completion`
// feature, so a package manager's copy is the only one in a normal build.

/// The mounted tree, when this build has one.
#[cfg(any(windows, feature = "embedded-bash-completion"))]
#[derive(rust_embed::Embed)]
#[folder = "../../vendor/bash-completion/"]
#[allow_missing = true]
struct Library;

/// Every file of the tree, paths relative to [`MOUNT`].
#[cfg(any(windows, feature = "embedded-bash-completion"))]
fn files() -> Vec<(Vec<u8>, Cow<'static, [u8]>)> {
    Library::iter()
        .filter_map(|name| Library::get(&name).map(|file| (name.as_bytes().to_vec(), file.data)))
        .collect()
}

#[cfg(not(any(windows, feature = "embedded-bash-completion")))]
fn files() -> Vec<(Vec<u8>, Cow<'static, [u8]>)> {
    Vec::new()
}

/// Mount the library in `shell` and name its `bash_completion`.
///
/// `None` means this binary carries no tree (the default outside Windows), or
/// the tree is there but the folder was empty at build time. The shell then
/// behaves as it did before: `config.sh` finds a system library or nothing.
pub fn install(shell: &mut Interp) -> Option<Vec<u8>> {
    let files = files();
    if files.is_empty() {
        return None;
    }
    shell.state().vfs.mount(MOUNT, files);
    let mut path = MOUNT.to_vec();
    path.push(b'/');
    path.extend_from_slice(LIBRARY.as_bytes());
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests are meaningless in a build without the tree; they say so
    /// instead of failing, the same way the pty tests skip.
    fn shell_with_tree() -> Option<Interp> {
        let mut shell = Interp::new(false);
        install(&mut shell)?;
        Some(shell)
    }

    #[test]
    fn the_tree_is_mounted_and_the_library_is_a_file_in_it() {
        let Some(mut shell) = shell_with_tree() else {
            eprintln!("skipped: this binary carries no bash-completion tree");
            return;
        };
        let vfs = &shell.state().vfs;
        let library = [MOUNT, b"/", LIBRARY.as_bytes()].concat();
        assert_eq!(vfs.kind(MOUNT), Some(baash_runtime::vfs::Kind::Dir));
        assert_eq!(
            vfs.kind(&library),
            Some(baash_runtime::vfs::Kind::File),
            "{} must be a file",
            String::from_utf8_lossy(&library)
        );
        let bytes = vfs.file(&library).expect("the library's bytes");
        assert!(bytes.len() > 100_000, "the framework is not tiny");
        assert_eq!(
            vfs.kind(&[MOUNT, b"/completions-core"].concat()),
            Some(baash_runtime::vfs::Kind::Dir)
        );
        assert!(
            vfs.file(&[MOUNT, b"/completions-core/tar.bash"].concat())
                .is_some(),
            "the per-command scripts come with the framework"
        );
    }
}
