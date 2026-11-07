//! Programmable completion: the rules `complete` and `compopt` define.
//!
//! `baash-runtime` owns the registry (`CompletionTable`) because the built-ins
//! mutate it and `complete -p` prints from it, and `baash-complete` must not
//! depend on the runtime (`DESIGN.md` §4.1). So the shell hands this crate a
//! [`SpecSource`]: a snapshot of the rules, one lookup by command name. The
//! engine then does what `complete` asked for, from data:
//!
//! * `-W 'a b'` — words filtered by the current word;
//! * `-A name` — an [`Action`], mapped onto the providers this crate already
//!   has;
//! * `-P prefix` / `-S suffix` — decoration applied after filtering, which is
//!   the order bash uses (`compgen -P pre -W 'one' -- o` prints `preone`, and
//!   `-- pre` prints nothing);
//! * `-o nospace|default|bashdefault|dirnames|plusdirs` — the options that
//!   change which candidates exist or how they are inserted.
//!
//! `-F` is a handoff rather than an absence: the name travels in
//! [`Spec::function`] and the shell runs it (see [`crate::Deferred`]).

/// `-o nospace`: do not append a space to a unique match.
pub const OPT_NOSPACE: &[u8] = b"nospace";
/// `-o default`: fall back to the shell's default completion.
pub const OPT_DEFAULT: &[u8] = b"default";
/// `-o bashdefault`: bash's default completion, which for us is the same set.
pub const OPT_BASHDEFAULT: &[u8] = b"bashdefault";
/// `-o dirnames`: fall back to directory completion.
pub const OPT_DIRNAMES: &[u8] = b"dirnames";
/// `-o plusdirs`: add directory candidates to the generated ones.
pub const OPT_PLUSDIRS: &[u8] = b"plusdirs";

/// One command's completion rule, as this crate needs it.
///
/// The shell's registry stores the raw `-W` *text* and the `-F` function name;
/// a [`SpecSource`] resolves the parts that can be resolved without running
/// shell code (the word list is split, not expanded — see
/// `docs/contracts.md`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Spec {
    /// `-A name`, in the order given. Names this crate cannot serve produce no
    /// candidates; see [`Action`].
    pub actions: Vec<Vec<u8>>,
    /// `-W` words, already split on `IFS`.
    pub words: Vec<Vec<u8>>,
    /// `-P prefix`, prepended to every candidate.
    pub prefix: Option<Vec<u8>>,
    /// `-S suffix`, appended to every candidate.
    pub suffix: Option<Vec<u8>>,
    /// `-o option`, in the order given.
    pub options: Vec<Vec<u8>>,
    /// `-F function`.
    ///
    /// The function is shell code, so this crate carries the name and never
    /// runs it. [`crate::CompletionEngine::deferred`] recognises a rule that
    /// names one, [`crate::Deferred`] assembles the input bash gives it, and
    /// the caller — the shell's `ShellCompleter` — runs it and returns
    /// [`crate::Replies`] for
    /// [`crate::CompletionEngine::deferred_menu`].
    pub function: Option<Vec<u8>>,
}

impl Spec {
    /// Whether `-o option` is set.
    pub fn has_option(&self, option: &[u8]) -> bool {
        self.options
            .iter()
            .any(|candidate| candidate.as_slice() == option)
    }

    /// Whether the spec asks for something this crate can generate.
    pub fn has_generator(&self) -> bool {
        !self.actions.is_empty() || !self.words.is_empty()
    }
}

/// Whether `name` is in an `-o` option list.
///
/// The `-F` path carries the options in force *after* `compopt`, so they are a
/// bare list rather than a [`Spec`].
pub fn has_option(options: &[Vec<u8>], name: &[u8]) -> bool {
    options.iter().any(|candidate| candidate.as_slice() == name)
}

/// A `complete -A` action this crate can answer.
///
/// The names match `baash-runtime`'s `CompletionAction`, but the list is the
/// subset the engine can generate from its own providers. Actions outside it
/// (`setopt`, `shopt`, `user`, `group`, `hostname`, `job`, `service`,
/// `binding`, `disabled`, `enabled`, `running`, `stopped`) need shell state this
/// crate does not have; they are accepted by `complete` and simply offer
/// nothing at the prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    /// `-A alias`
    Alias,
    /// `-A arrayvar` (answered from the variable names)
    ArrayVar,
    /// `-A builtin`
    Builtin,
    /// `-A command`: aliases, functions, builtins, keywords and `PATH`.
    Command,
    /// `-A directory`
    Directory,
    /// `-A export` (answered from the variable names)
    Export,
    /// `-A file`
    File,
    /// `-A function`
    Function,
    /// `-A helptopic` (every documented topic is a builtin here)
    Helptopic,
    /// `-A keyword`
    Keyword,
    /// `-A signal`
    Signal,
    /// `-A variable`
    Variable,
}

impl Action {
    /// Every action this crate serves.
    pub const ALL: &'static [Action] = &[
        Action::Alias,
        Action::ArrayVar,
        Action::Builtin,
        Action::Command,
        Action::Directory,
        Action::Export,
        Action::File,
        Action::Function,
        Action::Helptopic,
        Action::Keyword,
        Action::Signal,
        Action::Variable,
    ];

    /// The `-A` name.
    pub fn name(self) -> &'static str {
        match self {
            Action::Alias => "alias",
            Action::ArrayVar => "arrayvar",
            Action::Builtin => "builtin",
            Action::Command => "command",
            Action::Directory => "directory",
            Action::Export => "export",
            Action::File => "file",
            Action::Function => "function",
            Action::Helptopic => "helptopic",
            Action::Keyword => "keyword",
            Action::Signal => "signal",
            Action::Variable => "variable",
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

/// Where [`crate::CompletionEngine`] gets the `complete` rules.
///
/// Implemented by the binary over `baash_runtime`'s registry, because the
/// dependency edge runs the other way. The snapshot is replaced before every
/// prompt, like the other completion sources.
pub trait SpecSource: Send + Sync {
    /// The rule for `command`, or `None` for the default completion.
    fn spec(&self, command: &[u8]) -> Option<Spec>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_names_round_trip() {
        for action in Action::ALL {
            assert_eq!(Action::from_name(action.name().as_bytes()), Some(*action));
        }
        assert_eq!(Action::from_name(b"nope"), None);
        // Actions the runtime accepts but this crate cannot generate are not
        // actions here; the engine skips them instead of guessing.
        assert_eq!(Action::from_name(b"setopt"), None);
    }

    #[test]
    fn options_are_looked_up_by_name() {
        let spec = Spec {
            options: vec![b"nospace".to_vec(), b"plusdirs".to_vec()],
            ..Spec::default()
        };
        assert!(spec.has_option(OPT_NOSPACE));
        assert!(spec.has_option(OPT_PLUSDIRS));
        assert!(!spec.has_option(OPT_DIRNAMES));
        assert!(!spec.has_generator());
    }
}
