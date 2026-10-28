//! Shell options: the bidirectional name table that `set -o`/`set +o` and
//! `shopt -s`/`shopt -u` drive (DESIGN.md §3.2, §3.3).
//!
//! A profile is a named preset of booleans, never a code path. Two *kinds* of
//! option exist, because POSIX and bash spell them differently:
//!
//! * [`OptionKind::SetO`] — named options of `set -o`/`set +o` (POSIX §2.14
//!   plus bash additions).
//! * [`OptionKind::Shopt`] — `shopt -s`/`shopt -u` options, all bash
//!   extensions.
//!
//! Some names are members of both tables (`interactive_comments`,
//! `expand_aliases` in bash is shopt-only and is modelled that way).
//!
//! # Which options are bash extensions (DESIGN.md §3.3)
//!
//! POSIX 2024 `set(1p)` defines exactly these `-o` names: `allexport`,
//! `errexit`, `ignoreeof`, `monitor`, `noclobber`, `noglob`, `noexec`,
//! `nolog`, `notify`, `nounset`, `pipefail`, `verbose`, `vi`, `xtrace`. Its
//! short-option synopsis is `-abCefhmnuvx`. Everything else in this table —
//! every `shopt` option, plus `braceexpand`, `emacs`, `errtrace`, `functrace`,
//! `histexpand`, `history`, `keyword`, `onecmd`, `physical`, `posix`,
//! `privileged` — is a bash extension and must be rejected by a strict `--posix`
//! `set -o`.
//!
//! Two entries need a note:
//!
//! * `hashall`: the *name* is a bash extension, but `set -h` is POSIX
//!   (`[OB]`), so the behavior is still available in strict mode via the short
//!   flag.
//! * `pipefail`: DESIGN.md §2 asserts it is "still not POSIX". The Issue 8
//!   `set(1p)` text lists `pipefail` as a `-o` option (verified against the
//!   published standard while writing this), so it is treated as POSIX here.
//!   If the project wants to follow the design note instead, flip
//!   `is_bash_extension` for `Pipefail` and re-run the tests.

use crate::errors::Error;

/// Which option namespace a name belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OptionKind {
    /// A `set -o` / `set +o` option.
    SetO,
    /// A bash `shopt` option.
    Shopt,
}

impl OptionKind {
    /// The spelling used in diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            OptionKind::SetO => "set -o",
            OptionKind::Shopt => "shopt",
        }
    }
}

macro_rules! option_table {
    ($(
        $(#[$doc:meta])*
        $flag:ident => $field:ident, $name:literal, $short:expr,
            [$($kind:ident),+], $bash_ext:literal, $posix_default:literal,
            $bash_default:literal, $aliases:expr;
    )+) => {
        /// Option bundle for one shell. Presets are values, not code paths
        /// (DESIGN.md §3.2).
        ///
        /// Construct with [`Opts::POSIX`], [`Opts::BASH`] or
        /// [`Opts::default`] (the bash profile), then toggle through
        /// [`Opts::set_by_name`] / [`Opts::set_by_short`] so that parsing and
        /// printing go through one table.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct Opts {
            $(
                $(#[$doc])*
                pub $field: bool,
            )+
        }

        impl Opts {
            /// The POSIX profile: strict mode with the conservative POSIX
            /// defaults. Options that POSIX does not define are off.
            pub const POSIX: Opts = Opts {
                $( $field: $posix_default, )+
            };

            /// The bash compatibility profile (DESIGN.md §3.2). Interactive
            /// defaults such as `monitor` are enabled by the interactive
            /// startup path, not here; `histexpand` stays off deliberately.
            pub const BASH: Opts = Opts {
                $( $field: $bash_default, )+
            };

            /// Current value of `flag`.
            pub fn get_flag(&self, flag: Flag) -> bool {
                match flag {
                    $( Flag::$flag => self.$field, )+
                }
            }

            /// Set `flag` to `on`.
            pub fn set_flag(&mut self, flag: Flag, on: bool) {
                match flag {
                    $( Flag::$flag => self.$field = on, )+
                }
            }
        }

        /// Every option, as a typed handle rather than a string.
        ///
        /// The variant list is the single source of truth for the names,
        /// short letters, table membership and bash-extension status of the
        /// option system.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Flag {
            $(
                $(#[$doc])*
                $flag,
            )+
        }

        impl Flag {
            /// Canonical name used by `set -o` / `shopt`.
            pub fn name(self) -> &'static str {
                match self {
                    $( Flag::$flag => $name, )+
                }
            }

            /// Single-letter `set` flag, if there is one.
            pub fn short(self) -> Option<u8> {
                match self {
                    $( Flag::$flag => $short, )+
                }
            }

            /// Tables this option belongs to. A few names are members of both.
            pub fn kinds(self) -> &'static [OptionKind] {
                match self {
                    $( Flag::$flag => &[$(OptionKind::$kind),+], )+
                }
            }

            /// The other half of a mutually exclusive pair, if any.
            ///
            /// Only `vi`/`emacs` qualify: they are two names for one editing
            /// mode, so enabling one must disable the other rather than leaving
            /// `set -o` reporting both.
            pub fn editor_opposite(self) -> Option<Flag> {
                match self {
                    Flag::Vi => Some(Flag::Emacs),
                    Flag::Emacs => Some(Flag::Vi),
                    _ => None,
                }
            }

            /// Whether this option is a bash extension rather than a POSIX
            /// 2024 `set -o` option (see the module docs for the exact list).
            pub fn is_bash_extension(self) -> bool {
                match self {
                    $( Flag::$flag => $bash_ext, )+
                }
            }

            /// Whether POSIX 2024 defines this option.
            pub fn is_posix(self) -> bool {
                !self.is_bash_extension()
            }

            /// Alternative spellings accepted for this option. The canonical
            /// name is what `names()` prints; aliases exist because bash
            /// spells `interactive_comments` with a hyphen in `set -o`.
            pub fn aliases(self) -> &'static [&'static str] {
                match self {
                    $( Flag::$flag => $aliases, )+
                }
            }

            /// Whether this option belongs to `kind`'s table.
            pub fn in_kind(self, kind: OptionKind) -> bool {
                self.kinds().contains(&kind)
            }

            /// Look up a name (canonical or alias), as bytes.
            pub fn lookup(name: &[u8]) -> Option<Flag> {
                ALL_FLAGS.iter().copied().find(|flag| {
                    flag.name().as_bytes() == name
                        || flag
                            .aliases()
                            .iter()
                            .any(|alias| alias.as_bytes() == name)
                })
            }

            /// Look up a single-letter `set` flag.
            pub fn from_short(short: u8) -> Option<Flag> {
                ALL_FLAGS
                    .iter()
                    .copied()
                    .find(|flag| flag.short() == Some(short))
            }
        }

        /// Every option, in table order. Iterate this for printing and for
        /// completeness tests.
        pub const ALL_FLAGS: &[Flag] = &[ $( Flag::$flag, )+ ];
    };
}

option_table! {
    /// `set -o posix`: strict POSIX mode — reject bash-only syntax
    /// (DESIGN.md §3.1). Bash extension; it is what `--posix` selects.
    Posix => posix_mode, "posix", None, [SetO], true, true, false, &[];

    /// `set -a` / `set -o allexport`: export every assigned variable.
    Allexport => allexport, "allexport", Some(b'a'), [SetO], false, false, false, &[];

    /// `set -B` / `set -o braceexpand`: brace expansion (bash extension).
    ///
    /// On in the bash profile, as in bash. A strict POSIX profile starts with
    /// it off because POSIX has no brace expansion, but `set -o posix` at run
    /// time does not change it — bash keeps expanding braces in POSIX mode.
    Braceexpand => braceexpand, "braceexpand", Some(b'B'), [SetO], true, false, true, &[];

    /// `set -o emacs`: emacs line editing (bash extension; `vi` is POSIX).
    Emacs => emacs, "emacs", None, [SetO], true, false, false, &[];

    /// `set -e` / `set -o errexit`: exit on a failing command (DESIGN.md §4.7).
    Errexit => errexit, "errexit", Some(b'e'), [SetO], false, false, false, &[];

    /// `set -E` / `set -o errtrace`: inherit the `ERR` trap in functions,
    /// command substitutions and subshells (bash extension).
    Errtrace => errtrace, "errtrace", Some(b'E'), [SetO], true, false, false, &[];

    /// `set -T` / `set -o functrace`: inherit the `DEBUG`/`RETURN` traps
    /// (bash extension).
    Functrace => functrace, "functrace", Some(b'T'), [SetO], true, false, false, &[];

    /// `set -h` / `set -o hashall`: remember command locations. The `-o` name
    /// is a bash extension; `set -h` itself is POSIX (`[OB]`), so the behavior
    /// is reachable in strict mode through the short flag.
    Hashall => hashall, "hashall", Some(b'h'), [SetO], true, true, true, &[];

    /// `set -H` / `set -o histexpand`: history expansion.
    ///
    /// Off in the table's default (which is the non-interactive default). The
    /// binary turns it on for an interactive shell, as bash does; every reader
    /// also requires the `history` option, which is what makes `set +o history`
    /// disable expansion as well as recording.
    Histexpand => histexpand, "histexpand", Some(b'H'), [SetO], true, false, false, &[];

    /// `set -o history`: record command history (bash extension).
    History => history, "history", None, [SetO], true, false, false, &[];

    /// `set -o ignoreeof`: an interactive shell does not exit on EOF.
    Ignoreeof => ignoreeof, "ignoreeof", None, [SetO], false, false, false, &[];

    /// `set -o interactive-comments` / `shopt -s interactive_comments`:
    /// `#` starts a comment at an interactive prompt. Bash spells the
    /// `set -o` form with a hyphen; both spellings are accepted here.
    InteractiveComments => interactive_comments, "interactive_comments", None,
        [SetO, Shopt], true, false, true, &["interactive-comments"];

    /// `set -k` / `set -o keyword`: put every `NAME=VALUE` word in the
    /// environment (bash extension).
    Keyword => keyword, "keyword", Some(b'k'), [SetO], true, false, false, &[];

    /// `set -m` / `set -o monitor`: job control.
    Monitor => monitor, "monitor", Some(b'm'), [SetO], false, false, false, &[];

    /// `set -C` / `set -o noclobber`: `>` does not overwrite existing files.
    Noclobber => noclobber, "noclobber", Some(b'C'), [SetO], false, false, false, &[];

    /// `set -n` / `set -o noexec`: read commands but do not execute them.
    Noexec => noexec, "noexec", Some(b'n'), [SetO], false, false, false, &[];

    /// `set -f` / `set -o noglob`: disable pathname expansion.
    Noglob => noglob, "noglob", Some(b'f'), [SetO], false, false, false, &[];

    /// `set -o nolog`: do not record function definitions in the history
    /// file (POSIX `[OB]`).
    Nolog => nolog, "nolog", None, [SetO], false, false, false, &[];

    /// `set -b` / `set -o notify`: report finished background jobs promptly.
    Notify => notify, "notify", Some(b'b'), [SetO], false, false, false, &[];

    /// `set -u` / `set -o nounset`: expanding an unset parameter is an error.
    Nounset => nounset, "nounset", Some(b'u'), [SetO], false, false, false, &[];

    /// `set -t` / `set -o onecmd`: exit after reading and executing one
    /// command (bash extension).
    Onecmd => onecmd, "onecmd", Some(b't'), [SetO], true, false, false, &[];

    /// `set -P` / `set -o physical`: do not resolve symlinks when changing
    /// directory (bash extension).
    Physical => physical, "physical", Some(b'P'), [SetO], true, false, false, &[];

    /// `set -o pipefail`: a pipeline's status is the last non-zero status of
    /// any element. POSIX 2024 (Issue 8) defines this; see the module note
    /// about the contradiction with DESIGN.md §2.
    Pipefail => pipefail, "pipefail", None, [SetO], false, false, false, &[];

    /// `set -p` / `set -o privileged`: privileged mode (bash extension; POSIX
    /// has no `-p` in `set`).
    Privileged => privileged, "privileged", Some(b'p'), [SetO], true, false, false, &[];

    /// `set -v` / `set -o verbose`: echo input lines as they are read.
    Verbose => verbose, "verbose", Some(b'v'), [SetO], false, false, false, &[];

    /// `set -o vi`: vi line editing.
    Vi => vi, "vi", None, [SetO], false, false, false, &[];

    /// `set -x` / `set -o xtrace`: trace commands after expansion.
    Xtrace => xtrace, "xtrace", Some(b'x'), [SetO], false, false, false, &[];

    /// `shopt -s autocd`: a command name that is a directory changes to it.
    Autocd => autocd, "autocd", None, [Shopt], true, false, false, &[];

    /// `shopt -s cdable_vars`: `cd` to a variable's value when it is not a
    /// directory.
    CdableVars => cdable_vars, "cdable_vars", None, [Shopt], true, false, false, &[];

    /// `shopt -s cdspell`: correct minor spelling errors in `cd` arguments.
    Cdspell => cdspell, "cdspell", None, [Shopt], true, false, false, &[];

    /// `shopt -s checkhash`: verify hashed paths still exist before use.
    Checkhash => checkhash, "checkhash", None, [Shopt], true, false, false, &[];

    /// `shopt -s checkjobs`: warn about running jobs on exit.
    Checkjobs => checkjobs, "checkjobs", None, [Shopt], true, false, false, &[];

    /// `shopt -s checkwinsize`: update `LINES`/`COLUMNS` after each command.
    Checkwinsize => checkwinsize, "checkwinsize", None, [Shopt], true, false, true, &[];

    /// `shopt -s cmdhist`: save multi-line commands as one history entry.
    Cmdhist => cmdhist, "cmdhist", None, [Shopt], true, false, true, &[];

    /// `shopt -s complete_fullquote`: quote all shell metacharacters in
    /// completion filenames.
    CompleteFullquote => complete_fullquote, "complete_fullquote", None, [Shopt], true,
        false, true, &[];

    /// `shopt -s direxpand`: expand directory names during completion.
    Direxpand => direxpand, "direxpand", None, [Shopt], true, false, false, &[];

    /// `shopt -s dirspell`: spell-correct directory names during completion.
    Dirspell => dirspell, "dirspell", None, [Shopt], true, false, false, &[];

    /// `shopt -s dotglob`: `*` also matches leading dots.
    Dotglob => dotglob, "dotglob", None, [Shopt], true, false, false, &[];

    /// `shopt -s execfail`: a non-interactive shell does not exit when `exec`
    /// fails.
    Execfail => execfail, "execfail", None, [Shopt], true, false, false, &[];

    /// `shopt -s expand_aliases`: substitute aliases. On in the bash profile.
    ExpandAliases => expand_aliases, "expand_aliases", None, [Shopt], true, false, true, &[];

    /// `shopt -s extdebug`: enable debugger-friendly function tracing.
    Extdebug => extdebug, "extdebug", None, [Shopt], true, false, false, &[];

    /// `shopt -s extglob`: extended pattern operators `?(…)` `*(…)` `+(…)`
    /// `@(…)` `!(…)` (DESIGN.md §3.3).
    Extglob => extglob, "extglob", None, [Shopt], true, false, false, &[];

    /// `shopt -s failglob`: an unmatched glob is an error instead of a
    /// literal word.
    Failglob => failglob, "failglob", None, [Shopt], true, false, false, &[];

    /// `shopt -s force_fignore`: `FIGNORE` applies to all completions.
    ForceFignore => force_fignore, "force_fignore", None, [Shopt], true, false, true, &[];

    /// `shopt -s globasciiranges`: bracket ranges use ASCII order even in a
    /// non-C locale.
    Globasciiranges => globasciiranges, "globasciiranges", None, [Shopt], true, false,
        true, &[];

    /// `shopt -s globskipdots`: `.` and `..` are never matched by a glob
    /// (bash 5.2+).
    Globskipdots => globskipdots, "globskipdots", None, [Shopt], true, false, true, &[];

    /// `shopt -s globstar`: `**` matches across directories.
    Globstar => globstar, "globstar", None, [Shopt], true, false, false, &[];

    /// `shopt -s gnu_errfmt`: diagnostics in GNU `file:line: message` form.
    GnuErrFmt => gnu_errfmt, "gnu_errfmt", None, [Shopt], true, false, false, &[];

    /// `shopt -s histappend`: append to the history file instead of rewriting
    /// it.
    Histappend => histappend, "histappend", None, [Shopt], true, false, false, &[];

    /// `shopt -s histreedit`: offer a failed history expansion for editing.
    Histreedit => histreedit, "histreedit", None, [Shopt], true, false, false, &[];

    /// `shopt -s histverify`: let the user edit history expansion before it
    /// runs.
    Histverify => histverify, "histverify", None, [Shopt], true, false, false, &[];

    /// `shopt -s hostcomplete`: complete hostnames after `@`.
    Hostcomplete => hostcomplete, "hostcomplete", None, [Shopt], true, false, true, &[];

    /// `shopt -s huponexit`: send `SIGHUP` to jobs on interactive exit.
    Huponexit => huponexit, "huponexit", None, [Shopt], true, false, false, &[];

    /// `shopt -s inherit_errexit`: command substitutions inherit `errexit`.
    InheritErrexit => inherit_errexit, "inherit_errexit", None, [Shopt], true, false,
        false, &[];

    /// `shopt -s lastpipe`: the last pipeline element runs in the current
    /// shell environment (DESIGN.md §3.4).
    Lastpipe => lastpipe, "lastpipe", None, [Shopt], true, false, true, &[];

    /// `shopt -s lithist`: save multi-line commands with embedded newlines.
    Lithist => lithist, "lithist", None, [Shopt], true, false, false, &[];

    /// `shopt -s localvar_inherit`: `local` inherits the outer variable's
    /// value.
    LocalvarInherit => localvar_inherit, "localvar_inherit", None, [Shopt], true, false,
        false, &[];

    /// `shopt -s localvar_unset`: `unset` in a function exposes an outer
    /// local.
    LocalvarUnset => localvar_unset, "localvar_unset", None, [Shopt], true, false, false, &[];

    /// `shopt -s mailwarn`: warn about changed mail files.
    Mailwarn => mailwarn, "mailwarn", None, [Shopt], true, false, false, &[];

    /// `shopt -s no_empty_cmd_completion`: do not complete an empty command
    /// name.
    NoEmptyCmdCompletion => no_empty_cmd_completion, "no_empty_cmd_completion", None,
        [Shopt], true, false, false, &[];

    /// `shopt -s nocaseglob`: pathname expansion ignores case.
    Nocaseglob => nocaseglob, "nocaseglob", None, [Shopt], true, false, false, &[];

    /// `shopt -s nocasematch`: `case` and `[[ ]]` matching ignore case.
    Nocasematch => nocasematch, "nocasematch", None, [Shopt], true, false, false, &[];

    /// `shopt -s nullglob`: an unmatched glob expands to nothing.
    Nullglob => nullglob, "nullglob", None, [Shopt], true, false, false, &[];

    /// `shopt -s progcomp`: enable programmable completion.
    Progcomp => progcomp, "progcomp", None, [Shopt], true, false, true, &[];

    /// `shopt -s progcomp_alias`: expand aliases before programmable
    /// completion.
    ProgcompAlias => progcomp_alias, "progcomp_alias", None, [Shopt], true, false, false, &[];

    /// `shopt -s promptvars`: expand prompt strings.
    Promptvars => promptvars, "promptvars", None, [Shopt], true, false, true, &[];

    /// `shopt -s shift_verbose`: `shift` reports a count larger than `$#`.
    ShiftVerbose => shift_verbose, "shift_verbose", None, [Shopt], true, false, false, &[];

    /// `shopt -s sourcepath`: `.`/`source` searches `PATH`.
    Sourcepath => sourcepath, "sourcepath", None, [Shopt], true, false, true, &[];

    /// `shopt -s strict_nameref`: an invalid or unset nameref target is an
    /// error instead of an empty string (an OSH extension; bash has no such
    /// option, and it is off by default).
    StrictNameref => strict_nameref, "strict_nameref", None, [Shopt], true, false, false, &[];

    /// `shopt -s varredir_close`: close `{var}>file` descriptors on command
    /// completion.
    VarredirClose => varredir_close, "varredir_close", None, [Shopt], true, false, false, &[];

    /// `shopt -s xpg_echo`: `echo` interprets escapes without `-e`.
    XpgEcho => xpg_echo, "xpg_echo", None, [Shopt], true, false, false, &[];
}

/// The current state of one option, for printing (`set -o`, `set +o`,
/// `shopt -p`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NamedOption {
    /// The option itself.
    pub flag: Flag,
    /// Its canonical name.
    pub name: &'static str,
    /// Whether it is currently on.
    pub on: bool,
}

impl Opts {
    /// Enable or disable the named option, whatever table it belongs to.
    ///
    /// `set -o` and `shopt` call [`Opts::set_by_name_in`] instead, so that a
    /// name can only be used from the table that defines it.
    pub fn set_by_name(&mut self, name: &[u8], on: bool) -> Result<(), Error> {
        let flag = Flag::lookup(name).ok_or_else(|| Error::UnknownOption {
            name: name.to_vec(),
        })?;
        self.set_flag(flag, on);
        // `vi` and `emacs` are two names for one choice: enabling either turns
        // the other off. bash reports them as a pair, and leaving both on makes
        // `set -o` a lie.
        if on {
            if let Some(other) = flag.editor_opposite() {
                self.set_flag(other, false);
            }
        }
        Ok(())
    }

    /// Enable or disable a named option, requiring it to be defined in
    /// `kind`'s table.
    pub fn set_by_name_in(&mut self, kind: OptionKind, name: &[u8], on: bool) -> Result<(), Error> {
        let flag = Flag::lookup(name).ok_or_else(|| Error::UnknownOption {
            name: name.to_vec(),
        })?;
        if !flag.in_kind(kind) {
            return Err(Error::WrongOptionKind {
                name: name.to_vec(),
                kind,
            });
        }
        self.set_flag(flag, on);
        Ok(())
    }

    /// Value of a named option, or `None` if no such option exists.
    pub fn get_by_name(&self, name: &[u8]) -> Option<bool> {
        Flag::lookup(name).map(|flag| self.get_flag(flag))
    }

    /// Enable or disable a single-letter `set` option (`-e`, `+x`, …).
    pub fn set_by_short(&mut self, short: u8, on: bool) -> Result<(), Error> {
        let flag = Flag::from_short(short).ok_or_else(|| Error::UnknownOption {
            name: vec![b'-', short],
        })?;
        self.set_flag(flag, on);
        Ok(())
    }

    /// The named options of `kind`, with their current values, in table order.
    pub fn options(&self, kind: OptionKind) -> impl Iterator<Item = NamedOption> + '_ {
        ALL_FLAGS
            .iter()
            .copied()
            .filter(move |flag| flag.in_kind(kind))
            .map(move |flag| NamedOption {
                flag,
                name: flag.name(),
                on: self.get_flag(flag),
            })
    }

    /// The names in `kind`'s table, in table order. Used for printing and for
    /// `set +o` output suitable for reinput.
    pub fn names(&self, kind: OptionKind) -> impl Iterator<Item = &'static str> + '_ {
        self.options(kind).map(|option| option.name)
    }

    /// Set or unset a `shopt` option by name.
    ///
    /// Separate from [`Opts::set_by_name_in`] because the CLI has a name and no
    /// [`OptionKind`] to offer, and because a name that is not a `shopt` option
    /// must be an error rather than a no-op.
    pub fn set_shopt_by_name(opts: &mut Opts, name: &[u8], on: bool) -> Result<(), Error> {
        opts.set_by_name_in(OptionKind::Shopt, name, on)
    }

    /// Whether `name` is a `shopt` option.
    ///
    /// The CLI needs this before it has an [`Opts`] to mutate: `-O extglob`
    /// must be rejected at parse time if the name does not exist, rather than
    /// silently accepted and ignored.
    pub fn is_shopt_name(name: &[u8]) -> bool {
        Flag::lookup(name).is_some_and(|flag| flag.in_kind(OptionKind::Shopt))
    }

    /// Every known name, regardless of table, in table order.
    pub fn all_names(&self) -> impl Iterator<Item = &'static str> + '_ {
        ALL_FLAGS.iter().map(|flag| flag.name())
    }

    /// The value of `$-`: the single-letter flags that are currently on.
    ///
    /// `i` (interactive) is not an option and is added by the caller if it
    /// wants it; POSIX leaves the letter set implementation-defined.
    /// The value `SHELLOPTS` should have: enabled `set -o` options, in table
    /// order, colon-separated.
    ///
    /// It is a *derived* variable, not stored state — bash recomputes it, and
    /// storing a copy is how a shell ends up with a `SHELLOPTS` that disagrees
    /// with `set -o`.
    pub fn shellopts(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for option in self.options(OptionKind::SetO) {
            if !option.on {
                continue;
            }
            if !out.is_empty() {
                out.push(b':');
            }
            out.extend_from_slice(option.name.as_bytes());
        }
        out
    }

    /// The value `BASHOPTS` should have: enabled `shopt` options, in table
    /// order, colon-separated.
    pub fn bashopts(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for option in self.options(OptionKind::Shopt) {
            if !option.on {
                continue;
            }
            if !out.is_empty() {
                out.push(b':');
            }
            out.extend_from_slice(option.name.as_bytes());
        }
        out
    }

    /// The value `$-` should have: enabled single-letter `set` options, in
    /// table order.
    ///
    /// The `c` (command string) and `i` (interactive) letters are not options
    /// and are added by the caller that knows how the shell was started.
    pub fn dash_flags(&self) -> Vec<u8> {
        ALL_FLAGS
            .iter()
            .filter(|flag| flag.in_kind(OptionKind::SetO))
            .filter(|flag| self.get_flag(**flag))
            .filter_map(|flag| flag.short())
            .collect()
    }
}

impl Default for Opts {
    fn default() -> Self {
        Self::BASH
    }
}
