//! Quoting for bracketed paste.
//!
//! A paste is bytes copied from somewhere else: a URL, a path, a line of
//! markdown. It is *data*, and the shell reads it as syntax — which is how
//! `https://…/watch?v=abc&t=10` becomes two commands with the first one
//! backgrounded, how `?q=$USER` quietly becomes a home directory, and how a
//! backtick in a query string executes whatever is inside it. Typing those
//! characters is deliberate; pasting them is not, so a paste that would change
//! the parse is quoted and the user gets one argument.
//!
//! The guess is deliberately narrow, because the two false positives are real
//! use cases:
//!
//! * **A pasted command stays a command.** `make -j8 && make test` has spaces,
//!   so it is never touched; whitespace alone is not a reason to quote.
//! * **A pasted glob stays a glob.** `*.rs` and `~/src` lex as one plain word
//!   and are left alone, so `ls *.rs` still expands. Globbing happens after the
//!   lexer, which is why these two rules can be stated in terms of "would the
//!   *parse* change" rather than "is there a metacharacter".
//!
//! What is left is one token of data that the lexer would not read as one
//! literal word: an operator (`& ; | ( ) < >`), a quote or backslash, an
//! expansion (`$` or a backtick), history expansion (`!`), a leading `#` (a
//! comment), or a path that begins with `/`, `~`, `./` or `../` and contains a
//! space. Those are quoted, and the quote is what the user sees: the line is
//! syntax-highlighted, so the paste is painted as a string.
//!
//! None of this applies to a paste with a newline in it: a pasted script is
//! commands by definition, and folding it into one quoted word would be worse
//! than useless. The editor shows such a block as a multiline edit — one row
//! per line, each inner row behind the continuation prompt — and nothing in it
//! submits anything, so running it waits for `Enter`. Neither does it apply, at a bare cursor, to a paste that
//! already arrives quoted — the user copied a shell-ready argument, and
//! `''url''` would only be noise. Inside the user's own quotes there is no such
//! thing: the bytes are literal data, and a `'` among them still has to be
//! escaped or it closes the string.
//!
//! `fish` reached the opposite conclusion (`fish-shell#2188`, closed as a
//! duplicate): it leaves a pasted URL alone and relies on one `Ctrl-Z` undoing
//! the whole paste. That escape hatch exists here too — a quoted paste is one
//! undo step, and the first `Backspace` after it deletes both quotes — which is
//! what makes the guess cheap enough to make by default.

use baash_lang::{QuoteCtx, quote};

/// The bytes a paste actually contributes.
///
/// A copy from a browser or a Windows editor carries `\r\n`, and a lone `\r`
/// reaching the line is a `^M` inside the word — visible in the frame, and
/// silently part of the argument. Both become `\n`; every other byte is left
/// exactly as it was copied, escapes included, because `docs/contracts.md`
/// promises that a paste is inserted verbatim and a shell that quietly drops
/// bytes from the user's clipboard is worse than one that shows them.
///
/// One trailing newline goes: copying a line usually takes its terminator, and
/// a block that ends with an empty row makes `Enter` run on nothing.
pub(crate) fn normalise(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\r' {
            if bytes.get(index + 1) == Some(&b'\n') {
                index += 1;
            }
            out.push(b'\n');
        } else {
            out.push(bytes[index]);
        }
        index += 1;
    }
    if out.last() == Some(&b'\n') {
        out.pop();
    }
    out
}

/// A paste's quoted form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Quoted {
    /// The bytes to insert.
    pub text: Vec<u8>,
    /// Whether `text` is one new `'…'` pair wrapped around the paste.
    ///
    /// Only then can a `Backspace` take the pair off again. An escape made
    /// *inside* an existing quote has no pair of its own — the inserted bytes
    /// need not begin or end with a quote — so the editor must not arm that
    /// hatch for it.
    pub wrapped: bool,
}

/// The quoted form of a paste, or `None` when it is to be inserted verbatim.
///
/// `ctx` is the quote context at the insertion point, so a paste that lands
/// inside `"…"` only gets what that context makes dangerous escaped, and a
/// paste inside `'…'` becomes the familiar `'\''` when it carries an
/// apostrophe.
pub(crate) fn quoted(bytes: &[u8], ctx: QuoteCtx) -> Option<Quoted> {
    if bytes.is_empty() || bytes.contains(&b'\n') {
        return None;
    }
    // "Already quoted" only means anything at a bare cursor, where the paste
    // could be one shell-ready word. Inside the user's own `'…'` the same bytes
    // are literal data, and a `'` among them still has to be escaped or it
    // closes the string the user opened.
    if ctx == QuoteCtx::Unquoted && already_quoted(bytes) {
        return None;
    }
    match ctx {
        // Inside single quotes the only byte that can end the string is the
        // quote itself, and `quote` turns each into `'\''`. This is a
        // correctness repair rather than a judgement: pasting `don't` into
        // `'…'` otherwise leaves the line in an unterminated string, which is
        // what `fish` escapes for and nothing more.
        QuoteCtx::SingleQuoted => Some(Quoted {
            text: quote(bytes, QuoteCtx::SingleQuoted),
            wrapped: false,
        }),
        // Inside double quotes `$`, a backtick, a backslash, `"` and `!` are
        // still live; `&`, `;` and the rest are already literal.
        QuoteCtx::DoubleQuoted => wants_quoting(bytes, false).then(|| Quoted {
            text: quote(bytes, QuoteCtx::DoubleQuoted),
            wrapped: false,
        }),
        // A bare word: wrap the whole paste in one pair of single quotes, so
        // what the user sees is one argument. `quote` is asked for the
        // single-quoted escape of the *contents*, which is exactly the inner
        // text of such a word.
        QuoteCtx::Unquoted => wants_quoting(bytes, true).then(|| {
            let mut out = Vec::with_capacity(bytes.len() + 2);
            out.push(b'\'');
            out.extend_from_slice(&quote(bytes, QuoteCtx::SingleQuoted));
            out.push(b'\'');
            Quoted {
                text: out,
                wrapped: true,
            }
        }),
    }
}

/// Whether `bytes` is data the parse would otherwise reinterpret.
///
/// `operators` says whether `&`, `;`, `|`, `(`, `)`, `<` and `>` count; inside
/// double quotes they do not, and the caller knows which context it is in.
fn wants_quoting(bytes: &[u8], operators: bool) -> bool {
    if bytes.first().is_some_and(|first| *first == b'#') {
        // A `#` that begins a word begins a comment.
        return true;
    }
    if is_path_with_a_space(bytes) {
        return true;
    }
    let has_space = bytes.iter().any(|byte| byte.is_ascii_whitespace());
    if has_space {
        // Whitespace only means "several words", and a pasted command is
        // several words on purpose. A space inside a *path* is handled above.
        return false;
    }
    bytes.iter().any(|&byte| is_live(byte, operators))
}

/// Bytes that change how a word lexes or expands.
fn is_live(byte: u8, operators: bool) -> bool {
    match byte {
        // A quote, or a backslash that escapes one.
        b'\'' | b'"' | b'\\' => true,
        // An expansion, or history expansion.
        b'$' | b'`' | b'!' => true,
        // Globs and brace expansion are deliberately *not* here: `*.rs` and
        // `{a,b}` are data the user wants expanded.
        b'&' | b';' | b'|' | b'(' | b')' | b'<' | b'>' => operators,
        _ => false,
    }
}

/// Whether `bytes` looks like a path that needs quoting to stay one word.
fn is_path_with_a_space(bytes: &[u8]) -> bool {
    let path = bytes.starts_with(b"/")
        || bytes.starts_with(b"~")
        || bytes.starts_with(b"./")
        || bytes.starts_with(b"../");
    path && bytes.iter().any(|byte| byte.is_ascii_whitespace())
}

/// Whether the paste already is one quoted word.
///
/// The user copied something they had already quoted for a shell; wrapping it
/// again would put the quotes *inside* the string. Only a balanced pair counts,
/// so an apostrophe-only paste (`don't`) is still quoted.
fn already_quoted(bytes: &[u8]) -> bool {
    let (Some(&first), Some(&last)) = (bytes.first(), bytes.last()) else {
        return false;
    };
    if first != last || !matches!(first, b'\'' | b'"') || bytes.len() < 2 {
        return false;
    }
    // Both ends being the same quote is not enough: `'a' & 'b'` also starts
    // and ends with a quote and is not one word. Require that the byte is not
    // closed early, i.e. an even count over the whole paste with the interior
    // holding no backslash-escaped copy of the same quote at the end.
    let interior = &bytes[1..bytes.len() - 1];
    if first == b'\'' {
        !interior.contains(&b'\'')
    } else {
        !interior
            .iter()
            .enumerate()
            .any(|(index, byte)| *byte == b'"' && !escaped(interior, index))
    }
}

/// Whether the byte at `index` is preceded by an odd number of backslashes.
fn escaped(bytes: &[u8], index: usize) -> bool {
    bytes[..index]
        .iter()
        .rev()
        .take_while(|byte| **byte == b'\\')
        .count()
        % 2
        == 1
}

#[cfg(test)]
mod tests {
    use super::{QuoteCtx, normalise, quoted};

    fn unquoted(bytes: &[u8]) -> Option<String> {
        quoted(bytes, QuoteCtx::Unquoted).map(|out| String::from_utf8_lossy(&out.text).into_owned())
    }

    #[test]
    fn a_pasted_block_gets_line_endings_not_carriage_returns() {
        // What a browser or a Windows editor puts on the clipboard.
        assert_eq!(
            normalise(b"echo one\r\necho two\r\n"),
            b"echo one\necho two"
        );
        // A lone `\r` is a line break too; neither leaves a `^M` in a word.
        assert_eq!(normalise(b"echo one\recho two"), b"echo one\necho two");
        // Blank lines inside the block survive; only the one at the end goes.
        assert_eq!(normalise(b"a\n\nb\n"), b"a\n\nb");
        // Everything else is the user's bytes, escapes included.
        assert_eq!(normalise(b"echo \x1b[A"), b"echo \x1b[A");
        assert_eq!(normalise(b"\tkeep the tab\n"), b"\tkeep the tab");
        assert_eq!(normalise(b""), b"");
    }

    #[test]
    fn a_pasted_url_becomes_one_argument() {
        // The case that started this: `&` in the query string, and a `?` that
        // would otherwise be a glob in a slash-free word.
        assert_eq!(
            unquoted(b"https://www.youtube.com/watch?v=abc&t=10").as_deref(),
            Some("'https://www.youtube.com/watch?v=abc&t=10'")
        );
        // `$` and a backtick are the dangerous ones: they expand or execute.
        assert_eq!(
            unquoted(b"https://x/?q=$USER").as_deref(),
            Some("'https://x/?q=$USER'")
        );
        assert_eq!(
            unquoted(b"https://x/a`id`").as_deref(),
            Some("'https://x/a`id`'")
        );
        // Wikipedia's parentheses are a parse error today.
        assert_eq!(
            unquoted(b"https://en.wikipedia.org/wiki/Foo_(bar)").as_deref(),
            Some("'https://en.wikipedia.org/wiki/Foo_(bar)'")
        );
    }

    #[test]
    fn a_pasted_command_is_left_alone() {
        // Whitespace is not a reason to quote: a pasted pipeline is several
        // words on purpose.
        assert_eq!(unquoted(b"make -j8 && make test"), None);
        assert_eq!(unquoted(b"git status"), None);
        assert_eq!(unquoted(b"curl -fsSL https://x/install.sh | sh"), None);
    }

    #[test]
    fn a_pasted_glob_and_a_home_path_keep_expanding() {
        // Both lex as one plain word, so the parse does not change and there is
        // nothing to escape; `ls *.rs` and `cd ~/src` are what the user meant.
        assert_eq!(unquoted(b"*.rs"), None);
        assert_eq!(unquoted(b"~/src/github.com"), None);
        assert_eq!(unquoted(b"{a,b}.txt"), None);
        assert_eq!(unquoted(b"/usr/share/doc"), None);
    }

    #[test]
    fn a_pasted_path_with_a_space_is_quoted() {
        assert_eq!(
            unquoted(b"/tmp/My File.txt").as_deref(),
            Some("'/tmp/My File.txt'")
        );
        assert_eq!(
            unquoted(b"~/My Documents/x").as_deref(),
            Some("'~/My Documents/x'")
        );
        assert_eq!(unquoted(b"./a b/c").as_deref(), Some("'./a b/c'"));
        // Not a path: a two-word command is not quoted for having a space.
        assert_eq!(unquoted(b"echo hello world"), None);
    }

    #[test]
    fn an_apostrophe_would_open_a_string() {
        // `don't` is an unterminated single quote; the wrapped form is one
        // argument and reads the way the user typed it.
        assert_eq!(unquoted(b"don't").as_deref(), Some("'don'\\''t'"));
        assert_eq!(unquoted(b"it's a test"), None);
    }

    #[test]
    fn a_leading_hash_is_a_comment() {
        assert_eq!(unquoted(b"#section-2").as_deref(), Some("'#section-2'"));
        // Mid-word `#` is literal, so nothing to do.
        assert_eq!(unquoted(b"page#section"), None);
    }

    #[test]
    fn a_multi_line_paste_is_commands() {
        // A pasted script must stay a script.
        assert_eq!(unquoted(b"echo one\necho two"), None);
        assert_eq!(unquoted(b"make && make install\nmake check"), None);
    }

    #[test]
    fn an_already_quoted_paste_is_not_wrapped_again() {
        assert_eq!(unquoted(b"'my file.txt'"), None);
        assert_eq!(unquoted(b"\"a&b\""), None);
        // …but one that merely starts and ends with a quote is not one word.
        assert_eq!(
            unquoted(b"'a' & 'b'").as_deref(),
            None,
            "it holds spaces and is not path-shaped, so the parse is left alone"
        );
    }

    /// An escape inside the user's own quote is not a wrapped word, so the
    /// editor must not offer to delete "the pair": the pasted bytes may start
    /// with a quote that belongs to the paste.
    #[test]
    fn an_in_place_escape_is_not_wrapped() {
        let out = quoted(b"'x'", QuoteCtx::SingleQuoted).expect("an escape");
        assert!(!out.wrapped, "no pair was added around it");
        assert_eq!(String::from_utf8_lossy(&out.text), r"'\''x'\''");
        let out = quoted(b"a&b", QuoteCtx::Unquoted).expect("a wrap");
        assert!(out.wrapped, "the whole paste was wrapped");
    }

    #[test]
    fn inside_single_quotes_only_the_quote_is_escaped() {
        // A correctness repair, not a judgement: `&` is already literal here.
        assert_eq!(
            quoted(b"a&b", QuoteCtx::SingleQuoted)
                .map(|out| String::from_utf8_lossy(&out.text).into_owned())
                .as_deref(),
            Some("a&b")
        );
        assert_eq!(
            quoted(b"don't", QuoteCtx::SingleQuoted)
                .map(|out| String::from_utf8_lossy(&out.text).into_owned())
                .as_deref(),
            Some("don'\\''t")
        );
    }

    #[test]
    fn inside_double_quotes_only_the_live_bytes_are_escaped() {
        let quoted_form = |bytes: &[u8]| {
            quoted(bytes, QuoteCtx::DoubleQuoted)
                .map(|out| String::from_utf8_lossy(&out.text).into_owned())
        };
        // `&` and `?` are literal inside `"…"`; `$` is not.
        assert_eq!(quoted_form(b"a&b"), None);
        assert_eq!(quoted_form(b"a$b").as_deref(), Some("a\\$b"));
        assert_eq!(quoted_form(b"a`b").as_deref(), Some("a\\`b"));
    }
}
