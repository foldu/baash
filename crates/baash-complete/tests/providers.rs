//! Provider tests against synthetic directory trees.

mod common;

use std::fs;
use std::time::Duration;

use baash_complete::{CandKind, Ctx, PathProvider, Provider, ProviderId, Ranker, context};
use baash_syntax::ParseOpts;
use common::TempTree;

fn ctx(line: &str) -> Ctx {
    context(line.as_bytes(), line.len(), &ParseOpts::DEFAULT)
}

fn inserts(candidates: &[baash_complete::Candidate]) -> Vec<Vec<u8>> {
    candidates
        .iter()
        .map(|candidate| candidate.insert.clone())
        .collect()
}

fn find<'a>(
    candidates: &'a [baash_complete::Candidate],
    insert: &[u8],
) -> Option<&'a baash_complete::Candidate> {
    candidates
        .iter()
        .find(|candidate| candidate.insert == insert)
}

#[test]
fn lists_the_typed_directory_only() {
    let tree = TempTree::new("components");
    tree.file("src/main.rs", b"");
    tree.file("src/lib.rs", b"");
    tree.file("src/nested/deep.rs", b"");
    tree.file("other.rs", b"");

    let provider = PathProvider::with_root(&tree.root);
    let candidates = provider.candidates(&ctx("cat src/ma"));
    let found = inserts(&candidates);
    assert!(found.contains(&b"src/main.rs".to_vec()));
    assert!(found.contains(&b"src/lib.rs".to_vec()));
    // Only `src` is listed; nothing descends into `src/nested`.
    assert!(found.contains(&b"src/nested/".to_vec()));
    assert!(!found.contains(&b"src/nested/deep.rs".to_vec()));
    assert!(!found.contains(&b"other.rs".to_vec()));
}

/// A command word that looks like a path asks for what could be executed:
/// executable files and directories, never a non-executable file. An ordinary
/// argument still sees every entry (`cat ./`).
#[test]
fn a_command_slot_with_a_slash_lists_executables_and_directories() {
    let tree = TempTree::new("command-paths");
    tree.exe("runme");
    tree.file("plain", b"");
    tree.dir("subdir");
    #[cfg(unix)]
    tree.symlink(&tree.path("runme"), "link-to-runme");

    let provider = PathProvider::with_root(&tree.root);
    let command = inserts(&provider.candidates(&ctx("./")));
    assert!(command.contains(&b"./runme".to_vec()));
    assert!(command.contains(&b"./subdir/".to_vec()));
    assert!(
        !command.contains(&b"./plain".to_vec()),
        "a non-executable file is not a command: {command:?}"
    );
    #[cfg(unix)]
    assert!(
        command.contains(&b"./link-to-runme".to_vec()),
        "a symlink to an executable is one: {command:?}"
    );

    // A wrapper's argument is a command slot too (`sudo ./…`).
    let wrapped = inserts(&provider.candidates(&ctx("sudo ./")));
    assert!(wrapped.contains(&b"./runme".to_vec()));
    assert!(!wrapped.contains(&b"./plain".to_vec()));

    // After a command the slot is a file argument, so nothing is filtered.
    let argument = inserts(&provider.candidates(&ctx("cat ./")));
    assert!(argument.contains(&b"./plain".to_vec()));
    assert!(argument.contains(&b"./runme".to_vec()));
    assert!(argument.contains(&b"./subdir/".to_vec()));
}

#[test]
fn directories_end_with_a_slash_and_do_not_append_a_space() {
    let tree = TempTree::new("dirs");
    tree.dir("alpha");
    tree.file("beta.txt", b"");

    let provider = PathProvider::with_root(&tree.root);
    let candidates = provider.candidates(&ctx("cat "));
    let alpha = find(&candidates, b"alpha/").expect("directory candidate");
    let beta = find(&candidates, b"beta.txt").expect("file candidate");
    assert_eq!(alpha.kind, CandKind::Path);
    assert!(!alpha.append_space);
    assert!(beta.append_space);
}

#[test]
fn hidden_entries_need_a_dot_prefix() {
    let tree = TempTree::new("hidden");
    tree.file(".hidden", b"");
    tree.file("shown", b"");

    let provider = PathProvider::with_root(&tree.root);
    let plain = inserts(&provider.candidates(&ctx("cat ")));
    assert!(!plain.contains(&b".hidden".to_vec()));
    assert!(plain.contains(&b"shown".to_vec()));

    let dotted = inserts(&provider.candidates(&ctx("cat .h")));
    assert!(dotted.contains(&b".hidden".to_vec()));
}

#[test]
fn entry_cap_truncates_a_listing() {
    let tree = TempTree::new("cap");
    for index in 0..6 {
        tree.file(&format!("file{index}"), b"");
    }
    let provider = PathProvider::with_root(&tree.root).with_entry_cap(3);
    let candidates = provider.candidates(&ctx("cat "));
    assert_eq!(candidates.len(), 3);
}

#[test]
fn listing_is_cached_and_refreshed_when_the_directory_moves() {
    let tree = TempTree::new("cache");
    tree.file("first", b"");
    let provider = PathProvider::with_root(&tree.root);
    let before = inserts(&provider.candidates(&ctx("cat ")));
    assert_eq!(before, vec![b"first".to_vec()]);

    // A short pause keeps this robust on filesystems with coarse timestamps.
    std::thread::sleep(Duration::from_millis(20));
    tree.file("second", b"");
    let after = inserts(&provider.candidates(&ctx("cat ")));
    assert!(
        after.contains(&b"second".to_vec()),
        "the cache did not notice the new entry: {after:?}"
    );

    provider.clear_cache();
    let cleared = inserts(&provider.candidates(&ctx("cat ")));
    assert_eq!(cleared.len(), after.len());
}

#[test]
fn absolute_and_home_prefixes() {
    let tree = TempTree::new("home");
    tree.file("home-file", b"");

    let provider = PathProvider::with_root(&tree.root).with_home(&tree.root);
    let home = inserts(&provider.candidates(&ctx("cat ~/")));
    assert!(home.contains(&b"~/home-file".to_vec()), "{home:?}");

    let absolute = format!("cat {}/", tree.root.display());
    let from_root = inserts(&provider.candidates(&ctx(&absolute)));
    assert!(
        from_root.contains(&format!("{}/home-file", tree.root.display()).into_bytes()),
        "{from_root:?}"
    );

    // A bare `~` names the home directory; the insert replaces the whole word.
    let bare = inserts(&provider.candidates(&ctx("cat ~")));
    assert!(bare.contains(&b"~/home-file".to_vec()), "{bare:?}");
}

#[test]
fn changing_the_root_changes_what_is_listed() {
    let first = TempTree::new("root-one");
    first.file("one", b"");
    let second = TempTree::new("root-two");
    second.file("two", b"");

    let provider = PathProvider::with_root(&first.root);
    assert_eq!(
        inserts(&provider.candidates(&ctx("cat "))),
        vec![b"one".to_vec()]
    );
    provider.set_root(&second.root);
    assert_eq!(
        inserts(&provider.candidates(&ctx("cat "))),
        vec![b"two".to_vec()]
    );
    assert_eq!(provider.root(), second.root);
}

#[test]
fn unknown_user_tilde_is_not_guessed() {
    let tree = TempTree::new("tilde");
    let provider = PathProvider::with_root(&tree.root);
    assert!(provider.candidates(&ctx("cat ~someuser/")).is_empty());
}

#[cfg(unix)]
#[test]
fn symlinks_to_directories_get_a_trailing_slash() {
    let tree = TempTree::new("symlink");
    let target = tree.dir("real");
    tree.symlink(&target, "link");

    let provider = PathProvider::with_root(&tree.root);
    let candidates = provider.candidates(&ctx("cat "));
    assert!(find(&candidates, b"link/").is_some(), "{candidates:?}");
}

#[cfg(unix)]
#[test]
fn invalid_utf8_names_keep_their_bytes() {
    let tree = TempTree::new("invalid");
    tree.raw_file(b"bad\xff");

    let provider = PathProvider::with_root(&tree.root);
    let candidates = provider.candidates(&ctx("cat "));
    let candidate = find(&candidates, b"bad\xff").expect("raw name candidate");
    assert_eq!(candidate.insert, b"bad\xff");
    // The display is a lossy projection, for rendering only.
    assert!(candidate.display.contains('\u{fffd}'));
}

#[test]
fn command_arguments_are_directories_only() {
    let tree = TempTree::new("cd");
    tree.dir("somewhere");
    tree.file("a-file", b"");

    let provider = PathProvider::with_root(&tree.root);
    let candidates = provider.candidates(&ctx("cd "));
    assert_eq!(inserts(&candidates), vec![b"somewhere/".to_vec()]);
}

#[test]
fn redirect_targets_put_directories_first() {
    let tree = TempTree::new("redirect");
    tree.file("aaa.txt", b"");
    tree.dir("zzz");

    let provider = PathProvider::with_root(&tree.root);
    let ctx = ctx("cat > ");
    assert!(provider.applies(&ctx));
    let mut candidates = provider.candidates(&ctx);
    let mut ranker = Ranker::new();
    ranker.rank(&mut candidates, &ctx.prefix);
    assert_eq!(candidates[0].insert, b"zzz/");
    assert_eq!(candidates[0].tier, 0);
}

#[test]
fn path_provider_declines_word_when_it_does_not_apply() {
    let tree = TempTree::new("applies");
    let provider = PathProvider::with_root(&tree.root);
    assert_eq!(provider.id(), ProviderId::Path);
    assert!(provider.applies(&ctx("cat ")));
    assert!(provider.applies(&ctx("cat > ")));
    assert!(!provider.applies(&ctx("echo ${x")));
    assert!(!provider.applies(&ctx("[[ -")));
}

#[test]
fn command_index_reads_path_directories() {
    let tree = TempTree::new("commands");
    tree.exe("bin/alpha");
    tree.file("bin/beta", b"not executable");
    tree.dir("bin/sub");
    tree.exe("other/delta");

    let path = format!(
        "{}:{}",
        tree.path("bin").display(),
        tree.path("other").display()
    );
    let provider = baash_complete::CommandProvider::with_path_var(path.into_bytes());
    let names = provider.names();
    assert!(names.contains(&b"alpha".to_vec()));
    assert!(!names.contains(&b"beta".to_vec()));
    assert!(!names.contains(&b"sub".to_vec()));
    assert!(names.contains(&b"delta".to_vec()));

    // Non-executables and directories never become candidates.
    let candidates = provider.candidates(&ctx("alp"));
    assert!(find(&candidates, b"alpha").is_some());
    assert!(find(&candidates, b"beta").is_none());
}

#[test]
fn changing_path_invalidates_the_index() {
    let tree = TempTree::new("pathchange");
    tree.exe("one/alpha");
    tree.exe("two/bravo");

    let one = tree.path("one");
    let two = tree.path("two");
    let provider =
        baash_complete::CommandProvider::with_path_var(one.display().to_string().into_bytes());
    assert_eq!(*provider.names(), vec![b"alpha".to_vec()]);

    provider.set_path_var(Some(
        format!("{}:{}", one.display(), two.display()).into_bytes(),
    ));
    let both = provider.names();
    assert!(both.contains(&b"alpha".to_vec()));
    assert!(both.contains(&b"bravo".to_vec()));
}

#[test]
fn a_new_executable_in_a_path_directory_is_noticed() {
    let tree = TempTree::new("mtime");
    tree.exe("bin/alpha");
    let provider = baash_complete::CommandProvider::with_path_var(
        tree.path("bin").display().to_string().into_bytes(),
    );
    assert_eq!(*provider.names(), vec![b"alpha".to_vec()]);

    std::thread::sleep(Duration::from_millis(20));
    tree.exe("bin/charlie");
    let refreshed = provider.names();
    assert!(
        refreshed.contains(&b"charlie".to_vec()),
        "the index did not notice a new command: {refreshed:?}"
    );

    provider.invalidate();
    assert!(provider.names().contains(&b"charlie".to_vec()));
}

#[test]
fn command_entry_cap_is_applied_per_directory() {
    let tree = TempTree::new("cmdcap");
    for index in 0..5 {
        tree.exe(&format!("bin/cmd{index}"));
    }
    let provider = baash_complete::CommandProvider::with_path_var(
        tree.path("bin").display().to_string().into_bytes(),
    )
    .with_entry_cap(2);
    assert!(provider.names().len() <= 2);
}

#[test]
fn empty_path_component_means_the_current_directory() {
    let tree = TempTree::new("empty-component");
    tree.exe("bin/alpha");
    let provider = baash_complete::CommandProvider::with_path_var(
        format!("{}:", tree.path("bin").display()).into_bytes(),
    );
    // `.` (the process directory) is scanned too; the point is that the empty
    // component does not break the parse.
    assert!(provider.names().contains(&b"alpha".to_vec()));
}

#[test]
fn a_stale_entry_is_removed_when_the_file_disappears() {
    let tree = TempTree::new("removed");
    let file = tree.exe("bin/alpha");
    let provider = baash_complete::CommandProvider::with_path_var(
        tree.path("bin").display().to_string().into_bytes(),
    );
    assert!(provider.names().contains(&b"alpha".to_vec()));

    std::thread::sleep(Duration::from_millis(20));
    fs::remove_file(file).expect("remove");
    assert!(!provider.names().contains(&b"alpha".to_vec()));
}
