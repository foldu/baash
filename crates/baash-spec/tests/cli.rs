//! End-to-end exercise of the CLI's counting and ratchet contract.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_dir(tag: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "baash-spec-test-{tag}-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &Path, name: &str, contents: &str) {
    std::fs::write(dir.join(name), contents).unwrap();
}

#[test]
fn counts_passes_separately_from_skips_and_honours_min() {
    let dir = temp_dir("cli");
    let spec_dir = dir.join("spec");
    std::fs::create_dir_all(&spec_dir).unwrap();
    write(
        &spec_dir,
        "a.test.sh",
        "## compare_shells: bash\n\n\
         #### passing case\n\
         echo hi\n\
         ## STDOUT:\n\
         hi\n\
         ## END\n\n\
         #### failing case\n\
         echo hi\n\
         ## STDOUT:\n\
         bye\n\
         ## END\n",
    );
    write(
        &spec_dir,
        "ysh-skipped.test.sh",
        "#### ysh case\nvar x = 1\n## STDOUT:\n1\n## END\n",
    );
    write(
        &spec_dir,
        "header-skip.test.sh",
        "## our_shell: ysh\n\n#### ysh header case\necho hi\n## stdout: hi\n",
    );

    let exe = env!("CARGO_BIN_EXE_baash-spec");
    let json = dir.join("report.json");
    let output = Command::new(exe)
        .args([
            "--spec-dir",
            spec_dir.to_str().unwrap(),
            "--shell",
            "/bin/sh",
            "--jobs",
            "1",
            "--json",
            json.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("TOTAL: 1/2 compared, 2 skipped, of 4 cases"),
        "{stdout}"
    );
    assert!(!stdout.contains("3 skipped"), "{stdout}");

    let report = std::fs::read_to_string(&json).unwrap();
    assert!(report.contains("\"passed\": 1"), "{report}");
    assert!(report.contains("\"skipped\": 2"), "{report}");
    assert!(report.contains("our_shell: ysh"), "{report}");
    assert!(report.contains("ysh suite (out of scope)"), "{report}");

    let ratchet = Command::new(exe)
        .args([
            "--spec-dir",
            spec_dir.to_str().unwrap(),
            "--shell",
            "/bin/sh",
            "--jobs",
            "1",
            "--min",
            "2",
        ])
        .output()
        .unwrap();
    assert_eq!(ratchet.status.code(), Some(1));

    let ok = Command::new(exe)
        .args([
            "--spec-dir",
            spec_dir.to_str().unwrap(),
            "--shell",
            "/bin/sh",
            "--jobs",
            "1",
            "--min",
            "1",
        ])
        .output()
        .unwrap();
    assert!(ok.status.success());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_shell_named_through_a_symlink_is_run_as_named() {
    // `busybox` and `toybox` choose their applet from `argv[0]`, and `/bin/sh`
    // is a symlink to the multi-call binary on Alpine — the CI image. A
    // harness that resolves the link runs `busybox` with no applet, which
    // prints its usage instead of the script, and every case fails.
    let dir = temp_dir("symlink");
    let spec_dir = dir.join("spec");
    std::fs::create_dir_all(&spec_dir).unwrap();
    write(
        &spec_dir,
        "a.test.sh",
        "#### passing case\necho hi\n## STDOUT:\nhi\n## END\n",
    );
    let link = dir.join("sh");
    std::os::unix::fs::symlink("/bin/sh", &link).expect("link to /bin/sh");

    let exe = env!("CARGO_BIN_EXE_baash-spec");
    let json = dir.join("report.json");
    let output = Command::new(exe)
        .args([
            "--spec-dir",
            spec_dir.to_str().unwrap(),
            "--shell",
            link.to_str().unwrap(),
            "--jobs",
            "1",
            "--json",
            json.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("TOTAL: 1/1 compared"), "{stdout}");

    // And the report names the shell it was given, not what the link points
    // at: resolving is how the applet gets lost.
    let report = std::fs::read_to_string(&json).unwrap();
    let named = format!("\"shell\": \"{}\"", link.display());
    assert!(report.contains(&named), "expected {named} in {report}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_relative_shell_path_is_taken_from_the_working_directory() {
    // Each case runs in its own temporary directory, so a relative `--shell`
    // has to be made absolute before it is spawned — `--shell
    // target/debug/baash`, which is exactly what CI passes, would otherwise be
    // looked up inside the case directory and fail every case at once.
    let dir = temp_dir("relative");
    let spec_dir = dir.join("spec");
    std::fs::create_dir_all(&spec_dir).unwrap();
    write(
        &spec_dir,
        "a.test.sh",
        "#### passing case\necho hi\n## STDOUT:\nhi\n## END\n",
    );
    let link = dir.join("sh");
    std::os::unix::fs::symlink("/bin/sh", &link).expect("link to /bin/sh");

    let exe = env!("CARGO_BIN_EXE_baash-spec");
    let json = dir.join("report.json");
    let output = Command::new(exe)
        .current_dir(&dir)
        .args([
            "--spec-dir",
            spec_dir.to_str().unwrap(),
            "--shell",
            "sh",
            "--jobs",
            "1",
            "--json",
            json.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("TOTAL: 1/1 compared"), "{stdout}");

    // The report carries the path the name resolved to, not the bare name, so
    // it says which binary ran even though the case directories came and went.
    let report = std::fs::read_to_string(&json).unwrap();
    let named = format!("\"shell\": \"{}\"", link.display());
    assert!(report.contains(&named), "expected {named} in {report}");

    std::fs::remove_dir_all(&dir).ok();
}
