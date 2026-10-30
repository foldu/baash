//! Embedded coreutils.
//!
//! `printf`, `test`/`[` and the rest of the standard utilities are regular
//! builtins that deliberately fall through to `PATH` (`DESIGN.md` §2), so an
//! external binary always wins. That works on Unix and is useless on Windows,
//! where a `PATH` has no coreutils at all. This crate is the fallback: when
//! resolution finds nothing, the executor runs the uutils implementation in
//! this process instead of reporting "command not found".
//!
//! Only the utilities that are actually embedded are linked: each `uu_*` crate
//! is a library with one entry point, and the table below is the whole
//! surface. `true`, `false`, `echo` and `pwd` stay the VM's own bodies — they
//! are cheaper in-process and bash-shaped, and a script must not notice the
//! difference between them and a utility.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::ffi::OsString;

/// One embedded utility: its name and the uutils entry point.
type Entry = fn(Vec<OsString>) -> i32;

/// The embedded utilities, by the name a command is typed as.
///
/// The first argument passed to [`run`] is the name itself, because that is
/// what `argv[0]` would be and what clap parses.
const UTILITIES: &[(&str, Entry)] = &[
    ("printf", run_printf),
    ("test", run_test),
    ("[", run_bracket),
];

/// Run `argv` if `argv[0]` names an embedded utility.
///
/// Returns `None` when the name is not embedded, so the caller keeps whatever
/// diagnostic it would have produced.
pub fn run(argv: &[Vec<u8>]) -> Option<i32> {
    let (name, _) = argv.split_first()?;
    let name = std::str::from_utf8(name).ok()?;
    let (_, entry) = UTILITIES.iter().find(|(entry, _)| *entry == name)?;
    let args: Vec<OsString> = argv
        .iter()
        .map(|arg| bsh_sys::os::os_from_bytes(arg))
        .collect();
    let status = entry(args);
    // A utility's own binary flushes these when the process exits; in-process
    // the shell has to, or `printf x > file` leaves the bytes in the buffer
    // and the file empty.
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let _ = std::io::Write::flush(&mut std::io::stderr());
    Some(status)
}

/// Whether `name` is embedded.
pub fn contains(name: &[u8]) -> bool {
    std::str::from_utf8(name)
        .ok()
        .is_some_and(|name| UTILITIES.iter().any(|(utility, _)| *utility == name))
}

fn run_printf(args: Vec<OsString>) -> i32 {
    uu_printf::uumain(args.into_iter())
}

fn run_test(args: Vec<OsString>) -> i32 {
    uu_test::uumain(args.into_iter())
}

/// `[ … ]` is `test …` with a closing bracket.
///
/// `uu_test` decides bracket mode from the *process* name
/// (`uucore::util_name()`), which is `bsh` here, so an embedded call would see
/// the `]` as an operand and report "extra argument". The bracket is therefore
/// removed here and the utility is entered as `test`, which is the definition.
fn run_bracket(mut args: Vec<OsString>) -> i32 {
    if args.last().is_some_and(|last| last == "]") {
        args.pop();
    } else if args.len() > 1 {
        eprintln!("bsh: [: missing `]'");
        return 2;
    }
    if let Some(first) = args.first_mut() {
        *first = OsString::from("test");
    }
    uu_test::uumain(args.into_iter())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<Vec<u8>> {
        items.iter().map(|item| item.as_bytes().to_vec()).collect()
    }

    #[test]
    fn test_is_embedded_and_returns_its_status() {
        assert_eq!(run(&argv(&["test", "1", "=", "1"])), Some(0));
        assert_eq!(run(&argv(&["test", "1", "=", "2"])), Some(1));
    }

    #[test]
    fn bracket_is_test_with_the_closing_bracket_removed() {
        assert_eq!(run(&argv(&["[", "1", "=", "1", "]"])), Some(0));
        assert_eq!(run(&argv(&["[", "1", "=", "2", "]"])), Some(1));
        assert_eq!(run(&argv(&["[", "1", "=", "1"])), Some(2));
    }

    #[test]
    fn an_unknown_name_is_not_embedded() {
        assert_eq!(run(&argv(&["cat", "x"])), None);
        assert!(!contains(b"cat"));
    }

    #[test]
    fn printf_is_embedded() {
        assert!(contains(b"printf"));
        assert_eq!(run(&argv(&["printf", "%s", "x"])), Some(0));
    }
}
