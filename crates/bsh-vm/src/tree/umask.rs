//! The `umask` built-in.
//!
//! Two grammars in one command: an octal mask, and the POSIX symbolic form
//! `[who...][[+-=][perms...]]...` with clauses separated by commas. The
//! symbolic form edits the *allowed* bits (the complement of the mask), which
//! is why `umask u-r` on `0124` yields `0524`: clearing user read from the
//! allowed `0653` leaves `0253`, whose complement is `0524`.
//!
//! The parser follows the behaviour `spec/oils/builtin-umask.test.sh`
//! records for `osh`, including the corners bash rejects: numeric masks are
//! truncated to the permission bits (`1234567` is `0567`), `X` only adds
//! execute where some execute bit is already allowed, `s`/`t` are accepted
//! and contribute nothing, and a permission may copy another class (`a=u`)
//! from the value the invocation started with.
//!
//! The mask is process-wide state in the kernel, so this body asks
//! [`bsh_sys::umask`] instead of caching a copy: an `exec`'d command must see
//! the same mask the shell printed.

use bsh_diag::Diagnostic;
use bsh_runtime::{ShellHost, Status};

use super::Interp;

pub(crate) fn bi_umask(interp: &mut Interp, argv: &[Vec<u8>]) -> Status {
    let name =
        String::from_utf8_lossy(argv.first().map_or(&b"umask"[..], Vec::as_slice)).into_owned();
    let mut symbolic = false;
    let mut reusable = false;
    let mut operands: Vec<&[u8]> = Vec::new();
    let mut index = 1;
    let mut options_done = false;
    while let Some(argument) = argv.get(index) {
        index += 1;
        if options_done {
            operands.push(argument.as_slice());
        } else if argument == b"--" {
            options_done = true;
        } else if argument == b"-" {
            // A lone `-` is not an option here: bash treats it as a query.
            options_done = true;
        } else if argument == b"-S" {
            symbolic = true;
        } else if argument == b"-p" {
            reusable = true;
        } else if argument.len() > 1 && argument[0] == b'-' {
            return fail(interp, &name, "invalid option");
        } else {
            operands.push(argument.as_slice());
        }
    }

    if operands.len() > 1 {
        return fail(interp, &name, "too many arguments");
    }

    let Some(operand) = operands.first() else {
        let Some(mask) = current_mask() else {
            return fail(interp, &name, "not supported on this platform");
        };
        let text = if symbolic {
            format!("{}\n", symbolic_form(mask))
        } else if reusable {
            format!("umask {mask:04o}\n")
        } else {
            format!("{mask:04o}\n")
        };
        return interp.write_out(text.as_bytes());
    };

    let mask = if !operand.is_empty() && operand.iter().all(|byte| (b'0'..=b'7').contains(byte)) {
        let mut mask = 0u32;
        for byte in *operand {
            mask = (mask << 3) | u32::from(byte - b'0');
        }
        mask & 0o777
    } else {
        match parse_symbolic(operand, current_mask().unwrap_or(0o022)) {
            Some(mask) => mask,
            None => return fail(interp, &name, "invalid symbolic mode"),
        }
    };

    if bsh_sys::umask(mask).is_none() {
        return fail(interp, &name, "not supported on this platform");
    }
    Status::SUCCESS
}

/// The current mask, read by setting the same value back: there is no
/// read-only syscall, and the shell is single-threaded around built-ins.
fn current_mask() -> Option<u32> {
    let previous = bsh_sys::umask(0)?;
    bsh_sys::umask(previous)?;
    Some(previous)
}

fn fail(interp: &mut Interp, name: &str, message: &str) -> Status {
    interp.diagnose(&Diagnostic::error(
        crate::codes::DEFERRED,
        0..0,
        format!("{name}: {message}"),
    ));
    Status::FAILURE
}

fn symbolic_form(mask: u32) -> String {
    let allowed = !mask & 0o777;
    let mut text = String::new();
    for (slot, who) in [(0, 'u'), (1, 'g'), (2, 'o')] {
        if slot > 0 {
            text.push(',');
        }
        text.push(who);
        text.push('=');
        let bits = (allowed >> (6 - 3 * slot)) & 0o7;
        if bits & 0o4 != 0 {
            text.push('r');
        }
        if bits & 0o2 != 0 {
            text.push('w');
        }
        if bits & 0o1 != 0 {
            text.push('x');
        }
    }
    text
}

fn parse_symbolic(text: &[u8], mask: u32) -> Option<u32> {
    let mut state = classes(!mask & 0o777);
    let initial = state;
    // `X` follows the reference shell, which adds execute only when the mask
    // itself forbids some execute bit; a mask with no execute bits makes
    // `X` contribute nothing.
    let executes = mask & 0o111 != 0;
    for clause in text.split(|&byte| byte == b',') {
        if clause.is_empty() {
            return None;
        }
        let mut position = 0;
        let mut who = [false; 3];
        let mut any_who = false;
        while let Some(&byte) = clause.get(position) {
            let slot = match byte {
                b'u' => 0,
                b'g' => 1,
                b'o' => 2,
                b'a' => {
                    who = [true; 3];
                    any_who = true;
                    position += 1;
                    continue;
                }
                _ => break,
            };
            who[slot] = true;
            any_who = true;
            position += 1;
        }
        if !any_who {
            who = [true; 3];
        }
        if position == clause.len() {
            // A class with no operator (`umask u`) is not a mode.
            return None;
        }
        while position < clause.len() {
            let op = clause[position];
            if !matches!(op, b'+' | b'-' | b'=') {
                return None;
            }
            position += 1;
            let start = position;
            while position < clause.len() && !matches!(clause[position], b'+' | b'-' | b'=') {
                position += 1;
            }
            apply(
                &mut state,
                &who,
                &clause[start..position],
                op,
                initial,
                executes,
            )?;
        }
    }
    let mut mask = 0;
    for (slot, bits) in state.iter().enumerate() {
        mask |= u32::from(*bits) << (6 - 3 * slot);
    }
    Some(!mask & 0o777)
}

fn classes(allowed: u32) -> [u8; 3] {
    [
        (allowed >> 6) as u8 & 0o7,
        (allowed >> 3) as u8 & 0o7,
        allowed as u8 & 0o7,
    ]
}

fn apply(
    state: &mut [u8; 3],
    who: &[bool; 3],
    perms: &[u8],
    op: u8,
    initial: [u8; 3],
    executes: bool,
) -> Option<()> {
    if op == b'=' {
        for slot in 0..3 {
            if who[slot] {
                state[slot] = 0;
            }
        }
    }
    // `=` has already cleared, so every remaining permission is an addition.
    let adding = op != b'-';
    for &perm in perms {
        let bits = match perm {
            b'r' => 0o4,
            b'w' => 0o2,
            b'x' => 0o1,
            // `X` adds execute only where some class may already execute, and
            // never removes it: `a-X` is a no-op in the reference shell.
            b'X' => {
                if executes && op != b'-' {
                    0o1
                } else {
                    0
                }
            }
            b's' | b't' => 0,
            b'u' | b'g' | b'o' => {
                // Permissions copy from the value the invocation started
                // with, so `a=,a=u` copies the pre-`a=` user bits.
                let source = match perm {
                    b'u' => initial[0],
                    b'g' => initial[1],
                    _ => initial[2],
                };
                for slot in 0..3 {
                    if who[slot] {
                        if adding {
                            state[slot] |= source;
                        } else {
                            state[slot] &= !source;
                        }
                    }
                }
                continue;
            }
            _ => return None,
        };
        for slot in 0..3 {
            if !who[slot] {
                continue;
            }
            if adding {
                state[slot] |= bits;
            } else {
                state[slot] &= !bits;
            }
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::{parse_symbolic, symbolic_form};

    fn mask(mode: &str) -> u32 {
        // The starting mask is part of each case: `u-r` means "allowed bits
        // minus user read", so the expected result depends on it.
        parse_symbolic(mode.as_bytes(), 0o124).expect("mode parses")
    }

    #[test]
    fn symbolic_edits_the_allowed_bits() {
        assert_eq!(mask("u-r"), 0o524);
        assert_eq!(mask("a=r,g=w,o=x"), 0o356);
        assert_eq!(mask("a=u"), 0o111);
        assert_eq!(mask("a=o"), 0o444);
        assert_eq!(mask("g-w,o-w"), 0o126);
    }

    #[test]
    fn a_copy_uses_the_value_the_invocation_started_with() {
        assert_eq!(parse_symbolic(b"a=,a=u", 0o124), Some(0o111));
        assert_eq!(parse_symbolic(b"a=", 0o124), Some(0o777));
    }

    #[test]
    fn empty_permissions_clear_the_named_classes() {
        assert_eq!(parse_symbolic(b"=", 0o124), Some(0o777));
        assert_eq!(parse_symbolic(b"u=", 0o124), Some(0o724));
        assert_eq!(parse_symbolic(b"+", 0o124), Some(0o124));
    }

    #[test]
    fn sequential_actions_apply_left_to_right() {
        assert_eq!(parse_symbolic(b"u+r+w+x", 0o124), Some(0o024));
        assert_eq!(parse_symbolic(b"a+x+wr-r", 0o124), Some(0o444));
        assert_eq!(parse_symbolic(b"=+rwx+rx", 0o124), Some(0o000));
    }

    #[test]
    fn execute_conditional_follows_the_mask() {
        // A mask with an execute bit makes `X` add execute; one without makes
        // it a no-op.
        assert_eq!(parse_symbolic(b"a=X", 0o124), Some(0o666));
        assert_eq!(parse_symbolic(b"a=X", 0o246), Some(0o777));
        assert_eq!(parse_symbolic(b"a-X", 0o246), Some(0o246));
    }

    #[test]
    fn s_and_t_are_accepted_and_contribute_nothing() {
        assert_eq!(parse_symbolic(b"a=s", 0o124), Some(0o777));
        assert_eq!(parse_symbolic(b"a+t", 0o124), Some(0o124));
    }

    #[test]
    fn malformed_modes_are_rejected() {
        for text in ["", " ", "089", "b=rwx", "u", "u+r,,u-r", "u+Z"] {
            assert_eq!(parse_symbolic(text.as_bytes(), 0o124), None, "{text:?}");
        }
    }

    #[test]
    fn symbolic_form_spells_all_three_classes() {
        assert_eq!(symbolic_form(0o022), "u=rwx,g=rx,o=rx");
        assert_eq!(symbolic_form(0o077), "u=rwx,g=,o=");
        assert_eq!(symbolic_form(0o000), "u=rwx,g=rwx,o=rwx");
    }
}
