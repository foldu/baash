//! Rendering of variables for `declare -p` (and the listing forms of
//! `readonly`/`export`/`local`).
//!
//! The output must be *re-input safe*: feeding it back to `eval` has to restore
//! the same value and attributes, including array shape and holes. It is also
//! compared byte for byte by the conformance suite, which pins the layout
//! (bare scalars, compact indexed arrays, explicit subscripts for holes,
//! single-quoted associative keys).
//!
//! Verified against the suite's expectations for bash 5.3.20 and OSH:
//!
//! ```text
//! declare -- x=1                     scalar, no attributes, bare-safe value
//! declare -r x="a b"                 scalar that needs quoting
//! declare -n ref=target              nameref: the reference text, not the target
//! declare -a a=(1 2 3)               contiguous indexed array
//! declare -a a=([0]=x [2]=y)         indexed array with a hole
//! declare -A m=(['k']='v' ['a b']=2) associative: keys are always quoted
//! declare -- x                       declared but unset
//! ```
//!
//! Byte values that cannot appear literally use `$'…'`: control bytes as
//! `\u00XX` and bytes that are not valid UTF-8 as `\xXX`, which is the form
//! OSH produces and bash can read back.

use crate::vars::{Var, VarValue};

/// Render one `declare -… name=value` line, including the trailing newline.
pub fn render_declaration(out: &mut Vec<u8>, name: &[u8], var: &Var) {
    out.extend_from_slice(b"declare ");
    render_flags(out, var);
    out.push(b' ');
    out.extend_from_slice(name);
    render_value(out, &var.value);
    out.push(b'\n');
}

/// Render `name=value` (the bare `declare`/`local` listing form).
pub fn render_assignment(out: &mut Vec<u8>, name: &[u8], var: &Var) {
    out.extend_from_slice(name);
    render_value(out, &var.value);
    out.push(b'\n');
}

/// The attribute letters, or `--` when there are none.
///
/// Order is the one bash uses and the suite greps for: `n`ameref, `r`eadonly,
/// e`x`ported, `i`nteger, then the array shape.
pub fn render_flags(out: &mut Vec<u8>, var: &Var) {
    out.push(b'-');
    let mut any = false;
    if var.attrs.nameref {
        out.push(b'n');
        any = true;
    }
    if var.attrs.readonly {
        out.push(b'r');
        any = true;
    }
    if var.attrs.exported {
        out.push(b'x');
        any = true;
    }
    if var.attrs.integer {
        out.push(b'i');
        any = true;
    }
    match var.value {
        VarValue::Indexed(_) => {
            out.push(b'a');
            any = true;
        }
        VarValue::Assoc(_) => {
            out.push(b'A');
            any = true;
        }
        _ => {}
    }
    if !any {
        out.push(b'-');
    }
}

fn render_value(out: &mut Vec<u8>, value: &VarValue) {
    match value {
        VarValue::Unset => {}
        VarValue::Str(bytes) => {
            out.push(b'=');
            quote_value(out, bytes);
        }
        VarValue::Indexed(elements) => {
            out.extend_from_slice(b"=(");
            let contiguous = elements
                .keys()
                .enumerate()
                .all(|(position, index)| *index == position as i64);
            for (position, (index, element)) in elements.iter().enumerate() {
                if position > 0 {
                    out.push(b' ');
                }
                if !contiguous {
                    out.push(b'[');
                    out.extend_from_slice(index.to_string().as_bytes());
                    out.extend_from_slice(b"]=");
                }
                quote_value(out, element);
            }
            out.push(b')');
        }
        VarValue::Assoc(pairs) => {
            out.extend_from_slice(b"=(");
            // Associative arrays have no stable iteration order in bash (it is
            // the hash table's). Sorting by key makes the output deterministic
            // and matches the order the suite pins.
            let mut sorted: Vec<&(Vec<u8>, Vec<u8>)> = pairs.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            for (position, (key, value)) in sorted.into_iter().enumerate() {
                if position > 0 {
                    out.push(b' ');
                }
                out.push(b'[');
                quote_forced(out, key);
                out.extend_from_slice(b"]=");
                quote_value(out, value);
            }
            out.push(b')');
        }
    }
}

/// Quote a value only when it needs it: bare when every byte is safe, single
/// quotes for printable text, `$'…'` when a byte cannot be written literally.
pub fn quote_value(out: &mut Vec<u8>, bytes: &[u8]) {
    if is_bare_word(bytes) || bytes.is_empty() {
        out.extend_from_slice(bytes);
    } else if needs_dollar_quote(bytes) {
        quote_dollar(out, bytes);
    } else {
        quote_single(out, bytes);
    }
}

/// Quote `bytes` even when it would be bare-safe.
///
/// Associative keys are always quoted because an unquoted key that happens to
/// look like an arithmetic expression (`1+1`) would be re-read as one.
pub fn quote_forced(out: &mut Vec<u8>, bytes: &[u8]) {
    if needs_dollar_quote(bytes) {
        quote_dollar(out, bytes);
    } else {
        quote_single(out, bytes);
    }
}

/// Single-quote `bytes`, escaping embedded single quotes as `'\''`.
pub fn quote_single(out: &mut Vec<u8>, bytes: &[u8]) {
    out.push(b'\'');
    for &byte in bytes {
        if byte == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(byte);
        }
    }
    out.push(b'\'');
}

/// Bytes that need no quoting in `declare -p` output.
fn is_bare_word(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && bytes.iter().all(|&byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'_' | b'-' | b'.' | b'/' | b':' | b'@' | b'%' | b'+' | b',' | b'='
                )
        })
}

/// Whether `bytes` needs `$'…'` rather than single quotes: a control byte, or
/// a byte that does not form valid UTF-8 (which no terminal-safe literal can
/// spell).
fn needs_dollar_quote(bytes: &[u8]) -> bool {
    if bytes.iter().any(|&byte| byte < 0x20 || byte == 0x7f) {
        return true;
    }
    std::str::from_utf8(bytes).is_err()
}

/// `$'…'` with `\u00XX` for control bytes and `\xXX` for bytes that are not
/// valid UTF-8, matching OSH's output.
fn quote_dollar(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(b"$'");
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match byte {
            0x00..=0x1f | 0x7f => {
                out.extend_from_slice(format!("\\u{byte:04x}").as_bytes());
                index += 1;
            }
            b'\\' => {
                out.extend_from_slice(b"\\\\");
                index += 1;
            }
            b'\'' => {
                out.extend_from_slice(b"\\'");
                index += 1;
            }
            0x20..=0x7e => {
                out.push(byte);
                index += 1;
            }
            _ => {
                // Valid multi-byte UTF-8 sequences are written as themselves;
                // isolated invalid bytes become `\xXX`.
                match utf8_char_len(&bytes[index..]) {
                    Some(length) => {
                        out.extend_from_slice(&bytes[index..index + length]);
                        index += length;
                    }
                    None => {
                        out.extend_from_slice(format!("\\x{byte:02x}").as_bytes());
                        index += 1;
                    }
                }
            }
        }
    }
    out.push(b'\'');
}

/// The byte length of the valid, non-ASCII UTF-8 character at the start of
/// `bytes`, if there is one.
fn utf8_char_len(bytes: &[u8]) -> Option<usize> {
    let first = *bytes.first()?;
    let length = match first {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    };
    if bytes.len() < length {
        return None;
    }
    std::str::from_utf8(&bytes[..length]).ok().map(|_| length)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vars::Attrs;
    use std::collections::BTreeMap;

    fn render(var: &Var, name: &[u8]) -> String {
        let mut out = Vec::new();
        render_declaration(&mut out, name, var);
        String::from_utf8(out).expect("ascii")
    }

    #[test]
    fn scalar_without_attributes_is_double_dash() {
        let var = Var {
            value: VarValue::Str(b"111".to_vec()),
            attrs: Attrs::default(),
        };
        assert_eq!(render(&var, b"x"), "declare -- x=111\n");
    }

    #[test]
    fn scalar_that_needs_quoting_uses_single_quotes() {
        let var = Var {
            value: VarValue::Str(b"foo bar".to_vec()),
            attrs: Attrs::default(),
        };
        assert_eq!(render(&var, b"x"), "declare -- x='foo bar'\n");
    }

    #[test]
    fn control_bytes_use_dollar_quoting() {
        let var = Var {
            value: VarValue::Str(vec![0x1f]),
            attrs: Attrs::default(),
        };
        assert_eq!(render(&var, b"x"), "declare -- x=$'\\u001f'\n");
        let var = Var {
            value: VarValue::Str(vec![0xfe, 0xff]),
            attrs: Attrs::default(),
        };
        assert_eq!(render(&var, b"x"), "declare -- x=$'\\xfe\\xff'\n");
    }

    #[test]
    fn contiguous_indexed_array_is_compact() {
        let var = Var {
            value: VarValue::Indexed(BTreeMap::from([
                (0, b"1".to_vec()),
                (1, b"2".to_vec()),
                (2, b"3".to_vec()),
            ])),
            attrs: Attrs::default(),
        };
        assert_eq!(render(&var, b"a"), "declare -a a=(1 2 3)\n");
    }

    #[test]
    fn sparse_indexed_array_prints_subscripts() {
        let var = Var {
            value: VarValue::Indexed(BTreeMap::from([(3, b"".to_vec()), (4, b"foo".to_vec())])),
            attrs: Attrs::default(),
        };
        assert_eq!(render(&var, b"a"), "declare -a a=([3]= [4]=foo)\n");
    }

    #[test]
    fn assoc_keys_are_always_quoted() {
        let var = Var {
            value: VarValue::Assoc(vec![
                (b"a".to_vec(), b"1".to_vec()),
                (b"b".to_vec(), b"2".to_vec()),
            ]),
            attrs: Attrs::default(),
        };
        assert_eq!(render(&var, b"m"), "declare -A m=(['a']=1 ['b']=2)\n");
        let control = Var {
            value: VarValue::Assoc(vec![
                (b"foo".to_vec(), b"foo bar".to_vec()),
                (vec![0x1f], vec![0xfe, 0xff]),
            ]),
            attrs: Attrs::default(),
        };
        assert_eq!(
            render(&control, b"A"),
            "declare -A A=([$'\\u001f']=$'\\xfe\\xff' ['foo']='foo bar')\n"
        );
        let empty = Var {
            value: VarValue::Assoc(Vec::new()),
            attrs: Attrs::default(),
        };
        assert_eq!(render(&empty, b"m"), "declare -A m=()\n");
    }

    #[test]
    fn nameref_renders_its_reference_text() {
        let var = Var {
            value: VarValue::Str(b"target".to_vec()),
            attrs: Attrs {
                nameref: true,
                ..Attrs::default()
            },
        };
        assert_eq!(render(&var, b"ref"), "declare -n ref=target\n");
    }

    #[test]
    fn unset_prints_no_value() {
        let var = Var {
            value: VarValue::Unset,
            attrs: Attrs::default(),
        };
        assert_eq!(render(&var, b"x"), "declare -- x\n");
    }

    #[test]
    fn attributes_render_in_bash_order() {
        let var = Var {
            value: VarValue::Str(b"42".to_vec()),
            attrs: Attrs {
                exported: true,
                readonly: true,
                ..Attrs::default()
            },
        };
        assert_eq!(render(&var, b"x"), "declare -rx x=42\n");
    }
}
