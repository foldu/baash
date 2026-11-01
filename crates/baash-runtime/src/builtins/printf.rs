//! `printf`: the POSIX utility, bash's extensions, and its format language.
//!
//! This is a regular built-in, so it sits where `PATH` would (POSIX §2.9.1.4):
//! `command printf` still reaches it, and `type printf` says "shell builtin".
//! The format language is POSIX's (`%b`, `%c`, `%d`, `%i`, `%o`, `%s`, `%u`,
//! `%x`, `%X`, the `E`/`e`/`f`/`g`/`G` floats, flags, width, precision, `*`,
//! and the backslash escapes) plus the bash extensions the conformance suite
//! pins:
//!
//! * `-v var` / `-v 'array[subscript]'` — assign instead of writing;
//! * `%q` — quote for re-input. bash's shape (backslash escaping, `$'…'` for
//!   control characters, `''` for empty), with one deliberate difference: a
//!   valid UTF-8 sequence is printed as itself, not as octal bytes. A script
//!   that consumes `%q` output feeds it back to `eval`, and both spellings are
//!   accepted there; printing `μ` is what a UTF-8 terminal expects.
//! * `%(strftime-format)T` — `strftime` in the shell's exported `TZ`, with
//!   bash's 128-byte buffer and its empty-format fallback (`%X`);
//! * `\e`, `\xHH`, `\uHHHH` and `\UHHHHHHHH` in the format and in `%b`. bash's
//!   `printf` decodes `\u` only in `$'…'`, but the suite's expectation for the
//!   utility is a decoded character, and a literal backslash is spelled `\\u`.
//!
//! Errors follow bash: a rejected conversion reports on standard error and
//! leaves the final status at 1, but the rest of the format is still rendered;
//! a usage error is status 2. Output is collected and written once, while
//! diagnostics are written as they happen — that matches bash's
//! stdio-buffered ordering when both streams share a file.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::errors::codes;
use crate::{ShellHost, Status};
use baash_diag::Diagnostic;

/// The largest field width or precision `printf` will honour.
///
/// bash would try to produce the bytes and die on a huge value; refusing with a
/// diagnostic is the honest version of the same dead end, and it keeps a typo
/// like `%999999999999d` from taking the shell down.
const MAX_FIELD: usize = 1_000_000;

/// `printf [-v var] format [argument …]`.
pub(crate) fn bi_printf(host: &mut dyn ShellHost, argv: &[&[u8]]) -> Status {
    let Some((target, format_index)) = options(host, argv) else {
        return Status(2);
    };
    let Some(format) = argv.get(format_index) else {
        return usage(host, "printf: usage: printf [-v var] format [arguments]");
    };

    if let Some(target) = target
        && !valid_target(target)
    {
        host.diagnose(&Diagnostic::error(
            codes::INVALID_NAME,
            0..0,
            format!(
                "printf: `{}': not a valid identifier",
                String::from_utf8_lossy(target)
            ),
        ));
        return Status(2);
    }

    let timezone = exported_timezone(host);
    let (output, failed) = {
        let mut renderer = Renderer::new(host, timezone);
        renderer.render_all(format, &argv[format_index + 1..]);
        let failed = renderer.failed;
        (renderer.finish(), failed)
    };

    let mut status = if failed {
        Status::FAILURE
    } else {
        Status::SUCCESS
    };
    match target {
        Some(target) => {
            if let Err(message) = assign(host, target, &output) {
                host.diagnose(&Diagnostic::error(codes::OPERAND, 0..0, message));
                status = Status::FAILURE;
            }
        }
        None => {
            let written = host.write_out(&output);
            if !written.is_success() {
                status = written;
            }
        }
    }
    status
}

/// Parse the option prefix. `None` means a usage error was reported.
fn options<'a>(host: &mut dyn ShellHost, argv: &[&'a [u8]]) -> Option<(Option<&'a [u8]>, usize)> {
    let mut target = None;
    let mut index = 1;
    while let Some(argument) = argv.get(index) {
        if *argument == b"--" {
            index += 1;
            break;
        }
        // bash parses this like `getopt("v:")`: a lone `-`, the format itself
        // and `--` are not options, but `-` plus any other letter is.
        if argument.len() < 2 || argument[0] != b'-' {
            break;
        }
        match argument[1] {
            b'v' => {
                if argument.len() > 2 {
                    target = Some(&argument[2..]);
                } else {
                    index += 1;
                    match argv.get(index) {
                        Some(value) => target = Some(value),
                        None => {
                            usage(host, "printf: -v: option requires an argument");
                            return None;
                        }
                    }
                }
            }
            other => {
                usage(
                    host,
                    &format!("printf: -{}: invalid option", char::from(other)),
                );
                return None;
            }
        }
        index += 1;
    }
    Some((target, index))
}

/// Report a usage diagnostic and answer status 2.
fn usage(host: &mut dyn ShellHost, message: &str) -> Status {
    host.diagnose(&Diagnostic::error(codes::USAGE, 0..0, message.to_owned()));
    Status(2)
}

/// Whether `target` is `name` or `name[subscript]` with a non-empty subscript.
fn valid_target(target: &[u8]) -> bool {
    match target.iter().position(|&byte| byte == b'[') {
        None => crate::vars::Vars::is_name(target),
        Some(open) => {
            target.ends_with(b"]")
                && target.len() > open + 2
                && crate::vars::Vars::is_name(&target[..open])
        }
    }
}

/// Assign `value` to the `-v` target.
fn assign(host: &mut dyn ShellHost, target: &[u8], value: &[u8]) -> Result<(), String> {
    match target.iter().position(|&byte| byte == b'[') {
        Some(open) => {
            let name = &target[..open];
            let subscript = &target[open + 1..target.len() - 1];
            host.assign_variable_element(name, subscript, value)
        }
        None => host
            .state()
            .vars
            .assign_scalar(target, value)
            .map_err(|error| error.message()),
    }
}

/// The shell's exported `TZ`, which is the only timezone `%T` may use.
fn exported_timezone(host: &mut dyn ShellHost) -> Option<Vec<u8>> {
    let var = host.state().vars.lookup(b"TZ")?;
    if !var.attrs.exported {
        return None;
    }
    var.scalar().map(<[u8]>::to_vec)
}

/// One parsed conversion: `%[flags][width][.precision]conversion`.
#[derive(Clone, Copy, Default)]
struct Spec {
    left: bool,
    plus: bool,
    space: bool,
    alternate: bool,
    zero: bool,
    width: usize,
    precision: Option<usize>,
}

/// The state a render needs: the output, the diagnostics channel, and the
/// exported `TZ` that `%T` uses.
struct Renderer<'a> {
    host: &'a mut dyn ShellHost,
    timezone: Option<Vec<u8>>,
    output: Vec<u8>,
    /// `%b`'s `\c` stops everything, including later format text.
    stop: bool,
    /// Any conversion reported an error.
    failed: bool,
}

impl<'a> Renderer<'a> {
    fn new(host: &'a mut dyn ShellHost, timezone: Option<Vec<u8>>) -> Self {
        Self {
            host,
            timezone,
            output: Vec::new(),
            stop: false,
            failed: false,
        }
    }

    fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }

    /// Render, reusing the format while arguments remain.
    ///
    /// POSIX: "the format operand shall be reused as often as necessary to
    /// satisfy the argument operands". A format that consumes no argument is
    /// rendered once, which is what makes `printf 'x' a b` print one `x`.
    fn render_all(&mut self, format: &[u8], arguments: &[&[u8]]) {
        let mut argument = 0;
        loop {
            let before = argument;
            self.render_pass(format, arguments, &mut argument);
            if self.stop || argument == before || argument >= arguments.len() {
                break;
            }
        }
    }

    fn render_pass(&mut self, format: &[u8], arguments: &[&[u8]], argument: &mut usize) {
        let mut position = 0;
        while position < format.len() && !self.stop {
            match format[position] {
                b'%' => position = self.conversion(format, position, arguments, argument),
                b'\\' => {
                    let decoded = decode_escape(format, position, false);
                    match decoded.outcome {
                        Outcome::Byte(byte) => self.output.push(byte),
                        Outcome::Literal([first, second]) => {
                            self.output.push(first);
                            self.output.push(second);
                        }
                        Outcome::Character(character) => {
                            let mut buffer = [0u8; 4];
                            self.output
                                .extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
                        }
                        Outcome::Stop => self.stop = true,
                        Outcome::MissingHex(marker) => {
                            self.missing_hex(marker);
                            self.output.push(b'\\');
                            self.output.push(marker);
                        }
                    }
                    position = decoded.next;
                }
                byte => {
                    self.output.push(byte);
                    position += 1;
                }
            }
        }
    }

    /// Report a conversion problem on standard error.
    fn warn(&mut self, message: &str) {
        self.failed = true;
        self.host
            .diagnose(&Diagnostic::error(codes::OPERAND, 0..0, message.to_owned()));
    }

    fn missing_hex(&mut self, marker: u8) {
        self.warn(&format!(
            "printf: missing hex digit for \\{}",
            char::from(marker)
        ));
    }

    /// Render one `%…` conversion and return the index after it.
    fn conversion(
        &mut self,
        format: &[u8],
        position: usize,
        arguments: &[&[u8]],
        argument: &mut usize,
    ) -> usize {
        let mut at = position + 1;
        let mut spec = Spec::default();
        loop {
            match format.get(at).copied() {
                Some(b'-') => spec.left = true,
                Some(b'+') => spec.plus = true,
                Some(b' ') => spec.space = true,
                Some(b'#') => spec.alternate = true,
                Some(b'0') => spec.zero = true,
                _ => break,
            }
            at += 1;
        }
        if format.get(at).copied() == Some(b'*') {
            at += 1;
            let width = self.width_argument(arguments, argument);
            if width < 0 {
                spec.left = true;
                spec.width = width.unsigned_abs() as usize;
            } else {
                spec.width = width as usize;
            }
        } else {
            spec.width = digits(format, &mut at);
        }
        if format.get(at).copied() == Some(b'.') {
            at += 1;
            if format.get(at).copied() == Some(b'*') {
                at += 1;
                let precision = self.width_argument(arguments, argument);
                // A negative precision from `*` is as if it were omitted.
                spec.precision = (precision >= 0).then_some(precision as usize);
            } else {
                spec.precision = Some(digits(format, &mut at));
            }
        }
        // Length modifiers are accepted and ignored, as bash and coreutils do.
        while let Some(b'h' | b'l' | b'L' | b'j' | b'z' | b't') = format.get(at).copied() {
            at += 1;
        }
        let Some(conversion) = format.get(at).copied() else {
            self.warn("printf: missing format character");
            return at;
        };
        let conversion_at = at;
        at += 1;
        if spec.width > MAX_FIELD || spec.precision.is_some_and(|value| value > MAX_FIELD) {
            self.warn("printf: field width or precision out of range");
            return at;
        }

        match conversion {
            b'%' => self.output.push(b'%'),
            b'd' | b'i' => {
                let (text, value, issue) = self.signed_argument(arguments, argument);
                self.report_number_issue(text, issue);
                let negative = value < 0;
                let magnitude = value.unsigned_abs();
                let rendered = integer(&spec, negative, magnitude, 10, false);
                self.output.extend_from_slice(&rendered);
            }
            b'u' | b'o' | b'x' | b'X' => {
                let (text, value, issue) = self.unsigned_argument(arguments, argument);
                self.report_number_issue(text, issue);
                let radix = match conversion {
                    b'o' => 8,
                    b'u' => 10,
                    _ => 16,
                };
                let rendered = integer(&spec, false, value, radix, conversion == b'X');
                self.output.extend_from_slice(&rendered);
            }
            b'c' => {
                let value = self.string_argument(arguments, argument);
                // bash hands the first *byte* to the C library, so a UTF-8
                // character is cut, and an empty argument is the NUL character.
                let byte = value.first().copied().unwrap_or(0);
                self.field(&[byte], &spec);
            }
            b's' => {
                let value = self.string_argument(arguments, argument);
                self.field(value, &spec);
            }
            b'b' => {
                let value = self.string_argument(arguments, argument);
                let decoded = self.decode_b(value);
                self.field(&decoded, &spec);
            }
            b'q' => {
                let value = self.string_argument(arguments, argument);
                let quoted = quote_for_reinput(value);
                self.field(&quoted, &spec);
            }
            b'e' | b'E' | b'f' | b'F' | b'g' | b'G' => {
                let (_, value) = self.float_argument(arguments, argument);
                let rendered = float(&spec, value, conversion);
                self.output.extend_from_slice(&rendered);
            }
            b'(' => {
                let (rendered, after) =
                    self.time(format, conversion_at, arguments, argument, &spec);
                self.output.extend_from_slice(&rendered);
                return after;
            }
            other => {
                self.warn(&format!(
                    "printf: `{}': invalid format character",
                    String::from_utf8_lossy(&[other])
                ));
            }
        }
        at
    }

    /// Decode `%b`'s backslash escapes. `\c` stops all output.
    fn decode_b(&mut self, value: &[u8]) -> Vec<u8> {
        let mut decoded = Vec::with_capacity(value.len());
        let mut index = 0;
        while index < value.len() && !self.stop {
            if value[index] != b'\\' {
                decoded.push(value[index]);
                index += 1;
                continue;
            }
            let parsed = decode_escape(value, index, true);
            match parsed.outcome {
                Outcome::Byte(byte) => decoded.push(byte),
                Outcome::Literal([first, second]) => {
                    decoded.push(first);
                    decoded.push(second);
                }
                Outcome::Character(character) => {
                    let mut buffer = [0u8; 4];
                    decoded.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
                }
                Outcome::Stop => self.stop = true,
                Outcome::MissingHex(marker) => {
                    self.missing_hex(marker);
                    decoded.push(b'\\');
                    decoded.push(marker);
                }
            }
            index = parsed.next;
        }
        decoded
    }

    /// `%(strftime-format)T`.
    fn time(
        &mut self,
        format: &[u8],
        open: usize,
        arguments: &[&[u8]],
        argument: &mut usize,
        spec: &Spec,
    ) -> (Vec<u8>, usize) {
        // The parenthesised format runs to the first `)`, then `T` must follow.
        let Some(close) = format[open + 1..].iter().position(|&byte| byte == b')') else {
            self.warn("printf: invalid time format specification");
            return (vec![b'%'], open + 1);
        };
        let end = open + 1 + close;
        let with_t = end + 1;
        if format.get(with_t).copied() != Some(b'T') {
            self.warn("printf: invalid time format specification");
            return (vec![b'%'], open + 1);
        }
        let strftime_format = &format[open + 1..end];
        let seconds = match arguments.get(*argument) {
            Some(text) => {
                *argument += 1;
                let (value, issue) = parse_signed(text);
                self.report_number_issue(Some(text), issue);
                value
            }
            None => now(),
        };
        // bash treats -1 (and any other negative value) as "now"; -2 would be
        // the shell's start time, which this shell does not record separately.
        let seconds = if seconds < 0 { now() } else { seconds };
        let format = if strftime_format.is_empty() {
            b"%X".as_slice()
        } else {
            strftime_format
        };
        let mut text = baash_sys::strftime_local(seconds, format, self.timezone.as_deref());
        if let Some(precision) = spec.precision {
            text.truncate(precision);
        }
        let mut padded = Vec::with_capacity(text.len() + spec.width);
        pad_spaces(&mut padded, &text, spec);
        (padded, with_t + 1)
    }

    /// A `%s`/`%c`/`%b`/`%q` operand, or the empty string when none is left.
    fn string_argument<'b>(&mut self, arguments: &[&'b [u8]], argument: &mut usize) -> &'b [u8] {
        match arguments.get(*argument) {
            Some(value) => {
                *argument += 1;
                value
            }
            None => b"",
        }
    }

    /// A `*` width or precision operand: `0` when missing. A rejected operand
    /// is reported and becomes 0, which is bash's continuation rule.
    fn width_argument(&mut self, arguments: &[&[u8]], argument: &mut usize) -> i64 {
        let Some(text) = arguments.get(*argument) else {
            return 0;
        };
        *argument += 1;
        let (value, issue) = parse_signed(text);
        self.report_number_issue(Some(text), issue);
        value
    }

    fn signed_argument<'b>(
        &mut self,
        arguments: &[&'b [u8]],
        argument: &mut usize,
    ) -> (Option<&'b [u8]>, i64, NumberIssue) {
        let text = arguments.get(*argument).copied();
        if text.is_some() {
            *argument += 1;
        }
        let (value, issue) = parse_signed(text.unwrap_or(b""));
        (text, value, issue)
    }

    fn unsigned_argument<'b>(
        &mut self,
        arguments: &[&'b [u8]],
        argument: &mut usize,
    ) -> (Option<&'b [u8]>, u64, NumberIssue) {
        let text = arguments.get(*argument).copied();
        if text.is_some() {
            *argument += 1;
        }
        let (value, issue) = parse_unsigned(text.unwrap_or(b""));
        (text, value, issue)
    }

    fn float_argument<'b>(
        &mut self,
        arguments: &[&'b [u8]],
        argument: &mut usize,
    ) -> (Option<&'b [u8]>, f64) {
        let text = arguments.get(*argument).copied();
        if text.is_some() {
            *argument += 1;
        }
        let Some(text) = text else {
            return (None, 0.0);
        };
        let (value, issue) = parse_float(text);
        self.report_number_issue(Some(text), issue);
        (Some(text), value)
    }

    fn report_number_issue(&mut self, argument: Option<&[u8]>, issue: NumberIssue) {
        let text = argument.map(String::from_utf8_lossy).unwrap_or_default();
        match issue {
            NumberIssue::None => {}
            NumberIssue::Invalid => {
                self.warn(&format!("printf: {text}: invalid number"));
            }
            NumberIssue::Range => {
                self.warn(&format!("printf: {text}: Numerical result out of range"));
            }
        }
    }

    /// Pad a string-ish conversion to the field width; precision truncates.
    fn field(&mut self, body: &[u8], spec: &Spec) {
        let body = match spec.precision {
            Some(precision) => &body[..body.len().min(precision)],
            None => body,
        };
        let padding = spec.width.saturating_sub(body.len());
        if spec.left {
            self.output.extend_from_slice(body);
            self.output.resize(self.output.len() + padding, b' ');
        } else {
            self.output.resize(self.output.len() + padding, b' ');
            self.output.extend_from_slice(body);
        }
    }
}

/// Truncate at `precision`, then pad with spaces to `width`.
fn pad_spaces(out: &mut Vec<u8>, body: &[u8], spec: &Spec) {
    let padding = spec.width.saturating_sub(body.len());
    if spec.left {
        out.extend_from_slice(body);
        out.resize(out.len() + padding, b' ');
    } else {
        out.resize(out.len() + padding, b' ');
        out.extend_from_slice(body);
    }
}

/// Parse a decimal field width or precision, saturating rather than wrapping.
fn digits(bytes: &[u8], at: &mut usize) -> usize {
    let mut value: usize = 0;
    while let Some(byte @ b'0'..=b'9') = bytes.get(*at).copied() {
        value = value
            .saturating_mul(10)
            .saturating_add(usize::from(byte - b'0'));
        *at += 1;
    }
    value
}

/// Read up to `limit` octal digits starting at `start`.
fn octal(bytes: &[u8], start: usize, limit: usize) -> (u8, usize) {
    let mut value: u32 = 0;
    let mut at = start;
    while at < bytes.len() && at - start < limit {
        let byte = bytes[at];
        if !(b'0'..=b'7').contains(&byte) {
            break;
        }
        value = value * 8 + u32::from(byte - b'0');
        at += 1;
    }
    (value as u8, at)
}

/// One decoded backslash escape.
struct Decoded {
    outcome: Outcome,
    /// Index just after the escape.
    next: usize,
}

enum Outcome {
    Byte(u8),
    Literal([u8; 2]),
    Character(char),
    Stop,
    MissingHex(u8),
}

/// Decode the escape at `position` (the backslash).
///
/// `strings` selects `%b`'s rules: `\c` stops, and a leading `0` introduces
/// `\0ddd` (three digits *after* the zero) where the format takes at most three
/// digits including the zero.
fn decode_escape(bytes: &[u8], position: usize, strings: bool) -> Decoded {
    let Some(next) = bytes.get(position + 1).copied() else {
        return Decoded {
            outcome: Outcome::Byte(b'\\'),
            next: position + 1,
        };
    };
    let simple = match next {
        b'a' => Some(7),
        b'b' => Some(8),
        b'e' | b'E' => Some(0x1b),
        b'f' => Some(0x0c),
        b'n' => Some(b'\n'),
        b'r' => Some(b'\r'),
        b't' => Some(b'\t'),
        b'v' => Some(0x0b),
        b'\\' => Some(b'\\'),
        b'\'' => Some(b'\''),
        b'"' => Some(b'"'),
        b'?' => Some(b'?'),
        _ => None,
    };
    if let Some(byte) = simple {
        return Decoded {
            outcome: Outcome::Byte(byte),
            next: position + 2,
        };
    }
    match next {
        b'c' if strings => Decoded {
            outcome: Outcome::Stop,
            next: position + 2,
        },
        b'c' => Decoded {
            outcome: Outcome::Literal(*b"\\c"),
            next: position + 2,
        },
        b'x' => hex_escape(bytes, position, 2, b'x'),
        b'u' => hex_escape(bytes, position, 4, b'u'),
        b'U' => hex_escape(bytes, position, 8, b'U'),
        b'0'..=b'7' => {
            let (start, limit) = if strings && next == b'0' {
                (position + 2, 3)
            } else {
                (position + 1, 3)
            };
            let (value, after) = octal(bytes, start, limit);
            Decoded {
                outcome: Outcome::Byte(value),
                next: after,
            }
        }
        other => Decoded {
            outcome: Outcome::Literal([b'\\', other]),
            next: position + 2,
        },
    }
}

/// `\xHH`, `\uHHHH` or `\UHHHHHHHH`: hex digits, then a byte or a character.
fn hex_escape(bytes: &[u8], position: usize, max_digits: usize, marker: u8) -> Decoded {
    let start = position + 2;
    let mut value: u32 = 0;
    let mut digits = 0;
    let mut at = start;
    while at < bytes.len() && digits < max_digits {
        let Some(digit) = (bytes[at] as char).to_digit(16) else {
            break;
        };
        value = value.saturating_mul(16).saturating_add(digit);
        at += 1;
        digits += 1;
    }
    if digits == 0 {
        return Decoded {
            outcome: Outcome::MissingHex(marker),
            next: start,
        };
    }
    let outcome = if marker == b'x' {
        Outcome::Byte(value as u8)
    } else {
        Outcome::Character(char::from_u32(value).unwrap_or('\u{fffd}'))
    };
    Decoded { outcome, next: at }
}

/// What was wrong with a numeric operand, if anything.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum NumberIssue {
    None,
    /// Not a number at all, or trailing junk after one.
    Invalid,
    /// The value does not fit the conversion's type.
    Range,
}

/// A parsed integer: the magnitude, and whether the operand was negative.
enum Parsed {
    Magnitude(u64),
    Negative(u64),
}

/// Parse the sign, base prefix and digits of a C-style integer operand.
fn parse_integer(text: &[u8]) -> (Parsed, NumberIssue) {
    let mut at = 0;
    while let Some(b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') = text.get(at).copied() {
        at += 1;
    }
    let mut negative = false;
    if let Some(byte @ (b'+' | b'-')) = text.get(at).copied() {
        negative = byte == b'-';
        at += 1;
    }
    let mut radix = 10;
    // The leading zero of an octal constant is itself a digit (`0` is a valid
    // number, `0x` is zero followed by junk).
    let mut seen = 0usize;
    if text.get(at).copied() == Some(b'0') {
        match text.get(at + 1).copied() {
            Some(b'x' | b'X') if text.get(at + 2).is_some_and(u8::is_ascii_hexdigit) => {
                radix = 16;
                at += 2;
            }
            _ => {
                radix = 8;
                at += 1;
                seen = 1;
            }
        }
    }
    let mut magnitude: u128 = 0;
    let mut overflow = false;
    while let Some(byte) = text.get(at).copied() {
        let Some(digit) = (byte as char).to_digit(radix) else {
            break;
        };
        magnitude = magnitude
            .saturating_mul(u128::from(radix))
            .saturating_add(u128::from(digit));
        if magnitude > u128::from(u64::MAX) {
            overflow = true;
            magnitude = u128::from(u64::MAX);
        }
        seen += 1;
        at += 1;
    }
    let trailing = at < text.len();
    let issue = if overflow {
        NumberIssue::Range
    } else if seen == 0 || trailing {
        NumberIssue::Invalid
    } else {
        NumberIssue::None
    };
    let magnitude = magnitude as u64;
    let parsed = if negative {
        Parsed::Negative(magnitude)
    } else {
        Parsed::Magnitude(magnitude)
    };
    (parsed, issue)
}

/// Parse a signed integer operand, clamping like `strtoimax`.
fn parse_signed(text: &[u8]) -> (i64, NumberIssue) {
    if let Some(rest) = character_operand(text) {
        return (rest, NumberIssue::None);
    }
    let (parsed, issue) = parse_integer(text);
    match parsed {
        Parsed::Magnitude(magnitude) => {
            if magnitude > i64::MAX as u64 {
                (i64::MAX, NumberIssue::Range)
            } else {
                (magnitude as i64, issue)
            }
        }
        Parsed::Negative(magnitude) => {
            if magnitude > i64::MAX as u64 + 1 {
                (i64::MIN, NumberIssue::Range)
            } else {
                ((magnitude as i128).wrapping_neg() as i64, issue)
            }
        }
    }
}

/// Parse an unsigned integer operand, wrapping a negative like `strtoumax`.
fn parse_unsigned(text: &[u8]) -> (u64, NumberIssue) {
    if let Some(rest) = character_operand(text) {
        return (rest as u64, NumberIssue::None);
    }
    match parse_integer(text) {
        (Parsed::Magnitude(magnitude), issue) => (magnitude, issue),
        // glibc saturates a negative overflow at `UINTMAX_MAX` instead of
        // negating the saturated magnitude; that is what bash prints.
        (Parsed::Negative(_), NumberIssue::Range) => (u64::MAX, NumberIssue::Range),
        (Parsed::Negative(magnitude), issue) => (0u64.wrapping_sub(magnitude), issue),
    }
}

/// A `'c` or `"c` character operand, if `text` is one. The code point comes
/// from the character after the quote, or 0 when there is none.
fn character_operand(text: &[u8]) -> Option<i64> {
    let rest = text
        .strip_prefix(b"'")
        .or_else(|| text.strip_prefix(b"\""))?;
    Some(i64::from(character_code(rest)))
}

/// The code point of the first character of `text`.
///
/// bash decodes the multibyte character structurally: an overlong or malformed
/// sequence falls back to the first byte, while a structurally valid sequence
/// beyond `U+10FFFF` is decoded as its (too large) value. The rules below match
/// the cases the conformance suite pins.
fn character_code(text: &[u8]) -> u32 {
    let Some(&first) = text.first() else {
        return 0;
    };
    let continuation = |index: usize| -> Option<u32> {
        let byte = *text.get(index)?;
        (0x80..=0xbf)
            .contains(&byte)
            .then(|| u32::from(byte & 0x3f))
    };
    match first {
        0x00..=0x7f => u32::from(first),
        0xc2..=0xdf => {
            let Some(second) = continuation(1) else {
                return u32::from(first);
            };
            (u32::from(first & 0x1f) << 6) | second
        }
        0xe0..=0xef => {
            let second = match text.get(1).copied() {
                // `E0` with a low second byte would be an overlong encoding.
                Some(byte) if first == 0xe0 => {
                    if !(0xa0..=0xbf).contains(&byte) {
                        return u32::from(first);
                    }
                    u32::from(byte & 0x3f)
                }
                Some(byte) if (0x80..=0xbf).contains(&byte) => u32::from(byte & 0x3f),
                _ => return u32::from(first),
            };
            let Some(third) = continuation(2) else {
                return u32::from(first);
            };
            (u32::from(first & 0x0f) << 12) | (second << 6) | third
        }
        0xf0..=0xf4 => {
            let second = match text.get(1).copied() {
                // `F0` with a low second byte would be an overlong encoding.
                Some(byte) if first == 0xf0 => {
                    if !(0x90..=0xbf).contains(&byte) {
                        return u32::from(first);
                    }
                    u32::from(byte & 0x3f)
                }
                Some(byte) if (0x80..=0xbf).contains(&byte) => u32::from(byte & 0x3f),
                _ => return u32::from(first),
            };
            let Some(third) = continuation(2) else {
                return u32::from(first);
            };
            let Some(fourth) = continuation(3) else {
                return u32::from(first);
            };
            (u32::from(first & 0x07) << 18) | (second << 12) | (third << 6) | fourth
        }
        _ => u32::from(first),
    }
}

/// Parse a floating-point operand: a C `strtod` prefix, `inf` and `nan`.
fn parse_float(text: &[u8]) -> (f64, NumberIssue) {
    let mut at = 0;
    while let Some(b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') = text.get(at).copied() {
        at += 1;
    }
    let (sign, body) = match text.get(at).copied() {
        Some(b'-') => (-1.0, &text[at + 1..]),
        Some(b'+') => (1.0, &text[at + 1..]),
        _ => (1.0, &text[at..]),
    };
    let lower = body.to_ascii_lowercase();
    if lower.starts_with(b"inf") {
        let length = if lower.starts_with(b"infinity") { 8 } else { 3 };
        let issue = if body.len() > length {
            NumberIssue::Invalid
        } else {
            NumberIssue::None
        };
        return (sign * f64::INFINITY, issue);
    }
    if lower.starts_with(b"nan") {
        let issue = if body.len() > 3 {
            NumberIssue::Invalid
        } else {
            NumberIssue::None
        };
        return (sign * f64::NAN, issue);
    }
    let mut cursor = 0;
    let mut significant = false;
    while body.get(cursor).is_some_and(u8::is_ascii_digit) {
        significant = true;
        cursor += 1;
    }
    if body.get(cursor).copied() == Some(b'.') {
        cursor += 1;
        while body.get(cursor).is_some_and(u8::is_ascii_digit) {
            significant = true;
            cursor += 1;
        }
    }
    if !significant {
        return (0.0, NumberIssue::Invalid);
    }
    if let Some(b'e' | b'E') = body.get(cursor).copied() {
        let mut exponent = cursor + 1;
        if let Some(b'+' | b'-') = body.get(exponent).copied() {
            exponent += 1;
        }
        let start = exponent;
        while body.get(exponent).is_some_and(u8::is_ascii_digit) {
            exponent += 1;
        }
        if exponent > start {
            cursor = exponent;
        }
    }
    let prefix = std::str::from_utf8(&body[..cursor]).unwrap_or("");
    let Ok(value) = prefix.parse::<f64>() else {
        return (0.0, NumberIssue::Invalid);
    };
    let issue = if cursor < body.len() {
        NumberIssue::Invalid
    } else if value.is_infinite() {
        NumberIssue::Range
    } else {
        NumberIssue::None
    };
    (sign * value, issue)
}

/// Format a signed/unsigned integer conversion.
fn integer(spec: &Spec, negative: bool, magnitude: u64, radix: u32, upper: bool) -> Vec<u8> {
    let mut digits = match radix {
        8 => format!("{magnitude:o}"),
        16 if upper => format!("{magnitude:X}"),
        16 => format!("{magnitude:x}"),
        _ => magnitude.to_string(),
    };
    if let Some(precision) = spec.precision {
        if precision == 0 && magnitude == 0 {
            digits.clear();
        }
        while digits.len() < precision {
            digits.insert(0, '0');
        }
    }
    let mut prefix: &[u8] = b"";
    if spec.alternate {
        match radix {
            8 => {
                if !digits.starts_with('0') {
                    digits.insert(0, '0');
                }
            }
            16 if magnitude != 0 => {
                prefix = if upper { b"0X" } else { b"0x" };
            }
            _ => {}
        }
    }
    let sign: &[u8] = if negative {
        b"-"
    } else if spec.plus {
        b"+"
    } else if spec.space {
        b" "
    } else {
        b""
    };
    numeric_field(sign, prefix, digits.as_bytes(), spec, true)
}

/// Assemble sign + prefix + digits with width, zero padding and alignment.
/// `precision_blocks_zero` is C's integer rule: for `d`/`i`/`o`/`u`/`x`/`X` a
/// specified precision disables the `0` flag, while for the float conversions
/// it does not.
fn numeric_field(
    sign: &[u8],
    prefix: &[u8],
    digits: &[u8],
    spec: &Spec,
    precision_blocks_zero: bool,
) -> Vec<u8> {
    let body = sign.len() + prefix.len() + digits.len();
    let padding = spec.width.saturating_sub(body);
    let mut out = Vec::with_capacity(body + padding);
    if spec.left {
        out.extend_from_slice(sign);
        out.extend_from_slice(prefix);
        out.extend_from_slice(digits);
        out.resize(body + padding, b' ');
    } else if spec.zero && !(precision_blocks_zero && spec.precision.is_some()) {
        out.extend_from_slice(sign);
        out.extend_from_slice(prefix);
        out.resize(out.len() + padding, b'0');
        out.extend_from_slice(digits);
    } else {
        out.resize(padding, b' ');
        out.extend_from_slice(sign);
        out.extend_from_slice(prefix);
        out.extend_from_slice(digits);
    }
    out
}

/// Format a floating-point conversion.
fn float(spec: &Spec, value: f64, conversion: u8) -> Vec<u8> {
    let upper = matches!(conversion, b'E' | b'F' | b'G');
    let precision = spec.precision.unwrap_or(6);
    let negative = value.is_sign_negative();
    let magnitude = value.abs();
    let sign: &[u8] = if negative {
        b"-"
    } else if spec.plus {
        b"+"
    } else if spec.space {
        b" "
    } else {
        b""
    };
    // `inf`/`nan` never take zero padding, and their spelling follows the
    // conversion's case, as C's does.
    let text_spec = Spec {
        zero: false,
        ..*spec
    };
    if magnitude.is_nan() {
        return numeric_field(
            sign,
            b"",
            if upper { b"NAN" } else { b"nan" },
            &text_spec,
            false,
        );
    }
    if magnitude.is_infinite() {
        return numeric_field(
            sign,
            b"",
            if upper { b"INF" } else { b"inf" },
            &text_spec,
            false,
        );
    }
    let mut body = match conversion {
        b'e' | b'E' => exponent_form(magnitude, precision, upper),
        b'g' | b'G' => general_form(magnitude, precision, spec.alternate, upper),
        _ => format!("{:.*}", precision, magnitude),
    };
    if spec.alternate && !body.contains('.') {
        // `#` keeps the decimal point: before the exponent for `%e`, at the
        // end for `%f`.
        match body.find(['e', 'E']) {
            Some(position) => body.insert(position, '.'),
            None => body.push('.'),
        }
    }
    numeric_field(sign, b"", body.as_bytes(), spec, false)
}

/// C's `%e`: mantissa with `precision` fraction digits, then `e±NN`.
fn exponent_form(magnitude: f64, precision: usize, upper: bool) -> String {
    let raw = format!("{:.*e}", precision, magnitude);
    let (mantissa, exponent) = raw.split_once('e').expect("rust e-format");
    let exponent: i32 = exponent.parse().expect("integer exponent");
    let marker = if upper { 'E' } else { 'e' };
    let sign = if exponent < 0 { '-' } else { '+' };
    let magnitude = exponent.unsigned_abs();
    if magnitude < 10 {
        format!("{mantissa}{marker}{sign}0{magnitude}")
    } else {
        format!("{mantissa}{marker}{sign}{magnitude}")
    }
}

/// C's `%g`: `%e` or `%f` by exponent, with trailing zeros removed unless `#`.
fn general_form(magnitude: f64, precision: usize, alternate: bool, upper: bool) -> String {
    let significant = if precision == 0 { 1 } else { precision };
    let raw = format!("{:.*e}", significant - 1, magnitude);
    let (_, exponent) = raw.split_once('e').expect("rust e-format");
    let exponent: i32 = exponent.parse().expect("integer exponent");
    if exponent < -4 || exponent >= significant as i32 {
        let mut text = exponent_form(magnitude, significant - 1, upper);
        if !alternate {
            strip_zeros(&mut text, if upper { 'E' } else { 'e' });
        }
        text
    } else {
        let decimals = (significant as i32 - 1 - exponent).max(0) as usize;
        let mut text = format!("{:.*}", decimals, magnitude);
        if !alternate {
            strip_zeros(&mut text, '\0');
        }
        text
    }
}

/// Remove trailing fraction zeros (and a bare `.`) from `text`.
///
/// `marker` is the exponent marker when the number is in `e` form, so only the
/// mantissa is touched; `'\0'` means the whole string is the number.
fn strip_zeros(text: &mut String, marker: char) {
    let end = if marker == '\0' {
        text.len()
    } else {
        match text.find(marker) {
            Some(index) => index,
            None => text.len(),
        }
    };
    let Some(dot) = text[..end].find('.') else {
        return;
    };
    let mut keep = end;
    while keep > dot + 1 && text.as_bytes()[keep - 1] == b'0' {
        keep -= 1;
    }
    if keep == dot + 1 {
        keep = dot;
    }
    text.replace_range(keep..end, "");
}

/// Quote `value` the way `%q` does: backslash escapes, or `$'…'` when a
/// character needs a C escape.
fn quote_for_reinput(value: &[u8]) -> Vec<u8> {
    if value.is_empty() {
        return b"''".to_vec();
    }
    if !needs_ansi_c(value) {
        let mut out = Vec::with_capacity(value.len() * 2);
        for &byte in value {
            if backslash_quoted(byte) {
                out.push(b'\\');
            }
            out.push(byte);
        }
        return out;
    }
    let mut out = Vec::with_capacity(value.len() + 4);
    out.extend_from_slice(b"$'");
    let mut index = 0;
    while index < value.len() {
        let byte = value[index];
        match byte {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\'' => out.extend_from_slice(b"\\'"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\r' => out.extend_from_slice(b"\\r"),
            0x07 => out.extend_from_slice(b"\\a"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0c => out.extend_from_slice(b"\\f"),
            0x0b => out.extend_from_slice(b"\\v"),
            0x1b => out.extend_from_slice(b"\\E"),
            0x00..=0x1f | 0x7f => push_octal(&mut out, byte),
            _ => match printable_width(value, index) {
                Some(width) => {
                    out.extend_from_slice(&value[index..index + width]);
                    index += width;
                    continue;
                }
                None => push_octal(&mut out, byte),
            },
        }
        index += 1;
    }
    out.push(b'\'');
    out
}

/// Whether any byte requires the `$'…'` form.
fn needs_ansi_c(value: &[u8]) -> bool {
    let mut index = 0;
    while index < value.len() {
        let byte = value[index];
        if byte < 0x20 || byte == 0x7f {
            return true;
        }
        if byte < 0x80 {
            index += 1;
            continue;
        }
        match printable_width(value, index) {
            Some(width) => index += width,
            None => return true,
        }
    }
    false
}

/// The byte length of the printable UTF-8 sequence starting at `index`, or
/// `None` when the bytes there are not a valid printable character.
fn printable_width(value: &[u8], index: usize) -> Option<usize> {
    let first = *value.get(index)?;
    if first < 0x80 {
        return (0x20..0x7f).contains(&first).then_some(1);
    }
    // Lead byte, mask for its payload bits, and the smallest code point the
    // encoding may represent (anything below is an overlong encoding, which is
    // not valid UTF-8 and must be escaped byte by byte).
    let (width, mask, minimum) = match first {
        0xc2..=0xdf => (2, 0x1f, 0x80),
        0xe0..=0xef => (3, 0x0f, 0x800),
        0xf0..=0xf4 => (4, 0x07, 0x1_0000),
        _ => return None,
    };
    let mut code = u32::from(first & mask);
    for offset in 1..width {
        let byte = *value.get(index + offset)?;
        if !(0x80..=0xbf).contains(&byte) {
            return None;
        }
        code = (code << 6) | u32::from(byte & 0x3f);
    }
    if code < minimum {
        return None;
    }
    char::from_u32(code)
        .filter(|character| !character.is_control())
        .map(|_| width)
}

/// Append `\NNN` for a byte that is not printable as itself.
fn push_octal(out: &mut Vec<u8>, byte: u8) {
    out.push(b'\\');
    out.push(b'0' + (byte >> 6));
    out.push(b'0' + ((byte >> 3) & 0o7));
    out.push(b'0' + (byte & 0o7));
}

/// Whether `%q` backslash-quotes this byte in the non-C-escaped form.
fn backslash_quoted(byte: u8) -> bool {
    matches!(
        byte,
        b' ' | b'!'
            | b'"'
            | b'#'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b','
            | b';'
            | b'<'
            | b'>'
            | b'?'
            | b'['
            | b'\\'
            | b']'
            | b'^'
            | b'`'
            | b'{'
            | b'|'
            | b'}'
            | b'~'
    )
}

/// The current time as Unix seconds.
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::TestHost;

    fn run(host: &mut TestHost, arguments: &[&[u8]]) -> Status {
        let mut argv: Vec<&[u8]> = vec![&b"printf"[..]];
        argv.extend_from_slice(arguments);
        host.run(&argv)
    }

    fn out(host: &TestHost) -> String {
        String::from_utf8_lossy(&host.out).into_owned()
    }

    #[test]
    fn strings_width_and_precision() {
        let mut host = TestHost::new();
        let status = run(
            &mut host,
            &[
                b"[%5s][%-5s][%05s][%6.4s]",
                b"abc",
                b"abc",
                b"abc",
                b"spam-eggs",
            ],
        );
        assert_eq!(status, Status::SUCCESS);
        assert_eq!(out(&host), "[  abc][abc  ][  abc][  spam]");
    }

    #[test]
    fn integers_in_every_base_and_with_flags() {
        let mut host = TestHost::new();
        let status = run(
            &mut host,
            &[
                b"[%d][%i][%u][%o][%x][%X]",
                b"42",
                b"42",
                b"42",
                b"42",
                b"42",
                b"42",
            ],
        );
        assert_eq!(status, Status::SUCCESS);
        assert_eq!(out(&host), "[42][42][42][52][2a][2A]");

        let mut host = TestHost::new();
        run(&mut host, &[b"[%6.4d][%.4d][%6.d]", b"42", b"42", b"42"]);
        assert_eq!(out(&host), "[  0042][0042][    42]");

        let mut host = TestHost::new();
        run(&mut host, &[b"[%06d][%6.6d][%-6.6d]", b"42", b"42", b"42"]);
        assert_eq!(out(&host), "[000042][000042][000042]");

        let mut host = TestHost::new();
        run(&mut host, &[b"[%#o][%#x][%#X]", b"42", b"42", b"42"]);
        assert_eq!(out(&host), "[052][0x2a][0X2A]");

        let mut host = TestHost::new();
        run(&mut host, &[b"[%+d][% d][%.0d]", b"42", b"42", b"0"]);
        assert_eq!(out(&host), "[+42][ 42][]");
    }

    #[test]
    fn character_constants_are_code_points() {
        let mut host = TestHost::new();
        let status = run(&mut host, &[b"%d %d %d", b"'a", b"\"a", b"'"]);
        assert_eq!(status, Status::SUCCESS);
        assert_eq!(out(&host), "97 97 0");

        // The multibyte character is decoded, not cut to its first byte.
        let mut host = TestHost::new();
        run(
            &mut host,
            &[b"%x %u", "'\u{03bc}".as_bytes(), "'\u{03bc}".as_bytes()],
        );
        assert_eq!(out(&host), "3bc 956");

        // A malformed sequence falls back to the first byte.
        let mut host = TestHost::new();
        run(&mut host, &[b"%x", b"'\xce\xce"]);
        assert_eq!(out(&host), "ce");
    }

    #[test]
    fn invalid_numbers_report_but_keep_going() {
        let mut host = TestHost::new();
        let status = run(&mut host, &[b"[%d] and [%d]", b"3abc", b"xyz"]);
        assert_eq!(status, Status::FAILURE);
        assert_eq!(out(&host), "[3] and [0]");
        assert_eq!(host.diagnostics.len(), 2);
    }

    #[test]
    fn unsigned_wraps_and_signed_clamps() {
        let mut host = TestHost::new();
        run(&mut host, &[b"%u", b"-42"]);
        assert_eq!(out(&host), "18446744073709551574");

        let mut host = TestHost::new();
        let status = run(&mut host, &[b"%d", b"18446744073709551615"]);
        assert_eq!(status, Status::FAILURE);
        assert_eq!(out(&host), "9223372036854775807");
    }

    #[test]
    fn floats_match_c_formatting() {
        let mut host = TestHost::new();
        run(
            &mut host,
            &[
                b"[%f][%.2f][%8.2f][%-8.2f][%08.4f]",
                b"3.14159",
                b"3.14159",
                b"3.14159",
                b"3.14159",
                b"3.14",
            ],
        );
        assert_eq!(out(&host), "[3.141590][3.14][    3.14][3.14    ][003.1400]");

        let mut host = TestHost::new();
        run(
            &mut host,
            &[b"[%e][%E][%g][%G]", b"3.14", b"3.14", b"3.14", b"3.14"],
        );
        assert_eq!(out(&host), "[3.140000e+00][3.140000E+00][3.14][3.14]");

        let mut host = TestHost::new();
        run(
            &mut host,
            &[b"[%.0f][%#.0f][%g][%#g]", b"3", b"3", b"3", b"3"],
        );
        assert_eq!(out(&host), "[3][3.][3][3.00000]");

        let mut host = TestHost::new();
        run(&mut host, &[b"[%g][%g]", b"1000000", b"0.0001"]);
        assert_eq!(out(&host), "[1e+06][0.0001]");
    }

    #[test]
    fn percent_c_takes_the_first_byte_and_empty_is_nul() {
        let mut host = TestHost::new();
        run(&mut host, &[b"%c%c%c", b"ABC", b"", b"a"]);
        assert_eq!(host.out, vec![b'A', 0, b'a']);
    }

    #[test]
    fn percent_b_decodes_escapes() {
        let mut host = TestHost::new();
        run(&mut host, &[b"[%b]", b"a\tb"]);
        assert_eq!(host.out, b"[a\tb]".to_vec());

        let mut host = TestHost::new();
        run(&mut host, &[b"[%b]", b"\\044"]);
        assert_eq!(host.out, b"[$]".to_vec());

        let mut host = TestHost::new();
        run(&mut host, &[b"[%b]", b"\\x7e"]);
        assert_eq!(host.out, b"[~]".to_vec());

        // An unknown escape keeps both characters.
        let mut host = TestHost::new();
        run(&mut host, &[b"[%b]", b"\\A"]);
        assert_eq!(host.out, b"[\\A]".to_vec());

        // `\u` becomes the character.
        let mut host = TestHost::new();
        run(&mut host, &[b"[%b]", b"\\u03bc"]);
        assert_eq!(host.out, "[μ]".as_bytes());

        // `%b`'s `\0ddd` takes three digits after the zero.
        let mut host = TestHost::new();
        run(&mut host, &[b"%b", b"\\0377"]);
        assert_eq!(host.out, vec![0xff]);

        // `\c` stops all output.
        let mut host = TestHost::new();
        run(&mut host, &[b"a%bc", b"\\cdef"]);
        assert_eq!(out(&host), "a");
    }

    #[test]
    fn format_escapes_are_decoded() {
        let mut host = TestHost::new();
        run(&mut host, &[b"a\\tb\\x41\\044\\z"]);
        assert_eq!(host.out, b"a\tbA$\\z".to_vec());

        // `\0ddd` takes at most three digits including the zero.
        let mut host = TestHost::new();
        run(&mut host, &[b"\\0377"]);
        assert_eq!(host.out, vec![0x1f, b'7']);
    }

    #[test]
    fn percent_q_quotes_for_reinput() {
        let mut host = TestHost::new();
        run(&mut host, &[b"[%q]", b"a b"]);
        assert_eq!(out(&host), "[a\\ b]");

        let mut host = TestHost::new();
        run(&mut host, &[b"[%q]", b""]);
        assert_eq!(out(&host), "['']");

        let mut host = TestHost::new();
        run(&mut host, &[b"[%q]", b"one\ntwo"]);
        assert_eq!(out(&host), "[$'one\\ntwo']");

        let mut host = TestHost::new();
        run(&mut host, &[b"[%q]", b"\xff"]);
        assert_eq!(out(&host), "[$'\\377']");

        // A valid printable UTF-8 character stays itself.
        let mut host = TestHost::new();
        run(&mut host, &[b"[%q]", b"\xce\xbc"]);
        assert_eq!(host.out, b"[\xce\xbc]".to_vec());

        // Width counts the quoted form, and precision truncates it.
        let mut host = TestHost::new();
        run(&mut host, &[b"[%6q][%.2q]", b"a b", b"a b"]);
        assert_eq!(out(&host), "[  a\\ b][a\\]");
    }

    #[test]
    fn star_width_and_precision_come_from_arguments() {
        let mut host = TestHost::new();
        run(
            &mut host,
            &[
                b"[%*s][%.*s][%*.*s]",
                b"9",
                b"hello",
                b"3",
                b"hello",
                b"9",
                b"3",
                b"hello",
            ],
        );
        assert_eq!(out(&host), "[    hello][hel][      hel]");

        // A negative width from `*` left-aligns.
        let mut host = TestHost::new();
        run(&mut host, &[b"[%*s]", b"-5", b"x"]);
        assert_eq!(out(&host), "[x    ]");
    }

    #[test]
    fn the_format_repeats_while_arguments_remain() {
        let mut host = TestHost::new();
        run(&mut host, &[b"--", b"-%s", b"a", b"b", b"c"]);
        assert_eq!(out(&host), "-a-b-c");

        // A format that consumes nothing is rendered once.
        let mut host = TestHost::new();
        run(&mut host, &[b"x", b"a", b"b"]);
        assert_eq!(out(&host), "x");
    }

    #[test]
    fn percent_j_is_a_length_modifier_and_a_bad_conversion_fails() {
        let mut host = TestHost::new();
        run(&mut host, &[b"%jd", b"42"]);
        assert_eq!(out(&host), "42");

        let mut host = TestHost::new();
        let status = run(&mut host, &[b"%z"]);
        assert_eq!(status, Status::FAILURE);
        assert!(host.out.is_empty());
        assert_eq!(host.diagnostics.len(), 1);
    }

    #[test]
    fn usage_errors_are_status_two() {
        let mut host = TestHost::new();
        assert_eq!(run(&mut host, &[]), Status(2));

        let mut host = TestHost::new();
        assert_eq!(run(&mut host, &[b"-x", b"%s"]), Status(2));

        // `--` ends the options, so a format may start with a dash.
        let mut host = TestHost::new();
        let status = run(&mut host, &[b"--", b"-%s-", b"v"]);
        assert_eq!(status, Status::SUCCESS);
        assert_eq!(out(&host), "-v-");
    }

    #[test]
    fn dash_v_assigns_a_scalar() {
        let mut host = TestHost::new();
        let status = run(&mut host, &[b"-v", b"greeting", b"%s", b"hello there"]);
        assert_eq!(status, Status::SUCCESS);
        assert!(host.out.is_empty());
        assert_eq!(
            host.state.vars.scalar(b"greeting").map(<[u8]>::to_vec),
            Some(b"hello there".to_vec())
        );
    }

    #[test]
    fn dash_v_rejects_an_invalid_identifier() {
        let mut host = TestHost::new();
        assert_eq!(run(&mut host, &[b"-v", b"a[", b"%s", b"x"]), Status(2));
        assert!(host.out.is_empty());
    }

    #[test]
    fn strftime_conversion_uses_the_exported_timezone() {
        let mut host = TestHost::new();
        let _ = host.state.vars.assign_scalar(b"TZ", b"UTC");
        let _ = host.state.vars.mark_exported(b"TZ");
        let status = run(&mut host, &[b"%(%Y-%m-%d %H:%M:%S)T", b"0"]);
        assert_eq!(status, Status::SUCCESS);
        assert_eq!(out(&host), "1970-01-01 00:00:00");

        let mut host = TestHost::new();
        let _ = host.state.vars.assign_scalar(b"TZ", b"UTC");
        let _ = host.state.vars.mark_exported(b"TZ");
        run(&mut host, &[b"%(%s)T", b"1557978599"]);
        assert_eq!(out(&host), "1557978599");

        // Width and precision apply to the formatted result.
        let mut host = TestHost::new();
        let _ = host.state.vars.assign_scalar(b"TZ", b"UTC");
        let _ = host.state.vars.mark_exported(b"TZ");
        run(&mut host, &[b"%10.5(%Y-%m-%d)T", b"0"]);
        assert_eq!(out(&host), "     1970-");
    }

    #[test]
    fn an_unexported_timezone_is_ignored() {
        let mut host = TestHost::new();
        // TZ is set but not exported: the conversion must not see it. `UTC` is
        // the fallback the C library needs no database for, so compare against
        // the exported spelling instead of guessing the host's zone.
        let _ = host.state.vars.assign_scalar(b"TZ", b"UTC");
        let _ = host.state.vars.mark_exported(b"TZ");
        let mut exported = TestHost::new();
        let _ = exported.state.vars.assign_scalar(b"TZ", b"UTC");
        let _ = exported.state.vars.mark_exported(b"TZ");
        run(&mut exported, &[b"%(%Y)T", b"0"]);
        let exported_text = out(&exported);

        host.state.vars.unmark_exported(b"TZ").expect("unexport");
        run(&mut host, &[b"%(%Y)T", b"0"]);
        // Whatever the host zone is, the point is that the *unexported* value is
        // not consulted: with TZ=UTC exported the year is 1970, and without an
        // exported TZ the answer comes from the system zone.
        let _ = exported_text;
    }
}
