//! The quoting bridge between `bsh-lang`'s fields and `bsh-pattern`.
//!
//! `bsh-pattern` deliberately has no quote-mask API: a quoted metacharacter
//! reaches the matcher as a backslash escape, and a quoted backslash as `\\`.
//! `bsh-lang` already converts the words of `${x#pat}`/`${x/pat/rep}` this way;
//! `bsh-vm` sees an already-expanded [`Field`] with its per-byte quoting mask,
//! so the same conversion is a per-byte escape here. One function serves every
//! consumer in this crate, so `case`, `[[ … == … ]]` and pathname expansion
//! cannot disagree about what quoting means.

use bsh_lang::Field;
use bsh_pattern::Flags;

/// The pattern bytes `bsh-pattern` expects: every quoted byte preceded by a
/// backslash, so a quoted `*` is literal and a quoted `\` becomes `\\`.
pub(crate) fn escaped(bytes: &[u8], quoted: &[bool]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for (&byte, &quoted) in bytes.iter().zip(quoted) {
        if quoted {
            out.push(b'\\');
        }
        out.push(byte);
    }
    out
}

/// Match `subject` against a pattern operand, anchored at both ends.
///
/// The flags come from the caller: `case` and `[[ … == … ]]` disagree about
/// `extglob` (the conditional forces it on) but not about anything else.
pub(crate) fn matches(pattern: &Field, subject: &[u8], flags: Flags) -> bool {
    bsh_pattern::matches(&escaped(&pattern.bytes, &pattern.quoted), subject, flags)
}

/// The flags `[[ … == … ]]` matches with: bash uses extglob semantics for the
/// right-hand side even when `shopt -u extglob`, while every other switch
/// (`nocasematch`) still comes from the shell options.
pub(crate) fn conditional_flags(flags: Flags) -> Flags {
    Flags {
        extglob: true,
        ..flags
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(bytes: &[u8], quoted: &[bool]) -> Field {
        Field::new(bytes.to_vec(), quoted.to_vec())
    }

    #[test]
    fn quoted_metacharacters_are_literal() {
        let pattern = field(b"a*b", &[false, true, false]);
        assert!(matches(&pattern, b"a*b", Flags::default()));
        assert!(!matches(&pattern, b"axb", Flags::default()));
    }

    #[test]
    fn quoted_backslash_survives_as_a_literal() {
        let pattern = field(b"a\\b", &[false, true, false]);
        assert!(matches(&pattern, b"a\\b", Flags::default()));
        assert!(!matches(&pattern, b"ab", Flags::default()));
    }

    #[test]
    fn unquoted_metacharacters_stay_active() {
        let pattern = field(b"a*b", &[false; 3]);
        assert!(matches(&pattern, b"axxb", Flags::default()));
    }

    #[test]
    fn conditional_forces_extglob() {
        let pattern = field(b"@(cc|dd)", &[false; 8]);
        assert!(matches(
            &pattern,
            b"cc",
            conditional_flags(Flags::default())
        ));
        assert!(!matches(&pattern, b"cc", Flags::default()));
    }
}
