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
//! # The history picker (`Ctrl-R`)
//!
//! `Ctrl-R` opens the full-screen fuzzy picker from `baash-picker`: the query
//! on the top row, ranked matches under it, a count at the bottom, and the
//! terminal's alternate screen for as long as it is up. `Enter` (or `Tab`)
//! *accepts* the selected command onto the line for editing; it never runs it,
//! so an accidental `Enter` costs nothing. `Esc`/`Ctrl-G` leave the line as it
//! was, `Ctrl-C` abandons it.
//!
//! The picker paints its own screen: while one is open, [`Editor::draw`] hands
//! the terminal to it and the frame renderer stays out of the way. Leaving the
//! alternate screen puts the primary one back exactly as it was, so the frame
//! the editor last painted is still there and needs no re-anchoring.
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
use baash_picker::{Action, Outcome, Picker};

use baash_runtime::{History, PICKER_DEPTH};

use crate::completion::MenuState;
use crate::key::{Code, Decoder, Key, Modifiers};
use crate::line::Line;
use crate::paste::{normalise, quoted};
use crate::prompt::Prompt;
use crate::render::{Frame, MenuView, Renderer};
use crate::term::{Input, Terminal};
use crate::undo::{EditKind, Undo};

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
    /// Whether a pasted word is quoted when the shell would read it as syntax.
    ///
    /// On by default: pasting `https://…/watch?v=abc&t=10` must not run
    /// `https://…/watch?v=abc` in the background. The rule is deliberately
    /// narrow ([`crate::paste`]), and the first `Backspace` after a quoted
    /// paste removes the quote pair, so a wrong guess costs one keystroke.
    pub paste_quoting: bool,
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
            paste_quoting: true,
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
    /// A line to put in the buffer for the next read, used by `histverify`
    /// after history expansion.
    initial: Option<Vec<u8>>,
    /// The history picker, while it owns the screen.
    picker: Option<Picker>,
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
    /// Whether a pasted word that the shell would read as syntax is quoted
    /// (see [`Config::paste_quoting`]).
    paste_quoting: bool,
    /// The undo history for the line being edited.
    undo: Undo,
    /// Whether the newlines in the buffer came from a paste.
    ///
    /// They make the buffer a multiline block whose inner rows carry the
    /// continuation prompt. A recalled history entry that spans lines is *not*
    /// one: readline paints those as plain rows, and the block structure is
    /// already visible without a `PS2` in front of every line.
    pasted_block: bool,
    /// Where the quotes of the last auto-quoted paste are, while they can
    /// still be taken off as a pair by `Backspace`.
    ///
    /// Cleared by any other key: the offsets are only meaningful while the
    /// buffer is exactly what the paste left, and a `Backspace` that removes
    /// both quotes is the escape hatch that makes the guess cheap.
    paste_quotes: Option<PasteQuotes>,
}

/// The two quote bytes an auto-quoted paste was wrapped in.
#[derive(Clone, Copy, Debug)]
struct PasteQuotes {
    /// The offset of the opening quote.
    open: usize,
    /// The offset of the closing quote.
    close: usize,
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
            picker: None,
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
            paste_quoting: config.paste_quoting,
            undo: Undo::default(),
            pasted_block: false,
            paste_quotes: None,
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
        // A paste is its own undo step; a typed character may join the run
        // before it (`undo.rs`). Anything else is a step of its own, and a key
        // that did not touch the text records nothing at all.
        let kind = match key.code {
            Code::Paste(_) => EditKind::Paste,
            Code::Char(_) | Code::Byte(_) => EditKind::Typing,
            _ => EditKind::Other,
        };
        let flow = self.handle_with(key, shell)?;
        if flow == Flow::Continue {
            self.record_edit(kind);
            self.draw()?;
        }
        Ok(flow)
    }

    /// Note the line's new state in the undo history.
    ///
    /// `kind` decides whether the edit joins the previous step; a key that did
    /// not change the text is not a step. Any key also retires the "the last
    /// thing that happened was a quoted paste" marker, since a later
    /// `Backspace` must then be an ordinary one.
    fn record_edit(&mut self, kind: EditKind) {
        if kind != EditKind::Paste {
            self.paste_quotes = None;
        }
        self.undo.record(&self.line, kind);
    }

    /// Undo the last edit of the line being typed.
    fn undo(&mut self) {
        self.paste_quotes = None;
        self.menu = None;
        if self.undo.undo(&mut self.line) {
            self.dirty = true;
        }
    }

    /// Redo the edit the last undo took back.
    fn redo(&mut self) {
        self.paste_quotes = None;
        self.menu = None;
        if self.undo.redo(&mut self.line) {
            self.dirty = true;
        }
    }

    /// Insert a bracketed paste, quoting it when the shell would read it as
    /// syntax rather than as the data it is.
    ///
    /// The quote context comes from the completer, which is the one thing here
    /// that has a lexer; the policy is [`crate::paste`]. A paste that is
    /// wrapped in quotes is remembered so that one `Backspace` can take the
    /// pair off again.
    fn insert_paste(&mut self, bytes: &[u8]) {
        // Line endings first: a browser or a Windows editor pastes `\r\n`, and
        // a carriage return that reaches the buffer is a `^M` inside the
        // argument rather than a line break.
        let text = normalise(bytes);
        if text.is_empty() {
            return;
        }
        // A block is commands, never one argument, and it is shown as the
        // multiline edit it is: the user presses Enter to run it, and nothing
        // in the paste submits anything.
        if text.contains(&b'\n') {
            self.pasted_block = true;
            self.line.insert(&text);
            return;
        }
        let at = self.line.cursor();
        let quoted = self
            .paste_quoting
            .then(|| quoted(&text, self.completer.quote_context(self.line.buffer(), at)))
            .flatten();
        let Some(quoted) = quoted else {
            self.line.insert(&text);
            return;
        };
        if quoted.wrapped {
            // Only a pair this editor added can be taken off again; an escape
            // made inside the user's own `'…'` has no pair of its own.
            self.paste_quotes = Some(PasteQuotes {
                open: at,
                close: at + quoted.text.len() - 1,
            });
        }
        self.line.insert(&quoted.text);
    }

    /// Take the quotes off the last auto-quoted paste, if they are still there.
    ///
    /// Returns `false` when this `Backspace` is an ordinary one. The check is
    /// structural rather than a flag: a buffer that no longer holds a quote at
    /// each recorded offset (because something was typed, killed or undone) is
    /// not the paste any more, and a stale marker must not eat bytes.
    fn unquote_paste(&mut self) -> bool {
        let Some(PasteQuotes { open, close }) = self.paste_quotes.take() else {
            return false;
        };
        let buffer = self.line.buffer();
        if open >= close
            || close >= buffer.len()
            || buffer[open] != b'\''
            || buffer[close] != b'\''
            || self.line.cursor() != close + 1
        {
            return false;
        }
        // Remove the closing quote, then the opening one, and leave the cursor
        // after the (now bare) pasted text.
        self.line.set_cursor(close);
        self.line.delete();
        self.line.set_cursor(open);
        self.line.delete();
        self.line.set_cursor(close - 1);
        true
    }

    /// Reset per-line state and paint the first prompt.
    fn begin(&mut self) {
        self.prompt_row = None;
        self.line.reset();
        if let Some(initial) = self.initial.take() {
            self.line.set_text(&initial);
        }
        self.menu = None;
        self.picker = None;
        self.prefix_tab = false;
        self.suggestion_dismissed = false;
        self.history.reset_navigation();
        self.secondary = false;
        self.completion_pending = false;
        self.deferred_key = None;
        self.deferred_deadline = None;
        // A new line starts a new history: `Ctrl-Z` must never reach back into
        // the command before this one, and the pasted-quote offsets belong to
        // the old buffer.
        self.undo.reset(&self.line);
        self.pasted_block = false;
        self.paste_quotes = None;
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
        if self.picker.is_some() {
            return self.handle_picker(key);
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
            (Code::Paste(bytes), _) => self.insert_paste(&bytes),
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
                // The first Backspace after an auto-quoted paste takes the
                // quotes off again, leaving the pasted text: the paste was
                // guessed at, and undoing the guess must cost one keystroke.
                if !self.unquote_paste() {
                    self.line.backspace();
                }
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
            (Code::Char('r'), Modifiers::CTRL) => self.open_picker()?,
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
            // Undo. On Windows the console has no end-of-file byte, so an
            // empty line still spends `Ctrl-Z` on leaving the session; with
            // text on the line it undoes, which is what every other platform
            // does with both spellings (`Ctrl-Z` and readline's `Ctrl-_`).
            (Code::Char('z'), Modifiers::CTRL) => {
                #[cfg(windows)]
                if self.line.buffer().is_empty() && !self.line.in_continuation() {
                    return Ok(Flow::Eof);
                }
                self.undo();
            }
            (Code::Char('_'), Modifiers::CTRL) => self.undo(),
            // `Ctrl-Shift-Z` needs a guard: in a pattern `|` is alternation, so
            // `CTRL | SHIFT` would mean either one (`redo` by `Ctrl-Z`, which
            // the arm above already handles).
            (Code::Char('z'), mods) if mods == Modifiers::CTRL | Modifiers::SHIFT => self.redo(),
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
                // `print_above` erases the frame, so the line has to be drawn
                // again whatever is offered — and `offer` clears the flag when
                // there is nothing to offer, which is only right when the line
                // is still on screen.
                let painted = !shell_menu.output.is_empty();
                if painted {
                    self.print_above(&shell_menu.output)?;
                }
                self.dirty = true;
                let offered = self.offer(shell_menu.menu);
                if painted {
                    self.dirty = true;
                }
                offered
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
            // A menu that only opened shows nothing new in the text; one that
            // inserted a candidate is an undo step of its own.
            self.record_edit(EditKind::Other);
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

    /// `Ctrl-R`: pick an earlier command on the full screen.
    ///
    /// The candidates are the history's *pool* — the ring plus older rows from
    /// the database, de-duplicated, newest first ([`History::pool`]) — because
    /// "the command I ran three weeks ago" is what a history search is for, and
    /// a session ring does not hold it.
    ///
    /// A continuation line never opens the picker: its earlier lines are
    /// already scrollback, and the picker would have to be closed again to get
    /// back to them.
    fn open_picker(&mut self) -> io::Result<()> {
        if self.picker.is_some() || self.line.in_continuation() {
            return Ok(());
        }
        let lines: Vec<Vec<u8>> = self
            .history
            .pool(PICKER_DEPTH)
            .iter()
            .map(|entry| entry.text().to_vec())
            .collect();
        // The alternate screen comes first: it starts blank, so a frame
        // painted before the switch would land on the primary screen and be
        // lost.
        self.term.write_all(baash_picker::ENTER_SCREEN)?;
        let mut picker = Picker::new(b"history> ", lines);
        let size = self.term.size();
        let frame = picker.frame(
            size.columns_or_fallback(),
            size.rows_or_fallback(),
            self.color,
        );
        if let Err(error) = self.term.write_all(&frame) {
            // The switch already happened, so a failure here must not leave the
            // terminal on a screen nothing is going to paint.
            let _ = self.term.write_all(baash_picker::LEAVE_SCREEN);
            return Err(error);
        }
        self.picker = Some(picker);
        // The line still holds what the user had typed; `Esc` only has to put
        // the screen back, not the buffer.
        self.dirty = false;
        Ok(())
    }

    /// Give the screen back to the frame renderer.
    ///
    /// Nothing else to undo: the alternate screen holds every byte the picker
    /// painted, and the primary screen behind it — the frame on it included —
    /// is exactly as it was left.
    fn close_picker(&mut self) -> io::Result<()> {
        if self.picker.take().is_some() {
            self.term.write_all(baash_picker::LEAVE_SCREEN)?;
        }
        self.dirty = true;
        Ok(())
    }

    /// Paint the picker's screen.
    fn paint_picker(&mut self) -> io::Result<()> {
        let size = self.term.size();
        let Some(picker) = self.picker.as_mut() else {
            return Ok(());
        };
        let frame = picker.frame(
            size.columns_or_fallback(),
            size.rows_or_fallback(),
            self.color,
        );
        self.term.write_all(&frame)
    }

    /// One key while the picker owns the screen.
    ///
    /// The keys mean what `baash_picker::decode` says they mean — that module
    /// is where a screen-owning builtin decodes the same set — so this is only
    /// the translation from the line editor's richer key type.
    fn handle_picker(&mut self, key: Key) -> io::Result<Flow> {
        let (code, mods) = (key.code, key.mods.binding());
        let action = match (code, mods) {
            // Ctrl-C abandons the whole line, as it does at the prompt: the
            // picker is a mode of the line, not a program of its own.
            (Code::Char('c'), Modifiers::CTRL) => {
                self.close_picker()?;
                return Ok(Flow::Interrupt);
            }
            // `Tab` accepts as well as `Enter`: the completion menu's `Tab`
            // accepts a selection, and this is the same kind of list.
            (Code::Enter | Code::Tab, Modifiers::NONE) => Action::Accept,
            (Code::Esc, _) | (Code::Char('g'), Modifiers::CTRL) => Action::Cancel,
            (Code::Backspace, Modifiers::NONE) => Action::Backspace,
            (Code::Char('w'), Modifiers::CTRL) => Action::KillWord,
            (Code::Char('u'), Modifiers::CTRL) => Action::Clear,
            (Code::Up, Modifiers::NONE) | (Code::Char('p'), Modifiers::CTRL) => Action::Up,
            (Code::Down, Modifiers::NONE)
            | (Code::Char('n'), Modifiers::CTRL)
            | (Code::Char('r'), Modifiers::CTRL) => Action::Down,
            (Code::PageUp, Modifiers::NONE) | (Code::Char('b'), Modifiers::CTRL) => Action::PageUp,
            (Code::PageDown, Modifiers::NONE) | (Code::Char('f'), Modifiers::CTRL) => {
                Action::PageDown
            }
            (Code::Home, Modifiers::NONE) => Action::First,
            (Code::End, Modifiers::NONE) => Action::Last,
            (Code::Paste(bytes), _) => Action::Insert(bytes),
            (Code::Char(character), mods) if mods.is_text() => {
                Action::Insert(shifted(character, mods).to_string().into_bytes())
            }
            (Code::Byte(byte), _) => Action::Insert(vec![byte]),
            // Another window may have painted over this one while the picker
            // sat idle, so gaining focus repaints; losing it changes nothing.
            (Code::FocusIn, _) => {
                self.dirty = true;
                return self.draw().map(|()| Flow::Continue);
            }
            // Everything else is not a gesture the picker has: a mouse click,
            // a motion key, a release report.
            _ => return Ok(Flow::Continue),
        };
        let Some(picker) = self.picker.as_mut() else {
            return Ok(Flow::Continue);
        };
        let outcome = picker.act(action);
        let chosen = picker.selected().map(<[u8]>::to_vec);
        match outcome {
            Outcome::Continue => self.dirty = true,
            Outcome::Accept => {
                self.close_picker()?;
                if let Some(text) = chosen {
                    // One undo step for the whole replacement: `Ctrl-Z` after
                    // accepting puts back the line the picker was opened from.
                    self.record_edit(EditKind::Other);
                    self.line.set_text(&text);
                }
            }
            Outcome::Cancel => self.close_picker()?,
        }
        self.draw()?;
        Ok(Flow::Continue)
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
        if self.picker.is_some() {
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
        if self.picker.is_some() {
            return self.paint_picker();
        }
        let prompt = if self.secondary {
            self.prompt.secondary_text()
        } else {
            self.prompt.primary_text()
        };
        let size = self.term.size();
        let columns = size.columns_or_fallback();
        let ghost = if self.menu.is_some() {
            None
        } else {
            self.suggestion()
        };
        let (hint, bad) = self.check();
        // A buffer holding newlines but no committed prefix is a pasted block:
        // its inner rows get the continuation prompt, so the block reads as the
        // multiline edit it is. A typed continuation has already committed its
        // earlier lines, which leaves `editable()` single-line and `None` here.
        let continuation = (self.pasted_block && self.line.editable().contains(&b'\n'))
            .then(|| self.prompt.secondary_text());
        let mut frame = Frame {
            prompt,
            line: self.line.editable(),
            cursor: self.line.editable_cursor(),
            menu: None,
            ghost: ghost.as_deref(),
            hint: hint.as_deref(),
            bad,
            continuation,
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
