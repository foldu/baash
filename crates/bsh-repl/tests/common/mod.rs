//! Shared test scaffolding: a fake terminal, a stub completer, a stub parser.
//!
//! Every behavioural test in this crate drives these instead of a real
//! terminal. `bsh-sys` has no pty helper yet, so a fake is the only way to run
//! the editor deterministically; the `Terminal` trait is the seam a pty harness
//! will implement later.

#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::rc::Rc;

use bsh_complete::{Candidate, Completer, CompletionEngine, Menu};
use bsh_repl::{Config, Editor, NeverNeedsMore, Prompt, ReadResult, Size, Syntax, Terminal};

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
/// `bsh-complete`; only the candidate list is scripted.
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
                .map(|word| Candidate::new(*word, bsh_complete::CandKind::Command))
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
        history = editor.history().entries().to_vec();
    }
    Session {
        results,
        term,
        history,
    }
}

/// Apply `keys` one at a time and report the buffer and cursor.
pub fn apply_keys(keys: &[bsh_repl::Key]) -> (Vec<u8>, usize) {
    let mut term = FakeTerminal::new(b"");
    let completer = StubCompleter::empty();
    let mut editor = Editor::new(&mut term, &completer, &NeverNeedsMore);
    for key in keys {
        editor.step(key.clone()).expect("step");
    }
    (editor.line().buffer().to_vec(), editor.line().cursor())
}

/// Apply `keys`, returning the bytes written by the *last* frame only.
pub fn last_frame(keys: &[bsh_repl::Key], columns: u16) -> String {
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
pub fn chars(text: &str) -> Vec<bsh_repl::Key> {
    text.chars().map(bsh_repl::Key::Char).collect()
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
