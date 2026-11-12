//! The stock prompt's live state: directory, git facts and the watcher.
//!
//! The prompt the shell renders and the prompt the editor keeps painting are
//! the same bytes, but only the editor is running while the user is thinking.
//! This type owns everything the stock prompt needs between lines:
//!
//! * the abbreviated directory ([`PathAbbreviator`]), recomputed once per
//!   `cd`, not once per repaint;
//! * the git branch and counters ([`GitSegment`]), where the branch is an
//!   exact `.git/HEAD` read and the counters are the worker's last completed
//!   `git status` (`docs/prompt-watch.md` §2);
//! * the working-tree watcher, so a file created while the line is idle makes
//!   the counters refresh and repaint.
//!
//! It holds no shell state: `PWD`, `HOME` and whether the user is root are
//! copied when the prompt is armed. That is what lets the editor borrow it for
//! as long as a line is being read without borrowing the interpreter, and why
//! no shell code (a `PROMPT_COMMAND`, a substitution) can run between
//! keystrokes.

use std::path::{Path, PathBuf};

use baash_lang::prompt::{ExpandedPrompt, PathAbbreviator, PromptContext};
use baash_repl::{Editor, LivePrompt, Prompt};

use crate::git_prompt::GitSegment;

/// The stock prompt's state, owned by the interactive loop.
pub struct PromptState {
    git: GitSegment,
    paths: PathAbbreviator,
    /// The directory the prompt describes, as a path (for the watcher and the
    /// git lookup).
    cwd: PathBuf,
    /// The same directory as shell bytes (for display and `HOME` matching).
    cwd_bytes: Vec<u8>,
    home: Vec<u8>,
    root: bool,
    color: bool,
    /// `user@host` when the session is remote, captured when the prompt was
    /// armed: an SSH session does not become local halfway through a line.
    ssh: Option<(Vec<u8>, Vec<u8>)>,
    /// `PS2`, which a git change never alters but a [`Prompt`] has to carry.
    secondary: Vec<u8>,
    /// The prompt as last handed to the editor, so an unchanged recompute is
    /// not a repaint.
    last: Vec<u8>,
    /// The working-tree watch. Linux-only: the editor's `await_input` knows how
    /// to wait on it beside the keyboard, and no other platform has an
    /// equivalent in this shell yet.
    #[cfg(target_os = "linux")]
    watcher: Option<baash_sys::watch::Watcher>,
    /// The directory the current watcher is armed for; a `cd` re-arms.
    #[cfg(target_os = "linux")]
    armed_for: Option<Vec<u8>>,
    /// Whether the editor has already been given this watcher's descriptor.
    #[cfg(target_os = "linux")]
    adopted: bool,
}

impl PromptState {
    /// A prompt state with an empty git cache and no watcher.
    pub fn new() -> Self {
        Self {
            git: GitSegment::new(),
            paths: PathAbbreviator::default(),
            cwd: PathBuf::new(),
            cwd_bytes: Vec::new(),
            home: Vec::new(),
            root: false,
            color: false,
            ssh: None,
            secondary: Vec::new(),
            last: Vec::new(),
            #[cfg(target_os = "linux")]
            watcher: None,
            #[cfg(target_os = "linux")]
            armed_for: None,
            #[cfg(target_os = "linux")]
            adopted: false,
        }
    }

    /// Point the state at `cwd` without arming the stock prompt.
    ///
    /// A user-defined `PS1` still gets the legacy `[branch]` suffix, so the
    /// git cache has to follow the directory even when the stock prompt is not
    /// in use.
    pub fn tick(&mut self, cwd: &Path) {
        if self.cwd != cwd {
            self.cwd = cwd.to_path_buf();
            self.last.clear();
        }
        self.git.tick(cwd);
    }

    /// Copy the prompt context for the stock prompt and arm the watcher.
    ///
    /// Re-arming is skipped while the directory is unchanged: inotify watches
    /// are per directory, so rebuilding them per prompt would be a few hundred
    /// syscalls between every command for no new information.
    pub fn arm(&mut self, context: &PromptContext, color: bool) {
        self.home.clone_from(&context.home);
        self.root = context.root;
        self.color = color;
        self.ssh = baash_lang::prompt::ssh_context(&context.user, &context.host);

        if self.cwd_bytes != context.cwd {
            self.cwd_bytes.clone_from(&context.cwd);
            self.cwd = baash_sys::os::path_from_bytes(&context.cwd);
            self.last.clear();
            self.git.tick(&self.cwd);
        }

        #[cfg(target_os = "linux")]
        {
            if self.armed_for.as_deref() == Some(context.cwd.as_slice()) {
                return;
            }
            self.armed_for = Some(context.cwd.clone());
            let limits = baash_sys::watch::Limits::default();
            // No repository, or one too big to watch, is not an error: the
            // prompt is then computed once per prompt instead of observed
            // (`baash_sys::watch::Why`).
            self.watcher = baash_sys::watch::watch_worktree(
                &baash_sys::os::path_from_bytes(&context.cwd),
                limits,
            )
            .ok();
            self.adopted = false;
        }
    }

    /// Give the editor the descriptor to await. Only the first call after
    /// arming does anything.
    pub fn adopt(&mut self, editor: &mut Editor<'_>) {
        self.secondary = editor.prompt().secondary_bytes().to_vec();
        #[cfg(target_os = "linux")]
        {
            if self.adopted {
                return;
            }
            let dup = self
                .watcher
                .as_ref()
                .and_then(|watcher| watcher.as_fd().try_clone_to_owned().ok());
            if let Some(dup) = dup {
                editor.set_watch(Some(dup));
                self.adopted = true;
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = editor;
    }

    /// Stop watching: the prompt is the user's own now.
    pub fn disarm(&mut self, editor: &mut Editor<'_>) {
        #[cfg(target_os = "linux")]
        {
            if self.watcher.take().is_some() || self.armed_for.take().is_some() {
                editor.set_watch(None);
            }
            self.adopted = false;
        }
        #[cfg(not(target_os = "linux"))]
        let _ = editor;
    }

    /// Compose the stock prompt from the cached facts, and remember it.
    pub fn primary(&mut self) -> ExpandedPrompt {
        let primary = self.compose();
        self.last = primary.bytes().to_vec();
        primary
    }

    /// The legacy `[branch ~N]` suffix for a user-defined `PS1`.
    pub fn legacy_segment(&self) -> Option<Vec<u8>> {
        self.git.legacy()
    }

    /// Build the stock prompt from the cache without touching [`Self::last`].
    fn compose(&mut self) -> ExpandedPrompt {
        let facts = self.git.facts();
        let directory = self.paths.render(&self.cwd_bytes, &self.home).to_vec();
        let ssh = self
            .ssh
            .as_ref()
            .map(|(user, host)| (user.as_slice(), host.as_slice()));
        baash_lang::prompt::tide_prompt(
            &directory,
            facts.branch.as_deref(),
            facts.status,
            ssh,
            self.root,
            self.color,
        )
    }

    /// Recompute and answer the editor only when the bytes changed.
    fn repaint_if_changed(&mut self) -> Option<Prompt> {
        let next = self.compose();
        if next.bytes() == self.last {
            return None;
        }
        self.last = next.bytes().to_vec();
        Some(Prompt::from_expanded(
            next,
            ExpandedPrompt::from_bytes(self.secondary.clone()),
        ))
    }
}

impl Default for PromptState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "linux")]
impl LivePrompt for PromptState {
    fn refresh(&mut self) -> Option<Prompt> {
        // Draining is what re-arms the poll: readiness stays set until the
        // queue is empty, so a refresh that did not read would spin.
        let changed = self
            .watcher
            .as_mut()
            .is_some_and(|watcher| watcher.pending());
        if changed {
            // A real change: worth asking git for a new count. The request is
            // rate-limited and coalesced, so an editor save that touches a
            // hundred paths is still one `git status`.
            self.git.tick(&self.cwd);
        } else {
            // The editor's one-second backstop, or a wake-up with nothing in
            // the queue: install a refresh that has finished since the last
            // frame. This is how a worker result eventually reaches the screen
            // without a keystroke.
            self.git.poll();
        }
        self.repaint_if_changed()
    }
}

#[cfg(not(target_os = "linux"))]
impl LivePrompt for PromptState {
    fn refresh(&mut self) -> Option<Prompt> {
        // No watch is armed on this platform, so the editor never asks.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unarmed_state_has_no_legacy_segment() {
        let mut state = PromptState::new();
        let root = std::env::temp_dir().join(format!("baash-prompt-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        state.tick(&root);
        assert_eq!(state.legacy_segment(), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
