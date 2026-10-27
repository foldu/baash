//! The menu the editor receives: candidates plus the span to replace.

use bsh_lang::{QuoteCtx, quote};
use bsh_syntax::Expect;

use crate::candidate::{Candidate, Insertion, Span};
use crate::context::Ctx;

/// A ranked set of completions for one cursor position.
///
/// The editor needs three things and this carries all of them: what could be
/// inserted ([`Menu::candidates`]), which bytes of the line it would replace
/// ([`Menu::replace`]) and how insertion must be quoted ([`Menu::quote`]).
/// [`Menu::insertion`] does the quoting, so an editor never re-implements
/// escaping and the round trip is tested here.
#[derive(Clone, Debug)]
pub struct Menu {
    /// Ranked candidates, best first.
    pub candidates: Vec<Candidate>,
    /// The full span of the word under the cursor.
    pub word_span: Span,
    /// The span [`Menu::insertion`] replaces.
    pub replace: Span,
    /// The bytes typed between the insertion point and the cursor.
    pub prefix: Vec<u8>,
    /// The quote context at the insertion point.
    pub quote: QuoteCtx,
    /// Whether the word continued past the cursor.
    pub has_suffix: bool,
    /// What the parser said may appear here; useful for a menu header.
    pub expect: Expect,
}

impl Menu {
    /// Build a menu from a context and its ranked candidates.
    pub fn new(ctx: Ctx, candidates: Vec<Candidate>) -> Self {
        Self {
            candidates,
            word_span: ctx.word,
            replace: ctx.replace,
            prefix: ctx.prefix,
            quote: ctx.quote,
            has_suffix: ctx.has_suffix,
            expect: ctx.expect,
        }
    }

    /// Whether there is nothing to offer.
    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    /// The only candidate, when exactly one matched.
    pub fn unique(&self) -> Option<&Candidate> {
        match self.candidates.as_slice() {
            [candidate] => Some(candidate),
            _ => None,
        }
    }

    /// The bytes to insert for candidate `index`, already quoted, plus the
    /// trailing `/` or space the candidate asked for.
    ///
    /// Returns `None` for an out-of-range index, and for a source candidate in
    /// single quotes, where its syntax would be inert.
    ///
    /// A unique match appends a space unless it is a directory (which already
    /// ends in `/`), the word continued past the cursor, or the cursor is
    /// inside quotes.
    pub fn insertion(&self, index: usize) -> Option<Vec<u8>> {
        let candidate = self.candidates.get(index)?;
        let mut text = match candidate.insertion {
            Insertion::Literal => quote(&candidate.insert, self.quote),
            Insertion::Source => {
                if self.quote == QuoteCtx::SingleQuoted {
                    return None;
                }
                candidate.insert.clone()
            }
        };
        if self.candidates.len() == 1
            && !self.has_suffix
            && candidate.append_space
            && self.quote == QuoteCtx::Unquoted
        {
            text.push(b' ');
        }
        Some(text)
    }

    /// Splice candidate `index` into `line` and return the span it occupies.
    ///
    /// `line` must be the line this menu was computed for. The suffix of the
    /// completed word is replaced, so text after the cursor inside the word is
    /// removed.
    pub fn apply(&self, line: &mut Vec<u8>, index: usize) -> Option<Span> {
        let text = self.insertion(index)?;
        let start = self.replace.start.min(line.len());
        let end = self.replace.end.min(line.len()).max(start);
        line.splice(start..end, text.iter().copied());
        Some(start..start + text.len())
    }
}
