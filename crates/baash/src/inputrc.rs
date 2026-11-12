//! The one readline setting the shell reads: `completion-ignore-case`.
//!
//! baash has no `bind` and no inputrc parser (`crates/baash-repl/src/lib.rs`
//! says so), but `~/.inputrc` is the only place a bash user states that
//! completion should match case-insensitively, and bash's own `compgen`
//! honours it *inside a completion*. The difference is not cosmetic: with
//! case-sensitive matching `mpv Me` matches neither `MEGALOVANIA …` nor
//! `measure.py`, and a completion function that falls back to
//! `COMPREPLY=($(compgen -f))` on an empty `_filedir` then offers the whole
//! directory. Reading this one variable is the difference between that and a
//! working completion, so it is an exception rather than the first line of an
//! inputrc implementation.
//!
//! Conditionals (`$if …`) are not evaluated: a line that sets the variable
//! counts wherever it appears. That is wrong for a file that sets it
//! differently per mode, and right for the common case.

use std::path::PathBuf;

/// Whether the user's inputrc asks for case-insensitive completion.
pub fn completion_ignore_case() -> bool {
    let Some(path) = path() else {
        return false;
    };
    match std::fs::read(path) {
        Ok(bytes) => parse(&bytes),
        Err(_) => false,
    }
}

/// The file bash would read: `$INPUTRC`, else `~/.inputrc`.
fn path() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("INPUTRC") {
        return Some(PathBuf::from(explicit));
    }
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".inputrc"))
}

/// The value the file sets. The last assignment wins, as it does in readline.
fn parse(bytes: &[u8]) -> bool {
    let mut ignore = false;
    for line in bytes.split(|&byte| byte == b'\n') {
        let line = trim(line);
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        let mut fields = line
            .split(|&byte| byte == b' ' || byte == b'\t')
            .filter(|field| !field.is_empty());
        let Some(keyword) = fields.next() else {
            continue;
        };
        if !keyword.eq_ignore_ascii_case(b"set") {
            continue;
        }
        let Some(name) = fields.next() else {
            continue;
        };
        if !name.eq_ignore_ascii_case(b"completion-ignore-case") {
            continue;
        }
        if let Some(value) = fields.next() {
            ignore = value.eq_ignore_ascii_case(b"on") || value == b"1";
        }
    }
    ignore
}

/// `bytes` without leading and trailing blanks.
fn trim(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t' | b'\r'))
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !matches!(byte, b' ' | b'\t' | b'\r'))
        .map(|last| last + 1)
        .unwrap_or(start);
    &bytes[start..end.max(start)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_line_turns_it_on() {
        assert!(parse(b"set completion-ignore-case on\n"));
        assert!(parse(b"set completion-ignore-case 1\n"));
        assert!(parse(b"SET completion-ignore-case ON\n"));
    }

    #[test]
    fn the_last_assignment_wins() {
        assert!(!parse(
            b"set completion-ignore-case on\nset completion-ignore-case off\n"
        ));
        assert!(parse(
            b"set completion-ignore-case off\nset completion-ignore-case on\n"
        ));
    }

    #[test]
    fn other_lines_are_ignored() {
        assert!(!parse(
            b"# completion-ignore-case on\nC-n: history-search-forward\nset show-all-if-ambiguous on\n"
        ));
        assert!(parse(b"  set   completion-ignore-case   on  \r\n"));
    }

    #[test]
    fn a_file_without_the_line_is_off() {
        assert!(!parse(b""));
        assert!(!parse(b"set completion-ignore-case\n"));
    }
}
