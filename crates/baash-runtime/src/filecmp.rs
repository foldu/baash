//! `-nt`, `-ot` and `-ef`: comparing two paths.
//!
//! `test`/`[` and `[[ … ]]` answer the same three questions about a pair of
//! paths, and bash answers them the same way in both. The predicates live here
//! once, so the built-in and the conditional command cannot drift apart.
//!
//! The answers are bash's, which are not the obvious ones. `-nt` is "left is
//! newer than right, **or right does not exist**", and a missing *left* is
//! false rather than an error: `[ a -nt b ]` with neither file present is a
//! false comparison, not a diagnostic. `-ef` is device and inode, so two names
//! for one file compare equal and a hard link is indistinguishable from its
//! target. A platform without inodes cannot answer `-ef` at all, and says false
//! rather than guessing (docs/portability.md §6).

use std::time::SystemTime;

/// `-nt`: `left` exists and `right` does not, or is newer.
pub fn newer_than(left: &[u8], right: &[u8]) -> bool {
    match (modification_time(left), modification_time(right)) {
        (Some(left), Some(right)) => left > right,
        (Some(_), None) => true,
        _ => false,
    }
}

/// `-ot`: `left` does not exist and `right` does, or is older.
pub fn older_than(left: &[u8], right: &[u8]) -> bool {
    match (modification_time(left), modification_time(right)) {
        (Some(left), Some(right)) => left < right,
        (None, Some(_)) => true,
        _ => false,
    }
}

/// `-ef`: the same file, by device and inode.
#[cfg(unix)]
pub fn same_file(left: &[u8], right: &[u8]) -> bool {
    use std::os::unix::fs::MetadataExt;

    match (metadata(left), metadata(right)) {
        (Some(left), Some(right)) => left.dev() == right.dev() && left.ino() == right.ino(),
        _ => false,
    }
}

/// `-ef` needs inodes; without them the question cannot be answered.
#[cfg(not(unix))]
pub fn same_file(_left: &[u8], _right: &[u8]) -> bool {
    false
}

fn metadata(argument: &[u8]) -> Option<std::fs::Metadata> {
    std::fs::metadata(baash_sys::os::path_from_bytes(argument)).ok()
}

fn modification_time(argument: &[u8]) -> Option<SystemTime> {
    metadata(argument)?.modified().ok()
}
