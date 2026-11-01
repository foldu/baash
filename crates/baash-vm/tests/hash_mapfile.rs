//! `hash` and `mapfile`/`readarray`: built-ins whose state is the executor's.
//!
//! The path table decides which file a later command word runs, and mapfile
//! writes through the same per-descriptor read buffer `read` owns, so both
//! bodies live in `baash-vm` even though their registry entries are the
//! runtime's. Everything here is observed through shell state or an exit
//! status — never through stdout, which several threads share in this test
//! binary.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use baash_lang::{ArrayValue, Vars};
use baash_vm::Interp;

/// Tests that redirect descriptors must not run concurrently with each other.
static FDS: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    FDS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn shell() -> Interp {
    let mut interp = Interp::new(false);
    interp.set_arg0("baash");
    interp
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("baash-state-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn text(interp: &Interp, name: &[u8]) -> String {
    String::from_utf8_lossy(Vars::get(interp, name).unwrap_or_default()).into_owned()
}

fn elements(interp: &Interp, name: &[u8]) -> Vec<String> {
    match Vars::array(interp, name) {
        Some(ArrayValue::Indexed(values)) => values
            .values()
            .map(|value| String::from_utf8_lossy(value).into_owned())
            .collect(),
        _ => Vec::new(),
    }
}

/// A script whose only effect is its exit status, so a `PATH` hit is
/// observable without reading the program's output.
fn write_script(dir: &Path, name: &str, status: i32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\nexit {status}\n")).expect("write fixture");
    let mut permissions = std::fs::metadata(&path)
        .expect("stat fixture")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).expect("chmod fixture");
    path
}

#[test]
fn a_cached_path_wins_until_the_hash_is_cleared() {
    let _serial = serial();
    let dir = temp_dir("hash-cache");
    let one = dir.join("one");
    let two = dir.join("two");
    std::fs::create_dir_all(&one).expect("mkdir one");
    std::fs::create_dir_all(&two).expect("mkdir two");
    write_script(&two, "mycmd", 20);
    write_script(&one, "mycmd", 10);
    let mut interp = shell();

    // The first run caches the hit in `two`.
    interp.run_named(
        "<hash>",
        format!(
            "PATH='{}':'{}':$PATH\nmycmd; first=$?\n",
            two.display(),
            one.display()
        )
        .as_bytes(),
    );
    assert_eq!(text(&interp, b"first"), "20");

    // Reversing `PATH` does not change the answer: the remembered path wins,
    // which is the whole reason `hash` exists.
    interp.run_named(
        "<hash>",
        format!(
            "PATH='{}':'{}':$PATH\nmycmd; second=$?\n",
            one.display(),
            two.display()
        )
        .as_bytes(),
    );
    assert_eq!(text(&interp, b"second"), "20");

    // `hash -r` forgets it, so a fresh search finds `one`.
    interp.run_named("<hash>", b"hash -r\nmycmd; third=$?\n");
    assert_eq!(text(&interp, b"third"), "10");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hash_d_forgets_one_name_and_hash_p_pins_a_path() {
    let _serial = serial();
    let dir = temp_dir("hash-d");
    let one = dir.join("one");
    let two = dir.join("two");
    std::fs::create_dir_all(&one).expect("mkdir one");
    std::fs::create_dir_all(&two).expect("mkdir two");
    write_script(&two, "mycmd", 20);
    let pinned = write_script(&one, "mycmd", 10);
    let mut interp = shell();

    interp.run_named(
        "<hash>",
        format!("PATH='{}':$PATH\nmycmd; cached=$?\n", two.display()).as_bytes(),
    );
    assert_eq!(text(&interp, b"cached"), "20");

    // `hash -d` drops just that name, so the next run searches `PATH` again.
    interp.run_named(
        "<hash>",
        format!(
            "PATH='{}':'{}':$PATH\nhash -d mycmd\nmycmd; rediscovered=$?\n",
            one.display(),
            two.display()
        )
        .as_bytes(),
    );
    assert_eq!(text(&interp, b"rediscovered"), "10");

    // `hash -p` records a path without searching for it.
    interp.run_named(
        "<hash>",
        format!(
            "PATH='{}':$PATH\nhash -p '{}' mycmd\nmycmd; pinned=$?\n",
            two.display(),
            pinned.display()
        )
        .as_bytes(),
    );
    assert_eq!(text(&interp, b"pinned"), "10");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hash_reports_a_name_it_cannot_find() {
    let _serial = serial();
    let mut interp = shell();
    let status = interp.run_named("<hash>", b"hash definitely-not-a-program");
    assert_eq!(status.exit_code(), 1);
    // `hash -r` with names is a usage error rather than a silent no-op.
    assert_eq!(interp.run_named("<hash>", b"hash -r mycmd").exit_code(), 1);
}

#[test]
fn mapfile_reads_records_into_an_array() {
    let _serial = serial();
    let dir = temp_dir("mapfile");
    let input = dir.join("in.txt");
    std::fs::write(&input, b"1\n3\n5\n").expect("write input");
    let mut interp = shell();
    interp.run_named(
        "<mapfile>",
        format!("mapfile -t arr < '{}'\n", input.display()).as_bytes(),
    );
    assert_eq!(elements(&interp, b"arr"), ["1", "3", "5"]);

    // Without `-t` the delimiter stays in the element, and `-n`/`-s` count
    // records rather than bytes.
    interp.run_named(
        "<mapfile>",
        format!("mapfile -s 1 -n 1 arr < '{}'\n", input.display()).as_bytes(),
    );
    assert_eq!(elements(&interp, b"arr"), ["3\n"]);

    // `-d` changes the record separator.
    let colon = dir.join("colon.txt");
    std::fs::write(&colon, b"a:b:c:").expect("write colon input");
    interp.run_named(
        "<mapfile>",
        format!("mapfile -d : -t arr < '{}'\n", colon.display()).as_bytes(),
    );
    assert_eq!(elements(&interp, b"arr"), ["a", "b", "c"]);

    // `-d ''` is the NUL separator.
    let nul = dir.join("nul.txt");
    std::fs::write(&nul, b"a\0b\0").expect("write nul input");
    interp.run_named(
        "<readarray>",
        format!("readarray -d '' arr < '{}'\n", nul.display()).as_bytes(),
    );
    assert_eq!(elements(&interp, b"arr"), ["a", "b"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mapfile_origin_keeps_existing_elements() {
    let _serial = serial();
    let dir = temp_dir("mapfile-origin");
    let input = dir.join("in.txt");
    std::fs::write(&input, b"a0\na1\n").expect("write input");
    let mut interp = shell();
    let script = format!("arr=(x y z)\nmapfile -O 2 -t arr < '{}'\n", input.display());
    interp.run_named("<mapfile>", script.as_bytes());
    assert_eq!(elements(&interp, b"arr"), ["x", "y", "a0", "a1"]);

    // Without `-O` the array is replaced.
    interp.run_named(
        "<mapfile>",
        format!("mapfile -t arr < '{}'\n", input.display()).as_bytes(),
    );
    assert_eq!(elements(&interp, b"arr"), ["a0", "a1"]);

    // An empty input leaves an empty array and succeeds.
    let empty = dir.join("empty.txt");
    std::fs::write(&empty, b"").expect("write empty input");
    interp.run_named(
        "<mapfile>",
        format!("mapfile -t arr < '{}'; st=$?\n", empty.display()).as_bytes(),
    );
    assert_eq!(text(&interp, b"st"), "0");
    assert!(elements(&interp, b"arr").is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
