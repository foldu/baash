//! Programmable completion: the `complete`/`compgen` rules applied to a menu.
//!
//! The registry itself is `bsh-runtime`'s; these tests hand the engine the
//! [`SpecSource`] the shell's bridge builds and assert what the user sees for
//! each `-W`, `-A`, `-P`/`-S` and `-o` form.

mod common;

use std::sync::Arc;

use bsh_complete::{
    Completer, CompletionEngine, History, Menu, ShellNames, Spec, SpecSource, VarNames,
};
use common::TempTree;

fn vars(names: &[&[u8]]) -> Arc<dyn VarNames> {
    struct StaticVars(Vec<Vec<u8>>);
    impl VarNames for StaticVars {
        fn names(&self) -> Vec<Vec<u8>> {
            self.0.clone()
        }
    }
    Arc::new(StaticVars(names.iter().map(|name| name.to_vec()).collect()))
}

fn names(functions: &[&[u8]], aliases: &[&[u8]]) -> Arc<dyn ShellNames> {
    struct StaticNames {
        functions: Vec<Vec<u8>>,
        aliases: Vec<Vec<u8>>,
    }
    impl ShellNames for StaticNames {
        fn functions(&self) -> Vec<Vec<u8>> {
            self.functions.clone()
        }
        fn aliases(&self) -> Vec<Vec<u8>> {
            self.aliases.clone()
        }
    }
    Arc::new(StaticNames {
        functions: functions.iter().map(|name| name.to_vec()).collect(),
        aliases: aliases.iter().map(|name| name.to_vec()).collect(),
    })
}

fn history(entries: &[&[u8]]) -> Arc<dyn History> {
    struct StaticHistory(Vec<Vec<u8>>);
    impl History for StaticHistory {
        fn entries(&self) -> Vec<Vec<u8>> {
            self.0.clone()
        }
    }
    Arc::new(StaticHistory(
        entries.iter().map(|entry| entry.to_vec()).collect(),
    ))
}

/// A `complete` registry with one rule per command.
struct Rules {
    entries: Vec<(Vec<u8>, Spec)>,
    default: Option<Spec>,
}

impl Rules {
    fn new(entries: Vec<(&[u8], Spec)>) -> Arc<Self> {
        Arc::new(Self {
            entries: entries
                .into_iter()
                .map(|(name, spec)| (name.to_vec(), spec))
                .collect(),
            default: None,
        })
    }
}

impl SpecSource for Rules {
    fn spec(&self, command: &[u8]) -> Option<Spec> {
        self.entries
            .iter()
            .find(|(name, _)| name == command)
            .map(|(_, spec)| spec.clone())
            .or_else(|| self.default.clone())
    }
}

/// A registry with one default rule (`complete -D`).
fn default_rule(spec: Spec) -> Arc<Rules> {
    Arc::new(Rules {
        entries: Vec::new(),
        default: Some(spec),
    })
}

fn spec_words(words: &[&str]) -> Spec {
    Spec {
        words: words.iter().map(|word| word.as_bytes().to_vec()).collect(),
        ..Spec::default()
    }
}

fn inserts(menu: &Menu) -> Vec<String> {
    menu.candidates
        .iter()
        .map(|candidate| candidate.display.clone())
        .collect()
}

fn insert_bytes(menu: &Menu) -> Vec<Vec<u8>> {
    menu.candidates
        .iter()
        .map(|candidate| candidate.insert.clone())
        .collect()
}

#[test]
fn word_lists_are_filtered_by_the_current_word() {
    let engine = CompletionEngine::new()
        .with_specs(Rules::new(vec![(b"mycmd", spec_words(&["alpha", "beta"]))]));

    let menu = engine.complete(b"mycmd al", 8);
    assert_eq!(insert_bytes(&menu), vec![b"alpha".to_vec()]);
    assert_eq!(menu.candidates[0].desc.as_deref(), Some("completion word"));

    let all = engine.complete(b"mycmd ", 6);
    assert_eq!(
        inserts(&all),
        vec!["alpha".to_string(), "beta".to_string()],
        "an empty word offers the whole list, sorted"
    );

    // The rule is for arguments only: the command word itself is still a
    // command.
    let command_word = engine.complete(b"myc", 3);
    assert!(
        command_word
            .candidates
            .iter()
            .all(|candidate| candidate.insert != b"alpha")
    );
}

#[test]
fn a_word_list_rule_belongs_to_its_own_command() {
    let engine = CompletionEngine::new().with_specs(Rules::new(vec![
        (b"alpha-cmd", spec_words(&["from-alpha"])),
        (b"beta-cmd", spec_words(&["from-beta"])),
    ]));
    let menu = engine.complete(b"alpha-cmd fr", 12);
    assert_eq!(insert_bytes(&menu), vec![b"from-alpha".to_vec()]);
    let other = engine.complete(b"beta-cmd fr", 11);
    assert_eq!(insert_bytes(&other), vec![b"from-beta".to_vec()]);
}

#[test]
fn variable_action_inserts_bare_names() {
    let engine = CompletionEngine::new()
        .with_vars(vars(&[b"HOME", b"HOSTNAME", b"PATH"]))
        .with_specs(Rules::new(vec![(
            b"mycmd",
            Spec {
                actions: vec![b"variable".to_vec()],
                ..Spec::default()
            },
        )]));
    let menu = engine.complete(b"mycmd HO", 8);
    assert_eq!(
        insert_bytes(&menu),
        vec![b"HOME".to_vec(), b"HOSTNAME".to_vec()]
    );
    // No `$`: `-A variable` is `compgen -v`, not `$…` completion.
    assert!(menu.candidates.iter().all(|c| !c.insert.starts_with(b"$")));
}

#[test]
fn directory_action_offers_only_directories() {
    let tree = TempTree::new("action-dir");
    tree.dir("adir");
    tree.file("afile", b"");
    let engine = CompletionEngine::new()
        .with_root(&tree.root)
        .with_specs(Rules::new(vec![(
            b"mycmd",
            Spec {
                actions: vec![b"directory".to_vec()],
                ..Spec::default()
            },
        )]));
    let menu = engine.complete(b"mycmd a", 7);
    assert_eq!(insert_bytes(&menu), vec![b"adir/".to_vec()]);
}

#[test]
fn file_action_offers_files_and_directories() {
    let tree = TempTree::new("action-file");
    tree.dir("adir");
    tree.file("afile", b"");
    let engine = CompletionEngine::new()
        .with_root(&tree.root)
        .with_specs(Rules::new(vec![(
            b"mycmd",
            Spec {
                actions: vec![b"file".to_vec()],
                ..Spec::default()
            },
        )]));
    let mut got = insert_bytes(&engine.complete(b"mycmd a", 7));
    got.sort();
    assert_eq!(got, vec![b"adir/".to_vec(), b"afile".to_vec()]);
}

#[test]
fn command_action_is_the_union_and_deduplicated() {
    let tree = TempTree::new("action-command");
    tree.exe("bin/uniq-name");
    let engine = CompletionEngine::new()
        .with_path_var(tree.path("bin").display().to_string().into_bytes())
        .with_shell_names(names(&[b"uniq-name", b"only-func"], &[b"only-alias"]))
        .with_specs(Rules::new(vec![(
            b"mycmd",
            Spec {
                actions: vec![b"command".to_vec()],
                ..Spec::default()
            },
        )]));

    let menu = engine.complete(b"mycmd ", 6);
    let all = insert_bytes(&menu);
    assert_eq!(
        all.iter().filter(|insert| *insert == b"uniq-name").count(),
        1,
        "a function that shadows a PATH entry is one candidate: {all:?}"
    );
    assert!(all.contains(&b"only-func".to_vec()));
    assert!(all.contains(&b"only-alias".to_vec()));
    assert!(all.contains(&b"cd".to_vec()), "builtins are commands");
    assert!(all.contains(&b"for".to_vec()), "keywords are commands");
}

#[test]
fn keyword_and_signal_actions_are_served() {
    let engine = CompletionEngine::new().with_specs(Rules::new(vec![
        (
            b"kw-cmd",
            Spec {
                actions: vec![b"keyword".to_vec()],
                ..Spec::default()
            },
        ),
        (
            b"sig-cmd",
            Spec {
                actions: vec![b"signal".to_vec()],
                ..Spec::default()
            },
        ),
    ]));
    assert_eq!(
        insert_bytes(&engine.complete(b"kw-cmd fo", 9)),
        vec![b"for".to_vec()]
    );
    let signals = insert_bytes(&engine.complete(b"sig-cmd SIGT", 12));
    assert!(signals.contains(&b"SIGTERM".to_vec()), "{signals:?}");
    assert!(signals.contains(&b"SIGTRAP".to_vec()), "{signals:?}");
    assert!(!engine.complete(b"sig-cmd SIGH", 12).candidates.is_empty());
}

#[test]
fn prefix_and_suffix_are_added_after_matching() {
    let engine = CompletionEngine::new().with_specs(Rules::new(vec![(
        b"mycmd",
        Spec {
            words: vec![b"one".to_vec(), b"two".to_vec()],
            prefix: Some(b"pre".to_vec()),
            suffix: Some(b"suf".to_vec()),
            ..Spec::default()
        },
    )]));
    // The typed word matches the *word*, not the decorated text.
    let menu = engine.complete(b"mycmd o", 7);
    assert_eq!(insert_bytes(&menu), vec![b"preonesuf".to_vec()]);
    assert_eq!(menu.candidates[0].display, "preonesuf");
    assert!(
        engine.complete(b"mycmd p", 7).candidates.is_empty(),
        "the prefix is not part of what the word has to match"
    );
}

#[test]
fn nospace_suppresses_the_trailing_space() {
    let engine = CompletionEngine::new().with_specs(Rules::new(vec![(
        b"mycmd",
        Spec {
            options: vec![b"nospace".to_vec()],
            ..spec_words(&["alpha"])
        },
    )]));
    let menu = engine.complete(b"mycmd al", 8);
    assert_eq!(menu.candidates.len(), 1);
    assert_eq!(menu.insertion(0).as_deref(), Some(&b"alpha"[..]));
    // Without the option a unique word gets the space.
    let spaced = CompletionEngine::new()
        .with_specs(Rules::new(vec![(b"mycmd", spec_words(&["alpha"]))]))
        .complete(b"mycmd al", 8);
    assert_eq!(spaced.insertion(0).as_deref(), Some(&b"alpha "[..]));
}

#[test]
fn plusdirs_adds_directories_to_the_rule() {
    let tree = TempTree::new("plusdirs");
    tree.dir("adir");
    let engine = CompletionEngine::new()
        .with_root(&tree.root)
        .with_specs(Rules::new(vec![(
            b"mycmd",
            Spec {
                words: vec![b"alpha".to_vec()],
                options: vec![b"plusdirs".to_vec()],
                ..Spec::default()
            },
        )]));
    let mut got = insert_bytes(&engine.complete(b"mycmd a", 7));
    got.sort();
    assert_eq!(got, vec![b"adir/".to_vec(), b"alpha".to_vec()]);
}

#[test]
fn default_falls_back_to_the_shells_completion_when_the_rule_is_empty() {
    let tree = TempTree::new("fallback");
    tree.file("alpha-file", b"");
    let engine = CompletionEngine::new()
        .with_root(&tree.root)
        .with_specs(Rules::new(vec![(
            b"mycmd",
            Spec {
                words: vec![b"other".to_vec()],
                options: vec![b"default".to_vec()],
                ..Spec::default()
            },
        )]));
    let menu = engine.complete(b"mycmd al", 8);
    assert!(
        menu.candidates.iter().any(|c| c.insert == b"alpha-file"),
        "{:?}",
        inserts(&menu)
    );
}

#[test]
fn dirnames_falls_back_to_directories() {
    let tree = TempTree::new("dirnames");
    tree.dir("adir");
    tree.file("afile", b"");
    let engine = CompletionEngine::new()
        .with_root(&tree.root)
        .with_specs(Rules::new(vec![(
            b"mycmd",
            Spec {
                options: vec![b"dirnames".to_vec()],
                ..Spec::default()
            },
        )]));
    assert_eq!(
        insert_bytes(&engine.complete(b"mycmd a", 7)),
        vec![b"adir/".to_vec()]
    );
}

#[test]
fn unspecified_actions_offer_nothing_rather_than_guessing() {
    let engine = CompletionEngine::new().with_specs(Rules::new(vec![(
        b"mycmd",
        Spec {
            actions: vec![b"setopt".to_vec(), b"user".to_vec()],
            ..Spec::default()
        },
    )]));
    assert!(engine.complete(b"mycmd ", 6).candidates.is_empty());
}

#[test]
fn cd_completes_directories_and_still_reaches_variables() {
    let tree = TempTree::new("cd-dirs");
    tree.dir("adir");
    tree.file("afile", b"");
    let engine = CompletionEngine::new()
        .with_root(&tree.root)
        .with_vars(vars(&[b"HOME"]))
        .with_shell_names(names(&[], &[]))
        .with_history(history(&[]));

    // The built-in rule offers directories only.
    let menu = engine.complete(b"cd a", 4);
    assert_eq!(insert_bytes(&menu), vec![b"adir/".to_vec()]);

    // ... and its `default` option still lets `$HOME` through when no
    // directory matches.
    let dollar = engine.complete(b"cd $HO", 6);
    assert_eq!(insert_bytes(&dollar), vec![b"$HOME".to_vec()]);
}

#[test]
fn a_default_rule_applies_when_no_command_rule_exists() {
    let engine = CompletionEngine::new().with_specs(default_rule(Spec {
        actions: vec![b"keyword".to_vec()],
        ..Spec::default()
    }));
    assert_eq!(
        insert_bytes(&engine.complete(b"anything fo", 11)),
        vec![b"for".to_vec()]
    );
}

#[test]
fn a_rule_does_not_apply_to_redirections_or_assignments() {
    let tree = TempTree::new("spec-redirect");
    tree.file("alpha-file", b"");
    let engine = CompletionEngine::new()
        .with_root(&tree.root)
        .with_specs(Rules::new(vec![(b"mycmd", spec_words(&["alpha"]))]));

    // A redirection target is a filename, not the command's argument.
    let redirect = engine.complete(b"mycmd > al", 10);
    assert!(
        redirect.candidates.iter().all(|c| c.insert != b"alpha"),
        "{:?}",
        inserts(&redirect)
    );
    assert!(
        redirect
            .candidates
            .iter()
            .any(|c| c.insert == b"alpha-file"),
        "{:?}",
        inserts(&redirect)
    );

    // An assignment value is a word the shell expands, not an argument slot
    // with a rule.
    let assign = engine.complete(b"mycmd FOO=al", 12);
    assert!(
        assign.candidates.iter().all(|c| c.insert != b"alpha"),
        "{:?}",
        inserts(&assign)
    );
}
