//! Cross-cutting tests: the tier table, option round-tripping, scope
//! behavior, environment construction and the builtins that are implemented.

use crate::*;
use bsh_diag::Diagnostic;

/// A host that records everything, so builtins can be tested without a VM.
pub(crate) struct TestHost {
    pub state: ShellState,
    pub out: Vec<u8>,
    pub err: Vec<u8>,
    pub diagnostics: Vec<Diagnostic>,
    pub sources: Vec<Vec<u8>>,
    pub interactive: bool,
    /// Canned command classifications, so `type`/`command -v` can be tested
    /// without a VM. The real resolution lives in `bsh-vm`.
    pub resolutions: Vec<(Vec<u8>, Vec<CommandInfo>)>,
    /// Function names the host knows about, for `unset -f`.
    pub functions: Vec<Vec<u8>>,
}

impl TestHost {
    pub(crate) fn new() -> Self {
        Self {
            state: ShellState::default(),
            out: Vec::new(),
            err: Vec::new(),
            diagnostics: Vec::new(),
            sources: Vec::new(),
            interactive: false,
            resolutions: Vec::new(),
            functions: Vec::new(),
        }
    }

    /// Teach the host what `name` classifies to.
    pub(crate) fn classify(&mut self, name: &[u8], infos: Vec<CommandInfo>) {
        self.resolutions.push((name.to_vec(), infos));
    }

    /// Run a builtin, returning its status. Fails the test if the name is not
    /// registered at all.
    ///
    /// The VM consumes the error class after every command; the test host does
    /// the same so a fault from one builtin cannot leak into the next test
    /// assertion.
    pub(crate) fn run(&mut self, argv: &[&[u8]]) -> Status {
        self.state.clear_error();
        let name = argv.first().copied().unwrap_or(b"");
        run_builtin(name, self, argv).expect("builtin should be registered")
    }
}

impl ShellHost for TestHost {
    fn state(&mut self) -> &mut ShellState {
        &mut self.state
    }

    fn run_source(&mut self, src: &[u8]) -> Status {
        self.sources.push(src.to_vec());
        Status::SUCCESS
    }

    fn write_out(&mut self, bytes: &[u8]) -> Status {
        self.out.extend_from_slice(bytes);
        Status::SUCCESS
    }

    fn write_err(&mut self, bytes: &[u8]) -> Status {
        self.err.extend_from_slice(bytes);
        Status::SUCCESS
    }

    fn is_interactive(&self) -> bool {
        self.interactive
    }

    fn diagnose(&mut self, diagnostic: &Diagnostic) {
        self.diagnostics.push(diagnostic.clone());
    }

    fn resolve_command(&self, name: &[u8], query: ResolveQuery) -> Vec<CommandInfo> {
        let Some((_, infos)) = self
            .resolutions
            .iter()
            .find(|(candidate, _)| candidate.as_slice() == name)
        else {
            return Vec::new();
        };
        let mut out: Vec<CommandInfo> = infos
            .iter()
            .filter(|info| !query.files_only || info.kind.is_external())
            .filter(|info| !query.skip_functions || info.kind != CommandKind::Function)
            .cloned()
            .collect();
        if !query.all {
            out.truncate(1);
        }
        out
    }

    fn unset_function(&mut self, name: &[u8]) -> bool {
        let before = self.functions.len();
        self.functions.retain(|function| function != name);
        self.functions.len() != before
    }
}

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

#[test]
fn every_option_name_round_trips() {
    let mut opts = Opts::default();
    for &flag in ALL_FLAGS {
        let name = flag.name().as_bytes();
        assert_eq!(
            Flag::lookup(name),
            Some(flag),
            "{} should round-trip through lookup",
            flag.name()
        );
        assert_eq!(opts.get_by_name(name), Some(opts.get_flag(flag)));

        opts.set_by_name(name, true).unwrap();
        assert!(opts.get_flag(flag), "{} should be on", flag.name());
        assert_eq!(opts.get_by_name(name), Some(true));

        opts.set_by_name(name, false).unwrap();
        assert!(!opts.get_flag(flag));

        for alias in flag.aliases() {
            assert_eq!(Flag::lookup(alias.as_bytes()), Some(flag));
            opts.set_by_name(alias.as_bytes(), true).unwrap();
            assert!(opts.get_flag(flag));
            opts.set_by_name(alias.as_bytes(), false).unwrap();
        }

        for &kind in flag.kinds() {
            opts.set_by_name_in(kind, name, true).unwrap();
            assert!(opts.get_flag(flag));
            opts.set_by_name_in(kind, name, false).unwrap();
        }

        if let Some(short) = flag.short() {
            assert_eq!(Flag::from_short(short), Some(flag));
            opts.set_by_short(short, true).unwrap();
            assert!(opts.get_flag(flag));
            opts.set_by_short(short, false).unwrap();
        }
    }
}

#[test]
fn option_names_are_unique() {
    let mut names: Vec<&str> = ALL_FLAGS.iter().map(|flag| flag.name()).collect();
    names.sort_unstable();
    let count = names.len();
    names.dedup();
    assert_eq!(names.len(), count, "option names must be unique");

    let mut shorts: Vec<u8> = ALL_FLAGS.iter().filter_map(|flag| flag.short()).collect();
    shorts.sort_unstable();
    let short_count = shorts.len();
    shorts.dedup();
    assert_eq!(shorts.len(), short_count, "short flags must be unique");
}

#[test]
fn option_tables_are_separate_but_share_members() {
    let opts = Opts::default();
    let set_o: Vec<&str> = opts.names(OptionKind::SetO).collect();
    let shopt: Vec<&str> = opts.names(OptionKind::Shopt).collect();

    assert_eq!(set_o.len(), 27);
    assert_eq!(shopt.len(), 44);
    assert!(set_o.contains(&"errexit"));
    assert!(set_o.contains(&"pipefail"));
    assert!(!set_o.contains(&"extglob"));
    assert!(shopt.contains(&"extglob"));
    assert!(shopt.contains(&"xpg_echo"));
    assert!(!shopt.contains(&"errexit"));
    assert!(set_o.contains(&"interactive_comments"));
    assert!(shopt.contains(&"interactive_comments"));

    assert_eq!(
        ALL_FLAGS.len(),
        opts.names(OptionKind::SetO).count() + opts.names(OptionKind::Shopt).count() - 1, // interactive_comments is in both tables
    );
}

#[test]
fn option_kind_is_enforced() {
    let mut opts = Opts::default();
    assert!(matches!(
        opts.set_by_name_in(OptionKind::SetO, b"extglob", true),
        Err(Error::WrongOptionKind { .. })
    ));
    assert!(matches!(
        opts.set_by_name_in(OptionKind::Shopt, b"errexit", true),
        Err(Error::WrongOptionKind { .. })
    ));
    assert!(
        opts.set_by_name_in(OptionKind::SetO, b"errexit", true)
            .is_ok()
    );
    assert!(
        opts.set_by_name_in(OptionKind::Shopt, b"extglob", true)
            .is_ok()
    );
    assert!(matches!(
        opts.set_by_name(b"nonsense", true),
        Err(Error::UnknownOption { .. })
    ));
    assert_eq!(opts.get_by_name(b"nonsense"), None);
}

#[test]
fn option_classification_matches_posix_2024() {
    // POSIX 2024 set(1p) names these; see the opts module documentation.
    for name in [
        b"allexport".as_slice(),
        b"errexit",
        b"ignoreeof",
        b"monitor",
        b"noclobber",
        b"noglob",
        b"noexec",
        b"nolog",
        b"notify",
        b"nounset",
        b"pipefail",
        b"verbose",
        b"vi",
        b"xtrace",
    ] {
        let flag = Flag::lookup(name).expect("known option");
        assert!(flag.is_posix(), "{} should be POSIX", flag.name());
    }
    for name in [
        b"extglob".as_slice(),
        b"globstar",
        b"lastpipe",
        b"nocasematch",
        b"xpg_echo",
        b"expand_aliases",
        b"braceexpand",
        b"histexpand",
    ] {
        let flag = Flag::lookup(name).expect("known option");
        assert!(
            flag.is_bash_extension(),
            "{} should be an extension",
            flag.name()
        );
    }
}

#[test]
fn presets_are_presets_not_code_paths() {
    // Bind through locals so the values are not lint-visible constants.
    let posix = Opts::POSIX;
    let bash = Opts::BASH;
    assert!(posix.posix_mode);
    assert!(!bash.posix_mode);
    assert!(!posix.extglob);
    assert!(bash.expand_aliases);
    assert!(bash.lastpipe);
    // `!$` is a footgun: off even in the interactive profile.
    assert!(!bash.histexpand);
    assert_eq!(Opts::default(), bash);
}
#[test]
fn dash_flags_lists_enabled_short_options() {
    let mut opts = Opts::BASH;
    let flags = opts.dash_flags();
    assert!(flags.contains(&b'h'), "hashall is on in the bash profile");
    // `braceexpand` is a bash default and brace expansion is implemented, so
    // `$-` reports the flag the stage actually honours.
    assert!(
        flags.contains(&b'B'),
        "braceexpand is on in the bash profile"
    );
    assert!(!flags.contains(&b'e'));
    opts.errexit = true;
    assert!(opts.dash_flags().contains(&b'e'));
    opts.errexit = false;
    opts.nounset = true;
    assert!(opts.dash_flags().contains(&b'u'));
}

// ---------------------------------------------------------------------------
// Builtin tiers
// ---------------------------------------------------------------------------

fn sorted_names(tier: Tier) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = names_in(tier).collect();
    names.sort_unstable();
    names
}

#[test]
fn tier_table_has_exact_posix_membership() {
    assert_eq!(
        sorted_names(Tier::Special),
        [
            ".", ":", "break", "continue", "eval", "exec", "exit", "export", "readonly", "return",
            "set", "shift", "times", "trap", "unset",
        ]
    );
    assert_eq!(
        sorted_names(Tier::Intrinsic),
        [
            "alias", "bg", "cd", "command", "fc", "fg", "getopts", "hash", "jobs", "kill", "read",
            "type", "ulimit", "umask", "unalias", "wait",
        ]
    );
    assert_eq!(
        sorted_names(Tier::Regular),
        ["[", "echo", "false", "printf", "pwd", "test", "true"]
    );
    assert_eq!(
        sorted_names(Tier::Extension),
        [
            "builtin",
            "compgen",
            "complete",
            "compopt",
            "declare",
            "dirs",
            "disown",
            "enable",
            "history",
            "let",
            "local",
            "mapfile",
            "popd",
            "pushd",
            "readarray",
            "shopt",
            "source",
            "typeset",
        ]
    );

    assert_eq!(names_in(Tier::Special).count(), 15);
    assert_eq!(names_in(Tier::Intrinsic).count(), 16);
    assert_eq!(names_in(Tier::Regular).count(), 7);
    assert_eq!(names_in(Tier::Extension).count(), 18);
    assert_eq!(BUILTINS.len(), 56);
}

#[test]
fn standard_utilities_are_not_intrinsic() {
    // DESIGN.md §2: these must remain exec-able, so they must not be
    // intrinsic utilities.
    for name in [
        b"printf".as_slice(),
        b"test",
        b"[",
        b"echo",
        b"pwd",
        b"true",
        b"false",
    ] {
        let tier = tier_of(name).expect("registered");
        assert_ne!(
            tier,
            Tier::Intrinsic,
            "{} must not be intrinsic",
            String::from_utf8_lossy(name)
        );
        assert_eq!(tier, Tier::Regular);
    }
    assert_eq!(tier_of(b"kill"), Some(Tier::Intrinsic));
}

#[test]
fn builtin_names_are_unique_and_implemented_set_is_explicit() {
    let mut names: Vec<&str> = BUILTINS.iter().map(|builtin| builtin.name).collect();
    names.sort_unstable();
    let count = names.len();
    names.dedup();
    assert_eq!(names.len(), count);

    let mut implemented: Vec<&str> = BUILTINS
        .iter()
        .filter(|builtin| builtin.is_implemented())
        .map(|builtin| builtin.name)
        .collect();
    implemented.sort_unstable();
    assert_eq!(
        implemented,
        [
            ":", "alias", "builtin", "command", "compgen", "exit", "export",
            "false", "getopts", "readonly", "set", "shift", "shopt", "trap", "true", "type",
            "unalias", "unset",
        ]
    );
    assert_eq!(stubs_in(Tier::Special), 15 - 8);
    // `alias`, `command`, `getopts`, `type` and `unalias` are written; the
    // rest (`bg`, `fc`, `fg`, `jobs`, `kill`, `read`, `ulimit`, `umask`,
    // `wait`, `cd`) are the executor's or still stubs.
    assert_eq!(stubs_in(Tier::Intrinsic), 16 - 5);
    // `hash` and `mapfile`/`readarray` are executor-side bodies, so the
    // registry still reports them as stubs: the executor's body runs only
    // while the entry has none.
    assert!(
        !lookup_builtin(b"hash")
            .expect("registered")
            .is_implemented()
    );
    assert!(
        !lookup_builtin(b"mapfile")
            .expect("registered")
            .is_implemented()
    );
}

#[test]
fn stub_builtins_return_status_and_diagnostic() {
    let mut host = TestHost::new();
    let status = host.run(&[b"local", b"x=1"]);
    assert_eq!(status, Status(2));
    assert_eq!(host.diagnostics.len(), 1);
    assert_eq!(host.diagnostics[0].code, codes::NOT_IMPLEMENTED);
    assert!(host.diagnostics[0].message.contains("local"));
    // Dispatch records the error class for §2.8.1.
    assert!(matches!(
        host.state.error(),
        Some(ShellError::Utility {
            status: Status(2),
            ..
        })
    ));
}

// ---------------------------------------------------------------------------
// Variables and scopes
// ---------------------------------------------------------------------------

#[test]
fn scope_shadowing_and_restoration() {
    let mut vars = Vars::default();
    vars.assign_scalar(b"x", b"global").unwrap();
    assert_eq!(vars.scalar(b"x"), Some(b"global".as_slice()));

    vars.push_frame();
    assert_eq!(vars.scalar(b"x"), Some(b"global".as_slice()));
    vars.local_scalar(b"x", b"local").unwrap();
    assert_eq!(vars.scalar(b"x"), Some(b"local".as_slice()));

    vars.push_frame();
    vars.local_scalar(b"x", b"inner").unwrap();
    assert_eq!(vars.scalar(b"x"), Some(b"inner".as_slice()));
    vars.pop_frame().unwrap();
    assert_eq!(vars.scalar(b"x"), Some(b"local".as_slice()));

    vars.pop_frame().unwrap();
    assert_eq!(vars.scalar(b"x"), Some(b"global".as_slice()));
    assert_eq!(vars.frame_depth(), 0);
    assert!(matches!(vars.pop_frame(), Err(Error::FrameUnderflow)));
}

#[test]
fn unsetting_a_local_reveals_the_outer_variable() {
    let mut vars = Vars::default();
    vars.assign_scalar(b"x", b"global").unwrap();
    vars.push_frame();
    vars.local_scalar(b"x", b"local").unwrap();
    vars.unset(b"x").unwrap();
    assert_eq!(vars.scalar(b"x"), Some(b"global".as_slice()));
    vars.pop_frame().unwrap();
    assert_eq!(vars.scalar(b"x"), Some(b"global".as_slice()));
}

#[test]
fn function_frames_save_and_restore_positional_parameters() {
    let mut vars = Vars::default();
    vars.set_positional(vec![b"outer1".to_vec(), b"outer2".to_vec()]);
    vars.push_frame();
    vars.set_positional(vec![b"inner".to_vec()]);
    assert_eq!(vars.arg_count(), 1);
    vars.shift(1).unwrap();
    assert_eq!(vars.arg_count(), 0);
    vars.pop_frame().unwrap();
    assert_eq!(vars.positional().get(1), Some(b"outer1".as_slice()));
    assert_eq!(vars.positional().get(2), Some(b"outer2".as_slice()));
}

#[test]
fn local_assignment_inside_a_frame_does_not_leak() {
    let mut vars = Vars::default();
    vars.assign_scalar(b"x", b"global").unwrap();
    vars.push_frame();
    // Plain assignment (CreateScope::Default) writes the global even inside a
    // function, matching the shell.
    vars.assign_scalar(b"x", b"changed").unwrap();
    assert_eq!(vars.scalar(b"x"), Some(b"changed".as_slice()));
    vars.declare_local(b"y").unwrap();
    vars.assign_scalar_in(b"y", b"local", CreateScope::Local)
        .unwrap();
    assert_eq!(vars.scalar(b"y"), Some(b"local".as_slice()));
    vars.pop_frame().unwrap();
    assert_eq!(vars.scalar(b"x"), Some(b"changed".as_slice()));
    assert_eq!(vars.scalar(b"y"), None);
}

#[test]
fn local_inherits_exported_but_not_integer() {
    let mut vars = Vars::default();
    vars.assign_scalar(b"x", b"G").unwrap();
    vars.mark_exported(b"x").unwrap();
    vars.assign_scalar(b"n", b"1").unwrap();
    vars.mark_integer(b"n").unwrap();
    vars.push_frame();
    vars.local_scalar(b"x", b"L").unwrap();
    vars.declare_local(b"n").unwrap();
    assert!(vars.is_exported(b"x"));
    assert!(!vars.lookup(b"n").unwrap().attrs.integer);
    vars.pop_frame().unwrap();
    assert!(vars.is_exported(b"x"));
}

#[test]
fn readonly_writes_are_errors_not_panics() {
    let mut vars = Vars::default();
    vars.assign_scalar(b"r", b"1").unwrap();
    vars.mark_readonly(b"r").unwrap();

    assert!(matches!(
        vars.assign_scalar(b"r", b"2"),
        Err(Error::Readonly { .. })
    ));
    assert!(matches!(
        vars.append(b"r", b"x"),
        Err(Error::Readonly { .. })
    ));
    assert!(matches!(vars.unset(b"r"), Err(Error::Readonly { .. })));
    assert!(matches!(vars.resolve(b"r"), Err(Error::Readonly { .. })));
    assert_eq!(vars.scalar(b"r"), Some(b"1".as_slice()));
    // Attribute changes are not value writes.
    vars.mark_exported(b"r").unwrap();
    assert!(vars.is_exported(b"r"));
}

#[test]
fn local_cannot_shadow_a_readonly_global() {
    let mut vars = Vars::default();
    vars.assign_scalar(b"r", b"1").unwrap();
    vars.mark_readonly(b"r").unwrap();
    vars.push_frame();
    assert!(matches!(
        vars.declare_local(b"r"),
        Err(Error::Readonly { .. })
    ));
    assert_eq!(vars.scalar(b"r"), Some(b"1".as_slice()));
}

#[test]
fn invalid_names_are_refused() {
    let mut vars = Vars::default();
    for name in [b"1x".as_slice(), b"", b"a-b", b"a b", b"\xff"] {
        assert!(matches!(
            vars.assign_scalar(name, b"v"),
            Err(Error::InvalidName { .. })
        ));
    }
    assert!(Vars::is_name(b"_x9"));
    assert!(!Vars::is_name(b"9x"));
}

#[test]
fn arrays_keep_shape_order_and_holes() {
    let mut vars = Vars::default();
    // Scalar becomes an indexed array when a subscript is assigned.
    vars.assign_scalar(b"a", b"zero").unwrap();
    vars.set_indexed(b"a", 0, b"0").unwrap();
    vars.set_indexed(b"a", 3, b"3").unwrap();
    assert_eq!(vars.element_indexed(b"a", 0), Some(b"0".as_slice()));
    // Storage is sparse: an unwritten subscript is absent, not an empty cell.
    assert_eq!(vars.element_indexed(b"a", 1), None);
    assert_eq!(vars.element_indexed(b"a", 3), Some(b"3".as_slice()));
    // A scalar assignment to an indexed array sets element 0.
    vars.assign_scalar(b"a", b"new").unwrap();
    assert_eq!(vars.element_indexed(b"a", 0), Some(b"new".as_slice()));

    vars.declare_shape(b"m", ArrayShape::Associative).unwrap();
    vars.set_assoc(b"m", b"b", b"2").unwrap();
    vars.set_assoc(b"m", b"a", b"1").unwrap();
    vars.set_assoc(b"m", b"b", b"20").unwrap();
    let VarValue::Assoc(pairs) = vars.value(b"m").unwrap() else {
        panic!("expected an associative array");
    };
    assert_eq!(
        pairs,
        &[
            (b"b".to_vec(), b"20".to_vec()),
            (b"a".to_vec(), b"1".to_vec()),
        ],
        "updates keep insertion position"
    );
    assert_eq!(vars.element_assoc(b"m", b"a"), Some(b"1".as_slice()));
    assert!(matches!(
        vars.set_indexed(b"m", 0, b"x"),
        Err(Error::NotIndexed { .. })
    ));
    // A bare scalar assignment to an associative array writes key `0`.
    vars.assign_scalar(b"m", b"x").unwrap();
    assert_eq!(vars.element_assoc(b"m", b"0"), Some(b"x".as_slice()));
    vars.declare_shape(b"i", ArrayShape::Indexed).unwrap();
    assert!(matches!(
        vars.set_assoc(b"i", b"k", b"v"),
        Err(Error::NotAssociative { .. })
    ));
    // `a+=text` appends to element zero of an indexed array.
    vars.set_indexed(b"i", 0, b"v").unwrap();
    vars.append(b"i", b"x").unwrap();
    assert_eq!(vars.element_indexed(b"i", 0), Some(b"vx".as_slice()));
}

#[test]
fn whole_value_assignment_and_append() {
    let mut vars = Vars::default();
    vars.assign(
        b"a",
        VarValue::Indexed(std::collections::BTreeMap::from([(0, b"x".to_vec())])),
    )
    .unwrap();
    assert_eq!(vars.element_indexed(b"a", 0), Some(b"x".as_slice()));
    vars.assign_scalar(b"s", b"ab").unwrap();
    vars.append(b"s", b"cd").unwrap();
    assert_eq!(vars.scalar(b"s"), Some(b"abcd".as_slice()));
    vars.append(b"fresh", b"v").unwrap();
    assert_eq!(vars.scalar(b"fresh"), Some(b"v".as_slice()));
}

#[test]
fn positional_shift_underflow_is_an_error_and_does_not_shift() {
    let mut args = Positional::new(vec![b"a".to_vec(), b"b".to_vec()]);
    let error = args.shift(5).unwrap_err();
    assert_eq!(
        error,
        Error::ShiftUnderflow {
            requested: 5,
            available: 2
        }
    );
    assert_eq!(args.count(), 2);
    assert_eq!(args.get(1), Some(b"a".as_slice()));
    assert_eq!(args.get(0), None, "$0 is not a positional parameter");

    args.shift(2).unwrap();
    assert!(args.is_empty());

    let mut vars = Vars::default();
    vars.set_positional(vec![b"one".to_vec()]);
    assert!(vars.shift(2).is_err());
    assert_eq!(vars.arg_count(), 1);
    vars.shift(0).unwrap();
    assert_eq!(vars.arg_count(), 1);
}

#[test]
fn special_parameters_have_a_home() {
    let mut vars = Vars::default();
    vars.set_arg0(b"bsh".to_vec());
    vars.set_last_status(Status(7));
    vars.set_shell_pid(4321);
    vars.set_last_bg_pid(Some(99));
    vars.set_positional(vec![b"a".to_vec(), b"b".to_vec()]);
    let opts = Opts::BASH;

    assert_eq!(vars.special_scalar(b'0', &opts), Some(b"bsh".to_vec()));
    assert_eq!(vars.special_scalar(b'?', &opts), Some(b"7".to_vec()));
    assert_eq!(vars.special_scalar(b'$', &opts), Some(b"4321".to_vec()));
    assert_eq!(vars.special_scalar(b'!', &opts), Some(b"99".to_vec()));
    assert_eq!(vars.special_scalar(b'#', &opts), Some(b"2".to_vec()));
    assert_eq!(vars.special_scalar(b'-', &opts), Some(opts.dash_flags()));
    assert_eq!(vars.special_scalar(b'@', &opts), None);
    assert_eq!(vars.special_scalar(b'*', &opts), None);
    vars.set_last_bg_pid(None);
    assert_eq!(vars.special_scalar(b'!', &opts), Some(Vec::new()));
}

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

fn envp_contains(envp: &[Vec<u8>], entry: &[u8]) -> bool {
    envp.iter().any(|candidate| candidate.as_slice() == entry)
}

#[test]
fn envp_exports_only_the_right_things() {
    let mut vars = Vars::default();
    vars.assign_scalar(b"a", b"1").unwrap();
    vars.mark_exported(b"a").unwrap();
    vars.assign_scalar(b"b", b"2").unwrap(); // not exported
    vars.mark_exported(b"u").unwrap(); // exported but unset
    vars.assign_scalar(b"e", b"").unwrap();
    vars.mark_exported(b"e").unwrap();
    vars.assign(
        b"arr",
        VarValue::Indexed(std::collections::BTreeMap::from([(0, b"x".to_vec())])),
    )
    .unwrap();
    vars.mark_exported(b"arr").unwrap();
    vars.assign_scalar(b"raw", b"a\xffb").unwrap();
    vars.mark_exported(b"raw").unwrap();

    let envp = vars.envp();
    assert!(envp_contains(&envp, b"a=1"));
    assert!(!envp_contains(&envp, b"b=2"));
    assert!(!envp.iter().any(|entry| entry.starts_with(b"u=")));
    assert!(envp_contains(&envp, b"e="));
    assert!(!envp.iter().any(|entry| entry.starts_with(b"arr=")));
    assert!(envp_contains(&envp, b"raw=a\xffb"));

    let mut into = Vec::new();
    vars.envp_into(&mut into);
    assert_eq!(into, envp);
}

#[test]
fn envp_follows_bash_shadowing_rules() {
    let mut vars = Vars::default();
    vars.assign_scalar(b"x", b"G").unwrap();
    vars.mark_exported(b"x").unwrap();

    vars.push_frame();
    vars.local_scalar(b"x", b"L").unwrap();
    // The local inherits exportedness, so children see L.
    assert!(envp_contains(&vars.envp(), b"x=L"));

    // Removing the attribute from the local leaves the exported global entry
    // visible, which is what bash does.
    vars.unmark_exported(b"x").unwrap();
    assert!(envp_contains(&vars.envp(), b"x=G"));

    vars.pop_frame().unwrap();
    assert!(envp_contains(&vars.envp(), b"x=G"));
}

#[test]
fn environment_round_trips() {
    let mut vars = Vars::default();
    vars.assign_scalar(b"PATH", b"/bin:/usr/bin").unwrap();
    vars.mark_exported(b"PATH").unwrap();
    let envp = vars.envp();

    let mut imported = Vars::default();
    assert_eq!(imported.import_envp(&envp), 1);
    assert_eq!(imported.scalar(b"PATH"), Some(b"/bin:/usr/bin".as_slice()));
    assert!(imported.is_exported(b"PATH"));
    assert_eq!(imported.envp(), envp);

    assert!(!imported.import_env_entry(b"no_equals"));
    assert!(!imported.import_env_entry(b"1bad=x"));
    assert_eq!(imported.import_envp(&[b"=x".to_vec()]), 0);
}

// ---------------------------------------------------------------------------
// Prefix assignments (§2.9.1.4)
// ---------------------------------------------------------------------------

#[test]
fn special_builtin_prefix_assignments_persist() {
    let mut vars = Vars::default();
    let applied = vars
        .apply_prefix_assignments(&[(b"x", b"1")], Tier::Special)
        .unwrap();
    assert_eq!(applied.env, vec![b"x=1".to_vec()]);
    assert!(!applied.needs_finish());
    assert_eq!(vars.scalar(b"x"), Some(b"1".as_slice()));
    // bash --posix exports persisted prefix assignments (verified 5.3.20).
    assert!(vars.is_exported(b"x"));
    vars.finish_prefix_assignments(applied).unwrap();
    assert_eq!(vars.scalar(b"x"), Some(b"1".as_slice()));
}

#[test]
fn non_special_prefix_assignments_are_temporary() {
    for tier in [Tier::Intrinsic, Tier::Regular, Tier::Extension] {
        let mut vars = Vars::default();
        let applied = vars
            .apply_prefix_assignments(&[(b"x", b"1")], tier)
            .unwrap();
        assert!(applied.needs_finish());
        assert_eq!(applied.env, vec![b"x=1".to_vec()]);
        assert_eq!(vars.scalar(b"x"), Some(b"1".as_slice()));
        assert!(vars.is_exported(b"x"));
        vars.finish_prefix_assignments(applied).unwrap();
        assert_eq!(
            vars.scalar(b"x"),
            None,
            "{tier:?} assignments must not leak"
        );
        assert_eq!(vars.frame_depth(), 0);
    }
}

#[test]
fn temporary_prefix_assignments_do_not_touch_an_outer_value() {
    let mut vars = Vars::default();
    vars.assign_scalar(b"x", b"outer").unwrap();
    let applied = vars
        .apply_prefix_assignments(&[(b"x", b"inner")], Tier::Intrinsic)
        .unwrap();
    assert_eq!(vars.scalar(b"x"), Some(b"inner".as_slice()));
    vars.finish_prefix_assignments(applied).unwrap();
    assert_eq!(vars.scalar(b"x"), Some(b"outer".as_slice()));
}

#[test]
fn prefix_assignment_to_readonly_fails_cleanly() {
    let mut vars = Vars::default();
    vars.assign_scalar(b"r", b"1").unwrap();
    vars.mark_readonly(b"r").unwrap();

    assert!(matches!(
        vars.apply_prefix_assignments(&[(b"r", b"2")], Tier::Special),
        Err(Error::Readonly { .. })
    ));
    assert_eq!(vars.scalar(b"r"), Some(b"1".as_slice()));

    assert!(matches!(
        vars.apply_prefix_assignments(&[(b"r", b"2")], Tier::Regular),
        Err(Error::Readonly { .. })
    ));
    assert_eq!(vars.frame_depth(), 0, "the temporary frame is unwound");
    assert_eq!(vars.scalar(b"r"), Some(b"1".as_slice()));
}

#[test]
fn repeated_prefix_assignments_keep_the_last_value() {
    let mut vars = Vars::default();
    let applied = vars
        .apply_prefix_assignments(&[(b"x", b"1"), (b"x", b"2")], Tier::Regular)
        .unwrap();
    assert_eq!(applied.env, vec![b"x=2".to_vec()]);
    assert_eq!(vars.scalar(b"x"), Some(b"2".as_slice()));
    vars.finish_prefix_assignments(applied).unwrap();
    assert_eq!(vars.scalar(b"x"), None);
}

#[test]
fn empty_prefix_assignment_list_is_a_no_op() {
    let mut vars = Vars::default();
    let applied = vars.apply_prefix_assignments(&[], Tier::Regular).unwrap();
    assert!(applied.env.is_empty());
    assert!(!applied.needs_finish());
    assert_eq!(vars.frame_depth(), 0);
}

// ---------------------------------------------------------------------------
// Builtins
// ---------------------------------------------------------------------------

#[test]
fn colon_true_false() {
    let mut host = TestHost::new();
    assert_eq!(host.run(&[b":"]), Status::SUCCESS);
    assert_eq!(host.run(&[b"true"]), Status::SUCCESS);
    assert_eq!(host.run(&[b"false"]), Status(1));
    assert!(host.diagnostics.is_empty());
}

#[test]
fn exit_requests_unwinding() {
    let mut host = TestHost::new();
    host.state.vars.set_last_status(Status(7));
    assert_eq!(host.run(&[b"exit"]), Status(7));
    assert_eq!(host.state.exit_requested(), Some(Status(7)));
    assert_eq!(host.state.take_exit_request(), Some(Status(7)));

    assert_eq!(host.run(&[b"exit", b"3"]), Status(3));
    assert_eq!(host.state.exit_requested(), Some(Status(3)));
    host.state.take_exit_request();

    assert_eq!(host.run(&[b"exit", b"abc"]), Status(2));
    assert_eq!(host.state.exit_requested(), Some(Status(2)));
    assert_eq!(host.diagnostics.len(), 1);
    host.state.take_exit_request();

    // Negative statuses are masked by `$?` (255 for -1).
    let status = host.run(&[b"exit", b"-1"]);
    assert_eq!(status.exit_code(), 255);
    host.state.take_exit_request();

    // Too many operands is an error; a non-interactive shell exits 1.
    assert_eq!(host.run(&[b"exit", b"1", b"2"]), Status(1));
    assert_eq!(host.state.exit_requested(), Some(Status(1)));
    host.state.take_exit_request();
    host.interactive = true;
    assert_eq!(host.run(&[b"exit", b"1", b"2"]), Status(1));
    assert_eq!(host.state.exit_requested(), None, "interactive shells stay");
}

#[test]
fn shift_builtin_reports_underflow() {
    let mut host = TestHost::new();
    host.state
        .vars
        .set_positional(vec![b"a".to_vec(), b"b".to_vec()]);
    assert_eq!(host.run(&[b"shift"]), Status::SUCCESS);
    assert_eq!(host.state.vars.arg_count(), 1);

    assert_eq!(host.run(&[b"shift", b"5"]), Status(1));
    assert_eq!(host.state.vars.arg_count(), 1);
    assert_eq!(host.diagnostics.len(), 1);
    assert_eq!(host.diagnostics[0].code, codes::SHIFT_UNDERFLOW);

    assert_eq!(host.run(&[b"shift", b"abc"]), Status(1));
    assert_eq!(host.run(&[b"shift", b"0"]), Status::SUCCESS);
    assert_eq!(host.state.vars.arg_count(), 1);
}

#[test]
fn export_and_unset_builtins() {
    let mut host = TestHost::new();
    assert_eq!(host.run(&[b"export", b"x=1"]), Status::SUCCESS);
    assert_eq!(host.state.vars.scalar(b"x"), Some(b"1".as_slice()));
    assert!(host.state.vars.is_exported(b"x"));

    assert_eq!(host.run(&[b"export"]), Status::SUCCESS);
    let printed = String::from_utf8_lossy(&host.out).into_owned();
    assert!(printed.contains("export x=1"), "{printed}");

    assert_eq!(host.run(&[b"export", b"-n", b"x"]), Status::SUCCESS);
    assert!(!host.state.vars.is_exported(b"x"));

    assert_eq!(host.run(&[b"export", b"x=1"]), Status::SUCCESS);
    assert_eq!(host.run(&[b"export", b"x+=2"]), Status::SUCCESS);
    assert_eq!(host.state.vars.scalar(b"x"), Some(b"12".as_slice()));
    assert!(host.state.vars.is_exported(b"x"));

    assert_eq!(host.run(&[b"export", b"1x=2"]), Status(1));
    assert_eq!(host.diagnostics.len(), 1);
    assert_eq!(host.diagnostics[0].code, codes::INVALID_NAME);

    assert_eq!(host.run(&[b"unset", b"x"]), Status::SUCCESS);
    assert_eq!(host.state.vars.scalar(b"x"), None);
    assert_eq!(host.run(&[b"unset", b"missing"]), Status::SUCCESS);

    // `unset -f` reaches the host's function table; a name that is not there
    // is not an error.
    host.functions.push(b"f".to_vec());
    assert_eq!(host.run(&[b"unset", b"-f", b"f"]), Status::SUCCESS);
    assert!(host.functions.is_empty());
    assert_eq!(host.run(&[b"unset", b"-f", b"f"]), Status::SUCCESS);
    // `unset -n` (nameref removal) is still deferred.
    assert_eq!(host.run(&[b"unset", b"-n", b"ref"]), Status(2));
    assert!(host.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == codes::NOT_IMPLEMENTED && diagnostic.message.contains("nameref removal")
    }));
}

#[test]
fn unset_refuses_readonly_variables() {
    let mut host = TestHost::new();
    assert_eq!(host.run(&[b"readonly", b"r=1"]), Status::SUCCESS);
    assert!(host.state.vars.is_readonly(b"r"));
    // `+=` on a readonly variable is still a write and is refused.
    assert_eq!(host.run(&[b"readonly", b"r+=2"]), Status(1));
    assert_eq!(host.state.vars.scalar(b"r"), Some(b"1".as_slice()));
    assert_eq!(host.run(&[b"unset", b"r"]), Status(1));
    assert_eq!(host.state.vars.scalar(b"r"), Some(b"1".as_slice()));
    assert!(matches!(
        host.state.error(),
        Some(ShellError::SpecialBuiltin { .. })
    ));
}

#[test]
fn readonly_assignment_is_a_variable_assignment_error() {
    let mut host = TestHost::new();
    assert_eq!(host.run(&[b"readonly", b"r=1"]), Status::SUCCESS);
    let status = host.run(&[b"export", b"r=2"]);
    assert_eq!(status, Status(1));
    assert_eq!(host.state.vars.scalar(b"r"), Some(b"1".as_slice()));
    assert_eq!(host.state.error(), Some(ShellError::VariableAssignment));
}

#[test]
fn set_builtin_toggles_options() {
    let mut host = TestHost::new();
    assert_eq!(host.run(&[b"set", b"-e"]), Status::SUCCESS);
    assert!(host.state.opts.errexit);
    assert_eq!(host.run(&[b"set", b"+e"]), Status::SUCCESS);
    assert!(!host.state.opts.errexit);

    assert_eq!(host.run(&[b"set", b"-eu"]), Status::SUCCESS);
    assert!(host.state.opts.errexit);
    assert!(host.state.opts.nounset);
    assert_eq!(host.run(&[b"set", b"+eu"]), Status::SUCCESS);
    assert!(!host.state.opts.errexit);
    assert!(!host.state.opts.nounset);

    assert_eq!(host.run(&[b"set", b"-o", b"pipefail"]), Status::SUCCESS);
    assert!(host.state.opts.pipefail);
    assert_eq!(host.run(&[b"set", b"+o", b"pipefail"]), Status::SUCCESS);
    assert!(!host.state.opts.pipefail);

    // The form every bash script uses: `-o` closes the bundle and takes the
    // next word as its name.
    assert_eq!(host.run(&[b"set", b"-euo", b"pipefail"]), Status::SUCCESS);
    assert!(host.state.opts.errexit);
    assert!(host.state.opts.nounset);
    assert!(host.state.opts.pipefail);
    assert_eq!(host.run(&[b"set", b"+euo", b"pipefail"]), Status::SUCCESS);
    assert!(!host.state.opts.errexit);
    assert!(!host.state.opts.nounset);
    assert!(!host.state.opts.pipefail);

    assert_eq!(host.run(&[b"set", b"-x"]), Status::SUCCESS);
    assert!(host.state.opts.xtrace);
    assert!(host.state.opts.dash_flags().contains(&b'x'));

    assert_eq!(host.run(&[b"set", b"-q"]), Status(2));
    assert_eq!(host.run(&[b"set", b"-o", b"nonsense"]), Status(2));
    assert_eq!(host.run(&[b"set", b"-o", b"extglob"]), Status(2));
    assert!(!host.state.opts.extglob);
}

#[test]
fn set_builtin_handles_positionals_and_printing() {
    let mut host = TestHost::new();
    assert_eq!(host.run(&[b"set", b"--", b"a", b"b"]), Status::SUCCESS);
    assert_eq!(host.state.vars.arg_count(), 2);
    assert_eq!(host.state.vars.positional().get(2), Some(b"b".as_slice()));

    assert_eq!(host.run(&[b"set", b"--"]), Status::SUCCESS);
    assert_eq!(host.state.vars.arg_count(), 0);

    assert_eq!(host.run(&[b"set", b"one", b"two"]), Status::SUCCESS);
    assert_eq!(host.state.vars.arg_count(), 2);
    assert_eq!(host.state.vars.positional().get(1), Some(b"one".as_slice()));

    host.state.opts.errexit = true;
    assert_eq!(host.run(&[b"set", b"-o"]), Status::SUCCESS);
    let printed = String::from_utf8_lossy(&host.out).into_owned();
    assert!(printed.contains("errexit\ton"), "{printed}");
    host.out.clear();

    assert_eq!(host.run(&[b"set", b"+o"]), Status::SUCCESS);
    let reinput = String::from_utf8_lossy(&host.out).into_owned();
    assert!(reinput.contains("set -o errexit"), "{reinput}");
    assert!(reinput.contains("set +o nounset"), "{reinput}");
    host.out.clear();

    host.state.vars.assign_scalar(b"v", b"hi there").unwrap();
    host.state.vars.mark_exported(b"u").unwrap();
    assert_eq!(host.run(&[b"set"]), Status::SUCCESS);
    let listing = String::from_utf8_lossy(&host.out).into_owned();
    assert!(listing.contains("v='hi there'"), "{listing}");
    // POSIX's no-argument `set` format is NAME=VALUE; an unset variable has an
    // empty value, not a missing equals sign.
    assert!(listing.contains("u="), "{listing}");

    // Historical `set -` turns off -v and -x.
    host.state.opts.verbose = true;
    host.state.opts.xtrace = true;
    assert_eq!(host.run(&[b"set", b"-"]), Status::SUCCESS);
    assert!(!host.state.opts.verbose);
    assert!(!host.state.opts.xtrace);
}

// ---------------------------------------------------------------------------
// §2.8.1 consequences
// ---------------------------------------------------------------------------

#[test]
fn consequence_table_matches_posix_2_8_1() {
    // Syntax errors: non-interactive exits, interactive continues.
    assert_eq!(consequence(&ShellError::Syntax, true), Action::Continue);
    assert_eq!(
        consequence(&ShellError::Syntax, false),
        Action::Exit(Status(2))
    );

    // Special built-in failure, direct vs through `command`.
    let direct = ShellError::SpecialBuiltin {
        status: Status(1),
        via_command: false,
    };
    assert_eq!(consequence(&direct, true), Action::Continue);
    assert_eq!(consequence(&direct, false), Action::Exit(Status(1)));
    let via_command = ShellError::SpecialBuiltin {
        status: Status(1),
        via_command: true,
    };
    assert_eq!(consequence(&via_command, false), Action::Continue);

    // Redirection with a special builtin exits non-interactively.
    assert_eq!(
        consequence(&ShellError::SpecialBuiltinRedirection, false),
        Action::Exit(Status(1))
    );
    assert_eq!(
        consequence(&ShellError::SpecialBuiltinRedirection, true),
        Action::Continue
    );

    // Other utilities and their redirection errors never exit.
    for error in [
        ShellError::Utility {
            status: Status(1),
            redirection: false,
        },
        ShellError::Utility {
            status: Status(1),
            redirection: true,
        },
        ShellError::CompoundRedirection,
        ShellError::FunctionRedirection,
        ShellError::CommandNotFound,
    ] {
        assert_eq!(consequence(&error, true), Action::Continue);
        assert_eq!(consequence(&error, false), Action::Continue);
    }

    // Assignment and expansion errors exit a non-interactive shell.
    for error in [ShellError::VariableAssignment, ShellError::Expansion] {
        assert_eq!(consequence(&error, true), Action::Continue);
        assert_eq!(consequence(&error, false), Action::Exit(Status(1)));
    }

    // A read error is fatal even interactively.
    assert_eq!(
        consequence(&ShellError::ReadError, true),
        Action::Exit(Status(1))
    );
    assert_eq!(
        consequence(&ShellError::ReadError, false),
        Action::Exit(Status(1))
    );
}

#[test]
fn builtin_tier_derives_the_error_class() {
    assert_eq!(
        ShellError::from_builtin(Tier::Special, Status(1)),
        ShellError::SpecialBuiltin {
            status: Status(1),
            via_command: false
        }
    );
    assert_eq!(
        ShellError::from_builtin(Tier::Intrinsic, Status(1)),
        ShellError::Utility {
            status: Status(1),
            redirection: false
        }
    );
}

// ---------------------------------------------------------------------------
// Misc invariants
// ---------------------------------------------------------------------------

#[test]
fn declaration_scope_is_local_only_inside_a_frame() {
    let mut vars = Vars::default();
    assert_eq!(declaration_scope(&vars), CreateScope::Global);
    vars.push_frame();
    assert_eq!(declaration_scope(&vars), CreateScope::Local);
}

#[test]
fn nameref_hook_currently_resolves_identity() {
    let mut vars = Vars::default();
    vars.assign_scalar(b"x", b"1").unwrap();
    assert_eq!(vars.resolve(b"x").unwrap().scalar(), Some(b"1".as_slice()));
    // A miss creates in the global scope, and reads do not create.
    assert_eq!(vars.lookup(b"y"), None);
    vars.resolve(b"y").unwrap();
    assert!(vars.contains(b"y"));
}

#[test]
fn set_option_table_is_printable() {
    let table = set_option_table(&Opts::BASH);
    assert!(table.iter().any(|(name, on)| *name == "pipefail" && !*on));
    // On in the bash profile, like every other bash default; `set +B` prints
    // it as `off` in the `set -o` table.
    assert!(table.iter().any(|(name, on)| *name == "braceexpand" && *on));
}

#[test]
fn indexed_arrays_are_sparse_and_ordered() {
    let mut vars = Vars::default();
    vars.set_indexed(b"a", 100, b"x").unwrap();
    assert_eq!(
        vars.array_len(b"a"),
        1,
        "a[100] must not allocate 101 slots"
    );
    assert_eq!(vars.max_index(b"a"), Some(100));
    assert_eq!(vars.element_indexed(b"a", 0), None);
    assert_eq!(vars.element_indexed(b"a", 100), Some(b"x".as_slice()));

    vars.set_indexed(b"a", 3, b"y").unwrap();
    vars.set_indexed(b"a", 0, b"z").unwrap();
    let VarValue::Indexed(elements) = vars.value(b"a").unwrap() else {
        panic!("expected an indexed array");
    };
    assert_eq!(
        elements.keys().copied().collect::<Vec<_>>(),
        vec![0, 3, 100],
        "iteration order is subscript order"
    );

    // Unsetting leaves a hole; the count and the greatest index agree with it.
    vars.unset_element_indexed(b"a", 3).unwrap();
    assert_eq!(vars.array_len(b"a"), 2);
    assert_eq!(vars.element_indexed(b"a", 3), None);
    vars.unset_element_indexed(b"a", 100).unwrap();
    assert_eq!(vars.max_index(b"a"), Some(0));
    // Removing an absent, non-negative subscript succeeds.
    vars.unset_element_indexed(b"a", 77).unwrap();
}

#[test]
fn negative_subscripts_resolve_against_the_greatest_index() {
    let mut vars = Vars::default();
    for (index, value) in [(0, b"zero".as_slice()), (2, b"two")] {
        vars.set_indexed(b"a", index, value).unwrap();
    }
    // greatest+1 is 3: -1 is 2, -2 is the hole at 1, -3 is 0.
    assert_eq!(vars.element_indexed(b"a", -1), Some(b"two".as_slice()));
    assert_eq!(vars.element_indexed(b"a", -2), None);
    assert_eq!(vars.element_indexed(b"a", -3), Some(b"zero".as_slice()));
    assert_eq!(vars.element_indexed(b"a", -4), None);

    // A write at -1 lands on the greatest element and keeps it the greatest.
    vars.set_indexed(b"a", -1, b"TWO").unwrap();
    assert_eq!(vars.element_indexed(b"a", 2), Some(b"TWO".as_slice()));
    // Before the start is an error, and leaves the array untouched.
    assert!(matches!(
        vars.set_indexed(b"a", -4, b"x"),
        Err(Error::ArrayOutOfBounds { .. })
    ));
    assert_eq!(vars.element_indexed(b"a", 2), Some(b"TWO".as_slice()));
}

#[test]
fn indexed_append_and_scalar_promotion() {
    let mut vars = Vars::default();
    vars.set_indexed(b"a", 10, b"a").unwrap();
    vars.append_indexed(b"a", 10, b"b").unwrap();
    assert_eq!(vars.element_indexed(b"a", 10), Some(b"ab".as_slice()));
    // Appending to an absent element creates it.
    vars.append_indexed(b"a", 20, b"c").unwrap();
    assert_eq!(vars.element_indexed(b"a", 20), Some(b"c".as_slice()));

    // `s=hi; s[2]=x` keeps the old value at element zero.
    vars.assign_scalar(b"s", b"hi").unwrap();
    vars.set_indexed(b"s", 2, b"x").unwrap();
    assert_eq!(vars.element_indexed(b"s", 0), Some(b"hi".as_slice()));
    assert_eq!(vars.element_indexed(b"s", 2), Some(b"x".as_slice()));

    // A scalar converts to an associative array with key zero.
    vars.assign_scalar(b"m", b"hello").unwrap();
    vars.declare_shape(b"m", ArrayShape::Associative).unwrap();
    assert_eq!(vars.element_assoc(b"m", b"0"), Some(b"hello".as_slice()));
    // But an indexed array cannot change shape.
    assert!(matches!(
        vars.declare_shape(b"s", ArrayShape::Associative),
        Err(Error::ArrayShapeMismatch { .. })
    ));
}

#[test]
fn associative_arrays_keep_insertion_order_and_removal() {
    let mut vars = Vars::default();
    vars.declare_shape(b"m", ArrayShape::Associative).unwrap();
    vars.set_assoc(b"m", b"b", b"2").unwrap();
    vars.set_assoc(b"m", b"a", b"1").unwrap();
    vars.append_assoc(b"m", b"b", b"0").unwrap();
    assert_eq!(vars.element_assoc(b"m", b"b"), Some(b"20".as_slice()));
    let VarValue::Assoc(pairs) = vars.value(b"m").unwrap() else {
        panic!("expected an associative array");
    };
    assert_eq!(
        pairs,
        &[
            (b"b".to_vec(), b"20".to_vec()),
            (b"a".to_vec(), b"1".to_vec()),
        ],
        "updating a key keeps its position"
    );
    vars.unset_element_assoc(b"m", b"b").unwrap();
    assert_eq!(vars.element_assoc(b"m", b"b"), None);
    assert_eq!(vars.array_len(b"m"), 1);
    // Removing an absent key succeeds.
    vars.unset_element_assoc(b"m", b"zzz").unwrap();
}

// ---------------------------------------------------------------------------
// type, command, builtin and aliases
// ---------------------------------------------------------------------------

fn classified(kind: CommandKind, target: &[u8]) -> CommandInfo {
    CommandInfo {
        kind,
        target: target.to_vec(),
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn var(host: &TestHost, name: &[u8]) -> String {
    host.state
        .vars
        .scalar(name)
        .map(text)
        .unwrap_or_else(|| "<unset>".to_owned())
}

/// The classifications every `type` test starts from.
fn introspection_host() -> TestHost {
    let mut host = TestHost::new();
    host.classify(b"while", vec![CommandInfo::plain(CommandKind::Keyword)]);
    host.classify(b"ll", vec![classified(CommandKind::Alias, b"ls -l")]);
    host.classify(b"myfunc", vec![CommandInfo::plain(CommandKind::Function)]);
    host.classify(
        b"eval",
        vec![CommandInfo::plain(CommandKind::Builtin(Tier::Special))],
    );
    host.classify(
        b"cd",
        vec![CommandInfo::plain(CommandKind::Builtin(Tier::Intrinsic))],
    );
    host.classify(
        b"date",
        vec![classified(CommandKind::External, b"/bin/date")],
    );
    host.classify(
        b"pwd",
        vec![
            CommandInfo::plain(CommandKind::Function),
            CommandInfo::plain(CommandKind::Builtin(Tier::Regular)),
            classified(CommandKind::External, b"/bin/pwd"),
            classified(CommandKind::External, b"/tmp/pwd"),
        ],
    );
    host
}

#[test]
fn type_names_each_class_the_way_the_suite_expects() {
    let mut host = introspection_host();
    let status = host.run(&[b"type", b"while", b"ll", b"myfunc", b"eval", b"cd", b"date"]);
    assert_eq!(status, Status::SUCCESS);
    assert_eq!(
        text(&host.out),
        "while is a shell keyword\n\
         ll is an alias for \"ls -l\"\n\
         myfunc is a shell function\n\
         eval is a special shell builtin\n\
         cd is a shell builtin\n\
         date is /bin/date\n"
    );
}

#[test]
fn type_t_prints_one_word_and_stays_silent_when_a_name_is_missing() {
    let mut host = introspection_host();
    let status = host.run(&[b"type", b"-t", b"while", b"ll", b"eval", b"date"]);
    assert_eq!(status, Status::SUCCESS);
    assert_eq!(text(&host.out), "keyword\nalias\nbuiltin\nfile\n");

    // `-t` answers through the status only: no diagnostic, no stdout.
    host.out.clear();
    host.diagnostics.clear();
    assert_eq!(host.run(&[b"type", b"-t", b"zz", b"date"]), Status::FAILURE);
    assert_eq!(text(&host.out), "file\n");
    assert!(host.diagnostics.is_empty(), "type -t must not diagnose");

    // The plain form does diagnose.
    assert_eq!(host.run(&[b"type", b"zz"]), Status::FAILURE);
    assert_eq!(host.diagnostics[0].code, codes::NOT_FOUND);
    assert!(host.diagnostics[0].message.contains("zz: not found"));
}

#[test]
fn type_a_lists_the_whole_search_order() {
    let mut host = introspection_host();
    assert_eq!(host.run(&[b"type", b"-a", b"pwd"]), Status::SUCCESS);
    assert_eq!(
        text(&host.out),
        "pwd is a shell function\n\
         pwd is a shell builtin\n\
         pwd is /bin/pwd\n\
         pwd is /tmp/pwd\n"
    );

    // `-f` skips the function; `-a -p` prints only files.
    host.out.clear();
    assert_eq!(host.run(&[b"type", b"-a", b"-f", b"pwd"]), Status::SUCCESS);
    assert_eq!(
        text(&host.out),
        "pwd is a shell builtin\npwd is /bin/pwd\npwd is /tmp/pwd\n"
    );
    host.out.clear();
    assert_eq!(host.run(&[b"type", b"-ap", b"pwd"]), Status::SUCCESS);
    assert_eq!(text(&host.out), "/bin/pwd\n/tmp/pwd\n");
    // `type -a` on a name with one match prints one line.
    host.out.clear();
    assert_eq!(host.run(&[b"type", b"-a", b"date"]), Status::SUCCESS);
    assert_eq!(text(&host.out), "date is /bin/date\n");
}

#[test]
fn command_v_reports_functions_and_files_differently() {
    let mut host = introspection_host();
    // A function is reported by name; an external by path. This is the
    // distinction scripts actually rely on (`command -v f` vs `command -v ls`).
    assert_eq!(
        host.run(&[b"command", b"-v", b"myfunc", b"date"]),
        Status::SUCCESS
    );
    assert_eq!(text(&host.out), "myfunc\n/bin/date\n");

    host.out.clear();
    assert_eq!(
        host.run(&[b"command", b"-V", b"myfunc", b"date"]),
        Status::SUCCESS
    );
    assert_eq!(
        text(&host.out),
        "myfunc is a shell function\ndate is /bin/date\n"
    );

    // An alias prints as a definition, which is the only re-input-safe form.
    host.out.clear();
    assert_eq!(host.run(&[b"command", b"-v", b"ll"]), Status::SUCCESS);
    assert_eq!(text(&host.out), "alias ll='ls -l'\n");
}

#[test]
fn command_v_fails_when_any_name_is_missing_but_still_prints_the_rest() {
    let mut host = introspection_host();
    assert_eq!(
        host.run(&[b"command", b"-v", b"myfunc", b"ZZZ", b"date"]),
        Status::FAILURE
    );
    assert_eq!(text(&host.out), "myfunc\n/bin/date\n");
    // `-v` says nothing about the failure; `-V` does.
    assert!(host.diagnostics.is_empty());
    host.out.clear();
    assert_eq!(host.run(&[b"command", b"-V", b"ZZZ"]), Status::FAILURE);
    assert_eq!(text(&host.out), "");
    assert_eq!(host.diagnostics[0].code, codes::NOT_FOUND);
}

#[test]
fn alias_output_reparses_and_unalias_removes() {
    let mut host = TestHost::new();
    assert_eq!(
        host.run(&[b"alias", b"ll=ls -l", b"e=echo"]),
        Status::SUCCESS
    );
    assert_eq!(host.state.aliases.get(b"ll"), Some(&b"ls -l"[..]));

    // Listing is name-sorted and always single-quoted, so feeding it back to
    // the shell recreates exactly these definitions.
    host.out.clear();
    assert_eq!(host.run(&[b"alias"]), Status::SUCCESS);
    assert_eq!(text(&host.out), "alias e='echo'\nalias ll='ls -l'\n");

    // A value containing a quote stays re-input-safe.
    assert_eq!(host.run(&[b"alias", b"q=it's"]), Status::SUCCESS);
    host.out.clear();
    assert_eq!(host.run(&[b"alias", b"q"]), Status::SUCCESS);
    assert_eq!(text(&host.out), "alias q='it'\\''s'\n");

    assert_eq!(host.run(&[b"alias", b"missing"]), Status::FAILURE);
    assert_eq!(host.run(&[b"unalias", b"missing"]), Status::FAILURE);
    assert_eq!(host.run(&[b"unalias", b"ll"]), Status::SUCCESS);
    assert_eq!(host.state.aliases.get(b"ll"), None);
    assert_eq!(host.run(&[b"unalias", b"-a"]), Status::SUCCESS);
    assert!(host.state.aliases.is_empty());
    // A bare `unalias` is a usage error.
    assert_eq!(host.run(&[b"unalias"]), Status(2));
}

// ---------------------------------------------------------------------------
// getopts
// ---------------------------------------------------------------------------

#[test]
fn getopts_parses_a_cluster_lazily_then_advances() {
    let mut host = TestHost::new();
    host.state.vars.set_positional(vec![b"-ab".to_vec()]);

    assert_eq!(host.run(&[b"getopts", b"ab", b"opt"]), Status::SUCCESS);
    assert_eq!(var(&host, b"opt"), "a");
    // The cluster is not exhausted, so `OPTIND` still names the argument.
    assert_eq!(var(&host, b"OPTIND"), "1");

    assert_eq!(host.run(&[b"getopts", b"ab", b"opt"]), Status::SUCCESS);
    assert_eq!(var(&host, b"opt"), "b");
    assert_eq!(var(&host, b"OPTIND"), "2");

    // Past the end: status 1, and the variable is still assigned `?`.
    assert_eq!(host.run(&[b"getopts", b"ab", b"opt"]), Status::FAILURE);
    assert_eq!(var(&host, b"opt"), "?");
}

#[test]
fn getopts_takes_a_fused_or_separate_argument() {
    let mut host = TestHost::new();
    host.state
        .vars
        .set_positional(vec![b"-c10".to_vec(), b"x".to_vec()]);
    assert_eq!(host.run(&[b"getopts", b"c:", b"opt"]), Status::SUCCESS);
    assert_eq!(var(&host, b"opt"), "c");
    assert_eq!(var(&host, b"OPTARG"), "10");
    assert_eq!(var(&host, b"OPTIND"), "2");

    host.state.vars.assign_scalar(b"OPTIND", b"1").unwrap();
    host.state
        .vars
        .set_positional(vec![b"-c".to_vec(), b"10".to_vec()]);
    assert_eq!(host.run(&[b"getopts", b"c:", b"opt"]), Status::SUCCESS);
    assert_eq!(var(&host, b"OPTARG"), "10");
    // Both words were consumed.
    assert_eq!(var(&host, b"OPTIND"), "3");
}

#[test]
fn getopts_reports_a_missing_argument_verbosely_and_silently() {
    let mut host = TestHost::new();
    host.state.vars.set_positional(vec![b"-c".to_vec()]);
    assert_eq!(host.run(&[b"getopts", b"c:", b"opt"]), Status::SUCCESS);
    assert_eq!(var(&host, b"opt"), "?");
    assert_eq!(var(&host, b"OPTARG"), "");
    assert_eq!(var(&host, b"OPTIND"), "2");
    assert!(
        host.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("option requires an argument"))
    );

    // Leading `:`: the result is `:` with the option letter in `OPTARG`, and
    // no diagnostic at all.
    host.diagnostics.clear();
    host.state.vars.assign_scalar(b"OPTIND", b"1").unwrap();
    assert_eq!(host.run(&[b"getopts", b":c:", b"opt"]), Status::SUCCESS);
    assert_eq!(var(&host, b"opt"), ":");
    assert_eq!(var(&host, b"OPTARG"), "c");
    assert!(host.diagnostics.is_empty());

    // An unknown option in silent mode hands back the character.
    host.state.vars.assign_scalar(b"OPTIND", b"1").unwrap();
    host.state.vars.set_positional(vec![b"-Z".to_vec()]);
    assert_eq!(host.run(&[b"getopts", b":c", b"opt"]), Status::SUCCESS);
    assert_eq!(var(&host, b"opt"), "?");
    assert_eq!(var(&host, b"OPTARG"), "Z");
    assert!(host.diagnostics.is_empty());
}

#[test]
fn getopts_consumes_double_dash_and_stops_at_an_operand() {
    let mut host = TestHost::new();
    host.state
        .vars
        .set_positional(vec![b"-a".to_vec(), b"--".to_vec(), b"-b".to_vec()]);
    assert_eq!(host.run(&[b"getopts", b"a", b"opt"]), Status::SUCCESS);
    assert_eq!(var(&host, b"opt"), "a");
    assert_eq!(var(&host, b"OPTIND"), "2");

    // `--` ends the options and is consumed, so `shift $((OPTIND-1))` leaves
    // the operands exactly as written.
    assert_eq!(host.run(&[b"getopts", b"a", b"opt"]), Status::FAILURE);
    assert_eq!(var(&host, b"OPTIND"), "3");

    // A non-option word stops the scan without being consumed.
    host.state.vars.assign_scalar(b"OPTIND", b"1").unwrap();
    host.state
        .vars
        .set_positional(vec![b"x".to_vec(), b"-a".to_vec()]);
    assert_eq!(host.run(&[b"getopts", b"a", b"opt"]), Status::FAILURE);
    assert_eq!(var(&host, b"OPTIND"), "1");
}

#[test]
fn getopts_assigns_optind_back_into_range_and_optarg_is_always_set() {
    let mut host = TestHost::new();
    host.state
        .vars
        .set_positional(vec![b"a".to_vec(), b"b".to_vec()]);
    host.state.vars.assign_scalar(b"OPTIND", b"9").unwrap();
    assert_eq!(host.run(&[b"getopts", b"a", b"opt"]), Status::FAILURE);
    // Parked at `$# + 1`, which is where bash leaves it.
    assert_eq!(var(&host, b"OPTIND"), "3");

    // A flag that takes no argument still sets `OPTARG` (to empty), so a
    // `set -u` script can read it.
    host.state.vars.assign_scalar(b"OPTIND", b"1").unwrap();
    host.state.vars.unset(b"OPTARG").unwrap();
    host.state.vars.set_positional(vec![b"-a".to_vec()]);
    assert_eq!(host.run(&[b"getopts", b"a", b"opt"]), Status::SUCCESS);
    assert_eq!(var(&host, b"OPTARG"), "");
}

#[test]
fn getopts_rejects_an_invalid_option_variable_after_parsing() {
    let mut host = TestHost::new();
    host.state
        .vars
        .set_positional(vec![b"-c".to_vec(), b"foo".to_vec()]);
    // The parse happens; only the assignment fails, and that is status 2.
    assert_eq!(host.run(&[b"getopts", b"c:", b"opt-"]), Status(2));
    assert_eq!(var(&host, b"OPTARG"), "foo");
    assert_eq!(var(&host, b"OPTIND"), "3");
    assert_eq!(var(&host, b"opt"), "<unset>");
}

#[test]
fn shellopts_and_bashopts_are_derived_from_the_tables() {
    let mut opts = Opts::BASH;
    opts.set_by_name(b"hashall", true).expect("hashall");
    let shellopts = String::from_utf8(opts.shellopts()).expect("ascii");
    assert!(shellopts.split(':').any(|name| name == "hashall"));
    // A `shopt` option is not a `set -o` option and must not leak into
    // SHELLOPTS; bash keeps the two lists disjoint.
    assert!(!shellopts.split(':').any(|name| name == "extglob"));

    Opts::set_shopt_by_name(&mut opts, b"extglob", true).expect("extglob");
    let bashopts = String::from_utf8(opts.bashopts()).expect("ascii");
    assert!(bashopts.split(':').any(|name| name == "extglob"));
    assert!(!bashopts.split(':').any(|name| name == "hashall"));

    // Disabling removes it again: the strings are derived, never cached.
    Opts::set_shopt_by_name(&mut opts, b"extglob", false).expect("unset");
    let bashopts = String::from_utf8(opts.bashopts()).expect("ascii");
    assert!(!bashopts.split(':').any(|name| name == "extglob"));
}

#[test]
fn vi_and_emacs_are_mutually_exclusive() {
    let mut opts = Opts::BASH;
    opts.set_by_name(b"vi", true).expect("vi");
    assert_eq!(
        opts.options(OptionKind::SetO)
            .filter(|o| o.on)
            .filter(|o| o.name == "vi")
            .count(),
        1
    );
    opts.set_by_name(b"emacs", true).expect("emacs");
    let on: Vec<&str> = opts
        .options(OptionKind::SetO)
        .filter(|option| option.on)
        .map(|option| option.name)
        .filter(|name| *name == "vi" || *name == "emacs")
        .collect();
    assert_eq!(on, vec!["emacs"], "enabling emacs must disable vi");
}
