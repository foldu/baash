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
//! the user with an unusable shell; the guard in `baash-sys` restores on drop.

use std::io;
use std::ops::Range;

use baash_complete::{Completer, Menu};
use baash_highlight::Theme;

use baash_runtime::History;

use crate::completion::MenuState;
use crate::key::{Code, Decoder, Key, Modifiers};
use crate::line::Line;
use crate::prompt::{ExpandedPrompt, Prompt};
use crate::render::{Frame, MenuView, Renderer};
use crate::term::{Input, Terminal};

/// How long the prompt waits for a cursor-position report.
///
/// A terminal answers in microseconds; the deadline only bounds the cost on
/// one that does not answer at all, which is why it is short. Such a terminal
/// pays it on every prompt, so it must not be generous.
const CURSOR_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(50);

/// How often the read loop asks a pending shell completion whether it has an
/// answer. The completion itself is bounded by its own deadline; this only
/// decides how promptly a finished answer is painted while no key arrives.
const COMPLETION_WAIT: std::time::Duration = std::time::Duration::from_millis(10);

/// How long a submitted `Enter` waits for a pending completion to answer.
///
/// A hook's own deadline is shorter (carapace's is 500 ms), so this is only
/// the backstop for a hook that never answers: the line is submitted as typed
/// rather than held forever.
const DEFERRED_KEY_WAIT: std::time::Duration = std::time::Duration::from_millis(1_000);

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

/// What one processed key did to the read loop.
enum KeyStep {
    /// Keep reading keys for this line.
    Continue,
    /// The line is over: submitted, interrupted, or end of input.
    Done(ReadResult),
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
/// `baash-repl` does not depend on `baash-syntax` (`docs/contracts.md` keeps the
/// front end out of the editor's dependency set), so the parser enters through
/// this trait. `baash` implements it with `parse_bytes` and
/// `ParseOutcome::NeedMoreInput`.
pub trait Syntax {
    /// Whether `buffer` is unfinished — an open quote, `if`, here-doc, `$(`.
    fn needs_more_input(&self, buffer: &[u8]) -> bool;

    /// A syntax error in `buffer`, for the hint under the line.
    ///
    /// The answer must be a *certain* error: `if x; then` and an unclosed
    /// quote are lines still being typed, and the implementation is what
    /// decides that, because only it has a parser. The default answers
    /// `None`, so an editor driven by a parser that only knows "is it
    /// finished?" invents nothing.
    fn check(&self, _buffer: &[u8]) -> Option<LineError> {
        None
    }
}

/// A syntax error the editor can paint while the line is still being typed.
///
/// The editor does not parse; [`Syntax::check`] does. This is the small part
/// the editor needs: where the error is, and what to say about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineError {
    /// Byte range in the checked buffer the error is about. It may be empty —
    /// a missing token has no bytes — and may sit in an earlier continuation
    /// line, in which case only the message is painted.
    pub span: Range<usize>,
    /// One line, already short enough to be clipped to the terminal width.
    pub message: String,
}

/// A prompt that can change while a line is being edited.
///
/// The editor owns the screen, not the shell, so a prompt that updates while
/// the user is thinking enters as this: the caller answers with a fresh
/// [`Prompt`] when something changed, and `None` when nothing did. The
/// recompute runs between keystrokes, never in the middle of one, so a slow
/// answer delays the repaint and not the typing.
pub trait LivePrompt {
    /// The prompt to paint now, or `None` if it is unchanged.
    fn refresh(&mut self) -> Option<Prompt>;
}

/// A [`LivePrompt`] that never changes anything.
#[derive(Clone, Copy, Debug, Default)]
pub struct NeverRefreshes;

impl LivePrompt for NeverRefreshes {
    fn refresh(&mut self) -> Option<Prompt> {
        None
    }
}

/// Completion that needs the shell itself.
///
/// `complete -F` names a shell function, so its candidates are shell code: the
/// engine can assemble the input but cannot run it (`docs/contracts.md`). A
/// caller that owns the interpreter implements this and passes it to
/// [`Editor::read_line_with_shell`]; the editor asks it before the data
/// completer, and `None` means "no rule here needs shell code", so the ordinary
/// completer answers.
///
/// The call happens while the terminal is in raw mode and the frame is on
/// screen. An implementation must not write to the terminal: whatever the
/// function prints has to be captured and painted by the caller after the read,
/// or dropped.
pub trait ShellCompletion {
    /// The menu for the word at `cursor`, or `None` when no rule needs the
    /// shell.
    fn complete(&mut self, line: &[u8], cursor: usize) -> Option<ShellMenu>;

    /// Start a completion that may finish later.
    ///
    /// This is what makes a slow source (an external `carapace` lookup) not
    /// block the prompt: the hook can spawn the process, return
    /// [`ShellCompletionState::Pending`], and be polled while the editor keeps
    /// reading keys. The default is the old synchronous answer, so a hook that
    /// has nothing asynchronous to do does not implement this or
    /// [`ShellCompletion::poll`].
    fn begin(&mut self, line: &[u8], cursor: usize) -> ShellCompletionState {
        ShellCompletionState::Ready(self.complete(line, cursor))
    }

    /// Continue a completion that [`ShellCompletion::begin`] left pending.
    ///
    /// Called repeatedly while the editor waits; it must not block. A hook
    /// that returned [`ShellCompletionState::Pending`] from `begin` must
    /// eventually answer `Ready`, and `Ready(None)` means "let the data engine
    /// answer", exactly as `None` from `complete` does.
    fn poll(&mut self) -> ShellCompletionState {
        ShellCompletionState::Ready(None)
    }

    /// Abandon a pending completion because the user typed or the line is
    /// over. Called at most once per `Pending`; the hook must not keep a
    /// process or its pipe alive afterwards.
    fn cancel(&mut self) {}
}

/// What a [`ShellCompletion`] has for the current Tab.
#[derive(Clone, Debug)]
pub enum ShellCompletionState {
    /// The hook has an answer: `Some` is its menu, `None` means the data
    /// engine should answer.
    Ready(Option<ShellMenu>),
    /// The answer is not available yet; call [`ShellCompletion::poll`] again
    /// from the editor's wait loop.
    Pending,
}

/// A menu that came from shell code, plus whatever the function printed.
///
/// bash paints a completion function's output above the prompt and then
/// repaints the line; the editor does the same with [`ShellMenu::output`], so a
/// script that warns while completing does not lose its message and does not
/// garble the frame.
#[derive(Clone, Debug)]
pub struct ShellMenu {
    /// The candidates the function produced.
    pub menu: Menu,
    /// The function's standard output, in the order it was written.
    pub output: Vec<u8>,
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
    /// Whether to emit ANSI styling. Set from `color_supported` in `baash`.
    pub color: bool,
    /// `PS1`/`PS2` templates and the state their escapes read.
    pub prompt: Prompt,
    /// History: the session ring, with the database behind it.
    pub history: History,
    /// Maximum number of rows the completion menu may use.
    pub max_menu_rows: usize,
    /// Whether to paint history ghost text after the cursor.
    pub auto_suggest: bool,
    /// How long a submitted `Enter` waits for a pending completion to answer.
    ///
    /// A hook's own deadline is shorter (carapace's lookup is bounded at
    /// 500 ms); this is the backstop for a hook that never answers, so the
    /// line is submitted as typed rather than held forever.
    pub submit_wait: std::time::Duration,
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
            submit_wait: DEFERRED_KEY_WAIT,
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
    /// Bytes written to the terminal immediately before the next line's
    /// prompt: announcements that are not part of the prompt itself, such as
    /// the working-directory report (OSC 7). They are not painted through the
    /// frame, because they have no width and the renderer would count them.
    preamble: Vec<u8>,
    /// The screen row (one-based) the prompt started on, from the
    /// cursor-position report. Mouse clicks are reported in screen cells, so
    /// without this there is nothing to measure them against; `None` means the
    /// terminal did not answer and clicks are ignored.
    prompt_row: Option<u16>,
    /// A [`ShellCompletion::begin`] returned [`ShellCompletionState::Pending`]
    /// and has not answered yet. While this is set the read loop polls the hook
    /// instead of parking on the keyboard alone.
    completion_pending: bool,
    /// A key that has to wait for a pending completion to answer.
    ///
    /// Only `Enter` waits: bash performs completion synchronously, so a fast
    /// Tab-Enter runs the completed line. For the asynchronous carapace lookup
    /// that means keeping the process alive instead of cancelling it, and
    /// processing the key once the answer is painted.
    deferred_key: Option<Key>,
    /// When a deferred key stops waiting for the hook.
    ///
    /// The hook is supposed to answer (carapace's own deadline is 500 ms); this
    /// backstop only keeps a hook that never does from holding the line.
    deferred_deadline: Option<std::time::Instant>,
    /// How long a deferred key may wait (see [`Config::submit_wait`]).
    submit_wait: std::time::Duration,
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
            preamble: Vec::new(),
            prompt_row: None,
            completion_pending: false,
            deferred_key: None,
            deferred_deadline: None,
            submit_wait: config.submit_wait,
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

    /// Bytes to write once, immediately before the next line's prompt.
    ///
    /// For protocol announcements that are not part of the prompt and have no
    /// display width — today the working-directory report (OSC 7). The shell
    /// sets it with the prompt; the editor writes it once per line rather than
    /// once per repaint, so a keystroke cannot repeat it. An empty slice
    /// disables it.
    pub fn set_preamble(&mut self, bytes: &[u8]) {
        self.preamble.clear();
        self.preamble.extend_from_slice(bytes);
    }

    /// Tell the terminal a command is starting (OSC 133 `C`).
    ///
    /// The editor does not see execution, so the loop calls this between
    /// reading the line and running it, and [`Editor::command_finished`] when
    /// it returns. The mark is written outside raw mode, which is why it
    /// cannot go through the frame like the prompt marks do.
    pub fn command_started(&mut self, command: &[u8]) {
        if !self.term.supports_control_sequences() {
            return;
        }
        let _ = self.term.write_all(&baash_sys::osc::command_start(command));
    }

    /// Tell the terminal the command finished, with its exit status
    /// (OSC 133 `D`).
    pub fn command_finished(&mut self, status: i32) {
        if !self.term.supports_control_sequences() {
            return;
        }
        let _ = self
            .term
            .write_all(&baash_sys::osc::command_finished(status));
    }

    /// Ask the terminal to raise a desktop notification (`OSC 9`).
    ///
    /// The shell decides *when* — it is the only party that knows the command
    /// finished and how long it took — and the editor writes it, because it
    /// owns the terminal and knows whether the terminal can read control
    /// sequences at all. Written between commands, so a child never sees it.
    pub fn notify(&mut self, text: &[u8]) {
        if !self.term.supports_control_sequences() {
            return;
        }
        let _ = self.term.write_all(&baash_sys::osc::notify(text));
    }

    /// Wait for the history writes this session queued to reach the database.
    pub fn finish(&mut self) {
        self.history.flush();
    }

    /// Read one complete logical command.
    ///
    /// Raw mode is held only for the duration of this call.
    pub fn read_line(&mut self) -> io::Result<ReadResult> {
        self.read_line_with(&mut NeverRefreshes)
    }

    /// Read one complete logical command, keeping the prompt live.
    ///
    /// `live` is asked for a new prompt whenever the terminal reports that the
    /// watched directory changed, and its answer is painted immediately. The
    /// terminal has to be armed for that to happen
    /// ([`Terminal::set_watch`]); with no watch, this is [`Editor::read_line`].
    pub fn read_line_with(&mut self, live: &mut dyn LivePrompt) -> io::Result<ReadResult> {
        self.read_line_with_shell(live, None)
    }

    /// Read one line, letting `shell` answer completions that need the
    /// interpreter.
    ///
    /// This is [`Editor::read_line_with`] plus the `complete -F` seam: on Tab
    /// the editor first asks `shell`, and only when it answers `None` does the
    /// data completer run. A `None` hook is the plain editor.
    pub fn read_line_with_shell(
        &mut self,
        live: &mut dyn LivePrompt,
        mut shell: Option<&mut dyn ShellCompletion>,
    ) -> io::Result<ReadResult> {
        self.begin();
        self.term.enter_raw()?;
        self.start_at_a_fresh_line()?;
        // Announcements go before the prompt is painted, not inside it: the
        // frame's column arithmetic must not see them, and a broken write is
        // not worth failing the line over.
        if !self.preamble.is_empty() {
            let _ = self.term.write_all(&self.preamble);
        }
        // Shell-integration marks: `A` before the prompt text and `B` after
        // the first frame. The terminal shows neither; what it gains is
        // knowing where its prompt ends and the command line begins, which is
        // what prompt-jump, command selection and per-command cwd are built
        // on. `B` is written once per line, not per frame, so a keystroke does
        // not repeat it.
        let marked = self.term.supports_control_sequences();
        if marked {
            let _ = self.term.write_all(baash_sys::osc::PROMPT_START);
        }
        let result = self.read_line_loop(live, &mut shell, marked);
        // The loop cancels before every normal exit; an error short-circuit
        // could otherwise leave a completion child running with raw mode off.
        self.abandon_completion(&mut shell);
        let _ = self.term.leave_raw();
        result
    }

    /// Hand the terminal a directory watch to await with the keyboard.
    ///
    /// The editor does not create it (the prompt's git segment is the shell's
    /// business, not the editor's) and does not drain it: whoever owns the
    /// watch answers for it in [`LivePrompt::refresh`].
    #[cfg(unix)]
    pub fn set_watch(&mut self, fd: Option<std::os::fd::OwnedFd>) {
        self.term.set_watch(fd);
    }

    /// Hand the terminal a descriptor that goes ready when the live prompt has
    /// news that is not a filesystem event.
    ///
    /// The prompt's git counters come from a worker; nothing about a finished
    /// worker task is pollable, so the prompt supplies a channel the worker
    /// writes when its result lands. The editor treats it exactly like a
    /// changed directory: ask [`LivePrompt::refresh`] and repaint if the bytes
    /// differ. Whoever owns the channel drains it in `refresh`.
    #[cfg(unix)]
    pub fn set_wake(&mut self, fd: Option<std::os::fd::OwnedFd>) {
        self.term.set_wake(fd);
    }

    /// Apply one decoded key; returns what the caller should do next.
    ///
    /// The editor redraws for [`Flow::Continue`]. This is public so a caller
    /// with its own event loop (and the tests) can drive the editor key by key
    /// without going through [`Terminal::read_byte`].
    pub fn step(&mut self, key: Key) -> io::Result<Flow> {
        self.step_with(key, &mut None)
    }

    /// [`Editor::step`] with a shell-backed completion hook in scope.
    fn step_with(
        &mut self,
        key: Key,
        shell: &mut Option<&mut dyn ShellCompletion>,
    ) -> io::Result<Flow> {
        let flow = self.handle_with(key, shell)?;
        if flow == Flow::Continue {
            self.draw()?;
        }
        Ok(flow)
    }

    /// Reset per-line state and paint the first prompt.
    fn begin(&mut self) {
        self.prompt_row = None;
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
        self.completion_pending = false;
        self.deferred_key = None;
        self.deferred_deadline = None;
        self.renderer.reset();
        self.dirty = true;
    }

    /// Put the cursor where the user clicked on the command line.
    ///
    /// The terminal reports a screen cell; the frame is a `columns`-wide strip
    /// of rows beginning at the prompt's row, so subtracting the prompt's cell
    /// count gives an offset into the editable text. Without a
    /// cursor-position report there is no row to measure from, and a click is
    /// ignored rather than guessed at. A click on the prompt itself lands at
    /// the start of the line.
    fn move_cursor_to_click(&mut self, column: u16, row: u16) {
        let Some(prompt_row) = self.prompt_row else {
            return;
        };
        // A continuation line's editable region contains newlines, and the
        // strip model does not hold across them.
        if self.line.editable().contains(&b'\n') {
            return;
        }
        let Some(row_offset) = row.checked_sub(prompt_row) else {
            return;
        };
        let columns = self.term.size().columns_or_fallback().max(1);
        let cell = u32::from(row_offset)
            .saturating_mul(u32::from(columns))
            .saturating_add(u32::from(column.saturating_sub(1)));
        let offset = cell.saturating_sub(u32::from(self.renderer.prompt_cells())) as usize;
        let end = self.line.start() + self.line.editable().len();
        self.line.set_cursor(self.line.start() + offset.min(end));
        self.menu = None;
    }

    /// Start the prompt on a line of its own when the output before it did not
    /// end with a newline.
    ///
    /// A command can leave the cursor mid-line (`printf x`, a program that
    /// does not end its output), and frames are anchored at column 0 because
    /// the editor cannot see what a child wrote. Asking the terminal where the
    /// cursor is turns that guess into a fact: a column other than 1 means the
    /// frame would paint over the output, so a newline goes first. A terminal
    /// that does not answer costs one short timeout and keeps the old
    /// behaviour; the Windows console never answers and never asks.
    fn start_at_a_fresh_line(&mut self) -> io::Result<()> {
        if !self.term.reports_cursor_position() {
            return Ok(());
        }
        self.term.write_all(b"\x1b[6n")?;
        let position = self
            .decoder
            .cursor_position(self.term, CURSOR_QUERY_TIMEOUT)?;
        if let Some((row, column)) = position {
            if column != 1 {
                self.term.write_all(b"\r\n")?;
                self.renderer.reset();
            }
            // Clicks are reported in screen rows, so the prompt's own row is
            // what they are measured from. A newline above moves it down one;
            // if that newline scrolled the screen instead, the row is one too
            // high and a click lands a row off, which moves the cursor near
            // the click rather than somewhere unrelated.
            self.prompt_row = Some(if column == 1 {
                row
            } else {
                row.saturating_add(1)
            });
        }
        Ok(())
    }

    fn read_line_loop(
        &mut self,
        live: &mut dyn LivePrompt,
        shell: &mut Option<&mut dyn ShellCompletion>,
        marked: bool,
    ) -> io::Result<ReadResult> {
        self.draw()?;
        if marked {
            let _ = self.term.write_all(baash_sys::osc::PROMPT_END);
        }
        loop {
            // Poll an outstanding asynchronous completion before waiting: its
            // process may have answered while the loop was painting.
            if self.completion_pending {
                self.poll_shell(shell)?;
                if self.completion_pending && self.deferred_key.is_some() {
                    // A key is ordered behind the answer. Wait for it, but not
                    // past the backstop: a hook that never answers must not
                    // hold the line.
                    if self
                        .deferred_deadline
                        .is_some_and(|deadline| std::time::Instant::now() >= deadline)
                    {
                        self.abandon_completion(shell);
                    } else {
                        std::thread::sleep(COMPLETION_WAIT);
                        continue;
                    }
                }
                if !self.completion_pending {
                    if let Some(key) = self.deferred_key.take() {
                        self.deferred_deadline = None;
                        if let KeyStep::Done(result) = self.process_key(key, shell)? {
                            return Ok(result);
                        }
                    }
                    continue;
                }
            }
            // A key pushed back by the cursor-position query is already in the
            // decoder, so the kernel has nothing to wake on: waiting first
            // would park the line until the user typed again.
            if !self.decoder.has_pending() {
                let waited = if self.completion_pending {
                    self.term.await_input_timeout(COMPLETION_WAIT)?
                } else {
                    self.term.await_input()?
                };
                match waited {
                    Input::Key => {}
                    Input::Timeout => continue,
                    Input::Resize => {
                        // The prompt's screen row is no longer known: the
                        // terminal reflowed and the next report is a prompt
                        // away.
                        self.prompt_row = None;
                        // The terminal reflowed its contents; the frame's rows
                        // and columns describe the old width. Re-anchor from
                        // the cursor and paint again at the size the renderer
                        // reads now.
                        self.renderer.reset();
                        self.dirty = true;
                        self.draw()?;
                        continue;
                    }
                    Input::Watch => {
                        // No key was consumed and the line is untouched: the
                        // watched directory changed, so the prompt is
                        // repainted in place and the wait resumes. A refresh
                        // that finds nothing to say leaves the frame alone.
                        if let Some(prompt) = live.refresh() {
                            self.set_prompt(prompt);
                            self.draw()?;
                        }
                        continue;
                    }
                }
            }
            let Some(key) = self.decoder.next(self.term)? else {
                self.abandon_completion(shell);
                self.deferred_key = None;
                self.deferred_deadline = None;
                return self.finish_eof();
            };
            // A keystroke means the pending answer is for a line the user has
            // already moved on from. `Enter` is the exception: bash completes
            // synchronously, so a fast Tab-Enter runs the completed line
            // instead of cancelling the lookup. Keep it and process it once
            // the answer has been painted.
            if self.completion_pending && submits(&key) {
                self.deferred_key = Some(key);
                self.deferred_deadline = Some(std::time::Instant::now() + self.submit_wait);
                continue;
            }
            self.abandon_completion(shell);
            match self.process_key(key, shell)? {
                KeyStep::Continue => {}
                KeyStep::Done(result) => return Ok(result),
            }
        }
    }

    /// Apply one decoded key, finishing the line when it asks to.
    fn process_key(
        &mut self,
        key: Key,
        shell: &mut Option<&mut dyn ShellCompletion>,
    ) -> io::Result<KeyStep> {
        match self.step_with(key, shell)? {
            Flow::Continue => Ok(KeyStep::Continue),
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
                    return Ok(KeyStep::Continue);
                }
                let text = self.line.take();
                self.history.push(&text);
                self.renderer.goto_end(self.term)?;
                self.term.write_all(b"\r\n")?;
                self.renderer.reset();
                self.secondary = false;
                Ok(KeyStep::Done(ReadResult::Line(text)))
            }
            Flow::Interrupt => Ok(KeyStep::Done(self.finish_interrupt()?)),
            Flow::Eof => Ok(KeyStep::Done(self.finish_eof()?)),
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
        self.handle_with(key, &mut None)
    }

    fn handle_with(
        &mut self,
        key: Key,
        shell: &mut Option<&mut dyn ShellCompletion>,
    ) -> io::Result<Flow> {
        if self.search.is_some() {
            return self.handle_search(key, shell);
        }
        if self.menu.is_some() {
            return self.handle_menu(key);
        }
        if !is_kill(&key) {
            self.line.kill_reset();
        }
        if !key.is(Code::Tab) {
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
        // A lock key is not part of what a key means: the kitty protocol
        // reports it in the modifier field, so `Ctrl-D` with Num Lock on
        // arrives as `CSI 100 ; 133 u` and still has to be Ctrl-D.
        let (code, mods) = (key.code, key.mods.binding());
        match (code, mods) {
            // A character is text unless a command modifier is held: shift,
            // caps and num lock are already reflected in the code point (or,
            // from the kitty protocol, applied here for ASCII letters).
            (Code::Char(character), mods) if mods.is_text() => {
                self.line.insert_char(shifted(character, mods));
            }
            (Code::Byte(byte), _) => {
                self.line.insert(&[byte]);
            }
            (Code::Paste(bytes), _) => {
                self.line.insert(&bytes);
            }
            (Code::Enter, Modifiers::NONE) => return Ok(Flow::Submit),
            (Code::Tab, Modifiers::NONE) => self.complete_with(shell)?,
            // Shift-Tab is the menu's backward step; on the line it is not a
            // gesture.
            (Code::Tab, Modifiers::SHIFT) => {}
            (Code::Esc, _) => {
                // Dismiss the ghost text for this line. Typing refines the
                // line and brings it back.
                self.suggestion_dismissed = true;
            }
            (Code::FocusIn, _) => {
                // Another window may have painted over this one while the line
                // sat idle, so the frame is drawn again. Losing focus changes
                // nothing on screen.
            }
            (Code::FocusOut, _) => self.dirty = false,
            (Code::Backspace, Modifiers::NONE) => {
                self.line.backspace();
            }
            // Alt-Backspace: `ESC DEL` or `ESC BS` in the legacy encoding, and
            // `CSI 127 ; 3 u` from a terminal that disambiguates. Both are
            // readline's `backward-kill-word`.
            (Code::Backspace, Modifiers::ALT) => {
                self.line.kill_word_backward();
            }
            (Code::Delete, Modifiers::NONE) => {
                self.line.delete();
            }
            // `Ctrl-Delete` has no legacy encoding at all, so this arm only
            // fires on a terminal that speaks one of the enhanced protocols.
            (Code::Delete, Modifiers::CTRL) => {
                self.line.kill_word_forward();
            }
            (Code::Left, Modifiers::NONE) => {
                self.line.move_left();
            }
            (Code::Left, Modifiers::CTRL | Modifiers::ALT) => {
                self.line.move_word_left();
            }
            (Code::Right, Modifiers::NONE) => {
                if !self.line.move_right() {
                    self.accept_suggestion();
                }
            }
            (Code::Right, Modifiers::CTRL | Modifiers::ALT) => {
                self.line.move_word_right();
            }
            (Code::Home, Modifiers::NONE) => {
                self.line.move_home();
            }
            (Code::End, Modifiers::NONE) => {
                if !self.line.move_end() {
                    // Already at the end: End accepts history ghost text, the
                    // same as Right does.
                    self.accept_suggestion();
                }
            }
            (Code::Char('r'), Modifiers::CTRL) => self.begin_search(),
            (Code::Up, Modifiers::NONE) | (Code::Char('p'), Modifiers::CTRL) => {
                self.history_previous();
            }
            (Code::Down, Modifiers::NONE) | (Code::Char('n'), Modifiers::CTRL) => {
                self.history_next();
            }
            (Code::Char('a'), Modifiers::CTRL) => {
                self.line.move_home();
            }
            (Code::Char('b'), Modifiers::CTRL) => {
                self.line.move_left();
            }
            (Code::Char('c'), Modifiers::CTRL) => return Ok(Flow::Interrupt),
            (Code::Char('d'), Modifiers::CTRL) => {
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
            // The console's end-of-file key. The editor turns processed input
            // off so `Ctrl-C` is a keystroke rather than a signal, which also
            // means `Ctrl-Z` arrives as a byte; without this arm a Windows
            // user would have no key that ends the session.
            #[cfg(windows)]
            (Code::Char('z'), Modifiers::CTRL) => {
                if self.line.buffer().is_empty() && !self.line.in_continuation() {
                    return Ok(Flow::Eof);
                }
            }
            (Code::Char('e'), Modifiers::CTRL) => {
                // `End`'s readline spelling. Moving to the end is the same
                // gesture as pressing End there, so being there already
                // accepts the ghost text, as it does for `Code::End`.
                if !self.line.move_end() {
                    self.accept_suggestion();
                }
            }
            (Code::Char('f'), Modifiers::CTRL) => {
                // `Right`'s readline spelling, and fish's `forward-char`:
                // forward at the end of the line is how the suggestion is
                // taken, so `Ctrl-F` does what `Right` does.
                if !self.line.move_right() {
                    self.accept_suggestion();
                }
            }
            (Code::Char('k'), Modifiers::CTRL) => {
                self.line.kill_to_end();
            }
            (Code::MouseLeft { column, row }, _) => {
                self.move_cursor_to_click(column, row);
            }
            (Code::Char('x'), Modifiers::CTRL) => {
                // Copy the line to the terminal's clipboard (OSC 52). There is
                // no selection yet, so the whole buffer is the payload; the
                // clipboard belongs to the terminal, which is what makes this
                // work over ssh. A terminal may ignore it or cap the size.
                if self.term.supports_control_sequences() {
                    let copy = baash_sys::osc::clipboard(self.line.buffer());
                    self.term.write_all(&copy)?;
                }
            }
            (Code::Char('l'), Modifiers::CTRL) => {
                // Push what is on screen into the scrollback before clearing
                // it. `2J` erases the viewport and keeps no text, and the
                // output above the prompt is exactly what a user scrolls back
                // to; ECMA-48's `SU` scrolls the page contents off the top,
                // which is where terminals put scrollback. The cursor does not
                // move, so the clear that follows is the same one as before.
                let rows = self.term.size().rows_or_fallback();
                self.term.write_all(format!("\x1b[{rows}S").as_bytes())?;
                self.term.write_all(b"\x1b[H\x1b[2J")?;
                self.renderer.reset();
            }
            (Code::Char('u'), Modifiers::CTRL) => {
                self.line.kill_to_start();
            }
            (Code::Char('w'), Modifiers::CTRL) => {
                self.line.kill_word();
            }
            (Code::Char('y'), Modifiers::CTRL) => {
                self.line.yank();
            }
            (Code::Char('t'), Modifiers::CTRL) => {
                self.line.transpose();
            }
            (Code::Char('b'), Modifiers::ALT) => {
                self.line.move_word_left();
            }
            (Code::Char('f'), Modifiers::ALT) => {
                self.line.move_word_right();
            }
            (Code::Char('d'), Modifiers::ALT) => {
                self.line.kill_word_forward();
            }
            _ => self.dirty = false,
        }
        Ok(Flow::Continue)
    }

    fn handle_menu(&mut self, key: Key) -> io::Result<Flow> {
        let Key { code, mods } = key;
        let mods = mods.binding();
        match (code, mods) {
            // Tab and Shift-Tab cycle the menu (fish's pager behaviour), and
            // Enter inserts the selection. Up/Down and Ctrl-P/Ctrl-N are the
            // readline spellings of the same movement.
            (Code::Tab, Modifiers::NONE)
            | (Code::Down, Modifiers::NONE)
            | (Code::Char('n'), Modifiers::CTRL) => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.next();
                }
                self.dirty = true;
                Ok(Flow::Continue)
            }
            (Code::Tab, Modifiers::SHIFT)
            | (Code::Up, Modifiers::NONE)
            | (Code::Char('p'), Modifiers::CTRL) => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.previous();
                }
                self.dirty = true;
                Ok(Flow::Continue)
            }
            (Code::Enter, Modifiers::NONE) => {
                self.accept_selection();
                Ok(Flow::Continue)
            }
            (Code::Esc, _) => {
                self.menu = None;
                self.prefix_tab = false;
                self.dirty = true;
                Ok(Flow::Continue)
            }
            (code, mods) => {
                self.menu = None;
                self.prefix_tab = false;
                self.dirty = true;
                self.handle(Key { code, mods })
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

    /// Run `complete` for the current line, asking the shell first.
    ///
    /// A `-F` rule is shell code; when the hook answers, that menu is offered
    /// instead of the data one. A hook that answers `None` (or no hook at all)
    /// keeps the engine's completion, which is also what a rule whose function
    /// could not run falls back to. A hook that answers `Pending` has started
    /// work the loop polls; nothing is offered until it answers.
    fn complete_with(&mut self, shell: &mut Option<&mut dyn ShellCompletion>) -> io::Result<()> {
        let state = match shell {
            Some(shell) => shell.begin(self.line.buffer(), self.line.cursor()),
            None => ShellCompletionState::Ready(None),
        };
        self.apply_completion(state)
    }

    /// Offer whatever the hook answered, or fall back to the data engine.
    fn apply_completion(&mut self, state: ShellCompletionState) -> io::Result<()> {
        match state {
            ShellCompletionState::Pending => {
                self.completion_pending = true;
                Ok(())
            }
            ShellCompletionState::Ready(Some(shell_menu)) => {
                self.completion_pending = false;
                if !shell_menu.output.is_empty() {
                    self.print_above(&shell_menu.output)?;
                }
                self.dirty = true;
                self.offer(shell_menu.menu)
            }
            ShellCompletionState::Ready(None) => {
                self.completion_pending = false;
                let menu = self
                    .completer
                    .complete(self.line.buffer(), self.line.cursor());
                self.dirty = true;
                self.offer(menu)
            }
        }
    }

    /// Poll a pending hook; a finished answer is painted here, not by the key
    /// path, because no key produced it.
    fn poll_shell(&mut self, shell: &mut Option<&mut dyn ShellCompletion>) -> io::Result<()> {
        let state = match shell {
            Some(shell) => shell.poll(),
            None => ShellCompletionState::Ready(None),
        };
        let was_pending = self.completion_pending;
        self.apply_completion(state)?;
        if was_pending && !self.completion_pending {
            self.draw()?;
        }
        Ok(())
    }

    /// Stop polling and tell the hook to release its process.
    fn abandon_completion(&mut self, shell: &mut Option<&mut dyn ShellCompletion>) {
        if !self.completion_pending {
            return;
        }
        if let Some(shell) = shell {
            shell.cancel();
        }
        self.completion_pending = false;
    }

    /// Erase the frame, write `bytes` where it was, and repaint below.
    ///
    /// This is how bash shows a completion function's output: it belongs above
    /// the prompt, because the function wrote it while the line was being
    /// edited, and the line has to be repainted under it.
    fn print_above(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.renderer.clear_frame(self.term)?;
        self.term.write_all(bytes)?;
        if !bytes.ends_with(b"\n") {
            self.term.write_all(b"\r\n")?;
        }
        self.renderer.reset();
        self.dirty = true;
        // The frame is painted once below the output, together with whatever
        // `offer` decides (an insertion or the menu); drawing here would paint
        // twice and leave the insertion unpainted.
        Ok(())
    }

    fn offer(&mut self, menu: Menu) -> io::Result<()> {
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
    fn handle_search(
        &mut self,
        key: Key,
        shell: &mut Option<&mut dyn ShellCompletion>,
    ) -> io::Result<Flow> {
        let Key { code, mods } = key;
        let mods = mods.binding();
        match (code, mods) {
            (Code::Char('r'), Modifiers::CTRL) => {
                self.search_older();
                Ok(Flow::Continue)
            }
            (Code::Char('g'), Modifiers::CTRL) | (Code::Esc, _) => {
                self.abort_search();
                Ok(Flow::Continue)
            }
            (Code::Char('c'), Modifiers::CTRL) => {
                // The whole line is abandoned; there is nothing to restore.
                self.search = None;
                Ok(Flow::Interrupt)
            }
            (Code::Enter, Modifiers::NONE) => {
                self.search = None;
                Ok(Flow::Submit)
            }
            (Code::Backspace, Modifiers::NONE) => {
                if let Some(search) = self.search.as_mut()
                    && search.query.pop().is_some()
                {
                    self.research();
                }
                Ok(Flow::Continue)
            }
            (Code::Char(character), mods) if mods.is_text() => {
                self.search_insert(shifted(character, mods).to_string().as_bytes());
                Ok(Flow::Continue)
            }
            (Code::Byte(byte), _) => {
                self.search_insert(&[byte]);
                Ok(Flow::Continue)
            }
            (code, mods) => {
                // Accept and let the key act on the accepted line.
                self.search = None;
                self.dirty = true;
                self.handle_with(Key { code, mods }, shell)
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

    /// The syntax error to paint under the line, if there is a certain one.
    ///
    /// The check runs over the whole logical buffer rather than the live
    /// segment: a continuation line parsed alone (`fi`) is an error in
    /// isolation and fine in context, so checking the segment would flash a
    /// false error while a block is being finished. The span is then mapped
    /// into the editable segment's own coordinates, and dropped when it names
    /// an earlier line — the message is still worth showing, the underline is
    /// not.
    fn check(&self) -> (Option<String>, Option<Range<usize>>) {
        if self.search.is_some() {
            return (None, None);
        }
        let Some(issue) = self.syntax.check(self.line.buffer()) else {
            return (None, None);
        };
        let start = self.line.start();
        let end = start + self.line.editable().len();
        // An empty span is "something is missing here", not "this byte is
        // wrong": marking a byte would underline the token next to the point
        // and say the wrong thing about it.
        let bad = (issue.span.start < issue.span.end
            && issue.span.start >= start
            && issue.span.end <= end)
            .then(|| (issue.span.start - start)..(issue.span.end - start));
        (Some(issue.message), bad)
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
        let size = self.term.size();
        let columns = size.columns_or_fallback();
        let ghost = if self.menu.is_some() || self.search.is_some() {
            None
        } else {
            self.suggestion()
        };
        let (hint, bad) = self.check();
        let mut frame = Frame {
            prompt,
            line: self.line.editable(),
            cursor: self.line.editable_cursor(),
            menu: None,
            ghost: ghost.as_deref(),
            hint: hint.as_deref(),
            bad,
        };
        // A frame taller than the screen scrolls its own top row out of reach,
        // and the next repaint would then paint a second prompt below the
        // frame instead of over it. So the frame is measured before it is
        // painted and the parts that grow — the menu, the suggestion — get only
        // the rows that are left. A terminal that reports no size gets the
        // configured maximum.
        let screen_rows = usize::from(size.rows);
        let mut used = usize::from(frame.height(columns, self.color));
        if screen_rows > 0 && used > screen_rows && frame.ghost.is_some() {
            // The suggestion is decoration; keeping the line on screen is not.
            frame.ghost = None;
            used = usize::from(frame.height(columns, self.color));
        }
        let max_rows = match screen_rows {
            0 => self.max_menu_rows,
            rows => self.max_menu_rows.min(rows.saturating_sub(used)),
        };
        let rows = match max_rows {
            0 => None,
            max_rows => self.menu.as_mut().map(|menu| menu.rows(max_rows, columns)),
        };
        frame.menu = rows.as_deref().map(|rows| MenuView { rows });
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

/// Apply shift to a character a terminal reported unshifted.
///
/// The kitty protocol always reports the unshifted code point, so a
/// shift-only `CSI 97 ; 2 u` means `A`. Only ASCII has a case mapping the
/// editor can apply without knowing the keyboard layout; anything else is
/// inserted as the terminal reported it.
fn shifted(character: char, mods: Modifiers) -> char {
    if mods.contains(Modifiers::SHIFT) && character.is_ascii_lowercase() {
        character.to_ascii_uppercase()
    } else {
        character
    }
}

/// Whether `key` submits the line.
///
/// The same arm as [`Editor::handle_with`]'s `(Code::Enter, Modifiers::NONE)`,
/// with the lock modifiers folded out, so a key that waits for a completion
/// takes the path it would have taken if it had arrived later.
fn submits(key: &Key) -> bool {
    matches!(&key.code, Code::Enter) && key.mods.binding() == Modifiers::NONE
}

/// Whether `key` extends a kill sequence rather than ending one.
fn is_kill(key: &Key) -> bool {
    matches!(
        (&key.code, key.mods.binding()),
        (Code::Char('k'), Modifiers::CTRL)
            | (Code::Char('u'), Modifiers::CTRL)
            | (Code::Char('w'), Modifiers::CTRL)
            | (Code::Char('d'), Modifiers::ALT)
            | (Code::Backspace, Modifiers::ALT)
            | (Code::Delete, Modifiers::CTRL)
    )
}

/// Whether `key` changes the line, and so may change the ghost text.
fn edits_line(key: &Key) -> bool {
    if matches!(&key.code, Code::Char(_)) && key.mods.is_text() {
        return true;
    }
    matches!(
        (&key.code, key.mods.binding()),
        (Code::Byte(_), _)
            | (Code::Paste(_), _)
            | (Code::Backspace, _)
            | (Code::Delete, _)
            | (Code::Up, Modifiers::NONE)
            | (Code::Down, Modifiers::NONE)
            | (
                Code::Char('d' | 'k' | 'n' | 'p' | 't' | 'u' | 'w' | 'y'),
                Modifiers::CTRL
            )
    )
}
