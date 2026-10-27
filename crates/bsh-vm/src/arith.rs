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
//! Deferred: `**`, the comma operator, `?:`, `base#digits` literals, array
//! subscripts, and recursive evaluation of a variable whose value is itself an
//! expression.

use std::collections::HashMap;

/// The variable view arithmetic needs.
pub trait IntVars {
    /// Read a name as an integer. Unset or non-numeric reads as 0.
    fn get(&mut self, name: &[u8]) -> i64;

    /// Write the decimal text of `value` back to `name`.
    fn set(&mut self, name: &[u8], value: i64) -> Result<(), String>;
}

/// Evaluate an arithmetic expression.
pub fn eval(src: &[u8], vars: &mut impl IntVars) -> Result<i64, String> {
    let mut parser = Parser {
        src,
        pos: 0,
        dead: false,
        vars,
    };
    let value = parser.assignment()?;
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

struct Parser<'a, V: IntVars> {
    src: &'a [u8],
    pos: usize,
    /// Inside a short-circuited operand: parse, but do not evaluate.
    dead: bool,
    vars: &'a mut V,
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

    /// `name = expr` and the compound assignment operators.
    fn assignment(&mut self) -> Result<i64, String> {
        let start = self.pos;
        if let Some(name) = self.peek_name() {
            self.pos += name.len();
            self.skip_space();
            const ASSIGN: &[&str] = &[
                "<<=", ">>=", "=", "+=", "-=", "*=", "/=", "%=", "&=", "^=", "|=",
            ];
            let found = ASSIGN.iter().find(|token| {
                self.src[self.pos..].starts_with(token.as_bytes())
                    && !(token.starts_with('=') && self.src[self.pos..].starts_with(b"=="))
            });
            if let Some(&token) = found {
                self.pos += token.len();
                let value = self.assignment()?;
                let current = self.read(name);
                let value = if self.dead {
                    0
                } else {
                    match token {
                        "=" => value,
                        "+=" => current.wrapping_add(value),
                        "-=" => current.wrapping_sub(value),
                        "*=" => current.wrapping_mul(value),
                        "/=" => divide(current, value)?,
                        "%=" => remainder(current, value)?,
                        "&=" => current & value,
                        "^=" => current ^ value,
                        "|=" => current | value,
                        "<<" => current.wrapping_shl(value as u32),
                        ">>" => current.wrapping_shr(value as u32),
                        _ => value,
                    }
                };
                if !self.dead {
                    self.vars.set(name, value)?;
                }
                return Ok(value);
            }
            self.pos = start;
        }
        self.binary(1)
    }

    /// Precedence climbing, with short-circuiting `&&`/`||`.
    fn binary(&mut self, minimum: u8) -> Result<i64, String> {
        let mut lhs = self.unary()?;
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
            let rhs = self.binary(precedence + 1);
            self.dead = dead;
            lhs = self.apply(operator, lhs, rhs?)?;
        }
        Ok(lhs)
    }

    fn peek_binop(&self, minimum: u8) -> Option<(&'static [u8], u8)> {
        BINOPS.iter().find_map(|&(token, precedence)| {
            (precedence >= minimum && self.src[self.pos..].starts_with(token))
                .then_some((token, precedence))
        })
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
            self.pos += token.len();
            if kind <= 2 {
                let name = self.take_name()?;
                let step = if kind == 1 { 1 } else { -1 };
                let value = self.read(&name).wrapping_add(step);
                if !self.dead {
                    self.vars.set(&name, value)?;
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
            let value = self.assignment()?;
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
                if !self.eat(b"}") {
                    return Err("missing `}`".to_owned());
                }
                return Ok(self.read(&name));
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
        let current = self.read(&name);
        self.skip_space();
        let rest = &self.src[self.pos..];
        for (token, step) in [(b"++".as_slice(), 1i64), (b"--", -1)] {
            if rest.starts_with(token) {
                self.pos += 2;
                let value = current.wrapping_add(step);
                if !self.dead {
                    self.vars.set(&name, value)?;
                }
                return Ok(current);
            }
        }
        Ok(current)
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
        if self.dead { 0 } else { self.vars.get(name) }
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
}
