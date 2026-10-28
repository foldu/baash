//! Integer arithmetic for `$((…))`, `((…))`, `for ((…))` and `[[ -eq … ]]`.
//!
//! The operator set and the precedence ladder are C's, which is what POSIX
//! XCU 2.6.4 points at and what bash implements. Two documented Tier C
//! behaviours (DESIGN.md §3.3) live here:
//!
//! * **overflow wraps at 64 bits** — not an error, not arbitrary precision;
//! * **a bare name that is unset or non-numeric reads as 0** — `$(( x ))` is
//!   `0`, so `$((n + 1))` works before `n` exists.
//!
//! Literals are C-style: `0x1f` hexadecimal, `010` octal, else decimal.
//! Whitespace is insignificant. `&&` and `||` short-circuit, so
//! `((n != 0 && 10 / n > 1))` does not divide by zero.
//!
//! Assignments (`x=1`, `x+=2`, `x++`, `++x`) are supported because `((i++))`
//! and `for ((i = 0; …; i++))` are how shell loops count. They write the
//! *decimal text* of the result through [`IntVars::set`], which is the
//! contract `bsh-runtime` records ("integer attributes store raw bytes;
//! arithmetic happens before `assign`").
//!
//! A variable whose value is not a number is *evaluated as an expression*,
//! which is how bash makes `a=(1+2+3); $((a[0]))` six and
//! `i='0+1'; [[ -v a[i] ]]` look at element one. Recursion is bounded.
//!
//! Array elements are part of the lvalue grammar: `a[1]`, `a[i+1]`,
//! `(( a[i] += 1 ))` and `$(( a[i]++ ))`. The subscript is a full arithmetic
//! expression (`a[a[0]]` works), and an indexed array's element reads as 0
//! when it is unset — bash's "empty cell is zero" rule, which is what makes
//! `(( a[99] += 1 ))` a sparse increment rather than an error.
//!
//! `**` is right-associative and binds after the unary signs (`-2 ** 2` is
//! four in bash), and the comma operator evaluates its left side first.
//!
//! Deferred: `?:`, `base#digits` literals, and recursive evaluation of a
//! variable whose value is itself an expression.

use std::collections::HashMap;

/// The variable view arithmetic needs.
pub trait IntVars {
    /// Read a name as an integer. Unset or non-numeric reads as 0.
    fn get(&mut self, name: &[u8]) -> i64;

    /// Write the decimal text of `value` back to `name`.
    fn set(&mut self, name: &[u8], value: i64) -> Result<(), String>;

    /// Read one indexed element. Unset or non-numeric reads as 0.
    ///
    /// The default encodes the element as `name[index]` in the same variable
    /// namespace, which is enough for an in-memory test map; a real shell
    /// overrides it.
    fn get_index(&mut self, name: &[u8], index: i64) -> i64 {
        self.get(&element_key(name, index))
    }

    /// Write one indexed element. Negative indices have already been resolved
    /// by the array store, which is the only thing that knows the length.
    fn set_index(&mut self, name: &[u8], index: i64, value: i64) -> Result<(), String> {
        self.set(&element_key(name, index), value)
    }

    /// The raw bytes of `name`'s value, for recursive expression evaluation.
    ///
    /// `None` (the default) means the source cannot supply them; a shell that
    /// can should override it.
    fn get_text(&mut self, name: &[u8]) -> Option<Vec<u8>> {
        let _ = name;
        None
    }

    /// The raw bytes of one indexed element.
    fn get_index_text(&mut self, name: &[u8], index: i64) -> Option<Vec<u8>> {
        self.get_text(&element_key(name, index))
    }
}

/// How deep recursive variable-value evaluation may go before bash reports a
/// recursion error. bash's limit is in the same range.
const MAX_RECURSION: usize = 32;

/// `name[index]` as a byte string, for the default [`IntVars`] element access.
fn element_key(name: &[u8], index: i64) -> Vec<u8> {
    let mut key = name.to_vec();
    key.push(b'[');
    key.extend_from_slice(index.to_string().as_bytes());
    key.push(b']');
    key
}

/// Evaluate an arithmetic expression.
pub fn eval(src: &[u8], vars: &mut impl IntVars) -> Result<i64, String> {
    eval_depth(src, vars, 0)
}

fn eval_depth(src: &[u8], vars: &mut impl IntVars, depth: usize) -> Result<i64, String> {
    if depth > MAX_RECURSION {
        return Err("expression recursion level exceeded".to_owned());
    }
    let mut parser = Parser {
        src,
        pos: 0,
        dead: false,
        vars,
        depth,
    };
    let value = parser.expression()?;
    parser.skip_space();
    if parser.pos < parser.src.len() {
        return Err(format!(
            "syntax error near `{}`",
            String::from_utf8_lossy(&parser.src[parser.pos.min(parser.src.len())..])
        ));
    }
    Ok(value)
}

/// Parse a C-style integer literal with an optional sign, as a variable value
/// or a `test` operand. `None` when the bytes are not a number at all.
pub fn parse_int(bytes: &[u8]) -> Option<i64> {
    let text = bytes.trim_ascii();
    let (negative, digits) = match text.split_first() {
        Some((&b'-', rest)) => (true, rest),
        Some((&b'+', rest)) => (false, rest),
        _ => (false, text),
    };
    let (radix, digits) = if let Some(hex) = digits
        .strip_prefix(b"0x")
        .or_else(|| digits.strip_prefix(b"0X"))
    {
        (16, hex)
    } else if digits.len() > 1 && digits[0] == b'0' {
        (8, &digits[1..])
    } else {
        (10, digits)
    };
    if digits.is_empty() {
        return None;
    }
    let mut value: i64 = 0;
    for &byte in digits {
        let digit = (byte as char).to_digit(radix)?;
        value = value.wrapping_mul(i64::from(radix));
        value = value.wrapping_add(i64::from(digit));
    }
    Some(if negative {
        value.wrapping_neg()
    } else {
        value
    })
}

/// Assignment spellings, longest first. `=` is listed after the shifts so a
/// prefix search never mistakes `<<=` for `<<` plus `=`.
const ASSIGN_OPS: &[&str] = &[
    "<<=", ">>=", "=", "+=", "-=", "*=", "/=", "%=", "&=", "^=", "|=",
];

struct Parser<'a, V: IntVars> {
    src: &'a [u8],
    pos: usize,
    /// Inside a short-circuited operand: parse, but do not evaluate.
    dead: bool,
    vars: &'a mut V,
    /// Evaluation nesting, for recursive variable values.
    depth: usize,
}

/// Binary operators with C precedence, longest spelling first so `<<` beats
/// `<` and `&&` beats `&`.
const BINOPS: &[(&[u8], u8)] = &[
    (b"<<", 9),
    (b">>", 9),
    (b"<=", 8),
    (b">=", 8),
    (b"==", 7),
    (b"!=", 7),
    (b"&&", 2),
    (b"||", 1),
    (b"**", 12),
    (b"+", 10),
    (b"-", 10),
    (b"*", 11),
    (b"/", 11),
    (b"%", 11),
    (b"<", 8),
    (b">", 8),
    (b"&", 6),
    (b"^", 5),
    (b"|", 4),
];

impl<'a, V: IntVars> Parser<'a, V> {
    fn skip_space(&mut self) {
        while self.src.get(self.pos).is_some_and(u8::is_ascii_whitespace) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, token: &[u8]) -> bool {
        self.skip_space();
        if self.src[self.pos..].starts_with(token) {
            self.pos += token.len();
            return true;
        }
        false
    }

    /// The comma operator: lowest precedence, left-associative, both sides
    /// evaluated (the left only for its side effects).
    fn expression(&mut self) -> Result<i64, String> {
        let mut value = self.assignment()?;
        while self.eat(b",") {
            value = self.assignment()?;
        }
        Ok(value)
    }

    /// `name = expr`, `a[i] += expr`, and the compound assignment operators.
    fn assignment(&mut self) -> Result<i64, String> {
        self.skip_space();
        if let Some(name) = self.peek_name() {
            self.pos += name.len();
            self.skip_space();
            let subscript = self.take_subscript()?;
            self.skip_space();
            if let Some(token) = self.assign_op() {
                self.pos += token.len();
                if self.dead {
                    self.assignment()?;
                    return Ok(0);
                }
                // bash reads a compound assignment's current value *before*
                // evaluating the right-hand side: with `a=3`, `a += a++` is
                // 6 (3 + 3), not 7 (4 + 3). A plain `=` reads nothing, so
                // `a='not a number'; a=5` does not fail.
                let current = if token == "=" {
                    0
                } else {
                    self.read_lvalue(name, subscript)
                };
                let value = self.assignment()?;
                let value = assign_value(token, current, value)?;
                self.write_lvalue(name, subscript, value)?;
                return Ok(value);
            }
            // No assignment operator after all: the name (and any subscript)
            // is an rvalue. Do not backtrack and re-parse it — evaluating a
            // subscript twice would double its side effects.
            let current = self.read_lvalue(name, subscript);
            let current = self.take_postfix(current, name, subscript)?;
            return self.binary_from(current, 1);
        }
        self.binary(1)
    }

    fn assign_op(&self) -> Option<&'static str> {
        let rest = &self.src[self.pos..];
        ASSIGN_OPS.iter().copied().find(|token| {
            rest.starts_with(token.as_bytes()) && !(rest.starts_with(b"==") && *token == "=")
        })
    }

    /// Parse `[expr]` when present, returning the evaluated subscript.
    fn take_subscript(&mut self) -> Result<Option<i64>, String> {
        if !self.src[self.pos..].starts_with(b"[") {
            return Ok(None);
        }
        self.pos += 1;
        let index = self.assignment()?;
        self.skip_space();
        if !self.eat(b"]") {
            return Err("missing `]`".to_owned());
        }
        Ok(Some(index))
    }

    /// Consume a postfix `++`/`--` on an lvalue, returning the value before
    /// the increment (its value in the surrounding expression).
    fn take_postfix(
        &mut self,
        current: i64,
        name: &[u8],
        subscript: Option<i64>,
    ) -> Result<i64, String> {
        self.skip_space();
        for (token, step) in [(b"++".as_slice(), 1i64), (b"--", -1)] {
            if self.src[self.pos..].starts_with(token) {
                self.pos += 2;
                let value = current.wrapping_add(step);
                if !self.dead {
                    self.write_lvalue(name, subscript, value)?;
                }
                return Ok(current);
            }
        }
        Ok(current)
    }

    fn read_lvalue(&mut self, name: &[u8], subscript: Option<i64>) -> i64 {
        if self.dead {
            return 0;
        }
        let (value, text) = match subscript {
            Some(index) => (
                self.vars.get_index(name, index),
                self.vars.get_index_text(name, index),
            ),
            None => (self.vars.get(name), self.vars.get_text(name)),
        };
        // A non-numeric value is itself an expression: `a=(1+2+3);
        // $((a[0]))` is 6, and `i=0+1; $((i))` is 1.
        if let Some(text) = text {
            if !text.is_empty() && parse_int(&text).is_none() {
                if let Ok(nested) = eval_depth(&text, self.vars, self.depth + 1) {
                    return nested;
                }
            }
        }
        value
    }

    fn write_lvalue(
        &mut self,
        name: &[u8],
        subscript: Option<i64>,
        value: i64,
    ) -> Result<(), String> {
        match subscript {
            Some(index) => self.vars.set_index(name, index, value),
            None => self.vars.set(name, value),
        }
    }

    /// Precedence climbing, with short-circuiting `&&`/`||`.
    fn binary(&mut self, minimum: u8) -> Result<i64, String> {
        let lhs = self.unary()?;
        self.binary_from(lhs, minimum)
    }

    /// Continue a precedence climb from an already-parsed left operand.
    fn binary_from(&mut self, mut lhs: i64, minimum: u8) -> Result<i64, String> {
        loop {
            self.skip_space();
            let Some((operator, precedence)) = self.peek_binop(minimum) else {
                break;
            };
            self.pos += operator.len();
            let dead = self.dead;
            if operator == b"&&" {
                self.dead = dead || lhs == 0;
            } else if operator == b"||" {
                self.dead = dead || lhs != 0;
            }
            // `**` is right-associative; everything else climbs to `prec + 1`.
            let next = if operator == b"**" {
                precedence
            } else {
                precedence + 1
            };
            let rhs = self.binary(next);
            self.dead = dead;
            lhs = self.apply(operator, lhs, rhs?)?;
        }
        Ok(lhs)
    }

    fn peek_binop(&self, minimum: u8) -> Option<(&'static [u8], u8)> {
        // Longest spelling first: at `||` the single `|` must never win just
        // because the `||` is below the caller's minimum precedence.
        let rest = &self.src[self.pos..];
        let &(token, precedence) = BINOPS
            .iter()
            .filter(|(token, _)| rest.starts_with(token))
            .max_by_key(|(token, _)| token.len())?;
        (precedence >= minimum).then_some((token, precedence))
    }

    fn apply(&self, operator: &[u8], lhs: i64, rhs: i64) -> Result<i64, String> {
        if self.dead {
            return Ok(0);
        }
        Ok(match operator {
            b"+" => lhs.wrapping_add(rhs),
            b"-" => lhs.wrapping_sub(rhs),
            b"*" => lhs.wrapping_mul(rhs),
            b"/" => divide(lhs, rhs)?,
            b"%" => remainder(lhs, rhs)?,
            b"**" => power(lhs, rhs)?,
            b"<<" => lhs.wrapping_shl(rhs as u32),
            b">>" => lhs.wrapping_shr(rhs as u32),
            b"&" => lhs & rhs,
            b"|" => lhs | rhs,
            b"^" => lhs ^ rhs,
            b"==" => i64::from(lhs == rhs),
            b"!=" => i64::from(lhs != rhs),
            b"<" => i64::from(lhs < rhs),
            b"<=" => i64::from(lhs <= rhs),
            b">" => i64::from(lhs > rhs),
            b">=" => i64::from(lhs >= rhs),
            b"&&" => i64::from(lhs != 0 && rhs != 0),
            b"||" => i64::from(lhs != 0 || rhs != 0),
            _ => return Err("unknown operator".to_owned()),
        })
    }

    fn unary(&mut self) -> Result<i64, String> {
        self.skip_space();
        let rest = &self.src[self.pos..];
        for (token, kind) in [
            (b"++".as_slice(), 1u8),
            (b"--", 2),
            (b"!", 3),
            (b"~", 4),
            (b"-", 5),
            (b"+", 6),
        ] {
            if !rest.starts_with(token) {
                continue;
            }
            // `!=`, `-=`, `&&`-style operators are not unary prefixes; the
            // two-character forms are already excluded by the loop above.
            if matches!(token, b"!" | b"-" | b"+") && rest[1..].starts_with(b"=") {
                continue;
            }
            // In a run of three or more signs bash takes the first one as an
            // ordinary unary sign: `---b` is `-(--b)`, and `+++b` is
            // `+(++b)`. Skipping the two-character token here lets the
            // one-character arm below do exactly that.
            if kind <= 2 && rest.get(2) == rest.first() {
                continue;
            }
            self.pos += token.len();
            if kind <= 2 {
                let step = if kind == 1 { 1 } else { -1 };
                // bash accepts prefix `++`/`--` on a non-lvalue and returns
                // the operand unchanged (`++(5)` is 5, `++!0` is 1).
                if self.peek_name().is_none() {
                    return self.unary();
                }
                let name = self.take_name()?;
                let subscript = self.take_subscript()?;
                let value = self.read_lvalue(&name, subscript).wrapping_add(step);
                if !self.dead {
                    self.write_lvalue(&name, subscript, value)?;
                }
                return Ok(value);
            }
            let value = self.unary()?;
            return Ok(match token {
                b"!" => i64::from(value == 0),
                b"~" => !value,
                b"-" => value.wrapping_neg(),
                _ => value,
            });
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<i64, String> {
        self.skip_space();
        if self.eat(b"(") {
            let value = self.expression()?;
            if !self.eat(b")") {
                return Err("missing `)`".to_owned());
            }
            return Ok(value);
        }
        if self.src[self.pos..]
            .first()
            .is_some_and(|b| b.is_ascii_digit())
        {
            let start = self.pos;
            while self
                .src
                .get(self.pos)
                .is_some_and(|b| b.is_ascii_alphanumeric())
            {
                self.pos += 1;
            }
            return parse_int(&self.src[start..self.pos])
                .ok_or_else(|| "invalid number".to_owned());
        }
        // bash accepts `$name` and `${name}` inside arithmetic, and `$1`-
        // style positional references are ordinary names there.
        if self.eat(b"$") {
            if self.eat(b"{") {
                let name = self.take_name()?;
                // `${a[0]}` inside arithmetic is an element read.
                let subscript = self.take_subscript()?;
                if !self.eat(b"}") {
                    return Err("missing `}`".to_owned());
                }
                return Ok(self.read_lvalue(&name, subscript));
            }
            // `$1`-style positional references are ordinary names here.
            if self.src[self.pos..]
                .first()
                .is_some_and(|b| b.is_ascii_digit())
            {
                let start = self.pos;
                while self.src.get(self.pos).is_some_and(|b| b.is_ascii_digit()) {
                    self.pos += 1;
                }
                return Ok(self.read(&self.src[start..self.pos]));
            }
        }
        let name = self.take_name()?;
        let subscript = self.take_subscript()?;
        let current = self.read_lvalue(&name, subscript);
        self.take_postfix(current, &name, subscript)
    }

    fn peek_name(&self) -> Option<&'a [u8]> {
        let rest = &self.src[self.pos..];
        let len = name_len(rest)?;
        Some(&rest[..len])
    }

    fn take_name(&mut self) -> Result<Vec<u8>, String> {
        self.skip_space();
        let rest = &self.src[self.pos..];
        let Some(len) = name_len(rest) else {
            return Err("expected a name".to_owned());
        };
        self.pos += len;
        Ok(rest[..len].to_vec())
    }

    fn read(&mut self, name: &[u8]) -> i64 {
        self.read_lvalue(name, None)
    }
}

fn name_len(rest: &[u8]) -> Option<usize> {
    let first = *rest.first()?;
    if first != b'_' && !first.is_ascii_alphabetic() {
        return None;
    }
    Some(
        rest.iter()
            .take_while(|b| **b == b'_' || b.is_ascii_alphanumeric())
            .count(),
    )
}

fn assign_value(operator: &str, current: i64, value: i64) -> Result<i64, String> {
    Ok(match operator {
        "=" => value,
        "+=" => current.wrapping_add(value),
        "-=" => current.wrapping_sub(value),
        "*=" => current.wrapping_mul(value),
        "/=" => divide(current, value)?,
        "%=" => remainder(current, value)?,
        "<<=" => current.wrapping_shl(value as u32),
        ">>=" => current.wrapping_shr(value as u32),
        "&=" => current & value,
        "^=" => current ^ value,
        "|=" => current | value,
        _ => return Err("unknown assignment operator".to_owned()),
    })
}

fn power(base: i64, exponent: i64) -> Result<i64, String> {
    if exponent < 0 {
        return Err("exponent less than 0".to_owned());
    }
    let mut result: i64 = 1;
    let mut factor = base;
    let mut remaining = exponent as u64;
    while remaining > 0 {
        if remaining & 1 == 1 {
            result = result.wrapping_mul(factor);
        }
        remaining >>= 1;
        if remaining > 0 {
            factor = factor.wrapping_mul(factor);
        }
    }
    Ok(result)
}

fn divide(lhs: i64, rhs: i64) -> Result<i64, String> {
    if rhs == 0 {
        return Err("division by 0".to_owned());
    }
    Ok(lhs.wrapping_div(rhs))
}

fn remainder(lhs: i64, rhs: i64) -> Result<i64, String> {
    if rhs == 0 {
        return Err("division by 0".to_owned());
    }
    Ok(lhs.wrapping_rem(rhs))
}

/// An [`IntVars`] view over an in-memory map, used by tests.
#[derive(Default)]
pub struct MapVars {
    values: HashMap<Vec<u8>, i64>,
}

impl IntVars for MapVars {
    fn get(&mut self, name: &[u8]) -> i64 {
        self.values.get(name).copied().unwrap_or(0)
    }

    fn set(&mut self, name: &[u8], value: i64) -> Result<(), String> {
        self.values.insert(name.to_vec(), value);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(src: &str) -> i64 {
        eval(src.as_bytes(), &mut MapVars::default()).unwrap()
    }

    fn fails(src: &str) -> String {
        eval(src.as_bytes(), &mut MapVars::default()).unwrap_err()
    }

    #[test]
    fn precedence_matches_c() {
        assert_eq!(value("1 + 2 * 3"), 7);
        assert_eq!(value("(1 + 2) * 3"), 9);
        assert_eq!(value("1 << 2 + 1"), 8);
        assert_eq!(value("1 | 2 & 6"), 3);
        assert_eq!(value("1 == 1 & 2"), 0);
        assert_eq!(value("7 / 2"), 3, "integer division truncates");
        assert_eq!(value("7 % 3"), 1);
        assert_eq!(value("-7 / 2"), -3);
    }

    #[test]
    fn comparisons_and_logic_yield_zero_or_one() {
        assert_eq!(value("3 > 2"), 1);
        assert_eq!(value("3 < 2"), 0);
        assert_eq!(value("3 == 3 && 2 != 2"), 0);
        assert_eq!(value("0 || 5"), 1);
        assert_eq!(value("!0"), 1);
        assert_eq!(value("!5"), 0);
        assert_eq!(value("3 >= 3 != 0"), 1);
    }

    #[test]
    fn short_circuit_avoids_the_division() {
        assert_eq!(value("0 && 1 / 0"), 0);
        assert_eq!(value("1 || 1 / 0"), 1);
    }

    #[test]
    fn overflow_wraps_at_64_bits() {
        assert_eq!(value("9223372036854775807 + 1"), i64::MIN);
        assert_eq!(value("-9223372036854775808 - 1"), i64::MAX);
    }

    #[test]
    fn literals_are_c_style() {
        assert_eq!(value("0x10"), 16);
        assert_eq!(value("010"), 8);
        assert_eq!(value("+4"), 4);
    }

    #[test]
    fn unset_names_are_zero_and_assignments_persist() {
        let mut vars = MapVars::default();
        assert_eq!(eval(b"n + 1", &mut vars), Ok(1));
        assert_eq!(eval(b"n = 4", &mut vars), Ok(4));
        assert_eq!(eval(b"n += 3", &mut vars), Ok(7));
        assert_eq!(eval(b"n++", &mut vars), Ok(7));
        assert_eq!(eval(b"n", &mut vars), Ok(8));
        assert_eq!(eval(b"++n", &mut vars), Ok(9));
        assert_eq!(eval(b"n--", &mut vars), Ok(9));
        assert_eq!(eval(b"n", &mut vars), Ok(8));
    }

    #[test]
    fn dollar_names_and_positional_like_names() {
        let mut vars = MapVars::default();
        vars.set(b"1", 3).unwrap();
        vars.set(b"x", 4).unwrap();
        assert_eq!(eval(b" $1 - 1 ", &mut vars), Ok(2));
        assert_eq!(eval(b" $1 ", &mut vars), Ok(3));
        assert_eq!(eval(b" $x + $1 ", &mut vars), Ok(7));
        assert_eq!(eval(b" ${x} * 2 ", &mut vars), Ok(8));
    }

    #[test]
    fn division_by_zero_is_an_error() {
        assert!(fails("1 / 0").contains("division by 0"));
        assert!(fails("1 % 0").contains("division by 0"));
    }

    #[test]
    fn syntax_errors_are_reported() {
        assert!(fails("1 +").contains("expected a name"));
        assert!(fails("(1").contains("missing"));
        assert!(fails("1 2").contains("syntax error"));
    }

    #[test]
    fn parse_int_accepts_signed_and_unsigned_forms() {
        assert_eq!(parse_int(b"42"), Some(42));
        assert_eq!(parse_int(b" -7 "), Some(-7));
        assert_eq!(parse_int(b"0xff"), Some(255));
        assert_eq!(parse_int(b"nope"), None);
        assert_eq!(parse_int(b""), None);
    }

    #[test]
    fn exponentiation_is_right_associative_and_binds_after_unary() {
        assert_eq!(value("2**10"), 1024);
        assert_eq!(value("2 ** 3 ** 2"), 512);
        assert_eq!(value("3 ** 0"), 1);
        assert_eq!(value("0 ** 0"), 1);
        assert_eq!(value("-2 ** 2"), 4, "unary minus binds tighter than **");
        assert_eq!(value("-3 ** 2"), 9);
        assert_eq!(value("!2 ** 2"), 0);
        assert_eq!(value("~2 ** 2"), 9);
        assert_eq!(value("2 * 3 ** 2"), 18);
        assert_eq!(value("2 ** 2 * 3"), 12);
        assert_eq!(value("2 ** 63"), i64::MIN, "exponentiation wraps");
        assert_eq!(value("2 ** 64"), 0);
        assert_eq!(value("(-2) ** 3"), -8);
        assert_eq!(value("2 ** -0"), 1);
    }

    #[test]
    fn negative_exponent_is_an_error() {
        assert!(fails("2 ** -1").contains("exponent less than 0"));
        assert!(fails("0 ** -1").contains("exponent less than 0"));
    }

    #[test]
    fn comma_evaluates_left_to_right() {
        assert_eq!(value("1, 2, 3"), 3);
        assert_eq!(value("(1, 2)"), 2);
        let mut vars = MapVars::default();
        assert_eq!(eval(b"a = 1, b = 2, a + b", &mut vars), Ok(3));
        assert_eq!(eval(b"a", &mut vars), Ok(1));
        assert_eq!(eval(b"b", &mut vars), Ok(2));
        assert_eq!(eval(b"x = 5, x++", &mut vars), Ok(5));
        assert_eq!(eval(b"x", &mut vars), Ok(6));
        assert_eq!(eval(b"x = 5, ++x", &mut vars), Ok(6));
    }

    #[test]
    fn every_assignment_operator_writes_back() {
        let mut vars = MapVars::default();
        let cases: &[(&str, i64, &str)] = &[
            ("v = 6", 6, "v"),
            ("v += 4", 10, "v"),
            ("v -= 3", 7, "v"),
            ("v *= 3", 21, "v"),
            ("v /= 2", 10, "v"),
            ("v %= 3", 1, "v"),
            ("v = 1", 1, "v"),
            ("v <<= 5", 32, "v"),
            ("v >>= 2", 8, "v"),
            ("v = 6", 6, "v"),
            ("v &= 3", 2, "v"),
            ("v ^= 7", 5, "v"),
            ("v |= 8", 13, "v"),
        ];
        for (src, expected, name) in cases {
            assert_eq!(eval(src.as_bytes(), &mut vars), Ok(*expected), "{src}");
            assert_eq!(eval(name.as_bytes(), &mut vars), Ok(*expected), "{src}");
        }
        assert_eq!(eval(b"a = b = 2", &mut vars), Ok(2));
        assert_eq!(eval(b"a", &mut vars), Ok(2));
        assert_eq!(eval(b"b", &mut vars), Ok(2));
    }

    #[test]
    fn compound_assignment_reads_before_the_right_hand_side() {
        let mut vars = MapVars::default();
        assert_eq!(eval(b"a = 3", &mut vars), Ok(3));
        assert_eq!(eval(b"a += a++", &mut vars), Ok(6));
        assert_eq!(eval(b"a", &mut vars), Ok(6));
        assert_eq!(eval(b"a = 3", &mut vars), Ok(3));
        assert_eq!(eval(b"a += ++a", &mut vars), Ok(7));
        assert_eq!(eval(b"a = 3", &mut vars), Ok(3));
        assert_eq!(eval(b"a += (a = 5)", &mut vars), Ok(8));
        assert_eq!(eval(b"a", &mut vars), Ok(8));
    }

    #[test]
    fn prefix_increment_on_a_non_lvalue_is_a_no_op() {
        assert_eq!(value("++5"), 5);
        assert_eq!(value("--5"), 5);
        assert_eq!(value("++(5) + 1"), 6);
        assert_eq!(value("++!0"), 1);
        assert_eq!(value("++~5"), -6);
        assert_eq!(value("+++5"), 5, "`+` then `++`");
        let mut vars = MapVars::default();
        assert_eq!(eval(b"b = 2, ---b", &mut vars), Ok(-1));
        assert_eq!(eval(b"b", &mut vars), Ok(1));
        assert_eq!(eval(b"b = 2, ----b", &mut vars), Ok(1));
        assert_eq!(eval(b"b", &mut vars), Ok(1));
        assert_eq!(eval(b"b = 2, +++b", &mut vars), Ok(3));
        assert_eq!(eval(b"b", &mut vars), Ok(3));
    }

    #[test]
    fn logical_chains_do_not_confuse_single_and_double_operators() {
        assert_eq!(value("1 && 0 || 1"), 1);
        assert_eq!(value("0 && 0 || 1"), 1);
        assert_eq!(value("1 && 1 || 0"), 1);
        assert_eq!(value("0 || 1 && 0"), 0);
        assert_eq!(value("0 && 1 | 1"), 0);
        assert_eq!(value("1 && 2 | 1"), 1);
    }
}
