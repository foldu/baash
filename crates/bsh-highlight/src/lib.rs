//! Syntax highlighting: a token stream plus a theme becomes styled spans.
//!
//! The mapping is deliberately role-based rather than colour-based: a theme
//! names *what a token is*, and the renderer decides how to paint it. That is
//! what lets the same theme drive live highlighting in the line editor, the
//! completion menu, and (later) `miette`'s snippet highlighter without
//! duplicating the classifier.
//!
//! Rendering takes and returns **bytes**. Shell text is bytes, terminal output
//! is bytes, and routing display through `str` would force a lossy conversion
//! on every keystroke.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use bsh_syntax::{TokenKind, lex};

/// A terminal foreground colour, by role rather than by RGB so themes and
/// `NO_COLOR` degrade predictably.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Color {
    /// Inherit the terminal's foreground.
    #[default]
    Default,
    /// Black.
    Black,
    /// Red.
    Red,
    /// Green.
    Green,
    /// Yellow.
    Yellow,
    /// Blue.
    Blue,
    /// Magenta.
    Magenta,
    /// Cyan.
    Cyan,
    /// White/grey.
    Gray,
    /// Bright red.
    BrightRed,
    /// Bright green.
    BrightGreen,
    /// Bright yellow.
    BrightYellow,
    /// Bright blue.
    BrightBlue,
    /// Bright magenta.
    BrightMagenta,
    /// Bright cyan.
    BrightCyan,
    /// Bright white.
    BrightWhite,
}

impl Color {
    fn ansi(self) -> Option<u8> {
        let code = match self {
            Color::Default => return None,
            Color::Black => 30,
            Color::Red => 31,
            Color::Green => 32,
            Color::Yellow => 33,
            Color::Blue => 34,
            Color::Magenta => 35,
            Color::Cyan => 36,
            Color::Gray => 37,
            Color::BrightRed => 91,
            Color::BrightGreen => 92,
            Color::BrightYellow => 93,
            Color::BrightBlue => 94,
            Color::BrightMagenta => 95,
            Color::BrightCyan => 96,
            Color::BrightWhite => 97,
        };
        Some(code)
    }
}

/// How a token should look.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    /// Foreground colour.
    pub color: Color,
    /// Bold.
    pub bold: bool,
    /// Italic.
    pub italic: bool,
    /// Underline.
    pub underline: bool,
}

impl Style {
    /// A style that changes nothing.
    pub const PLAIN: Style = Style {
        color: Color::Default,
        bold: false,
        italic: false,
        underline: false,
    };

    /// Whether painting this style would be a no-op.
    pub fn is_plain(self) -> bool {
        self == Style::PLAIN
    }

    /// The SGR sequence that enters this style, or `None` when plain.
    pub fn sgr(self) -> Option<String> {
        if self.is_plain() {
            return None;
        }
        let mut parts: Vec<String> = Vec::new();
        if self.bold {
            parts.push("1".into());
        }
        if self.italic {
            parts.push("3".into());
        }
        if self.underline {
            parts.push("4".into());
        }
        if let Some(fg) = self.color.ansi() {
            parts.push(fg.to_string());
        }
        Some(format!("\x1b[{}m", parts.join(";")))
    }
}

/// SGR reset.
const RESET: &[u8] = b"\x1b[0m";

/// Role-to-style mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    /// Reserved words: `if`, `then`, `esac`, `[[` …
    pub keyword: Style,
    /// Control operators: `|`, `&&`, `;` …
    pub operator: Style,
    /// Redirection operators and their operands' operators.
    pub redirect: Style,
    /// `# …`
    pub comment: Style,
    /// Ordinary words.
    pub word: Style,
    /// A quote or substitution that was never closed.
    pub unterminated: Style,
}

impl Theme {
    /// Nothing is styled: the "no colour" theme.
    pub const PLAIN: Theme = Theme {
        keyword: Style::PLAIN,
        operator: Style::PLAIN,
        redirect: Style::PLAIN,
        comment: Style::PLAIN,
        word: Style::PLAIN,
        unterminated: Style::PLAIN,
    };

    /// A readable theme for dark terminals, using only the 16 base colours so
    /// it inherits the user's palette.
    pub const DARK: Theme = Theme {
        keyword: Style {
            color: Color::Magenta,
            bold: true,
            italic: false,
            underline: false,
        },
        operator: Style {
            color: Color::BrightYellow,
            bold: false,
            italic: false,
            underline: false,
        },
        redirect: Style {
            color: Color::BrightCyan,
            bold: false,
            italic: false,
            underline: false,
        },
        comment: Style {
            color: Color::Gray,
            bold: false,
            italic: true,
            underline: false,
        },
        word: Style::PLAIN,
        unterminated: Style {
            color: Color::BrightRed,
            bold: true,
            italic: false,
            underline: true,
        },
    };
}

impl Default for Theme {
    fn default() -> Self {
        Self::DARK
    }
}

/// A styled byte range into the source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Styled {
    /// Byte range.
    pub span: std::ops::Range<usize>,
    /// How to paint it.
    pub style: Style,
}

fn style_for(kind: TokenKind, theme: &Theme) -> Style {
    match kind {
        TokenKind::Whitespace | TokenKind::Newline => Style::PLAIN,
        TokenKind::Comment => theme.comment,
        TokenKind::Keyword => theme.keyword,
        TokenKind::Word => theme.word,
        TokenKind::WordUnterminated => theme.unterminated,
        TokenKind::Operator => theme.operator,
        TokenKind::Redirect => theme.redirect,
    }
}

/// Classify every non-trivia token in `src`.
pub fn highlight(src: &[u8], theme: &Theme) -> Vec<Styled> {
    lex(src)
        .into_iter()
        .filter(|t| !t.kind.is_layout())
        .map(|t| Styled {
            span: t.span,
            style: style_for(t.kind, theme),
        })
        .collect()
}

/// Render `src` with ANSI escapes (or as a plain passthrough when `color` is
/// false). The output is always a byte-exact copy of the input when `color` is
/// false.
pub fn render(src: &[u8], theme: &Theme, color: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() + 32);
    for token in lex(src) {
        let text = token.text(src);
        let sgr = if color {
            style_for(token.kind, theme).sgr()
        } else {
            None
        };
        match sgr {
            Some(prefix) => {
                out.extend_from_slice(prefix.as_bytes());
                out.extend_from_slice(text);
                out.extend_from_slice(RESET);
            }
            None => out.extend_from_slice(text),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_render_is_byte_exact() {
        for src in [
            &b"echo hi"[..],
            &b"if [ -f x ]; then # note\nfi\n"[..],
            &b"\xff\xfe not utf8"[..],
            &b""[..],
        ] {
            assert_eq!(render(src, &Theme::DARK, false), src);
        }
    }

    #[test]
    fn color_render_is_lossless_between_escapes() {
        let src = b"if x; then echo 'y'; fi";
        let painted = render(src, &Theme::DARK, true);
        assert!(painted.starts_with(b"\x1b["));
        let stripped: Vec<u8> = {
            let mut out = Vec::new();
            let mut i = 0;
            while i < painted.len() {
                if painted[i] == 0x1b {
                    while i < painted.len() && painted[i] != b'm' {
                        i += 1;
                    }
                    i += 1;
                } else {
                    out.push(painted[i]);
                    i += 1;
                }
            }
            out
        };
        assert_eq!(stripped, src);
    }

    #[test]
    fn comments_are_classified_as_comments() {
        let src = b"echo hi # there";
        let spans = highlight(src, &Theme::DARK);
        let comment = spans.last().expect("a comment span");
        assert_eq!(&src[comment.span.clone()], b"# there");
        assert_eq!(comment.style, Theme::DARK.comment);
    }

    #[test]
    fn keywords_and_operators_differ() {
        let src = b"if a; then b; fi";
        let spans = highlight(src, &Theme::DARK);
        let keyword = spans
            .iter()
            .find(|s| &src[s.span.clone()] == b"if")
            .unwrap();
        let operator = spans.iter().find(|s| &src[s.span.clone()] == b";").unwrap();
        assert_eq!(keyword.style, Theme::DARK.keyword);
        assert_eq!(operator.style, Theme::DARK.operator);
        assert_ne!(keyword.style, operator.style);
    }

    #[test]
    fn plain_theme_emits_no_escapes() {
        let src = b"if a; then b; fi";
        assert_eq!(render(src, &Theme::PLAIN, true), src);
    }

    #[test]
    fn sgr_encodes_attributes() {
        let style = Style {
            color: Color::Red,
            bold: true,
            italic: false,
            underline: true,
        };
        assert_eq!(style.sgr().as_deref(), Some("\x1b[1;4;31m"));
        assert_eq!(Style::PLAIN.sgr(), None);
    }
}
