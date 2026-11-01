//! Programmable completion end to end: `compgen`, `complete` and `compopt`
//! driven through the real built-in bodies over a real interpreter.
//!
//! The output is captured because that *is* the result of `compgen`; the
//! capture lives in the test's `ShellHost` wrapper rather than in a `> file`
//! redirection, because these tests run on several threads in one process and
//! `dup2`-ing the process's stdout would capture another test's harness output.
//! Candidate *sources* are the production ones — the interpreter's variable
//! store, function table, alias table and the filesystem — so a test here fails
//! when the shell's answer to "what could this name be?" changes.

use baash_lang::Vars;
use baash_runtime::complete::{CompletionSource, CompletionTable, WordList};
use baash_runtime::{ShellHost, Status};
use baash_vm::Interp;

/// An interpreter whose built-in output is captured instead of printed.
struct Capture {
    interp: Interp,
    out: Vec<u8>,
    err: Vec<u8>,
}

impl Capture {
    fn new() -> Self {
        let mut interp = Interp::new(false);
        interp.set_arg0("baash");
        Self {
            interp,
            out: Vec::new(),
            err: Vec::new(),
        }
    }

    /// Run shell code, for fixtures (variables, functions, options).
    fn run(&mut self, script: &str) {
        self.interp.run_named("<test>", script.as_bytes());
    }

    /// Run one built-in, returning its status with fresh capture buffers.
    fn builtin(&mut self, argv: &[&[u8]]) -> Status {
        self.out.clear();
        self.err.clear();
        self.interp.state().clear_error();
        let name = argv.first().copied().expect("argv[0]");
        baash_runtime::run_builtin(name, self, argv).expect("the built-in is registered")
    }

    fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.out).into_owned()
    }

    fn text(&self, name: &[u8]) -> String {
        String::from_utf8_lossy(Vars::get(&self.interp, name).unwrap_or_default()).into_owned()
    }
}

impl baash_runtime::ShellHost for Capture {
    fn state(&mut self) -> &mut baash_runtime::ShellState {
        self.interp.state()
    }

    fn run_source(&mut self, src: &[u8]) -> Status {
        self.interp.run_source(src)
    }

    fn write_out(&mut self, bytes: &[u8]) -> Status {
        self.out.extend_from_slice(bytes);
        Status::SUCCESS
    }

    fn write_err(&mut self, bytes: &[u8]) -> Status {
        self.err.extend_from_slice(bytes);
        Status::SUCCESS
    }

    fn diagnose(&mut self, diagnostic: &baash_diag::Diagnostic) {
        self.err.extend_from_slice(diagnostic.message.as_bytes());
        self.err.push(b'\n');
    }

    fn completion_source(&mut self) -> Option<&mut dyn CompletionSource> {
        self.interp.completion_source()
    }

    fn completion_words(&mut self, wordlist: &[u8]) -> WordList {
        self.interp.completion_words(wordlist)
    }

    fn check_completion_words(&mut self, wordlist: &[u8]) -> bool {
        self.interp.check_completion_words(wordlist)
    }

    fn is_function(&self, name: &[u8]) -> bool {
        self.interp.is_function(name)
    }

    fn call_function(&mut self, name: &[u8], argv: &[Vec<u8>]) -> Option<Status> {
        self.interp.call_function(name, argv)
    }
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("baash-completion-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

// ---------------------------------------------------------------- compgen -v

#[test]
fn compgen_lists_variables_and_functions() {
    let mut shell = Capture::new();
    shell.run("v_global=0\nv_other=1\nadd_one () { :; }\n__ec () { :; }\n");

    assert_eq!(shell.builtin(&[b"compgen", b"-v", b"v_"]), Status::SUCCESS);
    assert_eq!(shell.stdout(), "v_global\nv_other\n");

    assert_eq!(
        shell.builtin(&[b"compgen", b"-A", b"function", b"_"]),
        Status::SUCCESS
    );
    assert_eq!(shell.stdout(), "__ec\n");

    // No match is a failure with no output, which is what the suite pins for
    // `compgen -v __nonexistent__`.
    assert_eq!(
        shell.builtin(&[b"compgen", b"-v", b"__nonexistent__"]),
        Status::FAILURE
    );
    assert_eq!(shell.stdout(), "");
}

#[test]
fn compgen_exports_only_sees_exported_variables() {
    let mut shell = Capture::new();
    shell.run("export v_exported=1\nv_plain=2\n");
    assert_eq!(shell.builtin(&[b"compgen", b"-e", b"v_"]), Status::SUCCESS);
    assert_eq!(shell.stdout(), "v_exported\n");
    assert_eq!(
        shell.builtin(&[b"compgen", b"-e", b"v_plain"]),
        Status::FAILURE,
        "a known but unexported variable is not an export candidate"
    );
    assert_eq!(shell.stdout(), "");
}

// -------------------------------------------------------------- compgen files

#[test]
fn compgen_lists_files_and_directories() {
    let dir = temp_dir("files");
    std::fs::create_dir_all(dir.join("subdir")).expect("mkdir");
    std::fs::write(dir.join("one"), b"").expect("write");
    std::fs::write(dir.join("two"), b"").expect("write");

    let mut shell = Capture::new();

    let prefix = format!("{}/t", dir.display());
    assert_eq!(
        shell.builtin(&[b"compgen", b"-f", prefix.as_bytes()]),
        Status::SUCCESS
    );
    assert_eq!(shell.stdout(), format!("{}/two\n", dir.display()));

    let prefix = format!("{}/", dir.display());
    assert_eq!(
        shell.builtin(&[b"compgen", b"-d", prefix.as_bytes()]),
        Status::SUCCESS
    );
    assert_eq!(shell.stdout(), format!("{}/subdir\n", dir.display()));

    // A directory that does not exist offers nothing and fails, which the
    // suite pins for `compgen -f /non-existing-dir/`.
    assert_eq!(
        shell.builtin(&[b"compgen", b"-f", b"/non-existing-dir/"]),
        Status::FAILURE
    );
    assert_eq!(shell.stdout(), "");

    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------- compgen words

#[test]
fn compgen_splits_the_word_list_on_ifs_and_expands_it() {
    let mut shell = Capture::new();
    shell.run("IFS=':%'\n");
    assert_eq!(
        shell.builtin(&[b"compgen", b"-W", b"spam:eggs%ham cheese\\:colon"]),
        Status::SUCCESS
    );
    assert_eq!(shell.stdout(), "spam\neggs\nham cheese:colon\n");

    // A word list with no words at all is status 1, not a usage error.
    assert_eq!(shell.builtin(&[b"compgen", b"-W", b""]), Status::FAILURE);
    assert_eq!(shell.stdout(), "");

    // The word being completed filters the list; `-P`/`-S` decorate what is
    // left and `-X` removes matches before the decoration.
    let mut shell = Capture::new();
    assert_eq!(
        shell.builtin(&[b"compgen", b"-W", b"one two three", b"t"]),
        Status::SUCCESS
    );
    assert_eq!(shell.stdout(), "two\nthree\n");

    assert_eq!(
        shell.builtin(&[b"compgen", b"-W", b"one two three", b"-X", b"t*"]),
        Status::SUCCESS
    );
    assert_eq!(shell.stdout(), "one\n");

    assert_eq!(
        shell.builtin(&[b"compgen", b"-P", b"[", b"-S", b"]", b"-W", b"a b"]),
        Status::SUCCESS
    );
    assert_eq!(shell.stdout(), "[a]\n[b]\n");

    // `-A action` and `-W` combine, actions first, both filtered.
    assert_eq!(
        shell.builtin(&[b"compgen", b"-A", b"keyword", b"-W", b"wheel nope", b"wh"]),
        Status::SUCCESS
    );
    assert_eq!(shell.stdout(), "while\nwheel\n");
}

// -------------------------------------------------------- compgen -F and -C

#[test]
fn compgen_f_calls_the_function_with_the_completion_arguments() {
    let mut shell = Capture::new();
    // The function answers with the arguments and completion variables it was
    // given, so the calling convention is observable as the candidate list.
    shell.run(
        "probe() { COMPREPLY=( \"$1\" \"$2\" \"$3\" \"$COMP_CWORD\" \"$COMP_POINT\" \"$COMP_LINE\" ); }\n",
    );
    assert_eq!(
        shell.builtin(&[b"compgen", b"-F", b"probe", b"foo"]),
        Status::SUCCESS
    );
    // compgen, the word, the (absent) previous word, then COMP_CWORD=-1,
    // COMP_POINT=0 and an empty COMP_LINE: bash 5.3.20's values.
    assert_eq!(shell.stdout(), "compgen\nfoo\n\n-1\n0\n\n");
}

#[test]
fn compgen_f_accepts_a_scalar_compreply() {
    let mut shell = Capture::new();
    shell.run("_scalar() { unset -v COMPREPLY; COMPREPLY=hello; }\n");
    assert_eq!(
        shell.builtin(&[b"compgen", b"-F", b"_scalar"]),
        Status::SUCCESS
    );
    assert_eq!(shell.stdout(), "hello\n");
}

#[test]
fn compgen_f_reports_a_missing_function() {
    let mut shell = Capture::new();
    assert_eq!(
        shell.builtin(&[b"compgen", b"-F", b"missing_function"]),
        Status::FAILURE
    );
    assert_eq!(shell.stdout(), "");
}

#[test]
fn compgen_f_runs_the_function_in_this_shell() {
    let mut shell = Capture::new();
    shell.run("probe() { seen=yes; COMPREPLY=(ok); }\n");
    assert_eq!(
        shell.builtin(&[b"compgen", b"-F", b"probe", b"x"]),
        Status::SUCCESS
    );
    assert_eq!(shell.stdout(), "ok\n");
    // A subshell could not see or set this; `COMPREPLY` only works because the
    // function runs in the shell that is generating the completion.
    assert_eq!(shell.text(b"seen"), "yes");
}

// ------------------------------------------------------------------ complete

#[test]
fn complete_round_trips_through_complete_p() {
    let mut shell = Capture::new();
    // Nothing defined yet: `complete` prints nothing and succeeds.
    assert_eq!(shell.builtin(&[b"complete"]), Status::SUCCESS);
    assert_eq!(shell.stdout(), "");

    assert_eq!(
        shell.builtin(&[b"complete", b"-W", b"foo bar", b"mycommand"]),
        Status::SUCCESS
    );
    assert_eq!(shell.builtin(&[b"complete", b"-p"]), Status::SUCCESS);
    assert_eq!(shell.stdout(), "complete -W 'foo bar' mycommand\n");

    // `complete` with no arguments is an alias for `complete -p`.
    assert_eq!(shell.builtin(&[b"complete"]), Status::SUCCESS);
    assert_eq!(shell.stdout(), "complete -W 'foo bar' mycommand\n");

    // `-p name` prints that spec; an unknown name prints nothing and succeeds.
    assert_eq!(
        shell.builtin(&[b"complete", b"-p", b"mycommand"]),
        Status::SUCCESS
    );
    assert_eq!(shell.stdout(), "complete -W 'foo bar' mycommand\n");
    assert_eq!(
        shell.builtin(&[b"complete", b"-p", b"nonexistent"]),
        Status::SUCCESS
    );
    assert_eq!(shell.stdout(), "");

    // A second definition replaces the first, in place.
    assert_eq!(
        shell.builtin(&[b"complete", b"-A", b"file", b"mycommand"]),
        Status::SUCCESS
    );
    assert_eq!(shell.builtin(&[b"complete", b"-p"]), Status::SUCCESS);
    assert_eq!(shell.stdout(), "complete -f mycommand\n");

    // `-r` removes it again.
    assert_eq!(
        shell.builtin(&[b"complete", b"-r", b"mycommand"]),
        Status::SUCCESS
    );
    assert_eq!(shell.builtin(&[b"complete", b"-p"]), Status::SUCCESS);
    assert_eq!(shell.stdout(), "");
}

#[test]
fn complete_keeps_default_and_empty_specs_apart() {
    let mut shell = Capture::new();
    shell.run("helper () { :; }\n");
    assert_eq!(
        shell.builtin(&[b"complete", b"-D", b"-F", b"helper"]),
        Status::SUCCESS
    );
    assert_eq!(
        shell.builtin(&[b"complete", b"-E", b"-W", b"echo"]),
        Status::SUCCESS
    );
    // A default spec is not a named entry, so `-p` does not print it.
    assert_eq!(shell.builtin(&[b"complete", b"-p"]), Status::SUCCESS);
    assert_eq!(shell.stdout(), "");

    // A `-D` spec names no command, so its function has to exist now.
    assert_eq!(
        shell.builtin(&[b"complete", b"-D", b"-F", b"invalidZZ"]),
        Status(2)
    );
}

#[test]
fn complete_rejects_a_word_list_that_does_not_parse() {
    let mut shell = Capture::new();
    assert_eq!(
        shell.builtin(&[b"complete", b"-W", b"${", b"cmd"]),
        Status(2)
    );
}

// ------------------------------------------------------------------- compopt

#[test]
fn compopt_fails_outside_a_completion_function() {
    let mut shell = Capture::new();
    assert_eq!(
        shell.builtin(&[b"compopt", b"-o", b"filenames", b"+o", b"nospace"]),
        Status::FAILURE
    );
}

#[test]
fn compopt_rejects_an_unknown_option_name_before_anything_else() {
    let mut shell = Capture::new();
    assert_eq!(shell.builtin(&[b"compopt", b"-o", b"invalid"]), Status(2));
}

#[test]
fn compopt_edits_the_completion_in_progress() {
    let mut shell = Capture::new();
    shell.run(
        "probe() { if compopt -o nospace +o filenames; then COMPREPLY=(accepted); else COMPREPLY=(rejected); fi; }\n",
    );
    assert_eq!(
        shell.builtin(&[b"compgen", b"-F", b"probe", b"x"]),
        Status::SUCCESS
    );
    assert_eq!(shell.stdout(), "accepted\n");
    // The transient state is gone once the completion finished.
    assert!(!shell.interp.state().completion.is_active());
}

// ------------------------------------------------------------- usage statuses

#[test]
fn usage_errors_are_status_two() {
    let mut shell = Capture::new();
    // `-F` with no command name.
    assert_eq!(shell.builtin(&[b"complete", b"-F", b"f"]), Status(2));
    // A name with no action.
    assert_eq!(shell.builtin(&[b"complete", b"foo"]), Status(2));
    // An unknown action, for both builtins.
    assert_eq!(
        shell.builtin(&[b"complete", b"-A", b"nope", b"cmd"]),
        Status(2)
    );
    assert_eq!(shell.builtin(&[b"compgen", b"-A", b"nope"]), Status(2));
    // An unknown option.
    assert_eq!(shell.builtin(&[b"compgen", b"-Q"]), Status(2));
    // An unknown `-o` name.
    assert_eq!(
        shell.builtin(&[b"complete", b"-o", b"invalid", b"cmd"]),
        Status(2)
    );
}

// ------------------------------------------------------------ state plumbing

#[test]
fn the_shell_state_owns_the_completion_table() {
    // `complete` writes through `ShellState`, so a caller that only has the
    // state sees the same definitions `complete -p` prints. This is the seam a
    // replacing completion engine reads.
    let mut shell = Capture::new();
    assert_eq!(
        shell.builtin(&[b"complete", b"-W", b"x", b"cmd"]),
        Status::SUCCESS
    );
    let table: &CompletionTable = &shell.interp.state().completions;
    let spec = table.get(b"cmd").expect("a spec for cmd");
    assert_eq!(spec.wordlist.as_deref(), Some(&b"x"[..]));
    assert_eq!(table.len(), 1);
    // A different name falls back to no spec when no default is defined.
    assert!(table.get(b"other").is_none());
    assert!(shell.interp.state().completion.options().is_empty());
}
