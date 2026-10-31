//! `set -x`: tracing commands after expansion.
//!
//! The format follows `osh` because `spec/oils/xtrace.test.sh` records its
//! output as the primary expectation: words are quoted with single quotes when
//! they contain spaces or shell metacharacters, and with `$'…'` (using `\n`,
//! `\t`, `\'`, `\\` and `\uXXXX`) when they contain a control byte, a single
//! quote or a backslash. bash quotes differently, so matching bash here would
//! trade a primary pass for an `## OK` one.
//!
//! `PS4` is read at trace time and used as-is (it is not expanded): a
//! function that sets `local PS4='- '` gets its own prefix, and an unset
//! `PS4` traces with no prefix at all — both of which the suite pins.

use bsh_runtime::ShellHost;

use super::Interp;

/// Print one `PS4`-prefixed trace line for `words`.
///
/// A word listed in `unquoted` is one of the parser's array-literal
/// placeholders: its bytes are source text (`a+=(2)`), not an expanded word,
/// so quoting it would print something the user never typed.
pub(crate) fn trace(interp: &mut Interp, words: &[Vec<u8>], unquoted: &[Vec<u8>]) {
    let mut line = prefix(interp);
    for (index, word) in words.iter().enumerate() {
        if index > 0 {
            line.push(b' ');
        }
        if unquoted.iter().any(|raw| raw == word) {
            line.extend_from_slice(word);
        } else {
            line.extend_from_slice(&quote(word));
        }
    }
    line.push(b'\n');
    let _ = interp.write_err(&line);
}

/// `PS4`, with the parameter expansions bash applies to it.
///
/// Only the special parameters and plain names are handled: `$?` and a
/// `PS4='+$x:'` are what scripts actually use, and a command substitution in
/// `PS4` would run a command in the middle of tracing (bash forbids that in
/// some contexts too). An unset `PS4` has no prefix at all.
fn prefix(interp: &Interp) -> Vec<u8> {
    let Some(template) = interp.state.vars.scalar(b"PS4") else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(template.len());
    let mut position = 0;
    while position < template.len() {
        if template[position] != b'$' {
            out.push(template[position]);
            position += 1;
            continue;
        }
        let rest = &template[position + 1..];
        let (value, width) = match rest.first() {
            Some(b'?') => (Some(interp.specials.status.clone()), 2),
            Some(b'-') => (Some(interp.specials.dash.clone()), 2),
            Some(b'$') => (Some(interp.specials.pid.clone()), 2),
            Some(b'!') => (Some(interp.specials.bang.clone()), 2),
            Some(b'{') => match rest.iter().position(|&byte| byte == b'}') {
                Some(close) => {
                    let name = &rest[1..close];
                    (
                        interp.state.vars.scalar(name).map(<[u8]>::to_vec),
                        close + 2,
                    )
                }
                None => (None, 1),
            },
            Some(&first) if first.is_ascii_alphabetic() || first == b'_' => {
                let end = rest
                    .iter()
                    .position(|&byte| !(byte.is_ascii_alphanumeric() || byte == b'_'))
                    .unwrap_or(rest.len());
                (
                    interp.state.vars.scalar(&rest[..end]).map(<[u8]>::to_vec),
                    end + 1,
                )
            }
            _ => (None, 1),
        };
        if let Some(value) = value {
            out.extend_from_slice(&value);
            position += width;
        } else {
            // Not an expansion this tracer understands: keep the text.
            out.push(b'$');
            position += 1;
        }
    }
    out
}

/// Whether `byte` can appear in an unquoted trace word.
fn safe(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'-' | b'_' | b'.' | b'/' | b':' | b'=' | b'@' | b'%' | b'+' | b',' | b'^'
        )
}

/// Quote one expanded word the way the reference shell does.
pub(crate) fn quote(word: &[u8]) -> Vec<u8> {
    if !word.is_empty() && word.iter().all(|&byte| safe(byte)) {
        return word.to_vec();
    }
    let escape_with_dollars = word
        .iter()
        .any(|&byte| byte < b' ' || byte == 0x7f || byte == b'\'' || byte == b'\\');
    let mut out = Vec::with_capacity(word.len() + 4);
    if escape_with_dollars {
        out.extend_from_slice(b"$'");
        for &byte in word {
            match byte {
                b'\'' => out.extend_from_slice(b"\\'"),
                b'\\' => out.extend_from_slice(b"\\\\"),
                b'\n' => out.extend_from_slice(b"\\n"),
                b'\t' => out.extend_from_slice(b"\\t"),
                b'\r' => out.extend_from_slice(b"\\r"),
                byte if byte < b' ' || byte == 0x7f => {
                    out.extend_from_slice(format!("\\u{byte:04x}").as_bytes());
                }
                byte => out.push(byte),
            }
        }
        out.push(b'\'');
    } else {
        out.push(b'\'');
        out.extend_from_slice(word);
        out.push(b'\'');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::quote;

    #[test]
    fn safe_words_stay_bare() {
        assert_eq!(quote(b"echo"), b"echo");
        assert_eq!(quote(b"my-dir/my_file.cc"), b"my-dir/my_file.cc");
        assert_eq!(quote(b"x=3"), b"x=3");
    }

    #[test]
    fn spaces_and_quotes_are_single_quoted() {
        assert_eq!(quote(b"1 2"), b"'1 2'");
        assert_eq!(quote(b"\""), b"'\"'");
        assert_eq!(quote("[μ]".as_bytes()), "'[μ]'".as_bytes());
        assert_eq!(quote(b"PS4=- "), b"'PS4=- '");
    }

    #[test]
    fn control_bytes_and_backslashes_use_dollar_quotes() {
        assert_eq!(quote(b"a\x03b\x04c\0d"), b"$'a\\u0003b\\u0004c\\u0000d'");
        assert_eq!(quote(b"[\t]"), b"$'[\\t]'");
        assert_eq!(quote(b"[\n]"), b"$'[\\n]'");
        assert_eq!(quote(b"'"), b"$'\\''");
        assert_eq!(quote(b"\\"), b"$'\\\\'");
    }
}
