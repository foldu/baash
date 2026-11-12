//! Commands and options from the generated carapace tables.
//!
//! `baash-complete-data` carries, for every command carapace's export knows, the
//! subcommand tree and each option with a one-line help text. That is the data
//! `git --<TAB>` needs, and it is static: no process, no shell function, and no
//! JSON parser at the prompt. The tables are generated at development time by
//! `scripts/carapace-data.py` and compiled in (see `THIRD-PARTY.md`).
//!
//! The provider answers argument slots only (`Expect::Word`) and stays quiet
//! where the data has nothing to say: a command it does not know, an assignment
//! value, or a position past a subcommand it cannot resolve. Dynamic values —
//! branch names, container ids, package lists — are not data and stay with the
//! `complete -F` functions that can ask the tool.
//!
//! Walking the typed words is what makes the tree useful: `git checkout --`
//! wants `checkout`'s options, not the root's. The walk is a byte comparison per
//! word and stops at the first word that is neither a known option nor a known
//! subcommand, so an unresolvable position costs nothing.

use baash_complete_data::{COMMANDS, Cmd, Flag};

use crate::candidate::{CandKind, Candidate};
use crate::context::Ctx;
use crate::sources::{Provider, ProviderId};

/// Completes a command's subcommands and options from the generated tables.
pub struct CarapaceProvider;

impl Provider for CarapaceProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Carapace
    }

    fn applies(&self, ctx: &Ctx) -> bool {
        // An assignment value (`FOO=git --`) is not a command line, and a
        // command we have no table for has nothing to offer.
        !ctx.assign
            && ctx
                .command_name
                .as_deref()
                .is_some_and(|name| root(name).is_some())
    }

    fn candidates(&self, ctx: &Ctx) -> Vec<Candidate> {
        let Some(name) = ctx.command_name.as_deref() else {
            return Vec::new();
        };
        let Some(root) = root(name) else {
            return Vec::new();
        };

        // The nodes from the root to the subcommand the cursor is in. A flag
        // is offered at every level because cobra's persistent flags are
        // inherited; the engine deduplicates by insertion text.
        let path = descend(root, &ctx.words);
        let node = *path.last().expect("the path starts at the root");

        if ctx.prefix.first() == Some(&b'-') {
            let mut candidates = Vec::new();
            for node in &path {
                for flag in node.flags {
                    if !flag.long.is_empty() {
                        candidates.push(flag_candidate(&format!("--{}", flag.long), flag));
                    }
                    if !flag.short.is_empty() {
                        candidates.push(flag_candidate(&format!("-{}", flag.short), flag));
                    }
                }
            }
            candidates
        } else {
            node.subs
                .iter()
                .map(|sub| {
                    let candidate = Candidate::new(sub.name, CandKind::Command);
                    if sub.desc.is_empty() {
                        candidate
                    } else {
                        candidate.with_desc(sub.desc)
                    }
                })
                .collect()
        }
    }
}

/// The root table for `name`, which the generator writes in name order.
fn root(name: &[u8]) -> Option<&'static Cmd> {
    COMMANDS
        .binary_search_by(|cmd| cmd.name.as_bytes().cmp(name))
        .ok()
        .map(|index| &COMMANDS[index])
}

/// One candidate for an option.
///
/// `Insertion::Source` keeps the bytes as written: `--color` is shell syntax,
/// not an argument to quote. An option that takes a value does not append a
/// space, because the value may follow with `=` or as the next word and the
/// user should choose; one that takes none does, so `--version` ends the word.
fn flag_candidate(insert: &str, flag: &Flag) -> Candidate {
    let mut candidate = Candidate::source(insert, CandKind::Option);
    if !flag.desc.is_empty() {
        candidate = candidate.with_desc(flag.desc);
    }
    candidate.append_space = !flag.takes_value;
    candidate
}

/// The root-to-cursor node path for the words already typed.
///
/// The first word is the command itself; the rest are walked as far as they
/// name a subcommand. An option and the value it takes are skipped together so
/// `git -C dir status --` still lands on `status`; everything else ends the
/// walk at the current node, which is the best answer available for a
/// positional argument.
fn descend<'a>(root: &'a Cmd, words: &[Vec<u8>]) -> Vec<&'a Cmd> {
    let mut path = vec![root];
    let mut index = 1;
    while index < words.len() {
        let word = &words[index];
        if word == b"--" {
            break;
        }
        if word.starts_with(b"-") {
            // `--color=auto` carries its value, so only the next word is
            // skipped when the option actually takes a separate value.
            let separate_value = !word.contains(&b'=') && takes_value(&path, word);
            index += if separate_value { 2 } else { 1 };
            continue;
        }
        let current = *path.last().expect("the path starts at the root");
        match current
            .subs
            .iter()
            .find(|sub| sub.name.as_bytes() == word.as_slice())
        {
            Some(sub) => {
                path.push(sub);
                index += 1;
            }
            None => break,
        }
    }
    path
}

/// Whether the typed option (long or short) takes a separate value.
fn takes_value(path: &[&Cmd], word: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(word) else {
        return false;
    };
    if let Some(long) = text.strip_prefix("--") {
        let name = long.split('=').next().unwrap_or(long);
        return path.iter().rev().any(|node| {
            node.flags
                .iter()
                .any(|flag| flag.long == name && flag.takes_value)
        });
    }
    let Some(shorts) = text.strip_prefix('-') else {
        return false;
    };
    let Some(character) = shorts.chars().next() else {
        return false;
    };
    path.iter().rev().any(|node| {
        node.flags
            .iter()
            .any(|flag| flag.short.starts_with(character) && flag.takes_value)
    })
}

#[cfg(test)]
mod tests {
    use baash_syntax::ParseOpts;

    use super::*;
    use crate::context::context;

    fn ctx_at_end(line: &str) -> Ctx {
        context(line.as_bytes(), line.len(), &ParseOpts::DEFAULT)
    }

    fn inserts(candidates: &[Candidate]) -> Vec<String> {
        candidates
            .iter()
            .map(|candidate| String::from_utf8_lossy(&candidate.insert).into_owned())
            .collect()
    }

    #[test]
    fn long_options_come_with_descriptions() {
        let ctx = ctx_at_end("git --");
        let candidates = CarapaceProvider.candidates(&ctx);
        let version = candidates
            .iter()
            .find(|candidate| candidate.insert == b"--version")
            .expect("git --version is in the table");
        assert!(!version.desc.as_deref().unwrap_or("").is_empty());
        assert_eq!(version.kind, CandKind::Option);
        assert!(version.append_space, "a boolean option ends the word");
    }

    #[test]
    fn a_value_taking_option_does_not_append_a_space() {
        let ctx = ctx_at_end("git --git-d");
        let candidates = CarapaceProvider.candidates(&ctx);
        let option = candidates
            .iter()
            .find(|candidate| candidate.insert == b"--git-dir")
            .expect("git --git-dir is in the table");
        assert!(!option.append_space);
    }

    #[test]
    fn subcommands_come_with_descriptions() {
        let ctx = ctx_at_end("git ch");
        let candidates = CarapaceProvider.candidates(&ctx);
        let checkout = candidates
            .iter()
            .find(|candidate| candidate.insert == b"checkout")
            .expect("git checkout is in the table");
        assert!(!checkout.desc.as_deref().unwrap_or("").is_empty());
        assert_eq!(checkout.kind, CandKind::Command);
    }

    #[test]
    fn the_walk_finds_the_subcommands_options() {
        let root = ctx_at_end("git ");
        let checkout = ctx_at_end("git checkout --");
        assert!(
            !CarapaceProvider.candidates(&checkout).is_empty(),
            "checkout has options"
        );
        // The root's and checkout's options are not the same set.
        let root_flags = inserts(&CarapaceProvider.candidates(&ctx_at_end("git --")));
        let checkout_flags = inserts(&CarapaceProvider.candidates(&ctx_at_end("git checkout --")));
        assert_ne!(root_flags, checkout_flags);
        assert!(
            checkout_flags.iter().any(|flag| flag == "--detach"),
            "checkout --detach"
        );
        assert!(!CarapaceProvider.candidates(&root).is_empty());
    }

    #[test]
    fn an_option_value_is_skipped_so_the_subcommand_is_found() {
        let ctx = ctx_at_end("git -C some/dir status --");
        let candidates = CarapaceProvider.candidates(&ctx);
        assert!(
            inserts(&candidates).iter().any(|flag| flag == "--short"),
            "status --short, not the root's options"
        );
    }

    #[test]
    fn an_unknown_command_has_nothing_to_say() {
        let ctx = ctx_at_end("no-such-command --");
        assert!(CarapaceProvider.candidates(&ctx).is_empty());
        assert!(!CarapaceProvider.applies(&ctx));
    }

    #[test]
    fn an_assignment_value_is_not_a_command_line() {
        let ctx = ctx_at_end("FOO=git --");
        assert!(!CarapaceProvider.applies(&ctx));
    }

    #[test]
    fn a_positional_argument_offers_no_subcommands() {
        // `git log <TAB>` is a revision, not a subcommand; the table has no
        // data for it and the provider must not invent a subcommand list.
        let ctx = ctx_at_end("git log ");
        assert!(CarapaceProvider.candidates(&ctx).is_empty());
    }
}
