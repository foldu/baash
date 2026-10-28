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
//! | `{a,}` | `a` plus an empty word (which vanishes unquoted) |
//!
//! A brace group is expanded when its contents contain a **top-level comma**
//! (adjacent to neither nesting). A group without one — `{}`, `{a}`, `{1..3}`
//! (for now), `{1.3}` — is not a brace expansion at all; such words are
//! returned exactly as written, so an unsupported or misparsed form degrades to
//! the literal text the user typed instead of corrupting the command. Scanning
//! continues past a *matched but unusable* group, which is why `{{a,b}}` is
//! `{a} {b}`: the outer pair is balanced but has no top-level comma, so the
//! inner group is the expansion. A `{` with **no** matching `}` ends the scan
//! and leaves the rest of the word literal, so `{{a,b}` stays `{{a,b}` rather
//! than becoming bash's `{a {b`; a complete group to the left of it still
//! expands, which is why `{a,b}_{` is `a_{ b_{`.
//!
//! The words this stage produces are literal source text for the stages that
//! follow: not field-split, and glob-active (`{a,b}*` globs).

use bsh_diag::Span;

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
    /// The top-level commas between the braces.
    commas: Vec<Pos>,
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
                commas,
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

/// The pieces one group expands to, in output order.
fn alternatives(parts: &[Part], group: &Group) -> Vec<Vec<Part>> {
    let mut out = Vec::with_capacity(group.commas.len() + 1);
    let mut start = advance(parts, group.open);
    for &comma in &group.commas {
        out.push(slice(parts, start, comma));
        start = advance(parts, comma);
    }
    out.push(slice(parts, start, group.close));
    out
}
