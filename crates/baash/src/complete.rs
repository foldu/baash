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
use baash_syntax::Expect;
use baash_vm::Interp;

use crate::carapace_ext;

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
        // `-C` and `-G` still need something the completer cannot do from data
        // alone (run a command, expand a glob), so a rule whose only source is
        // one of them is treated as absent: the command keeps the default
        // completion instead of losing it. `-F` is carried through — the
        // shell's `ShellCompleter` runs the function at Tab — and a rule that
        // also has `-W`/`-A` keeps those parts.
        if (spec.command.is_some() || spec.glob.is_some())
            && spec.actions.is_empty()
            && spec.wordlist.is_none()
            && spec.function.is_none()
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
        // bash-completion edits `COMP_WORDBREAKS`; the words a `-F` function
        // sees have to be split the way the shell currently says.
        engine.set_word_breaks(scalar(b"COMP_WORDBREAKS"));
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

/// Runs `complete -F` at the prompt.
///
/// The engine decides that a function must run and assembles the input bash
/// gives it; this is the half that needs the interpreter, so it lives here
/// where the shell is. The editor calls it on Tab, while it owns the terminal in
/// raw mode — so the function's output is captured, never written.
pub struct ShellCompleter<'a> {
    shell: &'a mut Interp,
    engine: &'a baash_complete::CompletionEngine,
    /// A runtime carapace process started by [`ShellCompletion::begin`] and
    /// not yet answered or cancelled.
    pending: Option<PendingCarapace>,
}

/// One runtime lookup that has started but not finished.
struct PendingCarapace {
    carapace: carapace_ext::Carapace,
    /// The line and cursor the process was asked about. The editor cancels on
    /// any keystroke, so this is still the line when the answer arrives.
    line: Vec<u8>,
    cursor: usize,
}

impl<'a> ShellCompleter<'a> {
    /// A hook for the line about to be read.
    pub fn new(shell: &'a mut Interp, engine: &'a baash_complete::CompletionEngine) -> Self {
        Self {
            shell,
            engine,
            pending: None,
        }
    }

    /// The non-runtime precedence: static carapace tables, then `complete -F`,
    /// then `None` for the data engine.
    ///
    /// This order is the product promise (`external carapace > carapace
    /// builtin > bash completions`). The engine's own `complete` cannot express
    /// it because a matching `complete` spec short-circuits its providers; the
    /// static table is asked here first, by name, before the function rule is
    /// even looked up.
    fn finish(&mut self, line: &[u8], cursor: usize) -> Option<baash_repl::ShellMenu> {
        if let Some(menu) = self.engine.carapace_menu(line, cursor) {
            return Some(baash_repl::ShellMenu {
                menu,
                output: Vec::new(),
            });
        }
        self.rule(line, cursor)
    }

    /// Run a `complete -F` function, if the slot names one.
    fn rule(&mut self, line: &[u8], cursor: usize) -> Option<baash_repl::ShellMenu> {
        // The function may install the command's real rule while it runs
        // (bash-completion's loader sources the per-command script), so a
        // `124` means the answer is stale: refresh the snapshot and look the
        // rule up again. The bound is bash's own protection against a loader
        // that keeps asking for a retry.
        for _ in 0..4 {
            let deferred = self.engine.deferred(line, cursor)?;
            match self.run(&deferred) {
                Run::Replies(replies, output) => {
                    return Some(baash_repl::ShellMenu {
                        menu: self.engine.deferred_menu(&deferred, &replies),
                        output,
                    });
                }
                Run::Retry => refresh(self.shell, self.engine),
                Run::Fail(output) => {
                    // Something to say about a function that could not finish:
                    // offer the bytes as an empty menu, which the editor paints
                    // above the prompt before the bell. With nothing to say,
                    // return `None` so the ordinary completion still runs.
                    if output.is_empty() {
                        return None;
                    }
                    let ctx = self.engine.context(line, cursor.min(line.len()));
                    return Some(baash_repl::ShellMenu {
                        menu: baash_complete::Menu::new(ctx, Vec::new()),
                        output,
                    });
                }
            }
        }
        None
    }

    /// The argv/env for a runtime lookup, when one applies.
    ///
    /// The runtime provider mirrors the static provider's slot: an argument of
    /// a named command, not the command word, a redirection, an assignment
    /// value or a substitution body. `PATH` is the shell's own value, so a
    /// `PATH=` assignment changes whether carapace is used.
    fn invocation(&mut self, line: &[u8], cursor: usize) -> Option<carapace_ext::Invocation> {
        let cursor = cursor.min(line.len());
        let ctx = self.engine.context(line, cursor);
        if ctx.expect != Expect::Word
            || ctx.redirect
            || ctx.assign
            || ctx.param
            || ctx.arith
            || ctx.heredoc
        {
            return None;
        }
        let command = ctx.command_name.as_deref()?;
        if command.is_empty() {
            return None;
        }

        let state = self.shell.state();
        let path = state.vars.scalar(b"PATH")?;
        let program = carapace_ext::find(path)?;
        let mut envp = state.vars.envp();
        // `PATH` can be set without being exported (an assignment before the
        // command then does not reach children). Carapace needs it to run the
        // tools its dynamic actions call.
        if !envp.iter().any(|entry| entry.starts_with(b"PATH=")) {
            let mut entry = b"PATH=".to_vec();
            entry.extend_from_slice(path);
            envp.push(entry);
        }
        // A `COMP_LINE` inherited from a parent shell would make carapace's
        // bash patch replace the words we pass with its own parse of that
        // variable. This invocation carries the words in argv instead.
        envp.retain(|entry| !entry.starts_with(b"COMP_"));
        carapace_ext::invocation(program, &ctx, envp)
    }

    /// Build a menu from carapace's `export` answer.
    fn answer(&self, bytes: &[u8], line: &[u8], cursor: usize) -> Option<baash_repl::ShellMenu> {
        let answer = carapace_ext::parse_export(bytes)?;
        let mut candidates = carapace_ext::candidates(&answer.values, &answer.nospace);
        let mut seen = std::collections::HashSet::with_capacity(candidates.len());
        candidates.retain(|candidate| seen.insert(candidate.insert.clone()));
        candidates.truncate(self.engine.max_candidates());
        let ctx = self.engine.context(line, cursor.min(line.len()));
        Some(baash_repl::ShellMenu {
            menu: baash_complete::Menu::new(ctx, candidates),
            output: Vec::new(),
        })
    }

    /// Set up `COMP_*`, run the function, and read `COMPREPLY` back.
    ///
    /// A function can also answer with status `124`: bash's convention for
    /// "the rule has changed, run the completion again", which is how
    /// bash-completion's `-D` loader installs a command's script on first use.
    fn run(&mut self, deferred: &baash_complete::Deferred) -> Run {
        let function = deferred.function().to_vec();
        let words = deferred.words().to_vec();
        // bash's `$1`, `$2`, `$3`: the command, the current word and the word
        // before it (measured against 5.3.20 through a pty).
        let argv = vec![
            function.clone(),
            deferred.command().to_vec(),
            deferred.current_word().to_vec(),
            deferred.previous_word().to_vec(),
        ];
        let options = deferred.spec().options.clone();
        {
            let vars = &mut self.shell.state().vars;
            let _ = vars.assign_scalar(b"COMP_LINE", deferred.line());
            let _ = vars.assign_scalar(b"COMP_POINT", deferred.point().to_string().as_bytes());
            let _ = vars.assign_scalar(b"COMP_CWORD", deferred.cword().to_string().as_bytes());
            // `9` is Tab, the value a pty-driven bash reports for both.
            let _ = vars.assign_scalar(b"COMP_TYPE", b"9");
            let _ = vars.assign_scalar(b"COMP_KEY", b"9");
            let _ = vars.assign_elements(b"COMP_WORDS", &words);
            let _ = vars.unset(b"COMPREPLY");
        }
        // The rule's own `-o` options are in force before the function runs, so
        // `compopt +o …` can turn one off and `compopt -o …` can add another.
        // The command is named too, because the compatibility loaders call
        // `compopt -o … <command>` and that has to reach this run as well as
        // the saved rule (see `bi_compopt`).
        self.shell
            .state()
            .completion
            .set_command(Some(deferred.command()));
        self.shell.state().completion.begin(&options);
        let captured = self.shell.call_function_captured(&function, &argv);
        let options = self.shell.state().completion.options().to_vec();
        self.shell.state().completion.end();
        let Some((output, status)) = captured else {
            return Run::Fail(Vec::new());
        };
        if status == baash_runtime::Status(124) {
            return Run::Retry;
        }
        if !status.is_success() {
            // bash shows nothing for a function that failed part-way, and the
            // ordinary completion is better than half-filled replies — but what
            // the function wrote (a complaint, or the shell's own diagnostic
            // about a construct it rejects) is still worth painting.
            return Run::Fail(output);
        }
        let vars = &self.shell.state().vars;
        Run::Replies(
            baash_complete::Replies::new(completion_replies(vars), options),
            output,
        )
    }
}

impl baash_repl::ShellCompletion for ShellCompleter<'_> {
    fn complete(&mut self, line: &[u8], cursor: usize) -> Option<baash_repl::ShellMenu> {
        // The editor uses `begin`/`poll`, which let the prompt stay live. This
        // synchronous form exists for the trait's default and for callers that
        // have no event loop; it waits out the same deadline.
        match self.begin(line, cursor) {
            baash_repl::ShellCompletionState::Ready(menu) => menu,
            baash_repl::ShellCompletionState::Pending => loop {
                match self.poll() {
                    baash_repl::ShellCompletionState::Ready(menu) => break menu,
                    baash_repl::ShellCompletionState::Pending => {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                }
            },
        }
    }

    fn begin(&mut self, line: &[u8], cursor: usize) -> baash_repl::ShellCompletionState {
        // A new Tab abandons the previous lookup: its answer is for a word the
        // user has already replaced.
        self.pending = None;
        if let Some(invocation) = self.invocation(line, cursor) {
            if let Ok(carapace) = carapace_ext::Carapace::start(&invocation) {
                self.pending = Some(PendingCarapace {
                    carapace,
                    line: line.to_vec(),
                    cursor,
                });
                return baash_repl::ShellCompletionState::Pending;
            }
        }
        baash_repl::ShellCompletionState::Ready(self.finish(line, cursor))
    }

    fn poll(&mut self) -> baash_repl::ShellCompletionState {
        let Some(pending) = self.pending.as_mut() else {
            return baash_repl::ShellCompletionState::Ready(None);
        };
        match pending.carapace.poll() {
            carapace_ext::Poll::Pending => baash_repl::ShellCompletionState::Pending,
            carapace_ext::Poll::Failed => {
                let pending = self.pending.take().expect("checked just above");
                baash_repl::ShellCompletionState::Ready(self.finish(&pending.line, pending.cursor))
            }
            carapace_ext::Poll::Ready(bytes) => {
                let pending = self.pending.take().expect("checked just above");
                match self.answer(&bytes, &pending.line, pending.cursor) {
                    Some(menu) => baash_repl::ShellCompletionState::Ready(Some(menu)),
                    None => baash_repl::ShellCompletionState::Ready(
                        self.finish(&pending.line, pending.cursor),
                    ),
                }
            }
        }
    }

    fn cancel(&mut self) {
        // Dropping the process kills and reaps it.
        self.pending = None;
    }
}

/// What one `-F` run produced.
enum Run {
    /// The function's `COMPREPLY`, the options in force, and its output.
    Replies(baash_complete::Replies, Vec<u8>),
    /// Status 124: bash's "the rule changed, complete again".
    Retry,
    /// No function, or one that failed. The bytes it wrote are still its
    /// output: a completion function that hit a construct this shell rejects
    /// says so on standard error, and dropping that would leave the Tab
    /// silently unhelpful.
    Fail(Vec<u8>),
}

/// The candidates a completion function left in `COMPREPLY`.
///
/// bash reads it as an array and also accepts the scalar form, and a function
/// may have made it associative by accident; all three are read the way bash
/// reads them.
fn completion_replies(vars: &baash_runtime::Vars) -> Vec<Vec<u8>> {
    match vars.value(b"COMPREPLY") {
        Some(baash_runtime::VarValue::Indexed(elements)) => elements.values().cloned().collect(),
        Some(baash_runtime::VarValue::Assoc(pairs)) => {
            pairs.iter().map(|(_, value)| value.clone()).collect()
        }
        Some(baash_runtime::VarValue::Str(value)) => vec![value.clone()],
        Some(baash_runtime::VarValue::Unset) | None => Vec::new(),
    }
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
    fn a_function_rule_reaches_the_completer() {
        // `-F` is carried through and run by `ShellCompleter` at Tab, so the
        // engine's rule names the function instead of pretending the rule is
        // absent.
        let specs = Specs {
            table: table(&[b"-F", b"_git", b"git"]),
            ifs: DEFAULT_IFS.to_vec(),
        };
        let spec = specs.spec(b"git").expect("a function rule");
        assert_eq!(spec.function.as_deref(), Some(&b"_git"[..]));

        // A rule that also has words carries both sources; the engine keeps
        // the words and the shell runs the function.
        let mixed = Specs {
            table: table(&[b"-F", b"_git", b"-W", b"alpha", b"git"]),
            ifs: DEFAULT_IFS.to_vec(),
        };
        let spec = mixed.spec(b"git").expect("the words survive");
        assert_eq!(spec.words, vec![b"alpha".to_vec()]);
        assert_eq!(spec.function.as_deref(), Some(&b"_git"[..]));
    }

    #[test]
    fn a_command_or_glob_only_rule_stays_absent() {
        // `-C` and `-G` still have no seam: a rule whose only source is one of
        // them would swallow the command's default completion, so it is
        // reported as absent.
        let command = Specs {
            table: table(&[b"-C", b"my-program", b"cmd"]),
            ifs: DEFAULT_IFS.to_vec(),
        };
        assert!(command.spec(b"cmd").is_none());
        let glob = Specs {
            table: table(&[b"-G", b"*.txt", b"cmd2"]),
            ifs: DEFAULT_IFS.to_vec(),
        };
        assert!(glob.spec(b"cmd2").is_none());
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
    fn a_complete_function_runs_at_the_prompt() {
        let mut shell = Interp::new(false);
        shell.set_arg0("baash");
        shell.run_named(
            "<test>",
            b"f(){ COMPREPLY=(\"$COMP_LINE|$COMP_CWORD|$2|$3\" plain); }\ncomplete -F f mycmd\n",
        );
        let engine = CompletionEngine::new();
        refresh(&mut shell, &engine);
        let menu = {
            let mut completer = ShellCompleter::new(&mut shell, &engine);
            baash_repl::ShellCompletion::complete(&mut completer, b"mycmd al", 8)
                .expect("a -F rule runs the function")
        };
        let inserts: Vec<Vec<u8>> = menu
            .menu
            .candidates
            .iter()
            .map(|c| c.insert.clone())
            .collect();
        // The function saw the line, the word index, the current and the
        // previous word, exactly as bash sets them.
        assert_eq!(
            inserts,
            vec![b"mycmd al|1|al|mycmd".to_vec(), b"plain".to_vec()]
        );
    }

    #[test]
    fn a_loader_that_returns_124_is_retried() {
        // bash-completion's `-D` loader defines the command's real rule and
        // returns 124, bash's "complete again" status; the snapshot must be
        // refreshed so the retry sees the new rule.
        let mut shell = Interp::new(false);
        shell.set_arg0("baash");
        shell.run_named(
            "<test>",
            b"loader(){ complete -F answer mycmd; return 124; }\nanswer(){ COMPREPLY=(alpha); }\ncomplete -D -F loader\n",
        );
        let engine = CompletionEngine::new();
        refresh(&mut shell, &engine);
        let menu = {
            let mut completer = ShellCompleter::new(&mut shell, &engine);
            baash_repl::ShellCompletion::complete(&mut completer, b"mycmd al", 8)
                .expect("the retry runs the newly defined rule")
        };
        let inserts: Vec<Vec<u8>> = menu
            .menu
            .candidates
            .iter()
            .map(|c| c.insert.clone())
            .collect();
        assert_eq!(inserts, vec![b"alpha".to_vec()]);
    }

    #[test]
    fn a_failing_completion_function_leaves_the_data_path_alone() {
        let mut shell = Interp::new(false);
        shell.set_arg0("baash");
        shell.run_named("<test>", b"f(){ return 1; }\ncomplete -F f mycmd\n");
        let engine = CompletionEngine::new();
        refresh(&mut shell, &engine);
        let menu = {
            let mut completer = ShellCompleter::new(&mut shell, &engine);
            baash_repl::ShellCompletion::complete(&mut completer, b"mycmd al", 8)
        };
        assert!(menu.is_none(), "a failed function falls back");
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
