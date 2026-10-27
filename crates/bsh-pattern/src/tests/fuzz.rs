//! Differential fuzzing against two slow reference matchers, in both unit
//! modes.
//!
//! The engine is a compiled NFA; the references are deliberately naive and
//! allowed to be exponential, because they only ever see short inputs. There
//! are two of them:
//!
//! * [`simple_matches`] parses the raw pattern independently (for the
//!   `extglob`-off subset) and backtracks. This catches parser bugs as well as
//!   simulation bugs.
//! * [`ref_matches_ast`] evaluates the engine's own parsed tree by recursive
//!   set-of-ends, with no NFA and no memoisation. This covers `extglob`.
//!
//! Both are compared with the public API on random patterns, texts and flag
//! combinations, in byte mode (`utf8: false`, the C locale) and UTF-8 mode
//! (`utf8: true`). The generator is a fixed-seed xorshift so failures are
//! reproducible.

use std::collections::BTreeSet;

use crate::ast::{self, Bracket, BracketItem, Class, Elem, ExtKind};
use crate::units::{scalar, scalar_folded};
use crate::{Flags, matches, prefix_end, suffix_start};

/// One text or pattern unit: a character in UTF-8 mode, a byte in byte mode.
type Unit = Vec<u8>;

// ---------------------------------------------------------------- randomness

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        // xorshift64, the smallest generator that is not embarrassing.
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a, T>(&mut self, choices: &'a [T]) -> &'a T {
        &choices[self.below(choices.len())]
    }
}

// ------------------------------------------------------------ pattern corpus

const BRACKETS: &[&str] = &[
    "[abc]",
    "[!a-c]",
    "[^b]",
    "[]]",
    "[!]]",
    "[a-]",
    "[-a]",
    "[[:digit:]]",
    "[[:alpha:]]",
    "[[:bogus:]]",
    "[é]",
    "[à-ÿ]",
    "[a-c-e]",
    "[]-a]",
    "[a\\-z]",
    "[[]",
    "[abc",
    "[!]",
    "[[:space:]]",
    "[[:punct:]]",
    "[A-z]",
    "[--0]",
    "[z-a]",
    "[![:upper:]]",
];

const LITERALS: &[char] = &['a', 'b', 'c', '/', 'é', 'μ', 'A', '-', '!', '.', '0'];
const ESCAPED: &[char] = &['a', 'b', '*', '?', '[', ']', '(', ')', '|', '\\', '-'];
const OPERATORS: &[char] = &['?', '*', '+', '@', '!'];
const TEXT_ALPHABET: &[char] = &[
    'a', 'b', 'c', 'A', 'B', '-', ']', '!', ':', '/', 'é', 'μ', '.', '0', '5', '_', '(', ')', '|',
    '*', '?', '\\',
];

fn gen_pattern(rng: &mut Rng, depth: usize, extglob: bool) -> String {
    let count = rng.below(5);
    let mut out = String::new();
    for _ in 0..count {
        let choices = if extglob && depth > 0 { 9 } else { 7 };
        match rng.below(choices) {
            0 => out.push(*rng.pick(LITERALS)),
            1 => out.push('*'),
            2 => out.push('?'),
            3 => out.push_str(rng.pick(BRACKETS)),
            4 => {
                out.push('\\');
                out.push(*rng.pick(ESCAPED));
            }
            5 => out.push('|'),
            6 => out.push(if rng.below(2) == 0 { '(' } else { ')' }),
            7 | 8 => {
                out.push(*rng.pick(OPERATORS));
                out.push('(');
                let alternatives = 1 + rng.below(3);
                for i in 0..alternatives {
                    if i > 0 {
                        out.push('|');
                    }
                    out.push_str(&gen_pattern(rng, depth - 1, extglob));
                }
                out.push(')');
            }
            _ => unreachable!(),
        }
    }
    out
}

fn gen_text(rng: &mut Rng) -> String {
    let count = rng.below(9);
    let mut out = String::new();
    for _ in 0..count {
        out.push(*rng.pick(TEXT_ALPHABET));
    }
    out
}

fn random_flags(rng: &mut Rng, extglob: bool, utf8: bool) -> Flags {
    Flags {
        extglob,
        nocasematch: rng.below(2) == 0,
        pathname: rng.below(2) == 0,
        utf8,
    }
}

/// Split a string into the units the engine would use in this mode.
fn units_of(text: &str, utf8: bool) -> Vec<Unit> {
    if utf8 {
        text.chars().map(|c| c.to_string().into_bytes()).collect()
    } else {
        text.bytes().map(|b| vec![b]).collect()
    }
}

/// Byte offsets of every unit boundary.
fn boundaries(text: &str, utf8: bool) -> Vec<usize> {
    if utf8 {
        let mut out: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
        out.push(text.len());
        out
    } else {
        (0..=text.len()).collect()
    }
}

// -------------------------------------------------- reference: parsed the same

fn eq_unit(a: &[u8], b: &[u8], nocase: bool) -> bool {
    if nocase {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

fn byte_range(lo: u8, hi: u8, unit: u8, nocase: bool) -> bool {
    let (lo, hi, unit) = if nocase {
        (
            lo.to_ascii_lowercase(),
            hi.to_ascii_lowercase(),
            unit.to_ascii_lowercase(),
        )
    } else {
        (lo, hi, unit)
    };
    lo <= unit && unit <= hi
}

fn ref_range(lo: &[u8], hi: &[u8], unit: &[u8], flags: Flags) -> bool {
    if !flags.utf8 {
        return match (lo.first(), hi.first(), unit.first()) {
            (Some(&lo), Some(&hi), Some(&unit)) => byte_range(lo, hi, unit, flags.nocasematch),
            _ => false,
        };
    }
    let (lo, hi, unit) = if flags.nocasematch {
        (scalar_folded(lo), scalar_folded(hi), scalar_folded(unit))
    } else {
        (scalar(lo), scalar(hi), scalar(unit))
    };
    matches!((lo, hi, unit), (Some(lo), Some(hi), Some(unit)) if lo <= unit && unit <= hi)
}

fn ref_bracket(bracket: &Bracket, unit: &[u8], flags: Flags) -> bool {
    if flags.pathname && unit == b"/" {
        return false;
    }
    let hit = bracket.items.iter().any(|item| match item {
        BracketItem::Char(c) => eq_unit(c, unit, flags.nocasematch),
        BracketItem::Range(lo, hi) => ref_range(lo, hi, unit, flags),
        BracketItem::Class(class) => unit.len() == 1 && class.matches(unit[0]),
        BracketItem::Never => false,
    });
    hit != bracket.negate
}

fn ref_ends_alt(
    alts: &[Vec<Elem>],
    units: &[Vec<u8>],
    pos: usize,
    flags: Flags,
) -> BTreeSet<usize> {
    let mut out = BTreeSet::new();
    for alt in alts {
        out.extend(ref_ends_seq(alt, units, pos, flags));
    }
    out
}

fn ref_ends_elem(elem: &Elem, units: &[Vec<u8>], pos: usize, flags: Flags) -> BTreeSet<usize> {
    let n = units.len();
    let slash = |q: usize| units[q].as_slice() == b"/";
    match elem {
        Elem::Lit(bytes) => {
            if pos < n && eq_unit(bytes, &units[pos], flags.nocasematch) {
                BTreeSet::from([pos + 1])
            } else {
                BTreeSet::new()
            }
        }
        Elem::Any => {
            if pos < n && !(flags.pathname && slash(pos)) {
                BTreeSet::from([pos + 1])
            } else {
                BTreeSet::new()
            }
        }
        Elem::Star => {
            let mut out = BTreeSet::new();
            let mut q = pos;
            loop {
                out.insert(q);
                if q == n || (flags.pathname && slash(q)) {
                    break;
                }
                q += 1;
            }
            out
        }
        Elem::Bracket(bracket) => {
            if pos < n && ref_bracket(bracket, &units[pos], flags) {
                BTreeSet::from([pos + 1])
            } else {
                BTreeSet::new()
            }
        }
        Elem::Ext(kind, alts) => match kind {
            ExtKind::At => ref_ends_alt(alts, units, pos, flags),
            ExtKind::Opt => {
                let mut out = ref_ends_alt(alts, units, pos, flags);
                out.insert(pos);
                out
            }
            ExtKind::Star => {
                let mut reach = BTreeSet::from([pos]);
                let mut frontier = vec![pos];
                while let Some(p) = frontier.pop() {
                    for q in ref_ends_alt(alts, units, p, flags) {
                        if reach.insert(q) {
                            frontier.push(q);
                        }
                    }
                }
                reach
            }
            ExtKind::Plus => {
                let mut reach = ref_ends_alt(alts, units, pos, flags);
                let mut frontier: Vec<usize> = reach.iter().copied().collect();
                while let Some(p) = frontier.pop() {
                    for q in ref_ends_alt(alts, units, p, flags) {
                        if reach.insert(q) {
                            frontier.push(q);
                        }
                    }
                }
                reach
            }
            ExtKind::Not => {
                let at = ref_ends_alt(alts, units, pos, flags);
                (pos..=n).filter(|q| !at.contains(q)).collect()
            }
        },
    }
}

fn ref_ends_seq(seq: &[Elem], units: &[Vec<u8>], start: usize, flags: Flags) -> BTreeSet<usize> {
    let mut current = BTreeSet::from([start]);
    for elem in seq {
        let mut next = BTreeSet::new();
        for &pos in &current {
            next.extend(ref_ends_elem(elem, units, pos, flags));
        }
        current = next;
        if current.is_empty() {
            break;
        }
    }
    current
}

fn ref_matches_ast(ast: &[Elem], units: &[Vec<u8>], flags: Flags) -> bool {
    ref_ends_seq(ast, units, 0, flags).contains(&units.len())
}

// ------------------------------------ reference: independently parsed (no extglob)

enum SItem {
    Char(Unit),
    Range(Unit, Unit),
    Class(Class),
    Never,
}

struct SBracket {
    negate: bool,
    items: Vec<SItem>,
}

fn is(unit: &[u8], byte: u8) -> bool {
    unit.len() == 1 && unit[0] == byte
}

fn s_char(p: &[Unit], i: usize) -> Option<(Unit, usize)> {
    let unit = p.get(i)?;
    if is(unit, b'\\') && i + 1 < p.len() {
        Some((p[i + 1].clone(), i + 2))
    } else {
        Some((unit.clone(), i + 1))
    }
}

fn s_class(p: &[Unit], start: usize) -> Option<(Option<Class>, usize)> {
    let mut i = start;
    while i + 1 < p.len() {
        if is(&p[i], b':') && is(&p[i + 1], b']') {
            let mut name = Vec::new();
            for unit in &p[start..i] {
                name.extend_from_slice(unit);
            }
            return Some((Class::from_name(&name), i + 2));
        }
        i += 1;
    }
    None
}

fn s_bracket(p: &[Unit], start: usize) -> Option<(usize, SBracket)> {
    let mut i = start + 1;
    let mut negate = false;
    if i < p.len() && (is(&p[i], b'!') || is(&p[i], b'^')) {
        negate = true;
        i += 1;
    }
    let mut items = Vec::new();
    let mut first = true;
    while i < p.len() {
        if is(&p[i], b']') && !first {
            return Some((i + 1, SBracket { negate, items }));
        }
        if is(&p[i], b'[') && p.get(i + 1).is_some_and(|u| is(u, b':')) {
            if let Some((class, end)) = s_class(p, i + 2) {
                items.push(match class {
                    Some(class) => SItem::Class(class),
                    None => SItem::Never,
                });
                i = end;
                first = false;
                continue;
            }
        }
        let (ch, next) = s_char(p, i)?;
        i = next;
        if i < p.len() && is(&p[i], b'-') && p.get(i + 1).is_some_and(|u| !is(u, b']')) {
            if let Some((end_ch, after)) = s_char(p, i + 1) {
                items.push(SItem::Range(ch, end_ch));
                i = after;
                first = false;
                continue;
            }
        }
        items.push(SItem::Char(ch));
        first = false;
    }
    None
}

fn s_bracket_matches(bracket: &SBracket, unit: &[u8], flags: Flags) -> bool {
    let hit = bracket.items.iter().any(|item| match item {
        SItem::Char(c) => eq_unit(c, unit, flags.nocasematch),
        SItem::Range(lo, hi) => ref_range(lo, hi, unit, flags),
        SItem::Class(class) => unit.len() == 1 && class.matches(unit[0]),
        SItem::Never => false,
    });
    hit != bracket.negate
}

fn s_go(p: &[Unit], pi: usize, t: &[Unit], ti: usize, flags: Flags) -> bool {
    if pi == p.len() {
        return ti == t.len();
    }
    if is(&p[pi], b'\\') {
        let (lit, next) = if pi + 1 < p.len() {
            (p[pi + 1].as_slice(), pi + 2)
        } else {
            (b"\\".as_slice(), pi + 1)
        };
        return ti < t.len()
            && eq_unit(lit, &t[ti], flags.nocasematch)
            && s_go(p, next, t, ti + 1, flags);
    }
    if is(&p[pi], b'*') {
        for q in ti..=t.len() {
            if flags.pathname && t[ti..q].iter().any(|u| u.as_slice() == b"/") {
                break;
            }
            if s_go(p, pi + 1, t, q, flags) {
                return true;
            }
        }
        return false;
    }
    if is(&p[pi], b'?') {
        return ti < t.len()
            && !(flags.pathname && t[ti].as_slice() == b"/")
            && s_go(p, pi + 1, t, ti + 1, flags);
    }
    if is(&p[pi], b'[') {
        return match s_bracket(p, pi) {
            Some((next, bracket)) => {
                ti < t.len()
                    && !(flags.pathname && t[ti].as_slice() == b"/")
                    && s_bracket_matches(&bracket, &t[ti], flags)
                    && s_go(p, next, t, ti + 1, flags)
            }
            None => {
                ti < t.len()
                    && eq_unit(b"[", &t[ti], flags.nocasematch)
                    && s_go(p, pi + 1, t, ti + 1, flags)
            }
        };
    }
    ti < t.len() && eq_unit(&p[pi], &t[ti], flags.nocasematch) && s_go(p, pi + 1, t, ti + 1, flags)
}

fn simple_matches_units(pattern: &[Unit], text: &[Unit], flags: Flags) -> bool {
    s_go(pattern, 0, text, 0, flags)
}

fn simple_matches(pattern: &str, text: &str, flags: Flags) -> bool {
    let pattern = units_of(pattern, flags.utf8);
    let text = units_of(text, flags.utf8);
    simple_matches_units(&pattern, &text, flags)
}

// --------------------------------------------------------------------- checks

fn check_prefix(ast: &[Elem], simple: bool, pattern: &str, text: &str, flags: Flags) {
    let units = units_of(text, flags.utf8);
    let bounds = boundaries(text, flags.utf8);
    let pattern_units = units_of(pattern, flags.utf8);
    let mut prefix_hits = vec![false; units.len() + 1];
    let mut suffix_hits = vec![false; units.len() + 1];
    for q in 0..=units.len() {
        prefix_hits[q] = if simple {
            simple_matches_units(&pattern_units, &units[..q], flags)
        } else {
            ref_matches_ast(ast, &units[..q], flags)
        };
        suffix_hits[q] = if simple {
            simple_matches_units(&pattern_units, &units[q..], flags)
        } else {
            ref_matches_ast(ast, &units[q..], flags)
        };
    }
    let short = prefix_hits.iter().position(|&b| b).map(|q| bounds[q]);
    let long = prefix_hits.iter().rposition(|&b| b).map(|q| bounds[q]);
    assert_eq!(
        prefix_end(pattern.as_bytes(), text.as_bytes(), false, flags),
        short,
        "shortest prefix: pattern={pattern:?} text={text:?} flags={flags:?}"
    );
    assert_eq!(
        prefix_end(pattern.as_bytes(), text.as_bytes(), true, flags),
        long,
        "longest prefix: pattern={pattern:?} text={text:?} flags={flags:?}"
    );
    // Shortest suffix is the fewest units removed, i.e. the *largest* start
    // offset; longest is the smallest.
    let short = suffix_hits.iter().rposition(|&b| b).map(|q| bounds[q]);
    let long = suffix_hits.iter().position(|&b| b).map(|q| bounds[q]);
    assert_eq!(
        suffix_start(pattern.as_bytes(), text.as_bytes(), false, flags),
        short,
        "shortest suffix: pattern={pattern:?} text={text:?} flags={flags:?}"
    );
    assert_eq!(
        suffix_start(pattern.as_bytes(), text.as_bytes(), true, flags),
        long,
        "longest suffix: pattern={pattern:?} text={text:?} flags={flags:?}"
    );
}

#[test]
fn fuzz_without_extglob_against_independent_backtracker() {
    let mut rng = Rng::new(0x5eed_0000_0000_0001);
    for utf8 in [false, true] {
        for iteration in 0..2000 {
            let pattern = gen_pattern(&mut rng, 0, false);
            let text = gen_text(&mut rng);
            let flags = random_flags(&mut rng, false, utf8);
            let ast = ast::parse(pattern.as_bytes(), false, utf8);
            let expected = simple_matches(&pattern, &text, flags);
            assert_eq!(
                matches(pattern.as_bytes(), text.as_bytes(), flags),
                expected,
                "utf8={utf8} iteration {iteration}: pattern={pattern:?} text={text:?} flags={flags:?}"
            );
            // The AST reference must agree too; if it does not, one of the two
            // references is wrong and the differential is worthless.
            assert_eq!(
                ref_matches_ast(&ast, &units_of(&text, utf8), flags),
                expected,
                "reference disagreement, utf8={utf8}: pattern={pattern:?} text={text:?} flags={flags:?}"
            );
            check_prefix(&ast, true, &pattern, &text, flags);
        }
    }
}

#[test]
fn fuzz_with_extglob_against_ast_reference() {
    let mut rng = Rng::new(0x5eed_0000_0000_0002);
    for utf8 in [false, true] {
        for iteration in 0..2000 {
            let pattern = gen_pattern(&mut rng, 2, true);
            let text = gen_text(&mut rng);
            let flags = random_flags(&mut rng, true, utf8);
            let ast = ast::parse(pattern.as_bytes(), true, utf8);
            let units = units_of(&text, utf8);
            assert_eq!(
                matches(pattern.as_bytes(), text.as_bytes(), flags),
                ref_matches_ast(&ast, &units, flags),
                "utf8={utf8} iteration {iteration}: pattern={pattern:?} text={text:?} flags={flags:?}"
            );
            check_prefix(&ast, false, &pattern, &text, flags);
        }
    }
}

#[test]
fn extglob_off_is_the_same_language_as_literal_operators() {
    // A pattern generated with extglob on, then matched with the flag off, is
    // just its literal text with `*`/`?`/brackets still active. The independent
    // parser must agree, in both modes.
    let mut rng = Rng::new(0x5eed_0000_0000_0003);
    for utf8 in [false, true] {
        for iteration in 0..500 {
            let pattern = gen_pattern(&mut rng, 2, true);
            let text = gen_text(&mut rng);
            let flags = random_flags(&mut rng, false, utf8);
            assert_eq!(
                matches(pattern.as_bytes(), text.as_bytes(), flags),
                simple_matches(&pattern, &text, flags),
                "utf8={utf8} iteration {iteration}: pattern={pattern:?} text={text:?} flags={flags:?}"
            );
        }
    }
}
