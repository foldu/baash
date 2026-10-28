//! `PS1`/`PS2` expansion for the line editor.
//!
//! The escapes themselves live in [`bsh_lang::prompt`], together with
//! `${x@P}`, so the prompt a user sees and the string a script gets from
//! `${PS1@P}` cannot drift. This module is only the editor's adapter: it
//! reads the templates and the state from the environment and remembers the
//! non-printing stretches so the renderer does not count them.
//!
//! The editor does **not** perform bash's second, double-quoted expansion of
//! the decoded prompt (`shopt promptvars`, on by default in bash). It has no
//! interpreter to expand `$name` or run `$(…)` with, so it renders the decoded
//! text, exactly like `shopt -u promptvars`. The same expander with
//! [`bsh_lang::prompt::expand_with_variables`] supplies the two-pass form to
//! `${x@P}`.
//!
//! The values come from the process environment for this wave: `PS1` and `PS2`
//! themselves, plus the process's user, host, home and working directory.
//! `$PWD` changing inside the shell updates the prompt through
//! [`crate::Editor::set_cwd`], because the process environment is not the
//! shell's variable table.
//!
//! Root is detected from `/proc/self`'s owner, which is the process's effective
//! uid on Linux. When that is not readable we fall back to `USER=root`; when
//! neither answers, the prompt is `$ `.

use std::os::unix::ffi::OsStrExt;

pub use bsh_lang::prompt::is_root;
pub use bsh_lang::prompt::{ExpandedPrompt, expand};

/// The values the prompt escapes read.
///
/// This is [`bsh_lang::prompt::PromptContext`]: the expander is shared with
/// `${x@P}`, so the editor and the interpreter must describe the same facts
/// with the same type.
pub type PromptState = bsh_lang::prompt::PromptContext;

/// `PS1`/`PS2` and the state their escapes read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prompt {
    /// The raw `PS1` template.
    pub primary: Vec<u8>,
    /// The raw `PS2` template.
    pub secondary: Vec<u8>,
    /// Variables the escapes expand from.
    pub state: PromptState,
}

impl Prompt {
    /// A prompt from explicit templates.
    pub fn new(primary: impl Into<Vec<u8>>, secondary: impl Into<Vec<u8>>) -> Self {
        Self {
            primary: primary.into(),
            secondary: secondary.into(),
            state: PromptState::default(),
        }
    }

    /// Read `PS1` and `PS2` from the environment, with shell defaults.
    pub fn from_env() -> Self {
        let mut state = PromptState::from_env();
        // The editor does not run a second expansion, so a `$` in a value is
        // literal and must not be backslash-escaped for a pass that will not
        // happen.
        state.prompt_vars = false;
        let default_primary: &[u8] = if state.root { b"# " } else { b"$ " };
        Self {
            primary: env_bytes("PS1").unwrap_or_else(|| default_primary.to_vec()),
            secondary: env_bytes("PS2").unwrap_or_else(|| b"> ".to_vec()),
            state,
        }
    }

    /// The prompt before the first line of a command.
    pub fn primary_text(&self) -> ExpandedPrompt {
        expand(&self.primary, &self.state)
    }

    /// The prompt before a continuation line.
    pub fn secondary_text(&self) -> ExpandedPrompt {
        expand(&self.secondary, &self.state)
    }

    /// The primary prompt as plain bytes, without the width annotations.
    pub fn primary_bytes(&self) -> Vec<u8> {
        self.primary_text().into_bytes()
    }

    /// The secondary prompt as plain bytes, without the width annotations.
    pub fn secondary_bytes(&self) -> Vec<u8> {
        self.secondary_text().into_bytes()
    }
}

impl Default for Prompt {
    fn default() -> Self {
        Self::new(b"$ ".to_vec(), b"> ".to_vec())
    }
}

fn env_bytes(name: &str) -> Option<Vec<u8>> {
    std::env::var_os(name).map(|value| value.as_bytes().to_vec())
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

    #[test]
    fn the_common_escapes() {
        assert_eq!(
            expand(b"\\u@\\h:\\w\\$ ", &state()).into_bytes(),
            b"ada@lab:~/work$ ".to_vec()
        );
    }

    #[test]
    fn root_and_literal_escapes() {
        let mut state = state();
        state.root = true;
        assert_eq!(expand(b"\\$", &state).into_bytes(), b"#");
        assert_eq!(expand(b"\\\\", &state).into_bytes(), b"\\");
        assert_eq!(expand(b"a\\nb", &state).into_bytes(), b"a\nb");
        assert_eq!(expand(b"\\!", &state).into_bytes(), b"42");
    }

    #[test]
    fn full_host_and_basename() {
        assert_eq!(expand(b"\\H", &state()).into_bytes(), b"lab.example.com");
        assert_eq!(expand(b"\\W", &state()).into_bytes(), b"work");
    }

    #[test]
    fn home_itself_is_a_tilde() {
        let mut state = state();
        state.cwd = b"/home/ada".to_vec();
        assert_eq!(expand(b"\\w", &state).into_bytes(), b"~");
    }

    #[test]
    fn a_home_prefix_without_a_slash_is_not_abbreviated() {
        let mut state = state();
        state.cwd = b"/home/adam".to_vec();
        assert_eq!(expand(b"\\w", &state).into_bytes(), b"/home/adam");
    }

    #[test]
    fn unknown_escapes_keep_their_backslash() {
        assert_eq!(expand(b"\\q", &state()).into_bytes(), b"\\q");
        assert_eq!(expand(b"trailing\\", &state()).into_bytes(), b"trailing\\");
    }

    #[test]
    fn root_cwd_is_its_own_basename() {
        assert_eq!(
            expand(
                b"\\W",
                &PromptState {
                    cwd: b"/".to_vec(),
                    ..state()
                }
            )
            .into_bytes(),
            b"/".to_vec()
        );
    }

    #[test]
    fn non_printing_markers_survive_the_adapter() {
        let expanded = expand(b"\\[\\e[31m\\]$ \\[\\e[0m\\]", &state());
        assert_eq!(expanded.bytes(), b"\x1b[31m$ \x1b[0m");
        assert_eq!(expanded.non_printing().len(), 2);
    }
}
