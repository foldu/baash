//! Test fixtures: a map-backed [`Shell`] and a small reference word parser.
//!
//! The parser is **not** the front end. It is a deliberately compact
//! implementation of the quoting and expansion syntax this crate promises to
//! round-trip through — `\x`, `'…'`, `"…"`, `$'…'`, `$x`, `${…}`, `$(…)`,
//! `$((…))` and backticks — so tests can write words the way a user would
//! and the expansion pipeline sees a realistic [`Word`].
//!
//! Only the syntax tests need is supported. Anything else parses as literal
//! bytes, which keeps the fixture honest: a test that passes here is testing
//! the pipeline, not a parser.

use std::collections::{BTreeMap, HashMap};

use crate::host::Shell;
use crate::vars::{ArrayValue, SetError, Vars};
use crate::word::{
    Arithmetic, CaseHow, CmdSubStyle, CommandSub, DoubleQ, Lit, ParamExp, ParamName, ParamOp, Part,
    ReplaceMode, TildeSpec, UnsupportedOp, Word,
};
use crate::{Error, ErrorKind, Field};

/// Test-shell associative storage: key/value pairs in insertion order.
type AssocMap = HashMap<Vec<u8>, Vec<(Vec<u8>, Vec<u8>)>>;

/// An in-memory shell: variables, positional parameters, and map-backed
/// command substitution, arithmetic, globbing and home directories.
#[derive(Clone, Debug, Default)]
pub struct TestShell {
    /// Variable values. Absence means unset; an empty vector means null.
    pub vars: HashMap<Vec<u8>, Vec<u8>>,
    /// Positional parameters, starting at `$1`.
    pub positional: Vec<Vec<u8>>,
    /// `$(src)` results.
    pub cmdsub: HashMap<Vec<u8>, Vec<u8>>,
    /// `$((src))` results.
    pub arith: HashMap<Vec<u8>, Vec<u8>>,
    /// Pattern → matches for pathname expansion.
    pub globs: HashMap<Vec<u8>, Vec<Vec<u8>>>,
    /// Login name → home directory.
    pub homes: HashMap<Vec<u8>, Vec<u8>>,
    /// Every command substitution that was actually evaluated.
    pub cmdsub_calls: Vec<Vec<u8>>,
    /// Every arithmetic expression that was actually evaluated.
    pub arith_calls: Vec<Vec<u8>>,
    /// Every pattern that reached pathname expansion.
    pub glob_calls: Vec<Vec<u8>>,
    /// Every assignment attempted through [`Vars::set`].
    pub set_calls: Vec<(Vec<u8>, Vec<u8>)>,
    /// Whether the fixture is in a UTF-8 locale (`${#x}` counts characters).
    pub utf8: bool,
    /// When true, [`Vars::set`] refuses with [`SetError::ReadOnly`].
    pub read_only: bool,
    /// `set -u`: expanding an unset parameter is an error.
    pub nounset: bool,
    /// Arrays, by name. Separate from `vars` so a name cannot be both.
    pub indexed: HashMap<Vec<u8>, BTreeMap<i64, Vec<u8>>>,
    /// Associative arrays, in insertion order.
    pub assoc: AssocMap,
}

impl TestShell {
    /// A shell with nothing set (IFS therefore reads as unset).
    pub fn new() -> Self {
        Self::default()
    }

    /// Set `IFS` to an explicit value.
    #[must_use]
    pub fn ifs(mut self, value: &[u8]) -> Self {
        self.vars.insert(b"IFS".to_vec(), value.to_vec());
        self
    }

    /// Set a variable.
    #[must_use]
    pub fn var(mut self, name: &[u8], value: &[u8]) -> Self {
        self.vars.insert(name.to_vec(), value.to_vec());
        self
    }

    /// Put the fixture in a UTF-8 locale.
    #[must_use]
    pub fn utf8(mut self, utf8: bool) -> Self {
        self.utf8 = utf8;
        self
    }

    /// Set the positional parameters.
    #[must_use]
    pub fn positionals(mut self, args: &[&[u8]]) -> Self {
        self.positional = args.iter().map(|arg| arg.to_vec()).collect();
        self
    }

    /// Provide a `$(src)` result.
    #[must_use]
    pub fn cmdsub(mut self, source: &[u8], output: &[u8]) -> Self {
        self.cmdsub.insert(source.to_vec(), output.to_vec());
        self
    }

    /// Provide an arithmetic result.
    #[must_use]
    pub fn arith(mut self, source: &[u8], output: &[u8]) -> Self {
        self.arith.insert(source.to_vec(), output.to_vec());
        self
    }

    /// Provide a glob result.
    #[must_use]
    pub fn glob(mut self, pattern: &[u8], matches: &[&[u8]]) -> Self {
        self.globs.insert(
            pattern.to_vec(),
            matches.iter().map(|m| m.to_vec()).collect(),
        );
        self
    }

    /// Provide a home directory for a login name (empty means `~`).
    #[must_use]
    pub fn home(mut self, user: &[u8], dir: &[u8]) -> Self {
        self.homes.insert(user.to_vec(), dir.to_vec());
        self
    }

    /// Turn `set -u` (nounset) on.
    #[must_use]
    pub fn nounset(mut self, on: bool) -> Self {
        self.nounset = on;
        self
    }

    /// Refuse assignments.
    #[must_use]
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    /// Give `name` an indexed array with these elements.
    #[must_use]
    pub fn indexed(mut self, name: &[u8], elements: &[(i64, &[u8])]) -> Self {
        self.indexed.insert(
            name.to_vec(),
            elements
                .iter()
                .map(|(index, value)| (*index, value.to_vec()))
                .collect(),
        );
        self
    }

    /// Give `name` an associative array with these keys, in this order.
    #[must_use]
    pub fn assoc(mut self, name: &[u8], pairs: &[(&[u8], &[u8])]) -> Self {
        self.assoc.insert(
            name.to_vec(),
            pairs
                .iter()
                .map(|(key, value)| (key.to_vec(), value.to_vec()))
                .collect(),
        );
        self
    }
}

impl Vars for TestShell {
    fn nounset(&self) -> bool {
        self.nounset
    }

    fn utf8(&self) -> bool {
        self.utf8
    }

    fn get(&self, name: &[u8]) -> Option<&[u8]> {
        self.vars.get(name).map(Vec::as_slice)
    }

    fn positional(&self) -> &[Vec<u8>] {
        &self.positional
    }

    fn set(&mut self, name: &[u8], value: Vec<u8>) -> Result<(), SetError> {
        if self.read_only {
            return Err(SetError::ReadOnly);
        }
        self.set_calls.push((name.to_vec(), value.clone()));
        self.vars.insert(name.to_vec(), value);
        Ok(())
    }

    fn array(&self, name: &[u8]) -> Option<ArrayValue<'_>> {
        if let Some(elements) = self.indexed.get(name) {
            return Some(ArrayValue::Indexed(elements));
        }
        self.assoc
            .get(name)
            .map(|pairs| ArrayValue::Assoc(pairs.as_slice()))
    }
}

impl Shell for TestShell {
    fn command_substitution(&mut self, cmd: &CommandSub) -> Result<Vec<u8>, Error> {
        self.cmdsub_calls.push(cmd.source.clone());
        self.cmdsub.get(&cmd.source).cloned().ok_or_else(|| {
            Error::new(
                ErrorKind::Host {
                    message: format!(
                        "no command substitution fixture for {:?}",
                        String::from_utf8_lossy(&cmd.source)
                    ),
                },
                0..0,
            )
        })
    }

    fn arithmetic(&mut self, expr: &Arithmetic) -> Result<Vec<u8>, Error> {
        self.arith_calls.push(expr.source.clone());
        self.arith.get(&expr.source).cloned().ok_or_else(|| {
            Error::new(
                ErrorKind::Host {
                    message: "no arithmetic fixture".to_string(),
                },
                0..0,
            )
        })
    }

    fn arithmetic_value(&mut self, expr: &Arithmetic) -> Result<i64, Error> {
        if let Some(bytes) = self.arith.get(&expr.source) {
            self.arith_calls.push(expr.source.clone());
            return std::str::from_utf8(bytes)
                .ok()
                .and_then(|text| text.trim().parse().ok())
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::Host {
                            message: "arithmetic fixture is not an integer".to_string(),
                        },
                        0..0,
                    )
                });
        }
        // A bare integer subscript needs no fixture; that keeps array tests
        // readable (`${a[1]}` rather than an `arith("1", "1")` setup).
        std::str::from_utf8(&expr.source)
            .ok()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .and_then(|text| text.parse::<i64>().ok())
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::Host {
                        message: "no arithmetic fixture".to_string(),
                    },
                    0..0,
                )
            })
    }

    fn pathname(&mut self, pattern: &Field) -> Result<Option<Vec<Vec<u8>>>, Error> {
        self.glob_calls.push(pattern.bytes.clone());
        Ok(self.globs.get(&pattern.bytes).cloned())
    }

    fn home_dir(&mut self, user: &[u8]) -> Option<Vec<u8>> {
        self.homes.get(user).cloned()
    }
}

/// Parse a single word, as if it were the only thing on the line.
pub fn parse_word(src: &[u8]) -> Word {
    let mut index = 0;
    let parts = parse_parts(src, &mut index, true);
    Word {
        parts,
        span: 0..src.len(),
    }
}

/// Parse a whitespace-separated list of words.
pub fn parse_words(src: &[u8]) -> Vec<Word> {
    let mut words = Vec::new();
    let mut index = 0;
    while index < src.len() {
        while index < src.len() && is_blank(src[index]) {
            index += 1;
        }
        if index >= src.len() {
            break;
        }
        let start = index;
        let parts = parse_parts(src, &mut index, true);
        words.push(Word {
            parts,
            span: start..index,
        });
    }
    words
}

fn is_blank(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n')
}

fn is_name_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_name_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

struct PartsBuilder {
    parts: Vec<Part>,
    lit: Vec<u8>,
    escaped: Vec<u8>,
}

impl PartsBuilder {
    fn new() -> Self {
        Self {
            parts: Vec::new(),
            lit: Vec::new(),
            escaped: Vec::new(),
        }
    }

    fn at_start(&self) -> bool {
        self.parts.is_empty() && self.lit.is_empty() && self.escaped.is_empty()
    }

    fn flush_lit(&mut self) {
        if !self.lit.is_empty() {
            self.parts
                .push(Part::Lit(Lit::unquoted(std::mem::take(&mut self.lit))));
        }
    }

    fn flush_escaped(&mut self) {
        if !self.escaped.is_empty() {
            self.parts
                .push(Part::Lit(Lit::quoted(std::mem::take(&mut self.escaped))));
        }
    }

    fn byte(&mut self, b: u8) {
        self.flush_escaped();
        self.lit.push(b);
    }

    fn escaped_byte(&mut self, b: u8) {
        self.flush_lit();
        self.escaped.push(b);
    }

    fn part(&mut self, part: Part) {
        self.flush_lit();
        self.flush_escaped();
        self.parts.push(part);
    }

    fn finish(mut self) -> Vec<Part> {
        self.flush_lit();
        self.flush_escaped();
        self.parts
    }
}

fn parse_parts(src: &[u8], index: &mut usize, stop_at_blank: bool) -> Vec<Part> {
    let mut builder = PartsBuilder::new();
    while *index < src.len() {
        if stop_at_blank && is_blank(src[*index]) {
            break;
        }
        let byte = src[*index];
        if byte == b'~' && builder.at_start() {
            let end = src[*index..]
                .iter()
                .position(|&b| b == b'/')
                .map_or(src.len(), |offset| *index + offset);
            let user = src[*index + 1..end].to_vec();
            if user
                .iter()
                .all(|&b| !is_blank(b) && !matches!(b, b'$' | b'"' | b'\'' | b'\\'))
            {
                builder.part(Part::Tilde(TildeSpec { user }));
                *index = end;
                continue;
            }
        }
        match byte {
            b'\\' => {
                *index += 1;
                if *index >= src.len() {
                    builder.byte(b'\\');
                    break;
                }
                let next = src[*index];
                *index += 1;
                if next == b'\n' {
                    continue; // line continuation
                }
                builder.escaped_byte(next);
            }
            b'\'' => {
                *index += 1;
                let start = *index;
                while *index < src.len() && src[*index] != b'\'' {
                    *index += 1;
                }
                let bytes = src[start..*index].to_vec();
                if *index < src.len() {
                    *index += 1;
                }
                builder.part(Part::single_quoted(bytes));
            }
            b'"' => {
                *index += 1;
                let parts = parse_double(src, index);
                builder.part(Part::DoubleQ(DoubleQ { parts }));
            }
            b'$' => match parse_dollar(src, index) {
                Some(part) => builder.part(part),
                None => builder.byte(b'$'),
            },
            b'`' => {
                *index += 1;
                let start = *index;
                while *index < src.len() && src[*index] != b'`' {
                    if src[*index] == b'\\' && *index + 1 < src.len() {
                        *index += 1;
                    }
                    *index += 1;
                }
                let source = src[start..*index].to_vec();
                if *index < src.len() {
                    *index += 1;
                }
                builder.part(Part::CmdSub(CommandSub {
                    source,
                    style: CmdSubStyle::Backtick,
                    span: 0..0,
                }));
            }
            _ => {
                builder.byte(byte);
                *index += 1;
            }
        }
    }
    builder.finish()
}

fn parse_double(src: &[u8], index: &mut usize) -> Vec<Part> {
    let mut builder = PartsBuilder::new();
    while *index < src.len() {
        match src[*index] {
            b'"' => {
                *index += 1;
                break;
            }
            b'\\' => {
                *index += 1;
                if *index >= src.len() {
                    builder.byte(b'\\');
                    break;
                }
                let next = src[*index];
                *index += 1;
                match next {
                    b'\n' => continue,
                    b'$' | b'`' | b'"' | b'\\' => builder.escaped_byte(next),
                    _ => {
                        builder.escaped_byte(b'\\');
                        builder.escaped_byte(next);
                    }
                }
            }
            b'$' => match parse_dollar(src, index) {
                Some(part) => builder.part(part),
                None => builder.byte(b'$'),
            },
            b'`' => {
                *index += 1;
                let start = *index;
                while *index < src.len() && src[*index] != b'`' {
                    *index += 1;
                }
                let source = src[start..*index].to_vec();
                if *index < src.len() {
                    *index += 1;
                }
                builder.part(Part::CmdSub(CommandSub {
                    source,
                    style: CmdSubStyle::Backtick,
                    span: 0..0,
                }));
            }
            byte => {
                builder.byte(byte);
                *index += 1;
            }
        }
    }
    builder.finish()
}

/// Parse a `$…` construct. `index` points at the `$`; on success it is
/// advanced past the construct, and on `None` it is left just after the `$`.
fn parse_dollar(src: &[u8], index: &mut usize) -> Option<Part> {
    let start = *index;
    *index += 1;
    let byte = *src.get(*index)?;
    match byte {
        b'\'' => {
            *index += 1;
            let raw_start = *index;
            while *index < src.len() && src[*index] != b'\'' {
                if src[*index] == b'\\' && *index + 1 < src.len() {
                    *index += 1;
                }
                *index += 1;
            }
            let raw = &src[raw_start..*index];
            if *index < src.len() {
                *index += 1;
            }
            Some(Part::ansi_c(decode_ansi_c(raw)))
        }
        b'"' => {
            *index += 1;
            let parts = parse_double(src, index);
            Some(Part::DoubleQ(DoubleQ { parts }))
        }
        b'(' => {
            if src.get(*index + 1) == Some(&b'(') {
                *index += 2;
                let expr_start = *index;
                let mut depth = 0usize;
                while *index < src.len() {
                    match src[*index] {
                        b'(' => {
                            depth += 1;
                            *index += 1;
                        }
                        b')' => {
                            if depth == 0 && src.get(*index + 1) == Some(&b')') {
                                break;
                            }
                            depth = depth.saturating_sub(1);
                            *index += 1;
                        }
                        _ => *index += 1,
                    }
                }
                let source = src[expr_start..*index].to_vec();
                *index = (*index + 2).min(src.len());
                Some(Part::Arith(Arithmetic { source, span: 0..0 }))
            } else {
                *index += 1;
                let source_start = *index;
                let mut depth = 0usize;
                let mut quote: Option<u8> = None;
                while *index < src.len() {
                    let b = src[*index];
                    match quote {
                        Some(q) => {
                            if b == q {
                                quote = None;
                            } else if b == b'\\' && q == b'"' {
                                *index += 1;
                            }
                            *index += 1;
                        }
                        None => {
                            if b == b'\'' || b == b'"' {
                                quote = Some(b);
                                *index += 1;
                            } else if b == b'\\' && *index + 1 < src.len() {
                                *index += 2;
                            } else if b == b'(' {
                                depth += 1;
                                *index += 1;
                            } else if b == b')' {
                                if depth == 0 {
                                    break;
                                }
                                depth -= 1;
                                *index += 1;
                            } else {
                                *index += 1;
                            }
                        }
                    }
                }
                let source = src[source_start..*index].to_vec();
                if *index < src.len() {
                    *index += 1;
                }
                Some(Part::CmdSub(CommandSub {
                    source,
                    style: CmdSubStyle::DollarParen,
                    span: 0..0,
                }))
            }
        }
        b'{' => {
            *index += 1;
            let inner_start = *index;
            let mut depth = 0usize;
            let mut quote: Option<u8> = None;
            while *index < src.len() {
                let b = src[*index];
                match quote {
                    Some(q) => {
                        if b == q {
                            quote = None;
                        } else if b == b'\\' && q == b'"' {
                            *index += 1;
                        }
                        *index += 1;
                    }
                    None => {
                        if b == b'\'' || b == b'"' {
                            quote = Some(b);
                            *index += 1;
                        } else if b == b'\\' && *index + 1 < src.len() {
                            *index += 2;
                        } else if b == b'{' {
                            depth += 1;
                            *index += 1;
                        } else if b == b'}' {
                            if depth == 0 {
                                break;
                            }
                            depth -= 1;
                            *index += 1;
                        } else {
                            *index += 1;
                        }
                    }
                }
            }
            let inner = &src[inner_start..*index];
            if *index < src.len() {
                *index += 1;
            }
            Some(parse_param(inner))
        }
        b'@' => {
            *index += 1;
            Some(Part::Param(ParamExp::value(ParamName::At, 0..0)))
        }
        b'*' => {
            *index += 1;
            Some(Part::Param(ParamExp::value(ParamName::Star, 0..0)))
        }
        b'#' => {
            *index += 1;
            Some(Part::Param(ParamExp::value(ParamName::Count, 0..0)))
        }
        b'?' | b'-' | b'$' | b'!' => {
            *index += 1;
            Some(Part::Param(ParamExp::value(
                ParamName::Var(vec![byte]),
                0..0,
            )))
        }
        b'0'..=b'9' => {
            *index += 1;
            Some(Part::Param(ParamExp::value(
                ParamName::Positional(usize::from(byte - b'0')),
                0..0,
            )))
        }
        _ if is_name_start(byte) => {
            let name_start = *index;
            while *index < src.len() && is_name_continue(src[*index]) {
                *index += 1;
            }
            Some(Part::Param(ParamExp::value(
                ParamName::Var(src[name_start..*index].to_vec()),
                0..0,
            )))
        }
        _ => {
            *index = start + 1;
            None
        }
    }
}

fn parse_param(inner: &[u8]) -> Part {
    let span = 0..inner.len();
    if inner.is_empty() {
        return Part::lit(b"${}".to_vec());
    }
    if inner[0] == b'!' {
        // `${!a[@]}` is the index list; `${!x}` would re-parse the value as a
        // name and stays unsupported.
        if let Some((name, rest)) = parse_name(&inner[1..])
            && rest.is_empty()
        {
            match &name {
                ParamName::Elements { .. } => {
                    return Part::Param(ParamExp::new(name, ParamOp::Indices, span));
                }
                ParamName::Element { .. } => {
                    return Part::Param(ParamExp::new(name, ParamOp::Indirect, span));
                }
                _ => {}
            }
        }
        return Part::Param(ParamExp::new(
            ParamName::Var(inner[1..].to_vec()),
            ParamOp::Unsupported(UnsupportedOp::Indirect),
            span,
        ));
    }
    // `${#x}` is the length operator, `${#}` / `${##}` are ordinary expansions
    // of the special parameter `#`.
    if inner[0] == b'#' && inner.len() > 1 {
        if let Some((name, rest)) = parse_name(&inner[1..]) {
            if rest.is_empty() {
                return Part::Param(ParamExp::new(name, ParamOp::Length, span));
            }
        }
        return Part::Param(ParamExp::new(
            ParamName::Var(inner[1..].to_vec()),
            ParamOp::Unsupported(UnsupportedOp::Other),
            span,
        ));
    }
    let Some((name, rest)) = parse_name(inner) else {
        return Part::lit(inner.to_vec());
    };
    let op = parse_op(rest);
    // An array/positional-list slice keeps its bounds as words so expansion
    // can evaluate them arithmetically, exactly like the real front end.
    let op = if matches!(
        name,
        ParamName::Elements { .. } | ParamName::At | ParamName::Star
    ) && (matches!(op, ParamOp::Substring { .. })
        || matches!(op, ParamOp::Unsupported(UnsupportedOp::ArithmeticSubstring)))
    {
        parse_list_slice(rest).unwrap_or(op)
    } else {
        op
    };
    // Case conversion of a whole list is out of scope, exactly as the real
    // front end reports it. The strip and replacement operators *are*
    // vectorised, so only `Case` is downgraded.
    let op = match &op {
        ParamOp::Case { .. }
            if matches!(
                name,
                ParamName::Elements { .. } | ParamName::At | ParamName::Star
            ) =>
        {
            ParamOp::Case {
                how: CaseHow::Upper,
                pattern: None,
            }
        }
        _ => op,
    };
    Part::Param(ParamExp::new(name, op, span))
}

fn parse_name(inner: &[u8]) -> Option<(ParamName, &[u8])> {
    let first = *inner.first()?;
    let (name, index) = if first.is_ascii_digit() {
        let mut value = 0usize;
        let mut index = 0;
        while index < inner.len() && inner[index].is_ascii_digit() {
            value = value * 10 + usize::from(inner[index] - b'0');
            index += 1;
        }
        let name = if value == 0 {
            ParamName::Var(b"0".to_vec())
        } else {
            ParamName::Positional(value)
        };
        (name, index)
    } else {
        match first {
            b'@' => (ParamName::At, 1),
            b'*' => (ParamName::Star, 1),
            b'#' => (ParamName::Count, 1),
            b'?' | b'-' | b'$' | b'!' => (ParamName::Var(vec![first]), 1),
            _ if is_name_start(first) => {
                let mut index = 1;
                while index < inner.len() && is_name_continue(inner[index]) {
                    index += 1;
                }
                (ParamName::Var(inner[..index].to_vec()), index)
            }
            _ => return None,
        }
    };
    // A subscript is only meaningful on a variable name.
    let name = match name {
        ParamName::Var(name) => name,
        other => return Some((other, &inner[index..])),
    };
    if inner.get(index) != Some(&b'[') {
        return Some((ParamName::Var(name), &inner[index..]));
    }
    let close = inner[index + 1..].iter().position(|&byte| byte == b']')? + index + 1;
    let subscript = &inner[index + 1..close];
    let name = if subscript == b"@" || subscript == b"*" {
        ParamName::elements(name, subscript == b"*")
    } else {
        ParamName::element(name, parse_subword(subscript))
    };
    Some((name, &inner[close + 1..]))
}

fn parse_op(rest: &[u8]) -> ParamOp {
    if rest.is_empty() {
        return ParamOp::Value;
    }
    let operators: [(&[u8], bool); 8] = [
        (b":-", true),
        (b":=", true),
        (b":+", true),
        (b":?", true),
        (b"-", false),
        (b"=", false),
        (b"+", false),
        (b"?", false),
    ];
    for (index, (prefix, colon)) in operators.iter().enumerate() {
        if let Some(after) = rest.strip_prefix(*prefix) {
            let word = parse_subword(after);
            let colon = *colon;
            return match index {
                0 | 4 => ParamOp::UseDefault { word, colon },
                1 | 5 => ParamOp::AssignDefault { word, colon },
                2 | 6 => ParamOp::UseAlternate { word, colon },
                _ => ParamOp::Error {
                    colon,
                    word: if after.is_empty() { None } else { Some(word) },
                },
            };
        }
    }
    if let Some(body) = rest.strip_prefix(b":") {
        return parse_substring(body);
    }
    if let Some(body) = rest.strip_prefix(b"##") {
        return strip_op(body, true, true);
    }
    if let Some(body) = rest.strip_prefix(b"#") {
        return strip_op(body, false, true);
    }
    if let Some(body) = rest.strip_prefix(b"%%") {
        return strip_op(body, true, false);
    }
    if let Some(body) = rest.strip_prefix(b"%") {
        return strip_op(body, false, false);
    }
    if let Some(body) = rest.strip_prefix(b"/") {
        return replace_op(body);
    }
    if let Some(body) = rest.strip_prefix(b"^^") {
        return case_op(body, CaseHow::Upper);
    }
    if let Some(body) = rest.strip_prefix(b",,") {
        return case_op(body, CaseHow::Lower);
    }
    if let Some(body) = rest.strip_prefix(b"^") {
        return case_op(body, CaseHow::UpperFirst);
    }
    if let Some(body) = rest.strip_prefix(b",") {
        return case_op(body, CaseHow::LowerFirst);
    }
    let unsupported = match rest[0] {
        b'@' => UnsupportedOp::Transform,
        b'[' => UnsupportedOp::ArraySubscript,
        _ => UnsupportedOp::Other,
    };
    ParamOp::Unsupported(unsupported)
}

/// `${x#pat}`, `${x##pat}`, `${x%pat}`, `${x%%pat}`; `body` is the pattern.
fn strip_op(body: &[u8], longest: bool, prefix: bool) -> ParamOp {
    let pattern = parse_subword(body);
    if prefix {
        ParamOp::RemovePrefix { pattern, longest }
    } else {
        ParamOp::RemoveSuffix { pattern, longest }
    }
}

/// `${x/pat/rep}` and the anchored and global forms; `body` starts just past
/// the first `/`.
fn replace_op(body: &[u8]) -> ParamOp {
    let (mut mode, body) = match body.first() {
        Some(b'/') => (ReplaceMode::All, &body[1..]),
        Some(b'#') => (ReplaceMode::Prefix, &body[1..]),
        Some(b'%') => (ReplaceMode::Suffix, &body[1..]),
        _ => (ReplaceMode::First, body),
    };
    let mut word = parse_subword(body);
    if mode == ReplaceMode::First {
        if let Some(rest) = word.strip_leading_unquoted(b'#') {
            word = rest;
            mode = ReplaceMode::Prefix;
        } else if let Some(rest) = word.strip_leading_unquoted(b'%') {
            word = rest;
            mode = ReplaceMode::Suffix;
        }
    }
    let skip_first = mode == ReplaceMode::All && word.starts_with_unquoted(b'/');
    let (pattern, replacement) = match word.split_unquoted(b'/', skip_first) {
        Some((pattern, replacement)) => (pattern, replacement),
        None => (word, Word::empty()),
    };
    ParamOp::Replace {
        pattern,
        replacement,
        mode,
    }
}

/// `${x^pat}` and the other case operators; `body` is what follows the
/// operator, empty when no pattern was written.
fn case_op(body: &[u8], how: CaseHow) -> ParamOp {
    let pattern = (!body.is_empty()).then(|| parse_subword(body));
    ParamOp::Case { how, pattern }
}

/// `${a[@]:off:len}` / `${@:off:len}`: the bounds stay words.
fn parse_list_slice(rest: &[u8]) -> Option<ParamOp> {
    let body = rest.strip_prefix(b":")?;
    if body.starts_with(b"-")
        || body.starts_with(b"=")
        || body.starts_with(b"+")
        || body.starts_with(b"?")
    {
        return None;
    }
    let (offset, length) = match body.iter().position(|&byte| byte == b':') {
        Some(index) => (&body[..index], Some(&body[index + 1..])),
        None => (body, None),
    };
    Some(ParamOp::ArraySlice {
        offset: parse_subword(offset),
        length: length.map(parse_subword),
    })
}

fn parse_substring(body: &[u8]) -> ParamOp {
    let (offset_src, length_src) = match body.iter().position(|&b| b == b':') {
        Some(index) => (&body[..index], Some(&body[index + 1..])),
        None => (body, None),
    };
    let Some(offset) = parse_int(offset_src) else {
        return ParamOp::Unsupported(UnsupportedOp::ArithmeticSubstring);
    };
    let length = match length_src {
        None => None,
        Some(src) => match parse_int(src) {
            Some(value) => Some(value),
            None => return ParamOp::Unsupported(UnsupportedOp::ArithmeticSubstring),
        },
    };
    ParamOp::Substring { offset, length }
}

fn parse_int(src: &[u8]) -> Option<i64> {
    let mut index = 0;
    while index < src.len() && matches!(src[index], b' ' | b'\t') {
        index += 1;
    }
    let mut negative = false;
    if index < src.len() && (src[index] == b'+' || src[index] == b'-') {
        negative = src[index] == b'-';
        index += 1;
        while index < src.len() && matches!(src[index], b' ' | b'\t') {
            index += 1;
        }
    }
    let digits_start = index;
    while index < src.len() && src[index].is_ascii_digit() {
        index += 1;
    }
    if index == digits_start {
        return None;
    }
    let value: i64 = std::str::from_utf8(&src[digits_start..index])
        .ok()?
        .parse()
        .ok()?;
    while index < src.len() && matches!(src[index], b' ' | b'\t') {
        index += 1;
    }
    if index != src.len() {
        return None;
    }
    Some(if negative { -value } else { value })
}

fn parse_subword(src: &[u8]) -> Word {
    let mut index = 0;
    let parts = parse_parts(src, &mut index, false);
    Word {
        parts,
        span: 0..src.len(),
    }
}

fn decode_ansi_c(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        if raw[index] != b'\\' || index + 1 >= raw.len() {
            out.push(raw[index]);
            index += 1;
            continue;
        }
        let escape = raw[index + 1];
        index += 2;
        match escape {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'a' => out.push(0x07),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'v' => out.push(0x0b),
            b'e' => out.push(0x1b),
            b'\\' => out.push(b'\\'),
            b'\'' => out.push(b'\''),
            b'"' => out.push(b'"'),
            b'x' => {
                let mut value = 0u8;
                let mut digits = 0;
                while digits < 2 && index < raw.len() {
                    let Some(nibble) = (raw[index] as char).to_digit(16) else {
                        break;
                    };
                    value = value * 16 + nibble as u8;
                    index += 1;
                    digits += 1;
                }
                if digits == 0 {
                    out.push(b'x');
                } else {
                    out.push(value);
                }
            }
            other => {
                out.push(b'\\');
                out.push(other);
            }
        }
    }
    out
}
