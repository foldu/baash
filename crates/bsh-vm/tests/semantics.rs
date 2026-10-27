//! The two traits `bsh-lang` expands against, exercised directly on [`Interp`].
//!
//! These are the contracts `docs/contracts.md` records for `bsh-lang`'s
//! `Vars` and `Shell` seams: the special parameters served by `get`, the
//! `unset` vs `null` distinction, `$#` from `positional()`, and the three
//! callbacks (command substitution, arithmetic, pathname expansion). The
//! end-to-end behaviour is covered by `bsh`'s `execute` test; this file pins
//! the API shape a later backend will also have to satisfy.

use std::path::PathBuf;

use bsh_lang::{Arithmetic, Field, Shell, Vars, Word};
use bsh_vm::Interp;

fn interp() -> Interp {
    let mut interp = Interp::new(false);
    interp.set_arg0("bsh");
    interp
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bsh-vm-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn text(bytes: Option<&[u8]>) -> Option<String> {
    bytes.map(|bytes| String::from_utf8_lossy(bytes).into_owned())
}

#[test]
fn get_serves_the_specials_the_contract_names() {
    let mut interp = interp();
    interp.set_positional(vec![b"one".to_vec(), b"two".to_vec()]);

    assert_eq!(text(Vars::get(&interp, b"0")), Some("bsh".to_owned()));
    assert_eq!(text(Vars::get(&interp, b"?")), Some("0".to_owned()));
    let pid = Vars::get(&interp, b"$").expect("$$ is always available");
    assert!(
        pid.iter().all(u8::is_ascii_digit),
        "$$ should be digits, got {:?}",
        text(Some(pid))
    );
    // No background job yet: `$!` is empty, not absent.
    assert_eq!(text(Vars::get(&interp, b"!")), Some(String::new()));
    // `$-` lists the enabled single-letter options.
    let flags = Vars::get(&interp, b"-").expect("$- is always available");
    assert!(flags.is_ascii(), "$- should be option letters: {flags:?}");
    // `$#` is *not* a `get` lookup: expansion computes it from `positional()`.
    assert_eq!(Vars::get(&interp, b"#"), None);
    assert_eq!(Vars::positional(&interp).len(), 2);
}

#[test]
fn status_updates_are_visible_through_get() {
    let mut interp = interp();
    assert_eq!(interp.run_named("-c", b"false"), bsh_runtime::Status(1));
    assert_eq!(text(Vars::get(&interp, b"?")), Some("1".to_owned()));
    assert_eq!(interp.run_named("-c", b"true"), bsh_runtime::Status(0));
    assert_eq!(text(Vars::get(&interp, b"?")), Some("0".to_owned()));
}

#[test]
fn unset_and_null_stay_distinct() {
    let mut interp = interp();
    interp.run_named("-c", b"empty=");
    assert_eq!(text(Vars::get(&interp, b"empty")), Some(String::new()));
    assert_eq!(
        text(Vars::get(&interp, b"missing")),
        None,
        "an unset variable is None, not an empty value"
    );
    // The difference is observable through expansion, which is the point.
    assert_eq!(
        interp.run_named("-c", b"echo ${missing-unset} ${empty-null}"),
        bsh_runtime::Status(0)
    );
}

#[test]
fn set_from_expansion_refuses_readonly_and_invalid_names() {
    let mut interp = interp();
    interp.run_named("-c", b"readonly frozen=1");
    assert_eq!(
        Vars::set(&mut interp, b"frozen", b"2".to_vec()),
        Err(bsh_lang::SetError::ReadOnly)
    );
    assert_eq!(
        Vars::set(&mut interp, b"not a name", b"2".to_vec()),
        Err(bsh_lang::SetError::InvalidName)
    );
    assert_eq!(Vars::set(&mut interp, b"free", b"2".to_vec()), Ok(()));
    assert_eq!(text(Vars::get(&interp, b"free")), Some("2".to_owned()));
}

#[test]
fn arithmetic_callback_evaluates_and_writes_back() {
    let mut interp = interp();
    let value = |interp: &mut Interp, source: &str| {
        Shell::arithmetic(interp, &Arithmetic::new(source)).expect("arithmetic should succeed")
    };
    assert_eq!(value(&mut interp, "1 + 2 * 3"), b"7");
    assert_eq!(value(&mut interp, "(1 + 2) * 3"), b"9");
    assert_eq!(value(&mut interp, "7 / 2"), b"3");
    // A non-numeric or unset name reads as 0 (DESIGN.md §3.3 Tier C).
    assert_eq!(value(&mut interp, "unknown + 1"), b"1");
    interp.run_named("-c", b"n=41");
    assert_eq!(value(&mut interp, "n + 1"), b"42");
    // Assignments inside arithmetic persist as decimal text.
    assert_eq!(value(&mut interp, "n += 1"), b"42");
    assert_eq!(value(&mut interp, "n++"), b"42");
    assert_eq!(value(&mut interp, "n"), b"43");
    // Overflow wraps at 64 bits rather than failing.
    assert_eq!(
        value(&mut interp, "9223372036854775807 + 1"),
        i64::MIN.to_string().as_bytes()
    );
    // Real errors are errors, with the shell's message.
    let error = Shell::arithmetic(&mut interp, &Arithmetic::new("1 / 0")).unwrap_err();
    assert!(format!("{error}").contains("division by 0"), "{error}");
}

// `Shell::command_substitution` forks, and DESIGN.md §4.3 is explicit that
// forking is only sound in the single-threaded shell process — a test harness
// runs tests on several threads, which is exactly the case the fork gate
// exists to forbid. The callback is therefore covered end to end by
// `bsh`'s `execute` tests (which run the real, single-threaded binary):
// trailing-newline stripping, `$?` from the substitution, and subshell
// isolation all live there.

#[test]
fn pathname_callback_expands_only_unquoted_metacharacters() {
    let dir = temp_dir("pathname");
    for name in ["a.txt", "b.txt", "c.md"] {
        std::fs::write(dir.join(name), "").unwrap();
    }
    let mut interp = interp();
    let pattern = format!("{}/*.txt", dir.display());

    let matches = Shell::pathname(&mut interp, &Field::plain(pattern.clone().into_bytes()))
        .expect("pathname")
        .expect("some matches");
    let text: Vec<String> = matches
        .iter()
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .collect();
    assert_eq!(text.len(), 2, "{text:?}");
    assert!(text[0].ends_with("a.txt"), "{text:?}");
    assert!(text[1].ends_with("b.txt"), "{text:?}");

    // Nothing matched: `None`, so the word is left alone (POSIX default).
    let pattern = format!("{}/*.none", dir.display());
    assert_eq!(
        Shell::pathname(&mut interp, &Field::plain(pattern.clone().into_bytes())).unwrap(),
        None
    );

    // A fully quoted field has no active metacharacter, so the callback must
    // not glob it; `bsh-lang` would not even call it, but the contract holds.
    let pattern = format!("{}/*.txt", dir.display());
    assert_eq!(
        Shell::pathname(&mut interp, &Field::matched(pattern.into_bytes())).unwrap(),
        None
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn command_substitution_is_not_run_for_a_plain_word() {
    // A guard against the tempting shortcut of treating `$(…)` text as data:
    // the callback is what executes it, so a literal word must stay literal.
    let mut interp = interp();
    interp.run_named("-c", b"echo '$(not-run)'");
    let word = Word::lit("$(not-run)");
    let expanded = bsh_lang::expand_words(&[word], &mut interp).expect("expansion");
    assert_eq!(expanded, vec![b"$(not-run)".to_vec()]);
}
