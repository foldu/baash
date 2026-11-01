//! The editor's prompt: the already-rendered `PS1`/`PS2` it paints.
//!
//! The escape table and both expansion passes live in [`baash_lang::prompt`],
//! which `${x@P}` uses too, so the string a user sees and the string a script
//! gets cannot drift. This module is only the editor's container for the
//! result.
//!
//! **The shell renders the prompt, not the editor.** Only the shell has a
//! variable table (`PS1` must come from there, not from the process
//! environment) and only the shell can run `$(…)` when `promptvars` is on, so
//! [`crate::Editor::set_prompt`] is called with a fresh [`Prompt`] before each
//! line is read. A `PS1` assigned by the command just run, by the rc file or
//! by the environment therefore takes effect at the next prompt. The editor
//! itself never reads a variable and never expands anything; a template given
//! to [`Prompt::new`] is expanded once, with a default context, which is what
//! tests and embedders want.
//!
//! The rendered value is an [`ExpandedPrompt`], not bare bytes, so the
//! `\[`/`\]` stretches that must not count toward the display width survive
//! both expansion passes and a coloured prompt with a variable in it still
//! puts the cursor in the right column.

pub use baash_lang::prompt::is_root;
pub use baash_lang::prompt::{ExpandedPrompt, PromptContext as PromptState, expand};

/// The rendered prompts the editor paints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prompt {
    primary: ExpandedPrompt,
    secondary: ExpandedPrompt,
}

impl Prompt {
    /// Expand two templates once, with a default context.
    ///
    /// This is the single-pass rendering (no `promptvars`), which is what a
    /// caller with no interpreter can do. The shell uses
    /// [`Prompt::from_expanded`] after running the real expansion.
    pub fn new(primary: impl Into<Vec<u8>>, secondary: impl Into<Vec<u8>>) -> Self {
        Self::from_template(&primary.into(), &secondary.into(), &PromptState::default())
    }

    /// Expand two templates with an explicit context, single-pass.
    pub fn from_template(primary: &[u8], secondary: &[u8], state: &PromptState) -> Self {
        Self {
            primary: expand(primary, state),
            secondary: expand(secondary, state),
        }
    }

    /// Prompts the shell has already rendered (escapes and `promptvars`).
    pub fn from_expanded(primary: ExpandedPrompt, secondary: ExpandedPrompt) -> Self {
        Self { primary, secondary }
    }

    /// The prompt before the first line of a command.
    pub fn primary_text(&self) -> &ExpandedPrompt {
        &self.primary
    }

    /// The prompt before a continuation line.
    pub fn secondary_text(&self) -> &ExpandedPrompt {
        &self.secondary
    }

    /// The primary prompt as plain bytes, without the width annotations.
    pub fn primary_bytes(&self) -> &[u8] {
        self.primary.bytes()
    }

    /// The secondary prompt as plain bytes, without the width annotations.
    pub fn secondary_bytes(&self) -> &[u8] {
        self.secondary.bytes()
    }
}

impl Default for Prompt {
    fn default() -> Self {
        Self::new(b"$ ".to_vec(), b"> ".to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> PromptState {
        PromptState {
            user: b"ada".to_vec(),
            host: b"lab.example.com".to_vec(),
            cwd: b"/home/ada/work".to_vec(),
            home: b"/home/ada".to_vec(),
            root: false,
            history_number: 42,
            prompt_vars: false,
            ..PromptState::default()
        }
    }

    fn rendered(template: &[u8]) -> Vec<u8> {
        Prompt::from_template(template, b"> ", &state())
            .primary_bytes()
            .to_vec()
    }

    #[test]
    fn the_common_escapes() {
        assert_eq!(rendered(b"\\u@\\h:\\w\\$ "), b"ada@lab:~/work$ ".to_vec());
    }

    #[test]
    fn root_and_literal_escapes() {
        let mut state = state();
        state.root = true;
        assert_eq!(
            Prompt::from_template(b"\\$", b"> ", &state).primary_bytes(),
            b"#"
        );
        assert_eq!(rendered(b"\\\\"), b"\\".to_vec());
        assert_eq!(rendered(b"a\\nb"), b"a\nb".to_vec());
        assert_eq!(rendered(b"\\!"), b"42".to_vec());
    }

    #[test]
    fn full_host_and_basename() {
        assert_eq!(rendered(b"\\H"), b"lab.example.com".to_vec());
        assert_eq!(rendered(b"\\W"), b"work".to_vec());
    }

    #[test]
    fn home_itself_is_a_tilde() {
        let mut state = state();
        state.cwd = b"/home/ada".to_vec();
        assert_eq!(
            Prompt::from_template(b"\\w", b"> ", &state).primary_bytes(),
            b"~"
        );
    }

    #[test]
    fn a_home_prefix_without_a_slash_is_not_abbreviated() {
        let mut state = state();
        state.cwd = b"/home/adam".to_vec();
        assert_eq!(
            Prompt::from_template(b"\\w", b"> ", &state).primary_bytes(),
            b"/home/adam"
        );
    }

    #[test]
    fn unknown_escapes_keep_their_backslash() {
        assert_eq!(rendered(b"\\q"), b"\\q".to_vec());
        assert_eq!(rendered(b"trailing\\"), b"trailing\\".to_vec());
    }

    #[test]
    fn root_cwd_is_its_own_basename() {
        let mut state = state();
        state.cwd = b"/".to_vec();
        assert_eq!(
            Prompt::from_template(b"\\W", b"> ", &state).primary_bytes(),
            b"/"
        );
    }

    #[test]
    fn non_printing_markers_survive_the_adapter() {
        let prompt = Prompt::from_template(b"\\[\\e[31m\\]$ \\[\\e[0m\\]", b"> ", &state());
        assert_eq!(prompt.primary_bytes(), b"\x1b[31m$ \x1b[0m");
        assert_eq!(prompt.primary_text().non_printing().len(), 2);
    }

    #[test]
    fn an_already_expanded_prompt_is_kept_verbatim() {
        let primary = expand(b"\\u\\$ ", &state());
        let secondary = expand(b"\\u> ", &state());
        let prompt = Prompt::from_expanded(primary.clone(), secondary.clone());
        assert_eq!(prompt.primary_text(), &primary);
        assert_eq!(prompt.secondary_text(), &secondary);
        assert_eq!(prompt.primary_bytes(), b"ada$ ");
    }
}
