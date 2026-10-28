//! Completion actions: what `compgen -A NAME` and `complete -A NAME` mean.
//!
//! The *candidates* come from shell state the runtime does not own — variables,
//! functions, aliases, commands on `PATH` — so they are reached through
//! [`CompletionSource`]. Everything that can be decided without asking the
//! shell (which action a name means, how a prefix filters a candidate list)
//! lives here, where it can be tested with a fake source.
//!
//! This is the `compgen` half of programmable completion. The bash grammar for
//! `complete`/`compopt` (`-F`, `-W`, `-o`, `-P`, `-S`, …) is a front end over
//! the same actions and lands with the built-in bodies.

#![forbid(unsafe_code)]

/// A `compgen`/`complete` action: `-A NAME`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CompletionAction {
    /// `alias`
    Alias,
    /// `arrayvar`
    ArrayVar,
    /// `binding`
    Binding,
    /// `builtin`
    Builtin,
    /// `command`
    Command,
    /// `directory`
    Directory,
    /// `disabled`
    Disabled,
    /// `enabled`
    Enabled,
    /// `export`
    Export,
    /// `file`
    File,
    /// `function`
    Function,
    /// `group`
    Group,
    /// `helptopic`
    Helptopic,
    /// `hostname`
    Hostname,
    /// `job`
    Job,
    /// `keyword`
    Keyword,
    /// `running`
    Running,
    /// `service`
    Service,
    /// `setopt`
    Setopt,
    /// `shopt`
    Shopt,
    /// `signal`
    Signal,
    /// `stopped`
    Stopped,
    /// `user`
    User,
    /// `variable`
    Variable,
}

impl CompletionAction {
    /// Every action, in the order `compgen -A` documents them.
    pub const ALL: &'static [CompletionAction] = &[
        CompletionAction::Alias,
        CompletionAction::ArrayVar,
        CompletionAction::Binding,
        CompletionAction::Builtin,
        CompletionAction::Command,
        CompletionAction::Directory,
        CompletionAction::Disabled,
        CompletionAction::Enabled,
        CompletionAction::Export,
        CompletionAction::File,
        CompletionAction::Function,
        CompletionAction::Group,
        CompletionAction::Helptopic,
        CompletionAction::Hostname,
        CompletionAction::Job,
        CompletionAction::Keyword,
        CompletionAction::Running,
        CompletionAction::Service,
        CompletionAction::Setopt,
        CompletionAction::Shopt,
        CompletionAction::Signal,
        CompletionAction::Stopped,
        CompletionAction::User,
        CompletionAction::Variable,
    ];

    /// The `-A` name.
    pub fn name(self) -> &'static str {
        match self {
            CompletionAction::Alias => "alias",
            CompletionAction::ArrayVar => "arrayvar",
            CompletionAction::Binding => "binding",
            CompletionAction::Builtin => "builtin",
            CompletionAction::Command => "command",
            CompletionAction::Directory => "directory",
            CompletionAction::Disabled => "disabled",
            CompletionAction::Enabled => "enabled",
            CompletionAction::Export => "export",
            CompletionAction::File => "file",
            CompletionAction::Function => "function",
            CompletionAction::Group => "group",
            CompletionAction::Helptopic => "helptopic",
            CompletionAction::Hostname => "hostname",
            CompletionAction::Job => "job",
            CompletionAction::Keyword => "keyword",
            CompletionAction::Running => "running",
            CompletionAction::Service => "service",
            CompletionAction::Setopt => "setopt",
            CompletionAction::Shopt => "shopt",
            CompletionAction::Signal => "signal",
            CompletionAction::Stopped => "stopped",
            CompletionAction::User => "user",
            CompletionAction::Variable => "variable",
        }
    }

    /// Parse an `-A` name.
    pub fn from_name(name: &[u8]) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|action| action.name().as_bytes() == name)
    }
}

/// What a shell must be able to list for `compgen`.
///
/// Every method takes the prefix so an implementation can avoid building a
/// list it would only filter, and returns owned bytes because the shell's own
/// storage borrows mutably in other places.
pub trait CompletionSource {
    /// Visible variable names.
    fn variables(&mut self, prefix: &[u8]) -> Vec<Vec<u8>>;
    /// Exported variable names.
    fn exports(&mut self, prefix: &[u8]) -> Vec<Vec<u8>>;
    /// Array variable names.
    fn array_variables(&mut self, prefix: &[u8]) -> Vec<Vec<u8>>;
    /// Function names.
    fn functions(&mut self, prefix: &[u8]) -> Vec<Vec<u8>>;
    /// Alias names.
    fn aliases(&mut self, prefix: &[u8]) -> Vec<Vec<u8>>;
    /// Built-in names (the registry, including executor-owned ones).
    fn builtins(&mut self, prefix: &[u8]) -> Vec<Vec<u8>>;
    /// Shell keywords.
    fn keywords(&mut self, prefix: &[u8]) -> Vec<Vec<u8>>;
    /// `set -o` option names.
    fn setopts(&mut self, prefix: &[u8]) -> Vec<Vec<u8>>;
    /// `shopt` option names.
    fn shopts(&mut self, prefix: &[u8]) -> Vec<Vec<u8>>;
    /// Path components: files, and directories when `directories_only`.
    fn files(&mut self, prefix: &[u8], directories_only: bool) -> Vec<Vec<u8>>;
    /// Command names: functions, built-ins, aliases and `PATH` executables.
    fn commands(&mut self, prefix: &[u8]) -> Vec<Vec<u8>>;
    /// User names.
    fn users(&mut self, prefix: &[u8]) -> Vec<Vec<u8>>;
    /// Group names.
    fn groups(&mut self, prefix: &[u8]) -> Vec<Vec<u8>>;
    /// Host names.
    fn hosts(&mut self, prefix: &[u8]) -> Vec<Vec<u8>>;
    /// Signal names, without the `SIG` prefix, as `compgen -A signal` prints
    /// them.
    fn signals(&mut self, prefix: &[u8]) -> Vec<Vec<u8>>;
    /// `help` topics.
    fn help_topics(&mut self, prefix: &[u8]) -> Vec<Vec<u8>>;
    /// `bind` function names.
    fn bindings(&mut self, prefix: &[u8]) -> Vec<Vec<u8>>;
    /// Job specifications (`%1`, …) of running jobs.
    fn running_jobs(&mut self, prefix: &[u8]) -> Vec<Vec<u8>>;
    /// Job specifications of stopped jobs.
    fn stopped_jobs(&mut self, prefix: &[u8]) -> Vec<Vec<u8>>;
    /// `systemd`/`service` unit names.
    fn services(&mut self, prefix: &[u8]) -> Vec<Vec<u8>>;
    /// Built-ins disabled with `enable -n`.
    fn disabled_builtins(&mut self, prefix: &[u8]) -> Vec<Vec<u8>>;
    /// Enabled built-ins.
    fn enabled_builtins(&mut self, prefix: &[u8]) -> Vec<Vec<u8>>;
}

/// Candidates for one action.
pub fn candidates(
    action: CompletionAction,
    prefix: &[u8],
    source: &mut dyn CompletionSource,
) -> Vec<Vec<u8>> {
    match action {
        CompletionAction::Alias => source.aliases(prefix),
        CompletionAction::ArrayVar => source.array_variables(prefix),
        CompletionAction::Binding => source.bindings(prefix),
        CompletionAction::Builtin => source.builtins(prefix),
        CompletionAction::Command => source.commands(prefix),
        CompletionAction::Directory => source.files(prefix, true),
        CompletionAction::Disabled => source.disabled_builtins(prefix),
        CompletionAction::Enabled => source.enabled_builtins(prefix),
        CompletionAction::Export => source.exports(prefix),
        CompletionAction::File => source.files(prefix, false),
        CompletionAction::Function => source.functions(prefix),
        CompletionAction::Group => source.groups(prefix),
        CompletionAction::Helptopic => source.help_topics(prefix),
        CompletionAction::Hostname => source.hosts(prefix),
        CompletionAction::Job => {
            let mut jobs = source.running_jobs(prefix);
            jobs.extend(source.stopped_jobs(prefix));
            jobs
        }
        CompletionAction::Keyword => source.keywords(prefix),
        CompletionAction::Running => source.running_jobs(prefix),
        CompletionAction::Service => source.services(prefix),
        CompletionAction::Setopt => source.setopts(prefix),
        CompletionAction::Shopt => source.shopts(prefix),
        CompletionAction::Signal => source.signals(prefix),
        CompletionAction::Stopped => source.stopped_jobs(prefix),
        CompletionAction::User => source.users(prefix),
        CompletionAction::Variable => source.variables(prefix),
    }
}

/// Drop everything that does not start with `prefix`, keeping the shell's order.
///
/// Used for the sources that cannot filter themselves; sources that can filter
/// early should, because a `PATH` walk is otherwise wasted work.
pub fn filter_prefix(candidates: &mut Vec<Vec<u8>>, prefix: &[u8]) {
    if prefix.is_empty() {
        return;
    }
    candidates.retain(|candidate| candidate.starts_with(prefix));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        variables: Vec<Vec<u8>>,
        functions: Vec<Vec<u8>>,
        aliases: Vec<Vec<u8>>,
        files: Vec<Vec<u8>>,
        dirs: Vec<Vec<u8>>,
    }

    impl CompletionSource for Fake {
        fn variables(&mut self, prefix: &[u8]) -> Vec<Vec<u8>> {
            let mut out = self.variables.clone();
            filter_prefix(&mut out, prefix);
            out
        }
        fn exports(&mut self, _: &[u8]) -> Vec<Vec<u8>> {
            Vec::new()
        }
        fn array_variables(&mut self, _: &[u8]) -> Vec<Vec<u8>> {
            Vec::new()
        }
        fn functions(&mut self, prefix: &[u8]) -> Vec<Vec<u8>> {
            let mut out = self.functions.clone();
            filter_prefix(&mut out, prefix);
            out
        }
        fn aliases(&mut self, prefix: &[u8]) -> Vec<Vec<u8>> {
            let mut out = self.aliases.clone();
            filter_prefix(&mut out, prefix);
            out
        }
        fn builtins(&mut self, _: &[u8]) -> Vec<Vec<u8>> {
            Vec::new()
        }
        fn keywords(&mut self, _: &[u8]) -> Vec<Vec<u8>> {
            Vec::new()
        }
        fn setopts(&mut self, _: &[u8]) -> Vec<Vec<u8>> {
            Vec::new()
        }
        fn shopts(&mut self, _: &[u8]) -> Vec<Vec<u8>> {
            Vec::new()
        }
        fn files(&mut self, prefix: &[u8], directories_only: bool) -> Vec<Vec<u8>> {
            let mut out = if directories_only {
                self.dirs.clone()
            } else {
                let mut all = self.files.clone();
                all.extend(self.dirs.clone());
                all
            };
            filter_prefix(&mut out, prefix);
            out
        }
        fn commands(&mut self, _: &[u8]) -> Vec<Vec<u8>> {
            Vec::new()
        }
        fn users(&mut self, _: &[u8]) -> Vec<Vec<u8>> {
            Vec::new()
        }
        fn groups(&mut self, _: &[u8]) -> Vec<Vec<u8>> {
            Vec::new()
        }
        fn hosts(&mut self, _: &[u8]) -> Vec<Vec<u8>> {
            Vec::new()
        }
        fn signals(&mut self, _: &[u8]) -> Vec<Vec<u8>> {
            Vec::new()
        }
        fn help_topics(&mut self, _: &[u8]) -> Vec<Vec<u8>> {
            Vec::new()
        }
        fn bindings(&mut self, _: &[u8]) -> Vec<Vec<u8>> {
            Vec::new()
        }
        fn running_jobs(&mut self, _: &[u8]) -> Vec<Vec<u8>> {
            Vec::new()
        }
        fn stopped_jobs(&mut self, _: &[u8]) -> Vec<Vec<u8>> {
            Vec::new()
        }
        fn services(&mut self, _: &[u8]) -> Vec<Vec<u8>> {
            Vec::new()
        }
        fn disabled_builtins(&mut self, _: &[u8]) -> Vec<Vec<u8>> {
            Vec::new()
        }
        fn enabled_builtins(&mut self, _: &[u8]) -> Vec<Vec<u8>> {
            Vec::new()
        }
    }

    #[test]
    fn action_names_round_trip() {
        for action in CompletionAction::ALL {
            assert_eq!(
                CompletionAction::from_name(action.name().as_bytes()),
                Some(*action)
            );
        }
        assert_eq!(CompletionAction::from_name(b"nope"), None);
    }

    #[test]
    fn candidates_are_filtered_by_prefix() {
        let mut source = Fake {
            variables: vec![b"PATH".to_vec(), b"PWD".to_vec(), b"HOME".to_vec()],
            ..Fake::default()
        };
        let got = candidates(CompletionAction::Variable, b"P", &mut source);
        assert_eq!(got, vec![b"PATH".to_vec(), b"PWD".to_vec()]);
        // An empty prefix is everything, not nothing.
        let got = candidates(CompletionAction::Variable, b"", &mut source);
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn directory_action_excludes_plain_files() {
        let mut source = Fake {
            files: vec![b"a.txt".to_vec()],
            dirs: vec![b"adir".to_vec()],
            ..Fake::default()
        };
        let dirs = candidates(CompletionAction::Directory, b"", &mut source);
        assert_eq!(dirs, vec![b"adir".to_vec()]);
        let all = candidates(CompletionAction::File, b"", &mut source);
        assert_eq!(all, vec![b"a.txt".to_vec(), b"adir".to_vec()]);
    }

    #[test]
    fn filter_prefix_keeps_order_and_handles_empty_prefix() {
        let mut candidates = vec![b"beta".to_vec(), b"alpha".to_vec(), b"bison".to_vec()];
        filter_prefix(&mut candidates, b"b");
        assert_eq!(candidates, vec![b"beta".to_vec(), b"bison".to_vec()]);
        filter_prefix(&mut candidates, b"");
        assert_eq!(candidates.len(), 2);
    }
}
