//! Which file a command name names, and how to start it.
//!
//! A command word is a **name**, not a path: `code` is looked up in `PATH`, and
//! only the platform can say which file that name means. On POSIX it is the
//! file the name spells, provided the execute bit is set. Windows has no
//! execute bit — "an extension in `PATHEXT` *is* the execute permission"
//! (`docs/portability.md` §5) — so a name does not name a program until a
//! suffix from `PATHEXT` names one.
//!
//! # Why the bare name is not tried first
//!
//! VS Code puts two files in one `PATH` directory, `…\Microsoft VS Code\bin`:
//! `code`, a 2 KB POSIX shell script that starts with `#!/usr/bin/env sh`, and
//! `code.cmd`, the batch file that runs the real `Code.exe`. Both are ordinary
//! files, so a search that asks "does `dir\code` exist?" finds the shell script
//! and stops; `CreateProcess` then refuses it with `ERROR_BAD_EXE_FORMAT` (193)
//! and `code --version` fails on a machine where `code` plainly works from
//! `cmd`. Measured on the `winvm` guest (Windows 11 26300) with the real
//! VS Code 1.146 install: `baash -c 'command -v code'` printed the
//! extensionless script and `baash -c 'code --version'` failed with os error
//! 193, while `cmd /c code --version` printed the version.
//!
//! `cmd.exe` never runs that file. It appends each suffix from `PATHEXT` to the
//! name and looks for *those*, in `PATHEXT`'s order, so it finds `code.cmd`.
//! The same measurement: in a directory holding a bare `dd` (a text file) and
//! `dd.cmd`, `cmd /c dd` ran `dd.cmd`; a copy of `whoami.exe` named `barefile`
//! was *not* run as `barefile`, though `where barefile` listed it — `where`
//! reports files, `cmd` executes names.
//!
//! So the candidates for a name, in order, are:
//!
//! | name | candidates |
//! |---|---|
//! | `code` | `code.COM`, `code.EXE`, `code.BAT`, `code.CMD`, … (`PATHEXT`'s order) |
//! | `foo.exe` | `foo.exe` — a name that already carries a known suffix *is* the file |
//! | `./tool`, spelled as a path | the suffixes above, then `./tool` itself |
//! | any name on POSIX | the name itself; `PATHEXT` does not exist there |
//!
//! The literal file is last, not absent, when the caller says the name was
//! spelled as a path: a path names a file. `CreateProcess` loads a PE image
//! whatever it is called (measured: `cmd.exe` refuses an extensionless PE that
//! `CreateProcess` loads), and reporting the file's own failure is better than
//! a false "command not found" for a file the user can see. A *searched* name
//! gets no literal candidate at all, because `PATH` holds names, and `cmd`
//! does not execute an extensionless file found there either.
//!
//! # Starting a `.cmd` or `.bat`
//!
//! `CreateProcess` loads a PE image; a batch file is text for the command
//! interpreter. A resolved `code.cmd` is therefore started as
//! `cmd.exe /d /s /c <line>`, which [`interpreter_line`] builds.
//!
//! The rules are parameterised by [`Flavour`] and take an `exists` predicate
//! rather than touching the file system, so the Windows order is testable from
//! a Linux host — the only way it can be tested in this tree's gate.

use crate::paths::Flavour;

/// The suffixes a default Windows install associates with an executable.
///
/// Used when `PATHEXT` is unset **or empty**, which is what `cmd.exe` does on
/// the guest: with `PATHEXT=` cleared, `cmd /c dd` still ran `dd.cmd`.
pub const DEFAULT_PATHEXT: &[u8] = b".COM;.EXE;.BAT;.CMD";

/// The suffix list to search, in the order to try it.
///
/// `pathext` is the variable's value as the shell holds it; `None` (unset) and
/// an empty value both fall back to [`DEFAULT_PATHEXT`], and POSIX has no
/// suffix list at all. Entries are kept exactly as written, because that is
/// what the platform appends — a machine that added `.PY` gets `tool.PY`, and
/// the list's own order is the search order.
pub fn suffixes(flavour: Flavour, pathext: Option<&[u8]>) -> Vec<Vec<u8>> {
    if flavour == Flavour::Posix {
        return Vec::new();
    }
    let list = match pathext {
        Some(value) if !value.is_empty() => value,
        _ => DEFAULT_PATHEXT,
    };
    list.split(|&byte| byte == b';')
        .filter(|suffix| !suffix.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

/// Whether `name` already ends with one of `suffixes`.
///
/// This is the "`foo.exe` typed in full" case: appending `PATHEXT` to it would
/// look for `foo.exe.COM`, which is not what any Windows shell does. Windows
/// compares suffixes case-insensitively — `Code.CMD` is `code.cmd` — and POSIX
/// has an empty list, so the comparison there never matches.
pub fn has_suffix(flavour: Flavour, name: &[u8], suffixes: &[Vec<u8>]) -> bool {
    match flavour {
        Flavour::Posix => false,
        Flavour::Windows => suffixes.iter().any(|suffix| {
            name.len() >= suffix.len()
                && name[name.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
        }),
    }
}

/// The files `name` may name, in the order to try them.
///
/// `spelled` says whether the caller wrote the name as a path (it contained a
/// separator). Only then does the literal name become a candidate, and only as
/// the last one; see the module documentation for why.
pub fn candidates(
    flavour: Flavour,
    name: &[u8],
    suffixes: &[Vec<u8>],
    spelled: bool,
) -> Vec<Vec<u8>> {
    if flavour == Flavour::Posix || has_suffix(flavour, name, suffixes) {
        return vec![name.to_vec()];
    }
    let mut out = Vec::with_capacity(suffixes.len() + 1);
    for suffix in suffixes {
        let mut candidate = name.to_vec();
        candidate.extend_from_slice(suffix);
        out.push(candidate);
    }
    if spelled {
        out.push(name.to_vec());
    }
    out
}

/// The first candidate `exists` accepts, or `None`.
///
/// `exists` is the platform's "could this file be executed" test — the execute
/// bit on POSIX, a regular file on Windows — so this function and its callers
/// cannot disagree about what a candidate means.
pub fn first(
    flavour: Flavour,
    name: &[u8],
    suffixes: &[Vec<u8>],
    spelled: bool,
    exists: impl Fn(&[u8]) -> bool,
) -> Option<Vec<u8>> {
    candidates(flavour, name, suffixes, spelled)
        .into_iter()
        .find(|candidate| exists(candidate))
}

/// Whether `path` names a batch file, which only the command interpreter can
/// run.
///
/// `.bat` and `.cmd` are the two extensions `cmd.exe` executes as text, and the
/// test folds case because a name reached through `PATH` may be spelled either
/// way (`CODE.CMD`).
pub fn is_batch_script(path: &[u8]) -> bool {
    let name = path
        .rsplit(|&byte| byte == b'/' || byte == b'\\')
        .next()
        .unwrap_or(path);
    [b".bat".as_slice(), b".cmd"].into_iter().any(|suffix| {
        name.len() > suffix.len() && name[name.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
    })
}

/// The command line `cmd.exe /s /c` takes to run batch file `program`.
///
/// # Shape, and why
///
/// The result is one line: an outer pair of quotes, then the program, then the
/// arguments, each quoted. Each of those choices answers a rule `cmd.exe`
/// actually implements:
///
/// * `/s` makes `cmd.exe` strip the first and last quote of the line and take
///   the rest literally. That outer pair is therefore not decoration: it is
///   what keeps `"C:\Program Files\…\code.cmd"` one word even with spaces in
///   the path, which is the whole reason `code.cmd` needs this path at all.
/// * Because the first and last characters of the line are stripped, the line
///   has to *end* in a quote, so every argument is quoted even when it has no
///   space in it. `interpreter_line(&b"c.cmd", &[b"a".to_vec()])` is
///   `""c.cmd" "a""`.
/// * Inside a token, a backslash run is doubled where it precedes a quote (and
///   at the end of the token) and a quote is written `\"`. That is the rule the
///   C runtime and `CommandLineToArgvW` parse, so the batch file's `%1`…`%9`
///   arrive as the bytes the shell was given.
///
/// # What this cannot preserve
///
/// `cmd.exe` parses the line itself before the batch file's own parser sees
/// it, so two classes of argument survive only as `cmd` reads them:
///
/// * `%NAME%` is expanded — measured on the guest: an argument `%PATH%`
///   arrived as the whole `PATH` — and `cmd.exe` has no escape for `%`.
/// * An embedded `"` is written `\"` for the program's parser, but `cmd`'s own
///   scan for the closing quote does not treat backslash as an escape, so a
///   quote inside an argument can end the quoted run early.
///
/// Quoting cannot fix either: the text is re-parsed by an interpreter whose
/// grammar is not the one the program will use. They are recorded in
/// `docs/portability.md` §8 rather than papered over.
pub fn interpreter_line(program: &[u8], args: &[Vec<u8>]) -> Vec<u8> {
    let mut line = Vec::with_capacity(program.len() + 2);
    line.push(b'"');
    quote_into(&mut line, program);
    for argument in args {
        line.push(b' ');
        quote_into(&mut line, argument);
    }
    line.push(b'"');
    line
}

/// Append `token`, quoted for the C runtime's command-line parser, to `line`.
fn quote_into(line: &mut Vec<u8>, token: &[u8]) {
    line.push(b'"');
    let mut backslashes = 0usize;
    for &byte in token {
        match byte {
            b'\\' => backslashes += 1,
            b'"' => {
                push_backslashes(line, backslashes * 2 + 1);
                line.push(b'"');
                backslashes = 0;
            }
            _ => {
                push_backslashes(line, backslashes);
                backslashes = 0;
                line.push(byte);
            }
        }
    }
    // A trailing backslash run has to be doubled too, or it would escape the
    // closing quote.
    push_backslashes(line, backslashes * 2);
    line.push(b'"');
}

fn push_backslashes(line: &mut Vec<u8>, count: usize) {
    for _ in 0..count {
        line.push(b'\\');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory under the platform's temporary directory.
    ///
    /// The name carries the process id because nextest runs every case in its
    /// own process in parallel with the others.
    fn scratch_directory(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("baash-program-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch directory");
        dir
    }

    fn write_file(path: &std::path::Path, bytes: &[u8]) {
        std::fs::write(path, bytes).expect("write scratch file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Windows has no execute bit, so both files pass its `exists`
            // test; giving the script one here keeps the comparison about the
            // suffix order rather than about permissions.
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
                .expect("make the scratch file executable");
        }
    }

    /// Whether `dir` holds an entry whose name is `candidate`'s final
    /// component, folded for case the way NTFS compares names.
    fn entry_matches(dir: &std::path::Path, candidate: &[u8]) -> bool {
        let name = candidate
            .rsplit(|&byte| byte == b'/')
            .next()
            .unwrap_or(candidate);
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        entries
            .flatten()
            .any(|entry| crate::os::bytes_from_os(entry.file_name()).eq_ignore_ascii_case(name))
    }

    /// The suffixes of a `PATHEXT` value, in order, with or without one.
    #[test]
    fn pathext_is_a_semicolon_list_that_falls_back_when_empty() {
        assert_eq!(
            suffixes(Flavour::Windows, Some(b".COM;.EXE;.BAT;.CMD")),
            [
                b".COM".to_vec(),
                b".EXE".to_vec(),
                b".BAT".to_vec(),
                b".CMD".to_vec()
            ]
        );
        // Unset and empty both mean the platform default: measured, `cmd` still
        // ran `dd.cmd` with `PATHEXT=` cleared.
        assert_eq!(
            suffixes(Flavour::Windows, None),
            [
                b".COM".to_vec(),
                b".EXE".to_vec(),
                b".BAT".to_vec(),
                b".CMD".to_vec()
            ]
        );
        assert_eq!(
            suffixes(Flavour::Windows, Some(b"")),
            [
                b".COM".to_vec(),
                b".EXE".to_vec(),
                b".BAT".to_vec(),
                b".CMD".to_vec()
            ]
        );
        // A machine's own list, including an entry with no dot and a stray
        // separator, is used as written.
        assert_eq!(
            suffixes(Flavour::Windows, Some(b".PY;;PY;.EXE;")),
            [b".PY".to_vec(), b"PY".to_vec(), b".EXE".to_vec()]
        );
        // POSIX has no suffix list, whatever the environment says.
        assert!(suffixes(Flavour::Posix, Some(b".EXE")).is_empty());
    }

    /// `code.cmd` is found although `code` exists; that is the whole bug.
    #[test]
    fn a_name_without_a_suffix_tries_pathext_in_order() {
        let suffixes = suffixes(Flavour::Windows, None);
        let found = first(
            Flavour::Windows,
            b"code",
            &suffixes,
            false,
            // The VS Code layout: the extensionless POSIX script and the batch
            // file beside it. The suffix comes from `PATHEXT`, which a Windows
            // machine spells in upper case, and the file it names is found
            // because the file system does not distinguish the case — which is
            // what `cmd` relies on too.
            |candidate| candidate.eq_ignore_ascii_case(b"code.cmd") || candidate == b"code",
        );
        assert_eq!(found.as_deref(), Some(&b"code.CMD"[..]));
    }

    /// `foo.exe` typed in full is the file, not `foo.exe.COM`.
    #[test]
    fn a_name_that_already_has_a_suffix_is_not_extended() {
        let suffixes = suffixes(Flavour::Windows, None);
        assert!(has_suffix(Flavour::Windows, b"foo.exe", &suffixes));
        assert!(has_suffix(Flavour::Windows, b"foo.EXE", &suffixes));
        // A path's directory may contain dots; only the name's own end counts.
        assert!(has_suffix(Flavour::Windows, br"C:\a.b\foo.Cmd", &suffixes));
        assert!(!has_suffix(Flavour::Windows, b"foo.qzx", &suffixes));
        assert!(!has_suffix(Flavour::Windows, b"exe", &suffixes));
        assert_eq!(
            candidates(Flavour::Windows, b"foo.exe", &suffixes, true),
            [b"foo.exe".to_vec()]
        );
        // The order a full name is tried in is: that name, once.
        assert_eq!(
            first(Flavour::Windows, b"foo.exe", &suffixes, false, |_| true).as_deref(),
            Some(&b"foo.exe"[..])
        );
    }

    /// A searched name never falls back to an extensionless file found in
    /// `PATH`, because `cmd` does not execute one — measured: a `whoami.exe`
    /// copy called `barefile` was listed by `where` and refused by `cmd`.
    #[test]
    fn a_searched_name_does_not_name_an_extensionless_file() {
        let suffixes = suffixes(Flavour::Windows, None);
        assert_eq!(
            first(
                Flavour::Windows,
                b"barefile",
                &suffixes,
                false,
                |candidate| { candidate == b"barefile" }
            ),
            None
        );
        // A path the user spelled is a file they named, so it is tried last.
        assert_eq!(
            first(
                Flavour::Windows,
                br".\barefile",
                &suffixes,
                true,
                |candidate| { candidate == br".\barefile" }
            )
            .as_deref(),
            Some(&br".\barefile"[..])
        );
    }

    /// The candidate order itself, which the `PATH` walk relies on.
    #[test]
    fn candidates_are_the_suffixes_then_the_spelled_file() {
        let suffixes = vec![b".EXE".to_vec(), b".CMD".to_vec()];
        assert_eq!(
            candidates(Flavour::Windows, b"tool", &suffixes, false),
            [b"tool.EXE".to_vec(), b"tool.CMD".to_vec()]
        );
        assert_eq!(
            candidates(Flavour::Windows, b"tool", &suffixes, true),
            [b"tool.EXE".to_vec(), b"tool.CMD".to_vec(), b"tool".to_vec()]
        );
        // `PATHEXT`'s order is the answer when several files exist.
        assert_eq!(
            first(Flavour::Windows, b"tool", &suffixes, false, |_| true).as_deref(),
            Some(&b"tool.EXE"[..])
        );
    }

    /// POSIX keeps exactly the old rule: the name, and no suffix list.
    #[test]
    fn posix_uses_the_name_itself() {
        assert_eq!(
            candidates(Flavour::Posix, b"code", &[], false),
            [b"code".to_vec()]
        );
        assert_eq!(
            candidates(Flavour::Posix, b"./code", &[], true),
            [b"./code".to_vec()]
        );
        // Even if a caller passed a list, POSIX ignores it.
        assert_eq!(
            candidates(Flavour::Posix, b"foo.exe", &[b".COM".to_vec()], false),
            [b"foo.exe".to_vec()]
        );
    }

    /// A batch file is the one kind of program `CreateProcess` cannot load.
    #[test]
    fn batch_scripts_are_recognised_by_extension_and_case() {
        assert!(is_batch_script(b"code.cmd"));
        assert!(is_batch_script(b"code.BAT"));
        assert!(is_batch_script(
            br"C:\Program Files\Microsoft VS Code\bin\code.cmd"
        ));
        assert!(is_batch_script(b"C:/tools/x/CODE.Cmd"));
        // A `.exe` is a PE image, and a name that merely contains `cmd` is not
        // a batch file.
        assert!(!is_batch_script(b"code.exe"));
        assert!(!is_batch_script(b"cmd.exe"));
        assert!(!is_batch_script(b"code.cmd.txt"));
        // The bare name `cmd` is not a batch file: it is the interpreter.
        assert!(!is_batch_script(b"cmd"));
    }

    /// The real shape of the bug, on a real file system.
    ///
    /// A directory holding what VS Code's `bin` holds: `code`, a POSIX shell
    /// script with the execute bit set, and `code.cmd`, the batch file beside
    /// it. On Windows there is no execute bit, so both are ordinary files and
    /// only the suffix order can choose between them — it has to choose
    /// `code.cmd`. The `exists` predicate below is a case-insensitive directory
    /// lookup because an NTFS directory does not distinguish `code.CMD` from
    /// `code.cmd`, and `PATHEXT` spells its suffixes in upper case.
    #[test]
    fn a_real_directory_resolves_code_to_the_batch_file() {
        let dir = scratch_directory("code");
        write_file(
            &dir.join("code"),
            b"#!/usr/bin/env sh\nexec code.exe \"$@\"\n",
        );
        write_file(
            &dir.join("code.cmd"),
            b"@echo off\r\n\"%~dp0..\\Code.exe\" %*\r\n",
        );
        let base = format!("{}/code", dir.display());
        let exists = |candidate: &[u8]| entry_matches(&dir, candidate);
        let suffixes = suffixes(Flavour::Windows, Some(b".COM;.EXE;.BAT;.CMD"));

        // Searched (no separator) and spelled (a path) alike: the batch file.
        let expected_cmd = format!("{base}.CMD");
        assert_eq!(
            first(Flavour::Windows, base.as_bytes(), &suffixes, false, exists).as_deref(),
            Some(expected_cmd.as_bytes())
        );
        assert_eq!(
            first(Flavour::Windows, base.as_bytes(), &suffixes, true, exists).as_deref(),
            Some(expected_cmd.as_bytes())
        );
        // POSIX has no suffix list and no execute-bit question here: the
        // script itself is the program, which is what makes this a Windows
        // rule rather than a change to every platform.
        assert_eq!(
            first(Flavour::Posix, base.as_bytes(), &[], true, exists).as_deref(),
            Some(base.as_bytes())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The line `cmd.exe /s /c` needs, including its outer quote pair.
    #[test]
    fn the_interpreter_line_quotes_every_token() {
        assert_eq!(interpreter_line(b"c.cmd", &[]), b"\"\"c.cmd\"\"");
        assert_eq!(
            interpreter_line(b"c.cmd", &[b"a".to_vec(), b"b".to_vec()]),
            b"\"\"c.cmd\" \"a\" \"b\"\""
        );
        // A path with spaces, which is where `code` lives on a real install.
        assert_eq!(
            interpreter_line(
                br"C:\Program Files\Microsoft VS Code\bin\code.cmd",
                &[b"--version".to_vec()]
            ),
            b"\"\"C:\\Program Files\\Microsoft VS Code\\bin\\code.cmd\" \"--version\"\""
        );
        // An empty argument stays an empty (but quoted) argument.
        assert_eq!(interpreter_line(b"c", &[b"".to_vec()]), b"\"\"c\" \"\"\"");
        // A trailing backslash is doubled so it cannot escape the closing
        // quote, and an embedded quote becomes `\"`.
        assert_eq!(
            interpreter_line(br"C:\dir\", &[br"a b\".to_vec()]),
            br#"""C:\dir\\" "a b\\"""#
        );
        assert_eq!(
            interpreter_line(b"c", &[br#"say "hi""#.to_vec()]),
            b"\"\"c\" \"say \\\"hi\\\"\"\""
        );
    }
}
