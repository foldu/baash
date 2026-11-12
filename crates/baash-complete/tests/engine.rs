//! End-to-end engine tests: expectation → providers → ranking → menu.

mod common;

use std::sync::Arc;

use baash_complete::{Completer, CompletionEngine, History, Menu, VarNames};
use baash_syntax::{Expect, ParseOpts};
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

fn inserts(menu: &Menu) -> Vec<String> {
    menu.candidates
        .iter()
        .map(|candidate| candidate.display.clone())
        .collect()
}

#[test]
fn command_position_lists_commands_and_builtins() {
    let tree = TempTree::new("commandpos");
    tree.exe("bin/mycommand");

    let engine =
        CompletionEngine::new().with_path_var(tree.path("bin").display().to_string().into_bytes());
    let menu = engine.complete(b"my", 2);
    assert_eq!(menu.expect, Expect::Command);
    assert!(!menu.is_empty());
    assert_eq!(menu.candidates[0].insert, b"mycommand");
    assert!(menu.candidates[0].tier == 0);
}

/// `./` at the prompt is a command word with a path, so Tab offers only what
/// could run: executable files and directories. Without the filter a
/// non-executable file is offered and `./notes.txt` fails with "Permission
/// denied" when chosen.
#[test]
fn a_dot_slash_command_word_lists_only_executables_and_directories() {
    let tree = TempTree::new("dotslash");
    tree.exe("runme");
    tree.file("notes.txt", b"");
    tree.dir("subdir");

    let engine = CompletionEngine::new().with_root(&tree.root);
    let menu = engine.complete(b"./", 2);
    assert_eq!(menu.expect, Expect::Command);
    let found: Vec<&[u8]> = menu
        .candidates
        .iter()
        .map(|candidate| candidate.insert.as_slice())
        .collect();
    assert!(found.contains(&b"./runme".as_slice()), "{found:?}");
    assert!(found.contains(&b"./subdir/".as_slice()), "{found:?}");
    assert!(
        !found.contains(&b"./notes.txt".as_slice()),
        "a non-executable file must not be offered as a command: {found:?}"
    );
    assert!(
        found.iter().all(|insert| insert.starts_with(b"./")),
        "`./` is a path, so keywords and variables are not candidates: {found:?}"
    );

    // A non-empty component must not let a fuzzy keyword in either: `for`
    // contains `r`, and `./r` is still a path.
    let menu = engine.complete(b"./r", 3);
    let found: Vec<&[u8]> = menu
        .candidates
        .iter()
        .map(|candidate| candidate.insert.as_slice())
        .collect();
    assert!(
        found.iter().all(|insert| insert.starts_with(b"./")),
        "a keyword is a fuzzy match, not a command: {found:?}"
    );
    assert!(found.contains(&b"./runme".as_slice()), "{found:?}");
}

#[test]
fn an_empty_command_word_offers_whole_history_lines_newest_first() {
    // fzf-style history: at a blank prompt Tab lists entire previous commands,
    // not just their first word, and the newest comes first.
    let engine = CompletionEngine::new()
        .with_path_var(Vec::new())
        .with_history(history(&[
            b"git status --short",
            b"cargo test --workspace",
            b"git status --short",
        ]));
    let menu = engine.complete(b"", 0);
    let inserts: Vec<&[u8]> = menu
        .candidates
        .iter()
        .filter(|candidate| candidate.kind == baash_complete::CandKind::History)
        .map(|candidate| candidate.insert.as_slice())
        .collect();
    assert_eq!(
        inserts,
        vec![
            b"git status --short".as_slice(),
            b"cargo test --workspace".as_slice()
        ],
        "newest first, one candidate per line"
    );
    assert!(
        menu.candidates
            .iter()
            .filter(|candidate| candidate.kind == baash_complete::CandKind::History)
            .all(|candidate| candidate.insertion == baash_complete::Insertion::Source)
    );
}

#[test]
fn a_typed_prefix_never_loses_to_a_fuzzy_match() {
    let tree = TempTree::new("tiers");
    tree.exe("bin/chunky");

    let engine =
        CompletionEngine::new().with_path_var(tree.path("bin").display().to_string().into_bytes());
    let menu = engine.complete(b"ch", 2);
    // `compile`-like noise is not present in the PATH, so use the engine's own
    // provider list: `chunky` must be first and every candidate before any
    // fuzzy one must be a prefix match.
    assert_eq!(menu.candidates[0].insert, b"chunky");
    let first_fuzzy = menu
        .candidates
        .iter()
        .position(|candidate| candidate.tier == 2);
    if let Some(first_fuzzy) = first_fuzzy {
        assert!(
            menu.candidates[..first_fuzzy]
                .iter()
                .all(|candidate| candidate.tier < 2)
        );
    }
}

#[test]
fn argument_position_offers_paths_and_variables_only_with_a_dollar() {
    let tree = TempTree::new("args");
    tree.file("some-file", b"");
    let engine = CompletionEngine::new()
        .with_root(&tree.root)
        .with_vars(vars(&[b"PATH", b"PWD"]));

    let plain = engine.complete(b"cat so", 6);
    assert_eq!(plain.expect, Expect::Word);
    assert!(plain.candidates.iter().any(|c| c.insert == b"some-file"));
    assert!(
        plain
            .candidates
            .iter()
            .all(|c| c.kind != baash_complete::CandKind::Variable)
    );

    let dollar = engine.complete(b"cat $P", 6);
    assert_eq!(dollar.candidates[0].insert, b"$PATH");
    assert_eq!(
        dollar.candidates[0].insertion,
        baash_complete::Insertion::Source
    );
}

#[test]
fn parameter_expansion_offers_variables_then_operators() {
    let engine = CompletionEngine::new().with_vars(vars(&[b"PATH", b"PWD"]));

    let empty = engine.complete(b"echo ${", 7);
    assert_eq!(empty.expect, Expect::ParamOp);
    let names = empty
        .candidates
        .iter()
        .map(|candidate| candidate.insert.clone())
        .collect::<Vec<_>>();
    // Bash closes the expansion for a bare name: `${PA<TAB>` offers `${PATH}`,
    // so the inserted name carries the closing brace.
    assert!(names.contains(&b"PATH}".to_vec()), "{names:?}");
    assert!(names.contains(&b"PWD}".to_vec()), "{names:?}");

    let typed = engine.complete(b"echo ${PATH", 11);
    assert_eq!(typed.expect, Expect::ParamOp);
    let operator = typed
        .candidates
        .iter()
        .find(|candidate| candidate.insert == b"PATH:-")
        .expect("parameter operator");
    assert_eq!(
        operator.desc.as_deref(),
        Some("use a default if unset or null")
    );
    let mut line = b"echo ${PATH".to_vec();
    typed
        .apply(
            &mut line,
            typed
                .candidates
                .iter()
                .position(|c| c.insert == b"PATH:-")
                .unwrap(),
        )
        .expect("insertion");
    assert_eq!(line, b"echo ${PATH:-");

    // The closing brace is offered too, and lands after the name.
    let close = engine.complete(b"echo ${PATH", 11);
    let index = close
        .candidates
        .iter()
        .position(|candidate| candidate.insert == b"PATH}")
        .expect("closing brace");
    let mut line = b"echo ${PATH".to_vec();
    close.apply(&mut line, index).expect("insertion");
    assert_eq!(line, b"echo ${PATH}");
}

#[test]
fn for_list_offers_paths_variables_and_history() {
    let tree = TempTree::new("forlist");
    tree.file("a-file", b"");
    let engine = CompletionEngine::new()
        .with_root(&tree.root)
        .with_vars(vars(&[b"HOME"]))
        .with_history(history(&[b"prev-word", b"other"]));

    let menu = engine.complete(b"for f in ", 9);
    assert_eq!(menu.expect, Expect::ForList);
    let all = inserts(&menu);
    assert!(menu.candidates.iter().any(|c| c.insert == b"a-file"));
    // Variables are offered as `$NAME` in a word list; history supplies words
    // from previous lines.
    assert!(menu.candidates.iter().any(|c| c.insert == b"$HOME"));
    assert!(
        menu.candidates
            .iter()
            .any(|c| c.kind == baash_complete::CandKind::History)
    );
    assert!(all.iter().any(|text| text == "a-file"));
}

#[test]
fn redirection_targets_list_paths_with_directories_first() {
    let tree = TempTree::new("redir");
    tree.file("aaa.txt", b"");
    tree.dir("zzz");
    let engine = CompletionEngine::new().with_root(&tree.root);

    let menu = engine.complete(b"echo > ", 7);
    assert_eq!(menu.expect, Expect::RedirectTarget);
    assert_eq!(menu.candidates[0].insert, b"zzz/");
}

#[test]
fn case_patterns_offer_the_catch_all() {
    let engine = CompletionEngine::new().with_case_tags(vec![b"start"[..].to_vec()]);
    let menu = engine.complete(b"case $x in ", 11);
    assert_eq!(menu.expect, Expect::CasePattern);
    assert_eq!(menu.candidates[0].insert, b"*)");
    assert!(menu.candidates.iter().any(|c| c.insert == b"start"));
}

#[test]
fn test_command_offers_operators() {
    let engine = CompletionEngine::new();
    let menu = engine.complete(b"[[ -", 4);
    assert_eq!(menu.expect, Expect::TestOp);
    assert!(menu.candidates[0].insert.starts_with(b"-"));
    assert!(menu.candidates.iter().any(|c| c.insert == b"-e"));
    assert!(menu.candidates.iter().any(|c| c.insert == b"-f"));
    assert!(
        menu.candidates
            .iter()
            .all(|c| c.desc.is_some() && c.insertion == baash_complete::Insertion::Source)
    );
}

#[test]
fn keywords_are_offered_for_closing_contexts() {
    let engine = CompletionEngine::new();
    // After a complete `if` the parser is back at command position.
    let menu = engine.complete(b"if true; then echo hi; fi", 23);
    assert_eq!(menu.expect, Expect::Command);

    // The parser reports `[Then, Command]` after `if true; `: the compound's
    // own keyword first, the list continuation last. The completer acts on the
    // innermost slot, so command completion inside the condition still works.
    let menu = engine.complete(b"if true; ", 9);
    assert_eq!(menu.expect, Expect::Command);
}

#[test]
fn wrapper_commands_promote_the_argument_to_a_command_position() {
    let tree = TempTree::new("wrapper");
    tree.exe("bin/git");
    let engine =
        CompletionEngine::new().with_path_var(tree.path("bin").display().to_string().into_bytes());
    let menu = engine.complete(b"sudo gi", 7);
    assert_eq!(menu.expect, Expect::Word);
    assert_eq!(menu.candidates[0].insert, b"git");
}

#[test]
fn assignment_values_complete_paths() {
    let tree = TempTree::new("assign");
    tree.file("bin-tool", b"");
    let engine = CompletionEngine::new().with_root(&tree.root);
    let menu = engine.complete(b"PATH=bin", 8);
    assert_eq!(menu.expect, Expect::Word);
    assert_eq!(menu.replace, 5..8);
    assert!(menu.candidates.iter().any(|c| c.insert == b"bin-tool"));
}

#[test]
fn command_substitution_completes_inside_the_substitution() {
    let tree = TempTree::new("cmdsub");
    tree.exe("bin/inner");
    let engine =
        CompletionEngine::new().with_path_var(tree.path("bin").display().to_string().into_bytes());
    let menu = engine.complete(b"echo $(in", 9);
    assert_eq!(menu.expect, Expect::Command);
    assert_eq!(menu.replace, 7..9);
    // `in` is a reserved word and command position offers reserved words, so
    // it legitimately sorts before the executable; the substitution's command
    // is still what the menu is about.
    assert!(
        menu.candidates.iter().any(|c| c.insert == b"inner"),
        "{:?}",
        inserts(&menu)
    );
    assert!(menu.candidates.iter().any(|c| c.insert == b"in"));
}

#[test]
fn quotes_suppress_source_candidates() {
    let engine = CompletionEngine::new().with_vars(vars(&[b"PATH"]));
    // Inside single quotes `$PATH` is inert, so no variable candidate.
    let menu = engine.complete(b"echo '$P", 9);
    assert!(menu.candidates.iter().all(|c| c.insert != b"$PATH"));

    // Inside double quotes it is live, and inserted without re-escaping.
    let menu = engine.complete(b"echo \"$P", 9);
    assert_eq!(menu.quote, baash_complete::QuoteCtx::DoubleQuoted);
    let index = menu
        .candidates
        .iter()
        .position(|c| c.insert == b"$PATH")
        .expect("variable inside double quotes");
    let mut line = b"echo \"$P".to_vec();
    menu.apply(&mut line, index).expect("insertion");
    assert_eq!(line, b"echo \"$PATH");
}

#[test]
fn arithmetic_context_completes_bare_variable_names() {
    let engine = CompletionEngine::new().with_vars(vars(&[b"HOME", b"PATH"]));
    let menu = engine.complete(b"echo $((HO", 10);
    assert_eq!(menu.expect, Expect::ArithExpr);
    assert_eq!(menu.replace, 8..10);
    let index = menu
        .candidates
        .iter()
        .position(|candidate| candidate.insert == b"HOME")
        .expect("variable");
    let mut line = b"echo $((HO".to_vec();
    menu.apply(&mut line, index).expect("insertion");
    assert_eq!(line, b"echo $((HOME");
}

#[test]
fn a_redirect_without_a_space_still_completes_its_target() {
    let tree = TempTree::new("nospace");
    tree.file("target.txt", b"");
    let engine = CompletionEngine::new().with_root(&tree.root);
    let menu = engine.complete(b"echo >", 6);
    assert_eq!(menu.expect, Expect::RedirectTarget);
    assert_eq!(menu.replace, 6..6);
    // A unique candidate for an empty prefix would need the whole listing;
    // just prove the provider answered and the insertion goes after the `>`.
    let index = menu
        .candidates
        .iter()
        .position(|candidate| candidate.insert == b"target.txt")
        .expect("path candidate");
    let mut line = b"echo >".to_vec();
    menu.apply(&mut line, index).expect("insertion");
    assert_eq!(line, b"echo >target.txt ");
}

#[test]
fn completion_replaces_the_whole_word_around_the_cursor() {
    let tree = TempTree::new("suffix");
    tree.exe("bin/git");
    let engine =
        CompletionEngine::new().with_path_var(tree.path("bin").display().to_string().into_bytes());
    let line = b"gitx";
    let menu = engine.complete(line, 3);
    assert_eq!(menu.word_span, 0..4);
    assert_eq!(menu.replace, 0..4);
    assert_eq!(menu.prefix, b"git");
    let index = menu
        .candidates
        .iter()
        .position(|candidate| candidate.insert == b"git")
        .expect("command candidate");
    let mut line = line.to_vec();
    menu.apply(&mut line, index).expect("insertion");
    // The suffix is gone either way; a trailing space depends on whether the
    // menu ended up unique, so only the replacement itself is asserted.
    assert!(line == b"git" || line == b"git ", "{line:?}");
}

#[test]
fn a_line_that_does_not_parse_still_completes() {
    let engine = CompletionEngine::new();
    // A trailing backslash and an unfinished substitution are ordinary input.
    for line in [
        &b"echo \\"[..],
        b"echo $(",
        b"if true; then",
        b"case $x in",
        b"'",
        b"[[",
    ] {
        let menu = engine.complete(line, line.len());
        // The only contract that matters here: no panic, and a sane span.
        assert!(menu.replace.start <= menu.replace.end);
        assert!(menu.replace.end <= line.len());
    }
}

#[test]
fn menu_spans_are_consistent() {
    let engine = CompletionEngine::new();
    for line in [
        &b"git checkout"[..],
        b"echo > ",
        b"echo \"fo",
        b"echo ${",
        b"for f in ",
    ] {
        let menu = engine.complete(line, line.len());
        assert!(menu.replace.start >= menu.word_span.start);
        assert!(menu.replace.start <= line.len());
        assert!(menu.word_span.end <= line.len());
        assert!(menu.replace.end <= menu.word_span.end);
    }
}

#[test]
fn engine_can_be_used_as_a_trait_object() {
    let engine: Arc<dyn Completer> = Arc::new(CompletionEngine::new());
    let menu = engine.complete(b"ec", 2);
    assert_eq!(menu.candidates[0].tier, 0);
    assert!(menu.candidates[0].insert.starts_with(b"ec"));
}

#[test]
fn posix_profile_changes_what_the_parser_accepts() {
    let mut opts = ParseOpts::DEFAULT;
    opts.posix_mode = true;
    let engine = CompletionEngine::new().with_parse_opts(opts);
    // `[[` is a bashism; in POSIX mode the parser reports it, and completion
    // must not offer test operators.
    let menu = engine.complete(b"[[ -", 4);
    assert_ne!(menu.expect, Expect::TestOp);
}

fn shell_names(functions: &[&[u8]], aliases: &[&[u8]]) -> Arc<dyn baash_complete::ShellNames> {
    struct StaticNames {
        functions: Vec<Vec<u8>>,
        aliases: Vec<Vec<u8>>,
    }
    impl baash_complete::ShellNames for StaticNames {
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

#[test]
fn command_position_is_a_deduplicated_union() {
    let tree = TempTree::new("union");
    tree.exe("bin/dup-name");
    tree.exe("bin/path-only");
    let engine = CompletionEngine::new()
        .with_path_var(tree.path("bin").display().to_string().into_bytes())
        .with_shell_names(shell_names(&[b"dup-name", b"func-only"], &[b"alias-only"]))
        .with_vars(vars(&[b"unused"]));

    let menu = engine.complete(b"", 0);
    let counts = |needle: &[u8]| {
        menu.candidates
            .iter()
            .filter(|candidate| candidate.insert == needle)
            .count()
    };
    assert_eq!(
        counts(b"dup-name"),
        1,
        "one function shadows one PATH entry"
    );
    assert!(counts(b"func-only") == 1);
    assert!(counts(b"alias-only") == 1);
    assert!(counts(b"path-only") == 1);
    assert!(counts(b"cd") == 1, "builtins are in the union");
    assert!(counts(b"for") == 1, "keywords are in the union");
    // Every candidate is distinct.
    let mut inserts: Vec<&[u8]> = menu
        .candidates
        .iter()
        .map(|candidate| candidate.insert.as_slice())
        .collect();
    let total = inserts.len();
    inserts.sort_unstable();
    inserts.dedup();
    assert_eq!(inserts.len(), total);
}

#[test]
fn a_keyword_is_offered_in_command_position() {
    // An empty `PATH` keeps the menu down to what the shell itself knows.
    let engine = CompletionEngine::new().with_path_var(Vec::new());
    let menu = engine.complete(b"fo", 2);
    assert_eq!(menu.expect, Expect::Command);
    assert_eq!(menu.candidates[0].insert, b"for", "{:?}", inserts(&menu));
    assert_eq!(menu.candidates[0].kind, baash_complete::CandKind::Keyword);

    // A keyword is also a word after a wrapper, which is a command slot too.
    let wrapper = engine.complete(b"sudo wh", 7);
    assert!(
        wrapper.candidates.iter().any(|c| c.insert == b"while"),
        "{:?}",
        inserts(&wrapper)
    );
}

#[test]
fn a_keyword_with_shell_syntax_is_inserted_verbatim() {
    let engine = CompletionEngine::new().with_path_var(Vec::new());
    // `[[` must not be escaped into `\[\[`: that would be an ordinary word,
    // not the conditional's keyword.
    let menu = engine.complete(b"[[", 2);
    let index = menu
        .candidates
        .iter()
        .position(|candidate| candidate.insert == b"[[")
        .expect("the keyword");
    assert_eq!(menu.insertion(index).as_deref(), Some(&b"[[ "[..]));
}

#[test]
fn dollar_completes_variable_names() {
    let engine = CompletionEngine::new()
        .with_vars(vars(&[b"HOME", b"HOSTNAME", b"PATH"]))
        .with_path_var(Vec::new());

    let menu = engine.complete(b"echo $HOM", 9);
    assert_eq!(menu.candidates[0].insert, b"$HOME", "{:?}", inserts(&menu));
    // A fuzzy candidate may follow (`$HOSTNAME` contains `$HOM` in order), but
    // the prefix match is first.
    assert!(menu.candidates[0].tier == 0);
    let mut line = b"echo $HOM".to_vec();
    menu.apply(&mut line, 0).expect("insertion");
    // The fuzzy alternative means this is not a unique match, so no space.
    assert_eq!(line, b"echo $HOME");

    // `$` alone is every name, not an error.
    let bare = engine.complete(b"echo $", 6);
    assert!(bare.candidates.iter().any(|c| c.insert == b"$PATH"));
    assert!(bare.candidates.iter().all(|c| c.insert.starts_with(b"$")));
}

#[test]
fn brace_completion_closes_the_expansion() {
    let engine = CompletionEngine::new()
        .with_vars(vars(&[b"HOME", b"HOSTNAME"]))
        .with_path_var(Vec::new());
    let menu = engine.complete(b"echo ${HOM", 10);
    assert_eq!(menu.expect, Expect::ParamOp);
    let index = menu
        .candidates
        .iter()
        .position(|candidate| candidate.insert == b"HOME}")
        .expect("closing brace");
    assert_eq!(menu.candidates[index].display, "${HOME}");
    let mut line = b"echo ${HOM".to_vec();
    menu.apply(&mut line, index).expect("insertion");
    // Operators are offered next to the closed name, so this is not a unique
    // match and gains no trailing space.
    assert_eq!(line, b"echo ${HOME}");
}

#[test]
fn a_command_word_is_not_completed_from_history_words() {
    // History candidates are real commands typed before, but a two-word
    // history line must not turn its argument into a command candidate.
    let engine = CompletionEngine::new()
        .with_path_var(Vec::new())
        .with_history(history(&[b"echo has-arg"]));
    let menu = engine.complete(b"echo ", 5);
    assert!(
        menu.candidates
            .iter()
            .all(|candidate| candidate.insert != b"has-arg")
    );
}

/// A completion under `~` must not escape the tilde.
///
/// `cd ~/src/gith<TAB>` inserting `\~/src/github.com/` makes the argument a
/// literal `~` directory, so the command fails on a path that does not exist.
/// The bug was in the insertion quoting (`baash_lang::quote`), and this pins
/// the bytes that actually reach the line.
#[test]
fn a_home_relative_completion_keeps_its_tilde() {
    let tree = TempTree::new("tilde");
    tree.dir("src/github.com");
    let engine = CompletionEngine::new()
        .with_root(&tree.root)
        .with_home(&tree.root);
    let line: &[u8] = b"cd ~/src/gith";
    let menu = engine.complete(line, line.len());
    assert_eq!(
        menu.insertion(0),
        Some(b"~/src/github.com/".to_vec()),
        "candidates: {:?}",
        inserts(&menu)
    );
}

/// A path with non-ASCII bytes is inserted byte for byte.
///
/// The repository this shell is developed in lives under `…/ßhell`, so this is
/// the everyday case, not an exotic one: escaping the two UTF-8 bytes of `ß`
/// inserts a backslash inside the character, which the terminal shows as
/// `\�\�hell` and which the column arithmetic counts as two extra characters.
#[test]
fn a_non_ascii_completion_is_not_escaped() {
    let tree = TempTree::new("utf8");
    tree.dir("lab.home.5kw.li/ßhell");
    let engine = CompletionEngine::new()
        .with_root(&tree.root)
        .with_home(&tree.root);
    let line: &[u8] = "cd ~/lab.home.5kw.li/ßh".as_bytes();
    let menu = engine.complete(line, line.len());
    assert_eq!(
        menu.insertion(0),
        Some("~/lab.home.5kw.li/ßhell/".as_bytes().to_vec()),
        "candidates: {:?}",
        inserts(&menu)
    );
}

#[test]
fn a_known_commands_options_are_completed_with_descriptions() {
    // The generated carapace tables reach the menu through the ordinary
    // provider table; nothing here runs a process or parses JSON.
    let engine = CompletionEngine::new();
    let menu = engine.complete(b"git --", 6);
    let version = menu
        .candidates
        .iter()
        .find(|candidate| candidate.insert == b"--version")
        .expect("git --version");
    assert!(
        version.desc.as_deref().is_some_and(|desc| !desc.is_empty()),
        "the option carries carapace's help text"
    );
}

#[test]
fn a_subcommand_is_completed_with_a_description() {
    let engine = CompletionEngine::new();
    let menu = engine.complete(b"git ch", 6);
    let checkout = menu
        .candidates
        .iter()
        .find(|candidate| candidate.insert == b"checkout")
        .expect("git checkout");
    assert!(
        checkout
            .desc
            .as_deref()
            .is_some_and(|desc| !desc.is_empty())
    );
}

#[test]
fn a_subcommands_own_options_are_not_the_roots() {
    let engine = CompletionEngine::new();
    let menu = engine.complete(b"git checkout --", 15);
    assert!(
        menu.candidates
            .iter()
            .any(|candidate| candidate.insert == b"--detach"),
        "checkout --detach"
    );
}
