//! The Windows shell icon.
//!
//! Explorer, the taskbar and Alt-Tab take a program's icon from a PE resource,
//! not from anything the program does. The resource is a `RT_GROUP_ICON`
//! directory plus one `RT_ICON` per size: bytes the loader never reads and the
//! shell does. `baash.rc` names them, the resource compiler turns it into a
//! COFF object, and the linker puts that object in the image like any other.
//!
//! Nothing here runs for a non-Windows target, so a Linux build neither needs
//! nor looks for a resource compiler.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=baash.rc");
    println!("cargo:rerun-if-changed=../../assets/baash.ico");
    println!("cargo:rerun-if-env-changed=BAASH_WINDRES");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets this"));
    let assets = manifest.join("../../assets");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets this"));
    let object = out.join("baash.res.o");

    // `BAASH_WINDRES` first: `flake.nix` points it at LLVM's resource compiler,
    // which is the only one installed there. `windres` on `PATH` is the name
    // the Windows build already exports a shim under (ci/windres), and the last
    // fallback is the same compiler under its own name.
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
