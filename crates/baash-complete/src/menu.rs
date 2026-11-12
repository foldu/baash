//! The menu the editor receives: candidates plus the span to replace.

use baash_lang::{QuoteCtx, quote};
use baash_syntax::Expect;

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

    /// The longest prefix every candidate shares, when it continues what the
    /// user typed.
    ///
    /// This is fish's first-Tab behaviour: complete the unambiguous part before
    /// showing a menu, so `git ch<TAB>` becomes `git che<TAB>` when both
    /// `checkout` and `cherry` are on offer. `None` means there is nothing
    /// unambiguous to add — including the case where a fuzzy candidate does not
    /// start with the typed bytes at all, where inserting a shared prefix could
    /// delete text the user meant to keep.
    pub fn common_prefix(&self) -> Option<Vec<u8>> {
        let mut candidates = self.candidates.iter();
        let mut common = candidates.next()?.insert.clone();
        for candidate in candidates {
            let shared = common
                .iter()
                .zip(&candidate.insert)
                .take_while(|(left, right)| left == right)
                .count();
            common.truncate(shared);
            if common.is_empty() {
                return None;
            }
        }
        (common.len() > self.prefix.len() && common.starts_with(&self.prefix)).then_some(common)
    }

    /// [`Menu::common_prefix`], quoted for the insertion context.
    ///
    /// Candidates whose bytes are shell source (`$FOO`, `${HOME}`, a keyword)
    /// must not be escaped; the others go through the same
    /// [`baash_lang::quote`] path as a complete insertion. A mix is quoted: the
    /// shared bytes are then literal text, which is what an ordinary word would
    /// insert.
    pub fn common_prefix_insertion(&self) -> Option<Vec<u8>> {
        let common = self.common_prefix()?;
        let all_source = self
            .candidates
            .iter()
            .all(|candidate| candidate.insertion == Insertion::Source);
        if all_source && self.quote != QuoteCtx::SingleQuoted {
            Some(common)
        } else {
            Some(quote(&common, self.quote))
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

#[cfg(test)]
mod tests {
    use super::*;
    use baash_syntax::ParseOpts;

    use crate::candidate::CandKind;
    use crate::context::context;

    fn menu(line: &[u8], words: &[&[u8]]) -> Menu {
        let ctx = context(line, line.len(), &ParseOpts::DEFAULT);
        let candidates = words
            .iter()
            .map(|word| Candidate::new(*word, CandKind::Command))
            .collect();
        Menu::new(ctx, candidates)
    }

    #[test]
    fn the_common_prefix_is_the_part_beyond_what_was_typed() {
        let menu = menu(b"ls al", &[b"alpha", b"alpine"]);
        assert_eq!(menu.common_prefix().as_deref(), Some(&b"alp"[..]));
        assert_eq!(menu.common_prefix_insertion().as_deref(), Some(&b"alp"[..]));
    }

    #[test]
    fn no_extension_means_no_prefix_insertion() {
        // The candidates disagree at once: the typed `a` is all they share.
        assert_eq!(menu(b"ls a", &[b"alpha", b"beta"]).common_prefix(), None);
        // A fuzzy candidate that does not start with the typed bytes would
        // make splicing a shared prefix destructive.
        let mut menu = menu(b"ls a", &[b"alpha", b"switch"]);
        menu.candidates[1].tier = 2;
        assert_eq!(menu.common_prefix(), None);
    }

    #[test]
    fn source_candidates_keep_their_syntax_and_literal_ones_are_quoted() {
        let ctx = context(b"echo $HO", 8, &ParseOpts::DEFAULT);
        let source = Menu::new(
            ctx,
            vec![
                Candidate::source(b"$HOME".to_vec(), CandKind::Variable),
                Candidate::source(b"$HOMEDIR".to_vec(), CandKind::Variable),
            ],
        );
        assert_eq!(
            source.common_prefix_insertion().as_deref(),
            Some(&b"$HOME"[..]),
            "a parameter expansion is source, not a word to escape"
        );

        let literal = menu(b"ls al", &[b"alpha", b"alpine"]);
        assert_eq!(
            literal.common_prefix_insertion().as_deref(),
            Some(&b"alp"[..])
        );
    }

    #[test]
    fn a_literal_bang_inside_double_quotes_is_inserted_as_a_single_quoted_piece() {
        // The menu only knows the insertion point's quote context; the bang
        // itself is the quoting layer's problem (`baash_lang::quote`). This
        // pins the seam: a filename completed inside `"…"` must keep the
        // double quote open around the result, with the `!` outside it.
        let menu = menu(b"cat \"bang", &[b"bang!name"]);
        assert_eq!(menu.quote, QuoteCtx::DoubleQuoted);
        let mut line = b"cat \"bang".to_vec();
        let span = menu.apply(&mut line, 0).expect("a full insertion");
        assert_eq!(line, b"cat \"bang\"'!'\"name");
        assert_eq!(span.end, line.len());
    }

    #[test]
    fn a_common_prefix_can_be_applied_to_a_line() {
        let menu = menu(b"ls al", &[b"alpha", b"alpine"]);
        let mut line = b"ls al".to_vec();
        let span = menu.apply(&mut line, 0).expect("a full insertion");
        assert_eq!(line, b"ls alpha");
        assert_eq!(span, 3..8);
    }
}
