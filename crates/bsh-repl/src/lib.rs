//! Line editor and terminal rendering.
//!
//! Not implemented beyond rendering: [`paint`] is the whole surface so far.
//! The editor is deliberately independent of the interpreter, talking to it
//! through traits, so it can be driven headlessly in tests (DESIGN.md §4.1).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub use bsh_highlight::Theme;

/// Paint `line` for display.
///
/// Returns bytes rather than `str` so that undecodable filenames and editors
/// containing arbitrary bytes survive the round trip.
pub fn paint(line: &[u8], theme: &Theme, color: bool) -> Vec<u8> {
    bsh_highlight::render(line, theme, color)
}

/// Whether colour should be emitted on `fd`: a terminal, and `NO_COLOR` unset.
pub fn color_supported(fd: i32) -> bool {
    std::env::var_os("NO_COLOR").is_none() && bsh_sys::is_tty(fd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_paint_is_byte_exact() {
        let line = b"echo 'hi'  # note";
        assert_eq!(paint(line, &Theme::DARK, false), line);
    }
}
