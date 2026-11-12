//! Shared test scaffolding: a fake terminal, a stub completer, a stub parser,
//! and a screen model for what the frames add up to.
//!
//! Every behavioural test in this crate drives these instead of a real
//! terminal. `baash-sys` has no pty helper yet, so a fake is the only way to run
//! the editor deterministically; the `Terminal` trait is the seam a pty harness
//! will implement later.

#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::rc::Rc;

use baash_complete::{Candidate, Completer, CompletionEngine, Menu};
use baash_repl::{
    Config, Editor, History, NeverNeedsMore, Prompt, ReadResult, Size, Syntax, Terminal,
};

/// A terminal whose input is a fixed script and whose output is recorded.
pub struct FakeTerminal {
    input: VecDeque<u8>,
    output: Rc<RefCell<Vec<u8>>>,
    pub size: Size,
    pub raw_entries: usize,
    pub raw_leaves: usize,
    raw: bool,
}

impl FakeTerminal {
    /// A terminal with the given input script and an 80×24 size.
    pub fn new(input: &[u8]) -> Self {
        Self::sized(input, 80, 24)
    }

    /// A terminal with an explicit size.
    pub fn sized(input: &[u8], columns: u16, rows: u16) -> Self {
        Self {
            input: input.iter().copied().collect(),
            output: Rc::new(RefCell::new(Vec::new())),
            size: Size { columns, rows },
            raw_entries: 0,
            raw_leaves: 0,
            raw: false,
        }
    }

    /// A handle to the output buffer that outlives the editor's borrow.
    pub fn output_handle(&self) -> Rc<RefCell<Vec<u8>>> {
        Rc::clone(&self.output)
    }

    /// Everything written so far, lossily decoded.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.output.borrow()).into_owned()
    }

    /// Take everything written since the last call.
    pub fn take_output(&self) -> String {
        let bytes = std::mem::take(&mut *self.output.borrow_mut());
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Whether raw mode was left as often as it was entered.
    pub fn raw_balanced(&self) -> bool {
        self.raw_entries == self.raw_leaves && !self.raw
    }
}

impl Terminal for FakeTerminal {
    fn read_byte(&mut self) -> io::Result<Option<u8>> {
        Ok(self.input.pop_front())
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.output.borrow_mut().extend_from_slice(bytes);
        Ok(())
    }

    fn size(&self) -> Size {
        self.size
    }

    fn read_byte_timeout(&mut self, _timeout: std::time::Duration) -> io::Result<Option<u8>> {
        self.read_byte()
    }

    fn reports_cursor_position(&self) -> bool {
        true
    }

    fn enter_raw(&mut self) -> io::Result<()> {
        self.raw_entries += 1;
        self.raw = true;
        Ok(())
    }

    fn leave_raw(&mut self) -> io::Result<()> {
        self.raw_leaves += 1;
        self.raw = false;
        Ok(())
    }
}

/// A completer that returns a fixed candidate list through the real context
/// and insertion machinery.
///
/// The context (which span is replaced, how to quote an insertion) comes from
/// `baash-complete`; only the candidate list is scripted.
pub struct StubCompleter {
    engine: CompletionEngine,
    candidates: Vec<Candidate>,
}

impl StubCompleter {
    /// A completer offering `candidates` for any word.
    pub fn new(candidates: Vec<Candidate>) -> Self {
        Self {
            engine: CompletionEngine::new(),
            candidates,
        }
    }

    /// A completer offering nothing.
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// Candidates from plain display text, as commands.
    pub fn words(words: &[&str]) -> Self {
        Self::new(
            words
                .iter()
                .map(|word| Candidate::new(*word, baash_complete::CandKind::Command))
                .collect(),
        )
    }
}

impl Completer for StubCompleter {
    fn complete(&self, line: &[u8], cursor: usize) -> Menu {
        let context = self.engine.context(line, cursor);
        Menu::new(context, self.candidates.clone())
    }
}

/// A "is this buffer incomplete?" predicate.
pub type NeedsMore = Box<dyn Fn(&[u8]) -> bool>;

/// A parser whose "needs more input" answer is a closure.
pub struct StubSyntax(pub NeedsMore);

impl StubSyntax {
    /// A parser that continues while `predicate` holds.
    pub fn new(predicate: impl Fn(&[u8]) -> bool + 'static) -> Self {
        Self(Box::new(predicate))
    }
}

impl Syntax for StubSyntax {
    fn needs_more_input(&self, buffer: &[u8]) -> bool {
        (self.0)(buffer)
    }
}

/// A history ring seeded with `entries`, oldest first.
pub fn seeded_history(entries: &[&str]) -> History {
    let mut history = History::new();
    for entry in entries {
        history.push(entry.as_bytes());
    }
    history
}

/// [`plain_config`] with a seeded history ring.
pub fn config_with_history(entries: &[&str]) -> Config {
    Config {
        history: seeded_history(entries),
        ..plain_config()
    }
}

/// Drive `keys` through the editor with a seeded ring; returns the buffer, the
/// cursor and everything painted.
pub fn drive(entries: &[&str], keys: &[baash_repl::Key]) -> (Vec<u8>, usize, String) {
    let mut term = FakeTerminal::sized(b"", 80, 24);
    let handle = term.output_handle();
    let completer = StubCompleter::empty();
    let config = config_with_history(entries);
    let mut editor = Editor::with_config(&mut term, &completer, &NeverNeedsMore, config);
    for key in keys {
        editor.step(key.clone()).expect("step");
    }
    (
        editor.line().buffer().to_vec(),
        editor.line().cursor(),
        output_text(&handle),
    )
}

/// A configuration with colour and ghost text off, and the default prompt.
pub fn plain_config() -> Config {
    Config {
        color: false,
        prompt: Prompt::default(),
        ..Config::default()
    }
}

/// Drive one `read_line` and hand the terminal back for inspection.
pub fn run(
    term: FakeTerminal,
    completer: &dyn Completer,
    syntax: &dyn Syntax,
    config: Config,
) -> (ReadResult, FakeTerminal) {
    let mut term = term;
    let result = {
        let mut editor = Editor::with_config(&mut term, completer, syntax, config);
        editor.read_line().expect("read_line")
    };
    (result, term)
}

/// Drive `read_line` until end of input, collecting results and history.
pub struct Session {
    pub results: Vec<ReadResult>,
    pub term: FakeTerminal,
    pub history: Vec<Vec<u8>>,
}

/// Run a multi-line session.
pub fn session(
    term: FakeTerminal,
    completer: &dyn Completer,
    syntax: &dyn Syntax,
    config: Config,
    max_lines: usize,
) -> Session {
    let mut term = term;
    let mut results = Vec::new();
    let history;
    {
        let mut editor = Editor::with_config(&mut term, completer, syntax, config);
        for _ in 0..max_lines {
            let result = editor.read_line().expect("read_line");
            let stop = matches!(result, ReadResult::Eof | ReadResult::Interrupted);
            results.push(result);
            if stop {
                break;
            }
        }
        history = editor.history().commands().map(<[u8]>::to_vec).collect();
    }
    Session {
        results,
        term,
        history,
    }
}

/// Apply `keys` one at a time and report the buffer and cursor.
pub fn apply_keys(keys: &[baash_repl::Key]) -> (Vec<u8>, usize) {
    let mut term = FakeTerminal::new(b"");
    let completer = StubCompleter::empty();
    let mut editor = Editor::new(&mut term, &completer, &NeverNeedsMore);
    for key in keys {
        editor.step(key.clone()).expect("step");
    }
    (editor.line().buffer().to_vec(), editor.line().cursor())
}

/// Apply `keys`, returning the bytes written by the *last* frame only.
pub fn last_frame(keys: &[baash_repl::Key], columns: u16) -> String {
    let mut term = FakeTerminal::sized(b"", columns, 24);
    let sink = term.output_handle();
    let completer = StubCompleter::empty();
    let mut editor = Editor::new(&mut term, &completer, &NeverNeedsMore);
    for (index, key) in keys.iter().enumerate() {
        if index + 1 == keys.len() {
            clear_output(&sink);
        }
        editor.step(key.clone()).expect("step");
    }
    output_text(&sink)
}

/// Build a list of character keys from ASCII text.
pub fn chars(text: &str) -> Vec<baash_repl::Key> {
    text.chars().map(baash_repl::Key::Char).collect()
}

/// Decode an output handle that outlives the editor's borrow.
pub fn output_text(handle: &Rc<RefCell<Vec<u8>>>) -> String {
    String::from_utf8_lossy(&handle.borrow()).into_owned()
}

/// Empty an output handle.
pub fn clear_output(handle: &Rc<RefCell<Vec<u8>>>) {
    handle.borrow_mut().clear();
}

/// Whether a string contains an SGR sequence (a terminal styling escape).
pub fn has_sgr(text: &str) -> bool {
    text.match_indices("\x1b[").any(|(at, _)| {
        text[at..].find('m').is_some_and(|end| {
            let between = &text[at + 2..at + end];
            !between.is_empty()
                && between
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte == b';')
        })
    })
}

/// A terminal screen, as the user would see it.
///
/// Every frame is a blob of movement and repainting, so asserting on the bytes
/// says little about whether the editor behaved. This replays them the way a
/// terminal does — `\r`, `\n` with a scroll at the bottom row, CSI `A`/`B`/
/// `C`/`D`/`H`/`J`, SGR ignored — and keeps the resulting grid and cursor.
///
/// Every printable character is one cell. The editor itself measures with
/// `unicode-width` (see `display.rs`); the tests here type ASCII, so the model
/// does not repeat that.
pub struct Screen {
    columns: usize,
    grid: Vec<Vec<char>>,
    row: usize,
    col: usize,
    scrolls: usize,
}

impl Screen {
    /// An empty screen of the given size.
    pub fn new(columns: u16, rows: u16) -> Self {
        let columns = usize::from(columns.max(1));
        let rows = usize::from(rows.max(1));
        Self {
            columns,
            grid: vec![vec![' '; columns]; rows],
            row: 0,
            col: 0,
            scrolls: 0,
        }
    }

    /// The screen a [`FakeTerminal`] painted.
    pub fn of(term: &FakeTerminal) -> Self {
        let mut screen = Self::new(term.size.columns, term.size.rows);
        let bytes = term.output.borrow();
        screen.feed(&bytes);
        screen
    }

    /// Replay terminal output.
    pub fn feed(&mut self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes).into_owned();
        let mut chars = text.chars().peekable();
        while let Some(character) = chars.next() {
            match character {
                '\x1b' => {
                    if chars.peek() == Some(&'[') {
                        chars.next();
                    }
                    let mut parameters = String::new();
                    let mut final_byte = None;
                    for character in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&character) {
                            final_byte = Some(character);
                            break;
                        }
                        parameters.push(character);
                    }
                    self.control(final_byte, &parameters);
                }
                '\r' => self.col = 0,
                '\n' => self.line_feed(),
                character if (character as u32) < 0x20 || character == '\x7f' => {}
                character => self.put(character),
            }
        }
    }

    /// The text of one row, without trailing blanks.
    pub fn row(&self, row: usize) -> String {
        let text: String = self.grid[row].iter().collect();
        text.trim_end().to_string()
    }

    /// Every row, top to bottom, without trailing blanks.
    pub fn lines(&self) -> Vec<String> {
        (0..self.grid.len()).map(|row| self.row(row)).collect()
    }

    /// The rows that have anything on them, top to bottom.
    pub fn used_lines(&self) -> Vec<String> {
        self.lines()
            .into_iter()
            .filter(|line| !line.is_empty())
            .collect()
    }

    /// The cursor as a `(row, column)` cell.
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    /// How many rows scrolled off the top.
    pub fn scrolls(&self) -> usize {
        self.scrolls
    }

    /// How many times `needle` appears anywhere on the screen.
    pub fn occurrences(&self, needle: &str) -> usize {
        self.lines()
            .iter()
            .map(|line| line.matches(needle).count())
            .sum()
    }

    fn control(&mut self, final_byte: Option<char>, parameters: &str) {
        let counts: Vec<usize> = parameters
            .split(';')
            .map(|part| part.trim_start_matches('?').parse().unwrap_or(0))
            .collect();
        let count = |index: usize| match counts.get(index).copied().unwrap_or(0) {
            0 => 1,
            value => value,
        };
        match final_byte {
            Some('A') => self.row = self.row.saturating_sub(count(0)),
            Some('B') => self.row = (self.row + count(0)).min(self.grid.len() - 1),
            Some('C') => self.col = (self.col + count(0)).min(self.columns - 1),
            Some('D') => self.col = self.col.saturating_sub(count(0)),
            Some('H') => {
                self.row = counts.first().copied().unwrap_or(0).saturating_sub(1);
                self.col = counts.get(1).copied().unwrap_or(0).saturating_sub(1);
            }
            Some('J') => match counts.first().copied().unwrap_or(0) {
                2 => {
                    for row in &mut self.grid {
                        row.fill(' ');
                    }
                }
                _ => {
                    for cell in &mut self.grid[self.row][self.col..] {
                        *cell = ' ';
                    }
                    for row in &mut self.grid[self.row + 1..] {
                        row.fill(' ');
                    }
                }
            },
            // SGR and anything the renderer does not emit are ignored: they
            // change no cell and move no cursor.
            _ => {}
        }
    }

    fn line_feed(&mut self) {
        if self.row + 1 == self.grid.len() {
            self.grid.remove(0);
            self.grid.push(vec![' '; self.columns]);
            self.scrolls += 1;
        } else {
            self.row += 1;
        }
    }

    fn put(&mut self, character: char) {
        // A cell printed in the last column leaves the terminal in a pending
        // wrap state: the *next* character starts a new row.
        if self.col >= self.columns {
            self.col = 0;
            self.line_feed();
        }
        self.grid[self.row][self.col] = character;
        self.col += 1;
    }
}
