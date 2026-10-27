//! Parsing a shell pattern into a small element tree.
//!
//! The parser never fails. Bash's own pattern parser is tolerant in the same
//! way: an unterminated `[` is a literal `[`, a trailing backslash is a
//! literal backslash, and an unterminated `@(` (with `extglob` on) degrades to
//! literal text rather than an error. Degrading instead of erroring is what
//! `case` and `${x#pat}` do.
//!
//! # Extglob grammar
//!
//! With `extglob` off, `(`, `)`, `|`, `@`, `+` and `!` are ordinary
//! characters. With it on, `?(`, `*(`, `+(`, `@(` and `!(` open a group whose
//! body is one or more `|`-separated alternatives; groups nest. A backslash
//! escapes the following character everywhere, including `|` and `)`.

use crate::units::char_len;

/// One pattern element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Elem {
    /// One character (one to four bytes), already unescaped.
    Lit(Vec<u8>),
    /// `?`: exactly one character.
    Any,
    /// `*`: zero or more characters.
    Star,
    /// A bracket expression.
    Bracket(Bracket),
    /// An extended glob group.
    Ext(ExtKind, Vec<Vec<Elem>>),
}

/// The five extended-glob operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExtKind {
    /// `@(…)`: exactly one alternative.
    At,
    /// `?(…)`: zero or one occurrence of an alternative.
    Opt,
    /// `*(…)`: zero or more occurrences.
    Star,
    /// `+(…)`: one or more occurrences.
    Plus,
    /// `!(…)`: any span that is *not* one alternative.
    Not,
}

impl ExtKind {
    fn from_operator(b: u8) -> Option<Self> {
        match b {
            b'@' => Some(Self::At),
            b'?' => Some(Self::Opt),
            b'*' => Some(Self::Star),
            b'+' => Some(Self::Plus),
            b'!' => Some(Self::Not),
            _ => None,
        }
    }
}

/// A parsed bracket expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Bracket {
    /// Whether the result is inverted (`[!…]` or `[^…]`).
    pub(crate) negate: bool,
    pub(crate) items: Vec<BracketItem>,
}

/// One member of a bracket expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BracketItem {
    /// A literal character.
    Char(Vec<u8>),
    /// An inclusive range between two characters.
    Range(Vec<u8>, Vec<u8>),
    /// A POSIX character class.
    Class(Class),
    /// An unknown `[:name:]` class. Bash matches nothing for these.
    Never,
}

/// The POSIX character classes, defined over ASCII only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    Alpha,
    Digit,
    Space,
    Upper,
    Lower,
    Alnum,
    Punct,
    Blank,
    Xdigit,
    Cntrl,
    Print,
    Graph,
}

impl Class {
    pub(crate) fn from_name(name: &[u8]) -> Option<Self> {
        Some(match name {
            b"alpha" => Self::Alpha,
            b"digit" => Self::Digit,
            b"space" => Self::Space,
            b"upper" => Self::Upper,
            b"lower" => Self::Lower,
            b"alnum" => Self::Alnum,
            b"punct" => Self::Punct,
            b"blank" => Self::Blank,
            b"xdigit" => Self::Xdigit,
            b"cntrl" => Self::Cntrl,
            b"print" => Self::Print,
            b"graph" => Self::Graph,
            _ => return None,
        })
    }

    /// Whether a one-byte unit is in the class.
    ///
    /// Deliberately ASCII: the shell is locale-independent, so `[:alpha:]`
    /// does not match `é` even in a UTF-8 locale. See the crate docs.
    pub(crate) fn matches(self, byte: u8) -> bool {
        match self {
            Self::Alpha => byte.is_ascii_alphabetic(),
            Self::Digit => byte.is_ascii_digit(),
            Self::Space => byte == b' ' || (0x09..=0x0D).contains(&byte),
            Self::Upper => byte.is_ascii_uppercase(),
            Self::Lower => byte.is_ascii_lowercase(),
            Self::Alnum => byte.is_ascii_alphanumeric(),
            Self::Punct => byte.is_ascii_punctuation(),
            Self::Blank => byte == b' ' || byte == b'\t',
            Self::Xdigit => byte.is_ascii_hexdigit(),
            Self::Cntrl => byte.is_ascii_control(),
            Self::Print => (0x20..=0x7E).contains(&byte),
            Self::Graph => (0x21..=0x7E).contains(&byte),
        }
    }
}

/// Maximum extglob nesting. Deeper groups degrade to literal text.
///
/// The parser, the compiler, `reverse` and the `!(…)` evaluator are all
/// recursive in the nesting depth, and a shell pattern can come from a
/// variable, so an unbounded depth would be a stack-overflow vector. 128 is
/// far beyond any real pattern.
const MAX_DEPTH: usize = 128;

/// Parse a pattern into one top-level sequence.
///
/// A `|` at the top level is literal: `case` has already split alternatives.
pub(crate) fn parse(pattern: &[u8], extglob: bool) -> Vec<Elem> {
    let mut parser = Parser {
        p: pattern,
        i: 0,
        extglob,
        broken: false,
    };
    parser.sequence(false, 0).0
}

/// Reverse every sequence in a parsed pattern.
///
/// Used for suffix matching: `reverse(P Q)` is `reverse(Q) reverse(P)`, and
/// every extglob operator commutes with reversal (`!` because complement and
/// reversal commute). Brackets are sets and are left alone.
pub(crate) fn reverse(elems: &[Elem]) -> Vec<Elem> {
    elems.iter().rev().map(reverse_elem).collect()
}

fn reverse_elem(elem: &Elem) -> Elem {
    match elem {
        Elem::Lit(v) => Elem::Lit(v.clone()),
        Elem::Any => Elem::Any,
        Elem::Star => Elem::Star,
        Elem::Bracket(b) => Elem::Bracket(b.clone()),
        Elem::Ext(kind, alts) => Elem::Ext(*kind, alts.iter().map(|a| reverse(a)).collect()),
    }
}

enum Term {
    Pipe,
    Close,
    Eof,
}

struct Parser<'a> {
    p: &'a [u8],
    i: usize,
    extglob: bool,
    /// A group failed to find its closing `)`, so no later group can find one
    /// either. Set once instead of rescanning to end-of-input for every later
    /// operator, which would make a pattern like `@(@(@(…` quadratic.
    broken: bool,
}

impl Parser<'_> {
    /// Parse elements until `|`, `)` (inside a group) or end of input.
    fn sequence(&mut self, in_group: bool, depth: usize) -> (Vec<Elem>, Term) {
        let mut out = Vec::new();
        while let Some(&b) = self.p.get(self.i) {
            if in_group && (b == b'|' || b == b')') {
                let term = if b == b'|' { Term::Pipe } else { Term::Close };
                return (out, term);
            }
            if b == b'\\' {
                self.i += 1;
                out.push(Elem::Lit(self.take_char_or(b'\\')));
                continue;
            }
            if self.extglob
                && !self.broken
                && depth < MAX_DEPTH
                && ExtKind::from_operator(b).is_some()
                && self.p.get(self.i + 1) == Some(&b'(')
            {
                let operator = b;
                let save = self.i;
                self.i += 2;
                match self.group(depth + 1) {
                    Some(alts) => {
                        let kind = ExtKind::from_operator(operator).expect("checked above");
                        out.push(Elem::Ext(kind, alts));
                    }
                    None => {
                        // Unterminated group: the operator and `(` are literal
                        // text. Reparse from the `(` so it stays literal too.
                        self.broken = true;
                        self.i = save + 1;
                        out.push(Elem::Lit(vec![operator]));
                    }
                }
                continue;
            }
            match b {
                b'*' => {
                    out.push(Elem::Star);
                    self.i += 1;
                }
                b'?' => {
                    out.push(Elem::Any);
                    self.i += 1;
                }
                b'[' => match self.bracket() {
                    Some(bracket) => out.push(Elem::Bracket(bracket)),
                    None => {
                        // Bash treats an unterminated `[` as a literal `[`.
                        out.push(Elem::Lit(vec![b'[']));
                        self.i += 1;
                    }
                },
                _ => out.push(Elem::Lit(self.take_char())),
            }
        }
        (out, Term::Eof)
    }

    /// Parse a group body; `self.i` is just past `(` and ends just past `)`.
    fn group(&mut self, depth: usize) -> Option<Vec<Vec<Elem>>> {
        let mut alts = Vec::new();
        loop {
            let (seq, term) = self.sequence(true, depth);
            alts.push(seq);
            match term {
                Term::Pipe => self.i += 1,
                Term::Close => {
                    self.i += 1;
                    return Some(alts);
                }
                Term::Eof => return None,
            }
        }
    }

    /// Take one character (not a backslash escape).
    fn take_char(&mut self) -> Vec<u8> {
        let n = char_len(self.p, self.i);
        let out = self.p[self.i..self.i + n].to_vec();
        self.i += n;
        out
    }

    /// Take one character, or `fallback` at end of input.
    fn take_char_or(&mut self, fallback: u8) -> Vec<u8> {
        if self.i < self.p.len() {
            self.take_char()
        } else {
            vec![fallback]
        }
    }

    /// Try to parse `[…]` at `self.i`; on success advance past it.
    fn bracket(&mut self) -> Option<Bracket> {
        let (bracket, end) = self.bracket_at(self.i)?;
        self.i = end;
        Some(bracket)
    }

    fn bracket_at(&self, start: usize) -> Option<(Bracket, usize)> {
        let p = self.p;
        let mut i = start + 1;
        let mut negate = false;
        if i < p.len() && (p[i] == b'!' || p[i] == b'^') {
            negate = true;
            i += 1;
        }
        let mut items = Vec::new();
        let mut first = true;
        while i < p.len() {
            if p[i] == b']' && !first {
                return Some((Bracket { negate, items }, i + 1));
            }
            if p[i] == b'[' && p.get(i + 1) == Some(&b':') {
                if let Some((class, end)) = class_at(p, i + 2) {
                    items.push(match class {
                        Some(class) => BracketItem::Class(class),
                        None => BracketItem::Never,
                    });
                    i = end;
                    first = false;
                    continue;
                }
            }
            let (ch, next) = bracket_char(p, i)?;
            i = next;
            // A `-` that is not last makes a range; `-` first or last is a
            // literal and is handled by simply parsing it as a character.
            if i < p.len() && p[i] == b'-' && p.get(i + 1).is_some_and(|&b| b != b']') {
                if let Some((end_ch, after)) = bracket_char(p, i + 1) {
                    items.push(BracketItem::Range(ch, end_ch));
                    i = after;
                    first = false;
                    continue;
                }
            }
            items.push(BracketItem::Char(ch));
            first = false;
        }
        None
    }
}

/// Parse the character at `i`, honouring one level of backslash escape.
fn bracket_char(p: &[u8], i: usize) -> Option<(Vec<u8>, usize)> {
    let &b = p.get(i)?;
    if b == b'\\' && i + 1 < p.len() {
        let n = char_len(p, i + 1);
        Some((p[i + 1..i + 1 + n].to_vec(), i + 1 + n))
    } else {
        let n = char_len(p, i);
        Some((p[i..i + n].to_vec(), i + n))
    }
}

/// Parse `name:]` after `[:`; the returned class is `None` for an unknown name.
fn class_at(p: &[u8], start: usize) -> Option<(Option<Class>, usize)> {
    let mut i = start;
    while i + 1 < p.len() {
        if p[i] == b':' && p[i + 1] == b']' {
            return Some((Class::from_name(&p[start..i]), i + 2));
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lits(s: &str) -> Vec<Elem> {
        s.chars()
            .map(|c| Elem::Lit(c.to_string().into_bytes()))
            .collect()
    }

    #[test]
    fn plain_pattern() {
        assert_eq!(parse(b"a*b?", false), {
            let mut v = lits("a");
            v.push(Elem::Star);
            v.extend(lits("b"));
            v.push(Elem::Any);
            v
        });
    }

    #[test]
    fn extglob_off_keeps_groups_literal() {
        assert_eq!(parse(b"@(a|b)", false), lits("@(a|b)"));
        assert_eq!(parse(b"?(a)", false), {
            let mut v = vec![Elem::Any];
            v.extend(lits("(a)"));
            v
        });
    }

    #[test]
    fn extglob_on_parses_groups() {
        let elems = parse(b"@(a|b)", true);
        assert_eq!(
            elems,
            vec![Elem::Ext(ExtKind::At, vec![lits("a"), lits("b")])]
        );
        let elems = parse(b"!(@(a|b))", true);
        assert_eq!(
            elems,
            vec![Elem::Ext(
                ExtKind::Not,
                vec![vec![Elem::Ext(ExtKind::At, vec![lits("a"), lits("b")])]]
            )]
        );
    }

    #[test]
    fn escapes_and_trailing_backslash() {
        assert_eq!(parse(b"\\*", true), vec![Elem::Lit(b"*".to_vec())]);
        assert_eq!(parse(b"a\\", true), {
            let mut v = lits("a");
            v.push(Elem::Lit(b"\\".to_vec()));
            v
        });
        assert_eq!(
            parse(b"@(a\\|b)", true),
            vec![Elem::Ext(ExtKind::At, vec![lits("a|b")])]
        );
    }

    #[test]
    fn unterminated_group_degrades_to_literals() {
        assert_eq!(parse(b"@(a", true), lits("@(a"));
        assert_eq!(parse(b"@(", true), lits("@("));
    }

    #[test]
    fn unterminated_bracket_is_literal() {
        assert_eq!(parse(b"[abc", false), lits("[abc"));
        assert_eq!(parse(b"[]", false), lits("[]"));
    }

    #[test]
    fn brackets() {
        assert_eq!(
            parse(b"[a-c]", false),
            vec![Elem::Bracket(Bracket {
                negate: false,
                items: vec![BracketItem::Range(b"a".to_vec(), b"c".to_vec())],
            })]
        );
        assert_eq!(
            parse(b"[!a]", false),
            vec![Elem::Bracket(Bracket {
                negate: true,
                items: vec![BracketItem::Char(b"a".to_vec())],
            })]
        );
        assert_eq!(
            parse(b"[[:digit:]]", false),
            vec![Elem::Bracket(Bracket {
                negate: false,
                items: vec![BracketItem::Class(Class::Digit)],
            })]
        );
        assert_eq!(
            parse(b"[[:bogus:]]", false),
            vec![Elem::Bracket(Bracket {
                negate: false,
                items: vec![BracketItem::Never],
            })]
        );
    }

    #[test]
    fn reverse_sequences_and_nested_groups() {
        let elems = parse(b"a*b", true);
        assert_eq!(reverse(&elems), {
            let mut v = lits("b");
            v.push(Elem::Star);
            v.extend(lits("a"));
            v
        });
        assert_eq!(
            reverse(&parse(b"@(ab|cd)", true)),
            vec![Elem::Ext(ExtKind::At, vec![lits("ba"), lits("dc")])]
        );
        assert_eq!(
            reverse(&parse(b"!(ab)", true)),
            vec![Elem::Ext(ExtKind::Not, vec![lits("ba")])]
        );
    }
}
