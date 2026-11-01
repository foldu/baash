//! `test` and `[`: the POSIX expression language and bash's extensions.
//!
//! The control flow is bash's `test.c`, ported literally, because the
//! interesting behaviour is in the *ambiguities* rather than in the operators:
//!
//! * 0–4 arguments are decided by POSIX's argument-count rules, which is why
//!   `test -n` is "is the string `-n` non-empty" (true) while `test -n x` is
//!   the operator;
//! * more than four are parsed by the grammar (`!`, `-a`, `-o`, parentheses,
//!   left to right with `-a` binding tighter), and a trailing argument is
//!   "too many arguments" (status 2) rather than ignored;
//! * `-a` is both "file exists" (unary) and "and" (binary), and `(` and `)`
//!   are context-dependent tokens, which is the source of the classic
//!   `[ -a -a -a ]` bewilderment.
//!
//! Errors are status 2, false is status 1, and `[` additionally requires the
//! final `]` argument. `-v` (is a variable/element set), `-R` (is it a
//! nameref), `-o` (is a shell option on), `-O`/`-G` (owner/group) and `-N`
//! (mtime newer than atime) are bash's; everything else is POSIX.

use std::time::SystemTime;

use crate::errors::codes;
use crate::{ShellHost, Status};
use baash_diag::Diagnostic;
use baash_sys::Access;

/// `test expression`.
pub(crate) fn bi_test(host: &mut dyn ShellHost, argv: &[&[u8]]) -> Status {
    evaluate(host, &argv[1..])
}

/// `[ expression ]`.
pub(crate) fn bi_bracket(host: &mut dyn ShellHost, argv: &[&[u8]]) -> Status {
    match argv.last() {
        Some(last) if *last == b"]" => evaluate(host, &argv[1..argv.len() - 1]),
        _ => {
            host.diagnose(&Diagnostic::error(
                codes::OPERAND,
                0..0,
                "[: missing `]'".to_owned(),
            ));
            Status(2)
        }
    }
}

/// Run the expression and turn it into a status.
fn evaluate(host: &mut dyn ShellHost, arguments: &[&[u8]]) -> Status {
    if arguments.is_empty() {
        return Status::FAILURE;
    }
    let mut parser = Parser {
        host,
        args: arguments,
        pos: 0,
    };
    match parser.posixtest(arguments.len()) {
        Ok(value) => {
            // Anything the parse did not consume is a syntax error, exactly as
            // `test_command` reports it.
            if parser.pos != arguments.len() {
                let trailing = arguments[parser.pos];
                let message = if trailing.first() == Some(&b'-') {
                    format!(
                        "test: syntax error: `{}' unexpected",
                        String::from_utf8_lossy(trailing)
                    )
                } else {
                    "test: too many arguments".to_owned()
                };
                parser.syntax_error(&message);
                return Status(2);
            }
            if value {
                Status::SUCCESS
            } else {
                Status::FAILURE
            }
        }
        Err(message) => {
            parser.syntax_error(&message);
            Status(2)
        }
    }
}

/// The expression parser: `pos` is an index into `args`, matching bash's
/// (one-based) `pos`, and [`Parser::advance`] is its bounds check.
struct Parser<'a, 'h> {
    host: &'h mut dyn ShellHost,
    args: &'a [&'a [u8]],
    pos: usize,
}

impl Parser<'_, '_> {
    fn syntax_error(&mut self, message: &str) {
        self.host
            .diagnose(&Diagnostic::error(codes::OPERAND, 0..0, message.to_owned()));
    }

    /// `advance(f)`: step forward, and fail when that ran off the end.
    fn advance(&mut self, checked: bool) -> Result<(), String> {
        self.pos += 1;
        if checked && self.pos >= self.args.len() {
            return Err("test: argument expected".to_owned());
        }
        Ok(())
    }

    /// The POSIX argument-count rules, then the grammar.
    fn posixtest(&mut self, nargs: usize) -> Result<bool, String> {
        match nargs {
            0 => Ok(false),
            1 => {
                let value = !self.args[0].is_empty();
                self.advance(false)?;
                Ok(value)
            }
            2 => self.two_arguments(),
            3 => self.three_arguments(),
            4 => {
                if self.args[self.pos] == b"!" {
                    self.advance(true)?;
                    return Ok(!self.three_arguments()?);
                }
                if self.args[self.pos] == b"(" && self.args[self.pos + 3] == b")" {
                    self.advance(true)?;
                    let value = self.two_arguments()?;
                    self.advance(false)?;
                    return Ok(value);
                }
                self.expression()
            }
            _ => self.expression(),
        }
    }

    /// `expr: or`.
    fn expression(&mut self) -> Result<bool, String> {
        if self.pos >= self.args.len() {
            return Err("test: argument expected".to_owned());
        }
        self.or()
    }

    /// `or: and ('-o' or)*`.
    fn or(&mut self) -> Result<bool, String> {
        let value = self.and()?;
        if self.pos < self.args.len() && self.args[self.pos] == b"-o" {
            self.advance(false)?;
            let right = self.or()?;
            return Ok(value || right);
        }
        Ok(value)
    }

    /// `and: term ('-a' and)*`.
    fn and(&mut self) -> Result<bool, String> {
        let value = self.term()?;
        if self.pos < self.args.len() && self.args[self.pos] == b"-a" {
            self.advance(false)?;
            let right = self.and()?;
            return Ok(value && right);
        }
        Ok(value)
    }

    /// `term`: `!`, a parenthesised expression, a binary test, a unary test,
    /// or a plain word (true when non-empty).
    fn term(&mut self) -> Result<bool, String> {
        if self.pos >= self.args.len() {
            return Err("test: argument expected".to_owned());
        }
        if self.args[self.pos] == b"!" {
            let mut negate = false;
            while self.pos < self.args.len() && self.args[self.pos] == b"!" {
                self.advance(true)?;
                negate = !negate;
            }
            let value = self.term()?;
            return Ok(if negate { !value } else { value });
        }
        if self.args[self.pos] == b"(" {
            self.advance(true)?;
            // Scan forward for the matching `)`, then decide whether the
            // sub-expression is small enough for the POSIX rules to apply.
            let mut nargs = 1;
            let mut depth = 1i32;
            while self.pos + nargs < self.args.len() {
                let token = self.args[self.pos + nargs];
                if token == b")" {
                    depth -= 1;
                } else if token == b"(" {
                    depth += 1;
                }
                if depth == 0 {
                    break;
                }
                nargs += 1;
            }
            let value = if self.pos + nargs < self.args.len() && nargs <= 4 {
                self.posixtest(nargs)?
            } else {
                self.expression()?
            };
            match self.args.get(self.pos) {
                None => return Err("test: `)' expected".to_owned()),
                Some(token) if *token != b")" => {
                    return Err(format!(
                        "test: `)' expected, found {}",
                        String::from_utf8_lossy(token)
                    ));
                }
                Some(_) => {
                    self.advance(false)?;
                }
            }
            return Ok(value);
        }
        if self.pos + 3 <= self.args.len() && test_binop(self.args[self.pos + 1]) {
            return self.binary_operator();
        }
        if self.pos + 2 <= self.args.len() && test_unop(self.args[self.pos]) {
            return self.unary_operator();
        }
        let value = !self.args[self.pos].is_empty();
        self.advance(false)?;
        Ok(value)
    }

    /// Two arguments: `! x` or `-op x`.
    fn two_arguments(&mut self) -> Result<bool, String> {
        if self.args[self.pos] == b"!" {
            self.advance(false)?;
            let value = self.args[self.pos].is_empty();
            self.pos += 1;
            return Ok(value);
        }
        if self.args[self.pos].len() == 2
            && self.args[self.pos][0] == b'-'
            && test_unop(self.args[self.pos])
        {
            return self.unary_operator();
        }
        Err(format!(
            "test: {}: unary operator expected",
            String::from_utf8_lossy(self.args[self.pos])
        ))
    }

    /// Three arguments: `x op y`, `x -a y`, `x -o y`, `! -op x`, or `( x )`.
    fn three_arguments(&mut self) -> Result<bool, String> {
        let second = self.args[self.pos + 1];
        if test_binop(second) {
            return self.binary_operator();
        }
        if second == b"-a" || second == b"-o" {
            let left = !self.args[self.pos].is_empty();
            let right = !self.args[self.pos + 2].is_empty();
            self.pos += 3;
            return Ok(if second == b"-a" {
                left && right
            } else {
                left || right
            });
        }
        if self.args[self.pos] == b"!" {
            self.advance(true)?;
            return Ok(!self.two_arguments()?);
        }
        if self.args[self.pos] == b"(" && self.args[self.pos + 2] == b")" {
            let value = !self.args[self.pos + 1].is_empty();
            self.pos += 3;
            return Ok(value);
        }
        Err(format!(
            "test: {}: binary operator expected",
            String::from_utf8_lossy(second)
        ))
    }

    /// A binary test: string comparison, integer comparison, or file times.
    fn binary_operator(&mut self) -> Result<bool, String> {
        let operator = self.args[self.pos + 1];
        let left = self.args[self.pos];
        let right = self.args[self.pos + 2];
        let value = match operator {
            b"=" | b"==" => left == right,
            b"!=" => left != right,
            b"<" => left < right,
            b">" => left > right,
            b"-eq" | b"-ne" | b"-lt" | b"-le" | b"-gt" | b"-ge" => {
                let left = self.integer(left)?;
                let right = self.integer(right)?;
                match operator {
                    b"-eq" => left == right,
                    b"-ne" => left != right,
                    b"-lt" => left < right,
                    b"-le" => left <= right,
                    b"-gt" => left > right,
                    _ => left >= right,
                }
            }
            b"-nt" => newer_than(left, right),
            b"-ot" => older_than(left, right),
            b"-ef" => same_file(left, right),
            other => {
                return Err(format!(
                    "test: {}: binary operator expected",
                    String::from_utf8_lossy(other)
                ));
            }
        };
        self.pos += 3;
        Ok(value)
    }

    /// A unary test, including bash's argument-optional `-t`.
    fn unary_operator(&mut self) -> Result<bool, String> {
        let operator = self.args[self.pos];
        if operator == b"-t" {
            // bash lets `-t` omit its operand when the next token is `-a`/`-o`
            // (`test -t -a x`) and defaults to descriptor 1 at the end.
            self.advance(false)?;
            if self.pos < self.args.len() {
                if legal_number(self.args[self.pos]).is_some() {
                    let argument = self.args[self.pos];
                    self.advance(false)?;
                    return self.unary_test(operator, argument);
                }
                if self.args.len() >= 5
                    && (self.args[self.pos] == b"-a" || self.args[self.pos] == b"-o")
                {
                    return self.unary_test(operator, b"1");
                }
                return Err(format!(
                    "test: {}: integer expected",
                    String::from_utf8_lossy(self.args[self.pos])
                ));
            }
            return self.unary_test(operator, b"1");
        }
        self.advance(true)?;
        let argument = self.args[self.pos];
        self.advance(false)?;
        self.unary_test(operator, argument)
    }

    fn unary_test(&mut self, operator: &[u8], argument: &[u8]) -> Result<bool, String> {
        Ok(match operator[1] {
            b'a' | b'e' => metadata(argument).is_some(),
            b'r' => baash_sys::access(argument, Access::Read),
            b'w' => baash_sys::access(argument, Access::Write),
            b'x' => baash_sys::access(argument, Access::Execute),
            b'O' => owned_by_user(argument),
            b'G' => owned_by_group(argument),
            b'N' => modified_later_than_read(argument),
            b'f' => metadata(argument).is_some_and(|metadata| metadata.is_file()),
            b'd' => metadata(argument).is_some_and(|metadata| metadata.is_dir()),
            b's' => metadata(argument).is_some_and(|metadata| metadata.len() > 0),
            b'S' => unix_file_type(argument, FileType::Socket),
            b'c' => unix_file_type(argument, FileType::CharDevice),
            b'b' => unix_file_type(argument, FileType::BlockDevice),
            b'p' => unix_file_type(argument, FileType::Fifo),
            b'L' | b'h' => {
                !argument.is_empty()
                    && std::fs::symlink_metadata(baash_sys::os::path_from_bytes(argument))
                        .is_ok_and(|metadata| metadata.file_type().is_symlink())
            }
            b'u' => permission_bits(argument).is_some_and(|mode| mode & 0o4000 != 0),
            b'g' => permission_bits(argument).is_some_and(|mode| mode & 0o2000 != 0),
            b'k' => permission_bits(argument).is_some_and(|mode| mode & 0o1000 != 0),
            b't' => {
                let descriptor = self.integer(argument)?;
                i32::try_from(descriptor).is_ok_and(baash_sys::is_tty)
            }
            b'n' => !argument.is_empty(),
            b'z' => argument.is_empty(),
            b'o' => self.option(argument),
            b'v' => self.variable(argument),
            b'R' => self.nameref(argument),
            other => {
                return Err(format!(
                    "test: -{}: unary operator expected",
                    char::from(other)
                ));
            }
        })
    }

    /// `-o name`: whether a `set -o` option is on.
    ///
    /// bash's `minus_o_option_value` only knows the `set -o` table, so a
    /// `shopt`-only name (or an unknown one) is simply false.
    fn option(&mut self, name: &[u8]) -> bool {
        let opts = &self.host.state().opts;
        match crate::Flag::lookup(name) {
            Some(flag) if flag.in_kind(crate::OptionKind::SetO) => opts.get_flag(flag),
            _ => false,
        }
    }

    /// `-v`: whether a variable, array element or positional parameter is set.
    fn variable(&mut self, argument: &[u8]) -> bool {
        // `test -v N` asks about `$N` — but only for an operand that is not an
        // array reference.
        if !argument.contains(&b'[')
            && let Some(number) = legal_number(argument)
        {
            return number >= 0 && number <= self.host.state().vars.arg_count() as i64;
        }
        self.host.variable_is_set(argument).unwrap_or(false)
    }

    /// `-R`: whether the name refers to a set nameref.
    fn nameref(&mut self, name: &[u8]) -> bool {
        match self.host.state().vars.lookup(name) {
            Some(var) => var.attrs.nameref && var.value.is_set(),
            None => false,
        }
    }

    /// Parse an integer operand, or report bash's "integer expected".
    fn integer(&self, text: &[u8]) -> Result<i64, String> {
        legal_number(text)
            .ok_or_else(|| format!("test: {}: integer expected", String::from_utf8_lossy(text)))
    }
}

/// bash's `test_binop`: the operators that take two operands.
fn test_binop(operator: &[u8]) -> bool {
    matches!(
        operator,
        b"=" | b"=="
            | b"!="
            | b"<"
            | b">"
            | b"-eq"
            | b"-ne"
            | b"-lt"
            | b"-le"
            | b"-gt"
            | b"-ge"
            | b"-nt"
            | b"-ot"
            | b"-ef"
    )
}

/// bash's `test_unop`: the operators that take one operand.
fn test_unop(operator: &[u8]) -> bool {
    operator.len() == 2
        && operator[0] == b'-'
        && matches!(
            operator[1],
            b'a' | b'b'
                | b'c'
                | b'd'
                | b'e'
                | b'f'
                | b'g'
                | b'h'
                | b'k'
                | b'n'
                | b'o'
                | b'p'
                | b'r'
                | b's'
                | b't'
                | b'u'
                | b'v'
                | b'w'
                | b'x'
                | b'z'
                | b'G'
                | b'L'
                | b'O'
                | b'S'
                | b'N'
                | b'R'
        )
}

/// bash's `legal_number`: base-10 `strtoimax`, surrounding blanks allowed.
fn legal_number(text: &[u8]) -> Option<i64> {
    let mut at = 0;
    while let Some(b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') = text.get(at).copied() {
        at += 1;
    }
    let mut negative = false;
    if let Some(byte @ (b'+' | b'-')) = text.get(at).copied() {
        negative = byte == b'-';
        at += 1;
    }
    let digits = at;
    let mut magnitude: u128 = 0;
    while let Some(byte @ b'0'..=b'9') = text.get(at).copied() {
        magnitude = magnitude
            .saturating_mul(10)
            .saturating_add(u128::from(byte - b'0'));
        at += 1;
    }
    if at == digits {
        return None;
    }
    // `strtoimax` reports ERANGE, which `legal_number` turns into "not a
    // number" rather than a clamp.
    let magnitude = if negative {
        (magnitude as i128).wrapping_neg()
    } else {
        magnitude as i128
    };
    let value = i64::try_from(magnitude).ok()?;
    while let Some(b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') = text.get(at).copied() {
        at += 1;
    }
    (at == text.len()).then_some(value)
}

fn metadata(argument: &[u8]) -> Option<std::fs::Metadata> {
    std::fs::metadata(baash_sys::os::path_from_bytes(argument)).ok()
}

fn modification_time(argument: &[u8]) -> Option<SystemTime> {
    metadata(argument)?.modified().ok()
}

/// `-nt`: `left` exists and `right` does not, or is newer.
fn newer_than(left: &[u8], right: &[u8]) -> bool {
    match (modification_time(left), modification_time(right)) {
        (Some(left), Some(right)) => left > right,
        (Some(_), None) => true,
        _ => false,
    }
}

/// `-ot`: `left` does not exist and `right` does, or is older.
fn older_than(left: &[u8], right: &[u8]) -> bool {
    match (modification_time(left), modification_time(right)) {
        (Some(left), Some(right)) => left < right,
        (None, Some(_)) => true,
        _ => false,
    }
}

/// `-ef`: the same file, by device and inode.
#[cfg(unix)]
fn same_file(left: &[u8], right: &[u8]) -> bool {
    use std::os::unix::fs::MetadataExt;

    match (metadata(left), metadata(right)) {
        (Some(left), Some(right)) => left.dev() == right.dev() && left.ino() == right.ino(),
        _ => false,
    }
}

/// `-ef` needs inodes; without them the question cannot be answered.
#[cfg(not(unix))]
fn same_file(_left: &[u8], _right: &[u8]) -> bool {
    false
}

/// `-O`: the file's owner is this (effective) user.
fn owned_by_user(argument: &[u8]) -> bool {
    let path = baash_sys::os::path_from_bytes(argument);
    match (
        baash_sys::os::own_uid(),
        baash_sys::os::owner_uid(&path),
        metadata(argument),
    ) {
        (Some(ours), Some(theirs), Some(_)) => ours == theirs,
        _ => false,
    }
}

/// `-G`: the file's group is this (effective) group.
fn owned_by_group(argument: &[u8]) -> bool {
    let path = baash_sys::os::path_from_bytes(argument);
    match (
        baash_sys::os::own_gid(),
        baash_sys::os::owner_gid(&path),
        metadata(argument),
    ) {
        (Some(ours), Some(theirs), Some(_)) => ours == theirs,
        _ => false,
    }
}

/// `-N`: the file was modified after it was last read.
fn modified_later_than_read(argument: &[u8]) -> bool {
    let Some(metadata) = metadata(argument) else {
        return false;
    };
    match (metadata.modified(), metadata.accessed()) {
        (Ok(modified), Ok(accessed)) => modified > accessed,
        _ => false,
    }
}

/// The file type questions that only Unix can answer.
enum FileType {
    Socket,
    CharDevice,
    BlockDevice,
    Fifo,
}

#[cfg(unix)]
fn unix_file_type(argument: &[u8], wanted: FileType) -> bool {
    use std::os::unix::fs::FileTypeExt;

    let Some(metadata) = metadata(argument) else {
        return false;
    };
    let file_type = metadata.file_type();
    match wanted {
        FileType::Socket => file_type.is_socket(),
        FileType::CharDevice => file_type.is_char_device(),
        FileType::BlockDevice => file_type.is_block_device(),
        FileType::Fifo => file_type.is_fifo(),
    }
}

/// Without Unix file types the answer is "no", not a guess.
#[cfg(not(unix))]
fn unix_file_type(_argument: &[u8], _wanted: FileType) -> bool {
    false
}

/// The permission bits (`-u`/`-g`/`-k` ask about the set-id and sticky bits).
#[cfg(unix)]
fn permission_bits(argument: &[u8]) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;

    metadata(argument).map(|metadata| metadata.permissions().mode())
}

/// There are no set-id or sticky bits to inspect off Unix.
#[cfg(not(unix))]
fn permission_bits(_argument: &[u8]) -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::TestHost;

    fn run(host: &mut TestHost, arguments: &[&[u8]]) -> Status {
        let mut argv: Vec<&[u8]> = vec![&b"test"[..]];
        argv.extend_from_slice(arguments);
        host.run(&argv)
    }

    fn run_bracket(host: &mut TestHost, arguments: &[&[u8]]) -> Status {
        let mut argv: Vec<&[u8]> = vec![&b"["[..]];
        argv.extend_from_slice(arguments);
        host.run(&argv)
    }

    const SUCCESS: Status = Status::SUCCESS;
    const FAILURE: Status = Status::FAILURE;
    const ERROR: Status = Status(2);

    #[test]
    fn one_and_two_argument_rules() {
        let mut host = TestHost::new();
        assert_eq!(run(&mut host, &[]), FAILURE);
        assert_eq!(run(&mut host, &[b""]), FAILURE);
        assert_eq!(run(&mut host, &[b"x"]), SUCCESS);
        assert_eq!(run(&mut host, &[b"!"]), SUCCESS);
        assert_eq!(run(&mut host, &[b"!", b""]), SUCCESS);
        assert_eq!(run(&mut host, &[b"!", b"x"]), FAILURE);
        assert_eq!(run(&mut host, &[b"-n"]), SUCCESS);
        assert_eq!(run(&mut host, &[b"-n", b""]), FAILURE);
        assert_eq!(run(&mut host, &[b"-z", b""]), SUCCESS);
        assert_eq!(run(&mut host, &[b"-n", b"-n"]), SUCCESS);
        assert_eq!(run(&mut host, &[b"-z", b"-z"]), FAILURE);
        // Two arguments with a non-operator first word is a syntax error.
        assert_eq!(run(&mut host, &[b"", b"-a"]), ERROR);
    }

    #[test]
    fn the_a_operator_is_both_unary_and_logical() {
        let mut host = TestHost::new();
        // The `-a -a …` battery from the Oils blog post.
        assert_eq!(run(&mut host, &[b"-a"]), SUCCESS);
        assert_eq!(run(&mut host, &[b"-a", b"-a"]), FAILURE);
        assert_eq!(run(&mut host, &[b"-a", b"-a", b"-a"]), SUCCESS);
        assert_eq!(run(&mut host, &[b"-a", b"-a", b"-a", b"-a"]), FAILURE);
        assert_eq!(
            run(&mut host, &[b"-a", b"-a", b"-a", b"-a", b"-a"]),
            FAILURE
        );
        assert_eq!(
            run(&mut host, &[b"-a", b"-a", b"-a", b"-a", b"-a", b"-a"]),
            ERROR
        );
        assert_eq!(
            run(
                &mut host,
                &[b"-a", b"-a", b"-a", b"-a", b"-a", b"-a", b"-a"]
            ),
            FAILURE
        );
    }

    #[test]
    fn integer_and_string_binaries() {
        let mut host = TestHost::new();
        assert_eq!(run(&mut host, &[b"1", b"-eq", b"1"]), SUCCESS);
        assert_eq!(run(&mut host, &[b"1", b"-ne", b"1"]), FAILURE);
        assert_eq!(run(&mut host, &[b"1", b"-lt", b"2"]), SUCCESS);
        assert_eq!(run(&mut host, &[b"2", b"-ge", b"2"]), SUCCESS);
        // `legal_number` is base 10 with surrounding blanks; `0x10` is not one.
        assert_eq!(run(&mut host, &[b"010", b"-eq", b"10"]), SUCCESS);
        assert_eq!(run(&mut host, &[b" 10 ", b"-eq", b"10"]), SUCCESS);
        assert_eq!(run(&mut host, &[b"0x10", b"-eq", b"16"]), ERROR);
        assert_eq!(run(&mut host, &[b"abc", b"-eq", b"3"]), ERROR);

        assert_eq!(run(&mut host, &[b"a", b"=", b"a"]), SUCCESS);
        assert_eq!(run(&mut host, &[b"a", b"==", b"a"]), SUCCESS);
        assert_eq!(run(&mut host, &[b"a", b"!=", b"b"]), SUCCESS);
        // `<` and `>` compare bytes and are only reachable as operands.
        assert_eq!(run(&mut host, &[b"a", b"<", b"b"]), SUCCESS);
        assert_eq!(run(&mut host, &[b"b", b">", b"a"]), SUCCESS);
        // `-f = -f` is a string comparison, not a unary operator.
        assert_eq!(run(&mut host, &[b"-f", b"=", b"-f"]), SUCCESS);
    }

    #[test]
    fn parentheses_and_logical_chains() {
        let mut host = TestHost::new();
        assert_eq!(run(&mut host, &[b"(", b"x", b")"]), SUCCESS);
        assert_eq!(run(&mut host, &[b"(", b")"]), ERROR);
        assert_eq!(run(&mut host, &[b"(", b"a", b"=", b"a", b")"]), SUCCESS);
        assert_eq!(
            run(&mut host, &[b"1", b"-eq", b"1", b"-a", b"2", b"-eq", b"2"]),
            SUCCESS
        );
        assert_eq!(
            run(&mut host, &[b"1", b"-eq", b"1", b"-o", b"2", b"-eq", b"3"]),
            SUCCESS
        );
        // `-a` binds tighter than `-o`.
        assert_eq!(run(&mut host, &[b"", b"-o", b"x", b"-a", b"y"]), SUCCESS);
        assert_eq!(run(&mut host, &[b"x", b"-a", b""]), FAILURE);
        // A word left over after the parse is an error, not ignored.
        assert_eq!(run(&mut host, &[b"a", b"=", b"a", b"b"]), ERROR);
        assert_eq!(run(&mut host, &[b"1", b"-eq", b"1", b"2"]), ERROR);
    }

    #[test]
    fn file_tests() {
        let directory = env!("CARGO_MANIFEST_DIR");
        let file = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
        let mut host = TestHost::new();
        assert_eq!(run(&mut host, &[b"-d", directory.as_bytes()]), SUCCESS);
        assert_eq!(run(&mut host, &[b"-f", directory.as_bytes()]), FAILURE);
        assert_eq!(run(&mut host, &[b"-e", directory.as_bytes()]), SUCCESS);
        assert_eq!(run(&mut host, &[b"-f", file.as_bytes()]), SUCCESS);
        assert_eq!(run(&mut host, &[b"-s", file.as_bytes()]), SUCCESS);
        assert_eq!(run(&mut host, &[b"-r", file.as_bytes()]), SUCCESS);
        assert_eq!(
            run(&mut host, &[b"-e", b"/nonexistent-baash-test"]),
            FAILURE
        );
        assert_eq!(
            run(&mut host, &[b"!", b"-f", b"/nonexistent-baash-test"]),
            SUCCESS
        );
        assert_eq!(run(&mut host, &[b"-t", b"0"]), FAILURE);
        #[cfg(unix)]
        assert_eq!(run(&mut host, &[b"-O", file.as_bytes()]), SUCCESS);
    }

    #[test]
    fn dash_o_queries_the_set_o_table() {
        let mut host = TestHost::new();
        assert_eq!(run(&mut host, &[b"-o", b"emacs"]), FAILURE);
        host.state.opts.emacs = true;
        assert_eq!(run(&mut host, &[b"-o", b"emacs"]), SUCCESS);
        // A `shopt`-only name is not a `set -o` option.
        host.state.opts.dotglob = true;
        assert_eq!(run(&mut host, &[b"-o", b"dotglob"]), FAILURE);
        assert_eq!(run(&mut host, &[b"-o", b"nosuchoption"]), FAILURE);
    }

    #[test]
    fn dash_v_accepts_positional_numbers() {
        let mut host = TestHost::new();
        // `$0` always exists; `$1` does not in a shell with no positional
        // parameters.
        assert_eq!(run(&mut host, &[b"-v", b"0"]), SUCCESS);
        assert_eq!(run(&mut host, &[b"-v", b"1"]), FAILURE);
        assert_eq!(run(&mut host, &[b"-v", b" "]), FAILURE);
    }

    #[test]
    fn bracket_requires_the_closing_token() {
        let mut host = TestHost::new();
        assert_eq!(run_bracket(&mut host, &[b"x", b"]"]), SUCCESS);
        assert_eq!(run_bracket(&mut host, &[b"]"]), FAILURE);
        assert_eq!(run_bracket(&mut host, &[b"-z", b""]), ERROR);
        assert_eq!(run_bracket(&mut host, &[b"-z", b"", b"]"]), SUCCESS);
        let mut host = TestHost::new();
        let mut argv: Vec<&[u8]> = vec![&b"["[..]];
        assert_eq!(host.run(&argv), ERROR);
        argv.clear();
        let _ = argv;
    }

    #[test]
    fn errors_report_and_return_two() {
        let mut host = TestHost::new();
        assert_eq!(run(&mut host, &[b"1", b"-eq"]), ERROR);
        assert_eq!(host.diagnostics.len(), 1);
        assert_eq!(run(&mut host, &[b"x", b"-foo", b"y"]), ERROR);
        assert_eq!(host.diagnostics.len(), 2);
    }
}
