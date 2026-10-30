//! The engine `bsh-repl` will hold: providers, ranking, and the menu.
//!
//! The engine outlives every prompt — the editor borrows it once for the whole
//! loop — while the shell's variable table, function table, history ring and
//! `complete` registry all change between lines. So every source that can move
//! is a `set_…` method taking `&self` and boxing a snapshot: the shell refreshes
//! the engine in `refresh_prompt`, before the next line is read. The consuming
//! `with_…` builders remain for tests and for a one-shot engine.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use bsh_syntax::ParseOpts;

use crate::candidate::{CandKind, Candidate, dedupe};
use crate::commands::CommandProvider;
use crate::context::{Ctx, context};
use crate::menu::Menu;
use crate::names::{
    AliasProvider, BuiltinProvider, FunctionProvider, History, HistoryProvider, ShellNames,
    VarNames, VarProvider,
};
use crate::ops::{CaseTagProvider, KeywordProvider, ParamOpProvider, TestOpProvider};
use crate::paths::{PathMode, PathProvider};
use crate::rank::Ranker;
use crate::sources::{self, Provider, ProviderId};
use crate::spec::{
    Action, OPT_BASHDEFAULT, OPT_DEFAULT, OPT_DIRNAMES, OPT_NOSPACE, OPT_PLUSDIRS, Spec, SpecSource,
};
use crate::text;

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
    providers: RwLock<Vec<Arc<dyn Provider>>>,
    paths: Arc<PathProvider>,
    commands: Arc<CommandProvider>,
    /// The variable-name source, if the shell supplied one. Kept apart from the
    /// provider list because `complete -A variable` inserts the bare name while
    /// the interactive `$…` completion inserts `$NAME`.
    vars: RwLock<Option<Arc<VarProvider>>>,
    /// `complete`/`compopt` rules for the command being completed.
    specs: RwLock<Option<Arc<dyn SpecSource>>>,
    ranker: Mutex<Ranker>,
}

impl CompletionEngine {
    /// The `PATH` command source, so a caller can build its index on a worker.
    ///
    /// The engine holds it behind an `Arc`; handing it out is what lets the
    /// REPL warm the index without borrowing the engine from the thread that
    /// reads keystrokes.
    pub fn commands(&self) -> Arc<CommandProvider> {
        Arc::clone(&self.commands)
    }

    /// An engine with the built-in providers and no variable, history or
    /// `complete` source.
    ///
    /// The shell installs those with [`CompletionEngine::set_vars`],
    /// [`CompletionEngine::set_history`],
    /// [`CompletionEngine::set_shell_names`] and
    /// [`CompletionEngine::set_specs`] before every prompt.
    pub fn new() -> Self {
        let paths = Arc::new(PathProvider::new());
        let commands = Arc::new(CommandProvider::new());
        let engine = Self {
            opts: ParseOpts::DEFAULT,
            config: Config::default(),
            providers: RwLock::new(Vec::new()),
            paths: Arc::clone(&paths),
            commands: Arc::clone(&commands),
            vars: RwLock::new(None),
            specs: RwLock::new(None),
            ranker: Mutex::new(Ranker::new()),
        };
        engine.set_provider(Arc::clone(&paths) as Arc<dyn Provider>);
        engine.set_provider(Arc::clone(&commands) as Arc<dyn Provider>);
        for provider in [
            Arc::new(BuiltinProvider) as Arc<dyn Provider>,
            Arc::new(ParamOpProvider),
            Arc::new(TestOpProvider),
            Arc::new(KeywordProvider),
            Arc::new(CaseTagProvider::new()),
        ] {
            engine.set_provider(provider);
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
    pub fn with_root(self, root: impl Into<PathBuf>) -> Self {
        self.paths.set_root(root);
        self
    }

    /// Index `PATH` from an explicit value instead of the process environment.
    #[must_use]
    pub fn with_path_var(self, path: impl Into<Vec<u8>>) -> Self {
        self.set_path_var(Some(path.into()));
        self
    }

    /// Complete variables from `names`.
    #[must_use]
    pub fn with_vars(self, names: Arc<dyn VarNames>) -> Self {
        self.set_vars(names);
        self
    }

    /// Complete commands from history.
    #[must_use]
    pub fn with_history(self, history: Arc<dyn History>) -> Self {
        self.set_history(history);
        self
    }

    /// Complete functions and aliases.
    #[must_use]
    pub fn with_shell_names(self, names: Arc<dyn ShellNames>) -> Self {
        self.set_shell_names(names);
        self
    }

    /// Offer extra `case` patterns alongside `*)`.
    #[must_use]
    pub fn with_case_tags(self, tags: Vec<Vec<u8>>) -> Self {
        self.set_case_tags(tags);
        self
    }

    /// Use `source` for the `complete` rules.
    #[must_use]
    pub fn with_specs(self, source: Arc<dyn SpecSource>) -> Self {
        self.set_specs(source);
        self
    }

    /// Add or replace a provider.
    #[must_use]
    pub fn with_provider(self, provider: Arc<dyn Provider>) -> Self {
        self.set_provider(provider);
        self
    }

    /// Resolve `~` against `home`.
    #[must_use]
    pub fn with_home(self, home: impl Into<PathBuf>) -> Self {
        self.paths.set_home(Some(home.into()));
        self
    }

    /// Resolve relative paths against `root` (after `cd`, or before a prompt).
    pub fn set_root(&self, root: impl Into<PathBuf>) {
        self.paths.set_root(root);
    }

    /// Replace the directory `~` expands to, as the shell's `HOME` says.
    pub fn set_home(&self, home: Option<PathBuf>) {
        self.paths.set_home(home);
    }

    /// Replace the `PATH` the command index searches.
    ///
    /// The shell passes its own `PATH` variable, not the process environment:
    /// `PATH=/nowhere` must change what Tab offers.
    pub fn set_path_var(&self, path: Option<Vec<u8>>) {
        self.commands.set_path_var(path);
    }

    /// Replace the variable names (`$HO<TAB>` and `-A variable`).
    pub fn set_vars(&self, names: Arc<dyn VarNames>) {
        let provider = Arc::new(VarProvider::new(names));
        *write(&self.vars) = Some(Arc::clone(&provider));
        self.set_provider(provider);
    }

    /// Replace the history ring used for command completion.
    pub fn set_history(&self, history: Arc<dyn History>) {
        self.set_provider(Arc::new(HistoryProvider::new(history)));
    }

    /// Replace the function and alias tables.
    pub fn set_shell_names(&self, names: Arc<dyn ShellNames>) {
        self.set_provider(Arc::new(FunctionProvider::new(Arc::clone(&names))));
        self.set_provider(Arc::new(AliasProvider::new(names)));
    }

    /// Replace the `case` tags offered alongside `*)`.
    pub fn set_case_tags(&self, tags: Vec<Vec<u8>>) {
        self.set_provider(Arc::new(CaseTagProvider::new().with_tags(tags)));
    }

    /// Replace the programmable-completion rules (`complete`).
    pub fn set_specs(&self, source: Arc<dyn SpecSource>) {
        *write(&self.specs) = Some(source);
    }

    /// Add or replace a provider.
    pub fn set_provider(&self, provider: Arc<dyn Provider>) {
        let id = provider.id();
        let mut providers = write(&self.providers);
        if let Some(slot) = providers.iter_mut().find(|slot| slot.id() == id) {
            *slot = provider;
        } else {
            providers.push(provider);
        }
    }

    /// The provider registered for `id`, if any.
    pub fn provider(&self, id: ProviderId) -> Option<Arc<dyn Provider>> {
        read(&self.providers)
            .iter()
            .find(|provider| provider.id() == id)
            .cloned()
    }

    /// A snapshot of the provider list, so completion does not hold the lock
    /// while it touches the filesystem.
    fn provider_list(&self) -> Vec<Arc<dyn Provider>> {
        read(&self.providers).clone()
    }

    /// Drop the cached `PATH` index.
    ///
    /// The index also notices a changed `PATH` by itself on the next
    /// completion; this is for the case where a directory on `PATH` was
    /// replaced without its mtime moving underneath the index.
    pub fn invalidate_commands(&self) {
        self.commands.invalidate();
    }

    /// Drop every cached directory listing.
    pub fn invalidate_paths(&self) {
        self.paths.clear_cache();
    }

    /// The context for `line` and `cursor`, without ranking.
    pub fn context(&self, line: &[u8], cursor: usize) -> Ctx {
        context(line, cursor, &self.opts)
    }

    /// The `complete` rule that applies to `ctx`, if any.
    ///
    /// A rule is only consulted for an argument of a named command: the command
    /// word itself is completed by the command union, which is what bash does.
    /// When the shell defines no rule, the built-in one for `cd`-like commands
    /// applies (`-A directory` with a `default` fallback, so `cd $HO<TAB>`
    /// still reaches the variable provider).
    fn spec_for(&self, ctx: &Ctx) -> Option<Spec> {
        // Redirection targets and assignment values are files, not the
        // command's arguments, even though they carry an argument index.
        if ctx.arg_index == 0 || ctx.param || ctx.arith || ctx.heredoc || ctx.redirect || ctx.assign
        {
            return None;
        }
        let command = ctx.command_name.as_deref()?;
        if let Some(spec) = read(&self.specs)
            .as_ref()
            .and_then(|source| source.spec(command))
        {
            return Some(spec);
        }
        builtin_spec(command)
    }

    /// Run every applicable provider for `ctx` and rank the result.
    fn gather(&self, ctx: &Ctx) -> Vec<Candidate> {
        if let Some(spec) = self.spec_for(ctx) {
            return self.spec_menu(&spec, ctx);
        }
        let mut candidates = self.default_candidates(ctx, &self.provider_list());
        dedupe(&mut candidates);
        let mut ranker = lock(&self.ranker);
        ranker.rank(&mut candidates, &ctx.prefix);
        candidates.truncate(self.config.max_candidates);
        candidates
    }

    /// The candidates the expectation table asks for, unfiltered.
    fn default_candidates(&self, ctx: &Ctx, providers: &[Arc<dyn Provider>]) -> Vec<Candidate> {
        let mut candidates = Vec::new();
        for &id in sources::providers_for(ctx.expect) {
            let Some(provider) = providers.iter().find(|provider| provider.id() == id) else {
                continue;
            };
            if provider.applies(ctx) {
                candidates.extend(provider.candidates(ctx));
            }
        }
        candidates
    }

    /// The menu for a programmable-completion rule.
    ///
    /// Candidates here are already filtered and decorated, so they are ordered
    /// the way `complete` orders them (alphabetically unless `-o nosort`) and
    /// never fuzzy-matched: `-P`/`-S` change the text after matching, which is
    /// bash's order and would otherwise make every decorated candidate look
    /// like a mismatch.
    fn spec_menu(&self, spec: &Spec, ctx: &Ctx) -> Vec<Candidate> {
        let mut candidates = self.spec_candidates(spec, ctx, &self.provider_list());
        dedupe(&mut candidates);
        if !spec.has_option(b"nosort") {
            candidates.sort_by(|a, b| {
                a.display
                    .cmp(&b.display)
                    .then_with(|| a.insert.cmp(&b.insert))
            });
        }
        candidates.truncate(self.config.max_candidates);
        candidates
    }

    /// Generate one spec's candidates.
    fn spec_candidates(
        &self,
        spec: &Spec,
        ctx: &Ctx,
        providers: &[Arc<dyn Provider>],
    ) -> Vec<Candidate> {
        let mut candidates = Vec::new();
        for word in &spec.words {
            if text::starts_with(word, &ctx.prefix) {
                candidates.push(
                    Candidate::new(word.clone(), CandKind::Other).with_desc("completion word"),
                );
            }
        }
        for name in &spec.actions {
            if let Some(action) = Action::from_name(name) {
                candidates.extend(self.action_candidates(action, ctx, providers));
            }
        }
        keep_prefixed(&mut candidates, &ctx.prefix);

        if candidates.is_empty() {
            if spec.has_option(OPT_DEFAULT) || spec.has_option(OPT_BASHDEFAULT) {
                candidates = self.default_candidates(ctx, providers);
                keep_prefixed(&mut candidates, &ctx.prefix);
            } else if spec.has_option(OPT_DIRNAMES) {
                candidates = self.directory_candidates(ctx);
                keep_prefixed(&mut candidates, &ctx.prefix);
            }
        }
        if spec.has_option(OPT_PLUSDIRS) {
            let mut directories = self.directory_candidates(ctx);
            keep_prefixed(&mut directories, &ctx.prefix);
            candidates.extend(directories);
        }

        // `-P`/`-S` decorate after matching: `compgen -P pre -W 'one' -- o`
        // prints `preone`, and `-- pre` prints nothing.
        if spec.prefix.is_some() || spec.suffix.is_some() {
            for candidate in &mut candidates {
                let mut insert = Vec::with_capacity(
                    candidate.insert.len()
                        + spec.prefix.as_ref().map_or(0, Vec::len)
                        + spec.suffix.as_ref().map_or(0, Vec::len),
                );
                if let Some(prefix) = &spec.prefix {
                    insert.extend_from_slice(prefix);
                }
                insert.extend_from_slice(&candidate.insert);
                if let Some(suffix) = &spec.suffix {
                    insert.extend_from_slice(suffix);
                }
                candidate.display = String::from_utf8_lossy(&insert).into_owned();
                candidate.insert = insert;
            }
        }
        if spec.has_option(OPT_NOSPACE) {
            for candidate in &mut candidates {
                candidate.append_space = false;
            }
        }
        candidates
    }

    /// Candidates for one `-A` action.
    fn action_candidates(
        &self,
        action: Action,
        ctx: &Ctx,
        providers: &[Arc<dyn Provider>],
    ) -> Vec<Candidate> {
        let from = |id: ProviderId| -> Vec<Candidate> {
            providers
                .iter()
                .find(|provider| provider.id() == id)
                .map(|provider| provider.candidates(ctx))
                .unwrap_or_default()
        };
        match action {
            Action::Alias => from(ProviderId::Alias),
            Action::Function => from(ProviderId::Function),
            Action::Builtin | Action::Helptopic => from(ProviderId::Builtin),
            Action::Command => {
                let mut candidates = Vec::new();
                for id in [ProviderId::Alias, ProviderId::Function, ProviderId::Builtin] {
                    candidates.extend(from(id));
                }
                candidates.extend(keyword_candidates());
                candidates.extend(from(ProviderId::Command));
                candidates
            }
            Action::Keyword => keyword_candidates(),
            Action::Signal => sources::SIGNALS
                .iter()
                .map(|name| {
                    Candidate::new(*name, CandKind::Other)
                        .with_desc("signal")
                        .appending_space()
                })
                .collect(),
            Action::Variable | Action::Export | Action::ArrayVar => self.variable_names(),
            Action::File => self.paths.candidates_in(ctx, PathMode::Any),
            Action::Directory => self.directory_candidates(ctx),
        }
    }

    /// Variable names as `-A variable` wants them: bare, without the `$`.
    fn variable_names(&self) -> Vec<Candidate> {
        let Some(vars) = read(&self.vars).clone() else {
            return Vec::new();
        };
        vars.names()
            .into_iter()
            .map(|name| {
                Candidate::new(name, CandKind::Variable)
                    .with_desc("variable")
                    .appending_space()
            })
            .collect()
    }

    /// Directory entries for `-A directory` and `-o plusdirs`.
    fn directory_candidates(&self, ctx: &Ctx) -> Vec<Candidate> {
        self.paths.candidates_in(ctx, PathMode::Directories)
    }
}

/// The rule a shell-less engine knows: `cd` and friends complete directories,
/// with the ordinary completion as the fallback (`-o default`).
///
/// This is a `complete` rule expressed as data, not a special case in the
/// provider: `cd -`-style options and `cd $HO` keep working because the fallback
/// runs the normal provider table when no directory matches.
fn builtin_spec(command: &[u8]) -> Option<Spec> {
    if sources::is_one_of(command, sources::DIRECTORY_COMMANDS) {
        Some(Spec {
            actions: vec![b"directory".to_vec()],
            options: vec![OPT_DEFAULT.to_vec()],
            ..Spec::default()
        })
    } else {
        None
    }
}

/// Reserved words as candidates, for command position and `-A keyword`.
///
/// Source candidates, not literals: `[[`, `{` and `!` are syntax. Inserting
/// them through `bsh_lang::quote` would escape the bracket or brace and turn
/// the keyword into an ordinary word.
fn keyword_candidates() -> Vec<Candidate> {
    sources::COMMAND_KEYWORDS
        .iter()
        .map(|keyword| {
            Candidate::source(*keyword, CandKind::Keyword)
                .with_desc("keyword")
                .appending_space()
        })
        .collect()
}

/// Drop candidates that do not start with the typed prefix.
fn keep_prefixed(candidates: &mut Vec<Candidate>, prefix: &[u8]) {
    if prefix.is_empty() {
        return;
    }
    candidates.retain(|candidate| text::starts_with(&candidate.insert, prefix));
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|error| error.into_inner())
}

fn write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|error| error.into_inner())
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
            .field("providers", &read(&self.providers).len())
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
