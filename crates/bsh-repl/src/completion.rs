//! The completion menu's state: the ranked candidates, a selection, a scroll
//! window, and the rows the renderer paints.
//!
//! This is presentation only. Which candidates exist and how each is quoted for
//! insertion belong to `bsh-complete` (`docs/contracts.md`); the editor asks for
//! a [`Menu`] and only ever inserts through [`Menu::apply`].

use bsh_complete::Menu;

use crate::display::{truncate, width_of};
use crate::render::MenuRow;

/// The open menu: candidates, selected index, scroll offset.
#[derive(Debug)]
pub struct MenuState {
    /// The menu exactly as the completer produced it.
    pub menu: Menu,
    /// Index of the selected candidate.
    pub selected: usize,
    /// Index of the first visible candidate.
    pub offset: usize,
}

impl MenuState {
    /// Open a menu with the first candidate selected.
    pub fn new(menu: Menu) -> Self {
        Self {
            menu,
            selected: 0,
            offset: 0,
        }
    }

    /// Select the next candidate, wrapping at the end.
    pub fn next(&mut self) {
        if self.menu.candidates.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.menu.candidates.len();
    }

    /// Select the previous candidate, wrapping at the start.
    pub fn previous(&mut self) {
        let count = self.menu.candidates.len();
        if count == 0 {
            return;
        }
        self.selected = (self.selected + count - 1) % count;
    }

    /// The selected candidate index.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Render the visible rows.
    ///
    /// `max_rows` bounds the menu height and `columns` bounds each row. The
    /// window scrolls so the selection is always visible.
    pub fn rows(&mut self, max_rows: usize, columns: u16) -> Vec<MenuRow> {
        let count = self.menu.candidates.len();
        let visible = max_rows.min(count).max(1);
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + visible {
            self.offset = self.selected + 1 - visible;
        }
        let offset = self.offset.min(count.saturating_sub(1));

        let name_width = self
            .menu
            .candidates
            .iter()
            .map(|candidate| width_of(&flatten(&candidate.display)))
            .max()
            .unwrap_or(0);
        // Two cells for the marker, two for the gap before a description.
        let columns = usize::from(columns);
        let description_at = (name_width + 4).min(columns);

        self.menu
            .candidates
            .iter()
            .enumerate()
            .skip(offset)
            .take(visible)
            .map(|(index, candidate)| {
                let selected = index == self.selected;
                let mut text = String::with_capacity(columns);
                text.push_str(if selected { "> " } else { "  " });
                let name = flatten(&candidate.display);
                text.push_str(&name);
                if let Some(description) = &candidate.desc {
                    let used = 2 + width_of(&name);
                    let padding = description_at.saturating_sub(used);
                    text.push_str(&" ".repeat(padding));
                    text.push_str(&flatten(description));
                }
                MenuRow {
                    text: truncate(&text, columns),
                    selected,
                }
            })
            .collect()
    }
}

/// Flatten a candidate for painting.
///
/// `Candidate::display` is not sanitised; a hostile filename may carry newlines
/// or escape bytes (`docs/contracts.md`, `bsh-complete`). Control characters
/// become spaces here, so a filename cannot move the cursor or repaint the
/// screen.
fn flatten(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bsh_complete::{CandKind, Candidate, Completer, CompletionEngine};

    fn menu_for(line: &[u8], candidates: &[&str]) -> Menu {
        let engine = CompletionEngine::new();
        let cursor = line.len();
        let context = engine.context(line, cursor);
        let candidates = candidates
            .iter()
            .map(|text| Candidate::new(*text, CandKind::Command))
            .collect();
        Menu::new(context, candidates)
    }

    #[test]
    fn control_characters_in_a_candidate_are_flattened() {
        assert_eq!(flatten("bad\nname\x1b"), "bad name ");
    }

    #[test]
    fn rows_mark_the_selection_and_describe_candidates() {
        let mut state = MenuState::new(menu_for(b"gi", &["git", "gitk", "git-commit"]));
        let rows = state.rows(10, 80);
        assert_eq!(rows.len(), 3);
        assert!(rows[0].selected);
        assert!(rows[0].text.starts_with("> git"));
        assert!(rows[1].text.starts_with("  gitk"));
        assert!(!rows[1].selected);
    }

    #[test]
    fn navigation_wraps_and_scrolls() {
        let mut state = MenuState::new(menu_for(b"g", &["a", "b", "c"]));
        state.previous();
        assert_eq!(state.selected(), 2);
        state.next();
        assert_eq!(state.selected(), 0);
        let rows = state.rows(2, 80);
        assert_eq!(rows.len(), 2);
        state.next();
        state.next();
        let rows = state.rows(2, 80);
        assert!(rows.iter().any(|row| row.selected));
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn a_narrow_terminal_truncates_rows() {
        let mut state = MenuState::new(menu_for(b"g", &["a-very-long-command"]));
        let rows = state.rows(10, 10);
        assert!(width_of(&rows[0].text) <= 10);
    }

    #[test]
    fn the_engine_menu_is_not_reordered() {
        let menu = menu_for(b"g", &["zebra", "apple"]);
        assert_eq!(menu.candidates[0].display, "zebra");
    }

    #[test]
    fn a_stub_completer_can_produce_a_menu() {
        struct Stub(Menu);
        impl Completer for Stub {
            fn complete(&self, _line: &[u8], _cursor: usize) -> Menu {
                self.0.clone()
            }
        }
        let stub = Stub(menu_for(b"x", &["xylophone"]));
        let menu = stub.complete(b"x", 1);
        assert_eq!(menu.candidates.len(), 1);
    }
}
