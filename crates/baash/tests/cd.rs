//! `cd`, `pwd` and the directory stack, through the real binary.
//!
//! The lexical walk has its own unit tests (`baash_sys::paths`, which covers
//! both platforms' roots); what these add is the wiring around it — `$PWD`,
//! `$OLDPWD`, `cd -`, `CDPATH`, the failure messages, and the difference
//! between the logical name `cd` records and the kernel's canonical one that
//! `pwd -P` asks for.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn baash() -> Command {
    Command::new(env!("CARGO_BIN_EXE_baash"))
}

/// Run `script` in `directory`.
fn run_in(directory: &Path, script: &str) -> Output {
    baash()
        .current_dir(directory)
        .args(["-c", script])
        .output()
        .expect("spawn baash")
}

fn code(output: &Output) -> i32 {
    output.status.code().unwrap_or_else(|| {
        panic!(
            "baash terminated by a signal; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Run `script` in `directory`, require success, return stdout.
fn out_in(directory: &Path, script: &str) -> String {
    let output = run_in(directory, script);
    assert_eq!(
        code(&output),
        0,
        "expected success for `{script}`; stderr: {}",
        stderr(&output)
    );
    stdout(&output)
}

/// A temporary directory that removes itself.
struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!("baash-cd-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the test directory");
        Self { path }
    }

    /// Create `name` below the root and return its path.
    fn child(&self, name: &str) -> PathBuf {
        let path = self.path.join(name);
        std::fs::create_dir_all(&path).expect("create a child directory");
        path
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn cd_records_the_name_it_entered_in_pwd_and_keeps_the_old_one() {
    let dir = TestDir::new("pwd");
    dir.child("sub/inner");
    let root = dir.path().display().to_string();
    // `..` is collapsed textually, which is why `sub/../sub` still ends in
    // `sub` and why the answer is a name rather than a canonical path.
    let script = "cd sub/../sub && pwd && echo \"$PWD\" && echo \"$OLDPWD\"";
    assert_eq!(
        out_in(dir.path(), script),
        format!("{root}/sub\n{root}/sub\n{root}\n")
    );
}

#[test]
fn cd_dash_returns_to_oldpwd_and_prints_it() {
    let dir = TestDir::new("dash");
    dir.child("a");
    dir.child("b");
    let root = dir.path().display().to_string();
    let script = "cd a && cd ../b && cd - && pwd";
    assert_eq!(out_in(dir.path(), script), format!("{root}/a\n{root}/a\n"));
}

#[test]
fn a_named_component_that_is_not_a_directory_fails_the_logical_cd() {
    let dir = TestDir::new("missing");
    dir.child("sub");
    // bash resolves `..` textually but still refuses a component it cannot
    // enter; only an existing named prefix is allowed to be dropped again.
    let failed = run_in(dir.path(), "cd missing/..");
    assert_eq!(code(&failed), 1);
    assert!(
        stderr(&failed).contains("No such file or directory"),
        "{}",
        stderr(&failed)
    );
    assert_eq!(
        out_in(dir.path(), "cd sub/.. && pwd").trim_end(),
        dir.path().display().to_string()
    );
}

#[test]
fn an_empty_operand_is_a_null_directory() {
    let dir = TestDir::new("empty");
    for script in ["cd \"\"", "cd -L \"\"", "pushd \"\""] {
        let output = run_in(dir.path(), script);
        assert_eq!(code(&output), 1, "`{script}`");
        assert!(
            stderr(&output).contains("null directory"),
            "`{script}`: {}",
            stderr(&output)
        );
    }
    // `CDPATH` is not searched for the empty name: it is not a name.
    dir.child("sub");
    let output = run_in(dir.path(), "CDPATH=sub; cd \"\"");
    assert_eq!(code(&output), 1);
    assert!(stderr(&output).contains("null directory"));
}

#[test]
fn cdpath_finds_a_bare_name_and_prints_where_it_went() {
    let dir = TestDir::new("cdpath");
    dir.child("sub/inner");
    let root = dir.path().display().to_string();
    // A match from a non-empty element is printed; the `.` element is not.
    let found = run_in(dir.path(), "CDPATH=sub; cd inner && pwd");
    assert_eq!(code(&found), 0, "{}", stderr(&found));
    assert_eq!(
        stdout(&found),
        format!("{root}/sub/inner\n{root}/sub/inner\n")
    );
    // An absolute or `..`-anchored name is never searched for.
    let anchored = run_in(dir.path(), "CDPATH=sub; cd ./sub && pwd");
    assert_eq!(code(&anchored), 0);
    assert_eq!(stdout(&anchored).trim_end(), format!("{root}/sub"));
}

#[test]
fn pushd_and_popd_move_the_stack_not_just_the_directory() {
    let dir = TestDir::new("stack");
    dir.child("sub");
    let root = dir.path().display().to_string();
    // `pushd` prints the stack, which is noise here; the point is that the
    // stack and the directory move together and `popd` puts both back.
    let script = "pushd sub > /dev/null && popd > /dev/null && pwd";
    assert_eq!(out_in(dir.path(), script).trim_end(), root);
    // `dirs` with `-l` prints the recorded name, not the kernel's.
    let script = "pushd sub > /dev/null && dirs -l -p";
    assert_eq!(out_in(dir.path(), script), format!("{root}/sub\n{root}\n"));
}

#[test]
fn autopushd_is_off_until_shopt_turns_it_on() {
    let dir = TestDir::new("autopushd-off");
    dir.child("a");
    dir.child("b");
    let root = dir.path().display().to_string();
    // Without the option `cd` only rewrites element 0, so `dirs` is `$PWD`.
    let script = "cd a && cd ../b && dirs -l -p";
    assert_eq!(out_in(dir.path(), script), format!("{root}/b\n"));
    // ... and there is nothing for `popd` to pop.
    let output = run_in(dir.path(), "cd a; popd");
    assert_eq!(code(&output), 1);
    assert!(
        stderr(&output).contains("directory stack empty"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn autopushd_keeps_the_directory_each_cd_left() {
    let dir = TestDir::new("autopushd-on");
    dir.child("a");
    dir.child("b");
    let root = dir.path().display().to_string();
    // Every `cd` leaves the directory it left at element 1, so the stack is
    // the walk itself and `popd` retraces it one step at a time.
    let script = "shopt -s autopushd\n\
                  cd a\n\
                  cd ../b\n\
                  dirs -l -p\n\
                  popd > /dev/null\n\
                  pwd\n\
                  popd > /dev/null\n\
                  pwd";
    assert_eq!(
        out_in(dir.path(), script),
        format!("{root}/b\n{root}/a\n{root}\n{root}/a\n{root}\n")
    );
}

#[test]
fn autopushd_records_what_cd_dash_left() {
    let dir = TestDir::new("autopushd-dash");
    dir.child("a");
    let root = dir.path().display().to_string();
    // `cd -` prints where it went, and the directory it left is pushed like
    // any other `cd`'s: the stack reads `root, a, root` — the way back is
    // `popd` to `a` and then to `root` again.
    let script = "shopt -s autopushd; cd a; cd -; dirs -l -p";
    assert_eq!(
        out_in(dir.path(), script),
        format!("{root}\n{root}\n{root}/a\n{root}\n")
    );
}

#[test]
fn autopushd_records_nothing_for_a_cd_that_stays_put() {
    let dir = TestDir::new("autopushd-still");
    let root = dir.path().display().to_string();
    let script = "shopt -s autopushd; cd .; cd \"$PWD\"; dirs -l -p";
    assert_eq!(out_in(dir.path(), script), format!("{root}\n"));
    // The stack is still the single current directory, so `popd` refuses.
    let output = run_in(dir.path(), "shopt -s autopushd; cd .; popd");
    assert_eq!(code(&output), 1);
    assert!(
        stderr(&output).contains("directory stack empty"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn autopushd_keeps_a_failed_cd_out_of_the_stack() {
    let dir = TestDir::new("autopushd-fail");
    let root = dir.path().display().to_string();
    let script = "shopt -s autopushd; cd missing; echo st=$?; dirs -l -p";
    let output = run_in(dir.path(), script);
    assert_eq!(code(&output), 0);
    assert_eq!(stdout(&output), format!("st=1\n{root}\n"));
    assert!(
        stderr(&output).contains("No such file or directory"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn autopushd_leaves_pushd_alone() {
    let dir = TestDir::new("autopushd-pushd");
    dir.child("a");
    let root = dir.path().display().to_string();
    // `pushd` already leaves the old directory at element 1; pushing it a
    // second time would put `root` on the stack twice.
    let script = "shopt -s autopushd; pushd a > /dev/null; dirs -l -p";
    assert_eq!(out_in(dir.path(), script), format!("{root}/a\n{root}\n"));
}

#[test]
fn autopushd_does_not_leak_out_of_a_subshell() {
    let dir = TestDir::new("autopushd-scope");
    dir.child("a");
    let root = dir.path().display().to_string();
    // The stack is part of `ShellState`, which a `( … )` clones; the push of
    // the inner `cd` dies with the scope like every other shell write.
    let script = "shopt -s autopushd\n( cd a )\ndirs -l -p";
    assert_eq!(out_in(dir.path(), script), format!("{root}\n"));
}

#[test]
fn cd_with_no_operand_goes_to_home() {
    let dir = TestDir::new("home");
    dir.child("sub");
    let root = dir.path().display().to_string();
    let output = baash()
        .current_dir(dir.path().join("sub"))
        .env("HOME", &root)
        .args(["-c", "cd && pwd && echo \"$PWD\""])
        .output()
        .expect("spawn baash");
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("{root}\n{root}\n"));
}

#[cfg(unix)]
#[test]
fn pwd_follows_the_logical_name_until_p_is_asked() {
    let dir = TestDir::new("symlink");
    let real = dir.child("real");
    std::os::unix::fs::symlink(&real, dir.path().join("link")).expect("create a symlink");
    let root = dir.path().display().to_string();
    // `-L` keeps the link's name, and `cd ..` leaves it by name instead of
    // through the link's target; `-P` asks the kernel.
    let script = "cd link && pwd && pwd -P && cd .. && pwd";
    assert_eq!(
        out_in(dir.path(), script),
        format!("{root}/link\n{root}/real\n{root}\n")
    );
}
