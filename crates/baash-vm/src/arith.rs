//! Integer arithmetic for `$((…))`, `((…))`, `for ((…))` and `[[ -eq … ]]`.
//!
//! The operator set and the precedence ladder are C's, which is what POSIX
//! XCU 2.6.4 points at, plus bash's `**`, `?:`, `,` and `base#digits`
//! literals. Two documented Tier C behaviours (DESIGN.md §3.3) live here:
//!
//! * **overflow wraps at 64 bits** — not an error, not arbitrary precision;
//! * **a bare name that is unset or non-numeric reads as 0** — `$(( x ))` is
//!   `0`, so `$((n + 1))` works before `n` exists.
//!
//! Precedence, tightest first (bash's table; `-` and `+` bind tighter than
//! `**`, so `-3 ** 2` is `9`, and `**` is right-associative, so `2 ** 3 ** 2`
//! is `512`):
//!
//! ```text
//! ++ -- (postfix)   ++ -- (prefix)   - + (unary)   ! ~
//! **
//! * / %
//! + -
//! << >>
//! < <= > >=
//! == !=
//! &
//! ^
//! |
//! &&
//! ||
//! ?:
//! = += -= *= /= %= <<= >>= &= ^= |=
//! ,
//! ```
//!
//! `&&`, `||` and `?:` short-circuit: the untaken operand is parsed but not
//! evaluated, so `(( 1 || (x = 22) ))` leaves `x` alone and
//! `(( n != 0 ? 10 / n : 0 ))` cannot divide by zero.
//!
//! Literals are C-style (`0x1f` hexadecimal, `010` octal, else decimal) plus
//! bash's `base#digits` form for bases 2 through 64: digits above nine are
//! `a`–`z` (10–35), `A`–`Z` (36–61 when the base is above 36, case-folded
//! below it), `@` (62) and `_` (63). `10#011` is eleven, not nine.
//!
//! Assignments (`x=1`, `x+=2`, `a[i] <<= 1`, `x++`, `++x`) are supported
//! because `((i++))` and `for ((i = 0; …; i++))` are how shell loops count.
//! They write the *decimal text* of the result through [`IntVars::set`],
//! which is the contract `baash-runtime` records ("integer attributes store raw
//! bytes; arithmetic happens before `assign`").
//!
//! A variable whose value is not a number is *evaluated as an expression*,
//! which is how bash makes `a=(1+2+3); $((a[0]))` six and
//! `i='0+1'; [[ -v a[i] ]]` look at element one. Errors inside that nested
//! expression propagate, so `s='12 34'; $((s[0]))` fails as it does in bash.
//! Recursion is bounded.
//!
//! The arithmetic text is word-expanded first, the way bash expands
//! `$((…))` before evaluating it: `$name` and `${name}` splice the variable's
//! *text* (`x='1 + 2'; $(( $x * 3 ))` is seven, while bare `x * 3` is nine),
//! `${a[i]}` splices an element, and double quotes are removed (`"3" + 1` is
//! four). Single quotes are not removed and remain a syntax error, as in
//! bash. Full parameter expansion (`${x:-1}`, `$(…)`, backticks) is handled
//! at the word layer, not here.
//!
//! Array elements are part of the lvalue grammar: `a[1]`, `a[i+1]`,
//! `(( a[i] += 1 ))` and `$(( a[i]++ ))`. The subscript is a full arithmetic
//! expression (`a[a[0]]` works), and an indexed array's element reads as 0
//! when it is unset — bash's "empty cell is zero" rule, which is what makes
//! `(( a[99] += 1 ))` a sparse increment rather than an error.

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

    /// The raw bytes of `name`'s value, for `$name` splicing and recursive
    /// expression evaluation.
    ///
    /// `None` (the default) means the source cannot supply them; a shell that
    /// can should override it. An unset variable splices as the empty string.
    fn get_text(&mut self, name: &[u8]) -> Option<Vec<u8>> {
        let _ = name;
        None
    }

    /// The raw bytes of one indexed element.
    fn get_index_text(&mut self, name: &[u8], index: i64) -> Option<Vec<u8>> {
        self.get_text(&element_key(name, index))
    }
}

/// How deep recursive variable-value evaluation may go before reporting a
/// recursion error. bash allows a chain of about 1020 (`loop='i<n&&…,loop'`
/// works up to n = 1020 and fails at 1021), so this matches that boundary.
const MAX_RECURSION: usize = 1022;

/// How deeply the parser itself may nest (`((((…`, `!!!!…`, `a=a=a=…`).
/// Separate from [`MAX_RECURSION`] because it bounds stack growth on hostile
/// input rather than bash's variable-value recursion.
const MAX_NESTING: usize = 1024;

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
    // Word expansion happens before evaluation, once: a variable's text is
    // spliced in but not rescanned for further `$` references.
    let expanded = expand(src, vars, 0)?;
    eval_depth(&expanded, vars, 0)
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
        level: 0,
    };
    parser.skip_space();
    if parser.pos == parser.src.len() {
        // `$(( ))` and `for (( ; ; ))` have an empty expression, which is 0.
        return Ok(0);
    }
    let value = parser.expression()?;
    parser.skip_space();
    if parser.pos < parser.src.len() {
        let rest = &parser.src[parser.pos..];
        if rest.starts_with(b"=") && !rest.starts_with(b"==") {
            return Err("attempted assignment to non-variable".to_owned());
        }
        return Err(format!(
            "syntax error near `{}`",
            String::from_utf8_lossy(rest)
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
    let value = parse_literal(digits).ok()?;
    Some(if negative {
        value.wrapping_neg()
    } else {
        value
    })
}

/// Parse an unsigned integer literal: decimal, `0x` hexadecimal, leading-zero
/// octal, or `base#digits`.
fn parse_literal(token: &[u8]) -> Result<i64, String> {
    if token.is_empty() {
        return Err("invalid number".to_owned());
    }
    if let Some(hash) = token.iter().position(|&byte| byte == b'#') {
        let base_text = &token[..hash];
        let digits = &token[hash + 1..];
        // bash rejects a zero or leading-zero base as a malformed number, not
        // as an out-of-range base.
        if base_text == b"0" || (base_text.len() > 1 && base_text[0] == b'0') {
            return Err("invalid number".to_owned());
        }
        if base_text.is_empty() || !base_text.iter().all(u8::is_ascii_digit) {
            return Err("invalid arithmetic base".to_owned());
        }
        let mut base: u32 = 0;
        for &byte in base_text {
            base = base
                .saturating_mul(10)
                .saturating_add(u32::from(byte - b'0'));
        }
        if !(2..=64).contains(&base) {
            return Err("invalid arithmetic base".to_owned());
        }
        if digits.is_empty() {
            return Err("invalid integer constant".to_owned());
        }
        let mut value: i64 = 0;
        for &byte in digits {
            let Some(digit) = digit_value(byte, base) else {
                return Err("value too great for base".to_owned());
            };
            value = value
                .wrapping_mul(i64::from(base))
                .wrapping_add(i64::from(digit));
        }
        return Ok(value);
    }
    if token.len() >= 2 && token[0] == b'0' && matches!(token[1], b'x' | b'X') {
        // `0x` with no digits is zero in bash.
        let mut value: i64 = 0;
        for &byte in &token[2..] {
            let Some(digit) = hex_value(byte) else {
                return Err("value too great for base".to_owned());
            };
            value = value.wrapping_mul(16).wrapping_add(i64::from(digit));
        }
        return Ok(value);
    }
    if token.len() > 1 && token[0] == b'0' {
        let mut value: i64 = 0;
        for &byte in &token[1..] {
            if !(b'0'..=b'7').contains(&byte) {
                return Err("value too great for base".to_owned());
            }
            value = value.wrapping_mul(8).wrapping_add(i64::from(byte - b'0'));
        }
        return Ok(value);
    }
    let mut value: i64 = 0;
    for &byte in token {
        if !byte.is_ascii_digit() {
            return Err("value too great for base".to_owned());
        }
        value = value.wrapping_mul(10).wrapping_add(i64::from(byte - b'0'));
    }
    Ok(value)
}

fn hex_value(byte: u8) -> Option<u32> {
    (byte as char).to_digit(16)
}

/// The value of one `base#digits` digit. Above base 36 the two cases are
/// distinct (`64#A` is 36, not 10), matching bash.
fn digit_value(byte: u8, base: u32) -> Option<u32> {
    let value = match byte {
        b'0'..=b'9' => u32::from(byte - b'0'),
        b'a'..=b'z' => u32::from(byte - b'a') + 10,
        b'A'..=b'Z' => {
            if base <= 36 {
                u32::from(byte - b'A') + 10
            } else {
                u32::from(byte - b'A') + 36
            }
        }
        b'@' => 62,
        b'_' => 63,
        _ => return None,
    };
    (value < base).then_some(value)
}

/// Word-expand arithmetic source: splice `$name`/`${name}` text, evaluate
/// `${a[expr]}` subscripts, and remove double quotes. Single quotes and
/// escapes are left for the parser to reject, as bash does.
fn expand(src: &[u8], vars: &mut impl IntVars, depth: usize) -> Result<Vec<u8>, String> {
    if depth > MAX_RECURSION {
        return Err("expression recursion level exceeded".to_owned());
    }
    let mut out = Vec::with_capacity(src.len());
    let mut pos = 0;
    while pos < src.len() {
        match src[pos] {
            b'\'' => {
                // bash does not remove single quotes here; the quoted run
                // reaches the arithmetic parser and is a syntax error.
                let start = pos;
                pos += 1;
                while pos < src.len() && src[pos] != b'\'' {
                    pos += 1;
                }
                if pos < src.len() {
                    pos += 1;
                }
                out.extend_from_slice(&src[start..pos]);
            }
            b'"' => {
                pos = expand_quoted(src, pos + 1, &mut out, vars, depth)?;
            }
            b'\\' => {
                // A backslash-newline pair is a line continuation; any other
                // escape is kept, because the arithmetic grammar has none and
                // bash rejects it.
                if src.get(pos + 1) == Some(&b'\n') {
                    pos += 2;
                    continue;
                }
                out.push(b'\\');
                pos += 1;
                if let Some(&byte) = src.get(pos) {
                    out.push(byte);
                    pos += 1;
                }
            }
            b'$' => pos = expand_dollar(src, pos, &mut out, vars, depth)?,
            byte => {
                out.push(byte);
                pos += 1;
            }
        }
    }
    Ok(out)
}

/// Expand the body of a double-quoted run; `pos` is just past the opening
/// quote. Returns the position just past the closing quote.
fn expand_quoted(
    src: &[u8],
    mut pos: usize,
    out: &mut Vec<u8>,
    vars: &mut impl IntVars,
    depth: usize,
) -> Result<usize, String> {
    while pos < src.len() {
        match src[pos] {
            b'"' => return Ok(pos + 1),
            b'\\' => {
                if src.get(pos + 1) == Some(&b'\n') {
                    pos += 2;
                    continue;
                }
                if let Some(&next) = src.get(pos + 1) {
                    if matches!(next, b'$' | b'`' | b'"' | b'\\') {
                        out.push(next);
                        pos += 2;
                        continue;
                    }
                }
                out.push(b'\\');
                pos += 1;
            }
            b'$' => pos = expand_dollar(src, pos, out, vars, depth)?,
            byte => {
                out.push(byte);
                pos += 1;
            }
        }
    }
    Err("unexpected end of file in arithmetic expression".to_owned())
}

/// Expand one `$…` reference. Anything this evaluator cannot expand is copied
/// literally so the parser still reports a syntax error.
fn expand_dollar(
    src: &[u8],
    pos: usize,
    out: &mut Vec<u8>,
    vars: &mut impl IntVars,
    depth: usize,
) -> Result<usize, String> {
    let Some(&next) = src.get(pos + 1) else {
        out.push(b'$');
        return Ok(pos + 1);
    };
    if next == b'{' {
        if let Some((text, end)) = braced_expansion(src, pos + 2, vars, depth)? {
            out.extend_from_slice(&text);
            return Ok(end);
        }
        out.push(b'$');
        return Ok(pos + 1);
    }
    let start = pos + 1;
    if next.is_ascii_digit() {
        let mut end = start;
        while src.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        push_variable_text(out, vars, &src[start..end]);
        return Ok(end);
    }
    if next == b'_' || next.is_ascii_alphabetic() {
        let mut end = start;
        while src
            .get(end)
            .is_some_and(|byte| *byte == b'_' || byte.is_ascii_alphanumeric())
        {
            end += 1;
        }
        push_variable_text(out, vars, &src[start..end]);
        return Ok(end);
    }
    out.push(b'$');
    Ok(pos + 1)
}

/// `${name}` or `${name[expr]}`; `start` is just past the `{`.
fn braced_expansion(
    src: &[u8],
    start: usize,
    vars: &mut impl IntVars,
    depth: usize,
) -> Result<Option<(Vec<u8>, usize)>, String> {
    let Some(length) = name_len(&src[start..]) else {
        return Ok(None);
    };
    let name = &src[start..start + length];
    let mut pos = start + length;
    if src.get(pos) == Some(&b'}') {
        return Ok(Some((variable_text(vars, name), pos + 1)));
    }
    if src.get(pos) != Some(&b'[') {
        return Ok(None);
    }
    let Some(close) = matching_bracket(src, pos) else {
        return Ok(None);
    };
    if src.get(close + 1) != Some(&b'}') {
        return Ok(None);
    }
    let subscript = expand(&src[pos + 1..close], vars, depth + 1)?;
    let index = eval_depth(&subscript, vars, depth + 1)?;
    pos = close + 2;
    Ok(Some((
        vars.get_index_text(name, index).unwrap_or_default(),
        pos,
    )))
}

/// The index of the `]` matching the `[` at `open`.
fn matching_bracket(src: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, &byte) in src[open..].iter().enumerate() {
        match byte {
            b'[' => depth += 1,
            b']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

/// The text a `$name` reference splices into the expression. An unset
/// variable splices as empty; `LINENO` has no stored text but arithmetic can
/// read its number, and `$LINENO` must keep working in `for ((…))` headers.
fn variable_text(vars: &mut impl IntVars, name: &[u8]) -> Vec<u8> {
    match vars.get_text(name) {
        Some(text) => text,
        None if name == b"LINENO" => vars.get(name).to_string().into_bytes(),
        None => Vec::new(),
    }
}

fn push_variable_text(out: &mut Vec<u8>, vars: &mut impl IntVars, name: &[u8]) {
    out.extend_from_slice(&variable_text(vars, name));
}

/// Binary operators with C precedence, longest spelling first so `**` beats
/// `*`, `<<` beats `<` and `&&` beats `&`.
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
    /// Parser nesting, bounding stack growth on hostile input.
    level: usize,
}

impl<'a, V: IntVars> Parser<'a, V> {
    /// Guard one level of parser recursion. Callers decrement `level` after a
    /// successful nested parse; an error abandons the parser.
    fn enter(&mut self) -> Result<(), String> {
        self.level += 1;
        if self.level > MAX_NESTING {
            return Err("expression recursion level exceeded".to_owned());
        }
        Ok(())
    }

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
                    self.read_lvalue(name, subscript)?
                };
                self.enter()?;
                let value = self.assignment()?;
                self.level -= 1;
                let value = assign_value(token, current, value)?;
                self.write_lvalue(name, subscript, value)?;
                return Ok(value);
            }
            // No assignment operator after all: the name (and any subscript)
            // is an rvalue. Do not backtrack and re-parse it — evaluating a
            // subscript twice would double its side effects.
            let current = self.read_lvalue(name, subscript)?;
            let current = self.take_postfix(current, name, subscript)?;
            let current = self.binary_from(current, 1)?;
            return self.conditional_from(current);
        }
        let value = self.binary(1)?;
        self.conditional_from(value)
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
        let index = self.expression()?;
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

    fn read_lvalue(&mut self, name: &[u8], subscript: Option<i64>) -> Result<i64, String> {
        if self.dead {
            return Ok(0);
        }
        let (value, text) = match subscript {
            Some(index) => (
                self.vars.get_index(name, index),
                self.vars.get_index_text(name, index),
            ),
            None => (self.vars.get(name), self.vars.get_text(name)),
        };
        // A non-numeric value is itself an expression: `a=(1+2+3);
        // $((a[0]))` is 6, and `i=0+1; $((i))` is 1. A broken expression is
        // an error, not a silent zero — `s='12 34'; $((s[0]))` fails.
        if let Some(text) = text {
            if !text.is_empty() && parse_int(&text).is_none() {
                return eval_depth(&text, self.vars, self.depth + 1);
            }
        }
        Ok(value)
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

    /// `cond ? then : else`, right-associative. Exactly one arm is
    /// evaluated; the other is parsed with `dead` set.
    fn conditional_from(&mut self, condition: i64) -> Result<i64, String> {
        self.skip_space();
        if !self.eat(b"?") {
            return Ok(condition);
        }
        let outer_dead = self.dead;
        let taken = !outer_dead && condition != 0;
        // The middle arm is a full expression, so `1 ? 2, 3 : 4` is three.
        self.dead = !taken;
        let then_value = self.expression()?;
        self.skip_space();
        if !self.eat(b":") {
            return Err("expected `:` in conditional".to_owned());
        }
        // The third arm is a conditional-expression: `1 ? 2 : 3, 4` applies
        // the comma to the whole conditional, so it is four.
        self.dead = outer_dead || taken;
        self.enter()?;
        let else_value = self.conditional()?;
        self.level -= 1;
        self.dead = outer_dead;
        if outer_dead {
            Ok(0)
        } else if taken {
            Ok(then_value)
        } else {
            Ok(else_value)
        }
    }

    fn conditional(&mut self) -> Result<i64, String> {
        let value = self.binary(1)?;
        self.conditional_from(value)
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
                let value = self.read_lvalue(&name, subscript)?.wrapping_add(step);
                if !self.dead {
                    self.write_lvalue(&name, subscript, value)?;
                }
                return Ok(value);
            }
            self.enter()?;
            let value = self.unary()?;
            self.level -= 1;
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
            self.enter()?;
            let value = self.expression()?;
            self.level -= 1;
            if !self.eat(b")") {
                return Err("missing `)`".to_owned());
            }
            return Ok(value);
        }
        if self.src[self.pos..]
            .first()
            .is_some_and(|b| b.is_ascii_digit())
        {
            return self.number();
        }
        let name = self.take_name()?;
        let subscript = self.take_subscript()?;
        let current = self.read_lvalue(&name, subscript)?;
        self.take_postfix(current, &name, subscript)
    }

    /// A numeric literal, including bash's `base#digits` form.
    fn number(&mut self) -> Result<i64, String> {
        let start = self.pos;
        while self.src.get(self.pos).is_some_and(u8::is_ascii_digit) {
            self.pos += 1;
        }
        if self.src.get(self.pos) == Some(&b'#') {
            self.pos += 1;
            while self.src.get(self.pos).is_some_and(|byte| {
                byte.is_ascii_alphanumeric() || matches!(*byte, b'@' | b'_' | b'#')
            }) {
                self.pos += 1;
            }
        } else if &self.src[start..self.pos] == b"0"
            && self
                .src
                .get(self.pos)
                .is_some_and(|byte| matches!(byte, b'x' | b'X'))
        {
            self.pos += 1;
            while self
                .src
                .get(self.pos)
                .is_some_and(u8::is_ascii_alphanumeric)
            {
                self.pos += 1;
            }
        } else {
            // Keep a malformed suffix (`42x`, `09`) in one token so the
            // diagnostic names the whole constant.
            while self
                .src
                .get(self.pos)
                .is_some_and(u8::is_ascii_alphanumeric)
            {
                self.pos += 1;
            }
        }
        parse_literal(&self.src[start..self.pos])
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
    texts: HashMap<Vec<u8>, Vec<u8>>,
}

impl MapVars {
    /// Set a variable's raw text, as a shell assignment would.
    pub fn set_text(&mut self, name: &[u8], text: &[u8]) {
        self.texts.insert(name.to_vec(), text.to_vec());
    }
}

impl IntVars for MapVars {
    fn get(&mut self, name: &[u8]) -> i64 {
        if let Some(value) = self.values.get(name) {
            return *value;
        }
        self.texts
            .get(name)
            .and_then(|text| parse_int(text))
            .unwrap_or(0)
    }

    fn set(&mut self, name: &[u8], value: i64) -> Result<(), String> {
        self.values.insert(name.to_vec(), value);
        self.texts
            .insert(name.to_vec(), value.to_string().into_bytes());
        Ok(())
    }

    fn get_text(&mut self, name: &[u8]) -> Option<Vec<u8>> {
        self.texts.get(name).cloned().or_else(|| {
            self.values
                .get(name)
                .map(|value| value.to_string().into_bytes())
        })
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
        assert_eq!(value("1 && 0 || 1"), 1);
        assert_eq!(value("0 && 0 || 1"), 1);
        assert_eq!(value("1 && 1 || 0"), 1);
        assert_eq!(value("0 || 1 && 0"), 0);
        assert_eq!(value("0 && 1 | 1"), 0);
        assert_eq!(value("1 && 2 | 1"), 1);
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
    fn ternary_selects_and_nests() {
        assert_eq!(value("1 ? 2 : 3"), 2);
        assert_eq!(value("0 ? 2 : 3"), 3);
        assert_eq!(value("5 > 3 ? 7 : 8"), 7);
        assert_eq!(value("1 ? 2 : 3 ? 4 : 5"), 2);
        assert_eq!(value("0 ? 2 : 3 ? 4 : 5"), 4);
        assert_eq!(value("1 ? 2 ? 3 : 4 : 5"), 3);
        assert_eq!(value("(1 ? 2 : 3) ? 4 : 5"), 4);
        assert_eq!(value("1 ? 2, 3 : 4"), 3, "comma is allowed in the middle");
        assert_eq!(
            value("1 ? 2 : 3, 4"),
            4,
            "a trailing comma applies to the whole conditional"
        );
    }

    #[test]
    fn ternary_is_short_circuit() {
        let mut vars = MapVars::default();
        assert_eq!(eval(b"1 ? 5 : 1 / 0", &mut vars), Ok(5));
        assert_eq!(eval(b"0 ? 1 / 0 : 7", &mut vars), Ok(7));
        // The untaken `x++` must not write.
        assert_eq!(eval(b"1 ? 5 : x++", &mut vars), Ok(5));
        assert_eq!(eval(b"x", &mut vars), Ok(0));
        assert_eq!(eval(b"0 ? x++ : 7", &mut vars), Ok(7));
        assert_eq!(eval(b"x", &mut vars), Ok(0));
        // The taken one must.
        assert_eq!(eval(b"1 ? y++ : 9", &mut vars), Ok(0));
        assert_eq!(eval(b"y", &mut vars), Ok(1));
        assert_eq!(eval(b"0 ? 9 : z++", &mut vars), Ok(0));
        assert_eq!(eval(b"z", &mut vars), Ok(1));
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
        assert_eq!(eval(b"a |= ++a", &mut vars), Ok(8 | 9));
        // A plain assignment must not read the old value.
        vars.set_text(b"s", b"12 34");
        assert_eq!(eval(b"s = 5", &mut vars), Ok(5));
        assert_eq!(eval(b"s", &mut vars), Ok(5));
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
    fn increment_and_decrement_are_pre_and_post() {
        let mut vars = MapVars::default();
        assert_eq!(eval(b"n = 4", &mut vars), Ok(4));
        assert_eq!(eval(b"n++", &mut vars), Ok(4));
        assert_eq!(eval(b"n", &mut vars), Ok(5));
        assert_eq!(eval(b"++n", &mut vars), Ok(6));
        assert_eq!(eval(b"n--", &mut vars), Ok(6));
        assert_eq!(eval(b"--n", &mut vars), Ok(4));
    }

    #[test]
    fn overflow_wraps_at_64_bits() {
        assert_eq!(value("9223372036854775807 + 1"), i64::MIN);
        assert_eq!(value("-9223372036854775808 - 1"), i64::MAX);
        assert_eq!(value("1 << 64"), 1, "bash masks the shift count");
        assert_eq!(value("(-9223372036854775807 - 1) / -1"), i64::MIN);
    }

    #[test]
    fn literals_are_c_style() {
        assert_eq!(value("0x10"), 16);
        assert_eq!(value("0XAA"), 170);
        assert_eq!(value("010"), 8);
        assert_eq!(value("10#011"), 11);
        assert_eq!(value("+4"), 4);
    }

    #[test]
    fn base_literals_follow_bash() {
        assert_eq!(value("10#11"), 11);
        assert_eq!(value("2#101"), 5);
        assert_eq!(value("2#0110"), 6);
        assert_eq!(value("16#1b"), 27);
        assert_eq!(value("10#0123"), 123);
        assert_eq!(value("24#ag7"), 6151);
        assert_eq!(value("36#a"), 10);
        assert_eq!(value("36#z"), 35);
        assert_eq!(value("36#Z"), 35, "case-folded at or below base 36");
        assert_eq!(value("64#a"), 10);
        assert_eq!(value("64#z"), 35);
        assert_eq!(value("64#A"), 36, "distinct above base 36");
        assert_eq!(value("64#Z"), 61);
        assert_eq!(value("64#@"), 62);
        assert_eq!(value("64#_"), 63);
        assert_eq!(value("64#10"), 64);
        assert_eq!(value("-2#1"), -1);
    }

    #[test]
    fn malformed_base_literals_are_errors() {
        for src in [
            "2#102",   // digit out of range
            "8#9",     // digit out of range
            "2#1_",    // `_` is 63, out of range for base 2
            "37#Z",    // Z is 61, above base 37
            "1#0",     // base too small
            "65#0",    // base too large
            "0#0",     // zero base
            "02#0110", // leading-zero base
            "2#",      // no digits
            "2##1",    // malformed
            "09",      // bad octal digit
            "0x1X",    // bad hex digit
            "42x",     // trailing junk
        ] {
            assert!(
                eval(src.as_bytes(), &mut MapVars::default()).is_err(),
                "{src} should fail"
            );
        }
        assert!(fails("2#102").contains("value too great for base"));
        assert!(fails("1#0").contains("invalid arithmetic base"));
        assert!(fails("2#").contains("invalid integer constant"));
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
    fn quoted_literals_are_expanded() {
        assert_eq!(value("\"3\" + 1"), 4);
        assert_eq!(value("\"1 + 2\" * 3"), 7);
        assert_eq!(value("\"1+2\" * 3"), 7);
        assert_eq!(value("\"\" + 1"), 1);
        assert_eq!(value("\"1,2\""), 2);
        assert!(fails("'1' + 2").contains("name"), "single quotes are not");
        assert!(fails("\"3").contains("end of file"));
    }

    #[test]
    fn dollar_expansion_splices_text() {
        let mut vars = MapVars::default();
        vars.set_text(b"x", b"1 + 2");
        // `$x` is textual: `1 + 2 * 3` is seven.
        assert_eq!(eval(b"$x * 3", &mut vars), Ok(7));
        assert_eq!(eval(b"\"$x\" * 3", &mut vars), Ok(7));
        // A bare name is evaluated as an expression first: three, times three.
        assert_eq!(eval(b"x * 3", &mut vars), Ok(9));
        assert_eq!(eval(b"x", &mut vars), Ok(3));
        // Unset names splice as the empty string.
        assert_eq!(eval(b"$unset + 1", &mut vars), Ok(1));
        assert_eq!(eval(b"$unset", &mut vars), Ok(0));
        // Dynamic names and base prefixes.
        vars.set_text(b"foo", b"bar");
        assert_eq!(eval(b"x$foo = 42", &mut vars), Ok(42));
        assert_eq!(eval(b"xbar", &mut vars), Ok(42));
        vars.set_text(b"base", b"16");
        assert_eq!(eval(b"${base}#a", &mut vars), Ok(10));
        vars.set_text(b"zero", b"0");
        assert_eq!(eval(b"${zero}11", &mut vars), Ok(9));
        assert_eq!(eval(b"${zero}xAB", &mut vars), Ok(171));
    }

    #[test]
    fn array_subscripts_in_expansions_are_evaluated() {
        let mut vars = MapVars::default();
        vars.set_index(b"a", 0, 4).unwrap();
        vars.set_index(b"a", 1, 7).unwrap();
        vars.set(b"i", 1).unwrap();
        assert_eq!(eval(b"${a[i]} + 1", &mut vars), Ok(8));
        assert_eq!(eval(b"${a[i + 1]} + 1", &mut vars), Ok(1));
    }

    #[test]
    fn empty_expressions_are_zero() {
        assert_eq!(value(""), 0);
        assert_eq!(value("   "), 0);
        assert!(fails("()").contains("expected a name"));
    }

    #[test]
    fn nested_expressions_propagate_errors() {
        let mut vars = MapVars::default();
        vars.set_text(b"a", b"12 34");
        assert!(eval(b"a", &mut vars).is_err());
        assert!(eval(b"a + 1", &mut vars).is_err());
        vars.set_text(b"b", b"1 / 0");
        assert!(eval(b"b", &mut vars).is_err());
        vars.set_text(b"c", b"abc");
        assert_eq!(eval(b"c + 1", &mut vars), Ok(1), "a name evaluates to 0");
    }

    #[test]
    fn division_by_zero_is_an_error() {
        assert!(fails("1 / 0").contains("division by 0"));
        assert!(fails("1 % 0").contains("division by 0"));
        assert!(fails("v = 1, v /= 0").contains("division by 0"));
        assert!(fails("v = 1, v %= 0").contains("division by 0"));
        assert!(fails("1 / (2 - 2)").contains("division by 0"));
    }

    #[test]
    fn assignment_to_a_non_lvalue_is_an_error() {
        assert!(fails("1 = 3").contains("attempted assignment"));
        assert!(fails("(a + 2) = 3").contains("attempted assignment"));
        // Still an error, but a parse one: a second subscript is not an
        // lvalue bash accepts either.
        assert!(eval(b"a[1][2] = 3", &mut MapVars::default()).is_err());
    }

    #[test]
    fn syntax_errors_are_reported() {
        assert!(fails("1 +").contains("expected a name"));
        assert!(fails("(1").contains("missing"));
        assert!(fails("1 2").contains("syntax error"));
        assert!(fails("1 ? 2").contains("conditional"));
    }

    #[test]
    fn parse_int_accepts_signed_and_unsigned_forms() {
        assert_eq!(parse_int(b"42"), Some(42));
        assert_eq!(parse_int(b" -7 "), Some(-7));
        assert_eq!(parse_int(b"0xff"), Some(255));
        assert_eq!(parse_int(b"2#101"), Some(5));
        assert_eq!(parse_int(b"2#102"), None);
        assert_eq!(parse_int(b"nope"), None);
        assert_eq!(parse_int(b""), None);
    }

    /// `LINENO` is readable as a number but has no stored text, like the
    /// shell's `IntVars` implementation.
    struct LineVars {
        vars: MapVars,
        line: i64,
    }

    impl IntVars for LineVars {
        fn get(&mut self, name: &[u8]) -> i64 {
            if name == b"LINENO" {
                return self.line;
            }
            self.vars.get(name)
        }

        fn set(&mut self, name: &[u8], value: i64) -> Result<(), String> {
            self.vars.set(name, value)
        }

        fn get_text(&mut self, name: &[u8]) -> Option<Vec<u8>> {
            self.vars.get_text(name)
        }
    }

    #[test]
    fn lineno_splices_its_number() {
        let mut vars = LineVars {
            vars: MapVars::default(),
            line: 7,
        };
        assert_eq!(eval(b"i < $LINENO", &mut vars), Ok(1));
        assert_eq!(eval(b"${LINENO}", &mut vars), Ok(7));
        assert_eq!(eval(b"$LINENO + 1", &mut vars), Ok(8));
    }

    #[test]
    fn evaluation_never_panics() {
        // Every prefix of a gnarly expression parses or errors cleanly.
        let src = b"1 ? 2 ** 3, 4 : 5#6 + a[1] <<= 7";
        for end in 0..=src.len() {
            let _ = eval(&src[..end], &mut MapVars::default());
        }
        for src in [
            "()".as_bytes(),
            b"((((((((((",
            b"+++++++++++",
            b"--------------------",
            b"a????????????",
            b"1 ? : 2",
            b"1 ? 2 :",
            b",,,",
            b"[]",
            b"a[",
            b"$",
            b"${",
            b"\"",
            b"'\''",
        ] {
            let _ = eval(src, &mut MapVars::default());
        }
    }
}
