//! The expectation-to-provider table, and the curated data the static
//! providers serve.
//!
//! Completion is data-driven from the grammar (`DESIGN.md` §6.3, §6.4): the
//! parser says what may appear at the cursor, and a table — not a chain of
//! per-command conditionals — selects which providers answer. Providers
//! themselves decide whether they apply (a variable provider has nothing to
//! say about a bare word that does not start with `$`), but the *table* is the
//! only place that knows which providers a production consults.

use bsh_syntax::Expect;

use crate::candidate::Candidate;
use crate::context::Ctx;
use crate::text;

/// The identity of a provider.
///
/// Providers are looked up by identity, so `bsh-repl` can replace one (for
/// example, with a `git` branch provider) without rebuilding the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProviderId {
    /// Files and directories.
    Path,
    /// Commands found on `PATH`.
    Command,
    /// Shell functions.
    Function,
    /// Shell builtins.
    Builtin,
    /// Shell aliases.
    Alias,
    /// Variables.
    Variable,
    /// Parameter-expansion operators (`:-`, `#`, …).
    ParamOp,
    /// `[[ … ]]` operators.
    TestOp,
    /// Shell keywords (`then`, `fi`, …).
    Keyword,
    /// A `case` pattern.
    CaseTag,
    /// Words taken from history.
    History,
}

/// A source of candidates.
///
/// The trait is deliberately tiny and synchronous: the editor calls every
/// provider whose expectation matches, paints what came back, and the
/// streaming tier (`DESIGN.md` §6.1) later extends this with a cancellable
/// second pass rather than changing the provider contract. Implementations are
/// `Send + Sync` because the full picker may move them to a worker thread.
pub trait Provider: Send + Sync {
    /// This provider's identity.
    fn id(&self) -> ProviderId;

    /// Whether this provider has anything to say about `ctx`.
    fn applies(&self, ctx: &Ctx) -> bool;

    /// Produce candidates for `ctx`, unfiltered and unranked.
    ///
    /// Ranking is [`crate::rank`]'s job; a provider must not pre-sort for
    /// presentation, though it may return entries in a natural order.
    fn candidates(&self, ctx: &Ctx) -> Vec<Candidate>;
}

const COMMAND: &[ProviderId] = &[
    ProviderId::Command,
    ProviderId::Function,
    ProviderId::Builtin,
    ProviderId::Alias,
    ProviderId::History,
    ProviderId::Path,
];
const WORD: &[ProviderId] = &[ProviderId::Path, ProviderId::Variable, ProviderId::Command];
const ARITH: &[ProviderId] = &[ProviderId::Variable];
const PARAM: &[ProviderId] = &[ProviderId::Variable, ProviderId::ParamOp];
const TEST: &[ProviderId] = &[ProviderId::TestOp];
const KEYWORD: &[ProviderId] = &[ProviderId::Keyword];
const CASE: &[ProviderId] = &[ProviderId::CaseTag];
const REDIRECT: &[ProviderId] = &[ProviderId::Path];
const FOR_LIST: &[ProviderId] = &[ProviderId::Path, ProviderId::Variable, ProviderId::History];
const NONE: &[ProviderId] = &[];

/// The provider set for each production, as data.
///
/// `Word` includes `Command` because argument position is a command position
/// after a wrapper such as `sudo` or `env`; the command provider checks that.
pub const PROVIDERS: &[(Expect, &[ProviderId])] = &[
    (Expect::Command, COMMAND),
    (Expect::Word, WORD),
    (Expect::ArithExpr, ARITH),
    (Expect::ParamOp, PARAM),
    (Expect::TestOp, TEST),
    (Expect::CasePattern, CASE),
    (Expect::RedirectTarget, REDIRECT),
    (Expect::ForList, FOR_LIST),
    (Expect::Then, KEYWORD),
    (Expect::Do, KEYWORD),
    (Expect::Done, KEYWORD),
    (Expect::Fi, KEYWORD),
    (Expect::Esac, KEYWORD),
    (Expect::In, KEYWORD),
    (Expect::FunctionBody, KEYWORD),
    (Expect::Operator, KEYWORD),
    (Expect::HereDocDelimiter, NONE),
];

/// The providers consulted for `expect`.
pub fn providers_for(expect: Expect) -> &'static [ProviderId] {
    PROVIDERS
        .iter()
        .find(|(candidate, _)| *candidate == expect)
        .map(|(_, providers)| *providers)
        .unwrap_or(NONE)
}

/// One operator suggestion: the text and a one-line description.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Op {
    /// The bytes to insert.
    pub insert: &'static [u8],
    /// A one-line description for the menu.
    pub desc: &'static str,
}

/// Parameter-expansion operators (`DESIGN.md` §6.3).
///
/// `bsh-lang` currently returns [`bsh_lang::ErrorKind::Unsupported`] for the
/// pattern operators (`#`, `%`, `/`, `^^`, `,,`); they are listed anyway
/// because the completion table is the parser's mapping of the surface forms,
/// and the diagnostic should point at the operator the user chose. See
/// `docs/contracts.md`, "bsh-lang".
pub const PARAM_OPS: &[Op] = &[
    Op {
        insert: b":-",
        desc: "use a default if unset or null",
    },
    Op {
        insert: b":=",
        desc: "assign a default if unset or null",
    },
    Op {
        insert: b":+",
        desc: "use an alternative if set",
    },
    Op {
        insert: b":?",
        desc: "error out if unset or null",
    },
    Op {
        insert: b"#",
        desc: "remove the shortest matching prefix",
    },
    Op {
        insert: b"##",
        desc: "remove the longest matching prefix",
    },
    Op {
        insert: b"%",
        desc: "remove the shortest matching suffix",
    },
    Op {
        insert: b"%%",
        desc: "remove the longest matching suffix",
    },
    Op {
        insert: b"//",
        desc: "replace every match",
    },
    Op {
        insert: b"^^",
        desc: "uppercase every character",
    },
    Op {
        insert: b",,",
        desc: "lowercase every character",
    },
    Op {
        insert: b":0:1",
        desc: "substring from offset 0, length 1",
    },
    Op {
        insert: b"}",
        desc: "close the expansion",
    },
];

/// `[[ … ]]` operators (`DESIGN.md` §6.3).
pub const TEST_OPS: &[Op] = &[
    Op {
        insert: b"-e",
        desc: "the path exists",
    },
    Op {
        insert: b"-f",
        desc: "a regular file exists",
    },
    Op {
        insert: b"-d",
        desc: "a directory exists",
    },
    Op {
        insert: b"-s",
        desc: "a file exists and is not empty",
    },
    Op {
        insert: b"-r",
        desc: "the file is readable",
    },
    Op {
        insert: b"-w",
        desc: "the file is writable",
    },
    Op {
        insert: b"-x",
        desc: "the file is executable",
    },
    Op {
        insert: b"-L",
        desc: "the path is a symbolic link",
    },
    Op {
        insert: b"-p",
        desc: "the path is a named pipe",
    },
    Op {
        insert: b"-S",
        desc: "the path is a socket",
    },
    Op {
        insert: b"-n",
        desc: "the string is not empty",
    },
    Op {
        insert: b"-z",
        desc: "the string is empty",
    },
    Op {
        insert: b"-t",
        desc: "the file descriptor is a terminal",
    },
    Op {
        insert: b"-v",
        desc: "the variable is set",
    },
    Op {
        insert: b"-o",
        desc: "the shell option is enabled",
    },
    Op {
        insert: b"==",
        desc: "strings are equal (pattern match)",
    },
    Op {
        insert: b"!=",
        desc: "strings differ (pattern match)",
    },
    Op {
        insert: b"=~",
        desc: "the string matches the regular expression",
    },
    Op {
        insert: b"<",
        desc: "sorts before, in the current locale",
    },
    Op {
        insert: b">",
        desc: "sorts after, in the current locale",
    },
    Op {
        insert: b"-eq",
        desc: "integers are equal",
    },
    Op {
        insert: b"-ne",
        desc: "integers differ",
    },
    Op {
        insert: b"-lt",
        desc: "integer is less than",
    },
    Op {
        insert: b"-le",
        desc: "integer is less than or equal",
    },
    Op {
        insert: b"-gt",
        desc: "integer is greater than",
    },
    Op {
        insert: b"-ge",
        desc: "integer is greater than or equal",
    },
    Op {
        insert: b"&&",
        desc: "both tests hold",
    },
    Op {
        insert: b"||",
        desc: "either test holds",
    },
    Op {
        insert: b"!",
        desc: "negate the test",
    },
    Op {
        insert: b"(",
        desc: "group tests",
    },
    Op {
        insert: b")",
        desc: "close the group",
    },
    Op {
        insert: b"]]",
        desc: "close the conditional",
    },
];

/// The `case` pattern that matches anything.
pub const CATCH_ALL_PATTERN: &[u8] = b"*)";

/// Commands after which the next argument is another command.
///
/// This is why `Expect::Word` consults the command provider: `sudo gi<TAB>` is
/// a command position, and `sudo` is not a `git`-specific special case.
pub const COMMAND_WRAPPERS: &[&[u8]] = &[
    b"sudo", b"doas", b"env", b"command", b"builtin", b"exec", b"nohup", b"nice", b"time",
    b"xargs", b"watch", b"chroot",
];

/// Commands whose arguments are directories.
pub const DIRECTORY_COMMANDS: &[&[u8]] = &[b"cd", b"pushd", b"popd", b"rmdir", b"mkdir"];

/// Shell builtins.
///
/// This is a copy of `bsh-runtime`'s table, because the dependency edge runs
/// the other way (`bsh-complete` sits below the runtime). It should be
/// replaced by a runtime-supplied list once `bsh-runtime` exposes one; until
/// then the completion menu is the only place a stale entry would show.
pub const BUILTINS: &[&[u8]] = &[
    b".",
    b":",
    b"[",
    b"alias",
    b"bg",
    b"break",
    b"builtin",
    b"cd",
    b"command",
    b"continue",
    b"declare",
    b"dirs",
    b"disown",
    b"echo",
    b"enable",
    b"eval",
    b"exec",
    b"exit",
    b"export",
    b"false",
    b"fc",
    b"fg",
    b"getopts",
    b"hash",
    b"help",
    b"history",
    b"jobs",
    b"kill",
    b"let",
    b"local",
    b"logout",
    b"mapfile",
    b"popd",
    b"printf",
    b"pushd",
    b"pwd",
    b"read",
    b"readarray",
    b"readonly",
    b"return",
    b"set",
    b"shift",
    b"shopt",
    b"source",
    b"suspend",
    b"test",
    b"times",
    b"trap",
    b"true",
    b"type",
    b"typeset",
    b"ulimit",
    b"umask",
    b"unalias",
    b"unset",
    b"wait",
];

/// Keywords offered for a given expectation.
pub fn keywords_for(expect: Expect) -> &'static [&'static [u8]] {
    match expect {
        Expect::Then => &[b"then"],
        Expect::Do => &[b"do"],
        Expect::Done => &[b"done"],
        Expect::Fi => &[b"fi"],
        Expect::Esac => &[b"esac"],
        Expect::In => &[b"in"],
        Expect::FunctionBody => &[b"{"],
        Expect::Operator => &[b";", b"&&", b"||", b"|"],
        _ => &[],
    }
}

/// Whether the cursor is in a command-name slot.
///
/// That is command position itself, or the argument after a wrapper such as
/// `sudo`, `env` or `command`. Completion is data-driven here: the wrapper list
/// is a table, not a chain of per-command conditionals.
pub fn command_slot(ctx: &Ctx) -> bool {
    if ctx.param || ctx.arith || ctx.heredoc {
        return false;
    }
    match ctx.expect {
        Expect::Command => true,
        Expect::Word => {
            ctx.arg_index >= 1
                && ctx
                    .command_name
                    .as_deref()
                    .is_some_and(|name| is_one_of(name, COMMAND_WRAPPERS))
        }
        _ => false,
    }
}

/// Whether the cursor is in a command slot and the prefix is not a path.
pub(crate) fn named_command_slot(ctx: &Ctx) -> bool {
    command_slot(ctx) && !text::looks_like_path(&ctx.prefix)
}

/// Whether `name` is one of `list`.
pub(crate) fn is_one_of(name: &[u8], list: &[&[u8]]) -> bool {
    list.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_expectation_has_a_table_entry() {
        for expect in [
            Expect::Command,
            Expect::Word,
            Expect::RedirectTarget,
            Expect::ForList,
            Expect::CasePattern,
            Expect::ParamOp,
            Expect::TestOp,
            Expect::ArithExpr,
            Expect::Then,
            Expect::Do,
            Expect::Done,
            Expect::Fi,
            Expect::Esac,
            Expect::In,
            Expect::FunctionBody,
            Expect::Operator,
            Expect::HereDocDelimiter,
        ] {
            assert!(
                PROVIDERS.iter().any(|(candidate, _)| *candidate == expect),
                "{expect:?} is missing from the provider table"
            );
        }
    }

    #[test]
    fn command_position_gets_commands_and_paths() {
        let providers = providers_for(Expect::Command);
        assert!(providers.contains(&ProviderId::Command));
        assert!(providers.contains(&ProviderId::Builtin));
        assert!(providers.contains(&ProviderId::Function));
        assert!(providers.contains(&ProviderId::Alias));
        assert!(providers.contains(&ProviderId::Path));
    }

    #[test]
    fn param_ops_are_ordered_and_described() {
        assert_eq!(PARAM_OPS[0].insert, b":-");
        assert!(PARAM_OPS.iter().any(|op| op.insert == b"//"));
        assert!(PARAM_OPS.iter().all(|op| !op.desc.is_empty()));
    }

    #[test]
    fn test_ops_are_described() {
        assert!(TEST_OPS.iter().all(|op| !op.desc.is_empty()));
        assert!(TEST_OPS.iter().any(|op| op.insert == b"-f"));
    }
}
