//! Compiling a pattern to an NFA and simulating it over the text.
//!
//! # Why an NFA, not backtracking
//!
//! A backtracking matcher tries every way of splitting `*` and every way of
//! ending a `!(…)` group; a pattern such as `a*a*a*…*b` is exponential in the
//! number of stars. This module compiles the pattern once into a Thompson-style
//! NFA and runs it as a *set of states* per text position. Every state at
//! every position is considered exactly once, so the work is polynomial by
//! construction — there is no backtracking and therefore no blow-up.
//!
//! `!(A)` cannot be a plain Thompson edge, because it accepts a span based on
//! whether the *sub*-automaton `A` rejects it. The `Neg` instruction computes
//! the set of ends `A` accepts from the current position (memoised per program
//! and start position) and injects its continuation at every other end. That
//! is the only construct with a quadratic-in-text cost.
//!
//! Complexity for `n` text characters and an NFA of `m` instructions:
//!
//! * no `!` groups: `O(n·m)` time, `O(n·m / 64)` space for the pending sets;
//! * with `!` groups: at most `O(n²·m)` time in the pathological case, still
//!   polynomial and independent of how the stars are arranged.

use std::rc::Rc;

use crate::Flags;
use crate::ast::{Bracket, BracketItem, Elem, ExtKind};
use crate::units::{Text, scalar, scalar_folded};

/// One NFA instruction.
#[derive(Clone, Debug)]
pub(crate) enum Inst {
    /// Consume one character equal to these bytes, then go to the target.
    Lit(Vec<u8>, usize),
    /// Consume any character (except `/` under `pathname`), then to target.
    Any(usize),
    /// `*`: consume any character and stay, or fall through to the target.
    Star(usize),
    /// Consume one character in the bracket expression, then to target.
    Bracket(Bracket, usize),
    /// Epsilon choice between two instructions.
    Split(usize, usize),
    /// `!(…)`: continue at `next` for every span the sub-program rejects.
    Neg { sub: usize, next: usize },
    /// The accepting state.
    Accept,
}

/// One sub-automaton: a start instruction and its accepting instruction.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Prog {
    start: usize,
    accept: usize,
}

/// A compiled pattern.
#[derive(Clone, Debug)]
pub(crate) struct Program {
    insts: Vec<Inst>,
    progs: Vec<Prog>,
    main: usize,
    /// Whether any `!(…)` group occurs.
    ///
    /// The only construct that needs the per-position `pending` sets and the
    /// sub-program cache; allocating those for every other pattern costs an
    /// allocation per text position and per sub-program on every run.
    has_neg: bool,
}

/// Compile a top-level sequence.
pub(crate) fn compile(top: &[Elem]) -> Program {
    let mut c = Compiler {
        insts: Vec::new(),
        progs: Vec::new(),
    };
    let accept = c.push(Inst::Accept);
    let start = c.sequence(top, accept);
    let main = c.progs.len();
    c.progs.push(Prog { start, accept });
    Program {
        has_neg: c.insts.iter().any(|inst| matches!(inst, Inst::Neg { .. })),
        insts: c.insts,
        progs: c.progs,
        main,
    }
}

struct Compiler {
    insts: Vec<Inst>,
    progs: Vec<Prog>,
}

impl Compiler {
    fn push(&mut self, inst: Inst) -> usize {
        let at = self.insts.len();
        self.insts.push(inst);
        at
    }

    /// Compile an alternation body into a fresh sub-program.
    fn program(&mut self, alts: &[Vec<Elem>]) -> usize {
        let accept = self.push(Inst::Accept);
        let start = self.alternatives(alts, accept);
        let id = self.progs.len();
        self.progs.push(Prog { start, accept });
        id
    }

    fn sequence(&mut self, elems: &[Elem], next: usize) -> usize {
        let mut entry = next;
        for elem in elems.iter().rev() {
            entry = self.elem(elem, entry);
        }
        entry
    }

    fn alternatives(&mut self, alts: &[Vec<Elem>], next: usize) -> usize {
        let mut entries = Vec::with_capacity(alts.len());
        for alt in alts {
            entries.push(self.sequence(alt, next));
        }
        let mut entry = entries[0];
        for &other in &entries[1..] {
            entry = self.push(Inst::Split(entry, other));
        }
        entry
    }

    fn elem(&mut self, elem: &Elem, entry_next: usize) -> usize {
        match elem {
            Elem::Lit(bytes) => self.push(Inst::Lit(bytes.clone(), entry_next)),
            Elem::Any => self.push(Inst::Any(entry_next)),
            Elem::Star => self.push(Inst::Star(entry_next)),
            Elem::Bracket(b) => self.push(Inst::Bracket(b.clone(), entry_next)),
            Elem::Ext(kind, alts) => match kind {
                ExtKind::At => self.alternatives(alts, entry_next),
                ExtKind::Opt => {
                    let alt = self.alternatives(alts, entry_next);
                    self.push(Inst::Split(alt, entry_next))
                }
                ExtKind::Star | ExtKind::Plus => {
                    // A shared Split is the repetition point: entering the
                    // group again or moving on. `+` starts at the group, so it
                    // must take at least one branch.
                    let split = self.push(Inst::Split(0, entry_next));
                    let alt = self.alternatives(alts, split);
                    self.insts[split] = Inst::Split(alt, entry_next);
                    if *kind == ExtKind::Star { split } else { alt }
                }
                ExtKind::Not => {
                    let sub = self.program(alts);
                    self.push(Inst::Neg {
                        sub,
                        next: entry_next,
                    })
                }
            },
        }
    }
}

/// A small fixed-size set of instruction indices.
#[derive(Clone)]
struct Bits {
    words: Vec<u64>,
}

impl Bits {
    fn new(len: usize) -> Self {
        Self {
            words: vec![0; len.div_ceil(64)],
        }
    }

    fn set(&mut self, i: usize) {
        self.words[i / 64] |= 1 << (i % 64);
    }

    fn get(&self, i: usize) -> bool {
        self.words[i / 64] & (1 << (i % 64)) != 0
    }

    /// Set `i`; returns whether it was newly set.
    fn insert(&mut self, i: usize) -> bool {
        let word = &mut self.words[i / 64];
        let mask = 1 << (i % 64);
        let was = *word & mask != 0;
        *word |= mask;
        !was
    }

    fn clear(&mut self) {
        self.words.fill(0);
    }

    fn or(&mut self, other: &Bits) {
        for (a, b) in self.words.iter_mut().zip(&other.words) {
            *a |= *b;
        }
    }

    fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.words.iter().enumerate().flat_map(|(w, &word)| {
            let mut bits = word;
            std::iter::from_fn(move || {
                if bits == 0 {
                    return None;
                }
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                Some(w * 64 + bit)
            })
        })
    }
}

/// The accepted end positions of a program started at some text position.
type Ends = Rc<[bool]>;

/// Run a program from the start of `text` and return which positions accept.
pub(crate) fn run(program: &Program, text: &Text<'_>, flags: Flags) -> Ends {
    let mut runner = Runner {
        program,
        text,
        flags,
        // Both are only reachable from a `Neg` instruction.
        cache: if program.has_neg {
            vec![vec![None; text.len() + 1]; program.progs.len()]
        } else {
            Vec::new()
        },
    };
    runner.run_from(program.main, 0)
}

struct Runner<'a> {
    program: &'a Program,
    text: &'a Text<'a>,
    flags: Flags,
    /// `cache[program][start]` is the accepted end set of that sub-program run
    /// from that position. Filled on demand; `!(…)` is the only user, so it is
    /// empty for every other pattern.
    cache: Vec<Vec<Option<Ends>>>,
}

/// Epsilon candidates, copied out so the mutable borrow for `run_from` is free.
enum Epsilon {
    Split(usize, usize),
    Star(usize),
    Neg(usize, usize),
    None,
}

impl Runner<'_> {
    fn run_from(&mut self, prog: usize, start: usize) -> Ends {
        if !self.cache.is_empty() {
            if let Some(hit) = &self.cache[prog][start] {
                return Rc::clone(hit);
            }
        }
        let n = self.text.len();
        let m = self.program.insts.len();
        let p = self.program.progs[prog];
        let mut accepted = vec![false; n + 1];
        let mut current = Bits::new(m);
        let mut next = Bits::new(m);
        let mut stack = Vec::with_capacity(m);
        // Only `!(…)` writes here; for every other pattern this would be one
        // allocation per text position on every run.
        let mut pending = if self.program.has_neg {
            vec![Bits::new(m); n + 1]
        } else {
            Vec::new()
        };

        current.set(p.start);
        self.close(start, &mut current, &mut pending, &mut stack);
        accepted[start] = current.get(p.accept);

        for q in start..n {
            next.clear();
            let unit = self.text.unit(q);
            let slash_blocked = self.flags.pathname && unit == b"/";
            for state in current.iter() {
                match &self.program.insts[state] {
                    Inst::Lit(bytes, target) => {
                        if self.equal(bytes, unit) {
                            next.set(*target);
                        }
                    }
                    Inst::Any(target) => {
                        if !slash_blocked {
                            next.set(*target);
                        }
                    }
                    // Staying on the same instruction consumes one more
                    // character; the epsilon fall-through is in `close`.
                    Inst::Star(_) => {
                        if !slash_blocked {
                            next.set(state);
                        }
                    }
                    Inst::Bracket(bracket, target) => {
                        if !slash_blocked && self.bracket(bracket, unit) {
                            next.set(*target);
                        }
                    }
                    Inst::Split(..) | Inst::Neg { .. } | Inst::Accept => {}
                }
            }
            if !pending.is_empty() {
                next.or(&pending[q + 1]);
            }
            self.close(q + 1, &mut next, &mut pending, &mut stack);
            accepted[q + 1] = next.get(p.accept);
            std::mem::swap(&mut current, &mut next);
        }

        let ends: Ends = accepted.into();
        if !self.cache.is_empty() {
            self.cache[prog][start] = Some(Rc::clone(&ends));
        }
        ends
    }

    /// Epsilon-closure `current` at text position `pos`.
    ///
    /// `Star` falls through to its target; `Split` explores both branches;
    /// `Neg` asks its sub-program which spans it rejects and schedules the
    /// continuation at every rejected end.
    fn close(
        &mut self,
        pos: usize,
        current: &mut Bits,
        pending: &mut [Bits],
        stack: &mut Vec<usize>,
    ) {
        stack.clear();
        stack.extend(current.iter());
        while let Some(state) = stack.pop() {
            let epsilon = match &self.program.insts[state] {
                Inst::Split(a, b) => Epsilon::Split(*a, *b),
                Inst::Star(target) => Epsilon::Star(*target),
                Inst::Neg { sub, next } => Epsilon::Neg(*sub, *next),
                _ => Epsilon::None,
            };
            match epsilon {
                Epsilon::Split(a, b) => {
                    for target in [a, b] {
                        if current.insert(target) {
                            stack.push(target);
                        }
                    }
                }
                Epsilon::Star(target) => {
                    if current.insert(target) {
                        stack.push(target);
                    }
                }
                Epsilon::Neg(sub, target) => {
                    let ends = self.run_from(sub, pos);
                    for q in pos..=self.text.len() {
                        if ends[q] {
                            continue;
                        }
                        if q == pos {
                            if current.insert(target) {
                                stack.push(target);
                            }
                        } else {
                            pending[q].set(target);
                        }
                    }
                }
                Epsilon::None => {}
            }
        }
    }

    fn equal(&self, pattern: &[u8], unit: &[u8]) -> bool {
        if self.flags.nocasematch {
            pattern.eq_ignore_ascii_case(unit)
        } else {
            pattern == unit
        }
    }

    fn bracket(&self, bracket: &Bracket, unit: &[u8]) -> bool {
        let hit = bracket
            .items
            .iter()
            .any(|item| self.bracket_item(item, unit));
        hit != bracket.negate
    }

    fn bracket_item(&self, item: &BracketItem, unit: &[u8]) -> bool {
        match item {
            BracketItem::Char(ch) => self.equal(ch, unit),
            BracketItem::Range(lo, hi) => self.range(lo, hi, unit),
            BracketItem::Class(class) => unit.len() == 1 && class.matches(unit[0]),
            BracketItem::Never => false,
        }
    }

    /// Range membership.
    ///
    /// In the default byte mode this is a byte comparison. In UTF-8 mode it
    /// compares Unicode scalar values. Under `nocasematch` both endpoints and
    /// the character are ASCII-folded first, which is what bash does:
    /// `[A-z]` then behaves as `[a-z]`.
    fn range(&self, lo: &[u8], hi: &[u8], unit: &[u8]) -> bool {
        if !self.flags.utf8 {
            let (Some(&lo), Some(&hi), Some(&unit)) = (lo.first(), hi.first(), unit.first()) else {
                return false;
            };
            let (lo, hi, unit) = if self.flags.nocasematch {
                (
                    lo.to_ascii_lowercase(),
                    hi.to_ascii_lowercase(),
                    unit.to_ascii_lowercase(),
                )
            } else {
                (lo, hi, unit)
            };
            return lo <= unit && unit <= hi;
        }
        let (lo, hi, unit) = if self.flags.nocasematch {
            (scalar_folded(lo), scalar_folded(hi), scalar_folded(unit))
        } else {
            (scalar(lo), scalar(hi), scalar(unit))
        };
        match (lo, hi, unit) {
            (Some(lo), Some(hi), Some(unit)) => lo <= unit && unit <= hi,
            // Invalid bytes have no scalar value and match no range. Literal
            // bracket characters still match them byte for byte.
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_roundtrip() {
        let mut bits = Bits::new(100);
        assert!(!bits.get(99));
        assert!(bits.insert(99));
        assert!(!bits.insert(99));
        assert!(bits.get(99));
        assert_eq!(bits.iter().collect::<Vec<_>>(), vec![99]);
        bits.clear();
        assert!(!bits.get(99));
    }
}
