//! The editing loop: keys, line, history, completion, rendering.
//!
//! [`Editor`] is the state machine. [`Editor::read_line`] drives it from a
//! [`Terminal`] until a complete logical command arrives; [`Editor::step`]
//! applies one decoded key, which is what the tests use to observe the buffer
//! and the cursor directly.
//!
//! # Continuation
//!
//! When Enter arrives, the editor asks [`Syntax`] whether the buffer is a
//! complete command. `NeedMoreInput` is not an error: the newline joins the
//! buffer, the editor prints it, and editing continues after a `PS2` prompt.
//! The already-submitted lines stay in the buffer because the parser needs
//! them, but they are scrollback now — the editor paints only the live segment
//! (see [`crate::Line::start`]).
//!
//! # Raw mode
//!
//! Raw mode is entered for the duration of `read_line` only. A command started
//! from the prompt must inherit a cooked terminal, and a panic must not leave
//! the user with an unusable shell; the guard in `bsh-sys` restores on drop.

use std::io;

use bsh_complete::Completer;
use bsh_highlight::Theme;

use bsh_runtime::History;

use crate::completion::MenuState;
use crate::key::{Decoder, Key};
use crate::line::Line;
use crate::prompt::Prompt;
use crate::render::{Frame, MenuView, Renderer};
use crate::term::Terminal;

/// What a single key did to the editing session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// Keep editing; the editor has already redrawn.
    Continue,
    /// Enter on a closed menu: the line is a candidate for execution.
    Submit,
    /// `Ctrl-C`: abandon the line.
    Interrupt,
    /// `Ctrl-D` on an empty line, or the terminal's input ended.
    Eof,
}

/// The outcome of a complete [`Editor::read_line`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadResult {
    /// A logical command, newline-terminated and possibly containing newlines.
    Line(Vec<u8>),
    /// The line was abandoned with `Ctrl-C`.
    Interrupted,
    /// End of input (`Ctrl-D` on an empty line, or the terminal closed).
    Eof,
}

/// What the shell's parser says about a buffer being edited.
///
/// `bsh-repl` does not depend on `bsh-syntax` (`docs/contracts.md` keeps the
/// front end out of the editor's dependency set), so the parser enters through
/// this trait. `bsh` implements it with `parse_bytes` and
/// `ParseOutcome::NeedMoreInput`.
pub trait Syntax {
    /// Whether `buffer` is unfinished — an open quote, `if`, here-doc, `$(`.
    fn needs_more_input(&self, buffer: &[u8]) -> bool;
}

/// A [`Syntax`] for which every buffer is complete.
///
/// Useful for a shell without a parser, and for tests that do not care about
/// continuation.
#[derive(Clone, Copy, Debug, Default)]
pub struct NeverNeedsMore;

impl Syntax for NeverNeedsMore {
    fn needs_more_input(&self, _buffer: &[u8]) -> bool {
        false
    }
}

/// Editor configuration.
#[derive(Clone, Debug)]
pub struct Config {
    /// Syntax-highlighting theme.
    pub theme: Theme,
    /// Whether to emit ANSI styling. Set from `color_supported` in `bsh`.
    pub color: bool,
    /// `PS1`/`PS2` templates and the state their escapes read.
    pub prompt: Prompt,
    /// History, including the optional file to append to on exit.
    pub history: History,
    /// Maximum number of rows the completion menu may use.
    pub max_menu_rows: usize,
    /// Whether to paint history ghost text after the cursor.
    pub auto_suggest: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: Theme::DARK,
            color: false,
            prompt: Prompt::default(),
            history: History::new(),
            max_menu_rows: 8,
            auto_suggest: true,
        }
    }
}

/// The interactive line editor.
pub struct Editor<'a> {
    term: &'a mut dyn Terminal,
    completer: &'a dyn Completer,
    syntax: &'a dyn Syntax,
    theme: Theme,
    color: bool,
    prompt: Prompt,
    history: History,
    max_menu_rows: usize,
    auto_suggest: bool,
    line: Line,
    renderer: Renderer,
    decoder: Decoder,
    menu: Option<MenuState>,
    secondary: bool,
    dirty: bool,
}

impl<'a> Editor<'a> {
    /// An editor with the default configuration.
    pub fn new(
        term: &'a mut dyn Terminal,
        completer: &'a dyn Completer,
        syntax: &'a dyn Syntax,
    ) -> Self {
        Self::with_config(term, completer, syntax, Config::default())
    }

    /// An editor with an explicit configuration.
    pub fn with_config(
        term: &'a mut dyn Terminal,
        completer: &'a dyn Completer,
        syntax: &'a dyn Syntax,
        config: Config,
    ) -> Self {
        Self {
            term,
            completer,
            syntax,
            theme: config.theme,
            color: config.color,
            prompt: config.prompt,
            history: config.history,
            max_menu_rows: config.max_menu_rows.max(1),
            auto_suggest: config.auto_suggest,
            line: Line::new(),
            renderer: Renderer::new(),
            decoder: Decoder::new(),
            menu: None,
            secondary: false,
            dirty: true,
        }
    }

    /// The line being edited (or the last one, after a submit).
    pub fn line(&self) -> &Line {
        &self.line
    }

    /// The history, for navigation tests and for the shell to seed.
    pub fn history(&self) -> &History {
        &self.history
    }

    /// Mutable history, so the shell can load or replace it.
    pub fn history_mut(&mut self) -> &mut History {
        &mut self.history
    }

    /// The prompts the editor paints.
    pub fn prompt(&self) -> &Prompt {
        &self.prompt
    }

    /// Replace the rendered prompts.
    ///
    /// The shell calls this before each line: `PS1` may have changed, the
    /// working directory may have changed, `\!` must name the line about to be
    /// typed, and `promptvars` may have run commands. The editor cannot derive
    /// any of that itself.
    pub fn set_prompt(&mut self, prompt: Prompt) {
        self.prompt = prompt;
        self.dirty = true;
    }

    /// Append this session's history to `HISTFILE`, if there is one.
    pub fn finish(&mut self) -> io::Result<()> {
        self.history.save()
    }

    /// Read one complete logical command.
    ///
    /// Raw mode is held only for the duration of this call.
    pub fn read_line(&mut self) -> io::Result<ReadResult> {
        self.begin();
        self.term.enter_raw()?;
        let result = self.read_line_loop();
        let _ = self.term.leave_raw();
        result
    }

    /// Apply one decoded key; returns what the caller should do next.
    ///
    /// The editor redraws for [`Flow::Continue`]. This is public so a caller
    /// with its own event loop (and the tests) can drive the editor key by key
    /// without going through [`Terminal::read_byte`].
    pub fn step(&mut self, key: Key) -> io::Result<Flow> {
        let flow = self.handle(key)?;
        if flow == Flow::Continue {
            self.draw()?;
        }
        Ok(flow)
    }

    /// Reset per-line state and paint the first prompt.
    fn begin(&mut self) {
        self.line.reset();
        self.menu = None;
        self.history.reset_navigation();
        self.secondary = false;
        self.renderer.reset();
        self.dirty = true;
    }

    fn read_line_loop(&mut self) -> io::Result<ReadResult> {
        self.draw()?;
        loop {
            let Some(key) = self.decoder.next(self.term)? else {
                return self.finish_eof();
            };
            match self.step(key)? {
                Flow::Continue => {}
                Flow::Submit => {
                    self.line.push_newline();
                    if self.syntax.needs_more_input(self.line.buffer()) {
                        self.renderer.goto_end(self.term)?;
                        self.term.write_all(b"\r\n")?;
                        self.renderer.reset();
                        self.line.commit_prefix();
                        self.secondary = true;
                        self.menu = None;
                        self.dirty = true;
                        self.draw()?;
                        continue;
                    }
                    let text = self.line.take();
                    self.history.push(&text);
                    self.renderer.goto_end(self.term)?;
                    self.term.write_all(b"\r\n")?;
                    self.renderer.reset();
                    self.secondary = false;
                    return Ok(ReadResult::Line(text));
                }
                Flow::Interrupt => return self.finish_interrupt(),
                Flow::Eof => return self.finish_eof(),
            }
        }
    }

    fn finish_interrupt(&mut self) -> io::Result<ReadResult> {
        // An open menu has rows below the line that must be erased before the
        // `^C` marker is written; without one the last frame is already right.
        if self.menu.take().is_some() {
            self.dirty = true;
            self.draw()?;
        }
        self.renderer.goto_end(self.term)?;
        self.term.write_all(b"^C\r\n")?;
        self.renderer.reset();
        self.line.reset();
        self.secondary = false;
        Ok(ReadResult::Interrupted)
    }

    fn finish_eof(&mut self) -> io::Result<ReadResult> {
        if self.menu.take().is_some() {
            self.dirty = true;
            self.draw()?;
        }
        self.renderer.goto_end(self.term)?;
        self.term.write_all(b"\r\n")?;
        self.renderer.reset();
        self.line.reset();
        self.secondary = false;
        Ok(ReadResult::Eof)
    }

    fn handle(&mut self, key: Key) -> io::Result<Flow> {
        if self.menu.is_some() {
            return self.handle_menu(key);
        }
        if !is_kill(&key) {
            self.line.kill_reset();
        }
        self.dirty = true;
        match key {
            Key::Char(character) => self.line.insert_char(character),
            Key::Byte(byte) => self.line.insert(&[byte]),
            Key::Paste(bytes) => self.line.insert(&bytes),
            Key::Enter => return Ok(Flow::Submit),
            Key::Tab => self.complete()?,
            Key::BackTab | Key::Esc => {}
            Key::Backspace => {
                self.line.backspace();
            }
            Key::Delete => {
                self.line.delete();
            }
            Key::Left => {
                self.line.move_left();
            }
            Key::Right => {
                if !self.line.move_right() {
                    self.accept_suggestion();
                }
            }
            Key::Home => {
                self.line.move_home();
            }
            Key::End => {
                if !self.line.move_end() {
                    // Already at the end: End accepts history ghost text, the
                    // same as Right does.
                    self.accept_suggestion();
                }
            }
            Key::Up | Key::Ctrl(0x10) => self.history_previous(),
            Key::Down | Key::Ctrl(0x0e) => self.history_next(),
            Key::Ctrl(0x01) => {
                self.line.move_home();
            }
            Key::Ctrl(0x02) => {
                self.line.move_left();
            }
            Key::Ctrl(0x03) => return Ok(Flow::Interrupt),
            Key::Ctrl(0x04) => {
                if self.line.buffer().is_empty() && !self.line.in_continuation() {
                    return Ok(Flow::Eof);
                }
                if self.line.in_continuation() && self.line.editable().is_empty() {
                    // EOF at an empty `PS2` line: abandon the whole command.
                    // bash reports a syntax error instead, which needs an
                    // "incomplete, report it" answer the `Syntax` trait does
                    // not have yet.
                    return Ok(Flow::Interrupt);
                }
                self.line.delete();
            }
            Key::Ctrl(0x05) => {
                self.line.move_end();
            }
            Key::Ctrl(0x06) => {
                self.line.move_right();
            }
            Key::Ctrl(0x0b) => {
                self.line.kill_to_end();
            }
            Key::Ctrl(0x0c) => {
                self.term.write_all(b"\x1b[H\x1b[2J")?;
                self.renderer.reset();
            }
            Key::Ctrl(0x15) => {
                self.line.kill_to_start();
            }
            Key::Ctrl(0x17) => {
                self.line.kill_word();
            }
            Key::Ctrl(0x19) => {
                self.line.yank();
            }
            Key::Ctrl(0x14) => {
                self.line.transpose();
            }
            Key::Alt('b') => {
                self.line.move_word_left();
            }
            Key::Alt('f') => {
                self.line.move_word_right();
            }
            _ => self.dirty = false,
        }
        Ok(Flow::Continue)
    }

    fn handle_menu(&mut self, key: Key) -> io::Result<Flow> {
        match key {
            Key::Up | Key::BackTab | Key::Ctrl(0x10) => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.previous();
                }
                self.dirty = true;
                Ok(Flow::Continue)
            }
            Key::Down | Key::Ctrl(0x0e) => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.next();
                }
                self.dirty = true;
                Ok(Flow::Continue)
            }
            Key::Tab | Key::Enter => {
                self.accept_selection();
                Ok(Flow::Continue)
            }
            Key::Esc => {
                self.menu = None;
                self.dirty = true;
                Ok(Flow::Continue)
            }
            other => {
                self.menu = None;
                self.dirty = true;
                self.handle(other)
            }
        }
    }

    fn accept_selection(&mut self) {
        let Some(menu) = self.menu.take() else {
            return;
        };
        let index = menu.selected();
        if self.line.apply_menu(&menu.menu, index) {
            self.dirty = true;
        } else {
            // The selection cannot be inserted here (a source candidate inside
            // single quotes); keep the menu so the user can pick another.
            self.menu = Some(menu);
        }
    }

    fn complete(&mut self) -> io::Result<()> {
        let menu = self
            .completer
            .complete(self.line.buffer(), self.line.cursor());
        if menu.is_empty() {
            // No candidates: a bell, not a redraw that changes nothing.
            self.term.write_all(b"\x07")?;
            self.dirty = false;
            return Ok(());
        }
        if menu.candidates.len() == 1 {
            self.line.apply_menu(&menu, 0);
            return Ok(());
        }
        self.menu = Some(MenuState::new(menu));
        Ok(())
    }

    fn history_previous(&mut self) {
        if self.line.in_continuation() {
            // Earlier lines are already scrollback; recalling an entry would
            // leave them on screen above an unrelated line.
            return;
        }
        let current = self.line.buffer().to_vec();
        if let Some(entry) = self.history.older(&current) {
            self.line.set_text(&entry);
        }
    }

    fn history_next(&mut self) {
        if self.line.in_continuation() {
            return;
        }
        if let Some(entry) = self.history.newer() {
            self.line.set_text(&entry);
        }
    }

    fn accept_suggestion(&mut self) {
        // `suggestion` already checks that the ghost text is visible (colour,
        // auto-suggest, cursor at the end); accepting invisible text would be
        // a surprise.
        let rest = self.suggestion();
        if let Some(rest) = rest {
            self.line.insert(&rest);
        }
    }

    /// The remainder of the newest history entry continuing the buffer.
    fn suggestion(&self) -> Option<Vec<u8>> {
        if !self.auto_suggest
            || !self.color
            || self.line.in_continuation()
            || self.line.cursor() != self.line.buffer().len()
            || self.line.buffer().is_empty()
        {
            return None;
        }
        self.history
            .suggestion(self.line.buffer())
            .map(<[u8]>::to_vec)
    }

    fn draw(&mut self) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        self.dirty = false;
        let prompt = if self.secondary {
            self.prompt.secondary_text()
        } else {
            self.prompt.primary_text()
        };
        let columns = self.term.size().columns_or_fallback();
        let rows = self
            .menu
            .as_mut()
            .map(|menu| menu.rows(self.max_menu_rows, columns));
        let menu = rows.as_deref().map(|rows| MenuView { rows });
        let ghost = if self.menu.is_some() {
            None
        } else {
            self.suggestion()
        };
        let frame = Frame {
            prompt,
            line: self.line.editable(),
            cursor: self.line.editable_cursor(),
            menu,
            ghost: ghost.as_deref(),
        };
        self.renderer
            .draw(self.term, &frame, &self.theme, self.color)
    }
}

/// Whether `key` extends a kill sequence rather than ending one.
fn is_kill(key: &Key) -> bool {
    matches!(key, Key::Ctrl(0x0b) | Key::Ctrl(0x15) | Key::Ctrl(0x17))
}
