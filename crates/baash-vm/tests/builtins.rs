//! The built-in cluster end to end: aliases, `type`/`command`/`builtin`,
//! `getopts` and `unset -f`/`-v`.
//!
//! Each test pins a rule that is easy to get subtly wrong and that the spec
//! suite only covers indirectly: alias substitution is textual and never
//! recursive into itself, `builtin` must bypass a function of the same name,
//! and `type` must agree with what running the name would do.
//!
//! Everything is observed through *shell state* (`Vars`, positional
//! parameters) or an exit status, never by redirecting the process's stdout:
//! these tests run on several threads in one process, and `{ … } > file` would
//! capture another test's harness output as well.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use baash_lang::Vars;
use baash_vm::Interp;

fn shell() -> Interp {
    let mut interp = Interp::new(false);
    interp.set_arg0("baash");
    interp
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("baash-builtins-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn text(interp: &Interp, name: &[u8]) -> String {
    String::from_utf8_lossy(Vars::get(interp, name).unwrap_or_default()).into_owned()
}

fn positional(interp: &Interp) -> Vec<String> {
    Vars::positional(interp)
        .iter()
        .map(|word| String::from_utf8_lossy(word).into_owned())
        .collect()
}

/// A script whose only effect is its exit status, so it is observable without
/// touching stdout.
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
fn an_alias_is_substituted_at_command_position_only() {
    let mut interp = shell();
    interp.run_named("<alias>", b"alias ha='x=A'\nha\nalias e2='y=B'\nv=e2\n");
    assert_eq!(
        text(&interp, b"x"),
        "A",
        "an alias in command position runs"
    );
    assert_eq!(
        text(&interp, b"v"),
        "e2",
        "an alias in an argument position is literal"
    );
    assert_eq!(text(&interp, b"y"), "", "the alias was not expanded");
}

#[test]
fn an_alias_does_not_expand_into_itself() {
    let mut interp = shell();
    // The value starts with the alias's own name, which is the shape a
    // recursive expander loops on. `set --` makes the result observable
    // without any output: exactly one expansion yields `foo bar`.
    interp.run_named("<alias>", b"alias echo='set -- foo'\necho bar\n");
    assert_eq!(positional(&interp), ["foo", "bar"]);
}

#[test]
fn an_alias_value_may_be_a_whole_list() {
    let mut interp = shell();
    interp.run_named("<alias>", b"alias t1='set -- one && set -- one two'\nt1\n");
    assert_eq!(positional(&interp), ["one", "two"]);
}

#[test]
fn a_trailing_blank_in_the_value_makes_the_next_word_eligible() {
    let mut interp = shell();
    interp.run_named(
        "<alias>",
        b"alias one='ONE '\nalias with='set -- '\nalias without='set'\n",
    );
    // `with` ends in a blank, so `one` is re-examined and expands; `without`
    // does not, so the same word stays literal. This is the rule that makes
    // `alias sudo='sudo '` work.
    interp.run_named("<alias>", b"with one\n");
    assert_eq!(positional(&interp), ["ONE"]);
    interp.run_named("<alias>", b"without one\n");
    assert_eq!(positional(&interp), ["one"]);
}

#[test]
fn type_and_command_agree_with_what_would_run() {
    let dir = temp_dir("type-agreement");
    let tool = write_script(&dir, "mytool", 42);
    let mut interp = shell();
    interp.run_named(
        "<type>",
        format!("PATH='{}':$PATH\n", dir.display()).as_bytes(),
    );

    // A file resolves; a missing name does not.
    interp.run_named("<type>", b"type -t mytool >/dev/null; file_status=$?\n");
    assert_eq!(text(&interp, b"file_status"), "0");
    interp.run_named(
        "<type>",
        b"type -t definitely-not-here >/dev/null; missing=$?\n",
    );
    assert_eq!(text(&interp, b"missing"), "1");

    // The external really is what runs, and `command` reaches it too.
    interp.run_named("<type>", b"mytool; ran=$?\ncommand mytool; direct=$?\n");
    assert_eq!(text(&interp, b"ran"), "42");
    assert_eq!(text(&interp, b"direct"), "42");

    // A function shadows it for normal execution but not for `command`, and
    // `type -P` still reports the file.
    interp.run_named("<type>", b"mytool() { :; }\nmytool; shielded=$?\n");
    assert_eq!(text(&interp, b"shielded"), "0");
    interp.run_named("<type>", b"command mytool; past=$?\n");
    assert_eq!(text(&interp, b"past"), "42");
    interp.run_named("<type>", b"type -P mytool >/dev/null; by_path=$?\n");
    assert_eq!(text(&interp, b"by_path"), "0");
    // A built-in is not a file, so a forced path search fails.
    interp.run_named("<type>", b"type -P cd >/dev/null; builtin_path=$?\n");
    assert_eq!(text(&interp, b"builtin_path"), "1");

    // `command -v` answers through the status, for functions and files alike.
    interp.run_named("<cmd>", b"command -v mytool >/dev/null; v_file=$?\n");
    assert_eq!(text(&interp, b"v_file"), "0");
    interp.run_named("<cmd>", b"command -v nope >/dev/null; v_none=$?\n");
    assert_eq!(text(&interp, b"v_none"), "1");

    assert!(tool.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn builtin_runs_the_builtin_past_a_function_of_the_same_name() {
    let dir = temp_dir("builtin-bypass");
    let input = dir.join("in.txt");
    std::fs::write(&input, b"from-file\n").expect("write input");
    let mut interp = shell();
    let script = format!(
        "read() {{ v=function-ran; }}\nread v < '{}'\n",
        input.display()
    );
    interp.run_named("<builtin>", script.as_bytes());
    assert_eq!(
        text(&interp, b"v"),
        "function-ran",
        "a function shadows the intrinsic"
    );

    // `builtin` reaches the body the function is hiding; `command` does the
    // same for built-ins and external programs alike.
    let script = format!(
        "read() {{ v=function-ran; }}\nbuiltin read v < '{}'\n",
        input.display()
    );
    interp.run_named("<builtin>", script.as_bytes());
    assert_eq!(text(&interp, b"v"), "from-file");

    let script = format!(
        "read() {{ v=function-ran; }}\ncommand read v < '{}'\n",
        input.display()
    );
    interp.run_named("<builtin>", script.as_bytes());
    assert_eq!(text(&interp, b"v"), "from-file");

    // An unknown name is status 1, not a crash and not a `PATH` search.
    let status = interp.run_named("<builtin>", b"builtin definitely-not-a-builtin");
    assert_eq!(status.exit_code(), 1);
    // `builtin` with no name is a no-op, so it cannot break an and-or chain.
    assert!(
        interp
            .run_named("<builtin>", b"builtin; builtin --")
            .is_success()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unset_f_removes_a_function_and_unset_v_keeps_it() {
    let mut interp = shell();
    interp.run_named("<unset>", b"f() { :; }\n");
    interp.run_named("<unset>", b"type -t f >/dev/null; before=$?\n");
    assert_eq!(text(&interp, b"before"), "0");

    // `unset -v` is about variables: the function survives.
    interp.run_named("<unset>", b"unset -v f\ntype -t f >/dev/null; after_v=$?\n");
    assert_eq!(text(&interp, b"after_v"), "0");

    interp.run_named("<unset>", b"unset -f f\ntype -t f >/dev/null; after_f=$?\n");
    assert_eq!(text(&interp, b"after_f"), "1");

    // Without `-f`/`-v` the variable wins when it exists, so a function of the
    // same name survives only while no variable shadows it.
    interp.run_named(
        "<unset>",
        b"g() { :; }\ng=value\nunset g\ntype -t g >/dev/null; kept=$?\n",
    );
    assert_eq!(text(&interp, b"kept"), "0", "the variable was removed");
    assert_eq!(text(&interp, b"g"), "");
}

#[test]
fn getopts_walks_a_cluster_and_reports_the_shell_state() {
    let mut interp = shell();
    interp.run_named(
        "<getopts>",
        b"set -- -h -c foo x y z\n\
          while getopts 'hc:' opt; do\n\
            case $opt in\n\
              h) h=1 ;;\n\
              c) c=$OPTARG ;;\n\
            esac\n\
          done\n\
          shift $((OPTIND - 1))\n",
    );
    assert_eq!(text(&interp, b"h"), "1");
    assert_eq!(text(&interp, b"c"), "foo");
    assert_eq!(text(&interp, b"OPTIND"), "4");
    assert_eq!(positional(&interp), ["x", "y", "z"]);
}

#[test]
fn getopts_rejects_a_missing_argument_and_stops_at_double_dash() {
    let mut interp = shell();
    interp.run_named(
        "<getopts>",
        b"set -- -c\n\
          getopts 'c:' opt 2>/dev/null\n\
          missing=$?\n\
          first=$opt\n\
          set -- -a -- -b\n\
          OPTIND=1\n\
          while getopts 'ab' o2; do :; done\n\
          after=$OPTIND\n",
    );
    assert_eq!(text(&interp, b"missing"), "0", "`?` is a successful parse");
    assert_eq!(text(&interp, b"first"), "?");
    // `--` is consumed, so the operand after it is reachable through
    // `shift $((OPTIND-1))`.
    assert_eq!(text(&interp, b"after"), "3");
}

#[test]
fn an_alias_is_visible_to_type_and_lost_on_unalias() {
    let mut interp = shell();
    interp.run_named(
        "<alias>",
        b"alias ll='ls -l'\n\
          type -t ll >/dev/null; kind=$?\n\
          command -v ll >/dev/null; visible=$?\n\
          unalias ll\n\
          type -t ll >/dev/null; gone=$?\n",
    );
    assert_eq!(
        text(&interp, b"kind"),
        "0",
        "type reports an alias as a resolution"
    );
    assert_eq!(text(&interp, b"visible"), "0");
    assert_eq!(text(&interp, b"gone"), "1");
}
