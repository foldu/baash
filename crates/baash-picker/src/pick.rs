//! The picker's state: the candidates, the query, the ranked matches.

use nucleo_matcher::{Config, Matcher, Utf32Str};

/// What a key asks the picker to do.
///
/// The picker does not know about key encodings: the editor maps its decoded
/// [`Key`](../../baash_repl/…) onto these, and [`Decoder`](crate::Decoder) maps
/// the legacy bytes a builtin reads onto the same set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Append these bytes to the query.
    ///
    /// A paste arrives as one `Insert`, so a pasted path with a space in it is
    /// one query edit and one step of progress rather than a dozen.
    Insert(Vec<u8>),
    /// Remove the query's last character.
    Backspace,
    /// Remove the query's last word.
    KillWord,
    /// Empty the query.
    Clear,
    /// Move the selection one row down.
    Down,
    /// Move the selection one row up.
    Up,
    /// Move the selection a screenful down.
    PageDown,
    /// Move the selection a screenful up.
    PageUp,
    /// Select the first row.
    First,
    /// Select the last row.
    Last,
    /// Choose the selected row.
    Accept,
    /// Leave without choosing.
    Cancel,
}

/// What an [`Action`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Stay in the picker.
    Continue,
    /// The caller should read [`Picker::selected`] and close the picker.
    Accept,
    /// The caller should close the picker and change nothing.
    Cancel,
}

/// One candidate.
///
/// `text` is what the caller gets back, byte for byte; `display` is the same
/// line as the screen shows it. They differ because a candidate may contain
/// anything at all — a pasted block is several lines, a file name may hold an
/// escape byte — and a row of the picker is one line, whatever the bytes say.
pub(crate) struct Line {
    text: Vec<u8>,
    pub(crate) display: String,
}

/// One match: which candidate, and how good the match is.
///
/// The matched *positions* are not stored. A picker can hold a hundred
/// thousand candidates and only a screenful is ever painted, so computing the
/// positions for every match would allocate once per candidate per keystroke
/// for the benefit of the twenty rows on screen. [`Picker::positions`]
/// recomputes them for the row that is about to be painted, and that is the
/// cheap direction: one candidate, not a hundred thousand.
pub(crate) struct Hit {
    /// Index into [`Picker::lines`].
    pub(crate) index: usize,
    /// The summed score of every query term; higher is better.
    score: u32,
}

/// The picker: candidates, the query, and the selection.
pub struct Picker {
    /// Painted before the query, so the two callers can say what is being
    /// searched (`history> ` for `Ctrl-R`, nothing at all for `baashzf`).
    pub(crate) prompt: Vec<u8>,
    pub(crate) lines: Vec<Line>,
    pub(crate) query: Vec<u8>,
    /// The matches, best first. Empty until the first query byte.
    pub(crate) hits: Vec<Hit>,
    /// Index into `hits`.
    pub(crate) selected: usize,
    /// Index of the first visible hit.
    pub(crate) offset: usize,
    /// The height of the list when it was last painted, for PageUp/PageDown.
    pub(crate) page: usize,
    matcher: Matcher,
    /// Scratch buffer for the UTF-32 projection of a non-ASCII haystack.
    scratch: Vec<char>,
}

impl Picker {
    /// A picker over `lines`, in the order the caller wants them shown.
    ///
    /// `prompt` is painted at the start of the query row; an empty one leaves
    /// the row to the query alone.
    pub fn new(prompt: &[u8], lines: Vec<Vec<u8>>) -> Self {
        let lines: Vec<Line> = lines
            .into_iter()
            .map(|text| {
                let display = display_of(&text);
                Line { text, display }
            })
            .collect();
        let hits = (0..lines.len())
            .map(|index| Hit { index, score: 0 })
            .collect();
        let mut config = Config::DEFAULT;
        // A match at the start of the line is worth more than one that only
        // turns up in the middle: `vi` should find `vim` before
        // `git log --reverse vi…`.
        config.prefer_prefix = true;
        Self {
            prompt: prompt.to_vec(),
            lines,
            query: Vec::new(),
            hits,
            selected: 0,
            offset: 0,
            page: 10,
            matcher: Matcher::new(config),
            scratch: Vec::new(),
        }
    }

    /// The bytes of the selected candidate, or `None` when nothing matches.
    pub fn selected(&self) -> Option<&[u8]> {
        self.hits
            .get(self.selected)
            .map(|hit| self.lines[hit.index].text.as_slice())
    }

    /// The query as typed.
    pub fn query(&self) -> &[u8] {
        &self.query
    }

    /// How many candidates match the query.
    pub fn matches(&self) -> usize {
        self.hits.len()
    }

    /// How many candidates there are.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether there is nothing to choose from.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Which match is selected.
    pub fn selection(&self) -> usize {
        self.selected
    }

    /// Apply one action.
    pub fn act(&mut self, action: Action) -> Outcome {
        match action {
            Action::Insert(bytes) => {
                self.query.extend_from_slice(&bytes);
                self.refresh();
            }
            Action::Backspace => {
                if pop_character(&mut self.query) {
                    self.refresh();
                }
            }
            Action::KillWord => {
                if kill_word(&mut self.query) {
                    self.refresh();
                }
            }
            Action::Clear => {
                if !self.query.is_empty() {
                    self.query.clear();
                    self.refresh();
                }
            }
            Action::Down => self.select(self.selected.saturating_add(1)),
            Action::Up => self.select(self.selected.saturating_sub(1)),
            Action::PageDown => self.select(self.selected.saturating_add(self.page.max(1))),
            Action::PageUp => self.select(self.selected.saturating_sub(self.page.max(1))),
            Action::First => self.select(0),
            Action::Last => self.select(self.hits.len().saturating_sub(1)),
            Action::Accept => return Outcome::Accept,
            Action::Cancel => return Outcome::Cancel,
        }
        Outcome::Continue
    }

    /// The characters of match `at` that the query matched.
    ///
    /// Recomputed rather than stored: see [`Hit`]. The positions are character
    /// indices into [`Line::display`], ascending and unique, which is what the
    /// painter needs to embolden them.
    pub(crate) fn positions(&mut self, at: usize) -> Vec<u32> {
        let mut positions = Vec::new();
        let Some(index) = self.hits.get(at).map(|hit| hit.index) else {
            return positions;
        };
        let terms = terms(&self.query);
        // Borrow the fields the loop needs at once, as `refresh` does.
        let Self {
            matcher,
            scratch,
            lines,
            ..
        } = self;
        let mut needle_scratch = Vec::new();
        if let Some(line) = lines.get(index) {
            for term in &terms {
                let needle = Utf32Str::new(term, &mut needle_scratch);
                let haystack = Utf32Str::new(&line.display, scratch);
                let _ = matcher.fuzzy_indices_greedy(haystack, needle, &mut positions);
            }
        }
        positions.sort_unstable();
        positions.dedup();
        positions
    }

    /// Select `index`, clamped to the matches there are.
    ///
    /// Clamping rather than wrapping is what fzf does, and it is the right
    /// shape for a list that a keystroke can lengthen: wrapping from the last
    /// match back to the best one would make `Ctrl-R Ctrl-R` at the end of the
    /// list jump to the newest entry again, which reads as a bug.
    fn select(&mut self, index: usize) {
        if self.hits.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = index.min(self.hits.len() - 1);
    }

    /// Recompute the matches for the current query.
    ///
    /// Kept as one function so the empty-query case and the matching case
    /// cannot disagree about what `selected` and `offset` mean afterwards.
    fn refresh(&mut self) {
        let terms = terms(&self.query);
        let mut hits = Vec::with_capacity(self.lines.len());
        if terms.is_empty() {
            // No query: every candidate, in the caller's order.
            hits.extend((0..self.lines.len()).map(|index| Hit { index, score: 0 }));
        } else {
            // Borrow the two fields the loop needs at once; `refresh` is on the
            // keystroke path, so it must not copy the candidate list.
            let Self {
                matcher,
                scratch,
                lines,
                ..
            } = self;
            let mut needle_scratch = Vec::new();
            for (index, line) in lines.iter().enumerate() {
                let mut score = 0u32;
                // A flag, not `score > 0`: a match is not required to *score*
                // anything, and a candidate that matches with a score of zero
                // belongs in the list.
                let mut matched = true;
                for term in &terms {
                    // An ASCII term needs no projection at all: `Utf32Str`
                    // borrows the bytes and leaves the scratch buffer alone,
                    // which is the case for every query a user types in
                    // practice.
                    let needle = Utf32Str::new(term, &mut needle_scratch);
                    let haystack = Utf32Str::new(&line.display, scratch);
                    match matcher.fuzzy_match_greedy(haystack, needle) {
                        Some(points) => score = score.saturating_add(u32::from(points)),
                        None => {
                            // One term that does not match is enough: the terms
                            // are ANDed, so the candidate is out.
                            matched = false;
                            break;
                        }
                    }
                }
                if matched {
                    hits.push(Hit { index, score });
                }
            }
        }
        // A stable sort keeps the caller's order for equal scores, which is
        // what makes the empty query and a tie both read as "as given".
        hits.sort_by_key(|hit| std::cmp::Reverse(hit.score));
        self.hits = hits;
        self.selected = 0;
        self.offset = 0;
    }
}

/// The query's terms.
///
/// Terms are split on spaces and tabs, and an empty term (the space between two
/// others) is dropped: the query `"  git  co "` asks for the same thing as
/// `"git co"`.
///
/// A term is kept as a `String` because that is the projection `nucleo` matches
/// against, and because `Utf32Str` borrows it rather than copying it.
fn terms(query: &[u8]) -> Vec<String> {
    query
        .split(|byte| *byte == b' ' || *byte == b'\t')
        .filter(|term| !term.is_empty())
        .map(|term| String::from_utf8_lossy(term).into_owned())
        .collect()
}

/// The screen form of one candidate: one row, whatever bytes it contains.
///
/// A candidate with a newline in it (a pasted block, a file name that lies)
/// would otherwise break the row arithmetic the whole frame is built on. A
/// tab is expanded and a newline becomes a space, so every candidate is exactly
/// one row and its width is a number the layout can trust.
fn display_of(text: &[u8]) -> String {
    let mut out = String::with_capacity(text.len());
    for character in String::from_utf8_lossy(text).chars() {
        match character {
            '\n' | '\r' => out.push(' '),
            '\t' => out.push_str("    "),
            other if other.is_control() => out.push(' '),
            other => out.push(other),
        }
    }
    out
}

/// Remove the query's last character; `false` when there was none.
///
/// Whole characters, not bytes: a query typed with an accented letter must lose
/// the letter on Backspace, not leave a stray UTF-8 continuation byte behind
/// that no terminal can paint.
fn pop_character(query: &mut Vec<u8>) -> bool {
    let Some(&last) = query.last() else {
        return false;
    };
    // ASCII, or a UTF-8 lead byte with no continuation after it: one byte is
    // the whole character.
    if last & 0xC0 != 0x80 {
        query.pop();
        return true;
    }
    // Walk back over continuation bytes (`10xxxxxx`) to the lead byte.
    let mut start = query.len() - 1;
    while start > 0 && query[start] & 0xC0 == 0x80 {
        start -= 1;
    }
    if query[start] & 0xC0 == 0x80 {
        // A run of continuation bytes with no lead at all: the input is not
        // UTF-8, so drop one byte rather than guess at a character.
        query.pop();
    } else {
        query.truncate(start);
    }
    true
}

/// Remove the query's last word; `false` when there was nothing to remove.
fn kill_word(query: &mut Vec<u8>) -> bool {
    let before = query.len();
    while query
        .last()
        .is_some_and(|byte| *byte == b' ' || *byte == b'\t')
    {
        query.pop();
    }
    while query
        .last()
        .is_some_and(|byte| *byte != b' ' && *byte != b'\t')
    {
        query.pop();
    }
    query.len() != before
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker(lines: &[&str]) -> Picker {
        Picker::new(
            b"history> ",
            lines.iter().map(|line| line.as_bytes().to_vec()).collect(),
        )
    }

    fn chosen(lines: &[&str], keys: &[Action]) -> Option<String> {
        let mut picker = picker(lines);
        for action in keys {
            picker.act(action.clone());
        }
        picker
            .selected()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
    }

    fn inserted(text: &str) -> Action {
        Action::Insert(text.as_bytes().to_vec())
    }

    #[test]
    fn an_empty_query_lists_every_candidate_in_the_order_given() {
        let picker = picker(&["newest", "middle", "oldest"]);
        assert_eq!(picker.matches(), 3);
        assert_eq!(picker.selected(), Some(b"newest".as_slice()));
    }

    #[test]
    fn a_subsequence_matches_where_a_prefix_would_not() {
        let lines = ["git checkout main", "ls"];
        // `gco` is not a prefix of anything, and not even a substring: the
        // letters are found in order, which is what fzf's default does.
        assert_eq!(
            chosen(&lines, &[inserted("gco")]).as_deref(),
            Some("git checkout main")
        );
        assert_eq!(chosen(&lines, &[inserted("ls")]).as_deref(), Some("ls"));
        assert_eq!(chosen(&lines, &[inserted("zz")]), None);
    }

    #[test]
    fn a_lowercase_query_matches_whatever_the_case() {
        let lines = ["GIT status", "other"];
        assert_eq!(
            chosen(&lines, &[inserted("git")]).as_deref(),
            Some("GIT status")
        );
    }

    #[test]
    fn an_uppercase_query_matches_case_only() {
        // Smart case, as `fzf` and `nucleo` both mean it: the query decides.
        let lines = ["git status", "GIT status"];
        assert_eq!(
            chosen(&lines, &[inserted("GIT")]).as_deref(),
            Some("GIT status")
        );
    }

    #[test]
    fn a_contiguous_match_beats_a_scattered_one() {
        // Both are subsequences of `gco`; `git commit` matches `co` together,
        // so it wins even though the other line is the longer, likelier one.
        let lines = ["git checkout main", "git commit -m x"];
        assert_eq!(
            chosen(&lines, &[inserted("gco")]).as_deref(),
            Some("git commit -m x")
        );
    }

    #[test]
    fn every_term_must_match() {
        let lines = ["git checkout main", "make all"];
        assert_eq!(
            chosen(&lines, &[inserted("git co")]).as_deref(),
            Some("git checkout main")
        );
        assert_eq!(chosen(&lines, &[inserted("git zz")]), None);
        // The order of the terms does not matter.
        assert_eq!(
            chosen(&lines, &[inserted("co git")]).as_deref(),
            Some("git checkout main")
        );
    }

    #[test]
    fn a_prefix_scores_higher_than_a_match_in_the_middle() {
        let lines = ["git log --reverse vi", "vim"];
        assert_eq!(chosen(&lines, &[inserted("vi")]).as_deref(), Some("vim"));
    }

    #[test]
    fn equal_scores_keep_the_order_the_caller_gave() {
        let lines = ["make one", "make two"];
        let picker = {
            let mut picker = picker(&lines);
            picker.act(inserted("make"));
            picker
        };
        assert_eq!(picker.matches(), 2);
        assert_eq!(picker.selected(), Some(b"make one".as_slice()));
    }

    #[test]
    fn selection_clamps_instead_of_wrapping() {
        let lines = ["a", "b"];
        let mut picker = picker(&lines);
        picker.act(Action::Up);
        assert_eq!(picker.selection(), 0, "up from the top stays put");
        picker.act(Action::Down);
        assert_eq!(picker.selection(), 1);
        picker.act(Action::Down);
        assert_eq!(picker.selection(), 1, "down from the bottom stays put");
        picker.act(Action::Last);
        assert_eq!(picker.selection(), 1);
        picker.act(Action::First);
        assert_eq!(picker.selection(), 0);
    }

    #[test]
    fn a_new_query_selects_the_best_match_again() {
        let lines = ["keep one", "other"];
        let mut picker = picker(&lines);
        picker.act(Action::Down);
        assert_eq!(picker.selection(), 1);
        picker.act(inserted("keep"));
        assert_eq!(picker.selection(), 0);
        assert_eq!(picker.selected(), Some(b"keep one".as_slice()));
    }

    #[test]
    fn backspace_edits_the_query_by_character() {
        let lines = ["héllo", "hello"];
        let mut picker = picker(&lines);
        picker.act(inserted("hé"));
        assert_eq!(picker.query(), "hé".as_bytes());
        picker.act(Action::Backspace);
        assert_eq!(picker.query(), b"h");
        assert_eq!(picker.matches(), 2);
        picker.act(Action::Backspace);
        assert_eq!(picker.query(), b"");
        assert_eq!(picker.matches(), 2);
        picker.act(Action::Backspace);
        assert_eq!(
            picker.query(),
            b"",
            "backspace on an empty query is a no-op"
        );
    }

    #[test]
    fn kill_word_and_clear() {
        let lines = ["git checkout main"];
        let mut picker = picker(&lines);
        picker.act(inserted("git checkout "));
        // Readline's `Ctrl-W` takes the word *and* the whitespace before it,
        // so the trailing space goes with the word it separated.
        picker.act(Action::KillWord);
        assert_eq!(picker.query(), b"git ");
        picker.act(Action::KillWord);
        assert_eq!(picker.query(), b"");
        picker.act(Action::Clear);
        assert_eq!(picker.query(), b"");
    }

    #[test]
    fn accept_and_cancel_are_reported_and_change_nothing() {
        let mut picker = picker(&["one"]);
        assert_eq!(picker.act(Action::Accept), Outcome::Accept);
        assert_eq!(picker.selected(), Some(b"one".as_slice()));
        assert_eq!(picker.act(Action::Cancel), Outcome::Cancel);
    }

    #[test]
    fn an_empty_pool_has_nothing_to_select() {
        let mut picker = picker(&[]);
        assert!(picker.is_empty());
        assert_eq!(picker.selected(), None);
        assert_eq!(picker.act(Action::Accept), Outcome::Accept);
        picker.act(inserted("anything"));
        assert_eq!(picker.matches(), 0);
        assert_eq!(picker.selected(), None);
    }

    #[test]
    fn a_candidate_with_newlines_or_control_bytes_is_one_row() {
        assert_eq!(display_of(b"one\ntwo"), "one two");
        assert_eq!(display_of(b"tab\there"), "tab    here");
        assert_eq!(display_of(b"esc\x1b[31mred"), "esc [31mred");
        // Invalid UTF-8 becomes the replacement character, one cell each.
        assert_eq!(display_of(b"a\xffb"), "a\u{fffd}b");
    }

    #[test]
    fn a_match_reports_which_characters_matched() {
        let lines = ["git checkout"];
        let mut picker = picker(&lines);
        picker.act(inserted("gco"));
        assert_eq!(picker.hits.len(), 1);
        let display = picker.lines[0].display.clone();
        let matched: String = picker
            .positions(0)
            .iter()
            .map(|index| display.chars().nth(*index as usize).unwrap())
            .collect();
        assert_eq!(matched, "gco");

        // Two terms: the positions of both, merged and in order.
        picker.act(Action::Clear);
        picker.act(inserted("git co"));
        let matched: String = picker
            .positions(0)
            .iter()
            .map(|index| display.chars().nth(*index as usize).unwrap())
            .collect();
        assert_eq!(matched, "gitco");
    }
}
