//! End-to-end tests for `printf`, `test` and `[`: the built-in bodies need the
//! interpreter (`printf -v 'a[i]'` assigns an element, `test -v 'a[$key]'`
//! expands a subscript), so the interesting cases run through the real binary.

use std::process::{Command, Output, Stdio};

fn baash() -> Command {
    Command::new(env!("CARGO_BIN_EXE_baash"))
}

fn run(script: &str) -> Output {
    baash().arg("-c").arg(script).output().expect("spawn baash")
}

fn run_interactive(script: &str) -> Output {
    baash()
        .arg("-i")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
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

#[test]
fn printf_is_a_shell_builtin() {
    assert_eq!(out("type printf"), "printf is a shell builtin\n");
    assert_eq!(out("command printf '%s\\n' hi"), "hi\n");
}

#[test]
fn dash_v_assigns_scalars_and_elements() {
    assert_eq!(
        out("printf -v greeting '%s %s' hello there; printf '%s\\n' \"$greeting\""),
        "hello there\n"
    );
    assert_eq!(
        out("a=(a b c); printf -v 'a[1]' %s foo; printf '%s\\n' \"${a[*]}\""),
        "a foo c\n"
    );
    // A subscript may be an arithmetic expression.
    assert_eq!(
        out("a=(a b c); printf -v 'a[1+1]' %s z; printf '%s\\n' \"${a[2]}\""),
        "z\n"
    );
    // An invalid target is a usage error (status 2) and writes nothing.
    let output = run("printf -v 'a[' %s foo");
    assert_eq!(code(&output), 2);
    assert!(stdout(&output).is_empty());
}

#[test]
fn percent_q_round_trips_through_eval() {
    let script = r#"
value='"quoted" with spaces and \'
quoted=$(printf %q "$value")
eval "restored=$quoted"
test "$value" = "$restored" && echo roundtrip-ok
"#;
    assert_eq!(out(script), "roundtrip-ok\n");

    assert_eq!(out("printf '[%q]\\n' 'a b'"), "[a\\ b]\n");
    assert_eq!(out("printf '[%q]\\n' ''"), "['']\n");
}

#[test]
fn strftime_conversion_uses_the_exported_tz() {
    assert_eq!(
        out("export TZ=UTC; printf '%(%Y-%m-%d %H:%M:%S)T\\n' 0"),
        "1970-01-01 00:00:00\n"
    );
    assert_eq!(
        out("export TZ=UTC; printf '%(%s)T\\n' 1557978599"),
        "1557978599\n"
    );
}

#[test]
fn the_format_repeats_for_remaining_arguments() {
    assert_eq!(out("printf -- '-%s-' a b c"), "-a--b--c-");
    assert_eq!(out("printf x a b; echo"), "x\n");
    assert_eq!(out("printf '%s' a b c; echo"), "abc\n");
}

#[test]
fn test_v_expands_subscripts() {
    let script = r#"
a=(1 2 3 '')
test -v 'a[1]'; echo $?
test -v 'a[3]'; echo $?
test -v 'a[4]'; echo $?
test -v 'a[1+1]'; echo $?
test -v 'a[@]'; echo $?
"#;
    assert_eq!(out(script), "0\n0\n1\n0\n0\n");

    let script = r#"
declare -A h=([k]=v [empty]='')
test -v 'h[k]'; echo $?
test -v 'h[empty]'; echo $?
test -v 'h[nope]'; echo $?
key=k
test -v 'h[$key]'; echo $?
key=nope
test -v 'h[$key]'; echo $?
"#;
    assert_eq!(out(script), "0\n0\n1\n0\n1\n");
}

#[test]
fn test_o_reports_the_editor_mode() {
    assert_eq!(out("test -o emacs; echo $?"), "1\n");
    let output = run_interactive("test -o emacs; echo $?");
    assert_eq!(stdout(&output), "0\n");
}

#[test]
fn bracket_forms_and_grammar() {
    assert_eq!(out("[ x ] && echo yes"), "yes\n");
    assert_eq!(out("[ -z '' ] && echo yes"), "yes\n");
    assert_eq!(out("[ -n ] && echo yes"), "yes\n");
    assert_eq!(out("[ foo = bar ]; echo $?"), "1\n");
    assert_eq!(out("[ 1 -eq 1 -a 2 -eq 2 ]; echo $?"), "0\n");
    assert_eq!(out("[ '(' x ')' ]; echo $?"), "0\n");
    let output = run("[ x");
    assert_eq!(code(&output), 2);
    assert!(stderr(&output).contains(']'));
}

#[test]
fn the_test_builtin_keeps_bashs_argument_count_rules() {
    assert_eq!(out("test -n; echo $?"), "0\n");
    assert_eq!(out("test -n ''; echo $?"), "1\n");
    assert_eq!(out("test -a -a; echo $?"), "1\n");
    assert_eq!(out("test -a -a -a; echo $?"), "0\n");
    assert_eq!(out("test 1 -eq; echo $?"), "2\n");
    assert_eq!(out("test x -foo y; echo $?"), "2\n");
}
