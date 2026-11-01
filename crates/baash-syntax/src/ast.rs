//! The syntax tree.
//!
//! The tree is deliberately **flat**: a command is a [`NodeId`] indexing
//! [`Ast::nodes`], and every child reference in a [`NodeKind`] is another
//! [`NodeId`]. There are no `Box`ed recursive nodes anywhere, so walking the
//! tree cannot overflow the stack on nested input deeper than the parser's
//! own limit.
//!
//! Words are the one place where a little nesting is unavoidable, and even
//! there it stays index-based: a [`Word`] owns a small `Vec<Part>`, never a
//! `Box`, and a command substitution inside a part points back into the same
//! flat node vector by [`NodeId`].
//!
//! Byte strings, never `str`: every literal is a [`Span`] into the original
//! source. Resolving a span to bytes is the consumer's job, because the
//! consumer is the thing that owns the source.

use crate::Token;
use crate::lexer::HereDoc;
use baash_diag::Span;

use baash_lang::Word;

/// Index of a node in [`Ast::nodes`].
pub type NodeId = u32;

/// A parsed chunk.
#[derive(Clone, Debug)]
pub struct Ast {
    /// The root node: normally a [`NodeKind::List`].
    pub root: NodeId,
    /// Every node in the chunk. Command substitutions keep their source text
    /// in the word model (`baash-lang` owns its execution), so there are no
    /// separate sub-trees here.
    pub nodes: Vec<Node>,
    /// Here-documents attached to redirections, indexed by
    /// [`Redirect::heredoc`].
    pub heredocs: Vec<HereDoc>,
    /// The token stream the tree was built from, kept for tooling and for the
    /// lossless/unparse contract.
    pub tokens: Vec<Token>,
}

impl Ast {
    /// The root node.
    pub fn root_node(&self) -> &Node {
        &self.nodes[self.root as usize]
    }

    /// Any node by id.
    ///
    /// # Panics
    ///
    /// Panics if `id` did not come from this `Ast`.
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    /// The children of a node, in source order.
    pub fn children(&self, id: NodeId) -> Vec<NodeId> {
        match &self.node(id).kind {
            NodeKind::Empty
            | NodeKind::Simple(_)
            | NodeKind::ArithCommand { .. }
            | NodeKind::TestCommand { .. } => Vec::new(),
            NodeKind::List { items } => items.iter().map(|i| i.node).collect(),
            NodeKind::AndOr { first, rest } => {
                let mut out = vec![*first];
                out.extend(rest.iter().map(|l| l.node));
                out
            }
            NodeKind::Pipeline { stages, .. } => stages.clone(),
            NodeKind::If {
                branches,
                else_body,
            } => {
                let mut out = Vec::new();
                for b in branches {
                    out.push(b.cond);
                    out.push(b.body);
                }
                if let Some(e) = else_body {
                    out.push(*e);
                }
                out
            }
            NodeKind::For { body, .. }
            | NodeKind::ForArith { body, .. }
            | NodeKind::BraceGroup { body }
            | NodeKind::Subshell { body }
            | NodeKind::Function { body, .. } => vec![*body],
            NodeKind::While { cond, body, .. } => vec![*cond, *body],
            NodeKind::Case { arms, .. } => arms.iter().map(|a| a.body).collect(),
            NodeKind::Redirected { body, .. } => vec![*body],
            NodeKind::Timed { body } => vec![*body],
        }
    }
}

/// One node: a kind and the source span it covers.
#[derive(Clone, Debug)]
pub struct Node {
    /// What the node is.
    pub kind: NodeKind,
    /// Byte range of the construct in the source.
    pub span: Span,
}

/// The kinds of node the grammar produces.
#[derive(Clone, Debug)]
pub enum NodeKind {
    /// Nothing: an empty command list, e.g. the body of `$( )`.
    Empty,
    /// A sequence of and-or lists separated by `;`, `&` or newlines.
    List {
        /// The items, in source order.
        items: Vec<ListItem>,
    },
    /// An `&&` / `||` chain.
    AndOr {
        /// The first pipeline.
        first: NodeId,
        /// Each following operator and pipeline.
        rest: Vec<AndOrLink>,
    },
    /// A pipeline, possibly negated with `!`.
    Pipeline {
        /// Whether `!` preceded the pipeline.
        negated: bool,
        /// The stages, in source order.
        stages: Vec<NodeId>,
        /// The operator between each pair of stages; one shorter than
        /// `stages`.
        links: Vec<PipeOp>,
    },
    /// A simple command: assignments, words and redirections.
    Simple(SimpleCommand),
    /// An `if` / `elif` chain with an optional `else`.
    If {
        /// The condition/body pairs, in order.
        branches: Vec<IfBranch>,
        /// The `else` branch, if any.
        else_body: Option<NodeId>,
    },
    /// A `for` loop.
    For {
        /// The loop variable.
        var: Word,
        /// The `in` list. `None` means no `in`: iterate over `"$@"`.
        /// `Some(vec![])` means `for x in; do`, which iterates zero times.
        in_list: Option<Vec<Word>>,
        /// The body.
        body: NodeId,
    },
    /// A bash `for (( … ))` loop.
    ForArith {
        /// The arithmetic header, excluding `((` and `))`.
        header: Span,
        /// The body.
        body: NodeId,
    },
    /// A `while` or `until` loop.
    While {
        /// `true` for `until`.
        until: bool,
        /// The condition list.
        cond: NodeId,
        /// The body.
        body: NodeId,
    },
    /// A `case` statement.
    Case {
        /// The word being matched.
        word: Word,
        /// The arms, in source order.
        arms: Vec<CaseArm>,
    },
    /// `{ …; }`.
    BraceGroup {
        /// The command list.
        body: NodeId,
    },
    /// `( … )`.
    Subshell {
        /// The command list.
        body: NodeId,
    },
    /// A function definition.
    Function {
        /// The function name.
        name: Word,
        /// The body: always a compound command.
        body: NodeId,
        /// `true` for the `function name` form, `false` for `name()`.
        keyword: bool,
    },
    /// A compound command with redirections attached, e.g. `{ …; } >out`.
    Redirected {
        /// The compound command.
        body: NodeId,
        /// The redirections, left to right.
        redirects: Vec<Redirect>,
    },
    /// A bash arithmetic command, `(( … ))`.
    ArithCommand {
        /// The expression, excluding `((` and `))`.
        expr: Span,
    },
    /// A bash `[[ … ]]` conditional.
    ///
    /// The body is kept as a span rather than a tree: `=~` regular expressions
    /// contain arbitrary bytes, and re-lexing them as shell tokens is exactly
    /// the mistake the span avoids. Parsing the expression belongs to the
    /// language layer.
    TestCommand {
        /// The condition, excluding `[[` and `]]`.
        body: Span,
    },
    /// A `time` prefix.
    Timed {
        /// The pipeline being timed.
        body: NodeId,
    },
}

/// One item of a [`NodeKind::List`].
#[derive(Clone, Debug)]
pub struct ListItem {
    /// The and-or list.
    pub node: NodeId,
    /// The separator that followed it, if any.
    pub sep: Option<Separator>,
}

/// How one list item was separated from the next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Separator {
    /// `;`
    Semi,
    /// `&`
    Amp,
    /// A newline.
    Newline,
}

/// One `&&` / `||` link.
#[derive(Clone, Debug)]
pub struct AndOrLink {
    /// The operator.
    pub op: AndOrOp,
    /// The right-hand pipeline.
    pub node: NodeId,
}

/// The operator between two and-or operands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AndOrOp {
    /// `&&`
    And,
    /// `||`
    Or,
}

/// The operator between two pipeline stages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipeOp {
    /// `|`
    Pipe,
    /// `|&`, which also sends stderr to the next stage.
    PipeStderr,
}

/// One `if`/`elif` branch.
#[derive(Clone, Debug)]
pub struct IfBranch {
    /// The condition list.
    pub cond: NodeId,
    /// The body list.
    pub body: NodeId,
}

/// One `case` arm.
#[derive(Clone, Debug)]
pub struct CaseArm {
    /// The patterns, joined by `|` in the source.
    pub patterns: Vec<Word>,
    /// The body list.
    pub body: NodeId,
    /// How the arm was terminated.
    pub term: CaseTerm,
}

/// How a `case` arm ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseTerm {
    /// `;;`
    Break,
    /// `;&`: run the next arm's body too.
    FallThrough,
    /// `;;&`: keep testing the next patterns.
    Continue,
    /// No terminator: the arm was the last one before `esac`.
    None,
}

/// A simple command.
#[derive(Clone, Debug)]
pub struct SimpleCommand {
    /// Variable assignments that precede the command word.
    pub assignments: Vec<Assignment>,
    /// The command word and its arguments.
    pub words: Vec<Word>,
    /// Redirections, in source order (which is observable).
    pub redirects: Vec<Redirect>,
    /// Array-literal arguments to a declaration builtin, e.g. `local x=(a b)`.
    ///
    /// `baash-lang` has no array word model yet, so the word at
    /// [`ArrayArg::word`] is a textual placeholder and the elements are kept
    /// here for the runtime's declaration builtins. See [`ArrayArg`].
    pub array_args: Vec<ArrayArg>,
}

/// An array-literal argument, `name=(…)`, in argument position.
///
/// Bash only accepts this after `local`, `declare`, `typeset`, `export`,
/// `readonly`, `alias` or `eval`; the parser enforces that. The corresponding
/// entry in [`SimpleCommand::words`] is a quoted literal holding the raw
/// `name=(…)` source so that ordinary word expansion leaves it alone, and the
/// structure lives here until arrays get a real word representation.
#[derive(Clone, Debug)]
pub struct ArrayArg {
    /// Index into [`SimpleCommand::words`] of the placeholder word.
    pub word: usize,
    /// The whole `name=(…)` span.
    pub span: Span,
    /// The variable name before `=`.
    pub name: Span,
    /// Whether this was `name+=(…)`.
    pub append: bool,
    /// The parsed elements.
    pub elements: Vec<ArrayElement>,
}

/// One element of an array literal.
///
/// `a=(1 [5]=x '2 3' [k]+=more)` has four: two bare values, an explicit
/// subscript and an explicit subscript append. The distinction is structural
/// — it must survive expansion, because `[i]=$v` is a scalar assignment to
/// element `i` while a bare `$v` is field-split into one or more elements.
#[derive(Clone, Debug)]
pub struct ArrayElement {
    /// The element's span, including any subscript.
    pub span: Span,
    /// The `[subscript]` prefix, when written.
    pub index: Option<ArrayIndex>,
    /// The value word: everything after `=` for a subscripted element, or the
    /// whole element otherwise.
    pub value: Word,
}

/// The `[subscript]` prefix of an array-literal element.
#[derive(Clone, Debug)]
pub struct ArrayIndex {
    /// The subscript text between the brackets, as a word.
    pub subscript: Word,
    /// Whether the element was written `[k]+=v`.
    pub append: bool,
}

/// A variable assignment.
#[derive(Clone, Debug)]
pub struct Assignment {
    /// The whole `name=value` span.
    pub span: Span,
    /// The variable name.
    pub name: Span,
    /// Whether this was `+=`.
    pub append: bool,
    /// The subscript of `name[sub]=value`, if any. It is a word because it is
    /// expanded (and, for an indexed array, then evaluated arithmetically)
    /// when the assignment runs.
    pub index: Option<Word>,
    /// The value word. Empty parts for `name=`.
    pub value: Word,
    /// For `name=( … )`, the elements. `Some(vec![])` for `name=()`.
    pub array: Option<Vec<ArrayElement>>,
}

/// A redirection.
#[derive(Clone, Debug)]
pub struct Redirect {
    /// The whole redirection, including any file-descriptor prefix and target
    /// written against the operator.
    pub span: Span,
    /// The file descriptor, when written (`2>` gives `Some(2)`).
    pub fd: Option<u32>,
    /// The operator.
    pub op: RedirectOp,
    /// The target word. `None` when the target is missing (a diagnostic has
    /// already been reported, or the input ended).
    pub target: Option<Word>,
    /// Index into [`Ast::heredocs`] for `<<` and `<<-`.
    pub heredoc: Option<usize>,
}

/// A redirection operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedirectOp {
    /// `<`
    Read,
    /// `>`
    Write,
    /// `>>`
    Append,
    /// `<>`
    ReadWrite,
    /// `>|`
    Clobber,
    /// `<<`
    HereDoc,
    /// `<<-`
    HereDocStrip,
    /// `<<<`
    HereString,
    /// `<&`
    DupRead,
    /// `>&`
    DupWrite,
    /// `&>`
    Both,
    /// `&>>`
    BothAppend,
}

impl RedirectOp {
    /// The operator as written, without any file-descriptor prefix.
    pub fn as_bytes(self) -> &'static [u8] {
        match self {
            RedirectOp::Read => b"<",
            RedirectOp::Write => b">",
            RedirectOp::Append => b">>",
            RedirectOp::ReadWrite => b"<>",
            RedirectOp::Clobber => b">|",
            RedirectOp::HereDoc => b"<<",
            RedirectOp::HereDocStrip => b"<<-",
            RedirectOp::HereString => b"<<<",
            RedirectOp::DupRead => b"<&",
            RedirectOp::DupWrite => b">&",
            RedirectOp::Both => b"&>",
            RedirectOp::BothAppend => b"&>>",
        }
    }

    /// Whether this operator introduces a here-document.
    pub fn is_heredoc(self) -> bool {
        matches!(self, RedirectOp::HereDoc | RedirectOp::HereDocStrip)
    }
}
