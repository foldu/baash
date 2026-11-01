//! End-to-end execution tests: every case runs the real binary with `-c`, a
//! script file or stdin, so the path exercised is lexer → parser → expansion →
//! evaluator → syscalls, not a unit-test double.
//!
//! Deliberate non-tests live in `baash-vm`'s module docs under `# Deferred`
//! (traps, job control, `select`, arrays, `${x#pat}`, `=~`, in-place `exec`).

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// The binary under test, as cargo built it.
fn baash() -> Command {
    Command::new(env!("CARGO_BIN_EXE_baash"))
}

fn run(script: &str) -> Output {
    baash().arg("-c").arg(script).output().expect("spawn baash")
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

/// Run `script`, require success, return stdout.
fn out(script: &str) -> String {
    let output = run(script);
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
        let path = std::env::temp_dir().join(format!("baash-exec-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the test directory");
        Self { path }
    }

    /// The path as bytes, for interpolation into a script. The temp directory
    /// is ASCII, so `Display` is safe here.
    fn at(&self, name: &str) -> String {
        self.path.join(name).display().to_string()
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

// ---------------------------------------------------------------------------
// Simple commands, statuses and the CLI
// ---------------------------------------------------------------------------

#[test]
fn echo_prints_its_arguments() {
    assert_eq!(out("echo hi"), "hi\n");
    assert_eq!(out("echo a b   c"), "a b c\n");
    assert_eq!(out("echo -n no-newline"), "no-newline");
    assert_eq!(out("echo"), "\n");
}

#[test]
fn true_and_false_set_the_status() {
    assert_eq!(code(&run("true")), 0);
    assert_eq!(code(&run("false")), 1);
    assert_eq!(out("true; echo $?"), "0\n");
    assert_eq!(out("false; echo $?"), "1\n");
    assert_eq!(out("false; true; echo $?"), "0\n");
}

#[test]
fn variable_assignment_and_expansion() {
    assert_eq!(out("x=hello; echo $x"), "hello\n");
    assert_eq!(out("x=hello; echo ${x}world"), "helloworld\n");
    assert_eq!(out("x=1 y=2; echo $x$y"), "12\n");
    assert_eq!(out("x=1; y=$x$x; echo $y"), "11\n");
    // Unset (None) and null (Some("")) stay distinct (baash-lang's contract).
    assert_eq!(out("unset x; echo ${x-unset}:${x:-null}"), "unset:null\n");
    assert_eq!(out("x=; echo ${x-unset}:${x:-null}"), ":null\n");
    assert_eq!(out("x=1; echo ${x+set}:${y+set}"), "set:\n");
}

#[test]
fn prefix_assignments_are_temporary_except_before_special_builtins() {
    // Expanded before the assignment, and scoped to the command.
    assert_eq!(out("x=1; x=2 echo $x; echo $x"), "1\n1\n");
    // POSIX: assignments before a *special* built-in persist. bash (non-posix)
    // drops them; we follow POSIX in both profiles (DESIGN.md §3.3 Tier C, and
    // docs/contracts.md records the reasoning).
    assert_eq!(out("x=1; x=2 :; echo $x"), "2\n");
    // A temporary assignment is visible in the command's environment.
    assert_eq!(out("X=temp sh -c 'echo $X'"), "temp\n");
    assert_eq!(out("x=1; X=temp sh -c 'echo $X'; echo $x"), "temp\n1\n");
}

#[test]
fn readonly_assignment_is_fatal_non_interactively() {
    let output = run("readonly r=1; r=2; echo unreached");
    assert_eq!(code(&output), 1);
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output).contains("BAASH-E0207"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn cd_changes_the_directory_and_survives() {
    let dir = TestDir::new("cd");
    let target = dir.path().display().to_string();
    assert_eq!(out(&format!("cd {target} && pwd")), format!("{target}\n"));
    // `cd -` prints the previous directory.
    assert_eq!(
        out(&format!("cd {target}; cd /; cd -")),
        format!("{target}\n")
    );
    let failed = run("cd /definitely/not/a/directory");
    assert_eq!(code(&failed), 1);
    // A failed `cd` leaves the shell where it was.
    assert_eq!(
        out("cd /definitely/not/a/directory; echo still-here"),
        "still-here\n"
    );
}

#[test]
fn exit_status_and_masking() {
    assert_eq!(code(&run("exit 0")), 0);
    assert_eq!(code(&run("exit 42")), 42);
    // Only the low eight bits reach the process exit status.
    assert_eq!(code(&run("exit 300")), 44);
    assert_eq!(code(&run("false; exit")), 1);
    assert_eq!(code(&run("exit 3 || echo no")), 3);
}

#[test]
fn syntax_errors_exit_two() {
    assert_eq!(code(&run("if true; then")), 2);
    assert_eq!(code(&run("case x in")), 2);
    let output = run("echo )");
    assert_eq!(code(&output), 2);
    assert!(stderr(&output).contains("BAASH-E"), "{}", stderr(&output));
}

#[test]
fn parse_only_reports_without_executing() {
    let dir = TestDir::new("parse-only");
    let good = dir.at("good.sh");
    let bad = dir.at("bad.sh");
    let side_effect = dir.at("side-effect");
    std::fs::write(&good, format!("echo hi > {side_effect}\n")).unwrap();
    std::fs::write(&bad, "if true; then\n").unwrap();

    let ok = baash().arg("-n").arg(&good).output().unwrap();
    assert_eq!(code(&ok), 0);
    assert!(!Path::new(&side_effect).exists(), "-n must not execute");

    let failed = baash().arg("-n").arg(&bad).output().unwrap();
    assert_eq!(code(&failed), 2);
    assert!(
        stderr(&failed).contains("BAASH-E0002"),
        "{}",
        stderr(&failed)
    );
}

#[test]
fn runs_a_script_file_with_positional_parameters() {
    let dir = TestDir::new("script");
    let script = dir.at("args.sh");
    std::fs::write(&script, "echo \"$0|$1|$2|$#\"\n").unwrap();
    let output = baash().arg(&script).arg("one").arg("two").output().unwrap();
    assert_eq!(code(&output), 0);
    assert_eq!(stdout(&output), format!("{script}|one|two|2\n"));
}

#[test]
fn runs_stdin_when_there_is_no_file() {
    let mut child = baash()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn baash");
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"x=41\necho $((x + 1))\n")
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(code(&output), 0);
    assert_eq!(stdout(&output), "42\n");
}

#[test]
fn dash_c_sets_dollar_zero_and_arguments() {
    let output = baash()
        .arg("-c")
        .arg("echo \"$0|$1|$#\"")
        .arg("myname")
        .arg("first")
        .output()
        .unwrap();
    assert_eq!(code(&output), 0);
    assert_eq!(stdout(&output), "myname|first|1\n");
}

#[test]
fn highlight_still_works() {
    let mut child = baash()
        .arg("--highlight")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"echo hi\n")
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(code(&output), 0);
    // Painted or not, the bytes survive; colour is decided by the tty.
    assert!(stdout(&output).contains("echo hi"));
}

// ---------------------------------------------------------------------------
// Expansion
// ---------------------------------------------------------------------------

#[test]
fn at_and_star_follow_quoting() {
    assert_eq!(
        out("set -- a b c; printf '<%s>' \"$@\"; echo"),
        "<a><b><c>\n"
    );
    assert_eq!(out("set -- a b c; printf '<%s>' $@; echo"), "<a><b><c>\n");
    assert_eq!(
        out("set -- 'a b' c; printf '<%s>' \"$@\"; echo"),
        "<a b><c>\n"
    );
    assert_eq!(
        out("set -- 'a b' c; printf '<%s>' $@; echo"),
        "<a><b><c>\n",
        "unquoted $@ is field-split"
    );
    assert_eq!(out("set -- a b; printf '<%s>' \"$*\"; echo"), "<a b>\n");
    assert_eq!(out("set -- a b; echo $#"), "2\n");
}

#[test]
fn ifs_field_splitting_uses_the_current_value() {
    assert_eq!(out("IFS=:; x=a:b:c; printf '<%s>' $x; echo"), "<a><b><c>\n");
    assert_eq!(
        out("IFS=:; x=a:b:c; printf '<%s>' \"$x\"; echo"),
        "<a:b:c>\n",
        "quoted expansion is never split"
    );
    // Literal source text is not an expansion result, so it is never split.
    assert_eq!(out("IFS=:; printf '<%s>' a:b; echo"), "<a:b>\n");
}

#[test]
fn command_substitution_captures_stdout_and_status() {
    assert_eq!(out("echo $(echo nested)"), "nested\n");
    assert_eq!(out("echo \"pre$(echo x)post\""), "prexpost\n");
    assert_eq!(out("echo $(echo a; echo b)"), "a b\n");
    assert_eq!(out("echo $(printf 'x\\n\\n\\n')"), "x\n");
    assert_eq!(out("x=$(false); echo $?"), "1\n");
    assert_eq!(out("x=$(exit 3); echo $?"), "3\n");
    // The *command's* status is `echo`'s own; only a command with no words
    // (`x=$(…)`) takes its status from the substitution.
    assert_eq!(out("echo $(exit 3); echo $?"), "\n0\n");
    assert_eq!(out("echo `echo backtick`"), "backtick\n");
}

#[test]
fn arithmetic_expansion_and_command() {
    assert_eq!(out("echo $((1 + 2 * 3))"), "7\n");
    assert_eq!(out("echo $((7 / 2)) $((7 % 2))"), "3 1\n");
    assert_eq!(out("echo $((1 << 4)) $((~0))"), "16 -1\n");
    assert_eq!(out("x=5; ((x++)); echo $x"), "6\n");
    assert_eq!(out("((1)) && echo true"), "true\n");
    assert_eq!(out("((0)) || echo false"), "false\n");
    assert_eq!(out("unset n; echo $((n + 1))"), "1\n");
    assert_eq!(
        out("for ((i = 0; i < 3; i++)); do echo $i; done"),
        "0\n1\n2\n"
    );
}

#[test]
fn conditional_command_operators() {
    assert_eq!(out("[[ abc == a* ]] && echo yes"), "yes\n");
    assert_eq!(out("[[ abc == \"a*\" ]] || echo literal"), "literal\n");
    assert_eq!(out("[[ abc != z* ]] && echo ne"), "ne\n");
    assert_eq!(out("[[ -z \"\" ]] && echo empty"), "empty\n");
    assert_eq!(out("[[ -n abc ]] && echo nonempty"), "nonempty\n");
    assert_eq!(out("[[ 3 -gt 2 ]] && echo gt"), "gt\n");
    assert_eq!(out("[[ a < b ]] && echo lt"), "lt\n");
    assert_eq!(
        out("[[ ! -e /definitely/not/here ]] && echo missing"),
        "missing\n"
    );
    assert_eq!(out("[[ abc == abc && 1 -eq 1 ]] && echo both"), "both\n");
    assert_eq!(out("[[ ( abc == abc ) || x ]] && echo paren"), "paren\n");
    assert_eq!(out("x=abc; [[ $x == a* ]] && echo var"), "var\n");
    let dir = TestDir::new("cond");
    let file = dir.at("f");
    std::fs::write(&file, "x").unwrap();
    assert_eq!(out(&format!("[[ -f {file} ]] && echo file")), "file\n");
    assert_eq!(out(&format!("[[ -d {file} ]] || echo notdir")), "notdir\n");
    // `[[ ]]` failure is a status, not an error.
    assert_eq!(out("[[ 1 -eq 2 ]]; echo $?"), "1\n");
}

#[test]
fn globbing_matches_quoting_and_ranges() {
    let dir = TestDir::new("glob");
    for name in ["a.txt", "b.txt", "c.md", ".hidden"] {
        std::fs::write(dir.path().join(name), "").unwrap();
    }
    std::fs::create_dir_all(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/deep.txt"), "").unwrap();
    let pattern = |p: &str| format!("cd {dir} && echo {p}", dir = dir.path().display());

    assert_eq!(out(&pattern("*.txt")), "a.txt b.txt\n");
    assert_eq!(out(&pattern("?.txt")), "a.txt b.txt\n");
    assert_eq!(out(&pattern("[ab].txt")), "a.txt b.txt\n");
    assert_eq!(out(&pattern("[!a].txt")), "b.txt\n");
    assert_eq!(
        out(&pattern("\"*.txt\"")),
        "*.txt\n",
        "quoting inhibits globbing"
    );
    assert_eq!(
        out(&pattern("*.nomatch")),
        "*.nomatch\n",
        "no match leaves the word"
    );
    assert_eq!(out(&pattern("sub/*.txt")), "sub/deep.txt\n");
    assert_eq!(out(&pattern("*/*.txt")), "sub/deep.txt\n");
    // Hidden files are not matched by a wildcard (no dotglob).
    assert_eq!(out(&pattern("*")), "a.txt b.txt c.md sub\n");
}

// ---------------------------------------------------------------------------
// Redirections
// ---------------------------------------------------------------------------

#[test]
fn output_redirections_write_files() {
    let dir = TestDir::new("redirect-out");
    let file = dir.at("f");
    assert_eq!(out(&format!("echo one > {file}; cat {file}")), "one\n");
    assert_eq!(
        out(&format!(
            "echo one > {file}; echo two >> {file}; cat {file}"
        )),
        "one\ntwo\n"
    );
    assert_eq!(
        out(&format!("echo again > {file}; cat {file}")),
        "again\n",
        "> truncates"
    );
    // A redirection on a compound command covers the whole command.
    assert_eq!(
        out(&format!("{{ echo a; echo b; }} > {file}; cat {file}")),
        "a\nb\n"
    );
    assert_eq!(
        out(&format!(
            "for i in 1 2; do echo $i; done > {file}; cat {file}"
        )),
        "1\n2\n"
    );
}

#[test]
fn input_redirections_and_here_documents() {
    let dir = TestDir::new("redirect-in");
    let file = dir.at("f");
    std::fs::write(&file, "line\n").unwrap();
    assert_eq!(out(&format!("cat < {file}")), "line\n");
    assert_eq!(out("cat <<EOF\nplain\nEOF"), "plain\n");
    assert_eq!(out("x=v; cat <<EOF\nv=$x\nEOF"), "v=v\n");
    assert_eq!(out("cat <<'EOF'\nlit $x\nEOF"), "lit $x\n");
    assert_eq!(out("cat <<-EOF\n\tstripped\nEOF"), "stripped\n");
    assert_eq!(out("cat <<< word"), "word\n");
    assert_eq!(
        out("for i in 1 2; do cat <<EOF\nline $i\nEOF\ndone"),
        "line 1\nline 2\n"
    );
    // A body larger than a pipe buffer must not deadlock (DESIGN.md §4.4).
    // It goes through a file because a single `-c` argument is limited to
    // MAX_ARG_STRLEN.
    let body: String = "x".repeat(200_000);
    let big = dir.at("big.sh");
    std::fs::write(&big, format!("cat <<EOF | wc -c\n{body}\nEOF\n")).unwrap();
    let output = baash().arg(&big).output().unwrap();
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "200001");
}

#[test]
fn descriptor_duplication_and_ordering() {
    assert_eq!(out("echo hi 2>&1 | cat"), "hi\n");
    // `2>&1 >file` duplicates the *terminal* stdout onto stderr, then moves
    // stdout to the file: stderr still reaches the pipe (DESIGN.md §4.4).
    let dir = TestDir::new("redirect-order");
    let file = dir.at("f");
    let script = format!("{{ echo out >&2; }} 2>&1 > {file} | cat; echo file:; cat {file}");
    assert_eq!(out(&script), "out\nfile:\n");
    assert_eq!(out("echo both &> /dev/null; echo $?"), "0\n");
    assert_eq!(
        out(&format!(
            "echo x &> {file}; cat {file}",
            file = dir.at("both")
        )),
        "x\n"
    );
    // A descriptor that `exec` opened stays open for later commands.
    let three = dir.at("three");
    assert_eq!(
        out(&format!(
            "exec 3> {three}; echo via3 >&3; exec 3>&-; cat {three}"
        )),
        "via3\n"
    );
}

#[test]
fn redirection_failures_are_status_one() {
    let output = run("echo hi > /definitely/not/a/dir/file");
    assert_eq!(code(&output), 1);
    assert!(
        stderr(&output).contains("BAASH-E0202"),
        "{}",
        stderr(&output)
    );
    // An ambiguous redirect never opens anything.
    let output = run("x='a b'; echo hi > $x");
    assert_eq!(code(&output), 1);
    assert!(stderr(&output).contains("ambiguous"), "{}", stderr(&output));
}

// ---------------------------------------------------------------------------
// Pipelines and processes
// ---------------------------------------------------------------------------

#[test]
fn pipelines_connect_stages() {
    assert_eq!(out("echo hi | cat"), "hi\n");
    assert_eq!(out("echo hi | cat | cat"), "hi\n");
    assert_eq!(out("printf 'b\\na\\n' | sort"), "a\nb\n");
    assert_eq!(out("echo hi | wc -l").trim(), "1");
    // Status is the last stage's.
    assert_eq!(out("false | true; echo $?"), "0\n");
    assert_eq!(out("true | false; echo $?"), "1\n");
    // With `pipefail` it is the last (rightmost) non-zero stage's.
    assert_eq!(out("set -o pipefail; false | true; echo $?"), "1\n");
    assert_eq!(
        out("set -o pipefail; sh -c 'exit 3' | sh -c 'exit 4' | true; echo $?"),
        "4\n"
    );
}

#[test]
fn pipefail_is_sampled_when_the_pipeline_starts() {
    // POSIX 2024 §2.9.2, DESIGN.md §2. `set -o pipefail` inside the last stage
    // runs in that stage's process, so the parent's setting does not change —
    // but the point of the rule is that a *later* change must not retroactively
    // judge an earlier pipeline either.
    assert_eq!(
        out("false | set -o pipefail; echo $?"),
        "0\n",
        "the pipeline started without pipefail"
    );
    assert_eq!(
        out("set -o pipefail; false | set +o pipefail; echo $?"),
        "1\n"
    );
}

#[test]
fn negation_and_pipeline_status() {
    assert_eq!(out("! true; echo $?"), "1\n");
    assert_eq!(out("! false; echo $?"), "0\n");
    assert_eq!(out("! false | false; echo $?"), "0\n");
}

#[test]
fn subshells_are_isolated() {
    assert_eq!(out("(echo sub)"), "sub\n");
    assert_eq!(out("x=1; (x=2; echo $x); echo $x"), "2\n1\n");
    assert_eq!(out("(exit 7); echo $?"), "7\n");
    assert_eq!(
        out("x=1; { x=2; }; echo $x"),
        "2\n",
        "brace groups are not subshells"
    );
    // Positional parameters are per-process too.
    assert_eq!(out("set -- a; (set -- b; echo $1); echo $1"), "b\na\n");
}

#[test]
fn background_jobs_set_the_last_pid() {
    assert_eq!(
        out("sleep 0.05 & echo \"$!\" | grep -qE '^[0-9]+$' && echo numeric"),
        "numeric\n"
    );
    // `&` does not wait, so the next command runs immediately.
    let output = run("sleep 5 & echo after");
    assert_eq!(code(&output), 0);
    assert_eq!(stdout(&output), "after\n");
}

// ---------------------------------------------------------------------------
// Compound commands
// ---------------------------------------------------------------------------

#[test]
fn if_elif_else() {
    assert_eq!(out("if true; then echo t; else echo f; fi"), "t\n");
    assert_eq!(out("if false; then echo t; else echo f; fi"), "f\n");
    assert_eq!(
        out("if false; then echo t; elif true; then echo e; else echo f; fi"),
        "e\n"
    );
    assert_eq!(out("if false; then echo t; fi; echo $?"), "0\n");
    assert_eq!(out("if true; then false; fi; echo $?"), "1\n");
}

#[test]
fn for_loops() {
    assert_eq!(out("for i in 1 2 3; do echo $i; done"), "1\n2\n3\n");
    assert_eq!(out("for i in; do echo $i; done; echo done"), "done\n");
    assert_eq!(
        out("set -- a b; for i; do echo $i; done"),
        "a\nb\n",
        "a `for` with no list iterates the positional parameters"
    );
    assert_eq!(
        out("for i in a b; do for j in 1 2; do echo $i$j; done; done"),
        "a1\na2\nb1\nb2\n"
    );
}

#[test]
fn while_until_and_break_continue() {
    assert_eq!(
        out("i=0; while [ $i -lt 3 ]; do echo $i; i=$((i+1)); done"),
        "0\n1\n2\n"
    );
    assert_eq!(
        out("i=0; until [ $i -ge 2 ]; do echo $i; i=$((i+1)); done"),
        "0\n1\n"
    );
    assert_eq!(
        out("for i in 1 2 3; do [ $i = 2 ] && continue; echo $i; done"),
        "1\n3\n"
    );
    assert_eq!(
        out("for i in 1 2 3; do [ $i = 2 ] && break; echo $i; done"),
        "1\n"
    );
    assert_eq!(
        out("for i in 1 2; do for j in a b; do break 2; done; done; echo out"),
        "out\n"
    );
    assert_eq!(
        out("for i in 1 2; do for j in a b; do continue 2; done; echo no; done; echo out"),
        "out\n"
    );
    assert_eq!(out("while false; do :; done; echo $?"), "0\n");
}

#[test]
fn case_with_all_three_terminators() {
    assert_eq!(out("case abc in a*) echo A;; *) echo B;; esac"), "A\n");
    assert_eq!(out("case xyz in a*) echo A;; *) echo B;; esac"), "B\n");
    assert_eq!(
        out("case b in a|b|c) echo listed;; esac"),
        "listed\n",
        "`|` gives alternative patterns"
    );
    // `;;` stops; `;&` falls into the next body; `;;&` keeps testing patterns.
    assert_eq!(
        out("case a in a) echo one;;& b) echo two;;& *) echo three;; esac"),
        "one\nthree\n"
    );
    assert_eq!(
        out("case a in a) echo one;& b) echo two;; esac"),
        "one\ntwo\n"
    );
    assert_eq!(out("case z in a) echo A;; esac; echo $?"), "0\n");
    assert_eq!(out("x=abc; case $x in a*) echo var;; esac"), "var\n");
}

#[test]
fn functions_take_arguments_and_return() {
    assert_eq!(out("f() { echo in; }; f"), "in\n");
    assert_eq!(out("f() { echo $1 $2; }; f a b"), "a b\n");
    assert_eq!(out("f() { echo $#; }; f a b c"), "3\n");
    assert_eq!(out("f() { echo \"$@\"; }; f 'a b' c"), "a b c\n");
    assert_eq!(out("f() { return 4; }; f; echo $?"), "4\n");
    assert_eq!(out("f() { return; }; false; f; echo $?"), "1\n");
    // A function sees its own positionals and restores the caller's.
    assert_eq!(
        out("f() { set -- x; echo $1; }; set -- a; f; echo $1"),
        "x\na\n"
    );
    // Redefinition replaces the body.
    assert_eq!(out("f() { echo one; }; f() { echo two; }; f"), "two\n");
    // Recursion, with arithmetic as the counter.
    assert_eq!(
        out("f() { if [ $1 -le 1 ]; then echo 1; else echo $1; f $(($1 - 1)); fi; }; f 3"),
        "3\n2\n1\n"
    );
    assert_eq!(
        out("f() { local v=inner; echo $v; }; v=outer; f; echo $v"),
        "inner\nouter\n"
    );
    assert_eq!(out("function g { echo keyword; }; g"), "keyword\n");
    assert_eq!(out("f() { echo body > /dev/null; }; f; echo $?"), "0\n");
}

#[test]
fn eval_and_source_run_in_the_current_environment() {
    assert_eq!(out("eval 'x=5; echo $x'"), "5\n");
    assert_eq!(out("x=1; eval 'x=2'; echo $x"), "2\n");
    assert_eq!(out("eval 'if true; then echo nested; fi'"), "nested\n");
    let output = run("eval 'exit 3'; echo no");
    assert_eq!(code(&output), 3);
    assert_eq!(stdout(&output), "");

    let dir = TestDir::new("source");
    let file = dir.at("lib.sh");
    std::fs::write(&file, "sourced=yes\n").unwrap();
    assert_eq!(
        out(&format!(". {file}; echo $sourced")),
        "yes\n",
        "dot sources in the current shell"
    );
    assert_eq!(out(&format!("source {file}; echo $sourced")), "yes\n");
}

#[test]
fn exec_replaces_the_command_and_keeps_redirects() {
    assert_eq!(out("exec echo replaced"), "replaced\n");
    assert_eq!(out("exec echo replaced; echo not-run"), "replaced\n");
    let dir = TestDir::new("exec");
    let file = dir.at("f");
    let output = run(&format!("exec echo hi > {file}; echo after"));
    assert_eq!(code(&output), 0);
    assert_eq!(stdout(&output), "");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "hi\n");
    // `exec` with only redirections changes the shell's own descriptor table.
    assert_eq!(
        out(&format!(
            "exec 4> {file}; echo via4 >&4; exec 4>&-; cat {file}"
        )),
        "via4\n"
    );
}

// ---------------------------------------------------------------------------
// Unwinding: errexit
// ---------------------------------------------------------------------------

#[test]
fn errexit_exits_on_a_failing_command() {
    let output = run("set -e; false; echo unreached");
    assert_eq!(code(&output), 1);
    assert_eq!(stdout(&output), "");
    assert_eq!(out("set -e; true; echo reached"), "reached\n");
    assert_eq!(out("set -e; false || true; echo reached"), "reached\n");
    assert_eq!(out("set -e; ! false; echo reached"), "reached\n");
    assert_eq!(out("set -e; ! true; echo reached"), "reached\n");
    // Conditions are exempt.
    assert_eq!(
        out("set -e; if false; then :; fi; echo reached"),
        "reached\n"
    );
    assert_eq!(
        out("set -e; while false; do :; done; echo reached"),
        "reached\n"
    );
    assert_eq!(
        out("set -e; until true; do :; done; echo reached"),
        "reached\n"
    );
    // Only the last command of an `&&`/`||` list is judged.
    assert_eq!(out("set -e; false && echo no; echo reached"), "reached\n");
    // A non-final pipeline stage is exempt; the pipeline's status is the
    // last stage's.
    assert_eq!(out("set -e; false | true; echo reached"), "reached\n");
    // A failing pipeline at the end is not exempt.
    let output = run("set -e; true | false; echo unreached");
    assert_eq!(code(&output), 1);
    assert_eq!(stdout(&output), "");
    // A function called in an ignored context does not exit the shell.
    assert_eq!(
        out("set -e; f() { false; }; if f; then :; fi; echo reached"),
        "reached\n"
    );
    let output = run("set -e; f() { false; }; f; echo unreached");
    assert_eq!(code(&output), 1);
    assert_eq!(stdout(&output), "");
    // `set +e` re-arms normal behaviour.
    assert_eq!(out("set -e; set +e; false; echo reached"), "reached\n");
}

#[test]
fn errexit_is_reset_inside_a_command_substitution() {
    // bash resets `errexit` for the body of a command substitution: a failure
    // contributes to the substitution's status and the body keeps going.
    assert_eq!(
        out("set -e; echo $(echo one; false; echo two); echo parent=$?"),
        "one two\nparent=0\n"
    );
    // `shopt -s inherit_errexit` is the opt-in to the other behaviour.
    assert_eq!(
        out("set -e; shopt -s inherit_errexit; echo $(echo one; false; echo two)"),
        "one\n"
    );
    // `set -e` written by the body itself still applies to the body.
    assert_eq!(
        out("echo $(set -e; false; echo two); echo st=$?"),
        "\nst=0\n"
    );
    // The substitution's status still reaches an assignment, and `-e` then
    // ends the shell.
    let output = run("set -e; x=$(false); echo unreached");
    assert_eq!(code(&output), 1);
    assert_eq!(stdout(&output), "");
}

#[test]
fn shellopts_and_bashopts_are_derived_from_the_live_options() {
    // The lists follow the option table, alphabetically, and the `set -o`
    // spelling of the shared option is hyphenated.
    assert_eq!(
        out("echo $SHELLOPTS"),
        "braceexpand:hashall:interactive-comments\n"
    );
    assert_eq!(
        out("set -e; echo $SHELLOPTS"),
        "braceexpand:errexit:hashall:interactive-comments\n"
    );
    assert_eq!(
        out("set -e; set +e; echo $SHELLOPTS"),
        "braceexpand:hashall:interactive-comments\n"
    );
    assert!(
        out("echo $BASHOPTS")
            .split(':')
            .any(|name| name == "promptvars")
    );
    // Both exist for `test -v` and `${x:?}`.
    assert_eq!(
        out("test -v SHELLOPTS && test -v BASHOPTS && echo both"),
        "both\n"
    );
    // They are readonly, and a readonly assignment is a POSIX fatal error in a
    // non-interactive shell (see `a_failing_special_builtin_exits_a_non_interactive_shell`).
    let output = run("SHELLOPTS=x; echo unreached");
    assert_eq!(code(&output), 1);
    assert_eq!(stdout(&output), "");
    assert!(stderr(&output).contains("readonly"));
}

#[test]
fn an_exported_shellopts_turns_tracing_on_in_a_child() {
    let output = baash()
        .env("SHELLOPTS", "xtrace:braceexpand")
        .arg("-c")
        .arg("echo hi")
        .output()
        .expect("spawn baash");
    assert_eq!(code(&output), 0);
    assert_eq!(stdout(&output), "hi\n");
    assert_eq!(stderr(&output), "+ echo hi\n");
}

#[test]
fn echo_e_understands_unicode_escapes() {
    assert_eq!(out("echo -e 'abcd\\u0065f'"), "abcdef\n");
    assert_eq!(out("echo -e 'abcd\\U00000065f'"), "abcdef\n");
    // An incomplete escape is the character itself.
    assert_eq!(
        out("echo -en 'abcd\\u006' | od -An -c | tr -s ' '"),
        " a b c d 006\n"
    );
    // A surrogate or an out-of-range value stays literal, as does an escape
    // with no digits at all.
    assert_eq!(out("echo -e 'a\\uD800'"), "a\\uD800\n");
    assert_eq!(out("echo -e 'a\\U00110000'"), "a\\U00110000\n");
    assert_eq!(out("echo -e 'a\\u'"), "a\\u\n");
    // Eight digits take at most eight: the ninth is text.
    assert_eq!(out("echo -e 'a\\U000000651'"), "ae1\n");
}

#[test]
fn a_malformed_conditional_is_rejected_before_the_chunk_runs() {
    // bash's parser rejects the whole line: `echo a` does not run.
    let output = run("echo a; [[ ]]; echo b");
    assert_eq!(code(&output), 2);
    assert_eq!(stdout(&output), "");
    for body in ["[[ -z ]]", "[[ && ]]", "[[ ) ]]", "[[ ( ]]", "[[ -f < ]]"] {
        let output = run(&format!("echo a; {body}; echo b"));
        assert_eq!(code(&output), 2, "{body} must be a syntax error");
        assert_eq!(stdout(&output), "", "{body}");
    }
    // Operands that *look* like operators are words, as in bash.
    assert_eq!(out("[[ = ]] && echo t"), "t\n");
    assert_eq!(out("[[ == ]] && echo t"), "t\n");
    assert_eq!(out("[[ '!' == ! ]] && echo t"), "t\n");
    assert_eq!(out("[[ -f == ]]; echo st=$?"), "st=1\n");
    // Control operators split words inside a conditional; `!` does not.
    assert_eq!(out("[[ ''||! (1 == 2)&&(2 == 2)]] && echo true"), "true\n");
    assert_eq!(out("[[ a!b == a!b ]] && echo t"), "t\n");
    let output = run("[[ a|b ]]");
    assert_eq!(code(&output), 2);
}

#[test]
fn set_option_parsing_matches_bash() {
    // A lone `-` ends option processing (turning off -v/-x) and the words after
    // it are positional; a lone `+` is ignored and parsing continues.
    assert_eq!(out("set a b; set - a b; echo \"$@\""), "a b\n");
    assert_eq!(out("set - -; echo \"$@\""), "-\n");
    assert_eq!(out("set -- --; echo \"$@\""), "--\n");
    assert_eq!(
        out("set -x - -v; case $- in *v*) echo v;; esac; echo argv \"$@\""),
        "argv -v\n"
    );
    assert_eq!(out("set a b; set +; echo \"$@\""), "a b\n");
    assert_eq!(out("set -x + -v x y; echo \"$@\""), "x y\n");
    assert_eq!(out("set - +; echo \"$@\""), "+\n");
    assert_eq!(out("set + -; echo \"$@\""), "\n");
    // An invalid name is status 2 but not fatal, even in a non-interactive
    // script (bash's behavior; POSIX would exit).
    assert_eq!(out("set -o STRICT; echo st=$?"), "st=2\n");
}

#[test]
fn set_a_exports_declaration_builtin_variables_and_set_quotes_values() {
    assert_eq!(
        out("set -a; f() { local z=zzz; declare -p z | grep -q -- '-x' && echo exported; }; f"),
        "exported\n"
    );
    assert_eq!(
        out("set -a; declare q=qux; declare -p q | grep -q -- '-x' && echo exported"),
        "exported\n"
    );
    // `set` prints values re-input-safe: a value with a newline uses `$'…'`
    // so it stays on one line. Control bytes are spelled `\u000a`, the form
    // `declare -p` uses; either spelling re-reads as the same value.
    assert_eq!(out("x='a b'; set | grep '^x='"), "x='a b'\n");
    assert_eq!(out("x=$'a\nb'; set | grep '^x='"), "x=$'a\\u000ab'\n");
}

#[test]
fn break_and_continue_outside_a_loop_warn_and_continue() {
    // bash warns on stderr and keeps running; the status is zero.
    assert_eq!(out("echo a; continue; echo b; echo st=$?"), "a\nb\nst=0\n");
    assert_eq!(out("echo a; break; echo st=$?"), "a\nst=0\n");
    assert_eq!(out("echo a; break 2; echo st=$?"), "a\nst=0\n");
    // A `break` in a loop's condition leaves that loop rather than the script.
    assert_eq!(out("while break; do echo x; done; echo done"), "done\n");
    // Extra words are a usage error, not a count: `continue 1 2 3` used to
    // loop forever.
    let output = run("while true; do continue 1 2 3; done; echo unreached");
    assert_eq!(code(&output), 1);
    assert_eq!(stdout(&output), "");
}

#[test]
fn eval_and_source_control_flow_reaches_their_caller() {
    // `eval` behaves as if its text appeared in place.
    assert_eq!(
        out("f() { for i in 1 2 3; do eval 'continue'; echo $i; done; }; f"),
        ""
    );
    assert_eq!(
        out("f() { for i in 1 2 3; do echo $i; [ $i = 2 ] && eval 'break'; done; }; f"),
        "1\n2\n"
    );
    assert_eq!(
        out("f() { eval 'return 7'; echo unreached; }; f; echo st=$?"),
        "st=7\n"
    );

    // `source` propagates `break`/`continue` too, but `return` ends the file,
    // not the caller's function.
    let dir = TestDir::new("source-control");
    std::fs::write(dir.path().join("break.sh"), "break\n").unwrap();
    std::fs::write(
        dir.path().join("return.sh"),
        "echo one; return 3; echo two\n",
    )
    .unwrap();
    assert_eq!(
        out(&format!(
            "for i in 1 2 3; do echo $i; [ $i = 2 ] && . '{}'; done",
            dir.at("break.sh")
        )),
        "1\n2\n"
    );
    assert_eq!(
        out(&format!(
            "f() {{ . '{}'; echo after=$?; }}; f",
            dir.at("return.sh")
        )),
        "one\nafter=3\n"
    );
}

#[test]
fn shopt_rejects_an_unknown_name_with_status_one() {
    let output = run("shopt -s nosuch; echo st=$?");
    assert_eq!(code(&output), 0);
    assert_eq!(stdout(&output), "st=1\n");
    assert!(stderr(&output).contains("invalid shell option name"));
    // Known names around it are still processed, and `-p` keeps the readable
    // form for both tables.
    assert_eq!(
        out("shopt -s nullglob nosuch; shopt -q nullglob; echo st=$?"),
        "st=0\n"
    );
    assert_eq!(out("shopt -p nullglob"), "shopt -u nullglob\n");
    assert_eq!(out("shopt -p -o errexit"), "set +o errexit\n");
}

#[test]
fn break_and_continue_only_unwind_to_their_loop() {
    // A `break` inside a function unwinds outwards, as bash does.
    assert_eq!(
        out("f() { break; }; for i in 1 2 3; do f; echo body; done; echo out"),
        "out\n"
    );
    assert_eq!(
        out("f() { for i in 1 2; do return 0; done; }; f; echo after"),
        "after\n"
    );
}

#[test]
fn errexit_is_inherited_by_compound_commands() {
    // A compound command as the *final* command of a list is not exempt: the
    // failure inside its body exits the shell.
    let output = run("set -e; { false; echo unreached; }; echo unreached");
    assert_eq!(code(&output), 1);
    assert_eq!(stdout(&output), "");

    let output = run("set -e; f() { false; echo unreached; }; true && f; echo unreached");
    assert_eq!(code(&output), 1);
    assert_eq!(stdout(&output), "");

    // A *non-final* `&&`/`||` operand is exempt, and its body inherits the
    // exemption — so the body keeps running and the list continues.
    assert_eq!(out("set -e; { false; } && true; echo after"), "after\n");
    assert_eq!(
        out("set -e; f() { false; echo body; }; f && true; echo after"),
        "body\nafter\n"
    );
    // `!` is an ignore context too.
    assert_eq!(
        out("set -e; ! { false; echo body; }; echo after"),
        "body\nafter\n"
    );
    // A pipeline stage is its own process and is *not* exempt internally, even
    // though the pipeline's own failure never exits the shell here.
    assert_eq!(
        out("set -e; { false; echo unreached; } | cat; echo after"),
        "after\n"
    );
}

#[test]
fn a_failing_special_builtin_exits_a_non_interactive_shell() {
    // POSIX §2.8.1 says a non-interactive shell exits when a *special* built-in
    // fails; bash (without --posix) continues. `baash_runtime::consequence` is
    // the landed authority for that table and encodes POSIX, so this is the
    // documented deviation (see `baash_vm::tree`'s module docs).
    let output = run("set -- one; shift 5; echo unreached");
    assert_eq!(code(&output), 1);
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output).contains("BAASH-E0505"),
        "{}",
        stderr(&output)
    );
    // A failing *ordinary* built-in is not fatal.
    assert_eq!(
        out("cd /definitely/not/here || echo recovered"),
        "recovered\n"
    );
}

/// Run the binary with explicit arguments rather than a `-c` script.
fn run_args(args: &[&str]) -> Output {
    baash().args(args).output().expect("spawn baash")
}

#[test]
fn shell_options_on_the_command_line_reach_the_shell() {
    // `$SH -e -c …` is how the conformance suite invokes the shell, so these
    // have to work before any spec case can pass.
    let output = run_args(&["-e", "-c", "false; echo after"]);
    assert_eq!(code(&output), 1);
    assert_eq!(stdout(&output), "");

    let output = run_args(&["-e", "-c", "set +e; false; echo after"]);
    assert_eq!(stdout(&output), "after\n");

    let output = run_args(&["-o", "pipefail", "-c", "false | true"]);
    assert_eq!(code(&output), 1);

    // An unknown option is a usage error, not a silent no-op.
    assert_eq!(code(&run_args(&["-Q", "-c", "echo hi"])), 2);
    assert_eq!(code(&run_args(&["-o", "nope", "-c", "echo hi"])), 2);

    // The flags show up in `$-`.
    let output = run_args(&["-e", "-c", "case $- in *e*) echo yes;; *) echo no;; esac"]);
    assert_eq!(stdout(&output), "yes\n");
}

#[test]
fn nounset_errors_only_where_bash_does() {
    // Reading an unset parameter is fatal in a non-interactive shell.
    let output = run_args(&["-u", "-c", "echo \"$nope\""]);
    assert_eq!(code(&output), 1);
    assert!(stderr(&output).contains("parameter not set"));

    // Operators that supply their own value are exempt.
    let output = run_args(&["-u", "-c", "echo \"${nope:-fallback}\""]);
    assert_eq!(stdout(&output), "fallback\n");

    // Enumeration is never an unbound variable, even when it yields nothing:
    // `set -u; unset a; echo "${a[@]}"` succeeds in bash.
    for script in [
        "set -u; set --; echo \"$@\"",
        "set -u; echo \"$*\"",
        "set -u; a=(); echo \"${a[@]}\"",
        "set -u; unset a; echo \"${a[@]}\"",
    ] {
        let output = run_args(&["-c", script]);
        assert_eq!(code(&output), 0, "{script}");
    }

    // A single unset element is an error, as in bash.
    let output = run_args(&["-u", "-c", "a=(1); echo \"${a[5]}\""]);
    assert_eq!(code(&output), 1);
}

#[test]
fn case_conversion_applies_to_lists() {
    let output = run("set -- ab cd; echo \"${@^^}\"");
    assert_eq!(stdout(&output), "AB CD\n");
    let output = run("a=(xy zz); echo \"${a[@]^^}\"");
    assert_eq!(stdout(&output), "XY ZZ\n");
}

#[test]
fn failglob_fails_the_command_but_not_the_shell() {
    // `shopt -s failglob; echo *.ZZ; echo status=$?` must print status=1 and
    // keep going, which is what bash does in a script. Only a `${x:?}` error
    // ends a non-interactive shell.
    let output = run("shopt -s failglob; echo *.ZZ; echo status=$?");
    assert_eq!(stdout(&output), "status=1\n");
    assert_eq!(code(&output), 0);
    assert!(stderr(&output).contains("no match"));

    let output = run("echo \"${nope:?gone}\"; echo after");
    assert_eq!(code(&output), 1);
    assert_eq!(stdout(&output), "");
}

#[test]
fn the_locale_decides_whether_length_counts_bytes_or_characters() {
    // Same bytes, two answers. `μ` is two bytes and one character; under
    // LC_ALL=C bash and dash both count 3 here, which is why the default must
    // be bytes rather than a guess from the terminal.
    let run_locale = |locale: &str| {
        let output = baash()
            .env("LC_ALL", locale)
            .arg("-c")
            .arg(r#"v=$(printf "\316\274x"); echo "${#v}""#)
            .output()
            .expect("spawn baash");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    assert_eq!(run_locale("C"), "3");
    assert_eq!(run_locale("C.UTF-8"), "2");
}

#[test]
fn dash_o_sets_a_shopt_option_before_parsing() {
    // `-O extglob` is parse-time, like bash: it is the only way a *script* can
    // be parsed with `!(…)` as a group rather than pipeline negation.
    let output = baash()
        .args(["-O", "extglob", "-c", "case abc in @(a|b)*) echo m;; esac"])
        .output()
        .expect("spawn baash");
    assert_eq!(stdout(&output), "m\n");

    // Without the flag the line still *parses* (the lexer is a superset of
    // bash's) but `@(…)` is not a pattern, so the arm does not match. bash
    // would report a syntax error here; the divergence is deliberate and
    // documented in the lexer.
    let output = run("case abc in @(a|b)*) echo m;; *) echo n;; esac");
    assert_eq!(stdout(&output), "n\n");

    let output = baash()
        .args(["-O", "nope", "-c", "echo x"])
        .output()
        .expect("spawn baash");
    assert_eq!(code(&output), 2, "an unknown shopt name must be rejected");
}

#[test]
fn the_default_startup_file_is_the_xdg_config_one() {
    let dir = TestDir::new("xdg-rc");
    let home = dir.path().join("home");
    let config = dir.path().join("config");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(config.join("baash")).expect("create config");

    // A `~/.bashrc` (read by every shell on this machine) and the pre-XDG
    // `~/.baashrc`, which this shell no longer looks at.
    std::fs::write(home.join(".bashrc"), "echo bashrc\n").expect("write .bashrc");
    std::fs::write(home.join(".baashrc"), "echo legacy\n").expect("write .baashrc");

    let run_with = |config_dir: Option<&Path>| {
        let mut command = baash();
        command
            .env("HOME", &home)
            .env_remove("XDG_DATA_HOME")
            .arg("-i")
            .arg("-c")
            .arg("echo cmd");
        match config_dir {
            Some(path) => command.env("XDG_CONFIG_HOME", path),
            None => command.env_remove("XDG_CONFIG_HOME"),
        };
        stdout(&command.output().expect("spawn baash"))
    };

    // No baash file anywhere: `~/.bashrc` is the fallback, and the old
    // `~/.baashrc` is not consulted at all.
    assert_eq!(run_with(None), "bashrc\ncmd\n");

    // `$HOME/.config/baash/config.sh` is the default location.
    let home_rc = home.join(".config").join("baash").join("config.sh");
    std::fs::create_dir_all(home_rc.parent().expect("parent")).expect("create home config");
    std::fs::write(&home_rc, "echo home-config\n").expect("write home config");
    assert_eq!(run_with(None), "home-config\ncmd\n");

    // `$XDG_CONFIG_HOME` replaces `$HOME/.config`, and wins over `~/.bashrc`.
    std::fs::write(config.join("baash").join("config.sh"), "echo xdg-config\n")
        .expect("write xdg config");
    assert_eq!(run_with(Some(&config)), "xdg-config\ncmd\n");

    // `--norc` still skips whichever file the default would have read.
    let output = baash()
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config)
        .args(["-i", "--norc", "-c", "echo cmd"])
        .output()
        .expect("spawn baash");
    assert_eq!(stdout(&output), "cmd\n");
}

#[test]
fn rcfile_is_read_only_for_an_interactive_shell() {
    let dir = std::env::temp_dir().join(format!("baash-rc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let rc = dir.join("rc");
    std::fs::write(&rc, "RC_VAR=from_rc\n").expect("write rc");

    let with_rc = |args: &[&str]| {
        let output = baash()
            .args(args)
            .arg("-i")
            .arg("-c")
            .arg("echo \"[$RC_VAR]\"")
            .output()
            .expect("spawn baash");
        stdout(&output).trim().to_string()
    };

    // Interactive: the file is read.
    assert_eq!(with_rc(&["--rcfile", &rc.to_string_lossy()]), "[from_rc]");
    // `--norc` wins over `--rcfile`.
    assert_eq!(
        with_rc(&["--rcfile", &rc.to_string_lossy(), "--norc"]),
        "[]"
    );
    // A missing file is ignored rather than an error, which is what makes
    // `--rcfile /dev/null` the portable way to ask for no startup file.
    assert_eq!(with_rc(&["--rcfile", "/nonexistent/rc"]), "[]");

    // Non-interactive: bash does not read a startup file at all.
    let output = baash()
        .args([
            "--rcfile",
            &rc.to_string_lossy(),
            "-c",
            "echo \"[$RC_VAR]\"",
        ])
        .output()
        .expect("spawn baash");
    assert_eq!(stdout(&output).trim(), "[]");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn conditional_integer_operands_are_arithmetic_expressions() {
    // bash evaluates each side of `-eq` as an arithmetic expression, so a
    // sum, an exponent, a base-n literal and a bare *name* all work; an
    // unset name is 0, not an error.
    for source in [
        "[[ 1+2 -eq 3 ]]",
        "[[ 2**3 -eq 8 ]]",
        "[[ 2#101 -eq 5 ]]",
        "[[ unset_name -eq 0 ]]",
    ] {
        let output = run(source);
        assert_eq!(code(&output), 0, "{source} should be true");
    }
}

#[test]
fn a_unary_test_without_an_operand_is_a_syntax_error() {
    // `[[ -z ]]` used to be quietly true (the operator read as a word). bash
    // rejects it, and so must a shell that claims good diagnostics.
    for source in ["[[ -z ]]", "[[ -n ]]", "[[ ]]", "[[ -z == -z ]]"] {
        let output = run(source);
        assert_eq!(code(&output), 2, "{source} should be a syntax error");
    }
    // With an operand it is an ordinary test again.
    assert_eq!(code(&run("[[ -z \"\" ]]")), 0);
    assert_eq!(code(&run("[[ -z a ]]")), 1);
}

// ---------------------------------------------------------------------------
// Removed job control: known names, capability diagnostics, inert `set -m`
// ---------------------------------------------------------------------------

#[test]
fn job_builtins_report_the_capability_gap() {
    // The names stay known (an rc file with `jobs` must not get "command not
    // found"), but running one says what is missing. Status 1 is the documented
    // failure for a capability rejection.
    for name in ["jobs", "fg", "bg", "disown"] {
        let output = run(name);
        assert_eq!(code(&output), 1, "`{name}` must fail");
        let text = stderr(&output);
        assert!(
            text.contains("error[BAASH-E0302]"),
            "`{name}` stderr: {text}"
        );
        assert!(
            text.contains(&format!(
                "{name}: job control is not implemented in this shell"
            )),
            "`{name}` stderr: {text}"
        );
        assert!(stdout(&output).is_empty(), "`{name}` wrote to stdout");
    }
}

#[test]
fn job_spec_operands_are_capability_errors() {
    // `wait %1` and `kill %1` used to resolve against a job table. With no
    // table they are the same capability error, not "no such job".
    for script in ["wait %1", "kill %1"] {
        let output = run(script);
        assert_eq!(code(&output), 1, "`{script}` must fail");
        let text = stderr(&output);
        assert!(text.contains("BAASH-E0302"), "`{script}` stderr: {text}");
        assert!(
            text.contains("%1: job control is not implemented in this shell"),
            "`{script}` stderr: {text}"
        );
    }
}

#[test]
fn monitor_option_is_accepted_and_inert() {
    // rc files and scripts may say `set -m`; it must parse, take effect in the
    // option table, and change no behaviour here.
    let output = run("set -m; set -o monitor; set +m; set +o monitor; echo ok");
    assert_eq!(code(&output), 0, "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "ok\n");

    // The option is still readable as an option.
    assert_eq!(out("set -m; set -o | grep '^monitor'"), "monitor\ton\n");
    assert_eq!(
        out("set -m; set +m; set -o | grep '^monitor'"),
        "monitor\toff\n"
    );
}

#[test]
fn tilde_expansion_reads_the_user_database_even_without_home() {
    // `$HOME` first, the password database second: bash only asks the
    // database when `HOME` is unset, and then it asks for the user running
    // the shell rather than for the empty login name.
    let output = baash()
        .env_remove("HOME")
        .arg("-c")
        .arg(r#"x="~"; eval y="$x"; echo "$y"; echo ~"#)
        .output()
        .expect("spawn baash");
    assert_eq!(code(&output), 0, "stderr: {}", stderr(&output));
    let text = stdout(&output);
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    assert_eq!(lines.len(), 2);
    for line in lines {
        assert!(line.starts_with('/'), "expected a home path, got {line:?}");
    }

    // `~user` still resolves a named login, and an unknown one stays literal.
    let output = baash()
        .env("HOME", "/home/bob")
        .arg("-c")
        .arg("echo ~root; echo ~nosuchuser99")
        .output()
        .expect("spawn baash");
    let text = stdout(&output);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "/root");
    assert_eq!(lines[1], "~nosuchuser99");
}

#[test]
fn declaration_operands_expand_tilde_after_the_equals() {
    // `declare x=~/p` is an assignment word even though it is an argument, so
    // the tilde that starts the *value* expands; a quoted `=` is not the
    // separator, so its tilde stays literal.
    let output = baash()
        .env("HOME", "/home/bob")
        .arg("-c")
        .arg(
            r#"readonly c=~/src; echo "$c"
declare d=~/x; echo "$d"
export e=~/y; echo "$e"
declare "q=~/p"; echo "$q"
declare a[1]=~/one; echo "${a[1]}""#,
        )
        .output()
        .expect("spawn baash");
    assert_eq!(code(&output), 0, "stderr: {}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "/home/bob/src\n/home/bob/x\n/home/bob/y\n~/p\n/home/bob/one\n"
    );

    // A declaration's value is not globbed, and neither is an array
    // subscript: the name keeps its source text.
    let output = baash()
        .arg("-c")
        .arg("declare s[1]=*; echo \"${s[1]}\"; readonly r=*; echo \"$r\"")
        .current_dir(std::env::temp_dir())
        .output()
        .expect("spawn baash");
    assert_eq!(stdout(&output), "*\n*\n");
}

#[test]
fn a_bad_array_subscript_is_an_expansion_error() {
    // An unset *name* in a subscript is arithmetic zero, but a malformed
    // expression is fatal: bash abandons the line (and a non-interactive
    // shell exits) rather than expanding to nothing.
    for script in [
        "a=(x y); echo ${a[~+]}; echo unreached",
        "a=(x y); echo ${#a[~+]}; echo unreached",
        "a=(x y); echo ${a[~+]:-d}; echo unreached",
        "a=(x y); a[~+]=1; echo unreached",
        "declare -i n; n=1+; echo unreached",
    ] {
        let output = run(script);
        assert_eq!(code(&output), 1, "expected status 1 for `{script}`");
        assert_eq!(stdout(&output), "", "expected no output for `{script}`");
        assert!(
            !stderr(&output).is_empty(),
            "expected a diagnostic for `{script}`"
        );
    }

    // The benign cases keep working: an unset name is zero, and `${a[foo]:-d}`
    // still prints the default word.
    assert_eq!(out("a=(x y); echo ${a[foo]:-d}"), "x\n");
    assert_eq!(
        out("a=(x y); declare -i n; n=1+2; echo $n ${a[1]}"),
        "3 y\n"
    );
}

#[test]
fn cd_tracks_the_logical_directory_and_pwd_reports_it() {
    let dir = TestDir::new("cd-logical");
    std::fs::create_dir_all(dir.path().join("target/sub")).expect("create target");
    std::fs::create_dir_all(dir.path().join("other")).expect("create other");
    let link = dir.at("link");
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.at("target"), &link).expect("symlink");
    #[cfg(not(unix))]
    let link = dir.at("target");

    let script = format!(
        r#"cd {link}
pwd
cd ..
pwd
cd {link}/sub
pwd
cd -L ..
pwd
cd -P {link}
pwd -P
pwd"#
    );
    let output = run(&script);
    assert_eq!(code(&output), 0, "stderr: {}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "{link}\n{root}\n{link}/sub\n{link}\n{root}/target\n{root}/target\n",
            root = dir.path().display()
        )
    );
}

#[test]
fn cd_checks_named_components_of_a_logical_path() {
    let dir = TestDir::new("cd-components");
    std::fs::create_dir_all(dir.path().join("a/b")).expect("create a/b");
    std::fs::create_dir_all(dir.path().join("c")).expect("create c");
    let root = dir.path().display();

    // `..` is resolved textually, but every component bash names must exist.
    let output = run(&format!("cd {root}/a/b/..; pwd"));
    assert_eq!(stdout(&output), format!("{root}/a\n"));
    let output = run(&format!("cd {root}; cd nonexistent/..; echo status=$?"));
    assert_eq!(stdout(&output), "status=1\n");
    assert_eq!(code(&output), 0);
    let output = run(&format!("cd {root}; cd a/nonexistent/..; echo status=$?"));
    assert_eq!(stdout(&output), "status=1\n");

    // Two operands are a usage error with bash's status 2.
    let output = run(&format!("cd {root}; cd a c"));
    assert_eq!(code(&output), 2);
    assert!(stderr(&output).contains("too many arguments"));
}

#[test]
fn cdpath_searches_a_bare_relative_name() {
    let dir = TestDir::new("cdpath");
    std::fs::create_dir_all(dir.path().join("first/target")).expect("create first");
    std::fs::create_dir_all(dir.path().join("second/target")).expect("create second");

    // The first match wins, and cd prints the directory it found because the
    // search path — not the current directory — supplied it.
    let script = format!(
        "CDPATH={first}:{second}; cd target; echo status=$?; pwd",
        first = dir.at("first"),
        second = dir.at("second")
    );
    let output = run(&script);
    assert_eq!(code(&output), 0, "stderr: {}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "{0}/first/target\nstatus=0\n{0}/first/target\n",
            dir.path().display()
        )
    );

    // An empty element means the current directory, and then nothing is
    // printed; an absolute path never searches CDPATH at all.
    std::fs::create_dir_all(dir.path().join("here/name")).expect("create here");
    let script = format!(
        "cd {root}/here; CDPATH=:{first}; cd name; echo status=$?; pwd",
        root = dir.path().display(),
        first = dir.at("first")
    );
    let output = run(&script);
    assert_eq!(
        stdout(&output),
        format!("status=0\n{0}/here/name\n", dir.path().display())
    );
}

#[test]
fn pwd_ignores_operands_and_honours_l_and_p() {
    let dir = TestDir::new("pwd-flags");
    let root = dir.path().display().to_string();

    assert_eq!(out(&format!("cd {root}; pwd /")), format!("{root}\n"));
    assert_eq!(out(&format!("cd {root}; pwd -L")), format!("{root}\n"));
    assert_eq!(out(&format!("cd {root}; pwd -P")), format!("{root}\n"));
    // An unknown flag is a usage error, not a silent success.
    let output = run("pwd -Z");
    assert_eq!(code(&output), 2);
}

#[test]
fn source_searches_path_and_accepts_a_bare_double_dash() {
    let dir = TestDir::new("source-path");
    std::fs::create_dir_all(dir.path().join("first")).expect("create first");
    std::fs::create_dir_all(dir.path().join("second")).expect("create second");
    std::fs::write(dir.path().join("first/lib.sh"), "echo from-first\n").expect("write");
    std::fs::write(dir.path().join("second/lib.sh"), "echo from-second\n").expect("write");
    std::fs::write(dir.path().join("lib.sh"), "echo from-current\n").expect("write");
    // A PATH entry that names a directory must not shadow the real file.
    std::fs::create_dir_all(dir.path().join("third/lib.sh")).expect("create decoy");

    // `PATH` order wins over the current directory.
    let script = format!(
        "PATH={first}:{second}:{third}:$PATH; . lib.sh; . -- lib.sh",
        first = dir.at("first"),
        second = dir.at("second"),
        third = dir.at("third")
    );
    let output = run(&script);
    assert_eq!(code(&output), 0, "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "from-first\nfrom-first\n");

    // The directory entry is skipped, so the second element supplies it.
    let script = format!(
        "cd {}; PATH={}:{}:$PATH; . lib.sh",
        dir.path().display(),
        dir.at("third"),
        dir.at("second")
    );
    assert_eq!(out(&script), "from-second\n");

    // A `PATH` with no match falls back to the current directory, and an
    // absolute name is never searched for.
    let script = format!(
        "cd {}; PATH=/nonexistent; . lib.sh; . {}/second/lib.sh",
        dir.path().display(),
        dir.path().display()
    );
    assert_eq!(out(&script), "from-current\nfrom-second\n");
}

#[test]
fn funcname_and_bash_source_describe_the_call_stack() {
    let dir = TestDir::new("introspect");
    std::fs::write(
        dir.path().join("lib.sh"),
        "g() {\n  echo \"g:${FUNCNAME[*]}:$(printf '%s,' \"${BASH_SOURCE[@]}\")\"\n}\n",
    )
    .expect("write lib.sh");
    std::fs::write(
        dir.path().join("main.sh"),
        format!(
            ". {lib}\nf() {{\n  echo \"f:${{FUNCNAME[*]}}\"\n  g\n  echo \"f-lines:${{BASH_LINENO[*]}}\"\n}}\nf\n",
            lib = dir.at("lib.sh")
        ),
    )
    .expect("write main.sh");

    let output = baash()
        .arg(dir.path().join("main.sh"))
        .output()
        .expect("spawn baash");
    assert_eq!(code(&output), 0, "stderr: {}", stderr(&output));
    let text = stdout(&output);
    let lines: Vec<&str> = text.lines().collect();
    // The script file is bash's `main` frame, so the stack runs innermost
    // first; BASH_SOURCE[0] is the file that *defined* `g`.
    assert_eq!(lines[0], "f:f main");
    assert_eq!(
        lines[1],
        format!(
            "g:g f main:{0}/lib.sh,{0}/main.sh,{0}/main.sh,",
            dir.path().display()
        )
    );
    // Inside `f` the stack is two frames deep: this function, called on line
    // 7, and `main` at line 0. (`g`'s frame would add the call line 4.)
    assert_eq!(lines[2], "f-lines:7 0");

    // At top level the three are unset, which `set -u` turns into an error.
    let output = run("set -u; echo \"$FUNCNAME\"; echo unreached");
    assert_eq!(code(&output), 1);
    assert_eq!(stdout(&output), "");
}

#[test]
fn trap_handlers_run_with_the_line_of_the_command_they_serve() {
    // bash reports the line of the command the trap is running for, not line 1
    // of the handler text, and the handler cannot shift the shell's own line.
    let script = "dbg() { echo \"  [$1]\"; }\ntrap 'dbg $LINENO' DEBUG\necho A\necho B\n";
    assert_eq!(out(script), "  [3]\nA\n  [4]\nB\n");

    // A compound command contributes its own event where bash has one: the
    // `case` statement and the `for` header fire, the body's commands fire
    // individually.
    let script = "dbg() { echo \"  [$1]\"; }\ntrap 'dbg $LINENO' DEBUG\ncase x in\n  x) echo arm ;;\nesac\nfor i in 1; do\n  echo body\ndone\n";
    assert_eq!(out(script), "  [3]\n  [4]\narm\n  [6]\n  [7]\nbody\n");
}

#[test]
fn the_err_trap_sees_the_failing_line_and_its_status() {
    let script = "trap 'echo \"err:$?:$LINENO\"' ERR\nfalse\necho done\n";
    assert_eq!(out(script), "err:1:2\ndone\n");
    // `errexit` still exits afterwards, with the failing command's status.
    let output = run("trap 'echo trap' ERR\nset -e\nfalse\necho unreached");
    assert_eq!(code(&output), 1);
    assert_eq!(stdout(&output), "trap\n");
}

#[test]
fn a_bad_substitution_fails_the_command_and_the_script_continues() {
    // bash does not end a script on a malformed `${…}`: the command fails and
    // the next one runs.
    assert_eq!(
        out("a='bad var name'; echo ref ${!a}; echo status=$?"),
        "status=1\n"
    );
    assert_eq!(out("echo ${undefined:}; echo status=$?"), "status=1\n");
    assert_eq!(out("s=123; echo ${s:}; echo status=$?"), "status=1\n");
    // A *valid* but odd offset still works: `${x: }` is arithmetic zero.
    assert_eq!(out("s=123; echo \"[${s: }]\""), "[123]\n");
}

#[test]
fn arithmetic_expands_its_text_before_evaluating() {
    assert_eq!(out("echo $(( ${j:-5} + 1 ))"), "6\n");
    assert_eq!(out("echo $((1 + $(echo 1)${undefined:-3}))"), "14\n");
    assert_eq!(out("echo $((1 + $((2 + 3)) + 4))"), "10\n");
    assert_eq!(out("echo $((`echo 1` + 2))"), "3\n");
    // No `$` means no expansion pass at all, so a variable name stays a name.
    assert_eq!(out("x=2; echo $(( x * 3 ))"), "6\n");
}

#[test]
fn a_regex_operand_may_hold_spaces_inside_a_group() {
    assert_eq!(out(r#"[[ 'a  b' =~ (a  b) ]] && echo one"#), "one\n");
    assert_eq!(out(r#"[[ 'a b' =~ (a b|c) ]] && echo two"#), "two\n");
    assert_eq!(out(r#"[[ ';' =~ (;) ]] && echo semi"#), "semi\n");
    // Outside a group the space ends the operand, which is a syntax error.
    let output = run(r#"[[ 'a  b' =~ a  b ]]"#);
    assert_eq!(code(&output), 2);
    // A `)` that is not part of a group closes the conditional instead.
    assert_eq!(out("if [[ (foo =~ fo) ]]; then echo yes; fi"), "yes\n");
}

#[test]
fn rcdir_loads_every_file_in_order_and_norc_skips_it() {
    let dir = TestDir::new("rcdir");
    let rc = dir.path().join("rc");
    let rcdir = dir.path().join("startup");
    std::fs::create_dir_all(&rcdir).expect("create rcdir");
    std::fs::write(&rc, "echo rcfile\n").expect("write rc");
    std::fs::write(rcdir.join("b"), "echo second\n").expect("write b");
    std::fs::write(rcdir.join("a"), "echo first\n").expect("write a");
    std::fs::create_dir_all(rcdir.join("subdir")).expect("create subdir");

    let run_with = |extra: &[&str]| {
        let mut command = baash();
        command.arg("--rcfile").arg(&rc).arg("--rcdir").arg(&rcdir);
        for argument in extra {
            command.arg(argument);
        }
        command
            .args(["-i", "-c", "echo command"])
            .output()
            .expect("spawn baash")
    };

    // Name order, not directory order, and the files run after `--rcfile`.
    assert_eq!(stdout(&run_with(&[])), "rcfile\nfirst\nsecond\ncommand\n");
    // `--norc` skips both, and a directory that does not exist is ignored.
    assert_eq!(stdout(&run_with(&["--norc"])), "command\n");
    let output = baash()
        .arg("--rcdir")
        .arg(dir.path().join("missing"))
        .args(["-i", "-c", "echo hi"])
        .output()
        .expect("spawn baash");
    assert_eq!(code(&output), 0);
    assert_eq!(stdout(&output), "hi\n");
}

#[test]
fn indirection_through_a_positional_parameter() {
    // `${!1}` re-reads the *value* of `$1` as a parameter name, subscripts and
    // all; bash supports it for every indirected name, not only variables.
    let script =
        "f() { echo \"${!1}\"; }\narray=(x y z)\nf 'array[0]'\nf 'array[1+1]'\nf 'array[@]'\n";
    assert_eq!(out(script), "x\nz\nx y z\n");

    // An operator on an unset indirection is bash's "invalid indirect
    // expansion": the command fails, the script continues.
    assert_eq!(out("echo undef=${!undef}; echo s=$?"), "undef=\ns=0\n");
    assert_eq!(out("echo undef=${!undef-word}; echo s=$?"), "s=1\n");
}

#[test]
fn a_script_without_a_shebang_runs_in_a_child_shell() {
    let dir = TestDir::new("noshebang");
    let script = dir.path().join("snork");
    std::fs::write(&script, "echo hello $BLAH\n").expect("write script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod script");
    }
    let snork = dir.at("snork");

    // bash re-executes the file with itself, so a plain shell variable is not
    // visible to it, while a prefix assignment and `exec` both pass it on.
    assert_eq!(out(&format!("BLAH=123; {snork}")), "hello\n");
    assert_eq!(out(&format!("BLAH=123 {snork}")), "hello 123\n");
    assert_eq!(out(&format!("BLAH=123; exec {snork}")), "hello\n");
    assert_eq!(out(&format!("BLAH=123 exec {snork}")), "hello 123\n");
}

#[test]
fn a_for_header_may_put_its_word_list_on_the_next_line() {
    assert_eq!(out("for i\nin one two\ndo echo $i\ndone\n"), "one\ntwo\n");
    // With no `in` at all the positional parameters are used.
    assert_eq!(out("for i\ndo echo $i\ndone\n"), "");
}

#[test]
fn the_err_trap_fires_once_per_failing_command() {
    // A compound whose status is a child's failure is not a second event.
    let script = "trap 'echo err' ERR\nfor i in 1 2; do false; done\n{ false; } \necho done\n";
    assert_eq!(out(script), "err\nerr\nerr\ndone\n");

    // A redirection failure is its own event, on the redirection's line.
    let script = "trap 'echo line=$LINENO' ERR\nfalse\n{ false\n} > /nonexistent-dir/x\necho ok\n";
    assert_eq!(out(script), "line=2\nline=4\nok\n");

    // `errtrace` exposes the call as an event of its own.
    let script = "trap 'echo err' ERR\nset -o errtrace\nf() { false; }\nf\necho ok\n";
    assert_eq!(out(script), "err\nerr\nok\n");
}

#[test]
fn the_debug_trap_runs_once_per_pipeline_stage_and_per_for_iteration() {
    let script =
        "trap 'echo D$LINENO' DEBUG\nfalse | false | false\nfor i in 1 2; do\n  echo x\n  done\n";
    assert_eq!(out(script), "D2\nD2\nD2\nD3\nD4\nx\nD3\nD4\nx\n");
    // A command substitution is a subprogram: no DEBUG trap inside it.
    let script = "trap 'echo D' DEBUG\necho \"$(echo inner)\"\n";
    assert_eq!(out(script), "D\ninner\n");
}

#[test]
fn an_action_named_like_a_signal_is_an_action() {
    // `trap err ERR` runs the function `err`; it must not be read as "reset
    // the ERR and ERR signals" the way a lone operand is.
    let script = "err() { echo handled; }\ntrap err ERR\nfalse\necho ok\n";
    assert_eq!(out(script), "handled\nok\n");
    // A lone operand, or a numeric first operand, still resets.
    assert_eq!(out("trap 'echo F' EXIT\ntrap EXIT\nexit 0\n"), "");
    assert_eq!(out("trap 'echo F' USR1\ntrap 2 EXIT\nexit 0\n"), "");
}

#[test]
fn nounset_reads_in_arithmetic_are_fatal() {
    // bash's "unbound variable" in a `$(( ))` word, a `(( ))` command and an
    // array element: the script ends with status 1 and nothing after runs.
    let output = run("set -u\nx=$(( y + 5 ))\necho unreached\n");
    assert_eq!(code(&output), 1);
    assert_eq!(stdout(&output), "");
    assert!(stderr(&output).contains("unbound variable"));

    let output = run("set -o nounset\n(( undef1++ ))\necho unreached\n");
    assert_eq!(code(&output), 1);
    assert_eq!(stdout(&output), "");

    let output = run("set -u\necho $(( undef[0] + 1 ))\necho unreached\n");
    assert_eq!(code(&output), 1);

    // A name being *assigned* is not a read, and without `nounset` an unset
    // name is zero (POSIX XCU 2.6.4).
    assert_eq!(out("unset x; (( x = 5 )); echo $x"), "5\n");
    assert_eq!(out("unset y; echo $(( y + 1 ))"), "1\n");
}

#[test]
fn reserved_words_and_braces_are_ordinary_arguments() {
    // A keyword or a brace is reserved only in command position.
    assert_eq!(out("echo }"), "}\n");
    assert_eq!(out("echo {"), "{\n");
    assert_eq!(out("echo !"), "!\n");
    assert_eq!(out("echo done if then fi"), "done if then fi\n");
    // Which is what closes `{ … }` at the *second* `}` here, not the first.
    assert_eq!(out("rbrace() { echo }; }; rbrace"), "}\n");
    assert_eq!(out("{ echo }; }"), "}\n");
}

#[test]
fn a_function_definition_tolerates_layout_and_odd_names() {
    assert_eq!(out("fun ( ) { echo in-func; }; fun"), "in-func\n");
    assert_eq!(
        out("func-name.ext ( ) { echo dot; }; func-name.ext"),
        "dot\n"
    );
    assert_eq!(out("f ( )\n{ echo nl; }\nf"), "nl\n");
    // An adjacent empty pair after a name with `=` is still an array.
    assert_eq!(out("a=(); echo ${#a[@]}"), "0\n");
}

#[test]
fn a_command_substitution_drops_nul_bytes() {
    // bash's strings are C strings, so a NUL from a command substitution is
    // dropped with a warning — and dropping it is also what lets the result be
    // handed to a child process, whose arguments cannot contain one.
    let output = run("x=$(printf 'a\\0b'); echo \"${#x}\"; echo \"$x\"");
    assert_eq!(stdout(&output), "2\nab\n");
    assert_eq!(code(&output), 0);
    assert!(
        stderr(&output).contains("ignored null byte"),
        "expected a warning, got {:?}",
        stderr(&output)
    );
    // A substitution with no NUL is untouched.
    let output = run("x=$(printf 'ab'); echo \"${#x}$x\"");
    assert_eq!(stdout(&output), "2ab\n");
    assert!(!stderr(&output).contains("ignored null byte"));
}

#[test]
fn a_substitution_that_is_only_a_read_redirect_returns_the_file() {
    let dir = TestDir::new("read-redirect");
    let path = dir.at("data.txt");
    std::fs::write(&path, "one\ntwo\n\n").expect("write data");
    assert_eq!(out(&format!("echo \"$(< {path})\"")), "one\ntwo\n");
    assert_eq!(out(&format!("x=`< {path}`; echo \"$x\"")), "one\ntwo\n");

    // A real command is still a real command, and a missing file reports.
    assert_eq!(out("echo $(echo hi)"), "hi\n");
    let output = run("echo \"$(< /no-such-file-here)\"; echo s=$?");
    assert_eq!(stdout(&output), "\ns=0\n");
    assert!(stderr(&output).contains("No such file"));
}

#[test]
fn file_descriptors_can_be_moved_and_duplicated_onto_themselves() {
    let dir = TestDir::new("fd-move");
    let path = dir.at("moved.txt");
    let script = format!(
        ": 3>&3; echo self-dup\n\
         exec 5>{path}; exec 6>&5-; echo hi >&5; echo ho >&6; exec 6>&-; cat {path}"
    );
    let output = run(&script);
    assert_eq!(stdout(&output), "self-dup\nho\n");
    // `5` was closed by the move, so the write to it fails (and says so).
    assert!(stderr(&output).contains("Bad file descriptor"));
}

#[test]
fn an_assignment_only_command_redirects_before_it_expands() {
    // POSIX XCU 2.9.1: no command word means the redirections happen first,
    // so the stderr of a command substitution inside the value is redirected.
    let dir = TestDir::new("assign-redirect");
    let path = dir.at("stderr.txt");
    let script = format!("x=$(echo out; echo err 1>&2) 2>{path}\necho \"x=$x\"\ncat {path}");
    let output = run(&script);
    assert_eq!(stdout(&output), "x=out\nerr\n");
    assert_eq!(stderr(&output), "");
}
