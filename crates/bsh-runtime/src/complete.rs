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

    /// The single-letter form `complete -p` prints instead of `-A name`.
    ///
    /// Measured against bash 5.3.20: it has a short letter for some actions and
    /// falls back to `-A name` for the rest, so a printer that always wrote
    /// `-A` would not round-trip a user's own `complete -f`.
    pub fn short(self) -> Option<u8> {
        match self {
            CompletionAction::Alias => Some(b'a'),
            CompletionAction::Binding => Some(b'b'),
            CompletionAction::Builtin => Some(b'b'),
            CompletionAction::Command => Some(b'c'),
            CompletionAction::Directory => Some(b'd'),
            CompletionAction::Export => Some(b'e'),
            CompletionAction::File => Some(b'f'),
            CompletionAction::Group => Some(b'g'),
            CompletionAction::Job => Some(b'j'),
            CompletionAction::Keyword => Some(b'k'),
            CompletionAction::Service => Some(b's'),
            CompletionAction::User => Some(b'u'),
            CompletionAction::Variable => Some(b'v'),
            _ => None,
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

/// A `complete` specification: what to offer for a command name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompletionSpec {
    /// `-A action`, repeatable.
    pub actions: Vec<CompletionAction>,
    /// `-W 'word list'`: split on `IFS` and expand.
    pub wordlist: Option<Vec<u8>>,
    /// `-F name`: a shell function that emits candidates.
    pub function: Option<Vec<u8>>,
    /// `-C name`: a command whose output is the candidate list.
    pub command: Option<Vec<u8>>,
    /// `-G glob`: filter generated candidates through a pattern.
    pub glob: Option<Vec<u8>>,
    /// `-P prefix`: prepend to every candidate.
    pub prefix: Option<Vec<u8>>,
    /// `-S suffix`: append to every candidate.
    pub suffix: Option<Vec<u8>>,
    /// `-X filter`: remove candidates matching a pattern.
    pub filter: Option<Vec<u8>>,
    /// `-o option`, repeatable: `nospace`, `filenames`, …
    pub options: Vec<Vec<u8>>,
}

impl CompletionSpec {
    /// Whether this spec would offer nothing.
    pub fn is_empty(&self) -> bool {
        // `-P`/`-S`/`-G` alone are a valid spec in bash (they decorate the
        // candidates another source produces), so they count as content.
        self.actions.is_empty()
            && self.wordlist.is_none()
            && self.function.is_none()
            && self.command.is_none()
            && self.glob.is_none()
            && self.prefix.is_none()
            && self.suffix.is_none()
            && self.filter.is_none()
            && self.options.is_empty()
    }

    /// Render this spec the way `complete -p` prints it, re-input-safe.
    pub fn render(&self, name: &[u8]) -> Vec<u8> {
        let mut out = b"complete".to_vec();
        for option in &self.options {
            out.extend_from_slice(b" -o ");
            out.extend_from_slice(option);
        }
        for action in &self.actions {
            match action.short() {
                Some(letter) => {
                    out.extend_from_slice(b" -");
                    out.push(letter);
                }
                None => {
                    out.extend_from_slice(b" -A ");
                    out.extend_from_slice(action.name().as_bytes());
                }
            }
        }
        if let Some(glob) = &self.glob {
            out.extend_from_slice(b" -G ");
            out.extend_from_slice(&quote(glob));
        }
        if let Some(wordlist) = &self.wordlist {
            out.extend_from_slice(b" -W ");
            out.extend_from_slice(&quote(wordlist));
        }
        // A function or command name cannot contain whitespace in bash, so it
        // is printed bare; the value options are always quoted.
        if let Some(function) = &self.function {
            out.extend_from_slice(b" -F ");
            out.extend_from_slice(function);
        }
        if let Some(command) = &self.command {
            out.extend_from_slice(b" -C ");
            out.extend_from_slice(command);
        }
        if let Some(prefix) = &self.prefix {
            out.extend_from_slice(b" -P ");
            out.extend_from_slice(&quote(prefix));
        }
        if let Some(suffix) = &self.suffix {
            out.extend_from_slice(b" -S ");
            out.extend_from_slice(&quote(suffix));
        }
        if let Some(filter) = &self.filter {
            out.extend_from_slice(b" -X ");
            out.extend_from_slice(&quote(filter));
        }
        out.push(b' ');
        // The command name is printed as given: bash does not quote it, and
        // quoting a plain name would make `complete -p` output look unlike
        // anything a user would write.
        out.extend_from_slice(name);
        out
    }
}

/// Single-quote a word for re-input, escaping embedded single quotes.
fn quote(word: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(word.len() + 2);
    out.push(b'\'');
    for &byte in word {
        if byte == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(byte);
        }
    }
    out.push(b'\'');
    out
}

/// What one `complete` invocation asks for.
///
/// `Set` is much larger than the other variants. That is fine here: a request
/// exists for the duration of one built-in call and is never stored in a
/// collection, so boxing it would cost an allocation per invocation to save
/// nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// `complete` or `complete -p [name…]`: print specs.
    Print {
        /// Empty prints every spec.
        names: Vec<Vec<u8>>,
    },
    /// A spec applies to these names; `-D` means the default spec.
    Set {
        /// The spec.
        spec: CompletionSpec,
        /// The command names it applies to, in the order given.
        names: Vec<Vec<u8>>,
        /// `-D`: the default spec, used when no name matches.
        default: bool,
        /// `-E`: the empty-line spec.
        empty: bool,
        /// `-I`: the initial-word spec.
        initial: bool,
    },
    /// `complete -r [name…]`: remove specs (all of them, without names).
    Remove {
        /// Empty removes every spec.
        names: Vec<Vec<u8>>,
    },
}

/// Parse `complete`'s arguments (without the command name).
pub fn parse_complete(args: &[&[u8]]) -> Result<Request, String> {
    let mut spec = CompletionSpec::default();
    let mut names: Vec<Vec<u8>> = Vec::new();
    let mut print = false;
    let mut remove = false;
    let mut default = false;
    let mut empty = false;
    let mut initial = false;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index];
        index += 1;
        let mut take = |what: &str| -> Result<Vec<u8>, String> {
            let Some(value) = args.get(index) else {
                return Err(format!("complete: {what} requires a value"));
            };
            index += 1;
            Ok(value.to_vec())
        };
        match arg {
            b"-p" => print = true,
            b"-r" => remove = true,
            b"-D" => default = true,
            b"-E" => empty = true,
            b"-I" => initial = true,
            b"-A" => {
                let value = take("-A")?;
                let action = CompletionAction::from_name(&value)
                    .ok_or_else(|| format!("complete: {}: invalid action", show(&value)))?;
                spec.actions.push(action);
            }
            b"-W" => spec.wordlist = Some(take("-W")?),
            b"-F" => spec.function = Some(take("-F")?),
            b"-C" => spec.command = Some(take("-C")?),
            b"-G" => spec.glob = Some(take("-G")?),
            b"-P" => spec.prefix = Some(take("-P")?),
            b"-S" => spec.suffix = Some(take("-S")?),
            b"-X" => spec.filter = Some(take("-X")?),
            b"-o" => spec.options.push(take("-o")?),
            b"--" => {
                names.extend(args[index..].iter().map(|value| value.to_vec()));
                index = args.len();
            }
            other if other.starts_with(b"-") && other.len() > 1 => {
                return Err(format!("complete: {}: invalid option", show(other)));
            }
            other => names.push(other.to_vec()),
        }
    }

    if remove {
        return Ok(Request::Remove { names });
    }
    // `complete` with no arguments prints; `complete -p` prints; anything that
    // carries an action or a name sets.
    if print
        || (names.is_empty() && spec == CompletionSpec::default() && !default && !empty && !initial)
    {
        return Ok(Request::Print { names });
    }
    if spec.is_empty() {
        // A name with no action is an error, not a silent no-op: a user who
        // typed `complete foo` wanted something and got nothing.
        return Err("complete: no action given".to_string());
    }
    if spec.function.is_some() && !default && !empty && !initial && names.is_empty() {
        return Err("complete: -F requires a command name".to_string());
    }
    Ok(Request::Set {
        spec,
        names,
        default,
        empty,
        initial,
    })
}

fn show(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The specs a shell holds, in insertion order (`complete -p` prints them in
/// the order they were defined, so a map would reorder them).
#[derive(Clone, Debug, Default)]
pub struct CompletionTable {
    entries: Vec<(Vec<u8>, CompletionSpec)>,
    default: Option<CompletionSpec>,
    empty: Option<CompletionSpec>,
    initial: Option<CompletionSpec>,
}

impl CompletionTable {
    /// Apply one request, returning output lines for the printing forms.
    pub fn apply(&mut self, request: &Request) -> Result<Vec<Vec<u8>>, String> {
        match request {
            Request::Print { names } => Ok(self.print(names)),
            Request::Remove { names } => {
                if names.is_empty() {
                    self.entries.clear();
                    self.default = None;
                    self.empty = None;
                    self.initial = None;
                } else {
                    for name in names {
                        self.entries.retain(|(existing, _)| existing != name);
                    }
                }
                Ok(Vec::new())
            }
            Request::Set {
                spec,
                names,
                default,
                empty,
                initial,
            } => {
                if *default {
                    self.default = Some(spec.clone());
                }
                if *empty {
                    self.empty = Some(spec.clone());
                }
                if *initial {
                    self.initial = Some(spec.clone());
                }
                for name in names {
                    match self
                        .entries
                        .iter_mut()
                        .find(|(existing, _)| existing == name)
                    {
                        Some(entry) => entry.1 = spec.clone(),
                        None => self.entries.push((name.clone(), spec.clone())),
                    }
                }
                Ok(Vec::new())
            }
        }
    }

    fn print(&self, names: &[Vec<u8>]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        if names.is_empty() {
            for (name, spec) in &self.entries {
                out.push(spec.render(name));
            }
            return out;
        }
        for name in names {
            if let Some((_, spec)) = self.entries.iter().find(|(existing, _)| existing == name) {
                out.push(spec.render(name));
            }
        }
        out
    }

    /// The spec for `name`, falling back to the `-D` default.
    pub fn get(&self, name: &[u8]) -> Option<&CompletionSpec> {
        self.entries
            .iter()
            .find(|(existing, _)| existing == name)
            .map(|(_, spec)| spec)
            .or(self.default.as_ref())
    }

    /// The `-E` spec, for an empty command line.
    pub fn empty(&self) -> Option<&CompletionSpec> {
        self.empty.as_ref()
    }

    /// The `-I` spec, for the first word of a line.
    pub fn initial(&self) -> Option<&CompletionSpec> {
        self.initial.as_ref()
    }

    /// How many command-specific specs are defined.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no command has a spec.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod spec_tests {
    use super::*;

    #[test]
    fn parses_and_prints_the_suites_examples() {
        let mut table = CompletionTable::default();
        // `complete` with no arguments prints nothing when nothing is defined.
        let request = parse_complete(&[]).expect("parse");
        assert_eq!(table.apply(&request).expect("apply"), Vec::<Vec<u8>>::new());

        let request = parse_complete(&[b"-W", b"foo bar", b"mycommand"]).expect("parse");
        table.apply(&request).expect("apply");
        assert_eq!(
            table
                .apply(&parse_complete(&[b"-p"]).expect("parse"))
                .expect("apply"),
            vec![b"complete -W 'foo bar' mycommand".to_vec()]
        );

        let request = parse_complete(&[b"-F", b"myfunc", b"other"]).expect("parse");
        table.apply(&request).expect("apply");
        assert_eq!(
            table
                .apply(&parse_complete(&[]).expect("parse"))
                .expect("apply"),
            vec![
                b"complete -W 'foo bar' mycommand".to_vec(),
                b"complete -F myfunc other".to_vec(),
            ]
        );
    }

    #[test]
    fn redefines_in_place_and_removes() {
        let mut table = CompletionTable::default();
        table
            .apply(&parse_complete(&[b"-W", b"a", b"cmd"]).expect("parse"))
            .expect("apply");
        table
            .apply(&parse_complete(&[b"-W", b"b", b"cmd"]).expect("parse"))
            .expect("apply");
        assert_eq!(table.len(), 1, "a second definition replaces the first");
        assert_eq!(
            table
                .apply(&parse_complete(&[b"-p"]).expect("parse"))
                .expect("apply"),
            vec![b"complete -W 'b' cmd".to_vec()]
        );
        table
            .apply(&parse_complete(&[b"-r", b"cmd"]).expect("parse"))
            .expect("apply");
        assert!(table.is_empty());
    }

    #[test]
    fn a_name_with_no_action_is_an_error() {
        // The suite expects status 2 here; bash returns 0, which is the BUG
        // annotation in the spec file.
        assert!(parse_complete(&[b"foo"]).is_err());
        // `-F` with no command name is a usage error too.
        assert!(parse_complete(&[b"-F", b"f"]).is_err());
        // An unknown action is rejected rather than ignored.
        assert!(parse_complete(&[b"-A", b"nope", b"cmd"]).is_err());
    }

    #[test]
    fn quoting_round_trips_through_single_quotes() {
        let mut table = CompletionTable::default();
        let request = parse_complete(&[b"-W", b"it's here", b"cmd"]).expect("parse");
        table.apply(&request).expect("apply");
        let printed = table
            .apply(&parse_complete(&[b"-p"]).expect("parse"))
            .expect("apply");
        assert_eq!(printed, vec![b"complete -W 'it'\\''s here' cmd".to_vec()]);
        // The rendered form escapes the quote so a shell can read it back.
        assert!(printed[0].windows(4).any(|window| window == b"'\\''"));
    }

    #[test]
    fn default_and_empty_specs_are_kept_apart() {
        let mut table = CompletionTable::default();
        table
            .apply(&parse_complete(&[b"-D", b"-F", b"_default"]).expect("parse"))
            .expect("apply");
        assert!(
            table.get(b"anything").is_some(),
            "the default applies to all names"
        );
        table
            .apply(&parse_complete(&[b"-E", b"-W", b"echo"]).expect("parse"))
            .expect("apply");
        assert!(table.empty().is_some());
        assert!(table.initial().is_none());
        assert_eq!(table.len(), 0, "a default is not a named entry");
    }
}

#[cfg(test)]
mod bash_output_tests {
    use super::*;

    /// The exact strings bash 5.3.20 prints, captured from a real shell.
    /// Matching them is what makes `complete -p` output re-input-safe and
    /// diffable against a reference.
    #[test]
    fn prints_what_bash_prints() {
        let cases: &[(&[&[u8]], &[u8])] = &[
            (&[b"-W", b"foo bar", b"cmd"], b"complete -W 'foo bar' cmd"),
            (&[b"-W", b"simple", b"cmd"], b"complete -W 'simple' cmd"),
            (&[b"-F", b"myfunc", b"other"], b"complete -F myfunc other"),
            (&[b"-A", b"file", b"cmd"], b"complete -f cmd"),
            (&[b"-A", b"variable", b"cmd"], b"complete -v cmd"),
            (&[b"-A", b"function", b"cmd"], b"complete -A function cmd"),
            (&[b"-A", b"arrayvar", b"cmd"], b"complete -A arrayvar cmd"),
            (
                &[b"-P", b"pre", b"-S", b"suf", b"cmd"],
                b"complete -P 'pre' -S 'suf' cmd",
            ),
        ];
        for (args, expected) in cases {
            let mut table = CompletionTable::default();
            table
                .apply(&parse_complete(args).expect("parse"))
                .expect("apply");
            let printed = table
                .apply(&parse_complete(&[b"-p"]).expect("parse"))
                .expect("apply");
            assert_eq!(printed, vec![expected.to_vec()], "for {args:?}");
        }
    }
}
