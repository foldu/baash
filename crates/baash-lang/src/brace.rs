//! Stage 0: brace expansion (a bash extension; POSIX XCU 2.6 has no such
//! stage).
//!
//! bash's order is brace expansion → tilde → parameter/command/arithmetic →
//! field splitting → pathname → quote removal, and the stage has to come
//! first because it produces *words*: `a{b,c}d` is one word in the source and
//! two fields in argv, and `{a,b}{1,2}` is a cartesian product that no later
//! stage can reconstruct. What follows the stage is untouched: `x{a,$y}b`
//! becomes `xab` and `x$yb` as [`Word`]s, and `$y` is expanded once, later, by
//! [`crate::expand_parameters`].
//!
//! # It is a text transformation over the parts
//!
//! The expansion is performed on the **literal, unquoted bytes** of a word and
//! nowhere else, and the pieces are kept as [`Part`]s rather than flattened to
//! bytes:
//!
//! * a quoted byte is never structural — `"{a,b}"` and `'{a,b}'` are literal,
//!   and so is a backslash-escaped brace (`\{{a,b}` expands the second brace
//!   group and not the escaped one);
//! * an opaque part ([`Part::Param`], [`Part::CmdSub`], [`Part::Arith`],
//!   [`Part::ProcSub`], [`Part::Tilde`]) is skipped entirely, which is what
//!   makes `x=; echo ${x:-{a,b}}` print `{a,b}`: brace expansion ran on the
//!   outer word before the parameter was expanded, and the replacement word is
//!   a separate expansion unit;
//! * everything the stage does not touch is preserved *as parts*, keeping its
//!   quoting and its identity, so a later stage still does its own job exactly
//!   once (`{a,b,c}-$((i++))` evaluates the arithmetic once per produced word,
//!   as bash does).
//!
//! # Forms
//!
//! | input | output |
//! |---|---|
//! | `a{b,c}d` | `abd acd` |
//! | `pre{a,{b,c}}post` | `preapost prebpost precpost` |
//! | `{a,b}{1,2}` | `a1 a2 b1 b2` |
//! | `{1..3}` / `{3..1}` | `1 2 3` / `3 2 1` |
//! | `{01..03}` | `01 02 03` (zero-padded to the widest operand) |
//! | `{a..c}` / `{a..e..2}` | `a b c` / `a c e` |
//! | `{1..10..3}` | `1 4 7 10` |
//! | `{a,}` | `a` plus an empty word (which vanishes unquoted) |
//!
//! A brace group is expanded when its contents contain a **top-level comma**
//! (adjacent to neither nesting) or parse as a **range**. A group with neither
//! — `{}`, `{a}`, `{1.3}`, `{1...3}` — is not a brace expansion at all; such
//! words are returned exactly as written, so an unsupported or misparsed form
//! degrades to the literal text the user typed instead of corrupting the
//! command. Scanning continues past a *matched but unusable* group, which is
//! why `{{a,b}}` is `{a} {b}`: the outer pair is balanced but has no top-level
//! comma, so the inner group is the expansion. A `{` with **no** matching `}`
//! ends the scan and leaves the rest of the word literal, so `{{a,b}` stays
//! `{{a,b}` rather than becoming bash's `{a {b`; a complete group to the left
//! of it still expands, which is why `{a,b}_{` is `a_{ b_{`.
//!
//! # Ranges
//!
//! The forms and their measured details (bash 5.3.20) are:
//!
//! * decimal integers only — `0x1` and `10#1` are not endpoints, and a value
//!   that does not fit `i64` leaves the word alone;
//! * zero padding is on when an operand is written with a leading zero (after
//!   an optional `-`; a leading `+` suppresses it, exactly as bash does), and
//!   the width is the wider of the two operands *including* its sign, so
//!   `{-01..3}` produces `-01 000 001 002 003`;
//! * the step is written after a second `..`; its **magnitude** is used — bash
//!   accepts `{1..8..-3}` and `{8..1..-3}` and walks in the direction the
//!   endpoints imply — and zero means one;
//! * character ranges need both endpoints to be a single ASCII alphabetic
//!   byte, which is bash's `isalpha` test: `{a..e..2}` is `a c e`, `{e..a..-2}`
//!   is `e c a`, and `{!..$}` stays literal.
//!
//! The words this stage produces are literal source text for the stages that
//! follow: not field-split, and glob-active (`{a,b}*` globs).

use baash_diag::Span;

use crate::word::{Lit, LitKind, Part, Word};

/// Stage 0: expand one word's brace groups, producing one or more words.
///
/// Returns exactly `vec![word.clone()]` when the word has no expandable brace
/// group — the "leave it as written" rule that makes an unsupported form
/// harmless. The result order is bash's: the leftmost group varies slowest,
/// so `{a,b}{1,2}` is `a1 a2 b1 b2`.
///
/// The stage is pure: it touches no [`crate::Shell`] callback, so it can run
/// (and be tested) before any runtime state exists.
pub fn expand_braces(word: &Word) -> Vec<Word> {
    match expand_once(&word.parts, &word.span) {
        Some(words) => words,
        None => vec![word.clone()],
    }
}

/// Expand the leftmost expandable group, then recurse into each alternative.
///
/// `None` means there is no such group in these parts.
fn expand_once(parts: &[Part], span: &Span) -> Option<Vec<Word>> {
    let group = find_group(parts)?;
    let prefix = slice(parts, Pos::START, group.open);
    let suffix = slice(parts, advance(parts, group.close), Pos::end(parts));
    let mut words = Vec::new();
    for alternative in alternatives(parts, &group) {
        let mut parts = Vec::with_capacity(prefix.len() + alternative.len() + suffix.len());
        parts.extend(prefix.iter().cloned());
        parts.extend(alternative);
        parts.extend(suffix.iter().cloned());
        let word = Word {
            parts,
            span: span.clone(),
        };
        // The group's braces are gone, so the remaining brace count strictly
        // decreases and this recursion terminates.
        words.extend(expand_braces(&word));
    }
    Some(words)
}

/// A byte boundary inside a word: before `byte` of `part`.
///
/// Brace structure is only ever found in [`LitKind::Unquoted`] literals, so a
/// position always names a byte offset within one. `part == parts.len()` is
/// the end of the word.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Pos {
    part: usize,
    byte: usize,
}

impl Pos {
    /// The start of a word.
    const START: Pos = Pos { part: 0, byte: 0 };

    /// The end of `parts`.
    fn end(parts: &[Part]) -> Self {
        Self {
            part: parts.len(),
            byte: 0,
        }
    }
}

/// The position just after `pos`.
fn advance(parts: &[Part], pos: Pos) -> Pos {
    match parts.get(pos.part) {
        Some(Part::Lit(lit)) if pos.byte + 1 < lit.bytes.len() => Pos {
            part: pos.part,
            byte: pos.byte + 1,
        },
        _ => Pos {
            part: pos.part + 1,
            byte: 0,
        },
    }
}

/// The parts between two positions, clipping the literals at each end.
///
/// A non-literal can only be named at its own start (`byte == 0`), which is how
/// [`advance`] steps over an opaque part: the whole part is then copied
/// unchanged, preserving its quoting and its identity for later stages. So is
/// an *empty* literal: `''` is an empty quoted part that still means "one
/// empty field", so `{a,b}''` must keep it in every alternative.
fn slice(parts: &[Part], from: Pos, to: Pos) -> Vec<Part> {
    let mut out = Vec::new();
    if from >= to {
        return out;
    }
    let stop = to.part.min(parts.len());
    let mut index = from.part;
    while index < stop {
        if index == from.part && from.byte > 0 {
            push_part(&mut out, &parts[index], from.byte, usize::MAX);
        } else {
            out.push(parts[index].clone());
        }
        index += 1;
    }
    if to.part < parts.len() && to.byte > 0 {
        let start = if to.part == from.part { from.byte } else { 0 };
        push_part(&mut out, &parts[to.part], start, to.byte);
    }
    out
}

/// Push `part[from..to]`, where `usize::MAX` means "to the end".
fn push_part(out: &mut Vec<Part>, part: &Part, from: usize, to: usize) {
    match part {
        Part::Lit(lit) => {
            let end = to.min(lit.bytes.len());
            if end > from {
                out.push(Part::Lit(Lit::new(lit.kind, lit.bytes[from..end].to_vec())));
            }
        }
        other => {
            // Only whole opaque parts are copied; a structural byte never
            // lives inside one.
            debug_assert_eq!(from, 0);
            debug_assert_eq!(to, usize::MAX);
            out.push(other.clone());
        }
    }
}

/// Every byte of unquoted literal text, in word order, with its position.
fn structural(parts: &[Part]) -> Vec<(Pos, u8)> {
    let mut units = Vec::new();
    for (part, piece) in parts.iter().enumerate() {
        let Part::Lit(lit) = piece else {
            continue;
        };
        if lit.kind != LitKind::Unquoted {
            continue;
        }
        for (byte, &value) in lit.bytes.iter().enumerate() {
            units.push((Pos { part, byte }, value));
        }
    }
    units
}

/// One expandable brace group.
struct Group {
    /// Position of the opening `{`.
    open: Pos,
    /// Position of the matching `}`.
    close: Pos,
    /// The comma-separated "words" between the braces, or a range to generate.
    body: Body,
}

/// What the inside of a brace group means.
enum Body {
    /// `{a,b,…}`: split at these top-level commas.
    Commas(Vec<Pos>),
    /// `{1..3}` and friends.
    Range(Range),
}

/// Find the leftmost brace group that is a real expansion.
///
/// Scanning is left to right. A `{` whose `}` is missing stops the scan: from
/// that byte on the word's brace syntax is unbalanced, and expanding a group
/// nested inside it would be guessing. The suite pins this for `{{a,b}`, which
/// stays literal rather than becoming bash's `{a {b`; a group already complete
/// to the *left* of the unbalanced brace still expands, which is why
/// `{a,b}_{` is `a_{ b_{` and `{a,b}{` is `a{ b{`.
fn find_group(parts: &[Part]) -> Option<Group> {
    let units = structural(parts);
    for (start, &(open, byte)) in units.iter().enumerate() {
        if byte != b'{' {
            continue;
        }
        let close = matching_close(&units[start..])?;
        let commas = top_level_commas(&units[start + 1..], close);
        if !commas.is_empty() {
            return Some(Group {
                open,
                close,
                body: Body::Commas(commas),
            });
        }
        if let Some(range) = literal_content(parts, open, close)
            .as_deref()
            .and_then(parse_range)
        {
            return Some(Group {
                open,
                close,
                body: Body::Range(range),
            });
        }
    }
    None
}

/// The position of the `}` matching a `units[0] == b'{'`, if there is one.
fn matching_close(units: &[(Pos, u8)]) -> Option<Pos> {
    let mut depth = 0usize;
    for &(pos, byte) in units {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(pos);
                }
            }
            _ => {}
        }
    }
    None
}

/// Commas at nesting depth zero, between a group's `{` and its `close`.
fn top_level_commas(units: &[(Pos, u8)], close: Pos) -> Vec<Pos> {
    let mut depth = 0usize;
    let mut commas = Vec::new();
    for &(pos, byte) in units {
        if pos == close {
            break;
        }
        match byte {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b',' if depth == 0 => commas.push(pos),
            _ => {}
        }
    }
    commas
}

/// A group's contents as text, when they are only unquoted literal bytes.
///
/// Quoting inside the group disqualifies a range on purpose: bash leaves both
/// `{1.."3"}` and `{"1"..3}` literal, so a quoted byte there is not an
/// endpoint.
fn literal_content(parts: &[Part], open: Pos, close: Pos) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for index in open.part..=close.part {
        let Part::Lit(lit) = parts.get(index)? else {
            return None;
        };
        if lit.kind != LitKind::Unquoted {
            return None;
        }
        let from = if index == open.part { open.byte + 1 } else { 0 };
        let to = if index == close.part {
            close.byte
        } else {
            lit.bytes.len()
        };
        out.extend_from_slice(lit.bytes.get(from..to)?);
    }
    Some(out)
}

/// The pieces one group expands to, in output order.
fn alternatives(parts: &[Part], group: &Group) -> Vec<Vec<Part>> {
    match &group.body {
        Body::Commas(commas) => {
            let mut out = Vec::with_capacity(commas.len() + 1);
            let mut start = advance(parts, group.open);
            for &comma in commas {
                out.push(slice(parts, start, comma));
                start = advance(parts, comma);
            }
            out.push(slice(parts, start, group.close));
            out
        }
        Body::Range(range) => range
            .render()
            .into_iter()
            .map(|bytes| vec![Part::Lit(Lit::unquoted(bytes))])
            .collect(),
    }
}

/// A parsed `{x..y[..step]}`.
enum Range {
    /// A decimal integer range, with bash's "did the source ask for padding?"
    /// flag and the width to pad to.
    Number {
        start: i64,
        end: i64,
        /// The step magnitude; zero means one.
        step: i128,
        pad: bool,
        width: usize,
    },
    /// A single-byte character range, `{a..e..2}`.
    Char {
        start: u8,
        end: u8,
        /// The step magnitude; zero means one.
        step: i128,
    },
}

impl Range {
    /// The words this range expands to, in order.
    fn render(&self) -> Vec<Vec<u8>> {
        match *self {
            Range::Number {
                start,
                end,
                step,
                pad,
                width,
            } => {
                let mut out = Vec::new();
                for value in walk(start as i128, end as i128, step) {
                    out.push(format_number(value, pad, width));
                }
                out
            }
            Range::Char { start, end, step } => {
                let mut out = Vec::new();
                for value in walk(start as i128, end as i128, step) {
                    out.push(vec![value as u8]);
                }
                out
            }
        }
    }
}

/// The values from `start` to `end` inclusive, stepping by `step` in the
/// direction the endpoints imply. A zero step is one, and a step larger than
/// the distance yields just `start`.
fn walk(start: i128, end: i128, step: i128) -> Vec<i128> {
    let magnitude = if step == 0 { 1 } else { step.abs() };
    let direction = if end >= start { 1 } else { -1 };
    let mut out = Vec::new();
    let mut value = start;
    loop {
        out.push(value);
        let next = value + direction * magnitude;
        if (direction > 0 && next > end) || (direction < 0 && next < end) {
            break;
        }
        value = next;
    }
    out
}

/// Parse `x..y` or `x..y..step`, or `None` when this is not a range at all.
fn parse_range(text: &[u8]) -> Option<Range> {
    let fields = split_dotdot(text);
    if fields.len() != 2 && fields.len() != 3 {
        return None;
    }
    let step = match fields.get(2) {
        None => 1,
        Some(field) => i128::from(parse_int(field)?),
    };
    if let (Some(start), Some(end)) = (parse_int(fields[0]), parse_int(fields[1])) {
        let pad = zero_padded(fields[0]) || zero_padded(fields[1]);
        let width = fields[0].len().max(fields[1].len());
        return Some(Range::Number {
            start,
            end,
            step,
            pad,
            width,
        });
    }
    let start = parse_char(fields[0])?;
    let end = parse_char(fields[1])?;
    Some(Range::Char { start, end, step })
}

/// Split on every non-overlapping `..`, so `1...3` is `["1", ".3"]` and fails
/// to parse rather than being read as `1`, empty, `3`.
fn split_dotdot(text: &[u8]) -> Vec<&[u8]> {
    let mut fields = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index + 1 < text.len() {
        if text[index] == b'.' && text[index + 1] == b'.' {
            fields.push(&text[start..index]);
            index += 2;
            start = index;
        } else {
            index += 1;
        }
    }
    fields.push(&text[start..]);
    fields
}

/// A signed decimal integer that fits `i64`; anything else is not an endpoint.
fn parse_int(text: &[u8]) -> Option<i64> {
    let (negative, digits) = match text.split_first() {
        Some((b'+', rest)) => (false, rest),
        Some((b'-', rest)) => (true, rest),
        _ => (false, text),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut value: i128 = 0;
    for &digit in digits {
        value = value
            .checked_mul(10)?
            .checked_add(i128::from(digit - b'0'))?;
    }
    let value = if negative { -value } else { value };
    i64::try_from(value).ok()
}

/// Whether this operand was written with a leading zero (bash's padding rule).
///
/// A leading `+` is not "written with a leading zero" even when zeros follow —
/// `{+01..+03}` is `1 2 3` in bash — and a lone `-0` is not padded either;
/// `-00` is.
fn zero_padded(text: &[u8]) -> bool {
    text.first() == Some(&b'0') || (text.starts_with(b"-0") && text.len() > 2)
}

/// A single ASCII alphabetic endpoint (`{a..e}`), matching bash's `isalpha`.
fn parse_char(text: &[u8]) -> Option<u8> {
    match text {
        [byte] if byte.is_ascii_alphabetic() => Some(*byte),
        _ => None,
    }
}

/// Format a range value, zero-padded to `width` when the source asked for it.
///
/// Width includes the sign, like C's `%0*ld`: `-1` at width 3 is `-01`, and `0`
/// at width 3 is `000`.
fn format_number(value: i128, pad: bool, width: usize) -> Vec<u8> {
    let text = value.to_string().into_bytes();
    if !pad || text.len() >= width {
        return text;
    }
    let zeros = width - text.len();
    let mut out = Vec::with_capacity(width);
    if value < 0 {
        out.push(b'-');
        out.extend(std::iter::repeat_n(b'0', zeros));
        out.extend_from_slice(&text[1..]);
    } else {
        out.extend(std::iter::repeat_n(b'0', zeros));
        out.extend_from_slice(&text);
    }
    out
}
