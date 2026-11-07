//! The directory stack: `dirs`, `pushd` and `popd`.
//!
//! The stack lives in [`ShellState`], not in this module, because a `( … )`
//! subshell clones the state and must not leak a `pushd` back into the parent.
//! Element 0 is always the current directory — the same order bash uses in
//! `DIRSTACK` and prints first — so `cd` only has to rewrite element 0 and
//! everything else stays where `pushd` put it.
//!
//! Paths are kept lexically normalised (`//`, `.` and `..` resolved) rather
//! than canonicalised: `pushd ..` must print `/` for `cd /tmp`, and a
//! symlinked directory must keep the name the user typed, which is also what
//! `PWD` and the prompt show.

use baash_diag::Diagnostic;
use baash_runtime::{ShellHost, Status};

use super::Interp;

/// Enter `target` and record it in `PWD`/`OLDPWD`, returning the new `PWD`.
///
/// Shared with `cd` so the two built-ins cannot disagree about what `PWD`
/// means; the caller decides what to print and which status a failure has.
///
/// `physical` selects bash's `-P` over the default `-L`. `-L` (logical) tracks
/// the name the user spelled: the target is appended to the *logical* `PWD`
/// and collapsed textually, and that string is both what is chdir'ed to and
/// what `PWD` records — which is why `cd symlink` keeps the symlink's name and
/// `cd ..` leaves it by name. `-P` asks the kernel and `getcwd()` instead.
///
/// The textual collapse is [`baash_sys::paths::resolve`]'s job, because what a
/// root is differs by platform (`docs/portability.md`). It answers `None` for
/// the names no string can place — a Windows drive-relative `C:foo` — and then
/// this asks the kernel, exactly as `-P` does.
pub(super) fn enter(
    interp: &mut Interp,
    target: &[u8],
    physical: bool,
) -> std::io::Result<Vec<u8>> {
    let previous = interp.state.vars.scalar(b"PWD").map(<[u8]>::to_vec);
    let current = if physical {
        baash_sys::chdir(target)?;
        cwd_bytes(target)
    } else {
        match baash_sys::paths::resolve(&current(interp), target) {
            Some(logical) => {
                baash_sys::chdir(&logical)?;
                logical
            }
            None => {
                baash_sys::chdir(target)?;
                cwd_bytes(target)
            }
        }
    };
    if let Some(previous) = previous {
        let _ = interp.state.vars.assign_scalar(b"OLDPWD", &previous);
        let _ = interp.state.vars.mark_exported(b"OLDPWD");
    }
    let _ = interp.state.vars.assign_scalar(b"PWD", &current);
    let _ = interp.state.vars.mark_exported(b"PWD");
    Ok(current)
}

/// Walk `target` the way a logical `cd` does, failing when a component it
/// names is not a directory.
///
/// bash's `-L` resolves `..` textually but still reports a path whose named
/// component does not exist: `cd nonexistent/..` fails while `cd a/b/..`
/// succeeds, and `cd symlink/../c` ends up in the textual parent's `c`. The
/// check is therefore per *named* component, against the textual prefix built
/// so far — never against the raw path, whose `..` the kernel would resolve
/// through the symlink.
pub(super) fn check_components(interp: &Interp, target: &[u8]) -> std::io::Result<()> {
    let Some(prefixes) = baash_sys::paths::prefixes(&current(interp), target) else {
        // The name needs the kernel to place it at all (a Windows
        // drive-relative `C:foo`), so it also gets the kernel's walk.
        return Ok(());
    };
    for path in prefixes {
        let metadata = std::fs::metadata(baash_sys::os::path_from_bytes(&path))?;
        if !metadata.is_dir() {
            return Err(std::io::Error::from(std::io::ErrorKind::NotADirectory));
        }
    }
    Ok(())
}

/// The directory the process is in, as the kernel spells it, falling back to
/// `target` when even `getcwd` fails.
fn cwd_bytes(target: &[u8]) -> Vec<u8> {
    std::env::current_dir()
        .map(baash_sys::os::bytes_from_os)
        .unwrap_or_else(|_| target.to_vec())
}

/// The current directory as the shell spells it.
fn current(interp: &Interp) -> Vec<u8> {
    interp
        .state
        .vars
        .scalar(b"PWD")
        .map(<[u8]>::to_vec)
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(baash_sys::os::bytes_from_os)
        })
        .unwrap_or_else(baash_sys::paths::root)
}

/// The stack with element 0 materialised even when nothing was pushed yet.
fn stack(interp: &Interp) -> Vec<Vec<u8>> {
    if interp.state.dirs.is_empty() {
        vec![current(interp)]
    } else {
        interp.state.dirs.clone()
    }
}

fn store(interp: &mut Interp, stack: Vec<Vec<u8>>) {
    interp.state.dirs = stack;
}

/// Replace element 0 after a successful `cd`, keeping the rest of the stack.
pub(super) fn sync(interp: &mut Interp, current: &[u8]) {
    if let Some(first) = interp.state.dirs.first_mut() {
        *first = current.to_vec();
    }
}

/// `pwd`: the logical directory (`-L`, the default) or the kernel's answer
/// (`-P`).
///
/// The logical answer is `$PWD`, but only when it still names the current
/// directory: a stale `PWD` (a script that `cd`s behind the shell's back, or a
/// directory that was removed) falls back to `getcwd()`, which is bash's
/// check.
pub(crate) fn bi_pwd(interp: &mut Interp, argv: &[Vec<u8>]) -> Status {
    let name = super::builtins::program(argv, "pwd");
    let mut physical = false;
    for argument in &argv[1..] {
        match argument.as_slice() {
            b"-L" => physical = false,
            b"-P" => physical = true,
            b"--" => {}
            other if other.first() == Some(&b'-') && other.len() > 1 => {
                let text = String::from_utf8_lossy(other).into_owned();
                return fail(interp, &name, &format!("{text}: invalid option"), Status(2));
            }
            // bash ignores operands (`pwd /` prints the directory).
            _ => {}
        }
    }
    let mut out = if !physical
        && let Some(pwd) = interp.state.vars.scalar(b"PWD")
        && names_current_directory(pwd)
    {
        pwd.to_vec()
    } else {
        std::env::current_dir()
            .map(baash_sys::os::bytes_from_os)
            .unwrap_or_else(|_| current(interp))
    };
    out.push(b'\n');
    interp.write_out(&out)
}

/// Whether `path` is an absolute name for the directory the process is in.
///
/// This is the test bash applies before trusting `PWD`: stat the two and
/// compare identity, so `PWD=/tmp; cd /var` (typed by something other than the
/// shell) is not believed.
fn names_current_directory(path: &[u8]) -> bool {
    // `$PWD` has to be an absolute name before comparing it with anything: a
    // relative one names different directories depending on where it is read
    // (`C:foo` on Windows is relative to wherever that drive was left).
    if !baash_sys::paths::is_absolute(path) {
        return false;
    }
    let probe = baash_sys::os::path_from_bytes(path);
    same_file(&probe, std::path::Path::new("."))
}

#[cfg(unix)]
fn same_file(left: &std::path::Path, right: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(left), std::fs::metadata(right)) {
        (Ok(left), Ok(right)) => left.dev() == right.dev() && left.ino() == right.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_file(left: &std::path::Path, right: &std::path::Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn fail(interp: &mut Interp, name: &str, message: &str, status: Status) -> Status {
    interp.diagnose(&Diagnostic::error(
        crate::codes::DEFERRED,
        0..0,
        format!("{name}: {message}"),
    ));
    status
}

/// The status for an empty directory name.
///
/// bash reports `cd ""` as "null directory" rather than as the failed `chdir`
/// an empty path would be, and does not search `CDPATH` for one; `pushd ""`
/// says the same thing.
pub(super) fn null_directory(interp: &mut Interp, name: &str) -> Status {
    fail(interp, name, "null directory", Status::FAILURE)
}

/// `$HOME`, or the platform's profile directory where a native process is
/// started without one (Windows; see [`baash_sys::dirs::fallback_home`]).
pub(super) fn home(interp: &Interp) -> Option<Vec<u8>> {
    interp
        .state
        .vars
        .scalar(b"HOME")
        .map(<[u8]>::to_vec)
        .or_else(baash_sys::dirs::fallback_home)
}

/// `%2d  path`, with the tilde abbreviation `dirs` uses unless `-l`.
fn spell(interp: &Interp, path: &[u8], long: bool) -> Vec<u8> {
    spell_with_home(path, home(interp).as_deref(), long)
}

/// [`spell`] against an explicit home, so the abbreviation is testable without
/// a shell.
fn spell_with_home(path: &[u8], home: Option<&[u8]>, long: bool) -> Vec<u8> {
    if long {
        return path.to_vec();
    }
    let Some(home) = home else {
        return path.to_vec();
    };
    if home.is_empty() || home == b"/" {
        return path.to_vec();
    }
    match baash_sys::paths::strip_prefix(path, home) {
        // `strip_prefix` accepts a whole component only, so `/home/ada`
        // abbreviates `/home/ada/work` but not `/home/adam`.
        //
        // A remainder made only of separators is the home directory itself:
        // the comparison drops a root's trailing separator first (`C:\` is
        // compared as `C:`), so its own root comes back as a lone separator.
        Some(rest)
            if rest
                .iter()
                .all(|&byte| baash_sys::paths::is_separator(byte)) =>
        {
            b"~".to_vec()
        }
        Some(rest) => {
            let mut out = b"~".to_vec();
            out.extend_from_slice(rest);
            out
        }
        None => path.to_vec(),
    }
}

fn write_stack(interp: &mut Interp, numbered: bool, one_per_line: bool, long: bool) -> Status {
    let entries = stack(interp);
    let mut out = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if numbered {
            if index > 0 {
                out.push(b'\n');
            }
            out.extend_from_slice(format!("{index:2}  ").as_bytes());
        } else if index > 0 {
            out.push(if one_per_line { b'\n' } else { b' ' });
        }
        out.extend_from_slice(&spell(interp, entry, long));
    }
    out.push(b'\n');
    interp.write_out(&out)
}

/// A parsed `+N` / `-N` operand.
enum Index {
    /// Not an index at all (`zzz`), which is a usage error.
    Malformed,
    /// An index outside the stack.
    OutOfRange,
    /// A valid position.
    At(usize),
}

fn lookup(argument: &[u8], len: usize) -> Index {
    let (from_end, digits) = match argument.split_first() {
        Some((b'+', rest)) => (false, rest),
        Some((b'-', rest)) => (true, rest),
        _ => return Index::Malformed,
    };
    if digits.is_empty() {
        return Index::Malformed;
    }
    let mut value: usize = 0;
    for digit in digits {
        if !digit.is_ascii_digit() {
            return Index::Malformed;
        }
        value = value * 10 + usize::from(digit - b'0');
    }
    let position = if from_end {
        len.checked_sub(value + 1)
    } else {
        (value < len).then_some(value)
    };
    match position {
        Some(position) => Index::At(position),
        None => Index::OutOfRange,
    }
}

pub(crate) fn bi_dirs(interp: &mut Interp, argv: &[Vec<u8>]) -> Status {
    let name = "dirs";
    let mut clear = false;
    let mut long = false;
    let mut one_per_line = false;
    let mut numbered = false;
    for argument in &argv[1..] {
        match argument.as_slice() {
            b"--" => {}
            b"-c" => clear = true,
            b"-l" => long = true,
            b"-p" => one_per_line = true,
            b"-v" => numbered = true,
            other if other.len() > 1 && other[0] == b'-' => {
                return fail(
                    interp,
                    name,
                    &format!("{}: invalid option", String::from_utf8_lossy(other)),
                    Status::FAILURE,
                );
            }
            other => {
                return fail(
                    interp,
                    name,
                    &format!("{}: too many arguments", String::from_utf8_lossy(other)),
                    Status::FAILURE,
                );
            }
        }
    }
    if clear {
        let cwd = current(interp);
        store(interp, vec![cwd]);
        return Status::SUCCESS;
    }
    write_stack(interp, numbered, one_per_line, long)
}

pub(crate) fn bi_pushd(interp: &mut Interp, argv: &[Vec<u8>]) -> Status {
    let name = "pushd";
    let mut rotate = false;
    let mut no_chdir = false;
    let mut operand: Option<Vec<u8>> = None;
    for argument in &argv[1..] {
        match argument.as_slice() {
            b"--" => {}
            b"-n" => no_chdir = true,
            other if other == b"+0" || (other.first() == Some(&b'+') && other.len() > 1) => {
                rotate = true;
                operand = Some(other.to_vec());
            }
            other if other.len() > 1 && other[0] == b'-' && other != b"-" => {
                if other[1].is_ascii_digit() {
                    rotate = true;
                    operand = Some(other.to_vec());
                } else {
                    return fail(
                        interp,
                        name,
                        &format!("{}: invalid option", String::from_utf8_lossy(other)),
                        Status(2),
                    );
                }
            }
            other => {
                if operand.is_some() {
                    return fail(interp, name, "too many arguments", Status::FAILURE);
                }
                operand = Some(other.to_vec());
            }
        }
    }

    let mut entries = stack(interp);
    let Some(operand) = operand else {
        if entries.len() < 2 {
            return fail(interp, name, "no other directory", Status::FAILURE);
        }
        entries.swap(0, 1);
        let target = entries[0].clone();
        if !no_chdir && let Err(error) = enter(interp, &target, false) {
            return fail(
                interp,
                name,
                &format!("{}: {error}", String::from_utf8_lossy(&target)),
                Status::FAILURE,
            );
        }
        store(interp, entries);
        return write_stack(interp, false, false, false);
    };

    if rotate {
        let index = match lookup(&operand, entries.len()) {
            Index::At(index) => index,
            Index::OutOfRange | Index::Malformed => {
                return fail(
                    interp,
                    name,
                    "directory stack index out of range",
                    Status::FAILURE,
                );
            }
        };
        entries.rotate_left(index);
        let target = entries[0].clone();
        if !no_chdir && let Err(error) = enter(interp, &target, false) {
            return fail(
                interp,
                name,
                &format!("{}: {error}", String::from_utf8_lossy(&target)),
                Status::FAILURE,
            );
        }
        store(interp, entries);
        return write_stack(interp, false, false, false);
    }

    if operand.is_empty() {
        return null_directory(interp, name);
    }
    // `-n` records the name without entering it, so the stack keeps what the
    // operand resolves to; otherwise `enter` decides, which also covers the
    // names only the kernel can place.
    let target = if no_chdir {
        baash_sys::paths::resolve(&current(interp), &operand).unwrap_or(operand)
    } else {
        match enter(interp, &operand, false) {
            Ok(entered) => entered,
            Err(error) => {
                return fail(
                    interp,
                    name,
                    &format!("{}: {error}", String::from_utf8_lossy(&operand)),
                    Status::FAILURE,
                );
            }
        }
    };
    entries.insert(0, target);
    store(interp, entries);
    write_stack(interp, false, false, false)
}

pub(crate) fn bi_popd(interp: &mut Interp, argv: &[Vec<u8>]) -> Status {
    let name = "popd";
    let mut no_chdir = false;
    let mut operand: Option<Vec<u8>> = None;
    for argument in &argv[1..] {
        match argument.as_slice() {
            b"--" => {}
            b"-n" => no_chdir = true,
            other
                if other.len() > 1
                    && (other[0] == b'+' || other[0] == b'-')
                    && other[1].is_ascii_digit() =>
            {
                operand = Some(other.to_vec());
            }
            other if other.len() > 1 && other[0] == b'-' => {
                return fail(
                    interp,
                    name,
                    &format!("{}: invalid option", String::from_utf8_lossy(other)),
                    Status(2),
                );
            }
            other => {
                if operand.is_some() {
                    return fail(interp, name, "too many arguments", Status::FAILURE);
                }
                operand = Some(other.to_vec());
            }
        }
    }

    let mut entries = stack(interp);
    let remove = match &operand {
        None => 0,
        Some(argument) => match lookup(argument, entries.len()) {
            Index::At(index) => index,
            Index::OutOfRange => {
                return fail(
                    interp,
                    name,
                    "directory stack index out of range",
                    Status::FAILURE,
                );
            }
            Index::Malformed => {
                return fail(
                    interp,
                    name,
                    &format!("{}: invalid argument", String::from_utf8_lossy(argument)),
                    Status(2),
                );
            }
        },
    };
    if entries.len() <= 1 && remove == 0 {
        return fail(interp, name, "directory stack empty", Status::FAILURE);
    }
    entries.remove(remove);
    let target = entries[0].clone();
    if !no_chdir && let Err(error) = enter(interp, &target, false) {
        return fail(
            interp,
            name,
            &format!("{}: {error}", String::from_utf8_lossy(&target)),
            Status::FAILURE,
        );
    }
    store(interp, entries);
    write_stack(interp, false, false, false)
}

#[cfg(test)]
mod tests {
    use super::{Index, lookup, spell_with_home};

    /// The lexical walk itself lives in `baash_sys::paths`, where both
    /// platforms' rules are tested; what is shell-specific is the spelling
    /// `dirs` prints, and that is what these cover.
    #[test]
    fn the_tilde_abbreviation_needs_a_whole_component() {
        let home = Some(b"/home/ada".as_slice());
        assert_eq!(spell_with_home(b"/home/ada/work", home, false), b"~/work");
        assert_eq!(spell_with_home(b"/home/ada", home, false), b"~");
        assert_eq!(spell_with_home(b"/home/adam", home, false), b"/home/adam");
        // `-l` prints the path as recorded.
        assert_eq!(
            spell_with_home(b"/home/ada/work", home, true),
            b"/home/ada/work"
        );
        // A home of `/` is not abbreviated: every path starts with it.
        assert_eq!(spell_with_home(b"/tmp", Some(b"/"), false), b"/tmp");
        assert_eq!(spell_with_home(b"/tmp", Some(b""), false), b"/tmp");
        assert_eq!(spell_with_home(b"/tmp", None, false), b"/tmp");
    }

    #[test]
    fn indexes_count_from_the_top_or_the_bottom() {
        assert!(matches!(lookup(b"+0", 3), Index::At(0)));
        assert!(matches!(lookup(b"+2", 3), Index::At(2)));
        assert!(matches!(lookup(b"-0", 3), Index::At(2)));
        assert!(matches!(lookup(b"-2", 3), Index::At(0)));
        assert!(matches!(lookup(b"+3", 3), Index::OutOfRange));
        assert!(matches!(lookup(b"-3", 3), Index::OutOfRange));
        assert!(matches!(lookup(b"zzz", 3), Index::Malformed));
        assert!(matches!(lookup(b"+1x", 3), Index::Malformed));
        assert!(matches!(lookup(b"+", 3), Index::Malformed));
    }
}
