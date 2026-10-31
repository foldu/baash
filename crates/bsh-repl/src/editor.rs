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
//! # Reverse incremental search
//!
//! `Ctrl-R` (`Ctrl-R` again for older matches) searches the ring the editor
//! already holds, with readline's interaction: the search *is* the prompt, the
//! line being painted is the match, and `Enter` runs it. Nothing here is a new
//! screen mode — [`Search`] only swaps the prompt and the buffer, so the
//! renderer, the wrapping and the cursor arithmetic are the ones already used
//! for typing.
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
use crate::prompt::{ExpandedPrompt, Prompt};
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
    /// History: the session ring, with the database behind it.
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

/// `Ctrl-R` reverse incremental search.
///
/// The query is what has been typed since the search began; the line itself
/// holds the current match, so the renderer paints the match with the normal
/// line code. `original` is what `Esc`/`Ctrl-G` put back.
#[derive(Clone, Debug)]
struct Search {
    /// Bytes typed into the search.
    query: Vec<u8>,
    /// The line as it was when `Ctrl-R` was pressed.
    original: Vec<u8>,
    /// The cursor as it was when `Ctrl-R` was pressed.
    original_cursor: usize,
    /// Index into the history entries of the match being shown.
    matched: Option<usize>,
    /// Whether the last search failed. The line keeps the last match, as
    /// readline does, while the prompt says the search failed.
    failed: bool,
}

impl Search {
    /// The prompt readline draws in place of `PS1`.
    fn prompt(&self) -> ExpandedPrompt {
        let prefix: &[u8] = if self.failed {
            b"(failed reverse-i-search)`"
        } else {
            b"(reverse-i-search)`"
        };
        let mut bytes = Vec::with_capacity(prefix.len() + self.query.len() + 3);
        bytes.extend_from_slice(prefix);
        bytes.extend_from_slice(&self.query);
        bytes.extend_from_slice(b"': ");
        ExpandedPrompt::from_bytes(bytes)
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
    /// A line to put in the buffer for the next read, used by `histverify`
    /// after history expansion.
    initial: Option<Vec<u8>>,
    /// The reverse search in progress, if any.
    search: Option<Search>,
    /// The last `Tab` inserted the candidates' common prefix, so the next one
    /// is what opens the menu. Any other key clears it.
    prefix_tab: bool,
    /// `Esc` dismissed the history suggestion for the current line. Typing
    /// clears it, so refining the line brings the suggestion back.
    suggestion_dismissed: bool,
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
            initial: None,
            search: None,
            prefix_tab: false,
            suggestion_dismissed: false,
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

    /// Put `bytes` on the editing line of the next [`Editor::read_line`].
    ///
    /// `histverify` uses this to show an expanded command for review instead
    /// of running it: the next prompt paints the line as if the user had typed
    /// it, and Enter submits it normally.
    pub fn set_initial_line(&mut self, bytes: &[u8]) {
        self.initial = Some(bytes.to_vec());
        self.dirty = true;
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

    /// Wait for the history writes this session queued to reach the database.
    pub fn finish(&mut self) {
        self.history.flush();
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
        if let Some(initial) = self.initial.take() {
            self.line.set_text(&initial);
        }
        self.menu = None;
        self.search = None;
        self.prefix_tab = false;
        self.suggestion_dismissed = false;
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
        if self.search.is_some() {
            return self.handle_search(key);
        }
        if self.menu.is_some() {
            return self.handle_menu(key);
        }
        if !is_kill(&key) {
            self.line.kill_reset();
        }
        if key != Key::Tab {
            // The second `Tab` is only special immediately after a prefix
            // completion; any other key starts a new completion sequence.
            self.prefix_tab = false;
        }
        if edits_line(&key) {
            // Editing the line refines the suggestion; a movement key does
            // not, so it must not resurrect one Escape dismissed.
            self.suggestion_dismissed = false;
        }
        self.dirty = true;
        match key {
            Key::Char(character) => {
                self.line.insert_char(character);
            }
            Key::Byte(byte) => {
                self.line.insert(&[byte]);
            }
            Key::Paste(bytes) => {
                self.line.insert(&bytes);
            }
            Key::Enter => return Ok(Flow::Submit),
            Key::Tab => self.complete()?,
            Key::Esc => {
                // Dismiss the ghost text for this line. Typing refines the
                // line and brings it back.
                self.suggestion_dismissed = true;
            }
            Key::BackTab => {}
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
            Key::Ctrl(0x12) => self.begin_search(),
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
            // Tab and Shift-Tab cycle the menu (fish's pager behaviour), and
            // Enter inserts the selection. Up/Down and Ctrl-P/Ctrl-N are the
            // readline spellings of the same movement.
            Key::Tab | Key::Down | Key::Ctrl(0x0e) => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.next();
                }
                self.dirty = true;
                Ok(Flow::Continue)
            }
            Key::BackTab | Key::Up | Key::Ctrl(0x10) => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.previous();
                }
                self.dirty = true;
                Ok(Flow::Continue)
            }
            Key::Enter => {
                self.accept_selection();
                Ok(Flow::Continue)
            }
            Key::Esc => {
                self.menu = None;
                self.prefix_tab = false;
                self.dirty = true;
                Ok(Flow::Continue)
            }
            other => {
                self.menu = None;
                self.prefix_tab = false;
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
            self.prefix_tab = false;
            self.term.write_all(b"\x07")?;
            self.dirty = false;
            return Ok(());
        }
        if menu.candidates.len() == 1 {
            self.prefix_tab = false;
            self.line.apply_menu(&menu, 0);
            return Ok(());
        }
        if self.prefix_tab {
            // The first Tab already inserted everything unambiguous; the
            // second is the request to see the choices.
            self.prefix_tab = false;
            self.menu = Some(MenuState::new(menu));
            return Ok(());
        }
        if self.line.apply_menu_prefix(&menu) {
            // Fish's first Tab: extend the word to the common prefix and do
            // not paint a menu. The next Tab opens it even when there is no
            // further prefix to add, which is why `prefix_tab` exists.
            self.prefix_tab = true;
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

    /// `Ctrl-R`: start a reverse search over the ring.
    ///
    /// The current line and cursor are remembered for `Esc`/`Ctrl-G`, and the
    /// newest entry is shown at once (readline does the same, so a second
    /// `Ctrl-R` walks back one entry at a time). A continuation line never
    /// enters the search: earlier lines are already scrollback.
    fn begin_search(&mut self) {
        if self.line.in_continuation() {
            return;
        }
        let mut search = Search {
            query: Vec::new(),
            original: self.line.buffer().to_vec(),
            original_cursor: self.line.cursor(),
            matched: None,
            failed: true,
        };
        if self.history.is_empty() {
            self.search = Some(search);
            self.dirty = true;
            return;
        }
        let newest = self.history.len() - 1;
        search.matched = Some(newest);
        search.failed = false;
        self.search = Some(search);
        self.show_match(newest);
    }

    /// One key while the search prompt is up.
    ///
    /// The keys readline gives a meaning are handled here; every other key
    /// accepts the match (the line already holds it) and is then processed
    /// normally — which is how `Left`/`Right` put the cursor on the match for
    /// editing.
    fn handle_search(&mut self, key: Key) -> io::Result<Flow> {
        match key {
            Key::Ctrl(0x12) => {
                self.search_older();
                Ok(Flow::Continue)
            }
            Key::Ctrl(0x07) | Key::Esc => {
                self.abort_search();
                Ok(Flow::Continue)
            }
            Key::Ctrl(0x03) => {
                // The whole line is abandoned; there is nothing to restore.
                self.search = None;
                Ok(Flow::Interrupt)
            }
            Key::Enter => {
                self.search = None;
                Ok(Flow::Submit)
            }
            Key::Backspace => {
                if let Some(search) = self.search.as_mut()
                    && search.query.pop().is_some()
                {
                    self.research();
                }
                Ok(Flow::Continue)
            }
            Key::Char(character) => {
                self.search_insert(character.to_string().as_bytes());
                Ok(Flow::Continue)
            }
            Key::Byte(byte) => {
                self.search_insert(&[byte]);
                Ok(Flow::Continue)
            }
            other => {
                // Accept and let the key act on the accepted line.
                self.search = None;
                self.dirty = true;
                self.handle(other)
            }
        }
    }

    /// Add bytes to the query and search again from where we are.
    fn search_insert(&mut self, bytes: &[u8]) {
        if let Some(search) = self.search.as_mut() {
            search.query.extend_from_slice(bytes);
        }
        self.research();
    }

    /// Re-run the search after a query edit.
    ///
    /// A failed search restarts from the newest entry (otherwise backspacing
    /// out of a dead end would keep looking where it failed); a live match is
    /// kept when it still contains the new query and the search walks older
    /// from there, which is readline's narrowing.
    fn research(&mut self) {
        let Some(search) = self.search.as_ref() else {
            return;
        };
        let query = search.query.clone();
        let from = if search.failed {
            self.history.len().saturating_sub(1)
        } else {
            match search.matched {
                Some(index) => index,
                None => self.history.len().saturating_sub(1),
            }
        };
        if self.history.is_empty() {
            if let Some(search) = self.search.as_mut() {
                search.failed = true;
                search.matched = None;
            }
            self.dirty = true;
            return;
        }
        match self.find_match(&query, from) {
            Some(index) => {
                if let Some(search) = self.search.as_mut() {
                    search.failed = false;
                    search.matched = Some(index);
                }
                self.show_match(index);
            }
            None => {
                if let Some(search) = self.search.as_mut() {
                    search.failed = true;
                    search.matched = None;
                }
                self.dirty = true;
            }
        }
    }

    /// `Ctrl-R` again: the next older match.
    fn search_older(&mut self) {
        let Some(search) = self.search.as_ref() else {
            return;
        };
        let query = search.query.clone();
        let from = match search.matched {
            Some(0) | None => {
                if let Some(search) = self.search.as_mut() {
                    search.failed = true;
                }
                self.dirty = true;
                return;
            }
            Some(index) => index - 1,
        };
        match self.find_match(&query, from) {
            Some(index) => {
                if let Some(search) = self.search.as_mut() {
                    search.failed = false;
                    search.matched = Some(index);
                }
                self.show_match(index);
            }
            None => {
                if let Some(search) = self.search.as_mut() {
                    search.failed = true;
                    search.matched = None;
                }
                self.dirty = true;
            }
        }
    }

    /// The newest entry at or below `from` that contains `query`.
    fn find_match(&self, query: &[u8], from: usize) -> Option<usize> {
        let entries = self.history.entries();
        let mut index = from.min(entries.len().checked_sub(1)?);
        loop {
            if contains(entries[index].text(), query) {
                return Some(index);
            }
            index = index.checked_sub(1)?;
        }
    }

    /// Put entry `index` on the line and the cursor at the start of the match.
    ///
    /// Leaving the cursor on the match (rather than at the end of the line)
    /// is what makes readline's "a movement key accepts the match and puts the
    /// cursor there for editing" work.
    fn show_match(&mut self, index: usize) {
        let query = self
            .search
            .as_ref()
            .map(|search| search.query.clone())
            .unwrap_or_default();
        let Some(entry) = self.history.entries().get(index) else {
            return;
        };
        let text = entry.text().to_vec();
        let at = contains_at(&text, &query).unwrap_or(0);
        self.line.set_text(&text);
        self.line.set_cursor(at);
        self.dirty = true;
    }

    /// `Esc`/`Ctrl-G`: restore the line the search started from.
    fn abort_search(&mut self) {
        let Some(search) = self.search.take() else {
            return;
        };
        self.line.set_text(&search.original);
        self.line
            .set_cursor(search.original_cursor.min(search.original.len()));
        self.dirty = true;
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
            || self.suggestion_dismissed
            || self.line.in_continuation()
            || self.line.cursor() != self.line.buffer().len()
            || self.line.buffer().is_empty()
            // Whitespace-only input is not a prefix of anything worth
            // suggesting, and painting a whole command after three spaces is
            // noise rather than help.
            || self.line.buffer().iter().all(u8::is_ascii_whitespace)
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
        let search_prompt = self.search.as_ref().map(Search::prompt);
        let prompt = if let Some(prompt) = &search_prompt {
            prompt
        } else if self.secondary {
            self.prompt.secondary_text()
        } else {
            self.prompt.primary_text()
        };
        let columns = self.term.size().columns_or_fallback();
        // A menu taller than the screen would scroll the prompt away, so the
        // height is capped by the rows left below the prompt and the line. A
        // terminal that reports no size falls back to the configured maximum.
        let rows_available = match self.term.size().rows {
            0 => self.max_menu_rows,
            rows => usize::from(rows.saturating_sub(2)).max(1),
        };
        let max_rows = self.max_menu_rows.min(rows_available);
        let rows = self.menu.as_mut().map(|menu| menu.rows(max_rows, columns));
        let menu = rows.as_deref().map(|rows| MenuView { rows });
        let ghost = if self.menu.is_some() || self.search.is_some() {
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

/// The offset of `query` in `text`, or `None`. An empty query matches at 0.
fn contains_at(text: &[u8], query: &[u8]) -> Option<usize> {
    if query.is_empty() {
        return Some(0);
    }
    text.windows(query.len()).position(|window| window == query)
}

/// Whether `query` occurs in `text`; an empty query occurs everywhere.
fn contains(text: &[u8], query: &[u8]) -> bool {
    contains_at(text, query).is_some()
}

/// Whether `key` extends a kill sequence rather than ending one.
fn is_kill(key: &Key) -> bool {
    matches!(key, Key::Ctrl(0x0b) | Key::Ctrl(0x15) | Key::Ctrl(0x17))
}

/// Whether `key` changes the line, and so may change the ghost text.
fn edits_line(key: &Key) -> bool {
    matches!(
        key,
        Key::Char(_)
            | Key::Byte(_)
            | Key::Paste(_)
            | Key::Backspace
            | Key::Delete
            | Key::Up
            | Key::Down
            | Key::Ctrl(0x04 | 0x0b | 0x0e | 0x10 | 0x14 | 0x15 | 0x17 | 0x19)
    )
}
