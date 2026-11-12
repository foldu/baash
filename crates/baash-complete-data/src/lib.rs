//! Static per-command completion data.
//!
//! The tables in [`data`] are generated from [carapace](https://github.com/carapace-sh/carapace-bin)
//! (MIT) by `scripts/carapace-data.py` and compiled in, so the shell carries the
//! command, option and description data it needs and parses nothing at run time.
//!
//! The crate exists separately from `baash-complete` for build time: the tables
//! are large and change only when the generator runs, while the completer is
//! edited constantly. Keeping them apart means an ordinary edit does not
//! recompile several megabytes of data.
//!
//! # What the data is
//!
//! For each command carapace knows, a tree of subcommands and the options each
//! one accepts, each with carapace's one-line help text. Dynamic values (branch
//! names, container ids, package lists) are not data and are not here; they stay
//! with the completion functions that can ask the tool.
//!
//! # Regenerating
//!
//! ```text
//! just carapace-data
//! ```
//!
//! That runs the pinned carapace binary over [`COMMANDS`]' manifest and rewrites
//! [`data`]. The generator records the carapace version in the file header;
//! [`SOURCE`] repeats it for tests.

#![forbid(unsafe_code)]

mod data;

pub use data::COMMANDS;

/// Where the tables came from, as `name version` plus the manifest's digest.
///
/// A test compares it with the generated header so a half-regenerated file is
/// caught rather than shipped.
pub const SOURCE: &str = data::SOURCE;

/// One option a command accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Flag {
    /// Long spelling without the leading `--`; empty when there is none.
    pub long: &'static str,
    /// Single-character spelling without the leading `-`; empty when none.
    pub short: &'static str,
    /// One-line help text, whitespace-collapsed; possibly empty.
    pub desc: &'static str,
    /// Whether the option takes a value.
    pub takes_value: bool,
}

/// One command, its subcommands and its options.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cmd {
    /// The word that selects this command.
    pub name: &'static str,
    /// One-line help text, whitespace-collapsed; possibly empty.
    pub desc: &'static str,
    /// Subcommands, in the order the tool declares them.
    pub subs: &'static [Cmd],
    /// Options this command accepts, local and persistent, deduplicated.
    pub flags: &'static [Flag],
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tables_are_present_and_ordered() {
        assert!(!COMMANDS.is_empty());
        assert!(
            COMMANDS.windows(2).all(|pair| pair[0].name <= pair[1].name),
            "the generator sorts by name, and the provider binary-searches"
        );
        assert!(COMMANDS.iter().all(|cmd| !cmd.name.is_empty()));
    }

    #[test]
    fn the_source_names_the_generator() {
        assert!(SOURCE.contains("carapace-bin"), "{SOURCE}");
    }

    #[test]
    fn a_known_command_carries_its_options() {
        let git = COMMANDS
            .iter()
            .find(|cmd| cmd.name == "git")
            .expect("git is in the table");
        assert!(git.subs.iter().any(|sub| sub.name == "checkout"));
        assert!(
            git.flags.iter().any(|flag| flag.long == "version"),
            "git --version"
        );
        assert!(
            git.flags
                .iter()
                .any(|flag| flag.long == "git-dir" && flag.takes_value)
        );
    }
}
