//! Shared helpers for the baash-syntax integration tests.

#![allow(dead_code)]

use baash_diag::{Diagnostic, SourceFile, Span};
use baash_syntax::{Ast, Expect, NodeKind, ParseOpts, ParseOutcome};

pub fn parse(src: &[u8]) -> ParseOutcome {
    baash_syntax::parse_bytes(src, &ParseOpts::DEFAULT)
}

pub fn parse_posix(src: &[u8]) -> ParseOutcome {
    baash_syntax::parse_bytes(src, &ParseOpts::POSIX)
}

/// Parse, asserting success.
pub fn ast(src: &[u8]) -> Ast {
    match parse(src) {
        ParseOutcome::Ok(a) => a,
        ParseOutcome::NeedMoreInput { open, expected } => {
            panic!(
                "expected {:?} to parse, got NeedMoreInput open={open:?} expected={expected:?}",
                String::from_utf8_lossy(src)
            )
        }
        ParseOutcome::Failed(ds) => panic!(
            "expected {:?} to parse, got failures: {:?}",
            String::from_utf8_lossy(src),
            ds.iter()
                .map(|d| d.render_line(&SourceFile::new("t", src)))
                .collect::<Vec<_>>()
        ),
    }
}

/// Parse, asserting the input is merely incomplete.
pub fn more(src: &[u8]) -> (Span, Vec<Expect>) {
    match parse(src) {
        ParseOutcome::NeedMoreInput { open, expected } => (open, expected),
        ParseOutcome::Ok(_) => panic!(
            "expected {:?} to need more input, but it parsed",
            String::from_utf8_lossy(src)
        ),
        ParseOutcome::Failed(ds) => panic!(
            "expected {:?} to need more input, got failures: {:?}",
            String::from_utf8_lossy(src),
            ds.iter()
                .map(|d| d.render_line(&SourceFile::new("t", src)))
                .collect::<Vec<_>>()
        ),
    }
}

/// Parse, asserting failure.
pub fn fail(src: &[u8]) -> Vec<Diagnostic> {
    match parse(src) {
        ParseOutcome::Failed(ds) => ds,
        ParseOutcome::Ok(_) => panic!(
            "expected {:?} to fail, but it parsed",
            String::from_utf8_lossy(src)
        ),
        ParseOutcome::NeedMoreInput { open, expected } => panic!(
            "expected {:?} to fail, got NeedMoreInput open={open:?} expected={expected:?}",
            String::from_utf8_lossy(src)
        ),
    }
}

/// Render one diagnostic in the "full report" style the tests pin: the
/// greppable location line, then every secondary label, then the help text.
pub fn render(d: &Diagnostic, name: &str, src: &[u8]) -> String {
    let file = SourceFile::new(name, src.to_vec());
    let mut out = d.render_line(&file);
    for label in &d.labels {
        let (line, col) = file.line_col(label.span.start);
        out.push_str(&format!(
            "\n  --> {}:{line}:{col}: {}",
            file.name(),
            label.message
        ));
    }
    if let Some(help) = &d.help {
        out.push_str(&format!("\n  help: {help}"));
    }
    out
}

pub fn has_node(ast: &Ast, pred: impl Fn(&NodeKind) -> bool) -> bool {
    ast.nodes.iter().any(|n| pred(&n.kind))
}

pub fn find_node(ast: &Ast, pred: impl Fn(&NodeKind) -> bool) -> Option<&NodeKind> {
    ast.nodes.iter().find(|n| pred(&n.kind)).map(|n| &n.kind)
}

/// The text of a word's span.
/// Descend to the first command of the program: root list -> and-or -> command.
pub fn first_command(ast: &Ast) -> baash_syntax::NodeId {
    let NodeKind::List { items } = &ast.node(ast.root).kind else {
        panic!("root is not a list");
    };
    let item = items.first().expect("empty program").node;
    match &ast.node(item).kind {
        NodeKind::AndOr { first, .. } => *first,
        _ => item,
    }
}

pub fn word_text<'a>(ast: &'a Ast, src: &'a [u8], w: &baash_syntax::Word) -> &'a [u8] {
    let _ = ast;
    &src[w.span.clone()]
}

/// A tiny deterministic PRNG, so a failure is reproducible.
pub struct Lcg(pub u64);

impl Lcg {
    pub fn next_u32(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
    pub fn byte(&mut self) -> u8 {
        (self.next_u32() & 0xff) as u8
    }
}
