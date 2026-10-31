//! The completion menu's state: the ranked candidates, a selection, a scroll
//! window, and the rows the renderer paints.
//!
//! This is presentation only. Which candidates exist and how each is quoted for
//! insertion belong to `bsh-complete` (`docs/contracts.md`); the editor asks for
//! a [`Menu`] and only ever inserts through [`Menu::apply`].

use bsh_complete::Menu;

use crate::display::{truncate, width_of};
use crate::render::MenuRow;

/// Cells between two menu columns, and between a name and its description.
const COLUMN_GAP: usize = 2;

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
    /// Entries are packed into as many columns as the terminal width allows,
    /// and the result degrades to one clipped column on a narrow terminal
    /// rather than wrapping (a wrapped menu row would push the prompt off the
    /// screen). `max_rows` bounds the height; the window scrolls by *row* so
    /// the selected cell is always visible.
    pub fn rows(&mut self, max_rows: usize, columns: u16) -> Vec<MenuRow> {
        let count = self.menu.candidates.len();
        if count == 0 {
            return Vec::new();
        }
        let columns = usize::from(columns.max(1));

        // Markers, names and descriptions are laid out once. Descriptions
        // start at one column across every cell, which is what makes a menu
        // scannable when the entries above and below have different name
        // lengths.
        let names: Vec<String> = self
            .menu
            .candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                let marker = if index == self.selected { "> " } else { "  " };
                format!("{marker}{}", flatten(&candidate.display))
            })
            .collect();
        let descriptions: Vec<Option<String>> = self
            .menu
            .candidates
            .iter()
            .map(|candidate| candidate.desc.as_ref().map(|desc| flatten(desc)))
            .collect();
        let name_width = names.iter().map(|name| width_of(name)).max().unwrap_or(0);
        let desc_width = descriptions
            .iter()
            .flatten()
            .map(|desc| width_of(desc))
            .max()
            .unwrap_or(0);
        let wanted = name_width
            + if desc_width > 0 {
                COLUMN_GAP + desc_width
            } else {
                0
            };
        let cell_width = wanted.min(columns).max(1);
        let column_count = if columns >= cell_width + COLUMN_GAP {
            ((columns + COLUMN_GAP) / (cell_width + COLUMN_GAP)).clamp(1, count)
        } else {
            1
        };

        let mut cells = Vec::with_capacity(count);
        for index in 0..count {
            let mut text = names[index].clone();
            if let Some(description) = descriptions[index].as_ref() {
                let used = width_of(&text);
                text.push_str(&" ".repeat(name_width.saturating_sub(used) + COLUMN_GAP));
                text.push_str(description);
            }
            if column_count > 1 {
                // Pad every cell to one width so the next column starts at the
                // same place; the final cell of a row does not need it, but a
                // uniform cell is simpler and is clipped at the row's end.
                text.push_str(&" ".repeat(cell_width.saturating_sub(width_of(&text))));
            }
            cells.push(truncate(&text, cell_width));
        }

        let row_count = count.div_ceil(column_count);
        let selected_row = self.selected / column_count;
        let visible = max_rows.min(row_count).max(1);
        if selected_row < self.offset {
            self.offset = selected_row;
        } else if selected_row >= self.offset + visible {
            self.offset = selected_row + 1 - visible;
        }
        self.offset = self.offset.min(row_count.saturating_sub(1));

        let mut rows = Vec::with_capacity(visible);
        for row_index in self.offset..(self.offset + visible).min(row_count) {
            let first = row_index * column_count;
            let last = (first + column_count).min(count);
            let mut text = String::with_capacity(columns);
            let mut selection = None;
            for (index, cell) in cells.iter().enumerate().take(last).skip(first) {
                if index == self.selected {
                    selection = Some(text.len()..text.len() + cell.len());
                }
                text.push_str(cell);
                if index + 1 < last {
                    text.push_str(&" ".repeat(COLUMN_GAP));
                }
            }
            let text = truncate(&text, columns);
            // The row may have been clipped before or inside the selected
            // cell; keep the range inside what is actually painted.
            let selection = selection.and_then(|range: std::ops::Range<usize>| {
                let end = range.end.min(text.len());
                (range.start < end).then_some(range.start..end)
            });
            rows.push(MenuRow {
                selected: selection.is_some(),
                selection,
                text,
            });
        }
        rows
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

    fn menu_candidates(line: &[u8], candidates: Vec<Candidate>) -> Menu {
        let engine = CompletionEngine::new();
        let cursor = line.len();
        let context = engine.context(line, cursor);
        Menu::new(context, candidates)
    }

    #[test]
    fn rows_mark_the_selection_and_describe_candidates() {
        let candidates = vec![
            Candidate::new("git", CandKind::Command).with_desc("version control"),
            Candidate::new("gitk", CandKind::Command).with_desc("browser"),
            Candidate::new("git-commit", CandKind::Command).with_desc("record changes"),
        ];
        let mut state = MenuState::new(menu_candidates(b"gi", candidates));
        let rows = state.rows(10, 30);
        assert_eq!(rows.len(), 3, "a 30-column menu is one column");
        assert!(rows[0].selected);
        assert!(rows[0].text.starts_with("> git "));
        assert!(rows[1].text.starts_with("  gitk"));
        assert!(!rows[1].selected);
        // The description column starts at the same cell on every row.
        let descriptions = rows
            .iter()
            .map(|row| row.text.rfind("  ").expect("name/description gap"))
            .collect::<Vec<_>>();
        assert!(
            descriptions.windows(2).all(|pair| pair[0] == pair[1]),
            "descriptions align: {descriptions:?}"
        );
        let selection = rows[0].selection.clone().expect("the selected cell");
        assert!(rows[0].text[selection].starts_with("> git"));
    }

    #[test]
    fn a_wide_terminal_packs_candidates_into_columns() {
        let candidates = ["alpha", "beta", "gamma", "delta", "epsilon"]
            .into_iter()
            .map(|name| Candidate::new(name, CandKind::Command))
            .collect();
        let mut state = MenuState::new(menu_candidates(b"", candidates));
        let rows = state.rows(8, 80);
        assert_eq!(rows.len(), 1, "five short entries fit on one row");
        let text = &rows[0].text;
        assert!(text.contains("> alpha"));
        assert!(text.contains("  gamma"), "later candidates share the row");
        // Only the selected cell is inverted, not its neighbours.
        let selection = rows[0].selection.clone().expect("a selected cell");
        assert!(text[selection.clone()].contains("alpha"));
        assert!(!text[selection].contains("beta"));
    }

    #[test]
    fn navigation_wraps_and_scrolls_rows() {
        let mut state = MenuState::new(menu_for(b"g", &["a", "b", "c"]));
        state.previous();
        assert_eq!(state.selected(), 2);
        state.next();
        assert_eq!(state.selected(), 0);
        // Twelve columns fit two short entries per row, so three entries make
        // two rows and the two-row window is the whole menu.
        let rows = state.rows(2, 12);
        assert_eq!(rows.len(), 2);
        state.next();
        state.next();
        let rows = state.rows(2, 12);
        assert!(rows.iter().any(|row| row.selected));
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn a_narrow_terminal_truncates_rows_to_one_column() {
        let mut state = MenuState::new(menu_for(b"g", &["a-very-long-command"]));
        let rows = state.rows(10, 10);
        assert_eq!(rows.len(), 1);
        assert!(width_of(&rows[0].text) <= 10);
    }

    #[test]
    fn a_narrow_terminal_pages_instead_of_growing_past_its_height() {
        let candidates = ["alpha", "beta", "gamma", "delta", "epsilon"]
            .into_iter()
            .map(|name| Candidate::new(name, CandKind::Command))
            .collect();
        let mut state = MenuState::new(menu_candidates(b"", candidates));
        let rows = state.rows(2, 9);
        assert_eq!(rows.len(), 2, "one column at nine cells; two rows fit");
        assert!(width_of(&rows[0].text) <= 9);
        // Selecting the last entry scrolls the window so it stays visible.
        for _ in 0..4 {
            state.next();
        }
        let rows = state.rows(2, 9);
        assert!(rows[1].selected, "the selection scrolls into view");
        // Every visible entry is still a real candidate, not a sliced one.
        assert!(rows.iter().all(|row| width_of(&row.text) <= 9));
    }

    #[test]
    fn a_narrow_menu_keeps_the_description_after_the_names() {
        let candidates = vec![
            Candidate::new("checkout", CandKind::Command).with_desc("switch branches"),
            Candidate::new("cherry-pick", CandKind::Command).with_desc("apply a commit"),
        ];
        let mut state = MenuState::new(menu_candidates(b"ch", candidates));
        let rows = state.rows(8, 40);
        assert!(rows[0].text.starts_with("> checkout"));
        assert!(rows[0].text.contains("switch branches"));
        assert!(width_of(&rows[0].text) <= 40);
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
