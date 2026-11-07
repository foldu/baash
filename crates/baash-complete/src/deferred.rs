//! The `-F` handoff: the input a completion function needs, and the replies it
//! returns.
//!
//! `complete -F` names a shell function, so the completion is shell code. This
//! crate deliberately has no interpreter (`DESIGN.md` §4.1), so the engine does
//! the part that needs only the line — it decides *that* a function must run and
//! assembles exactly the input bash gives it — and hands the caller a
//! [`Deferred`]. The caller runs the function, reads `COMPREPLY` and comes back
//! with [`Replies`]; [`crate::CompletionEngine::deferred_menu`] then applies the
//! rule's decoration and options the same way the data path does.
//!
//! Everything here is measured against bash 5.3.20 through a real pseudo-
//! terminal, because the manual does not say most of it:
//!
//! * `COMP_LINE` is the *current command*, not the whole editing line: after
//!   `true; cmd foo al<TAB>` bash reports `cmd foo al`, and `COMP_POINT` is an
//!   offset into that.
//! * `COMP_WORDS` is split on `COMP_WORDBREAKS`, not by the shell grammar: a
//!   break character is an element of its own (`cmd a=b c` gives `a`, `=`, `b`),
//!   quotes stay in the element (`"a b"` is one word, quotes included), and an
//!   empty current word is an empty element (`cmd foo ` has `COMP_CWORD` 2).
//! * the function is called as `$1` command, `$2` current word, `$3` previous
//!   word, with `COMP_TYPE` and `COMP_KEY` 9 for Tab.
//! * `-P`/`-S` decorate the function's replies too (`complete -P PRE -F f cmd`
//!   inserts `PREalpha`), `-W` is *not* added when `-F` is present, and a reply
//!   is not filtered against the typed prefix (`al<TAB>` inserts `zzz`).
//! * `compopt -o filenames` appends `/` to a reply that names a directory and
//!   suppresses the space; `compopt -o nospace` suppresses the space alone.
//!
//! `COMP_WORDBREAKS` is read from the shell when it differs from the default,
//! because bash-completion edits it (`_comp_initialize` relies on that).

use crate::candidate::Span;
use crate::context::Ctx;
use crate::spec::Spec;

/// bash's default `COMP_WORDBREAKS`.
pub const DEFAULT_WORDBREAKS: &[u8] = b" \t\n\"'><=;|&(:";

/// What the shell must hand a `-F` function.
///
/// Built by [`crate::CompletionEngine::deferred`]; a `Deferred` is only produced
/// when the argument slot has a `complete` rule whose source is a function.
#[derive(Clone, Debug)]
pub struct Deferred {
    /// The context the menu is built in once the replies are back.
    ctx: Ctx,
    /// The rule: its `-P`/`-S` decoration, options and fallbacks.
    spec: Spec,
    /// The `-F` function name.
    function: Vec<u8>,
    /// `COMP_LINE`: the current command, from the last separator.
    line: Vec<u8>,
    /// `COMP_POINT`: the cursor's offset into `line`.
    point: usize,
    /// `COMP_WORDS`: `line` split on `COMP_WORDBREAKS`.
    words: Vec<Vec<u8>>,
    /// `COMP_CWORD`: the index of the word the cursor is in.
    cword: usize,
}

impl Deferred {
    /// Assemble the handoff for `cursor` in `line`.
    ///
    /// `breaks` is the shell's `COMP_WORDBREAKS`; an empty value selects bash's
    /// default.
    pub(crate) fn new(
        ctx: Ctx,
        spec: Spec,
        function: Vec<u8>,
        line: &[u8],
        cursor: usize,
        breaks: &[u8],
    ) -> Self {
        let cursor = cursor.min(line.len());
        let start = command_start(line, cursor);
        let region = &line[start..cursor.max(start)];
        let (words, cword) = split_words(region, cursor - start, breaks);
        Self {
            ctx,
            spec,
            function,
            point: cursor - start,
            line: region.to_vec(),
            words,
            cword,
        }
    }

    /// The function `complete -F` named.
    pub fn function(&self) -> &[u8] {
        &self.function
    }

    /// `COMP_LINE`.
    pub fn line(&self) -> &[u8] {
        &self.line
    }

    /// `COMP_POINT`, an offset into [`Deferred::line`].
    pub fn point(&self) -> usize {
        self.point
    }

    /// `COMP_WORDS`.
    pub fn words(&self) -> &[Vec<u8>] {
        &self.words
    }

    /// `COMP_CWORD`.
    pub fn cword(&self) -> usize {
        self.cword
    }

    /// The command word, which bash passes as the function's `$1`.
    pub fn command(&self) -> &[u8] {
        self.ctx.command_name.as_deref().unwrap_or(b"")
    }

    /// The word under the cursor, which bash passes as `$2`.
    pub fn current_word(&self) -> &[u8] {
        self.words.get(self.cword).map(Vec::as_slice).unwrap_or(b"")
    }

    /// The word before the cursor, which bash passes as `$3`.
    pub fn previous_word(&self) -> &[u8] {
        if self.cword == 0 {
            return b"";
        }
        self.words
            .get(self.cword - 1)
            .map(Vec::as_slice)
            .unwrap_or(b"")
    }

    /// The rule, so the caller can start `compopt` with its options.
    pub fn spec(&self) -> &Spec {
        &self.spec
    }

    /// The completion context, for building the menu after the replies arrive.
    pub(crate) fn ctx(&self) -> &Ctx {
        &self.ctx
    }

    /// The `-P`/`-S` decoration, which applies to replies as well.
    pub fn decorate(&self, text: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            text.len()
                + self.spec.prefix.as_ref().map_or(0, Vec::len)
                + self.spec.suffix.as_ref().map_or(0, Vec::len),
        );
        if let Some(prefix) = &self.spec.prefix {
            out.extend_from_slice(prefix);
        }
        out.extend_from_slice(text);
        if let Some(suffix) = &self.spec.suffix {
            out.extend_from_slice(suffix);
        }
        out
    }
}

/// What a completion function produced, and the options in force after
/// `compopt` had its say.
///
/// `options` is the *effective* set: the rule's `-o` options, then whatever the
/// function turned on or off with `compopt`.
#[derive(Clone, Debug, Default)]
pub struct Replies {
    /// `COMPREPLY`, in the order the function assigned it.
    pub words: Vec<Vec<u8>>,
    /// The effective `-o` options.
    pub options: Vec<Vec<u8>>,
}

impl Replies {
    /// Replies from a function and the options in force when it returned.
    pub fn new(words: Vec<Vec<u8>>, options: Vec<Vec<u8>>) -> Self {
        Self { words, options }
    }
}

/// The index of the first byte of the command the cursor is in.
///
/// bash trims `COMP_LINE` at the last command separator: completing
/// `true; cmd foo al` reports `cmd foo al`, and `COMP_POINT` counts from
/// `cmd`. The lexer decides what a separator is, so a `;` inside a quote or a
/// substitution does not trim anything.
pub fn command_start(line: &[u8], cursor: usize) -> usize {
    let cursor = cursor.min(line.len());
    let lexed = baash_syntax::lex_full(line);
    let mut start = 0;
    for token in &lexed.tokens {
        if token.span.end > cursor {
            break;
        }
        if matches!(
            token.kind,
            baash_syntax::TokenKind::Operator | baash_syntax::TokenKind::Newline
        ) {
            start = token.span.end;
        }
    }
    // bash trims the separating whitespace too: `true; cmd …` reports `cmd …`,
    // not ` cmd …`.
    while start < cursor && matches!(line[start], b' ' | b'\t') {
        start += 1;
    }
    start
}

/// Split `text` on `COMP_WORDBREAKS`, returning the words and the cursor's
/// index.
///
/// `cursor` is an offset into `text`. Quote characters are kept and quoted
/// breaks are ignored, which is what bash's splitter does; a break character
/// that is not layout becomes a word of its own.
pub fn split_words(text: &[u8], cursor: usize, breaks: &[u8]) -> (Vec<Vec<u8>>, usize) {
    let breaks: &[u8] = if breaks.is_empty() {
        DEFAULT_WORDBREAKS
    } else {
        breaks
    };
    let cursor = cursor.min(text.len());
    let mut words: Vec<Vec<u8>> = Vec::new();
    let mut spans: Vec<Span> = Vec::new();
    let mut current = Vec::new();
    let mut start = 0usize;
    let mut ends_with_break = true;
    let mut quote = Quote::None;
    let mut at = 0usize;
    while at < text.len() {
        let byte = text[at];
        if quote == Quote::Single {
            current.push(byte);
            if byte == b'\'' {
                quote = Quote::None;
            }
            ends_with_break = false;
            at += 1;
            continue;
        }
        if quote == Quote::Double {
            current.push(byte);
            match byte {
                b'\\' => {
                    // Keep the escaped byte in the word, whatever it is.
                    if let Some(next) = text.get(at + 1) {
                        current.push(*next);
                        at += 2;
                        ends_with_break = false;
                        continue;
                    }
                }
                b'"' => quote = Quote::None,
                _ => {}
            }
            ends_with_break = false;
            at += 1;
            continue;
        }
        match byte {
            b'\\' => {
                current.push(byte);
                if let Some(next) = text.get(at + 1) {
                    current.push(*next);
                    at += 2;
                } else {
                    at += 1;
                }
                ends_with_break = false;
                continue;
            }
            b'\'' => {
                quote = Quote::Single;
                current.push(byte);
                ends_with_break = false;
                at += 1;
                continue;
            }
            b'"' => {
                quote = Quote::Double;
                current.push(byte);
                ends_with_break = false;
                at += 1;
                continue;
            }
            _ => {}
        }
        if breaks.contains(&byte) {
            if !current.is_empty() {
                spans.push(start..at);
                words.push(std::mem::take(&mut current));
            }
            if !byte.is_ascii_whitespace() {
                spans.push(at..at + 1);
                words.push(vec![byte]);
            }
            ends_with_break = true;
            at += 1;
            continue;
        }
        if current.is_empty() {
            start = at;
        }
        current.push(byte);
        ends_with_break = false;
        at += 1;
    }
    if !current.is_empty() {
        spans.push(start..text.len());
        words.push(current);
    }

    // The cursor is in the word whose span contains it. At the end of the text
    // the answer depends on what the text ends with: `cmd al` and its cursor
    // are in `al`, while `cmd ` has an empty current word after the space.
    let index = if cursor == text.len() {
        if ends_with_break {
            words.push(Vec::new());
            spans.push(text.len()..text.len());
            words.len() - 1
        } else {
            words.len().saturating_sub(1)
        }
    } else if let Some(index) = spans.iter().position(|span| span.contains(&cursor)) {
        index
    } else {
        // Between words, e.g. the cursor on the space after `cmd`: an empty
        // word at the cursor, at the position the spans put it.
        let index = spans
            .iter()
            .position(|span| span.start >= cursor)
            .unwrap_or(words.len());
        words.insert(index, Vec::new());
        spans.insert(index, cursor..cursor);
        index
    };
    (words, index)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    None,
    Single,
    Double,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_command_splits_on_whitespace() {
        assert_eq!(
            split_words(b"cmd al", 6, DEFAULT_WORDBREAKS),
            (vec![b"cmd".to_vec(), b"al".to_vec()], 1)
        );
    }

    #[test]
    fn an_empty_current_word_is_an_empty_element() {
        // Measured: `cmd foo ` has COMP_CWORD 2 and `$2` empty.
        let (words, cword) = split_words(b"cmd foo ", 8, DEFAULT_WORDBREAKS);
        assert_eq!(words, vec![b"cmd".to_vec(), b"foo".to_vec(), Vec::new()]);
        assert_eq!(cword, 2);
    }

    #[test]
    fn quotes_are_kept_and_quoted_spaces_do_not_split() {
        // Measured: `cmd "a b" c` gives `"a b"` as one word.
        let (words, cword) = split_words(b"\"a b\" c", 7, DEFAULT_WORDBREAKS);
        assert_eq!(words, vec![b"\"a b\"".to_vec(), b"c".to_vec()]);
        assert_eq!(cword, 1);
    }

    #[test]
    fn word_breaks_become_elements_of_their_own() {
        // Measured: `cmd a=b c` gives `a`, `=`, `b`, `c`.
        let (words, cword) = split_words(b"a=b c", 5, DEFAULT_WORDBREAKS);
        assert_eq!(
            words,
            vec![b"a".to_vec(), b"=".to_vec(), b"b".to_vec(), b"c".to_vec()]
        );
        assert_eq!(cword, 3);
    }

    #[test]
    fn a_break_inside_quotes_is_not_a_break() {
        let (words, _) = split_words(b"'a=b' c", 6, DEFAULT_WORDBREAKS);
        assert_eq!(words, vec![b"'a=b'".to_vec(), b"c".to_vec()]);
    }

    #[test]
    fn an_escaped_break_stays_in_the_word() {
        let (words, _) = split_words(b"a\\=b", 4, DEFAULT_WORDBREAKS);
        assert_eq!(words, vec![b"a\\=b".to_vec()]);
    }

    #[test]
    fn a_custom_break_set_is_honoured() {
        let (words, cword) = split_words(b"cmd a.b", 7, b" .");
        assert_eq!(
            words,
            vec![b"cmd".to_vec(), b"a".to_vec(), b".".to_vec(), b"b".to_vec()]
        );
        assert_eq!(cword, 3);
    }

    #[test]
    fn the_region_starts_after_the_last_separator() {
        // Measured: completing `true; cmd foo al` reports `cmd foo al`.
        assert_eq!(command_start(b"true; cmd foo al", 16), 6);
        assert_eq!(command_start(b"cmd foo al", 10), 0);
        assert_eq!(command_start(b"a; b\";\"", 7), 3);
        assert_eq!(command_start(b"echo $(a; b) x", 13), 0);
    }

    #[test]
    fn an_empty_line_has_one_empty_word() {
        assert_eq!(
            split_words(b"", 0, DEFAULT_WORDBREAKS),
            (vec![Vec::new()], 0)
        );
    }
}
