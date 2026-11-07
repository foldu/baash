//! Frames: prompt, line, ghost text and the completion menu, as terminal bytes.
//!
//! The editor repaints the whole edit region on every keystroke. "The whole
//! region" is everything from the start of the prompt down: moving the cursor
//! back to the first row, erasing to the end of the screen, and drawing again.
//! That is simpler than tracking which cells changed and it is what makes
//! wrapped lines, a shrinking menu and syntax colours all correct at once.
//!
//! Wrapping is done here rather than left to the terminal's automatic wrap:
//! the editor decides where a row ends, so the cursor's row and column are
//! known exactly instead of depending on a terminal's deferred-wrap quirk.
//!
//! The renderer remembers where it left the cursor relative to the start of the
//! prompt. That relative distance survives the terminal scrolling, which is why
//! the next repaint can get back to the top of the region without knowing an
//! absolute screen coordinate.
//!
//! That only holds while a frame fits on the screen: a frame taller than the
//! terminal scrolls its own top row out of reach, and the next repaint would
//! start below the frame and paint a second prompt under it. So the frame's
//! height is measured ([`Frame::height`]) before anything is painted, and the
//! parts that grow — the completion menu, the ghost text — are given only the
//! rows that are left.

use std::io;
use std::ops::Range;

use baash_highlight::{Style, Styled, Theme};
use baash_lang::prompt::ExpandedPrompt;
use unicode_width::UnicodeWidthChar;

use crate::display::Expanded;
use crate::term::{Size, Terminal};

/// SGR reset.
const RESET: &[u8] = b"\x1b[0m";

/// One row of the completion menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuRow {
    /// Plain text to paint, already flattened and prefixed with a selection
    /// marker. It may be truncated to the terminal width.
    pub text: String,
    /// Whether this row contains the current selection.
    pub selected: bool,
    /// The selected cell's byte range within [`MenuRow::text`].
    ///
    /// A columnar row holds several candidates, so reversing the whole row
    /// would paint its neighbours as selected. The range is only painted
    /// inverted when colour is on; without colour the `> ` marker is the
    /// selection.
    pub selection: Option<Range<usize>>,
}

/// The menu part of a frame.
#[derive(Clone, Copy, Debug)]
pub struct MenuView<'a> {
    /// Rows to paint, top to bottom. The editor has already scrolled the list
    /// to fit.
    pub rows: &'a [MenuRow],
}

/// Everything one repaint needs.
#[derive(Clone, Copy, Debug)]
pub struct Frame<'a> {
    /// The `PS1` or `PS2` text, already expanded, with the stretches that
    /// were between `\[` and `\]` marked so they do not count toward the
    /// display width.
    pub prompt: &'a ExpandedPrompt,
    /// The editable slice of the line.
    pub line: &'a [u8],
    /// The cursor as a byte offset into [`Frame::line`].
    pub cursor: usize,
    /// The menu, when one is open.
    pub menu: Option<MenuView<'a>>,
    /// Ghost-text continuation to paint after the line, when there is one.
    pub ghost: Option<&'a [u8]>,
}

impl Frame<'_> {
    /// The number of terminal rows this frame occupies at `columns`.
    ///
    /// Measured through the painting functions themselves, so the answer and
    /// what [`Renderer::draw`] paints cannot disagree about wrapping. The menu
    /// is not included: it is the caller's job to fit it into
    /// `TERM_ROWS - height`.
    pub fn height(&self, columns: u16, color: bool) -> u16 {
        let mut canvas = Canvas::measuring(columns);
        paint_prompt(&mut canvas, self.prompt);
        let _ = paint_line(&mut canvas, self.line, self.cursor, &Theme::DARK, false);
        if let Some(ghost) = self.ghost {
            paint_ghost(&mut canvas, ghost, color);
        }
        canvas.position().0.saturating_add(1)
    }
}

/// The painting state that survives between frames.
#[derive(Clone, Debug)]
pub struct Renderer {
    /// The cursor row relative to the start of the edit region.
    cursor_row: u16,
    /// The last row of the frame relative to the start of the edit region.
    end_row: u16,
    /// The column of the last cell of the frame.
    end_col: u16,
    /// Columns used for the last frame.
    columns: u16,
    /// Whether the cursor is known to be at the start of a fresh region.
    fresh: bool,
}

impl Renderer {
    /// A renderer that will draw its first frame without moving up.
    pub fn new() -> Self {
        Self {
            cursor_row: 0,
            end_row: 0,
            end_col: 0,
            columns: Size::FALLBACK.columns,
            fresh: true,
        }
    }

    /// Forget the current layout: the next frame is treated as the first one.
    ///
    /// Used after the editor prints a newline (command submitted) and after
    /// `Ctrl-L` cleared the screen.
    pub fn reset(&mut self) {
        self.cursor_row = 0;
        self.end_row = 0;
        self.end_col = 0;
        self.fresh = true;
    }

    /// Paint one frame and leave the cursor where the user is typing.
    pub fn draw(
        &mut self,
        term: &mut dyn Terminal,
        frame: &Frame<'_>,
        theme: &Theme,
        color: bool,
    ) -> io::Result<()> {
        let columns = term.size().columns_or_fallback().max(1);
        if columns != self.columns {
            // A resize invalidates the old row arithmetic; start a fresh frame.
            self.fresh = true;
        }
        self.columns = columns;
        let mut canvas = Canvas::new(columns);
        if self.fresh {
            canvas.cr();
        } else {
            canvas.up(self.cursor_row);
            canvas.cr();
        }
        canvas.clear_below();

        paint_prompt(&mut canvas, frame.prompt);
        let cursor_position = paint_line(&mut canvas, frame.line, frame.cursor, theme, color);
        if let Some(ghost) = frame.ghost {
            paint_ghost(&mut canvas, ghost, color);
        }
        if let Some(menu) = frame.menu {
            for row in menu.rows {
                canvas.newline();
                paint_menu_row(&mut canvas, row, color);
            }
        }

        // The canvas ends on the last painted cell — the menu's bottom row when
        // one is open — and the cursor belongs back on the line, above it. The
        // move is always *up*, never down: painting below the cursor and then
        // moving further down would scroll the frame off its own top row, and
        // the next frame would repaint the prompt under the stale menu.
        let end = canvas.position();
        canvas.up(end.0.saturating_sub(cursor_position.0));
        if end.0 < cursor_position.0 {
            canvas.down(cursor_position.0 - end.0);
        }
        canvas.cr();
        canvas.right(cursor_position.1);

        self.cursor_row = cursor_position.0;
        self.end_row = end.0;
        self.end_col = end.1;
        self.fresh = false;
        term.write_all(&canvas.out)
    }

    /// Erase the painted frame and leave the cursor where it began.
    ///
    /// A completion function's output belongs where the frame is, so the frame
    /// is removed first and repainted after the output; without the erase, the
    /// output would start at the cursor, in the middle of the line.
    pub fn clear_frame(&mut self, term: &mut dyn Terminal) -> io::Result<()> {
        let mut canvas = Canvas::new(self.columns);
        if !self.fresh {
            canvas.up(self.cursor_row);
        }
        canvas.cr();
        canvas.clear_below();
        self.reset();
        term.write_all(&canvas.out)
    }

    /// Move the cursor to the last cell the previous frame painted.
    ///
    /// Submitting a line, `Ctrl-C` and `Ctrl-D` all want to continue from the
    /// end of the painted region rather than from the cursor, so the newline
    /// they write lands after the text instead of in the middle of it.
    pub fn goto_end(&mut self, term: &mut dyn Terminal) -> io::Result<()> {
        let mut canvas = Canvas::new(self.columns);
        if self.end_row >= self.cursor_row {
            canvas.down(self.end_row - self.cursor_row);
        } else {
            canvas.up(self.cursor_row - self.end_row);
        }
        canvas.cr();
        canvas.right(self.end_col);
        self.cursor_row = self.end_row;
        term.write_all(&canvas.out)
    }
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

/// The bytes being assembled for one frame.
struct Canvas {
    columns: u16,
    out: Vec<u8>,
    row: u16,
    col: u16,
    /// Track the position but throw the bytes away, for measuring a frame.
    measure: bool,
}

impl Canvas {
    fn new(columns: u16) -> Self {
        Self {
            columns: columns.max(1),
            out: Vec::with_capacity(256),
            row: 0,
            col: 0,
            measure: false,
        }
    }

    /// A canvas that only tracks where the cursor would end up.
    fn measuring(columns: u16) -> Self {
        Self {
            measure: true,
            ..Self::new(columns)
        }
    }

    fn position(&self) -> (u16, u16) {
        (self.row, self.col)
    }

    fn raw(&mut self, bytes: &[u8]) {
        if !self.measure {
            self.out.extend_from_slice(bytes);
        }
    }

    fn cr(&mut self) {
        self.out.push(b'\r');
        self.col = 0;
    }

    fn up(&mut self, count: u16) {
        if count > 0 {
            self.out
                .extend_from_slice(format!("\x1b[{count}A").as_bytes());
            self.row = self.row.saturating_sub(count);
        }
    }

    fn down(&mut self, count: u16) {
        if count > 0 {
            self.out
                .extend_from_slice(format!("\x1b[{count}B").as_bytes());
            self.row = self.row.saturating_add(count);
        }
    }

    fn right(&mut self, count: u16) {
        if count > 0 {
            self.out
                .extend_from_slice(format!("\x1b[{count}C").as_bytes());
            self.col = self.col.saturating_add(count).min(self.columns);
        }
    }

    fn clear_below(&mut self) {
        self.raw(b"\x1b[J");
    }

    fn newline(&mut self) {
        self.raw(b"\r\n");
        self.row = self.row.saturating_add(1);
        self.col = 0;
    }

    /// Write one display unit, wrapping before it when it would not fit.
    ///
    /// A newline is a row break rather than a cell: the editor emits `\r\n`
    /// itself, so the cursor lands where it thinks it does. That is what makes
    /// a recalled history entry that spans lines paint as those lines instead of
    /// as literal `^J`.
    fn cell(&mut self, bytes: &[u8], width: usize) {
        if bytes == b"\n" {
            self.newline();
            return;
        }
        if width > 0 && self.col > 0 && usize::from(self.col) + width > usize::from(self.columns) {
            self.newline();
        }
        self.raw(bytes);
        if width > 0 {
            self.col = (usize::from(self.col) + width).min(usize::from(self.columns)) as u16;
        }
    }

    /// Write menu text, clipped so it can never wrap onto a second row.
    ///
    /// The column starts at the current position, not at zero: one row may hold
    /// several candidates side by side, with only the selected cell inverted.
    fn row_text(&mut self, text: &str) {
        let mut width = usize::from(self.col);
        for character in text.chars() {
            let cells = UnicodeWidthChar::width(character).unwrap_or(0);
            if width + cells > usize::from(self.columns) {
                break;
            }
            let mut buffer = [0u8; 4];
            let encoded = character.encode_utf8(&mut buffer);
            self.raw(encoded.as_bytes());
            width += cells;
        }
        self.col = width.min(usize::from(self.columns)) as u16;
    }
}

/// Paint one menu row, inverting only the selected cell.
///
/// Without colour there is nothing to invert, and the `> ` marker alone says
/// which cell is selected. Byte ranges are split on character boundaries built
/// from `char_indices`, so slicing a multi-byte candidate cannot panic.
fn paint_menu_row(canvas: &mut Canvas, row: &MenuRow, color: bool) {
    let Some(range) = row.selection.as_ref().filter(|_| color) else {
        canvas.row_text(&row.text);
        return;
    };
    let start = floor_boundary(&row.text, range.start);
    let end = floor_boundary(&row.text, range.end).max(start);
    canvas.row_text(&row.text[..start]);
    canvas.raw(b"\x1b[7m");
    canvas.row_text(&row.text[start..end]);
    canvas.raw(RESET);
    canvas.row_text(&row.text[end..]);
}

/// The greatest character boundary at or below `at`.
fn floor_boundary(text: &str, at: usize) -> usize {
    if at >= text.len() {
        return text.len();
    }
    let mut boundary = 0;
    for (index, _) in text.char_indices() {
        if index > at {
            break;
        }
        boundary = index;
    }
    boundary
}

/// Paint a prompt: non-printing stretches are written without advancing the
/// column, and visible text is measured.
///
/// `\[` and `\]` already delimit the hidden stretches, which is the reliable
/// signal. ANSI CSI sequences are also copied through without counting them:
/// a user who colours a prompt without the markers should not have every later
/// column offset.
fn paint_prompt(canvas: &mut Canvas, prompt: &ExpandedPrompt) {
    let bytes = prompt.bytes();
    let mut at = 0;
    for hidden in prompt.non_printing() {
        if hidden.start > at {
            paint_visible(canvas, &bytes[at..hidden.start]);
        }
        canvas.raw(&bytes[hidden.clone()]);
        at = hidden.end.max(at);
    }
    if at < bytes.len() {
        paint_visible(canvas, &bytes[at..]);
    }
}

/// Paint visible prompt text, splitting out ANSI CSI sequences.
fn paint_visible(canvas: &mut Canvas, prompt: &[u8]) {
    let mut text_start = 0;
    let mut at = 0;
    while at < prompt.len() {
        if prompt[at] != 0x1b {
            at += 1;
            continue;
        }
        paint_prompt_text(canvas, &prompt[text_start..at]);
        let sequence_start = at;
        at += 1;
        if prompt.get(at) == Some(&b'[') {
            at += 1;
            while at < prompt.len() && !(0x40..=0x7e).contains(&prompt[at]) {
                at += 1;
            }
            if at < prompt.len() {
                at += 1;
            }
        }
        canvas.raw(&prompt[sequence_start..at]);
        text_start = at;
    }
    paint_prompt_text(canvas, &prompt[text_start..]);
}

/// Paint prompt text, where a newline is a real line break.
fn paint_prompt_text(canvas: &mut Canvas, text: &[u8]) {
    let mut first = true;
    for part in text.split(|&byte| byte == b'\n') {
        if !first {
            canvas.newline();
        }
        first = false;
        let expanded = Expanded::new(part);
        for segment in expanded.segments() {
            canvas.cell(&expanded.bytes()[segment.display.clone()], segment.width);
        }
    }
}

/// Paint the line with syntax highlighting; returns the cursor's position.
fn paint_line(
    canvas: &mut Canvas,
    line: &[u8],
    cursor: usize,
    theme: &Theme,
    color: bool,
) -> (u16, u16) {
    let expanded = Expanded::new(line);
    let spans: Vec<Styled> = if color {
        baash_highlight::highlight(line, theme)
    } else {
        Vec::new()
    };
    let mut cursor_position = None;
    let mut span_index = 0;
    let mut style = Style::PLAIN;
    for segment in expanded.segments() {
        if cursor_position.is_none() && cursor <= segment.src.start {
            cursor_position = Some(canvas.position());
        }
        let next_style = style_at(&spans, &mut span_index, segment.src.start);
        if next_style != style {
            canvas.raw(RESET);
            if let Some(sgr) = next_style.sgr() {
                canvas.raw(sgr.as_bytes());
            }
            style = next_style;
        }
        canvas.cell(&expanded.bytes()[segment.display.clone()], segment.width);
    }
    if style != Style::PLAIN {
        canvas.raw(RESET);
    }
    cursor_position.unwrap_or_else(|| canvas.position())
}

/// The style covering source offset `position`, advancing `index`.
fn style_at(spans: &[Styled], index: &mut usize, position: usize) -> Style {
    while *index < spans.len() && spans[*index].span.end <= position {
        *index += 1;
    }
    match spans.get(*index) {
        Some(span) if span.span.start <= position && position < span.span.end => span.style,
        _ => Style::PLAIN,
    }
}

/// Paint history ghost text dimly. Without colour it would be
/// indistinguishable from text the user typed, so it is not painted at all.
///
/// `2` is the spec's own way to say this, which leaves how dim to the palette.
/// A terminal that draws faint as plain text is a terminal setting to fix, not
/// a reason to hard-code a grey over every palette that gets it right.
fn paint_ghost(canvas: &mut Canvas, ghost: &[u8], color: bool) {
    if !color {
        return;
    }
    canvas.raw(b"\x1b[2m");
    let expanded = Expanded::new(ghost);
    for segment in expanded.segments() {
        canvas.cell(&expanded.bytes()[segment.display.clone()], segment.width);
    }
    canvas.raw(b"\x1b[22m");
}
