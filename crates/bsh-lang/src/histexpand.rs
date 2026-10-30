//! History expansion: the interactive `!` event syntax.
//!
//! History expansion is a *textual* rewrite of the line the user typed, run
//! before the parser sees it. It is deliberately not part of the word model:
//! `!!` may expand to text with quotes, operators or several commands, and
//! that text is what the shell then parses and runs.
//!
//! This module is pure. The caller supplies the typed line and a snapshot of
//! the history ring (oldest first, with the numbers `history` prints); it
//! returns the rewritten line or an [`Error`]. The binary owns the policy
//! questions: when to expand (`interactive && histexpand && history`), what to
//! print, and what to record.
//!
//! # Measured against bash 5.3.20
//!
//! The syntax and the safety rules were measured on bash 5.3.20, not assumed:
//!
//! * Single quotes and `$'…'` inhibit expansion; double quotes do not.
//! * A backslash quotes the next byte (so an odd run of backslashes hides a
//!   `!`, an even run lets it expand).
//! * `#` starts a comment only at the start of a word.
//! * `!` followed by a blank, newline, `=`, `(`, `)`, `;`, `&`, `|`, `<` or
//!   `>` is literal. `!` at end of line is literal.
//! * Both `histexpand` and `history` must be on for an interactive line to be
//!   expanded.
//! * A here-document body is never expanded (bash reads it separately from the
//!   command line). The multi-line quote state is carried across continuation
//!   lines: a `!` inside a single-quoted string that spans lines stays literal,
//!   while one inside a double-quoted string still expands.
//!
//! Bash 5.3.20 has **no** `!{…}` event form: `!{echo}` is an event named
//! `{echo}` and reports "event not found", exactly like `!{xyz}`. The
//! expander matches that; adding the zsh-style brace form would be a visible
//! deviation, so it is deliberately absent.
//!
//! # The forms
//!
//! Events: `!!`, `!n`, `!-n`, `!string`, `!?string?`, `!#`.
//! Word designators: `!$`, `!*`, `!^`, `!%`, `!:n`, `!:n-m`, `!:n-`,
//! `!:-m`, `!:n*`, `!:*`.
//! Modifiers: `:h`, `:t`, `:r`, `:e`, `:p`, `:q`, `:x`, `:s/old/new/`,
//! `:gs/old/new/`, `:&`, `:g&`, and any chain of them.
//! Quick substitution: `^old^new^` at the start of a line.
//!
//! Words selected by a designator are rejoined with single spaces, which is
//! what bash does. `h`, `t`, `r`, `e`, `q` and `s` then apply to the whole
//! joined result (also measured); only `x` splits it into words again.

use std::fmt;

/// One history entry, as the expander sees it.
///
/// `number` is the number `history` prints (it may not equal the index: a
/// trimmed ring keeps its numbers, so after a 500-entry window the first entry
/// is `501`). `!n` refers to the number; `!-n` refers to the position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry<'a> {
    number: u32,
    text: &'a [u8],
}

impl<'a> Entry<'a> {
    /// An entry with a history number and its command text.
    pub fn new(number: u32, text: &'a [u8]) -> Self {
        Self { number, text }
    }

    /// The history number.
    pub fn number(&self) -> u32 {
        self.number
    }

    /// The command text.
    pub fn text(&self) -> &[u8] {
        self.text
    }
}

/// State that bash keeps between lines of history expansion.
///
/// `:&` repeats the previous substitution, `:s//new/` reuses the previous
/// `old`, and `!%` names the word matched by the most recent `!?string?`
/// search. Those three facts outlive one line, so they live here rather than
/// in a local of [`Session::expand`].
#[derive(Clone, Debug, Default)]
pub struct State {
    /// The last successful substitution: `(old, new)`.
    last_substitution: Option<(Vec<u8>, Vec<u8>)>,
    /// The string from the most recent `!?string?` search, for `!?` with no
    /// operand.
    last_search: Option<Vec<u8>>,
    /// The word that contained the most recent `!?string?` match, for `%`.
    last_match: Option<Vec<u8>>,
}

/// The rewritten line plus the flags the caller must honour.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Expansion {
    text: Vec<u8>,
    changed: bool,
    print: bool,
    print_only: bool,
}

impl Expansion {
    /// The rewritten bytes.
    pub fn text(&self) -> &[u8] {
        &self.text
    }

    /// The rewritten bytes, owned.
    pub fn into_text(self) -> Vec<u8> {
        self.text
    }

    /// Whether any event or quick substitution was expanded. A line with no
    /// history syntax comes back unchanged with `changed()` false, so the
    /// caller can skip display and history bookkeeping.
    pub fn changed(&self) -> bool {
        self.changed
    }

    /// Whether a `:p` modifier appeared: the caller prints the line.
    pub fn print(&self) -> bool {
        self.print
    }

    /// Whether the last modifier was `:p`, so the line must not run.
    pub fn print_only(&self) -> bool {
        self.print_only
    }

    fn unchanged(text: Vec<u8>) -> Self {
        Self {
            text,
            changed: false,
            print: false,
            print_only: false,
        }
    }
}

/// Which part of a substitution went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// No event matched (or there is no history at all).
    EventNotFound,
    /// The word designator names a word the event does not have.
    BadWordSpecifier,
    /// `:s/old/new/` (or a quick substitution) found no `old`.
    SubstitutionFailed,
    /// `:&` or an empty `old` with no earlier substitution.
    NoPreviousSubstitution,
    /// `!?` with no operand and no earlier `!?string?` search.
    NoPreviousSearch,
    /// A modifier letter the expander does not know.
    UnknownModifier(u8),
}

/// A failed expansion, with the text bash would quote in its message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    kind: ErrorKind,
    token: Vec<u8>,
}

impl Error {
    /// What failed.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The token bash quotes in the diagnostic (for example `!nosuch`, or
    /// `:s/old/new/`). It includes the leading `!` for event errors.
    pub fn token(&self) -> &[u8] {
        &self.token
    }

    fn event_not_found(token: &[u8]) -> Self {
        Self {
            kind: ErrorKind::EventNotFound,
            token: token.to_vec(),
        }
    }

    fn bad_word_specifier(token: &[u8]) -> Self {
        Self {
            kind: ErrorKind::BadWordSpecifier,
            token: token.to_vec(),
        }
    }

    fn substitution_failed(token: &[u8]) -> Self {
        Self {
            kind: ErrorKind::SubstitutionFailed,
            token: token.to_vec(),
        }
    }

    fn no_previous_substitution(token: &[u8]) -> Self {
        Self {
            kind: ErrorKind::NoPreviousSubstitution,
            token: token.to_vec(),
        }
    }

    fn no_previous_search(token: &[u8]) -> Self {
        Self {
            kind: ErrorKind::NoPreviousSearch,
            token: token.to_vec(),
        }
    }

    fn unknown_modifier(byte: u8) -> Self {
        Self {
            kind: ErrorKind::UnknownModifier(byte),
            token: vec![byte],
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let token = String::from_utf8_lossy(&self.token);
        match self.kind {
            ErrorKind::EventNotFound => write!(f, "{token}: event not found"),
            ErrorKind::BadWordSpecifier => write!(f, "{token}: bad word specifier"),
            ErrorKind::SubstitutionFailed => write!(f, "{token}: substitution failed"),
            ErrorKind::NoPreviousSubstitution => {
                write!(f, "{token}: no previous substitution")
            }
            ErrorKind::NoPreviousSearch => write!(f, "{token}: no previous search string"),
            ErrorKind::UnknownModifier(byte) => {
                write!(f, "{}: unrecognized history modifier", byte as char)
            }
        }
    }
}

impl std::error::Error for Error {}

/// A history-expansion session: one per interactive reader.
///
/// It holds the cross-line state (`:&`, `!%`) and the quote state that bash
/// carries across `PS2` continuation lines, plus the queue of here-documents
/// whose bodies must not be expanded.
#[derive(Clone, Debug, Default)]
pub struct Session {
    state: State,
    quote: Quote,
    heredocs: Vec<Heredoc>,
}

impl Session {
    /// A fresh session.
    pub fn new() -> Self {
        Self::default()
    }

    /// The cross-line state, for tests and for callers that need to inspect
    /// the last substitution.
    pub fn state(&self) -> &State {
        &self.state
    }

    /// Forget the partially parsed command after an expansion error.
    ///
    /// A failed event abandons the command, so a here-document that line
    /// declared is not waiting for a body, and the quote state belongs to a
    /// command that will never be parsed.
    pub fn abandon_command(&mut self) {
        self.quote = Quote::Open;
        self.heredocs.clear();
    }

    /// Expand one physical line, without its newline.
    ///
    /// Quote state and here-document state carry over from earlier calls, so a
    /// caller reading a command one line at a time behaves exactly like one
    /// expanding an assembled chunk.
    pub fn expand_line(&mut self, line: &[u8], history: &[Entry<'_>]) -> Result<Expansion, Error> {
        if let Some(heredoc) = self.heredocs.first()
            && heredoc.is_terminator(line)
        {
            self.heredocs.remove(0);
            return Ok(Expansion::unchanged(line.to_vec()));
        }
        if !self.heredocs.is_empty() {
            // The body of a here-document is read by the shell as data, not as
            // a command line; bash does not history-expand it.
            return Ok(Expansion::unchanged(line.to_vec()));
        }
        let start_quote = self.quote;
        if start_quote == Quote::Open && line.first() == Some(&b'^') {
            let mut expansion = quick_substitution(line, history, &mut self.state)?;
            scan_heredocs(&expansion.text, start_quote, &mut self.heredocs);
            if !expansion.changed {
                expansion.text = line.to_vec();
            }
            return Ok(expansion);
        }
        // Bash decides with a first pass whether the line contains anything
        // worth expanding; a line it rejects is returned completely unchanged,
        // including its backslashes. See `has_expandable_event` for why that
        // pass is not redundant.
        let (expandable, end_quote) = has_expandable_event(line, start_quote);
        if !expandable {
            self.quote = end_quote;
            scan_heredocs(line, start_quote, &mut self.heredocs);
            return Ok(Expansion::unchanged(line.to_vec()));
        }
        let mut expansion = expand_text(line, history, &mut self.state, &mut self.quote)?;
        scan_heredocs(&expansion.text, start_quote, &mut self.heredocs);
        if !expansion.changed {
            expansion.text = line.to_vec();
        }
        Ok(expansion)
    }

    /// Expand an assembled chunk (one or more physical lines).
    ///
    /// The newlines are preserved; each line is expanded with the same quote
    /// and here-document state. `print_only` is only meaningful for a
    /// single-line chunk, because that is the only shape bash would run as one
    /// command from one `:p`.
    pub fn expand(&mut self, chunk: &[u8], history: &[Entry<'_>]) -> Result<Expansion, Error> {
        let single_line = !chunk.contains(&b'\n');
        let mut text = Vec::with_capacity(chunk.len());
        let mut changed = false;
        let mut print = false;
        let mut print_only = false;
        for segment in chunk.split_inclusive(|byte| *byte == b'\n') {
            let (line, newline) = match segment.strip_suffix(b"\n") {
                Some(line) => (line, true),
                None => (segment, false),
            };
            let expansion = self.expand_line(line, history)?;
            changed |= expansion.changed;
            print |= expansion.print;
            print_only |= expansion.print_only;
            text.extend_from_slice(expansion.text());
            if newline {
                text.push(b'\n');
            }
        }
        Ok(Expansion {
            text,
            changed,
            print,
            print_only: single_line && print_only,
        })
    }
}

// -- scanning ---------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Quote {
    #[default]
    Open,
    Single,
    Double,
    /// `$'…'`: single-quote rules with backslash escapes.
    AnsiC,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Heredoc {
    delimiter: Vec<u8>,
    strip_tabs: bool,
}

impl Heredoc {
    fn is_terminator(&self, line: &[u8]) -> bool {
        let line = if self.strip_tabs {
            let start = line
                .iter()
                .position(|byte| *byte != b'\t')
                .unwrap_or(line.len());
            &line[start..]
        } else {
            line
        };
        line == self.delimiter
    }
}

/// Whether `!` followed by this byte is literal rather than an event.
fn is_literal_after_bang(byte: u8) -> bool {
    matches!(
        byte,
        b' ' | b'\t' | b'\n' | b'\r' | b'=' | b'(' | b')' | b';' | b'&' | b'|' | b'<' | b'>'
    )
}

/// Where an unbraced event name ends.
fn is_event_stop(byte: u8) -> bool {
    byte.is_ascii_whitespace()
        || matches!(
            byte,
            b':' | b'=' | b'(' | b')' | b';' | b'&' | b'|' | b'<' | b'>'
        )
}

fn is_word_start_at(line: &[u8], index: usize) -> bool {
    match index.checked_sub(1).and_then(|previous| line.get(previous)) {
        None => true,
        Some(byte) => {
            byte.is_ascii_whitespace()
                || matches!(byte, b';' | b'&' | b'|' | b'(' | b')' | b'<' | b'>')
        }
    }
}

/// Bash's first pass: does this line contain an event at all?
///
/// It is not an optimisation, because its backslash rule is different from the
/// real scan's: here a backslash hides the next byte only when that byte is a
/// quote or `!`. A `!` that this pass rejects is returned completely unchanged,
/// which is observable: `echo \\!!` at end of line stays literal, while
/// `echo \\!!x` expands (bash 5.3.20, measured). Matching that quirk also
/// keeps a failed event from being reported for a line bash would not touch.
fn has_expandable_event(line: &[u8], start_quote: Quote) -> (bool, Quote) {
    let mut quote = start_quote;
    let mut index = 0;
    while index < line.len() {
        let byte = line[index];
        let next = line.get(index + 1).copied();
        match quote {
            Quote::Single | Quote::AnsiC => {
                if quote == Quote::AnsiC && byte == b'\\' && index + 1 < line.len() {
                    index += 2;
                    continue;
                }
                index += 1;
                if byte == b'\'' {
                    quote = Quote::Open;
                }
            }
            Quote::Double => match byte {
                b'!' => {
                    if next.is_none_or(|next| is_literal_after_bang(next) || next == b'"') {
                        index += 1;
                    } else {
                        return (true, quote);
                    }
                }
                b'"' => {
                    quote = Quote::Open;
                    index += 1;
                }
                b'\\' if matches!(next, Some(b'"') | Some(b'\'') | Some(b'!')) => index += 2,
                _ => index += 1,
            },
            Quote::Open => match byte {
                b'#' if is_word_start_at(line, index) => return (false, quote),
                b'!' => {
                    if next.is_none_or(is_literal_after_bang) {
                        index += 1;
                    } else {
                        return (true, quote);
                    }
                }
                b'"' => {
                    quote = Quote::Double;
                    index += 1;
                }
                b'\'' => {
                    let ansi = index > 0 && line[index - 1] == b'$';
                    let (after, closed) = skip_single_quoted(line, index, ansi);
                    index = after;
                    if !closed {
                        quote = if ansi { Quote::AnsiC } else { Quote::Single };
                    }
                }
                b'\\' if matches!(next, Some(b'\'') | Some(b'!')) => index += 2,
                _ => index += 1,
            },
        }
    }
    (false, quote)
}

/// Skip a single-quoted string starting at `index` (the opening quote).
fn skip_single_quoted(line: &[u8], index: usize, ansi: bool) -> (usize, bool) {
    let mut index = index + 1;
    while index < line.len() {
        if ansi && line[index] == b'\\' && index + 1 < line.len() {
            index += 2;
            continue;
        }
        if line[index] == b'\'' {
            return (index + 1, true);
        }
        index += 1;
    }
    (index, false)
}

fn expand_text(
    line: &[u8],
    history: &[Entry<'_>],
    state: &mut State,
    quote: &mut Quote,
) -> Result<Expansion, Error> {
    if *quote == Quote::Open && line.first() == Some(&b'^') {
        return quick_substitution(line, history, state);
    }
    let mut out = Vec::with_capacity(line.len());
    let mut changed = false;
    let mut print = false;
    let mut print_only = false;
    let mut index = 0;
    while index < line.len() {
        let byte = line[index];
        match *quote {
            Quote::Single => {
                out.push(byte);
                index += 1;
                if byte == b'\'' {
                    *quote = Quote::Open;
                }
            }
            Quote::AnsiC => {
                if byte == b'\\' && index + 1 < line.len() {
                    out.push(byte);
                    out.push(line[index + 1]);
                    index += 2;
                } else {
                    out.push(byte);
                    index += 1;
                    if byte == b'\'' {
                        *quote = Quote::Open;
                    }
                }
            }
            Quote::Double => match byte {
                b'\\' if index + 1 < line.len() => {
                    out.push(byte);
                    out.push(line[index + 1]);
                    index += 2;
                }
                b'"' => {
                    out.push(byte);
                    index += 1;
                    *quote = Quote::Open;
                }
                b'!' => {
                    let event = expand_event(line, index, history, state, &out, true)?;
                    out.extend_from_slice(&event.text);
                    changed |= event.changed;
                    print |= event.print;
                    print_only |= event.print_only;
                    index = event.next;
                }
                _ => {
                    out.push(byte);
                    index += 1;
                }
            },
            Quote::Open => match byte {
                b'\\' if index + 1 < line.len() => {
                    out.push(byte);
                    out.push(line[index + 1]);
                    index += 2;
                }
                b'\'' => {
                    out.push(byte);
                    index += 1;
                    *quote = Quote::Single;
                }
                b'"' => {
                    out.push(byte);
                    index += 1;
                    *quote = Quote::Double;
                }
                b'$' if line.get(index + 1) == Some(&b'\'') => {
                    out.push(b'$');
                    out.push(b'\'');
                    index += 2;
                    *quote = Quote::AnsiC;
                }
                b'$' if line.get(index + 1) == Some(&b'"') => {
                    out.push(b'$');
                    out.push(b'"');
                    index += 2;
                    *quote = Quote::Double;
                }
                b'#' if is_word_start_at(line, index) => {
                    out.extend_from_slice(&line[index..]);
                    index = line.len();
                }
                b'!' => {
                    let event = expand_event(line, index, history, state, &out, true)?;
                    out.extend_from_slice(&event.text);
                    changed |= event.changed;
                    print |= event.print;
                    print_only |= event.print_only;
                    index = event.next;
                }
                _ => {
                    out.push(byte);
                    index += 1;
                }
            },
        }
    }
    Ok(Expansion {
        text: out,
        changed,
        print,
        print_only: print && print_only,
    })
}

struct EventResult {
    text: Vec<u8>,
    next: usize,
    changed: bool,
    print: bool,
    print_only: bool,
}

/// Parse one `!` event starting at `bang`.
fn expand_event(
    line: &[u8],
    bang: usize,
    history: &[Entry<'_>],
    state: &mut State,
    out_so_far: &[u8],
    in_double_quotes: bool,
) -> Result<EventResult, Error> {
    let literal = EventResult {
        text: vec![b'!'],
        next: bang + 1,
        changed: false,
        print: false,
        print_only: false,
    };
    let Some(&first) = line.get(bang + 1) else {
        return Ok(literal);
    };
    if is_literal_after_bang(first) || (in_double_quotes && first == b'"') {
        return Ok(literal);
    }

    let mut index = bang + 1;
    let mut bare: Option<u8> = None;
    let event_text: Vec<u8>;
    match first {
        b'!' => {
            index += 1;
            event_text = previous(history, &line[bang..index])?.text().to_vec();
        }
        b'#' => {
            index += 1;
            event_text = out_so_far.to_vec();
        }
        b'?' => {
            let (text, next) = search_contains(line, index, history, state)?;
            index = next;
            event_text = text;
        }
        b'$' | b'*' | b'^' | b'%' => {
            index += 1;
            bare = Some(first);
            event_text = previous(history, &line[bang..index])?.text().to_vec();
        }
        b':' => {
            event_text = previous(history, &line[bang..=index])?.text().to_vec();
        }
        b'-' if line.get(index + 1).is_some_and(u8::is_ascii_digit) => {
            let number_start = index + 1;
            index = number_start;
            while line.get(index).is_some_and(u8::is_ascii_digit) {
                index += 1;
            }
            let token = &line[bang..index];
            let Some(count) = parse_usize(&line[number_start..index]) else {
                return Err(Error::event_not_found(token));
            };
            let Some(offset) = count
                .checked_sub(1)
                .and_then(|offset| history.len().checked_sub(offset + 1))
            else {
                return Err(Error::event_not_found(token));
            };
            event_text = history[offset].text().to_vec();
        }
        byte if byte.is_ascii_digit() => {
            let number_start = index;
            while line.get(index).is_some_and(u8::is_ascii_digit) {
                index += 1;
            }
            let token = &line[bang..index];
            let number = parse_usize(&line[number_start..index]).unwrap_or(usize::MAX);
            let number = u32::try_from(number).ok();
            let Some(entry) =
                number.and_then(|number| history.iter().find(|entry| entry.number == number))
            else {
                return Err(Error::event_not_found(token));
            };
            event_text = entry.text().to_vec();
        }
        _ => {
            let name_start = index;
            while line.get(index).is_some_and(|byte| !is_event_stop(*byte)) {
                index += 1;
            }
            let name = &line[name_start..index];
            let Some(entry) = history
                .iter()
                .rev()
                .find(|entry| entry.text.starts_with(name))
            else {
                return Err(Error::event_not_found(&line[bang..index]));
            };
            event_text = entry.text().to_vec();
        }
    }

    // Word designator: the `:` form, or the bare `!$`/`!*`/`!^`/`!%` form.
    let mut spec: Option<Spec> = bare.map(Spec::from_bare);
    let mut spec_start = if bare.is_some() { bang + 1 } else { bang };
    if spec.is_none() && line.get(index) == Some(&b':') {
        spec_start = index;
        if let Some((parsed, next)) = parse_designator(line, index + 1) {
            spec = Some(parsed);
            index = next;
        }
        // With no designator the colon itself starts the modifier chain, so
        // `index` stays on it.
    }
    let spec_token = &line[spec_start..index];

    let value = match &spec {
        Some(spec) => select_words(&event_text, spec, state, spec_token)?,
        None => event_text,
    };

    let modifiers = apply_modifiers(line, index, value, state, index)?;
    index = modifiers.next;

    Ok(EventResult {
        text: modifiers.value,
        next: index,
        changed: true,
        print: modifiers.print,
        print_only: modifiers.print && modifiers.last_was_p,
    })
}

fn previous<'a>(history: &'a [Entry<'a>], token: &[u8]) -> Result<Entry<'a>, Error> {
    history
        .last()
        .copied()
        .ok_or_else(|| Error::event_not_found(token))
}

fn parse_usize(bytes: &[u8]) -> Option<usize> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

/// `!?string?`: the newest entry containing `string`.
fn search_contains(
    line: &[u8],
    index: usize,
    history: &[Entry<'_>],
    state: &mut State,
) -> Result<(Vec<u8>, usize), Error> {
    let start = index;
    let mut index = index + 1;
    let mut needle = Vec::new();
    while index < line.len() {
        match line[index] {
            b'\\' if index + 1 < line.len() => {
                needle.push(line[index + 1]);
                index += 2;
            }
            b'?' => {
                index += 1;
                break;
            }
            byte => {
                needle.push(byte);
                index += 1;
            }
        }
    }
    if needle.is_empty() {
        match &state.last_search {
            Some(previous) => needle = previous.clone(),
            None => return Err(Error::no_previous_search(&line[start..index])),
        }
    }
    let token = &line[start.saturating_sub(1)..index];
    let Some(entry) = history
        .iter()
        .rev()
        .find(|entry| contains(entry.text, &needle))
    else {
        return Err(Error::event_not_found(token));
    };
    state.last_search = Some(needle.clone());
    state.last_match = containing_word(entry.text, &needle);
    Ok((entry.text().to_vec(), index))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn containing_word(text: &[u8], needle: &[u8]) -> Option<Vec<u8>> {
    tokenize(text)
        .into_iter()
        .find(|word| contains(word, needle))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Spec {
    Word(usize),
    Range(usize, Bound),
    LastMatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Bound {
    Fixed(usize),
    Last,
    /// `x-`: up to but excluding the last word.
    LastExcluded,
}

impl Spec {
    fn from_bare(byte: u8) -> Self {
        match byte {
            b'$' => Spec::Word(usize::MAX),
            b'*' => Spec::Range(1, Bound::Last),
            b'^' => Spec::Word(1),
            b'%' => Spec::LastMatch,
            _ => unreachable!("only $, *, ^ and % are bare designators"),
        }
    }
}

/// Parse a word designator starting at `index`; `None` means the bytes there
/// are modifiers, not a designator.
fn parse_designator(line: &[u8], index: usize) -> Option<(Spec, usize)> {
    let byte = *line.get(index)?;
    match byte {
        b'^' => Some((Spec::Word(1), index + 1)),
        b'$' => Some((Spec::Word(usize::MAX), index + 1)),
        b'*' => Some((Spec::Range(1, Bound::Last), index + 1)),
        b'%' => Some((Spec::LastMatch, index + 1)),
        b'-' => {
            let start = index + 1;
            let end = read_number(line, start)?;
            Some((Spec::Range(0, Bound::Fixed(end.0)), end.1))
        }
        b if b.is_ascii_digit() => {
            let (first, mut next) = read_number(line, index)?;
            if line.get(next) == Some(&b'-') {
                next += 1;
                if line.get(next) == Some(&b'*') {
                    // Bash does not treat `x-*` as a range: `x-` is parsed and
                    // the `*` is left as literal text.
                    return Some((Spec::Range(first, Bound::LastExcluded), next));
                }
                if line.get(next) == Some(&b'$') {
                    return Some((Spec::Range(first, Bound::Last), next + 1));
                }
                return match read_number(line, next) {
                    Some((last, after)) => Some((Spec::Range(first, Bound::Fixed(last)), after)),
                    None => Some((Spec::Range(first, Bound::LastExcluded), next)),
                };
            }
            if line.get(next) == Some(&b'*') {
                next += 1;
                return Some((Spec::Range(first, Bound::Last), next));
            }
            Some((Spec::Word(first), next))
        }
        _ => None,
    }
}

fn read_number(line: &[u8], index: usize) -> Option<(usize, usize)> {
    let mut next = index;
    while line.get(next).is_some_and(u8::is_ascii_digit) {
        next += 1;
    }
    if next == index {
        return None;
    }
    parse_usize(&line[index..next]).map(|number| (number, next))
}

fn select_words(event: &[u8], spec: &Spec, state: &State, token: &[u8]) -> Result<Vec<u8>, Error> {
    let words = tokenize(event);
    let count = words.len();
    let pick = |index: usize| -> Result<Vec<u8>, Error> {
        words
            .get(index)
            .cloned()
            .ok_or_else(|| Error::bad_word_specifier(token))
    };
    let selected: Vec<Vec<u8>> = match *spec {
        Spec::LastMatch => state.last_match.iter().cloned().collect(),
        Spec::Word(index) => {
            if index == usize::MAX {
                // `$`: the last word, or word 0 when there is only the command.
                if count == 0 {
                    return Err(Error::bad_word_specifier(token));
                }
                vec![pick(count - 1)?]
            } else {
                vec![pick(index)?]
            }
        }
        Spec::Range(start, bound) => {
            if count == 0 {
                return Err(Error::bad_word_specifier(token));
            }
            if start >= count {
                // `!*` on a one-word event is the empty string, not an error.
                if start == 1 && count <= 1 && bound == Bound::Last {
                    return Ok(Vec::new());
                }
                return Err(Error::bad_word_specifier(token));
            }
            let end = match bound {
                Bound::Fixed(last) => {
                    if last >= count || last < start {
                        return Err(Error::bad_word_specifier(token));
                    }
                    last
                }
                Bound::Last => count - 1,
                Bound::LastExcluded => {
                    if count < 2 {
                        return Ok(Vec::new());
                    }
                    count - 2
                }
            };
            if end < start {
                Vec::new()
            } else {
                words[start..=end].to_vec()
            }
        }
    };
    let mut out = Vec::new();
    for (position, word) in selected.iter().enumerate() {
        if position > 0 {
            out.push(b' ');
        }
        out.extend_from_slice(word);
    }
    Ok(out)
}

/// Split an event into words, preserving quote characters.
///
/// This is the shape bash uses for designators: `cat "a b" c` has three words
/// and the middle one still reads `"a b"`.
fn tokenize(text: &[u8]) -> Vec<Vec<u8>> {
    let mut words: Vec<Vec<u8>> = Vec::new();
    let mut word: Vec<u8> = Vec::new();
    let mut quote = Quote::Open;
    let mut index = 0;
    while index < text.len() {
        let byte = text[index];
        match quote {
            Quote::Single => {
                word.push(byte);
                index += 1;
                if byte == b'\'' {
                    quote = Quote::Open;
                }
            }
            Quote::AnsiC => {
                if byte == b'\\' && index + 1 < text.len() {
                    word.push(byte);
                    word.push(text[index + 1]);
                    index += 2;
                } else {
                    word.push(byte);
                    index += 1;
                    if byte == b'\'' {
                        quote = Quote::Open;
                    }
                }
            }
            Quote::Double => {
                if byte == b'\\' && index + 1 < text.len() {
                    word.push(byte);
                    word.push(text[index + 1]);
                    index += 2;
                } else {
                    word.push(byte);
                    index += 1;
                    if byte == b'"' {
                        quote = Quote::Open;
                    }
                }
            }
            Quote::Open => {
                if byte.is_ascii_whitespace() {
                    if !word.is_empty() {
                        words.push(std::mem::take(&mut word));
                    }
                    index += 1;
                } else if byte == b'\'' {
                    word.push(byte);
                    index += 1;
                    quote = Quote::Single;
                } else if byte == b'"' {
                    word.push(byte);
                    index += 1;
                    quote = Quote::Double;
                } else if byte == b'\\' && index + 1 < text.len() {
                    word.push(byte);
                    word.push(text[index + 1]);
                    index += 2;
                } else {
                    word.push(byte);
                    index += 1;
                }
            }
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

struct ModifierResult {
    value: Vec<u8>,
    next: usize,
    print: bool,
    last_was_p: bool,
}

/// Apply a chain of `:modifier`s starting at `index` (which must be `:`).
fn apply_modifiers(
    line: &[u8],
    mut index: usize,
    mut value: Vec<u8>,
    state: &mut State,
    chain_start: usize,
) -> Result<ModifierResult, Error> {
    let mut print = false;
    let mut last_was_p = false;
    let mut first_colon: Option<usize> = None;
    while line.get(index) == Some(&b':') {
        if first_colon.is_none() {
            first_colon = Some(index);
        }
        index += 1;
        let Some(&modifier) = line.get(index) else {
            return Err(Error::unknown_modifier(b':'));
        };
        let token_start = first_colon.unwrap_or(chain_start);
        match modifier {
            b'h' => {
                value = head(&value);
                index += 1;
                last_was_p = false;
            }
            b't' => {
                value = tail(&value);
                index += 1;
                last_was_p = false;
            }
            b'r' => {
                value = root(&value);
                index += 1;
                last_was_p = false;
            }
            b'e' => {
                value = extension(&value);
                index += 1;
                last_was_p = false;
            }
            b'p' => {
                print = true;
                index += 1;
                last_was_p = true;
            }
            b'q' => {
                value = quote_single(&value);
                index += 1;
                last_was_p = false;
            }
            b'x' => {
                value = quote_words(&value);
                index += 1;
                last_was_p = false;
            }
            b'&' => {
                index += 1;
                value = repeat_substitution(&value, state, false, &line[token_start..index])?;
                last_was_p = false;
            }
            b'g' | b'a' => {
                index += 1;
                match line.get(index) {
                    Some(b'&') => {
                        index += 1;
                        value =
                            repeat_substitution(&value, state, true, &line[token_start..index])?;
                    }
                    Some(b's') => {
                        index += 1;
                        let (next_value, next) =
                            substitute_one(line, index, value, state, true, token_start)?;
                        value = next_value;
                        index = next;
                    }
                    _ => {
                        return Err(Error::unknown_modifier(modifier));
                    }
                }
                last_was_p = false;
            }
            b's' => {
                index += 1;
                let (next_value, next) =
                    substitute_one(line, index, value, state, false, token_start)?;
                value = next_value;
                index = next;
                last_was_p = false;
            }
            _ => return Err(Error::unknown_modifier(modifier)),
        }
    }
    Ok(ModifierResult {
        value,
        next: index,
        print,
        last_was_p,
    })
}

/// Parse and apply one `:s/old/new/`. `index` points just past the `s`.
fn substitute_one(
    line: &[u8],
    index: usize,
    value: Vec<u8>,
    state: &mut State,
    global: bool,
    token_start: usize,
) -> Result<(Vec<u8>, usize), Error> {
    let Some(&delimiter) = line.get(index) else {
        return Err(Error::substitution_failed(&line[token_start..]));
    };
    let (mut old, mut index, _) = read_substitution_field(line, index + 1, delimiter);
    let (mut new, index2, _) = read_substitution_field(line, index, delimiter);
    index = index2;
    if old.is_empty() {
        match &state.last_substitution {
            Some((previous, _)) => old = previous.clone(),
            None => match &state.last_search {
                Some(search) => old = search.clone(),
                None => {
                    return Err(Error::no_previous_substitution(&line[token_start..index]));
                }
            },
        }
    }
    if old.is_empty() {
        return Err(Error::no_previous_substitution(&line[token_start..index]));
    }
    new = expand_replacement(&new, &old);
    let Some(result) = substitute(&value, &old, &new, global) else {
        return Err(Error::substitution_failed(&line[token_start..index]));
    };
    state.last_substitution = Some((old, new));
    Ok((result, index))
}

/// Read one substitution field (between delimiters), with backslash escapes.
///
/// The third value is whether the closing delimiter was found; bash allows the
/// final delimiter to be omitted at the end of the line.
fn read_substitution_field(line: &[u8], start: usize, delimiter: u8) -> (Vec<u8>, usize, bool) {
    let mut field = Vec::new();
    let mut index = start;
    while index < line.len() {
        let byte = line[index];
        if byte == delimiter {
            return (field, index + 1, true);
        }
        if byte == b'\\' && index + 1 < line.len() {
            let next = line[index + 1];
            if next == delimiter || next == b'\\' {
                field.push(next);
            } else {
                field.push(byte);
                field.push(next);
            }
            index += 2;
            continue;
        }
        field.push(byte);
        index += 1;
    }
    (field, index, false)
}

/// Expand `&` (the matched text) and `\&` (a literal `&`) in a replacement.
fn expand_replacement(replacement: &[u8], old: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(replacement.len());
    let mut index = 0;
    while index < replacement.len() {
        match replacement[index] {
            b'&' => {
                out.extend_from_slice(old);
                index += 1;
            }
            b'\\' if replacement.get(index + 1) == Some(&b'&') => {
                out.push(b'&');
                index += 2;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    out
}

/// Replace `old` with `new`. An empty match is never replaced (bash's rule).
fn substitute(text: &[u8], old: &[u8], new: &[u8], global: bool) -> Option<Vec<u8>> {
    if old.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(text.len());
    let mut index = 0;
    let mut replaced = false;
    while index < text.len() {
        if text[index..].starts_with(old) {
            out.extend_from_slice(new);
            index += old.len();
            replaced = true;
            if !global {
                out.extend_from_slice(&text[index..]);
                return Some(out);
            }
            continue;
        }
        out.push(text[index]);
        index += 1;
    }
    replaced.then_some(out)
}

fn repeat_substitution(
    value: &[u8],
    state: &mut State,
    global: bool,
    token: &[u8],
) -> Result<Vec<u8>, Error> {
    let Some((old, new)) = state.last_substitution.clone() else {
        return Err(Error::no_previous_substitution(token));
    };
    substitute(value, &old, &new, global).ok_or_else(|| Error::substitution_failed(token))
}

fn head(value: &[u8]) -> Vec<u8> {
    match value.iter().rposition(|byte| *byte == b'/') {
        Some(position) => value[..position].to_vec(),
        None => value.to_vec(),
    }
}

fn tail(value: &[u8]) -> Vec<u8> {
    match value.iter().rposition(|byte| *byte == b'/') {
        Some(position) => value[position + 1..].to_vec(),
        None => value.to_vec(),
    }
}

fn root(value: &[u8]) -> Vec<u8> {
    let slash = value.iter().rposition(|byte| *byte == b'/');
    let dot = value.iter().rposition(|byte| *byte == b'.');
    match dot {
        Some(dot) if slash.is_none_or(|slash| dot > slash) => value[..dot].to_vec(),
        _ => value.to_vec(),
    }
}

fn extension(value: &[u8]) -> Vec<u8> {
    let slash = value.iter().rposition(|byte| *byte == b'/');
    let dot = value.iter().rposition(|byte| *byte == b'.');
    match dot {
        Some(dot) if slash.is_none_or(|slash| dot > slash) => value[dot..].to_vec(),
        _ => value.to_vec(),
    }
}

fn quote_single(value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 2);
    out.push(b'\'');
    for &byte in value {
        if byte == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(byte);
        }
    }
    out.push(b'\'');
    out
}

fn quote_words(value: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for (position, word) in value
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|word| !word.is_empty())
        .enumerate()
    {
        if position > 0 {
            out.push(b' ');
        }
        out.extend_from_slice(&quote_single(word));
    }
    out
}

// -- quick substitution -----------------------------------------------------

fn quick_substitution(
    line: &[u8],
    history: &[Entry<'_>],
    state: &mut State,
) -> Result<Expansion, Error> {
    let delimiter = b'^';
    let (old, next, old_closed) = read_substitution_field(line, 1, delimiter);
    let (new, next, new_closed) = if old_closed {
        read_substitution_field(line, next, delimiter)
    } else {
        (Vec::new(), next, false)
    };
    let rest = if new_closed {
        line.get(next..).unwrap_or_default()
    } else {
        &[][..]
    };
    let Some(entry) = history.last().copied() else {
        return Err(Error::event_not_found(b"!!"));
    };
    if old.is_empty() {
        return Err(Error::substitution_failed(&quick_token(&old, &new)));
    }
    let replacement = expand_replacement(&new, &old);
    let Some(value) = substitute(entry.text(), &old, &replacement, false) else {
        return Err(Error::substitution_failed(&quick_token(&old, &new)));
    };
    state.last_substitution = Some((old, replacement));
    // A remainder beginning with `:` continues as a modifier chain, so
    // `^a^b^:p` and `^a^b^:q` behave like the `!!:s…` spelling. Anything else
    // is appended verbatim (`^a^b^ tail`).
    let (text, print, print_only) = if rest.first() == Some(&b':') {
        let modifiers = apply_modifiers(rest, 0, value, state, 0)?;
        let mut text = modifiers.value;
        text.extend_from_slice(&rest[modifiers.next..]);
        (
            text,
            modifiers.print,
            modifiers.print && modifiers.last_was_p,
        )
    } else {
        let mut text = value;
        text.extend_from_slice(rest);
        (text, false, false)
    };
    Ok(Expansion {
        text,
        changed: true,
        print,
        print_only,
    })
}

fn quick_token(old: &[u8], new: &[u8]) -> Vec<u8> {
    let mut token = vec![b':', b's', b'^'];
    token.extend_from_slice(old);
    token.push(b'^');
    token.extend_from_slice(new);
    token.push(b'^');
    token
}

// -- here-documents ---------------------------------------------------------

/// Find here-document declarations on an expanded line and queue their
/// delimiters. Bodies are read by the caller one line at a time and skipped.
fn scan_heredocs(line: &[u8], start_quote: Quote, heredocs: &mut Vec<Heredoc>) {
    let mut quote = start_quote;
    let mut index = 0;
    while index < line.len() {
        let byte = line[index];
        match quote {
            Quote::Single => {
                index += 1;
                if byte == b'\'' {
                    quote = Quote::Open;
                }
            }
            Quote::AnsiC => {
                if byte == b'\\' {
                    index += 2;
                } else {
                    index += 1;
                    if byte == b'\'' {
                        quote = Quote::Open;
                    }
                }
            }
            Quote::Double => {
                if byte == b'\\' {
                    index += 2;
                } else {
                    index += 1;
                    if byte == b'"' {
                        quote = Quote::Open;
                    }
                }
            }
            Quote::Open => match byte {
                b'\\' => index += 2,
                b'\'' => {
                    quote = Quote::Single;
                    index += 1;
                }
                b'"' => {
                    quote = Quote::Double;
                    index += 1;
                }
                b'#' if is_word_start_at(line, index) => return,
                b'<' if line.get(index + 1) == Some(&b'<')
                    && line.get(index + 2) != Some(&b'<') =>
                {
                    let mut next = index + 2;
                    let strip_tabs = line.get(next) == Some(&b'-');
                    if strip_tabs {
                        next += 1;
                    }
                    while line
                        .get(next)
                        .is_some_and(|byte| *byte == b' ' || *byte == b'\t')
                    {
                        next += 1;
                    }
                    let (delimiter, after) = read_delimiter(line, next);
                    if !delimiter.is_empty() {
                        heredocs.push(Heredoc {
                            delimiter,
                            strip_tabs,
                        });
                    }
                    index = after.max(index + 2);
                }
                _ => index += 1,
            },
        }
    }
}

/// Read a here-document delimiter word with quote removal.
fn read_delimiter(line: &[u8], start: usize) -> (Vec<u8>, usize) {
    let mut delimiter = Vec::new();
    let mut index = start;
    let mut quote = Quote::Open;
    while index < line.len() {
        let byte = line[index];
        match quote {
            Quote::Single => {
                if byte == b'\'' {
                    quote = Quote::Open;
                    index += 1;
                } else {
                    delimiter.push(byte);
                    index += 1;
                }
            }
            Quote::Double => {
                if byte == b'"' {
                    quote = Quote::Open;
                    index += 1;
                } else if byte == b'\\'
                    && index + 1 < line.len()
                    && matches!(line[index + 1], b'"' | b'\\' | b'$' | b'`')
                {
                    delimiter.push(line[index + 1]);
                    index += 2;
                } else {
                    delimiter.push(byte);
                    index += 1;
                }
            }
            Quote::AnsiC => {
                delimiter.push(byte);
                index += 1;
                if byte == b'\'' {
                    quote = Quote::Open;
                }
            }
            Quote::Open => match byte {
                b' ' | b'\t' => break,
                b';' | b'&' | b'|' | b'<' | b'>' | b'(' | b')' => break,
                b'\'' => {
                    quote = Quote::Single;
                    index += 1;
                }
                b'"' => {
                    quote = Quote::Double;
                    index += 1;
                }
                b'\\' if index + 1 < line.len() => {
                    delimiter.push(line[index + 1]);
                    index += 2;
                }
                _ => {
                    delimiter.push(byte);
                    index += 1;
                }
            },
        }
    }
    (delimiter, index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries<'a>(items: &'a [(u32, &'a str)]) -> Vec<Entry<'a>> {
        items
            .iter()
            .map(|(number, text)| Entry::new(*number, text.as_bytes()))
            .collect()
    }

    fn ring() -> Vec<Entry<'static>> {
        entries(&[
            (1, "echo one two three"),
            (2, "ls /usr/local/bin/foo.txt"),
            (3, "cat a/b.c d"),
        ])
    }

    fn expand(line: &str, history: &[Entry<'_>]) -> String {
        let mut session = Session::new();
        let expansion = session
            .expand(line.as_bytes(), history)
            .unwrap_or_else(|error| panic!("{line:?}: {error}"));
        String::from_utf8_lossy(expansion.text()).into_owned()
    }

    fn expand_state(
        line: &str,
        history: &[Entry<'_>],
        session: &mut Session,
    ) -> Result<String, Error> {
        session
            .expand(line.as_bytes(), history)
            .map(|expansion| String::from_utf8_lossy(expansion.text()).into_owned())
    }

    #[test]
    fn previous_event_and_absolute_and_relative_numbers() {
        let history = ring();
        assert_eq!(expand("!!", &history), "cat a/b.c d");
        assert_eq!(expand("!1", &history), "echo one two three");
        assert_eq!(expand("!2", &history), "ls /usr/local/bin/foo.txt");
        assert_eq!(expand("!-1", &history), "cat a/b.c d");
        assert_eq!(expand("!-3", &history), "echo one two three");
    }

    #[test]
    fn prefix_and_contains_searches() {
        let history = ring();
        assert_eq!(expand("!ls", &history), "ls /usr/local/bin/foo.txt");
        assert_eq!(expand("!?two?", &history), "echo one two three");
        assert_eq!(expand("!?bin?", &history), "ls /usr/local/bin/foo.txt");
    }

    #[test]
    fn missing_events_report_the_typed_token() {
        let history = ring();
        let mut session = Session::new();
        let error = session.expand(b"!nope", &history).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::EventNotFound);
        assert_eq!(error.token(), b"!nope");
        let error = session.expand(b"!?zzz?", &history).unwrap_err();
        assert_eq!(error.token(), b"!?zzz?");
        let error = session.expand(b"!9", &history).unwrap_err();
        assert_eq!(error.token(), b"!9");
    }

    #[test]
    fn empty_history_is_an_event_not_found() {
        let mut session = Session::new();
        let error = session.expand(b"!!", &[]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::EventNotFound);
        assert_eq!(error.token(), b"!!");
        let error = session.expand(b"^a^b^", &[]).unwrap_err();
        assert_eq!(error.token(), b"!!");
    }

    #[test]
    fn numbers_survive_a_trimmed_ring_with_gaps() {
        // Numbers may start above 1 and have gaps: the ring trims and
        // `history -d` leaves holes, and an event names a number, not a slot.
        let history = entries(&[(5, "echo four"), (6, "echo five"), (7, "history")]);
        assert_eq!(expand("!6", &history), "echo five");
        assert_eq!(expand("!-2", &history), "echo five");
        let mut session = Session::new();
        for line in ["!1", "!3", "!4", "!8", "!0"] {
            let error = session.expand(line.as_bytes(), &history).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::EventNotFound, "{line}");
        }
    }

    #[test]
    fn word_designators() {
        let history = ring();
        let cat = "!3";
        assert_eq!(expand(&format!("{cat}:$"), &history), "d");
        assert_eq!(expand(&format!("{cat}:^"), &history), "a/b.c");
        assert_eq!(expand(&format!("{cat}:0"), &history), "cat");
        assert_eq!(expand(&format!("{cat}:1"), &history), "a/b.c");
        assert_eq!(expand(&format!("{cat}:2"), &history), "d");
        assert_eq!(expand(&format!("{cat}:1-2"), &history), "a/b.c d");
        assert_eq!(expand(&format!("{cat}:2-"), &history), "");
        assert_eq!(expand("!2:h", &history), "ls /usr/local/bin");
        assert_eq!(expand("!2:t", &history), "foo.txt");
        assert_eq!(expand("!2:r", &history), "ls /usr/local/bin/foo");
        assert_eq!(expand("!2:e", &history), ".txt");
        assert_eq!(expand("!3:h", &history), "cat a");
        assert_eq!(expand("!3:t", &history), "b.c d");
        assert_eq!(expand("!3:r", &history), "cat a/b");
        assert_eq!(expand("!3:e", &history), ".c d");
    }

    #[test]
    fn bare_word_designators_use_the_previous_event() {
        let history = ring();
        assert_eq!(expand("!$", &history), "d");
        assert_eq!(expand("!^", &history), "a/b.c");
        assert_eq!(expand("!*", &history), "a/b.c d");
        assert_eq!(expand("!:2", &history), "d");
    }

    #[test]
    fn words_are_split_quote_aware_and_rejoined_with_one_space() {
        let history = ring();
        assert_eq!(expand("!1:1-2", &history), "one two");
        assert_eq!(expand("!1:2*", &history), "two three");
        // The middle word keeps its quote characters.
        let history = entries(&[(1, "cat \"a b\" c")]);
        assert_eq!(expand("!1:1", &history), "\"a b\"");
        assert_eq!(expand("!1:$", &history), "c");
    }

    #[test]
    fn dollar_falls_back_to_the_command_word() {
        let history = entries(&[(1, "solo")]);
        assert_eq!(expand("!1:$", &history), "solo");
        assert_eq!(expand("!1:0", &history), "solo");
        assert_eq!(expand("!1:*", &history), "");
    }

    #[test]
    fn quick_substitution() {
        let history = entries(&[(1, "echo one two three")]);
        assert_eq!(expand("^two^2^", &history), "echo one 2 three");
        assert_eq!(expand("^two^2", &history), "echo one 2 three");
        assert_eq!(expand("^two^2^ tail", &history), "echo one 2 three tail");
        let mut session = Session::new();
        let error = session.expand(b"^zzz^2^", &history).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::SubstitutionFailed);
    }

    #[test]
    fn quick_substitution_applies_modifiers_after_the_last_caret() {
        let history = entries(&[(1, "echo a b")]);
        let mut session = Session::new();
        let expansion = session.expand(b"^a^X^:q", &history).unwrap();
        assert_eq!(expansion.text(), b"'echo X b'");
        assert!(expansion.changed());
        assert!(!expansion.print());
    }

    #[test]
    fn substitution_modifiers() {
        let history = entries(&[(1, "echo abcabc"), (2, "echo a/b")]);
        assert_eq!(expand("!1:s/a/X/", &history), "echo Xbcabc");
        assert_eq!(expand("!1:gs/a/X/", &history), "echo XbcXbc");
        assert_eq!(expand("!2:s#/#-#", &history), "echo a-b");
        assert_eq!(expand("!1:s/b//", &history), "echo acabc");
        assert_eq!(expand("!1:s/a/&&/", &history), "echo aabcabc");
        assert_eq!(expand("!1:s/a/\\&/", &history), "echo &bcabc");
    }

    #[test]
    fn substitution_state_repeats_and_reuses_old() {
        let history = entries(&[(1, "echo abcabc")]);
        let mut session = Session::new();
        assert_eq!(
            expand_state("!1:s/a/X/", &history, &mut session).unwrap(),
            "echo Xbcabc"
        );
        // `:&` re-applies the substitution to the new event. The recorded
        // command is now the substituted text, as bash would have recorded it.
        let substituted = entries(&[(1, "echo abcabc"), (2, "echo Xbcabc")]);
        assert_eq!(
            expand_state("!!:&", &substituted, &mut session).unwrap(),
            "echo XbcXbc"
        );
        // Empty old reuses the previous substitution's old.
        assert_eq!(
            expand_state("!1:s//Y/", &history, &mut session).unwrap(),
            "echo Ybcabc"
        );
    }

    #[test]
    fn substitution_failure_and_no_previous_substitution() {
        let history = entries(&[(1, "echo one")]);
        let mut session = Session::new();
        let error = session.expand(b"!1:s/zzz/x/", &history).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::SubstitutionFailed);
        assert_eq!(error.token(), b":s/zzz/x/");
        let error = session.expand(b"!1:&", &history).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NoPreviousSubstitution);
    }

    #[test]
    fn quoting_modifiers() {
        let history = entries(&[(1, "echo one two three")]);
        assert_eq!(expand("!1:1-$:q", &history), "'one two three'");
        assert_eq!(expand("!1:1-$:x", &history), "'one' 'two' 'three'");
        let history = entries(&[(1, "echo a'b c")]);
        assert_eq!(expand("!1:1-$:q", &history), "'a'\\''b c'");
        assert_eq!(expand("!1:1-$:x", &history), "'a'\\''b' 'c'");
    }

    #[test]
    fn print_flag_and_last_modifier() {
        let history = entries(&[(1, "echo hi")]);
        let mut session = Session::new();
        let expansion = session.expand(b"!!:p", &history).unwrap();
        assert!(expansion.print());
        assert!(expansion.print_only());
        assert_eq!(expansion.text(), b"echo hi");
        let expansion = session.expand(b"!!:p:q", &history).unwrap();
        assert!(expansion.print());
        assert!(!expansion.print_only());
        assert_eq!(expansion.text(), b"'echo hi'");
    }

    #[test]
    fn quoting_inhibition_and_escapes() {
        let history = entries(&[(1, "echo hi")]);
        assert_eq!(expand("echo '!!'", &history), "echo '!!'");
        assert_eq!(expand("echo $'!!'", &history), "echo $'!!'");
        assert_eq!(expand("echo \"!!\"", &history), "echo \"echo hi\"");
        assert_eq!(expand("echo \\!!", &history), "echo \\!!");
        assert_eq!(expand("echo \\\\!!", &history), "echo \\\\!!");
        assert_eq!(expand("echo \\\\!!x", &history), "echo \\\\echo hix");
        assert_eq!(expand("echo a\\!b", &history), "echo a\\!b");
        assert_eq!(expand("echo 'a'\\!!", &history), "echo 'a'\\!!");
    }

    #[test]
    fn bang_at_end_and_after_non_event_bytes_is_literal() {
        let history = entries(&[(1, "echo hi")]);
        for line in [
            "echo wow!",
            "echo != x",
            "echo a!(b",
            "echo a!)b",
            "echo a!;b",
            "echo a! | b",
            "echo a! b",
        ] {
            let mut session = Session::new();
            let expansion = session.expand(line.as_bytes(), &history).unwrap();
            assert_eq!(expansion.text(), line.as_bytes(), "{line}");
            assert!(!expansion.changed());
        }
    }

    #[test]
    fn comments_are_not_expanded() {
        let history = entries(&[(1, "echo hi")]);
        assert_eq!(expand("true; # !!", &history), "true; # !!");
        assert_eq!(expand("echo a#b !!", &history), "echo a#b echo hi");
        assert_eq!(expand("echo \"#\" !!", &history), "echo \"#\" echo hi");
    }

    #[test]
    fn line_so_far_uses_the_expanded_prefix() {
        let history = entries(&[(1, "echo hi")]);
        assert_eq!(expand("echo A !#", &history), "echo A echo A ");
        assert_eq!(expand("echo a!#X", &history), "echo aecho aX");
        assert_eq!(expand("echo !! !#", &history), "echo echo hi echo echo hi ");
        assert_eq!(expand("!#", &history), "");
    }

    #[test]
    fn brace_form_is_not_a_bash_event() {
        let history = entries(&[(1, "echo hi there")]);
        let mut session = Session::new();
        let error = session.expand(b"!{echo}", &history).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::EventNotFound);
        assert_eq!(error.token(), b"!{echo}");
    }

    #[test]
    fn bad_word_specifier_reports_the_designator() {
        let history = entries(&[(1, "solo")]);
        let mut session = Session::new();
        let error = session.expand(b"!^", &history).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::BadWordSpecifier);
        assert_eq!(error.token(), b"^");
        let error = session.expand(b"!1:5", &history).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::BadWordSpecifier);
        assert_eq!(error.token(), b":5");
    }

    #[test]
    fn unknown_modifier_reports_the_letter() {
        let history = entries(&[(1, "echo hi")]);
        let mut session = Session::new();
        let error = session.expand(b"!1:z", &history).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnknownModifier(b'z'));
        assert_eq!(error.to_string(), "z: unrecognized history modifier");
    }

    #[test]
    fn text_with_no_history_syntax_is_unchanged() {
        let history = entries(&[(1, "echo hi")]);
        let mut session = Session::new();
        let expansion = session.expand(b"echo plain", &history).unwrap();
        assert_eq!(expansion.text(), b"echo plain");
        assert!(!expansion.changed());
        assert!(!expansion.print());
    }

    #[test]
    fn multi_line_chunk_expands_each_line() {
        let history = entries(&[(1, "echo hi")]);
        let mut session = Session::new();
        let expansion = session
            .expand(b"if true; then\n!!\nfi\n", &history)
            .unwrap();
        assert_eq!(expansion.text(), b"if true; then\necho hi\nfi\n");
        assert!(expansion.changed());
        assert!(!expansion.print_only());
    }

    #[test]
    fn single_quote_state_carries_across_lines_but_double_quote_does_not() {
        let mut session = Session::new();
        let history = entries(&[(1, "echo hi")]);
        let expansion = session.expand(b"echo 'a\n!!'\n", &history).unwrap();
        assert_eq!(expansion.text(), b"echo 'a\n!!'\n");
        let mut session = Session::new();
        let expansion = session.expand(b"echo \"a\n!!\"\n", &history).unwrap();
        assert_eq!(expansion.text(), b"echo \"a\necho hi\"\n");
    }

    #[test]
    fn heredoc_bodies_are_not_expanded() {
        let mut session = Session::new();
        let history = entries(&[(1, "echo hi")]);
        let expansion = session
            .expand(b"cat <<EOF\n!!\nEOF\n!!\n", &history)
            .unwrap();
        assert_eq!(expansion.text(), b"cat <<EOF\n!!\nEOF\necho hi\n");
    }

    #[test]
    fn quoted_heredoc_delimiters_skip_the_body_too() {
        let mut session = Session::new();
        let history = entries(&[(1, "echo hi")]);
        let expansion = session.expand(b"cat <<'EOF'\n!!\nEOF\n", &history).unwrap();
        assert_eq!(expansion.text(), b"cat <<'EOF'\n!!\nEOF\n");
    }

    #[test]
    fn tab_stripping_heredoc_terminator() {
        let mut session = Session::new();
        let history = entries(&[(1, "echo hi")]);
        let expansion = session
            .expand(b"cat <<-EOF\n!!\n\tEOF\n!!\n", &history)
            .unwrap();
        assert_eq!(expansion.text(), b"cat <<-EOF\n!!\n\tEOF\necho hi\n");
    }

    #[test]
    fn previous_search_operand_is_reused() {
        let history = entries(&[(1, "echo one two"), (2, "echo three four")]);
        let mut session = Session::new();
        assert_eq!(
            expand_state("!?one? && !?", &history, &mut session).unwrap(),
            "echo one two && echo one two"
        );
    }

    #[test]
    fn percent_uses_the_word_matched_by_the_search() {
        let history = entries(&[(1, "echo alpha beta")]);
        let mut session = Session::new();
        assert_eq!(
            expand_state("!?beta?:p", &history, &mut session).unwrap(),
            "echo alpha beta"
        );
        assert_eq!(expand_state("!%", &history, &mut session).unwrap(), "beta");
    }
}
