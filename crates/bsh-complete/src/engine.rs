//! The engine `bsh-repl` will hold: providers, ranking, and the menu.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use bsh_syntax::ParseOpts;

use crate::candidate::{Candidate, dedupe};
use crate::commands::CommandProvider;
use crate::context::{Ctx, context};
use crate::menu::Menu;
use crate::names::{
    AliasProvider, BuiltinProvider, FunctionProvider, History, HistoryProvider, ShellNames,
    VarNames, VarProvider,
};
use crate::ops::{CaseTagProvider, KeywordProvider, ParamOpProvider, TestOpProvider};
use crate::paths::PathProvider;
use crate::rank::Ranker;
use crate::sources::{self, Provider, ProviderId};

/// What the editor calls.
///
/// The trait exists so `bsh-repl` can hold `Arc<dyn Completer>` and so tests
/// can substitute a fixed menu. It is deliberately synchronous: the first paint
/// must be (`DESIGN.md` §6.1); streaming slower sources is a later, additive
/// concern.
pub trait Completer {
    /// Complete the word at `cursor` in `line`.
    ///
    /// `cursor` is clamped to the line, and a line that does not parse is a
    /// normal input, not an error.
    fn complete(&self, line: &[u8], cursor: usize) -> Menu;
}

/// Knobs for [`CompletionEngine`].
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Maximum candidates returned in one menu.
    pub max_candidates: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // A menu larger than this is unreadable anyway; the ranking has
            // already decided what matters.
            max_candidates: 200,
        }
    }
}

/// The default completer: parser context, the built-in providers, two-tier
/// ranking.
pub struct CompletionEngine {
    opts: ParseOpts,
    config: Config,
    providers: Vec<Arc<dyn Provider>>,
    paths: Arc<PathProvider>,
    commands: Arc<CommandProvider>,
    ranker: Mutex<Ranker>,
}

impl CompletionEngine {
    /// An engine with the built-in providers and no variable or history
    /// source.
    ///
    /// `bsh-repl` adds those with [`CompletionEngine::with_vars`],
    /// [`CompletionEngine::with_history`] and
    /// [`CompletionEngine::with_shell_names`] once the runtime exposes them.
    pub fn new() -> Self {
        let paths = Arc::new(PathProvider::new());
        let commands = Arc::new(CommandProvider::new());
        let mut engine = Self {
            opts: ParseOpts::DEFAULT,
            config: Config::default(),
            providers: Vec::new(),
            paths,
            commands,
            ranker: Mutex::new(Ranker::new()),
        };
        engine.register(engine.paths.clone());
        engine.register(engine.commands.clone());
        for provider in [
            Arc::new(BuiltinProvider) as Arc<dyn Provider>,
            Arc::new(ParamOpProvider),
            Arc::new(TestOpProvider),
            Arc::new(KeywordProvider),
            Arc::new(CaseTagProvider::new()),
        ] {
            engine.register(provider);
        }
        engine
    }

    /// Replace the configuration.
    #[must_use]
    pub fn with_config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }

    /// Parse with an explicit profile.
    ///
    /// A `--posix` profile changes what `expectations_at` accepts, so a
    /// bash-only construct stops producing expectations in POSIX mode.
    #[must_use]
    pub fn with_parse_opts(mut self, opts: ParseOpts) -> Self {
        self.opts = opts;
        self
    }

    /// Resolve relative paths against `root` (the shell's working directory).
    #[must_use]
    pub fn with_root(mut self, root: impl Into<PathBuf>) -> Self {
        let paths = Arc::new(PathProvider::with_root(root));
        self.register(Arc::clone(&paths) as Arc<dyn Provider>);
        self.paths = paths;
        self
    }

    /// Index `PATH` from an explicit value instead of the process environment.
    #[must_use]
    pub fn with_path_var(mut self, path: impl Into<Vec<u8>>) -> Self {
        let commands = Arc::new(CommandProvider::with_path_var(path));
        self.register(Arc::clone(&commands) as Arc<dyn Provider>);
        self.commands = commands;
        self
    }

    /// Complete variables from `names`.
    #[must_use]
    pub fn with_vars(self, names: Arc<dyn VarNames>) -> Self {
        self.with_provider(Arc::new(VarProvider::new(names)))
    }

    /// Complete commands from history.
    #[must_use]
    pub fn with_history(self, history: Arc<dyn History>) -> Self {
        self.with_provider(Arc::new(HistoryProvider::new(history)))
    }

    /// Complete functions and aliases.
    #[must_use]
    pub fn with_shell_names(self, names: Arc<dyn ShellNames>) -> Self {
        let engine = self.with_provider(Arc::new(FunctionProvider::new(Arc::clone(&names))));
        engine.with_provider(Arc::new(AliasProvider::new(names)))
    }

    /// Offer extra `case` patterns alongside `*)`.
    #[must_use]
    pub fn with_case_tags(self, tags: Vec<Vec<u8>>) -> Self {
        self.with_provider(Arc::new(CaseTagProvider::new().with_tags(tags)))
    }

    /// Add or replace a provider.
    #[must_use]
    pub fn with_provider(mut self, provider: Arc<dyn Provider>) -> Self {
        self.register(provider);
        self
    }

    fn register(&mut self, provider: Arc<dyn Provider>) {
        let id = provider.id();
        if let Some(slot) = self.providers.iter_mut().find(|slot| slot.id() == id) {
            *slot = provider;
        } else {
            self.providers.push(provider);
        }
    }

    /// The provider registered for `id`, if any.
    pub fn provider(&self, id: ProviderId) -> Option<&dyn Provider> {
        self.providers
            .iter()
            .find(|provider| provider.id() == id)
            .map(|provider| provider.as_ref())
    }

    /// Drop the cached `PATH` index.
    ///
    /// The index also notices a changed `PATH` by itself on the next
    /// completion; this is for the case where a directory on `PATH` was
    /// replaced without its mtime moving underneath the index.
    pub fn invalidate_commands(&self) {
        self.commands.invalidate();
    }

    /// Change the directory relative paths resolve against (after `cd`).
    pub fn set_root(&self, root: impl Into<PathBuf>) {
        self.paths.set_root(root);
    }

    /// Drop every cached directory listing.
    pub fn invalidate_paths(&self) {
        self.paths.clear_cache();
    }

    /// The context for `line` and `cursor`, without ranking.
    pub fn context(&self, line: &[u8], cursor: usize) -> Ctx {
        context(line, cursor, &self.opts)
    }

    /// Run every applicable provider for `ctx` and rank the result.
    fn gather(&self, ctx: &Ctx) -> Vec<Candidate> {
        let mut candidates = Vec::new();
        for &id in sources::providers_for(ctx.expect) {
            let Some(provider) = self.provider(id) else {
                continue;
            };
            if provider.applies(ctx) {
                candidates.extend(provider.candidates(ctx));
            }
        }
        dedupe(&mut candidates);
        let mut ranker = self
            .ranker
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        ranker.rank(&mut candidates, &ctx.prefix);
        candidates.truncate(self.config.max_candidates);
        candidates
    }
}

impl Default for CompletionEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for CompletionEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompletionEngine")
            .field("parse_opts", &self.opts)
            .field("config", &self.config)
            .field("providers", &self.providers.len())
            .finish_non_exhaustive()
    }
}

impl Completer for CompletionEngine {
    fn complete(&self, line: &[u8], cursor: usize) -> Menu {
        let ctx = self.context(line, cursor);
        let candidates = self.gather(&ctx);
        Menu::new(ctx, candidates)
    }
}
