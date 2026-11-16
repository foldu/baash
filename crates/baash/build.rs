//! The Windows shell icon and the commit `--version` prints.
//!
//! Explorer, the taskbar and Alt-Tab take a program's icon from a PE resource,
//! not from anything the program does. The resource is a `RT_GROUP_ICON`
//! directory plus one `RT_ICON` per size: bytes the loader never reads and the
//! shell does. `baash.rc` names them, the resource compiler turns it into a
//! COFF object, and the linker puts that object in the image like any other.
//! Nothing about the icon runs for a non-Windows target, so a Linux build
//! neither needs nor looks for a resource compiler.
//!
//! The other job is `BAASH_GIT_VERSION`, which `main.rs` puts beside the
//! package version in `--version`. Every build of this tree is `0.0.0`, so the
//! commit is the only thing that tells two binaries apart, and a bug report
//! that names it is worth more than one that does not.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn main() {
    println!("cargo:rerun-if-changed=baash.rc");
    println!("cargo:rerun-if-changed=../../assets/baash.ico");
    println!("cargo:rerun-if-env-changed=BAASH_WINDRES");

    git_version();

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets this"));
    let assets = manifest.join("../../assets");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets this"));
    let object = out.join("baash.res.o");

    // `BAASH_WINDRES` first: `flake.nix` points it at LLVM's resource compiler,
    // which is the only one installed there. The other two names are what a
    // host with MinGW or LLVM on `PATH` already provides, so a developer
    // building for Windows outside the flake needs no configuration.
    let windres = env::var_os("BAASH_WINDRES")
        .map(PathBuf::from)
        .or_else(|| on_path("windres"))
        .or_else(|| on_path("llvm-windres"))
        .expect("no resource compiler: set BAASH_WINDRES, or put windres on PATH");

    let status = Command::new(&windres)
        .arg(format!("--input={}", manifest.join("baash.rc").display()))
        .arg(format!("--output={}", object.display()))
        .arg("--output-format=coff")
        .arg(format!("--include-dir={}", assets.display()))
        .status()
        .unwrap_or_else(|error| panic!("{} could not be run: {error}", windres.display()));
    assert!(
        status.success(),
        "{} could not compile baash.rc: {status}",
        windres.display()
    );

    // The binary only: a test or an example that linked the shell's icon would
    // be a second copy of it with no shell behind it.
    println!("cargo:rustc-link-arg-bins={}", object.display());
}

/// Publish `BAASH_GIT_VERSION`, the commit `--version` prints.
///
/// Three sources, in the order a build can be trusted to have them.
/// `BAASH_GIT_VERSION` is `flake.nix`'s, and inside the Nix sandbox it is the
/// only possible one: the store path built from has no `.git` to ask. A plain
/// `cargo build` in a checkout asks `git`. Anything else — an unpacked tarball,
/// a source drop with no history — says `unknown`, because a label is not worth
/// failing a build over.
fn git_version() {
    println!("cargo:rerun-if-env-changed=BAASH_GIT_VERSION");

    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets this"));

    // Registered before the value is decided, not after: cargo has to know to
    // rerun this script when the answer changes, and it decides that from the
    // paths named here rather than from anything the script reads.
    if let Some(git_dir) = git_dir(&manifest) {
        watch_git(&manifest, &git_dir);
    }

    let version = env::var("BAASH_GIT_VERSION")
        .ok()
        .filter(|version| !version.trim().is_empty())
        .or_else(|| git_describe(&manifest))
        .unwrap_or_else(|| String::from("unknown"));

    println!("cargo:rustc-env=BAASH_GIT_VERSION={version}");
}

/// The repository's `.git`, if this build is inside a checkout.
///
/// Read as a file rather than asked of `git`, because this runs in the sandbox
/// too, where there is no `git` and the question is only whether a `.git`
/// exists to point at.
fn git_dir(manifest: &Path) -> Option<PathBuf> {
    let dot_git = manifest.parent()?.parent()?.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    // A linked worktree or a submodule: `.git` is a file holding
    // `gitdir: <path>`, relative to its own directory unless absolute.
    let text = fs::read_to_string(&dot_git).ok()?;
    let target = Path::new(text.trim().strip_prefix("gitdir:")?.trim());
    Some(if target.is_absolute() {
        target.to_path_buf()
    } else {
        dot_git.parent()?.join(target)
    })
}

/// Ask `git` what this tree is, or `None` where it cannot answer.
///
/// `--always`: a clone with no tags still has a commit to name. `--dirty`: a
/// binary built from a working tree with edits is not the commit it would
/// otherwise claim, and a bug report that says otherwise sends its reader to
/// code that is not the code that misbehaved.
fn git_describe(manifest: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(manifest)
        .args(["describe", "--always", "--dirty", "--abbrev=7"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!version.is_empty()).then_some(version)
}

/// Tell cargo which files decide the label.
///
/// Cargo reruns this script only for the paths named here, and a new commit
/// touches no source file: `HEAD` is a pointer to `refs/heads/<branch>`, and
/// the commit moves the ref it names. Watching only `HEAD` would leave the
/// previous commit's label in the binary until some unrelated edit forced a
/// rebuild, which is exactly the wrong answer for the question the label
/// answers.
fn watch_git(manifest: &Path, git_dir: &Path) {
    for name in ["HEAD", "packed-refs"] {
        let path = git_dir.join(name);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    if let Some(reference) = fs::read_to_string(git_dir.join("HEAD"))
        .ok()
        .and_then(|head| head.strip_prefix("ref: ").map(str::to_owned))
    {
        let path = git_dir.join(reference.trim());
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }

    // `--dirty` asks about the working tree, and no single file stands for it.
    // These two are the ones a code change must touch, and a rebuild of this
    // crate already follows any edit under `crates/`; naming them here is what
    // makes the flag move to `-dirty` in the same build. A change somewhere
    // else in the tree (a document, the flake) is not watched, so a label
    // without `-dirty` means "no code was edited", not "the tree is clean".
    for path in [
        manifest.join("../../crates"),
        manifest.join("../../Cargo.toml"),
    ] {
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

/// The first `name` on `PATH`, if any.
///
/// A native Windows build names its tools with `.exe`; the cross builds use
/// the bare name, so both spellings are tried where the platform has them.
fn on_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .flat_map(|directory| {
            let bare = directory.join(name);
            let exe = directory.join(format!("{name}.exe"));
            [bare, exe]
        })
        .find(|candidate| is_executable(candidate))
}

/// Whether `path` is a file this process may execute.
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        path.is_file()
    }
}
