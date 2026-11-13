//! Completion compared with bash, case by case, without a terminal.
//!
//! Every case is a shell script that fills `COMPREPLY` — by calling
//! bash-completion's helpers or by calling `compgen` directly — and prints the
//! result one candidate per line. The *same* script runs under bash and under
//! baash, and the test compares the candidates and the exit status. That is
//! the contract a completion script sees, so it is the contract the shell has
//! to match; what the editor then does with the candidates (insertion,
//! quoting, the menu) is a different layer, covered by
//! `interactive_complete.rs`.
//!
//! Calling a completion function directly, outside a completion, is how
//! bash-completion's own helpers are written: they read `COMP_WORDS`,
//! `COMP_CWORD`, `COMP_LINE` and `COMP_POINT`, and write `COMPREPLY`. bash
//! fills in those variables before a Tab; the cases fill them in by hand, so
//! no pty is needed and hundreds of cases stay cheap.
//!
//! Two limits, both deliberate:
//!
//! * The library cases use the *same file* for both shells — the vendored
//!   `vendor/bash-completion/bash_completion` tree when it exists (the flake
//!   copies it in, the dev shell does, and it is what the binary embeds), else
//!   the system copy. A checkout with neither reports a skip. Comparing two
//!   different library versions would compare the libraries, not the shells.
//! * bash is required. Windows and a bare CI image have none, and the test
//!   reports the skip rather than pretending to have compared anything.
//!
//! Not covered here, and measured elsewhere: `complete -C` (a command's
//! output as candidates) is not run by the shell at all, and `complete -p
//! -D` prints the whole table instead of the default spec; both are in
//! `docs/state.md`'s known completion limitations.

#![forbid(unsafe_code)]
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// One differential case: a name for the report and the script both shells run.
struct Case {
    /// What the case proves, in the report.
    name: &'static str,
    /// The script. It runs with `cwd` on a fresh directory and `TMP` pointing
    /// at it, and prints its candidates.
    script: &'static str,
}

/// `compgen` on its own — the primitive every completion function is built on.
///
/// These need no library: `compgen` is a built-in in both shells.
const COMPGEN: &[Case] = &[
    Case {
        name: "compgen -f filters by the typed prefix",
        script: r#"
touch alpha.txt beta.log
printf '[%s]\n' $(compgen -f -- al)
"#,
    },
    Case {
        name: "compgen -f offers every entry for an empty prefix",
        script: r#"
touch alpha.txt beta.log
mkdir sub
printf '[%s]\n' $(compgen -f -- "")
"#,
    },
    Case {
        name: "compgen -d offers directories only",
        script: r#"
touch alpha.txt
mkdir sub .hiddendir
printf '[%s]\n' $(compgen -d -- "")
"#,
    },
    Case {
        name: "compgen -X removes matches",
        script: r#"
touch alpha.txt beta.log gamma.txt
printf '[%s]\n' $(compgen -f -X '*.log' -- "")
"#,
    },
    Case {
        name: "compgen -X with a negated pattern keeps only matches",
        script: r#"
touch alpha.txt beta.log gamma.txt
printf '[%s]\n' $(compgen -f -X '!*.txt' -- "")
"#,
    },
    Case {
        name: "compgen -P and -S decorate every candidate",
        script: r#"
touch alpha.txt
printf '[%s]\n' $(compgen -f -P 'pre-' -S '.x' -- al)
"#,
    },
    Case {
        name: "compgen -W filters a word list",
        script: r#"
printf '[%s]\n' $(compgen -W 'one two three' -- t)
"#,
    },
    Case {
        name: "compgen -W expands its word list",
        script: r#"
list='one two three'
printf '[%s]\n' $(compgen -W '$list' -- t)
"#,
    },
    Case {
        name: "compgen -A function lists shell functions",
        script: r#"
_zz_alpha() { :; }
_zz_beta() { :; }
printf '[%s]\n' $(compgen -A function -- _zz_)
"#,
    },
    Case {
        name: "compgen -A variable lists shell variables",
        script: r#"
ZZ_ONE=1
ZZ_TWO=2
printf '[%s]\n' $(compgen -A variable -- ZZ_)
"#,
    },
    Case {
        name: "compgen -A keyword lists reserved words",
        script: r#"
printf '[%s]\n' $(compgen -A keyword -- wh)
"#,
    },
    Case {
        name: "compgen -f matches hidden entries only when asked",
        script: r#"
touch .hidden visible
printf '[%s]\n' $(compgen -f -- .h)
printf '[%s]\n' $(compgen -f -- vis)
"#,
    },
    Case {
        name: "compgen -f keeps a relative path in the candidate",
        script: r#"
mkdir sub
touch sub/inner
printf '[%s]\n' $(compgen -f -- ./su)
"#,
    },
    Case {
        name: "compgen -G expands a glob",
        script: r#"
touch a.txt b.md c.txt
printf '[%s]\n' $(compgen -G '*.txt')
"#,
    },
    Case {
        name: "compgen -f expands a tilde prefix and keeps the word",
        script: r#"
mkdir -p "$TMP/home/dir" "$TMP/home/docs"
touch "$TMP/home/file.txt"
HOME="$TMP/home"
printf '[%s]\n' $(compgen -f -- '~/')
printf '[%s]\n' $(compgen -f -- '~/d')
"#,
    },
    Case {
        name: "compgen -f expands a parameter in the directory part",
        script: r#"
mkdir -p "$TMP/home/docs"
HOME="$TMP/home"
printf '[%s]\n' $(compgen -f -- '$HOME/do')
printf '[%s]\n' $(compgen -f -- '${HOME}/do')
"#,
    },
    Case {
        name: "compgen -f honours quoting for the lookup, not the answer",
        script: r#"
mkdir -p "$TMP/home/docs"
HOME="$TMP/home"
printf '[%s]\n' $(compgen -f -- '"$HOME"/do')
"#,
    },
    Case {
        name: "compgen -f leaves a command substitution alone",
        script: r#"
mkdir -p "$TMP/home/docs"
HOME="$TMP/home"
printf '[%s]\n' $(compgen -f -- '$(echo x)/')
printf '[%s]\n' $(compgen -f -- '`echo x`/')
"#,
    },
];

/// bash-completion's own helpers, driven the way a completion script drives
/// them. Each script sources the library from `$LIB`.
const LIBRARY: &[Case] = &[
    Case {
        name: "_filedir completes the typed prefix",
        script: r#"
. "$LIB"
touch alpha.txt 'with space.txt'
mkdir sub
COMP_WORDS=(cmd al); COMP_CWORD=1; COMP_LINE='cmd al'; COMP_POINT=${#COMP_LINE}
cur=al
_filedir
printf '[%s]\n' "${COMPREPLY[@]}"
"#,
    },
    Case {
        name: "_filedir -d completes directories",
        script: r#"
. "$LIB"
touch alpha.txt
mkdir sub other
COMP_WORDS=(cmd ""); COMP_CWORD=1; COMP_LINE='cmd '; COMP_POINT=${#COMP_LINE}
cur=
_filedir -d
printf '[%s]\n' "${COMPREPLY[@]}"
"#,
    },
    Case {
        name: "_filedir filters by an extension pattern",
        script: r#"
. "$LIB"
touch alpha.txt beta.log gamma.md
COMP_WORDS=(cmd ""); COMP_CWORD=1; COMP_LINE='cmd '; COMP_POINT=${#COMP_LINE}
cur=
_filedir '@(txt|md)'
printf '[%s]\n' "${COMPREPLY[@]}"
"#,
    },
    Case {
        name: "_comp_initialize fills cur and prev from COMP_WORDS",
        script: r#"
. "$LIB"
touch alpha.txt beta.log
comp() { _comp_initialize || return; COMPREPLY=($(compgen -f -- "$cur")); }
COMP_WORDS=(cmd al); COMP_CWORD=1; COMP_LINE='cmd al'; COMP_POINT=${#COMP_LINE}
comp cmd al al
printf '[%s]\n' "${COMPREPLY[@]}"
"#,
    },
    Case {
        name: "_comp_initialize leaves an option whole and -s splits it",
        script: r#"
. "$LIB"
whole() { _comp_initialize || return; printf 'cur=%s\n' "$cur"; }
split() { _comp_initialize -s || return; printf 'cur=%s\n' "$cur"; }
COMP_WORDS=(cmd --opt=va); COMP_CWORD=1; COMP_LINE='cmd --opt=va'; COMP_POINT=${#COMP_LINE}
whole cmd --opt=va va
split cmd --opt=va va
"#,
    },
    Case {
        name: "_comp_compgen -a filedir fills COMPREPLY",
        script: r#"
. "$LIB"
touch alpha.txt beta.log
comp() { _comp_initialize || return; _comp_compgen -a filedir; }
COMP_WORDS=(cmd al); COMP_CWORD=1; COMP_LINE='cmd al'; COMP_POINT=${#COMP_LINE}
comp cmd al al
printf '[%s]\n' "${COMPREPLY[@]}"
"#,
    },
    Case {
        name: "_comp_compgen -a variables completes parameter names",
        script: r#"
. "$LIB"
ZZ_ONE=1
ZZ_TWO=2
comp() { _comp_initialize || return; _comp_compgen -a variables; }
COMP_WORDS=(cmd '$ZZ_'); COMP_CWORD=1; COMP_LINE='cmd $ZZ_'; COMP_POINT=${#COMP_LINE}
comp cmd '$ZZ_' '$ZZ_'
printf '[%s]\n' "${COMPREPLY[@]}"
"#,
    },
    Case {
        name: "_comp_compgen -a directory completes directories",
        script: r#"
. "$LIB"
touch afile
mkdir -p adir bdir
comp() { _comp_initialize || return; _comp_compgen -a directory; }
COMP_WORDS=(cmd ad); COMP_CWORD=1; COMP_LINE='cmd ad'; COMP_POINT=${#COMP_LINE}
comp cmd ad ad
printf '[%s]\n' "${COMPREPLY[@]}"
"#,
    },
    Case {
        name: "_comp_compgen -c completes a word other than cur",
        script: r#"
. "$LIB"
mkdir -p sub
touch sub/afile
comp() { _comp_initialize || return; _comp_compgen -c 'sub/a' -a filedir; }
COMP_WORDS=(cmd x); COMP_CWORD=1; COMP_LINE='cmd x'; COMP_POINT=${#COMP_LINE}
comp cmd x x
printf '[%s]\n' "${COMPREPLY[@]}"
"#,
    },
    Case {
        name: "_comp_compgen -v writes a variable instead of COMPREPLY",
        script: r#"
. "$LIB"
mkdir -p sub
touch sub/afile
out=()
comp() { _comp_initialize || return; _comp_compgen -a filedir -v out; }
COMP_WORDS=(cmd sub/); COMP_CWORD=1; COMP_LINE='cmd sub/'; COMP_POINT=${#COMP_LINE}
comp cmd sub/ sub/
printf 'out=[%s]\n' "${out[@]}"
printf 'reply=[%s]\n' "${COMPREPLY[@]}"
"#,
    },
    Case {
        name: "_comp_compgen -W takes a quoted word list",
        script: r#"
. "$LIB"
comp() { _comp_initialize || return; _comp_compgen -- -W '"alpha beta" gamma'; }
COMP_WORDS=(cmd a); COMP_CWORD=1; COMP_LINE='cmd a'; COMP_POINT=${#COMP_LINE}
comp cmd a a
printf '[%s]\n' "${COMPREPLY[@]}"
"#,
    },
    Case {
        name: "_comp_get_words splits a path list on colon",
        script: r#"
. "$LIB"
COMP_WORDS=(cmd 'a:b'); COMP_CWORD=1; COMP_LINE='cmd a:b'; COMP_POINT=${#COMP_LINE}
_comp_get_words -n "<>&" cur prev words cword
printf 'cur=%s prev=%s\n' "$cur" "$prev"
printf '[%s]\n' "${words[@]}"
"#,
    },
    Case {
        name: "_comp_dequote strips shell quoting",
        script: r#"
. "$LIB"
for w in 'plain' "'sq'" '"dq"' 'a\ b'; do
    printf '%s -> ' "$w"
    _comp_dequote "$w"
done
"#,
    },
    Case {
        name: "_comp_split splits on a separator",
        script: r#"
. "$LIB"
_comp_split -F '%' 'a%b%c'
printf '[%s]\n' "${REPLY[@]:-}"
"#,
    },
    Case {
        name: "_comp_variable_assignments completes parameter names",
        script: r#"
. "$LIB"
ZZ_ONE=1
ZZ_TWO=2
COMP_WORDS=(cmd ZZ_); COMP_CWORD=1; COMP_LINE='cmd ZZ_'; COMP_POINT=${#COMP_LINE}
cur=ZZ_
_comp_variable_assignments
printf '[%s]\n' "${COMPREPLY[@]}"
"#,
    },
    Case {
        name: "_comp_count_args and _comp_get_first_arg read the word list",
        script: r#"
. "$LIB"
words=(cmd -a --long=x arg1 arg2)
cword=4
_comp_count_args
echo "count=$REPLY"
_comp_get_first_arg
echo "first=$REPLY"
"#,
    },
    Case {
        name: "_comp_have_command finds commands on PATH",
        script: r#"
. "$LIB"
for c in sh bash definitely_not_a_command; do
    _comp_have_command "$c" && echo "$c=yes" || echo "$c=no"
done
"#,
    },
    Case {
        name: "_comp_looks_like_path tells words from paths",
        script: r#"
. "$LIB"
for w in '' '.' '..' '~' '~x' 'a' '-' '--opt' '--opt=x' 'a:' ':a' 'a/b'; do
    _comp_looks_like_path "$w" && r=yes || r=no
    printf '%s=%s\n' "${w:-EMPTY}" "$r"
done
"#,
    },
    Case {
        name: "complete -p prints options in bash's order, once each",
        script: r#"
. "$LIB"
_f() { :; }
complete -o nospace -o filenames -o bashdefault -o nospace -F _f zzcmd
complete -p zzcmd
"#,
    },
    Case {
        name: "complete -p reports a name with no spec and exits 1",
        script: r#"
. "$LIB"
_f() { :; }
complete -F _f known
complete -p known missing
echo "status=$?"
"#,
    },
    Case {
        name: "compopt outside a completion is an error, not a usage error",
        script: r#"
. "$LIB"
compopt -o nospace zzcmd
echo "status=$?"
"#,
    },
];

#[test]
fn compgen_agrees_with_bash() {
    compare(COMPGEN, false);
}

#[test]
fn the_bash_completion_library_agrees_with_bash() {
    compare(LIBRARY, true);
}

/// Run every case under both shells and report all the differences at once.
fn compare(cases: &[Case], needs_library: bool) {
    let Some(bash) = find_bash() else {
        eprintln!("skipped: no bash to compare against");
        return;
    };
    let library = if needs_library {
        match find_library() {
            Some(path) => Some(path),
            None => {
                eprintln!(
                    "skipped: no bash-completion library (set BAASH_TEST_BASH_COMPLETION, \
                     or run in `nix develop` / after `nix build .#bash-completion`)"
                );
                return;
            }
        }
    } else {
        None
    };
    let baash = PathBuf::from(env!("CARGO_BIN_EXE_baash"));
    let mut failures = Vec::new();
    for (index, case) in cases.iter().enumerate() {
        let root =
            std::env::temp_dir().join(format!("baash-compat-{}-{index}", std::process::id()));
        let work = root.join("work");
        let home = root.join("home");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&work).expect("create the case directory");
        std::fs::create_dir_all(&home).expect("create the home directory");
        let script = root.join("case.sh");
        std::fs::write(&script, case.script).expect("write the case script");

        // The same directory for both shells: a case may print an absolute
        // path (a temporary directory name), and both must print the same one.
        // Every fixture is idempotent (`touch`, `mkdir -p`), so the second run
        // sees what the first did and nothing changes.
        let reference = run(&bash, &script, &work, &home, library.as_deref());
        let tested = run(&baash, &script, &work, &home, library.as_deref());
        let _ = std::fs::remove_dir_all(&root);

        let expected = lines(&reference);
        let actual = lines(&tested);
        if expected.is_empty() && actual.is_empty() {
            failures.push(format!(
                "{}: the case produced no candidates under bash either — it proves nothing\n  {}",
                case.name,
                case.script.trim().replace('\n', "\n  ")
            ));
            continue;
        }
        if expected != actual || reference.status.code() != tested.status.code() {
            failures.push(format!(
                "{}\n  bash  ({}): {:?}\n  baash ({}): {:?}\n  baash stderr: {}",
                case.name,
                status(&reference),
                expected,
                status(&tested),
                actual,
                String::from_utf8_lossy(&tested.stderr).trim()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} completion cases differ from bash:\n\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n\n")
    );
}

/// The shell under test, with the environment the case wrote for itself.
fn run(shell: &Path, script: &Path, work: &Path, home: &Path, library: Option<&Path>) -> Output {
    let mut command = Command::new(shell);
    command
        .arg("--norc")
        .arg(script)
        .current_dir(work)
        .env_clear()
        // `PATH` is kept (the cases ask about `sh` and `bash`), everything else
        // is the test's: a developer's `BASH_ENV`, `INPUTRC` or `SHELL` must
        // not decide what a case sees.
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("TMP", work)
        .env("LC_ALL", "C")
        .env("TERM", "dumb");
    if let Some(library) = library {
        command.env("LIB", library);
    }
    command.output().expect("spawn the shell")
}

/// The candidate lines, order-insensitive.
///
/// bash's `compgen` answers in directory order while baash sorts, and directory
/// order is a filesystem property — comparing it would measure the temporary
/// directory, not the shells. The editor sorts the menu anyway, so the set is
/// what the contract is about.
fn lines(output: &Output) -> Vec<String> {
    let mut lines: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    lines.sort_unstable();
    lines
}

fn status(output: &Output) -> String {
    match output.status.code() {
        Some(code) => format!("exit {code}"),
        None => "signal".to_owned(),
    }
}

/// `bash` from `$BASH` or from `PATH`.
fn find_bash() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("BASH") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join("bash"))
        .find(|candidate| candidate.is_file())
}

/// The bash-completion file both shells source.
///
/// The vendored copy wins because it is the one the binary embeds and the
/// flake and the dev shell both install; the system copy is the fallback for a
/// checkout that has neither.
fn find_library() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("BAASH_TEST_BASH_COMPLETION") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    [
        workspace.join("vendor/bash-completion/bash_completion"),
        PathBuf::from("/usr/share/bash-completion/bash_completion"),
        PathBuf::from("/etc/bash_completion"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}
