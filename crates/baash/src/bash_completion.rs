//! The bash-completion library the binary carries.
//!
//! A `complete -F` function is only half a completion: most of the useful ones
//! are written against bash-completion's helpers (`_filedir`,
//! `_init_completion`, `_comp_compgen_*`), and the library itself is
//! distribution data that Windows does not have. So the tree is compiled into
//! the binary and unpacked on the first interactive start:
//!
//! * `vendor/bash-completion/` is the embed folder, and it is **not** vendored
//!   in git. `flake.nix` copies the pinned nixpkgs `bash-completion` into it
//!   before the build (the dev shell does the same for `cargo test`), so the
//!   bytes come from a pinned source and the version moves with the flake. A
//!   build without the folder is not an error: it is a binary that carries no
//!   library, which is what a plain `cargo build` is — [`ensure_extracted`]
//!   then answers `None` and the shell behaves as it did before.
//! * the shell cannot `source` a file that only exists in the binary, so the
//!   tree is written to `$XDG_CACHE_HOME/baash/bash-completion/<tag>/`
//!   (`%LOCALAPPDATA%` on Windows) and the path is handed to the startup file
//!   as `BAASH_EMBEDDED_BASH_COMPLETION`. `config/config.sh` tries a copy the
//!   system installed first and this one last.
//! * `<tag>` is a digest of the tree's `PROVENANCE` file, which the copy step
//!   writes (in the flake it is the store path). A different library therefore
//!   never reuses an old extraction, and two builds of the same library share
//!   one — a 100-character store path in the cache directory would be how a
//!   Windows `MAX_PATH` failure happens instead.
//! * the extraction is atomic: files are written to a staging directory, the
//!   marker is written last, and the staging directory is renamed into place.
//!   A directory without the marker is half an extraction and is replaced, so
//!   a killed first start costs the next one a second attempt, not a broken
//!   library.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// Where the copy step records which tree this is.
///
/// It is the only thing that says the tree exists at all, so a folder without
/// it (a hand-made `cp` that forgot it) is as good as no folder.
#[cfg(feature = "embedded-bash-completion")]
const PROVENANCE: &str = "PROVENANCE";

/// The file the startup file sources; its directory is the library's home.
const LIBRARY: &str = "bash_completion";

/// Written last: its presence is the claim that the tree is whole.
const MARKER: &str = ".complete";

#[cfg(feature = "embedded-bash-completion")]
#[derive(rust_embed::Embed)]
#[folder = "../../vendor/bash-completion/"]
#[allow_missing = true]
struct Library;

/// Unpack the library and name the `bash_completion` inside it.
///
/// The bytes are the shell's own path form (`WTF-8`, forward slashes), ready to
/// be assigned to `BAASH_EMBEDDED_BASH_COMPLETION`. `None` means this binary
/// carries no library, or the machine has no cache directory to unpack it in.
pub fn ensure_extracted() -> Option<Vec<u8>> {
    let path = extract_into(&base()?)?;
    Some(shell_path(path))
}

/// The library's path in the shell's own form: `WTF-8`, forward slashes.
///
/// A Windows path in a shell word is written with `/`: the value is quoted
/// wherever it is used, so nothing would translate a `\`, and every layer below
/// (`.`, `-r`, the OS) accepts the forward-slash form.
fn shell_path(path: PathBuf) -> Vec<u8> {
    let bytes = baash_sys::os::bytes_from_os(path);
    #[cfg(windows)]
    let bytes = {
        let mut bytes = bytes;
        for byte in &mut bytes {
            if *byte == b'\\' {
                *byte = b'/';
            }
        }
        bytes
    };
    bytes
}

/// Where extractions live: `$XDG_CACHE_HOME/baash/bash-completion` (or the
/// platform's equivalent), never the data directory.
fn base() -> Option<PathBuf> {
    baash_sys::dirs::cache_home().map(|cache| cache.join("baash").join("bash-completion"))
}

/// The tree's identity, from its provenance line.
///
/// Whitespace is dropped because the copy step writes a trailing newline and
/// nothing else cares about it.
fn tag() -> Option<String> {
    provenance().as_deref().map(tag_of)
}

fn tag_of(provenance: &[u8]) -> String {
    // FNV-1a: 64 bits, no dependency, and this is a name, not a security
    // boundary — the bytes are ours.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in provenance.iter().filter(|byte| !byte.is_ascii_whitespace()) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Unpack into `base` when needed, and return the library's path.
fn extract_into(base: &Path) -> Option<PathBuf> {
    let tag = tag()?;
    let dir = base.join(&tag);
    let marker = dir.join(MARKER);
    let library = dir.join(LIBRARY);
    if marker.is_file() {
        return Some(library);
    }
    // A directory without the marker is half an extraction (a killed process,
    // a full disk). The tag names this exact tree, so it is ours to replace.
    let _ = std::fs::remove_dir_all(&dir);
    let staging = base.join(format!(".staging-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).ok()?;
    for (name, data) in files() {
        let path = staging.join(name.as_ref());
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok()?;
        }
        std::fs::write(&path, data.as_ref()).ok()?;
    }
    std::fs::write(staging.join(MARKER), &tag).ok()?;
    if std::fs::rename(&staging, &dir).is_err() {
        let _ = std::fs::remove_dir_all(&staging);
        // Another shell unpacked the same tree first: a rename onto a
        // non-empty directory fails on every platform, and the winner's
        // directory is the one to use.
        if !marker.is_file() {
            return None;
        }
    }
    Some(library)
}

#[cfg(feature = "embedded-bash-completion")]
fn provenance() -> Option<Vec<u8>> {
    Library::get(PROVENANCE).map(|file| file.data.into_owned())
}

#[cfg(not(feature = "embedded-bash-completion"))]
fn provenance() -> Option<Vec<u8>> {
    None
}

#[cfg(feature = "embedded-bash-completion")]
fn files() -> Vec<(Cow<'static, str>, Cow<'static, [u8]>)> {
    Library::iter()
        .filter_map(|name| Library::get(&name).map(|file| (name, file.data)))
        .collect()
}

#[cfg(not(feature = "embedded-bash-completion"))]
fn files() -> Vec<(Cow<'static, str>, Cow<'static, [u8]>)> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory under the system temporary directory.
    fn temp_dir(tag: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("baash-completion-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the temporary directory");
        path
    }

    /// The tests are meaningless in a build that carries no tree; they say so
    /// instead of failing, the same way the pty tests skip without a library.
    fn tree() -> Option<(String, Vec<u8>)> {
        let provenance = provenance()?;
        Some((tag_of(&provenance), provenance))
    }

    #[test]
    fn the_tree_unpacks_once_and_is_reused() {
        let Some((tag, _)) = tree() else {
            eprintln!("skipped: this binary carries no bash-completion tree");
            return;
        };
        let base = temp_dir("reuse");
        let library = extract_into(&base).expect("first extraction");
        assert_eq!(library, base.join(&tag).join(LIBRARY));
        assert!(library.is_file(), "the library file must exist");
        assert!(
            library
                .parent()
                .expect("a parent")
                .join("completions-core/tar.bash")
                .is_file(),
            "the completions come with the framework"
        );

        // A second start must not touch the tree: a file the shell itself
        // wrote (or a user edited) stays as it is.
        let edited = base.join(&tag).join("edited-by-the-test");
        std::fs::write(&edited, b"keep me").expect("write into the tree");
        let again = extract_into(&base).expect("second extraction");
        assert_eq!(again, library);
        assert!(
            edited.is_file(),
            "an existing extraction must not be overwritten"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_half_written_tree_is_replaced() {
        let Some((tag, _)) = tree() else {
            eprintln!("skipped: this binary carries no bash-completion tree");
            return;
        };
        let base = temp_dir("half");
        let dir = base.join(&tag);
        std::fs::create_dir_all(&dir).expect("create the half tree");
        std::fs::write(dir.join(LIBRARY), b"not the library").expect("write the bait");
        let library = extract_into(&base).expect("extraction over the half tree");
        assert!(library.parent().expect("a parent").join(MARKER).is_file());
        let contents = std::fs::read(&library).expect("read the library");
        assert_ne!(contents, b"not the library");
        assert!(contents.len() > 1_000, "the real library is not tiny");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_tag_is_stable_and_ignores_whitespace() {
        assert_eq!(
            tag_of(b"/nix/store/x-bash-completion\n"),
            tag_of(b"/nix/store/x-bash-completion")
        );
        assert_ne!(
            tag_of(b"/nix/store/x-bash-completion"),
            tag_of(b"/nix/store/y-bash-completion")
        );
        assert_eq!(tag_of(b"x").len(), 16);
    }
}
