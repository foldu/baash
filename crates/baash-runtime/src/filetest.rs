//! The one-operand file questions `test`/`[` and `[[ … ]]` share.
//!
//! `-O`, `-G`, `-N`, the set-id and sticky bits, the special file types and
//! `-t` are asked by both commands, and bash answers them identically in both.
//! The two commands are parsed by different code — the built-in in
//! `builtins/test.rs`, the conditional in `baash_vm::tree::cond` — so a
//! predicate spelled twice drifts, and this one had: `test -O file` answered
//! while `[[ -O file ]]` was a *parse* error (`BAASH-E0204`), because the
//! conditional's operator table lists `-O` and its evaluator had no arm for it.
//! The other six failed the same way; `-R` failed one step earlier, with
//! "unexpected token", because the table did not even list it. The predicates
//! live here once, beside [`crate::filecmp`] (the three that compare a *pair*
//! of paths) and for the same reason.
//!
//! A question the platform cannot answer is false, never a diagnostic: a system
//! without uids has no owner to compare, and a system without inodes has no
//! device to compare (`docs/portability.md` §9). A path that does not exist
//! answers every question here false, which is what bash does, and is not an
//! error — `[[ -k missing ]]` is a false test, not a failed command.

/// The file types only Unix has.
///
/// Without Unix file types the answer is "no", not a guess: Windows has no
/// FIFOs, sockets in the filesystem namespace or device nodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `-S`
    Socket,
    /// `-c`
    CharDevice,
    /// `-b`
    BlockDevice,
    /// `-p`
    Fifo,
}

/// `-S`, `-c`, `-b`, `-p`: whether `path` is one of those, following a symlink.
///
/// The four follow a link, like every file test other than `-h`/`-L`: bash
/// answers `-p` true for a symlink to a FIFO. The conditional command used
/// `symlink_metadata` for these and so answered false, which the shared
/// predicate fixes by asking the same question the built-in asks.
#[cfg(unix)]
pub fn is_kind(path: &[u8], wanted: Kind) -> bool {
    use std::os::unix::fs::FileTypeExt;

    let Some(metadata) = metadata(path) else {
        return false;
    };
    let file_type = metadata.file_type();
    match wanted {
        Kind::Socket => file_type.is_socket(),
        Kind::CharDevice => file_type.is_char_device(),
        Kind::BlockDevice => file_type.is_block_device(),
        Kind::Fifo => file_type.is_fifo(),
    }
}

/// Without Unix file types the answer is "no", not a guess.
#[cfg(not(unix))]
pub fn is_kind(_path: &[u8], _wanted: Kind) -> bool {
    false
}

/// `-O`: the file's owner is this (effective) user.
pub fn owned_by_user(path: &[u8]) -> bool {
    let path = baash_sys::os::path_from_bytes(path);
    match (baash_sys::os::own_uid(), baash_sys::os::owner_uid(&path)) {
        (Some(ours), Some(theirs)) => ours == theirs,
        _ => false,
    }
}

/// `-G`: the file's group is this (effective) group.
pub fn owned_by_group(path: &[u8]) -> bool {
    let path = baash_sys::os::path_from_bytes(path);
    match (baash_sys::os::own_gid(), baash_sys::os::owner_gid(&path)) {
        (Some(ours), Some(theirs)) => ours == theirs,
        _ => false,
    }
}

/// `-N`: the file was modified after it was last read.
pub fn modified_later_than_read(path: &[u8]) -> bool {
    let Some(metadata) = metadata(path) else {
        return false;
    };
    match (metadata.modified(), metadata.accessed()) {
        (Ok(modified), Ok(accessed)) => modified > accessed,
        _ => false,
    }
}

/// The permission bits, which `-u`/`-g`/`-k` ask about.
///
/// The three want one bit each (`0o4000`, `0o2000`, `0o1000`), so this returns
/// the whole mode and lets the caller mask it — one spelling, three tests.
#[cfg(unix)]
pub fn permission_bits(path: &[u8]) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;

    metadata(path).map(|metadata| metadata.permissions().mode())
}

/// There are no set-id or sticky bits to inspect off Unix.
#[cfg(not(unix))]
pub fn permission_bits(_path: &[u8]) -> Option<u32> {
    None
}

/// `-t`: whether `descriptor` is a terminal.
///
/// The conversion is the delicate part: a descriptor is an `int` in every API
/// underneath, and a number too large to be one is not a terminal rather than a
/// wrap-around. `[[ -t 999 ]]` is false on both commands, and `[[ -t -1 ]]`
/// too, which is why this is here rather than at each call site.
pub fn is_tty(descriptor: i64) -> bool {
    i32::try_from(descriptor).is_ok_and(baash_sys::is_tty)
}

/// bash's `legal_number`: base-10 `strtoimax`, surrounding blanks allowed.
///
/// It is here because `-t` is the operator that takes a *number* rather than a
/// path, and both commands read that number the same way: `test -t 1` and
/// `[[ -t 1 ]]`. `[[ -t 1+1 ]]` is an "integer expected" diagnostic in bash,
/// so the conditional command must not reach for its own arithmetic parser
/// (which `-eq` does use) — the two are different readers, and this is the one
/// `-t` wants. `test -v 0` reads its positional-parameter operand the same
/// way, which is the second caller.
pub fn legal_number(text: &[u8]) -> Option<i64> {
    let mut at = 0;
    while let Some(b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') = text.get(at).copied() {
        at += 1;
    }
    let mut negative = false;
    if let Some(byte @ (b'+' | b'-')) = text.get(at).copied() {
        negative = byte == b'-';
        at += 1;
    }
    let digits = at;
    let mut magnitude: u128 = 0;
    while let Some(byte @ b'0'..=b'9') = text.get(at).copied() {
        magnitude = magnitude
            .saturating_mul(10)
            .saturating_add(u128::from(byte - b'0'));
        at += 1;
    }
    if at == digits {
        return None;
    }
    // `strtoimax` reports ERANGE, which `legal_number` turns into "not a
    // number" rather than a clamp.
    let magnitude = if negative {
        (magnitude as i128).wrapping_neg()
    } else {
        magnitude as i128
    };
    let value = i64::try_from(magnitude).ok()?;
    while let Some(b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') = text.get(at).copied() {
        at += 1;
    }
    (at == text.len()).then_some(value)
}

fn metadata(path: &[u8]) -> Option<std::fs::Metadata> {
    std::fs::metadata(baash_sys::os::path_from_bytes(path)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legal_number_takes_a_sign_and_trailing_blanks() {
        assert_eq!(legal_number(b"0"), Some(0));
        assert_eq!(legal_number(b"12"), Some(12));
        assert_eq!(legal_number(b"+7"), Some(7));
        assert_eq!(legal_number(b"-2"), Some(-2));
        assert_eq!(legal_number(b" 3 "), Some(3));
        assert_eq!(legal_number(b"0+1"), None);
        assert_eq!(legal_number(b""), None);
        assert_eq!(legal_number(b"-"), None);
        assert_eq!(legal_number(b"1 2"), None);
        // Wider than i64: overflow is not a number, rather than a wraparound.
        assert_eq!(legal_number(b"99999999999999999999"), None);
        assert_eq!(legal_number(b"9223372036854775807"), Some(i64::MAX));
        // The negative end is one step further out, and `strtoimax` reaches it.
        assert_eq!(legal_number(b"-9223372036854775808"), Some(i64::MIN));
        assert_eq!(legal_number(b"-9223372036854775809"), None);
    }

    #[test]
    fn is_tty_maps_a_descriptor_only_when_it_fits() {
        // Choices only: which descriptors are terminals depends on the run.
        assert!(!is_tty(999));
        assert!(!is_tty(-1));
        assert!(!is_tty(i64::MAX));
        assert!(!is_tty(i64::MIN));
    }

    #[test]
    fn a_missing_path_answers_false() {
        let missing = b"/nonexistent/baash-filetest";
        assert!(!owned_by_user(missing));
        assert!(!owned_by_group(missing));
        assert!(!modified_later_than_read(missing));
        assert_eq!(permission_bits(missing), None);
        assert!(!is_kind(missing, Kind::Fifo));
    }

    #[test]
    fn a_file_this_user_created_is_owned_by_it() {
        let root = std::env::temp_dir().join(format!("baash-filetest-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("a directory to write into");
        let file = root.join("owned");
        std::fs::write(&file, b"x").expect("a file to look at");
        let path = baash_sys::os::bytes_from_os(&file);

        // `own_uid`/`own_gid` are None where the platform has no owner, and the
        // predicates then answer false; skip rather than assert a platform fact.
        if baash_sys::os::own_uid().is_some() {
            assert!(owned_by_user(&path));
        }
        if baash_sys::os::own_gid().is_some() {
            assert!(owned_by_group(&path));
        }
        assert_eq!(permission_bits(&path).is_some(), cfg!(unix));
        let _ = std::fs::remove_dir_all(&root);
    }
}
