//! The completion bridge: project the shell's state into `baash-complete`.
//!
//! The editor borrows the [`baash_complete::CompletionEngine`] for the whole
//! interactive loop, while the data completion reads — `PATH`, variables,
//! functions, aliases, history and the `complete` registry — changes between
//! lines. So the shell does not build an engine per line; it replaces the
//! engine's sources with snapshots in `refresh_prompt`, before the next line is
//! read. That is the same seam the prompt and the history ring already use.
//!
//! Snapshots are owned `Vec<u8>`s because the traits are `Send + Sync`: the
//! full picker may later move providers to a worker thread, and completion must
//! never observe a half-mutated shell.

use std::sync::Arc;

use baash_complete::{History, ShellNames, Spec, SpecSource, VarNames};
use baash_runtime::ShellHost;
use baash_runtime::complete::CompletionTable;
use baash_vm::Interp;

/// The most recent history entries offered as completion candidates.
const HISTORY_LIMIT: usize = 2_000;

/// The bytes `IFS` has when the variable is unset.
const DEFAULT_IFS: &[u8] = b" \t\n";

/// Names that are parameters rather than variables.
///
/// `$?`, `$#`, `$@` and the positional parameters are not in the variable
/// table, but a user typing `$<TAB>` expects them, and `${?}` is legal.
const PARAMETER_NAMES: &[&[u8]] = &[
    b"?", b"#", b"@", b"*", b"$", b"!", b"-", b"0", b"1", b"2", b"3", b"4", b"5", b"6", b"7", b"8",
    b"9",
];

/// A snapshot of the names completion may offer.
struct Names {
    variables: Vec<Vec<u8>>,
    functions: Vec<Vec<u8>>,
    aliases: Vec<Vec<u8>>,
}

impl VarNames for Names {
    fn names(&self) -> Vec<Vec<u8>> {
        self.variables.clone()
    }
}

impl ShellNames for Names {
    fn functions(&self) -> Vec<Vec<u8>> {
        self.functions.clone()
    }

    fn aliases(&self) -> Vec<Vec<u8>> {
        self.aliases.clone()
    }
}

/// A snapshot of the history ring, most recent first.
struct Entries(Vec<Vec<u8>>);

impl History for Entries {
    fn entries(&self) -> Vec<Vec<u8>> {
        self.0.clone()
    }
}

/// A clone of the `complete` registry, resolved per lookup.
///
/// The registry lives in `baash-runtime` and this crate's completion engine sits
/// below it, so the rule vocabulary is translated here: the engine never sees a
/// `CompletionSpec`, and this bridge never generates candidates.
struct Specs {
    table: CompletionTable,
    ifs: Vec<u8>,
}

impl SpecSource for Specs {
    fn spec(&self, command: &[u8]) -> Option<Spec> {
        let spec = self.table.get(command)?;
        // `-F`, `-C` and `-G` need to run shell code during completion. That is
        // not possible from the completer (see `docs/state.md` §4), so a rule
        // whose only source is one of them is treated as absent: the command
        // keeps the default completion instead of losing it. A rule that also
        // has `-W`/`-A` keeps those parts.
        if (spec.function.is_some() || spec.command.is_some() || spec.glob.is_some())
            && spec.actions.is_empty()
            && spec.wordlist.is_none()
        {
            return None;
        }
        let words = spec
            .wordlist
            .as_ref()
            .map(|text| split_words(text, &self.ifs))
            .unwrap_or_default();
        Some(Spec {
            actions: spec
                .actions
                .iter()
                .map(|action| action.name().as_bytes().to_vec())
                .collect(),
            words,
            prefix: spec.prefix.clone(),
            suffix: spec.suffix.clone(),
            options: spec.options.clone(),
            function: spec.function.clone(),
        })
    }
}

/// Split a `-W` word list on `IFS` bytes.
///
/// The list is *not* expanded: `$(…)` in a word list would run a command every
/// time the prompt refreshes (bash runs it on Tab, which this seam cannot do),
/// and running shell code is the same blocker as `-F`. Quoted and escaped
/// delimiters are also left alone; the common case is a plain list of words.
fn split_words(text: &[u8], ifs: &[u8]) -> Vec<Vec<u8>> {
    let mut words = Vec::new();
    let mut current = Vec::new();
    for &byte in text {
        if ifs.contains(&byte) {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
        } else {
            current.push(byte);
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// Replace the engine's sources with the shell's current state.
///
/// Called before every prompt, after `PROMPT_COMMAND` has run (it may define a
/// function or a `complete` rule) and while the history ring is in the shell.
pub fn refresh(shell: &mut Interp, engine: &baash_complete::CompletionEngine) {
    let functions = shell.function_names();
    let names: Arc<Names>;
    let history: Arc<Entries>;
    let specs: Arc<Specs>;
    {
        let state = shell.state();
        let scalar = |name: &[u8]| state.vars.scalar(name).map(<[u8]>::to_vec);
        let path = scalar(b"PATH").unwrap_or_default();
        let home = scalar(b"HOME").or_else(baash_sys::dirs::fallback_home);
        let pwd = scalar(b"PWD");
        let ifs = scalar(b"IFS").unwrap_or_else(|| DEFAULT_IFS.to_vec());

        let mut variables: Vec<Vec<u8>> = state
            .vars
            .visible()
            .into_iter()
            .map(|(name, _)| name.to_vec())
            .collect();
        variables.extend(PARAMETER_NAMES.iter().map(|name| name.to_vec()));
        variables.sort_unstable();
        variables.dedup();

        names = Arc::new(Names {
            variables,
            functions,
            aliases: state
                .aliases
                .iter()
                .map(|(name, _)| name.to_vec())
                .collect(),
        });
        history = Arc::new(Entries(
            state
                .history
                .commands()
                .rev()
                .take(HISTORY_LIMIT)
                .map(<[u8]>::to_vec)
                .collect(),
        ));
        specs = Arc::new(Specs {
            table: state.completions.clone(),
            ifs,
        });

        engine.set_path_var(Some(path));
        engine.set_home(home.as_deref().map(baash_sys::os::path_from_bytes));
        if let Some(pwd) = pwd.as_deref() {
            engine.set_root(baash_sys::os::path_from_bytes(pwd));
        }
    }
    // One snapshot is both the variable-name and the function/alias source.
    engine.set_vars(Arc::clone(&names) as Arc<dyn VarNames>);
    engine.set_shell_names(names as Arc<dyn ShellNames>);
    engine.set_history(history);
    engine.set_specs(specs);
}

#[cfg(test)]
mod tests {
    use super::*;
    use baash_complete::{Completer, CompletionEngine};
    use baash_runtime::complete::{CompletionTable, parse_complete};

    fn table(args: &[&[u8]]) -> CompletionTable {
        let mut table = CompletionTable::default();
        table
            .apply(&parse_complete(args).expect("parse"))
            .expect("apply");
        table
    }

    #[test]
    fn a_word_list_is_split_on_ifs_and_dropped_when_empty() {
        assert_eq!(
            split_words(b"alpha  beta\tgamma\n", DEFAULT_IFS),
            vec![b"alpha".to_vec(), b"beta".to_vec(), b"gamma".to_vec()]
        );
        assert!(split_words(b"   ", DEFAULT_IFS).is_empty());
        // A custom `IFS` is the shell's current value, not a hard-coded blank.
        assert_eq!(
            split_words(b"a:b", b":"),
            vec![b"a".to_vec(), b"b".to_vec()]
        );
    }

    #[test]
    fn a_rule_is_translated_into_the_engines_vocabulary() {
        let specs = Specs {
            table: table(&[
                b"-W",
                b"alpha beta",
                b"-P",
                b"pre",
                b"-S",
                b"suf",
                b"-o",
                b"nospace",
                b"mycmd",
            ]),
            ifs: DEFAULT_IFS.to_vec(),
        };
        let spec = specs.spec(b"mycmd").expect("a rule for mycmd");
        assert_eq!(spec.words, vec![b"alpha".to_vec(), b"beta".to_vec()]);
        assert_eq!(spec.prefix.as_deref(), Some(&b"pre"[..]));
        assert_eq!(spec.suffix.as_deref(), Some(&b"suf"[..]));
        assert!(spec.has_option(b"nospace"));
        assert!(spec.function.is_none());

        let actions = Specs {
            table: table(&[b"-A", b"variable", b"-A", b"function", b"cmd"]),
            ifs: DEFAULT_IFS.to_vec(),
        };
        let spec = actions.spec(b"cmd").expect("a rule for cmd");
        assert_eq!(
            spec.actions,
            vec![b"variable".to_vec(), b"function".to_vec()]
        );
    }

    #[test]
    fn a_function_only_rule_leaves_the_default_completion_alone() {
        // `-F` cannot run from the completer (docs/state.md §4). A rule whose
        // only source is a function must not swallow the command's filename
        // completion, so it is reported as absent.
        let specs = Specs {
            table: table(&[b"-F", b"_git", b"git"]),
            ifs: DEFAULT_IFS.to_vec(),
        };
        assert!(specs.spec(b"git").is_none());

        // A rule that also has words keeps them; only the function part is
        // ignored.
        let mixed = Specs {
            table: table(&[b"-F", b"_git", b"-W", b"alpha", b"git"]),
            ifs: DEFAULT_IFS.to_vec(),
        };
        let spec = mixed.spec(b"git").expect("the words survive");
        assert_eq!(spec.words, vec![b"alpha".to_vec()]);
        assert_eq!(spec.function.as_deref(), Some(&b"_git"[..]));
    }

    #[test]
    fn the_default_rule_applies_when_no_command_rule_does() {
        let specs = Specs {
            table: table(&[b"-D", b"-W", b"only", b"unused"]),
            ifs: DEFAULT_IFS.to_vec(),
        };
        assert_eq!(
            specs.spec(b"anything").expect("the default").words,
            vec![b"only".to_vec()]
        );
        assert!(specs.spec(b"unused").is_some());
    }

    #[test]
    fn refresh_projects_variables_functions_and_path() {
        let mut shell = Interp::new(false);
        shell.set_arg0("baash");
        shell.run_named(
            "<test>",
            b"HOME=/home/example\nPATH=/tmp\nmy_func () { :; }\nalias my_alias=ls\n",
        );
        let engine = CompletionEngine::new();
        refresh(&mut shell, &engine);

        let menu = engine.complete(b"echo $HO", 9);
        assert!(
            menu.candidates
                .iter()
                .any(|candidate| candidate.insert == b"$HOME"),
            "{:?}",
            menu.candidates
                .iter()
                .map(|candidate| candidate.insert.clone())
                .collect::<Vec<_>>()
        );
        let function = engine.complete(b"my_fu", 5);
        assert!(
            function
                .candidates
                .iter()
                .any(|candidate| candidate.insert == b"my_func"),
            "{:?}",
            function
                .candidates
                .iter()
                .map(|candidate| candidate.insert.clone())
                .collect::<Vec<_>>()
        );

        // A `complete` rule defined in the shell reaches the next menu.
        shell.run_named("<test>", b"complete -W 'alpha beta' mycmd\n");
        refresh(&mut shell, &engine);
        let menu = engine.complete(b"mycmd al", 8);
        assert_eq!(
            menu.candidates
                .iter()
                .map(|candidate| candidate.insert.clone())
                .collect::<Vec<_>>(),
            vec![b"alpha".to_vec()]
        );
    }

    #[test]
    fn refresh_uses_the_shells_path_not_the_process_environment() {
        let mut shell = Interp::new(false);
        shell.set_arg0("baash");
        // A directory that certainly has no executables.
        shell.run_named("<test>", b"PATH=/nonexistent-baash-test-dir\n");
        let engine = CompletionEngine::new();
        refresh(&mut shell, &engine);
        let menu = engine.complete(b"ec", 2);
        assert!(
            menu.candidates
                .iter()
                .all(|candidate| candidate.kind != baash_complete::CandKind::Command),
            "no PATH commands when PATH names nothing"
        );
    }
}
