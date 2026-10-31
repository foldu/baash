//! Parser coverage: one test per construct, plus the AST shapes the compiler
//! will rely on.

mod common;

use bsh_syntax::{
    AndOrOp, CaseHow, CaseTerm, CmdSubStyle, DoubleQ, Lit, LitKind, NodeKind, ParamExp, ParamName,
    ParamOp, Part, PipeOp, RedirectOp, ReplaceMode, Separator, Transform, UnsupportedOp, Word,
};

fn first_simple(ast: &bsh_syntax::Ast) -> &bsh_syntax::SimpleCommand {
    match &ast.node(common::first_command(ast)).kind {
        NodeKind::Simple(cmd) => cmd,
        other => panic!("first command is not simple: {other:?}"),
    }
}

fn cmd_words(ast: &bsh_syntax::Ast, src: &[u8]) -> Vec<Vec<u8>> {
    first_simple(ast)
        .words
        .iter()
        .map(|w| src[w.span.clone()].to_vec())
        .collect()
}

#[test]
fn simple_command_with_assignments_words_and_redirects() {
    let src = b"a=1 b=2 cmd arg >out 2>&1";
    let ast = common::ast(src);
    let cmd = first_simple(&ast);
    assert_eq!(cmd.assignments.len(), 2);
    assert_eq!(cmd.assignments[0].name, 0..1);
    assert_eq!(&src[cmd.assignments[0].value.span.clone()], b"1");
    assert_eq!(cmd.assignments[1].name, 4..5);
    assert_eq!(cmd.words.len(), 2);
    assert_eq!(&src[cmd.words[0].span.clone()], b"cmd");
    assert_eq!(cmd.redirects.len(), 2);
    assert_eq!(cmd.redirects[0].op, RedirectOp::Write);
    assert_eq!(
        &src[cmd.redirects[0].target.as_ref().unwrap().span.clone()],
        b"out"
    );
    assert_eq!(cmd.redirects[1].fd, Some(2));
    assert_eq!(cmd.redirects[1].op, RedirectOp::DupWrite);
    assert_eq!(
        &src[cmd.redirects[1].target.as_ref().unwrap().span.clone()],
        b"1"
    );
}

#[test]
fn lists_and_separators() {
    let ast = common::ast(b"a; b & c\nd");
    let Some(NodeKind::List { items }) =
        common::find_node(&ast, |k| matches!(k, NodeKind::List { .. }))
    else {
        panic!("no list");
    };
    assert_eq!(items.len(), 4);
    assert_eq!(items[0].sep, Some(Separator::Semi));
    assert_eq!(items[1].sep, Some(Separator::Amp));
    assert_eq!(items[2].sep, Some(Separator::Newline));
    assert_eq!(items[3].sep, None);
}

#[test]
fn and_or_chain() {
    let ast = common::ast(b"a && b || c");
    let Some(NodeKind::AndOr { rest, .. }) =
        common::find_node(&ast, |k| matches!(k, NodeKind::AndOr { .. }))
    else {
        panic!("no and-or");
    };
    assert_eq!(rest.len(), 2);
    assert_eq!(rest[0].op, AndOrOp::And);
    assert_eq!(rest[1].op, AndOrOp::Or);
}

#[test]
fn pipelines_negation_and_stderr() {
    let ast = common::ast(b"! a | b |& c");
    let Some(NodeKind::Pipeline {
        negated,
        stages,
        links,
    }) = common::find_node(&ast, |k| matches!(k, NodeKind::Pipeline { .. }))
    else {
        panic!("no pipeline");
    };
    assert!(negated);
    assert_eq!(stages.len(), 3);
    assert_eq!(links, &[PipeOp::Pipe, PipeOp::PipeStderr]);
}

#[test]
fn if_elif_else() {
    let ast = common::ast(b"if a; then b; elif c; then d; else e; fi");
    let Some(NodeKind::If {
        branches,
        else_body,
    }) = common::find_node(&ast, |k| matches!(k, NodeKind::If { .. }))
    else {
        panic!("no if");
    };
    assert_eq!(branches.len(), 2);
    assert!(else_body.is_some());
}

#[test]
fn for_forms() {
    // bash and POSIX accept `for x do` when there is no `in` list.
    let ast = common::ast(b"for f do :; done");
    assert!(matches!(
        common::find_node(&ast, |k| matches!(k, NodeKind::For { .. })),
        Some(NodeKind::For { in_list: None, .. })
    ));

    let ast = common::ast(b"for f in a b; do :; done");
    let Some(NodeKind::For { in_list, body, .. }) =
        common::find_node(&ast, |k| matches!(k, NodeKind::For { .. }))
    else {
        panic!("no for");
    };
    assert_eq!(in_list.as_ref().unwrap().len(), 2);
    assert!(matches!(ast.node(*body).kind, NodeKind::List { .. }));

    // No `in`: iterate over "$@".
    let ast = common::ast(b"for f; do :; done");
    assert!(matches!(
        common::find_node(&ast, |k| matches!(k, NodeKind::For { .. })),
        Some(NodeKind::For { in_list: None, .. })
    ));

    // Empty `in` list: zero iterations.
    let ast = common::ast(b"for f in; do :; done");
    assert!(matches!(
        common::find_node(&ast, |k| matches!(k, NodeKind::For { .. })),
        Some(NodeKind::For { in_list: Some(v), .. }) if v.is_empty()
    ));

    let ast = common::ast(b"for ((i = 0; i < 3; i++)); do :; done");
    assert!(matches!(
        common::find_node(&ast, |k| matches!(k, NodeKind::ForArith { .. })),
        Some(NodeKind::ForArith { .. })
    ));
}

#[test]
fn while_and_until() {
    let ast = common::ast(b"while a; do b; done");
    assert!(matches!(
        common::find_node(&ast, |k| matches!(k, NodeKind::While { .. })),
        Some(NodeKind::While { until: false, .. })
    ));
    let ast = common::ast(b"until a; do b; done");
    assert!(matches!(
        common::find_node(&ast, |k| matches!(k, NodeKind::While { .. })),
        Some(NodeKind::While { until: true, .. })
    ));
}

#[test]
fn case_patterns_and_terminators() {
    let ast = common::ast(b"case $x in (a|b) y;; c) z;& d) w;;& esac");
    let Some(NodeKind::Case { arms, .. }) =
        common::find_node(&ast, |k| matches!(k, NodeKind::Case { .. }))
    else {
        panic!("no case");
    };
    assert_eq!(arms.len(), 3);
    assert_eq!(arms[0].patterns.len(), 2);
    assert_eq!(arms[0].term, CaseTerm::Break);
    assert_eq!(arms[1].term, CaseTerm::FallThrough);
    assert_eq!(arms[2].term, CaseTerm::Continue);
}

#[test]
fn case_patterns_with_extglob_groups() {
    // `|` inside the group belongs to the pattern, not to the case
    // alternative list; the pattern still ends at the `)` after the group.
    let ast = common::ast(b"case x in @(a|b)|c) :;; esac");
    let Some(NodeKind::Case { arms, .. }) =
        common::find_node(&ast, |k| matches!(k, NodeKind::Case { .. }))
    else {
        panic!("no case");
    };
    assert_eq!(arms.len(), 1);
    assert_eq!(arms[0].patterns.len(), 2);
    let src: &[u8] = b"case x in @(a|b)|c) :;; esac";
    assert_eq!(&src[arms[0].patterns[0].span.clone()], b"@(a|b)");
    assert_eq!(&src[arms[0].patterns[1].span.clone()], b"c");

    // A single group is a single pattern, and the second `)` is its
    // terminator.
    let ast = common::ast(b"case x in @(a|b)) :;; esac");
    let Some(NodeKind::Case { arms, .. }) =
        common::find_node(&ast, |k| matches!(k, NodeKind::Case { .. }))
    else {
        panic!("no case");
    };
    assert_eq!(arms.len(), 1);
    assert_eq!(arms[0].patterns.len(), 1);
    assert_eq!(arms[0].patterns[0].span, 10..16);

    // Quoting inside the group is preserved: the matcher needs the raw bytes
    // and their quoting, not a dequoted literal.
    let src: &[u8] = b"case \"a b\" in @(\"a b\"|c)) :;; esac";
    let ast = common::ast(src);
    let Some(NodeKind::Case { arms, .. }) =
        common::find_node(&ast, |k| matches!(k, NodeKind::Case { .. }))
    else {
        panic!("no case");
    };
    assert_eq!(&src[arms[0].patterns[0].span.clone()], b"@(\"a b\"|c)");

    // `!(…)` is a pattern word, not the conditional's `!`.
    let ast = common::ast(b"case x in !(a)) :;; esac");
    let Some(NodeKind::Case { arms, .. }) =
        common::find_node(&ast, |k| matches!(k, NodeKind::Case { .. }))
    else {
        panic!("no case");
    };
    assert_eq!(arms[0].patterns.len(), 1);
    assert_eq!(arms[0].patterns[0].span, 10..14);

    // An unclosed group falls back to ordinary bytes, so the pattern is the
    // literal word and the missing `)` is the only problem.
    let src = b"echo @(a";
    let ast = common::ast(src);
    assert_eq!(
        cmd_words(&ast, src),
        vec![b"echo".to_vec(), b"@(a".to_vec()]
    );
}

#[test]
fn extglob_groups_parse_in_every_word_position() {
    // Command argument.
    let src = b"echo @(a|b)";
    let ast = common::ast(src);
    assert_eq!(
        cmd_words(&ast, src),
        vec![b"echo".to_vec(), b"@(a|b)".to_vec()]
    );

    // Assignment value.
    let src = b"g=--@(help|verbose)";
    let ast = common::ast(src);
    let cmd = first_simple(&ast);
    assert_eq!(cmd.assignments.len(), 1);
    assert_eq!(
        &src[cmd.assignments[0].value.span.clone()],
        b"--@(help|verbose)"
    );

    // Redirect target, fused to the operator and separate from it.
    for (src, want) in [
        (&b"cmd >@(out|err).txt"[..], &b"@(out|err).txt"[..]),
        (&b"cmd > @(out|err).txt"[..], &b"@(out|err).txt"[..]),
    ] {
        let ast = common::ast(src);
        let cmd = first_simple(&ast);
        assert_eq!(
            &src[cmd.redirects[0].target.as_ref().unwrap().span.clone()],
            want
        );
    }

    // Array literal element.
    let src = b"A=(zzz @(a|b).py)";
    let ast = common::ast(src);
    let cmd = first_simple(&ast);
    let array = cmd.assignments[0].array.as_ref().expect("array literal");
    assert_eq!(array.len(), 2);
    assert_eq!(&src[array[1].value.span.clone()], b"@(a|b).py");

    // `for … in` list.
    let src = b"for f in @(a|b) c; do :; done";
    let ast = common::ast(src);
    let Some(NodeKind::For {
        in_list: Some(list),
        ..
    }) = common::find_node(&ast, |k| matches!(k, NodeKind::For { .. }))
    else {
        panic!("no for list");
    };
    assert_eq!(list.len(), 2);
    assert_eq!(&src[list[0].span.clone()], b"@(a|b)");

    // Function body.
    let src = b"f() { echo @(a|b); }";
    let ast = common::ast(src);
    let inner = ast
        .nodes
        .iter()
        .find_map(|node| match &node.kind {
            NodeKind::Simple(cmd) if !cmd.words.is_empty() => Some(cmd),
            _ => None,
        })
        .expect("simple command in function body");
    assert_eq!(&src[inner.words[1].span.clone()], b"@(a|b)");
}

#[test]
fn brace_group_and_subshell() {
    let ast = common::ast(b"{ a; }");
    assert!(matches!(
        common::find_node(&ast, |k| matches!(k, NodeKind::BraceGroup { .. })),
        Some(NodeKind::BraceGroup { .. })
    ));
    let ast = common::ast(b"( a )");
    assert!(matches!(
        common::find_node(&ast, |k| matches!(k, NodeKind::Subshell { .. })),
        Some(NodeKind::Subshell { .. })
    ));
}

#[test]
fn function_definitions_both_forms() {
    for src in [
        &b"f() { :; }"[..],
        &b"function f { :; }"[..],
        &b"function f() { :; }"[..],
    ] {
        let ast = common::ast(src);
        let Some(NodeKind::Function {
            name,
            body,
            keyword,
        }) = common::find_node(&ast, |k| matches!(k, NodeKind::Function { .. }))
        else {
            panic!("no function in {:?}", String::from_utf8_lossy(src));
        };
        assert_eq!(&src[name.span.clone()], b"f");
        assert!(matches!(ast.node(*body).kind, NodeKind::BraceGroup { .. }));
        assert_eq!(
            *keyword,
            src.starts_with(b"function"),
            "keyword flag for {:?}",
            String::from_utf8_lossy(src)
        );
    }
}

#[test]
fn compound_commands_can_have_redirects() {
    let ast = common::ast(b"if a; then b; fi >out 2>&1");
    let Some(NodeKind::Redirected { redirects, .. }) =
        common::find_node(&ast, |k| matches!(k, NodeKind::Redirected { .. }))
    else {
        panic!("no redirected compound command");
    };
    assert_eq!(redirects.len(), 2);
}

#[test]
fn heredoc_attaches_to_the_right_redirect() {
    let src = b"cat <<A <<B\none\nA\ntwo\nB\n";
    let ast = common::ast(src);
    let cmd = first_simple(&ast);
    assert_eq!(cmd.redirects.len(), 2);
    let h0 = &ast.heredocs[cmd.redirects[0].heredoc.unwrap()];
    let h1 = &ast.heredocs[cmd.redirects[1].heredoc.unwrap()];
    assert_eq!(h0.delimiter_text, b"A");
    assert_eq!(&src[h0.body.clone()], b"one\n");
    assert_eq!(h1.delimiter_text, b"B");
    assert_eq!(&src[h1.body.clone()], b"two\n");
    assert_eq!(cmd.redirects[0].op, RedirectOp::HereDoc);
}

#[test]
fn here_doc_body_is_not_parsed() {
    let src = b"cat <<EOF\nif then ;; ) ( $(x\nEOF\necho done\n";
    let ast = common::ast(src);
    // Exactly two simple commands: `cat` and `echo done`.
    let simples = ast
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Simple(_)))
        .count();
    assert_eq!(simples, 2);
    let mut ifs = 0;
    for n in &ast.nodes {
        if matches!(n.kind, NodeKind::If { .. }) {
            ifs += 1;
        }
    }
    assert_eq!(ifs, 0);
}

#[test]
fn arithmetic_command_and_test_command() {
    let src = b"(( x = 1 + 2 ))";
    let ast = common::ast(src);
    let Some(NodeKind::ArithCommand { expr }) =
        common::find_node(&ast, |k| matches!(k, NodeKind::ArithCommand { .. }))
    else {
        panic!("no arithmetic command");
    };
    assert_eq!(&src[expr.clone()], b" x = 1 + 2 ");

    let src = b"[[ -f x && ( -d y ) ]]";
    let ast = common::ast(src);
    let Some(NodeKind::TestCommand { body }) =
        common::find_node(&ast, |k| matches!(k, NodeKind::TestCommand { .. }))
    else {
        panic!("no test command");
    };
    assert_eq!(&src[body.clone()], b" -f x && ( -d y ) ");
}

#[test]
fn word_parts_carry_quotedness_via_doubleq() {
    // Quotedness belongs to the part; a double-quoted run is a DoubleQ group.
    let src = b"echo \"a${x:-b}c\"'d'";
    let ast = common::ast(src);
    let cmd = first_simple(&ast);
    let word = &cmd.words[1];
    assert_eq!(word.parts.len(), 2, "parts: {:?}", word.parts);

    let Part::DoubleQ(DoubleQ { parts }) = &word.parts[0] else {
        panic!("expected a double-quoted group: {:?}", word.parts[0]);
    };
    assert_eq!(parts.len(), 3);
    assert!(matches!(
        &parts[0],
        Part::Lit(Lit { kind: LitKind::Unquoted, bytes }) if bytes == b"a"
    ));
    let Part::Param(p) = &parts[1] else {
        panic!("expected a parameter expansion: {:?}", parts[1]);
    };
    assert_eq!(p.name, ParamName::Var(b"x".to_vec()));
    let ParamOp::UseDefault {
        word: replacement,
        colon,
    } = &p.op
    else {
        panic!("expected :-: {:?}", p.op);
    };
    assert!(*colon);
    assert!(matches!(
        &replacement.parts[0],
        Part::Lit(Lit { kind: LitKind::Unquoted, bytes }) if bytes == b"b"
    ));
    // ParamExp.span is the whole source construct.
    assert_eq!(&src[p.span.clone()], b"${x:-b}");

    assert!(matches!(
        &word.parts[1],
        Part::Lit(Lit { kind: LitKind::SingleQuoted, bytes }) if bytes == b"d"
    ));
}

#[test]
fn escaped_bytes_are_quoted_literals() {
    let src = b"echo a\\ b \"c\\\"d\"";
    let ast = common::ast(src);
    let cmd = first_simple(&ast);
    let plain = &cmd.words[1];
    assert_eq!(plain.parts.len(), 3, "parts: {:?}", plain.parts);
    assert!(matches!(
        &plain.parts[0],
        Part::Lit(Lit { kind: LitKind::Unquoted, bytes }) if bytes == b"a"
    ));
    assert!(matches!(
        &plain.parts[1],
        Part::Lit(Lit { kind: LitKind::Quoted, bytes }) if bytes == b" "
    ));
    assert!(matches!(
        &plain.parts[2],
        Part::Lit(Lit { kind: LitKind::Unquoted, bytes }) if bytes == b"b"
    ));

    let Part::DoubleQ(DoubleQ { parts }) = &cmd.words[2].parts[0] else {
        panic!("expected a double-quoted group");
    };
    assert!(matches!(
        &parts[1],
        Part::Lit(Lit { kind: LitKind::Quoted, bytes }) if bytes == b"\""
    ));
}

#[test]
fn ansi_c_quoting_is_decoded_here() {
    let src = br"echo $'a\tb\x41\u00e9\\'";
    let ast = common::ast(src);
    let cmd = first_simple(&ast);
    let Part::Lit(lit) = &cmd.words[1].parts[0] else {
        panic!("expected a literal: {:?}", cmd.words[1].parts[0]);
    };
    assert_eq!(lit.kind, LitKind::AnsiC);
    assert_eq!(lit.bytes, b"a\tbA\xc3\xa9\\".to_vec());
}

#[test]
fn tilde_prefixes_are_marked_only_where_legal() {
    for (src, user, rest) in [
        (&b"echo ~"[..], &b""[..], &b""[..]),
        (&b"echo ~/x"[..], &b""[..], &b"/x"[..]),
        (&b"echo ~bob/x"[..], &b"bob"[..], &b"/x"[..]),
    ] {
        let ast = common::ast(src);
        let cmd = first_simple(&ast);
        let word = &cmd.words[1];
        let Part::Tilde(spec) = &word.parts[0] else {
            panic!(
                "expected a tilde part in {:?}: {:?}",
                String::from_utf8_lossy(src),
                word.parts
            );
        };
        assert_eq!(spec.user, user);
        if !rest.is_empty() {
            assert!(matches!(
                &word.parts[1],
                Part::Lit(Lit { bytes, .. }) if bytes == rest
            ));
        }
    }
    // Not a tilde prefix.
    for src in [&b"echo a~b"[..], &b"echo \"~\"x"[..], &b"echo ~$x"[..]] {
        let ast = common::ast(src);
        let cmd = first_simple(&ast);
        assert!(
            !cmd.words[1]
                .parts
                .iter()
                .any(|p| matches!(p, Part::Tilde(_))),
            "unexpected tilde in {:?}",
            String::from_utf8_lossy(src)
        );
    }
}

#[test]
fn parameter_name_mapping_is_exact() {
    let cases: &[(&[u8], ParamName)] = &[
        (b"echo $0", ParamName::Var(b"0".to_vec())),
        (b"echo $1", ParamName::Positional(1)),
        (b"echo $9", ParamName::Positional(9)),
        (b"echo $#", ParamName::Count),
        (b"echo $?", ParamName::Var(b"?".to_vec())),
        (b"echo $-", ParamName::Var(b"-".to_vec())),
        (b"echo $$", ParamName::Var(b"$".to_vec())),
        (b"echo $!", ParamName::Var(b"!".to_vec())),
        (b"echo $*", ParamName::Star),
        (b"echo $@", ParamName::At),
        (b"echo ${10}", ParamName::Positional(10)),
        (b"echo ${#x}", ParamName::Var(b"x".to_vec())),
        (b"echo ${!x}", ParamName::Var(b"x".to_vec())),
        (b"echo ${x#p}", ParamName::Var(b"x".to_vec())),
    ];
    for (src, want) in cases {
        let ast = common::ast(src);
        let cmd = first_simple(&ast);
        let part = &cmd.words[1].parts[0];
        let Part::Param(p) = part else {
            panic!(
                "expected a parameter for {:?}: {part:?}",
                String::from_utf8_lossy(src)
            );
        };
        assert_eq!(p.name, *want, "for {:?}", String::from_utf8_lossy(src));
    }
    // `$10` is `$1` followed by the literal `0` (POSIX single-digit rule).
    let ast = common::ast(b"echo $10");
    let cmd = first_simple(&ast);
    assert_eq!(cmd.words[1].parts.len(), 2);
    assert!(matches!(
        &cmd.words[1].parts[0],
        Part::Param(p) if p.name == ParamName::Positional(1)
    ));
}

#[test]
fn pattern_operators_keep_their_quoting() {
    // The quoting written inside a pattern has to survive into the `Word`, or
    // `${x#"$y"}` would silently match `$y` as a pattern instead of comparing
    // it literally. The expansion side renders quoted bytes as backslash
    // escapes, so this is where that information is preserved or lost.
    let ast = common::ast(br#"echo ${x#"$y"} ${x#$y} ${x#'*'} ${x#\*}"#);
    let cmd = first_simple(&ast);
    let patterns: Vec<(&Vec<Part>, bool)> = cmd.words[1..]
        .iter()
        .map(|word| {
            let Part::Param(p) = &word.parts[0] else {
                panic!("expected a parameter, got {:?}", word.parts);
            };
            let ParamOp::RemovePrefix { pattern, longest } = &p.op else {
                panic!("expected a prefix strip, got {:?}", p.op);
            };
            assert!(!longest);
            (&pattern.parts, false)
        })
        .collect();
    // `"$y"`: one double-quoted group containing the parameter.
    assert!(matches!(
        &patterns[0].0[0],
        Part::DoubleQ(DoubleQ { parts })
            if matches!(&parts[0], Part::Param(e) if e.name == ParamName::var(b"y"))
    ));
    // `$y`: a bare parameter part, which stays an active pattern.
    assert!(matches!(
        &patterns[1].0[0],
        Part::Param(e) if e.name == ParamName::var(b"y")
    ));
    // `'*'`: a single-quoted literal.
    assert!(matches!(
        &patterns[2].0[0],
        Part::Lit(Lit { kind: LitKind::SingleQuoted, bytes }) if bytes == b"*"
    ));
    // `\*`: a backslash-escaped byte.
    assert!(matches!(
        &patterns[3].0[0],
        Part::Lit(Lit { kind: LitKind::Quoted, bytes }) if bytes == b"*"
    ));
}

#[test]
fn replacement_splits_at_the_first_unquoted_slash() {
    // `${x////c}` replaces `/` with `c`; `${x//'/'/c}` does the same with a
    // quoted pattern. A naive scan for the first `/` gets both wrong.
    let ast = common::ast(br#"echo ${x////c} ${x//'/'/c} ${x/#/p} ${x/$p/r}"#);
    let cmd = first_simple(&ast);
    let cases: &[(&[u8], &[u8], ReplaceMode)] = &[
        (b"/", b"c", ReplaceMode::All),
        (b"/", b"c", ReplaceMode::All),
        (b"", b"p", ReplaceMode::Prefix),
    ];
    for (index, (pattern, replacement, mode)) in cases.iter().enumerate() {
        let Part::Param(p) = &cmd.words[index + 1].parts[0] else {
            panic!("expected a parameter");
        };
        let ParamOp::Replace {
            pattern: got_pattern,
            replacement: got_replacement,
            mode: got_mode,
        } = &p.op
        else {
            panic!("expected a replacement, got {:?}", p.op);
        };
        assert_eq!(got_mode, mode, "mode for word {index}");
        assert_eq!(
            word_bytes(got_pattern),
            *pattern,
            "pattern for word {index}"
        );
        assert_eq!(
            word_bytes(got_replacement),
            *replacement,
            "replacement for word {index}"
        );
    }
}

/// The literal text of a word, ignoring quoting (a test convenience).
fn word_bytes(word: &Word) -> Vec<u8> {
    let mut out = Vec::new();
    for part in &word.parts {
        match part {
            Part::Lit(lit) => out.extend_from_slice(&lit.bytes),
            Part::Param(_) => out.push(b'$'),
            other => panic!("unexpected part in a test pattern: {other:?}"),
        }
    }
    out
}

#[test]
fn parameter_operators_map_to_bsh_lang() {
    let cases: &[(&[u8], ParamOp)] = &[
        (b"echo ${x}", ParamOp::Value),
        (b"echo ${#x}", ParamOp::Length),
        (
            b"echo ${x:1:2}",
            ParamOp::Substring {
                offset: Word::lit("1").with_span(9..10),
                length: Some(Word::lit("2").with_span(11..12)),
            },
        ),
        (
            b"echo ${x:1}",
            ParamOp::Substring {
                offset: Word::lit("1").with_span(9..10),
                length: None,
            },
        ),
        (
            b"echo ${x: -2}",
            ParamOp::Substring {
                offset: Word::lit(" -2").with_span(9..12),
                length: None,
            },
        ),
        // A bound is a word, so arithmetic in it is evaluated by expansion
        // rather than being rejected here.
        (
            b"echo ${x:1+2}",
            ParamOp::Substring {
                offset: Word::lit("1+2").with_span(9..12),
                length: None,
            },
        ),
        (
            b"echo ${x^^}",
            ParamOp::Case {
                how: CaseHow::Upper,
                pattern: None,
            },
        ),
        (
            b"echo ${x,,}",
            ParamOp::Case {
                how: CaseHow::Lower,
                pattern: None,
            },
        ),
        (
            b"echo ${x^}",
            ParamOp::Case {
                how: CaseHow::UpperFirst,
                pattern: None,
            },
        ),
        (
            b"echo ${x,}",
            ParamOp::Case {
                how: CaseHow::LowerFirst,
                pattern: None,
            },
        ),
        (
            b"echo ${x#[a-c]}",
            ParamOp::RemovePrefix {
                pattern: Word::lit(*b"[a-c]").with_span(9..14),
                longest: false,
            },
        ),
        (
            b"echo ${x##p}",
            ParamOp::RemovePrefix {
                pattern: Word::lit(*b"p").with_span(10..11),
                longest: true,
            },
        ),
        (
            b"echo ${x%p}",
            ParamOp::RemoveSuffix {
                pattern: Word::lit(*b"p").with_span(9..10),
                longest: false,
            },
        ),
        (
            b"echo ${x%%p}",
            ParamOp::RemoveSuffix {
                pattern: Word::lit(*b"p").with_span(10..11),
                longest: true,
            },
        ),
        (
            b"echo ${x/p/r}",
            ParamOp::Replace {
                // Both halves carry the body's span; the parts inside keep
                // their own, which is what diagnostics point at.
                pattern: Word::lit(*b"p").with_span(9..12),
                replacement: Word::lit(*b"r").with_span(9..12),
                mode: ReplaceMode::First,
            },
        ),
        (
            b"echo ${x//p/r}",
            ParamOp::Replace {
                pattern: Word::lit(*b"p").with_span(10..13),
                replacement: Word::lit(*b"r").with_span(10..13),
                mode: ReplaceMode::All,
            },
        ),
        (
            b"echo ${x/#p/r}",
            ParamOp::Replace {
                pattern: Word::lit(*b"p").with_span(10..13),
                replacement: Word::lit(*b"r").with_span(10..13),
                mode: ReplaceMode::Prefix,
            },
        ),
        (
            b"echo ${x/%p/r}",
            ParamOp::Replace {
                pattern: Word::lit(*b"p").with_span(10..13),
                replacement: Word::lit(*b"r").with_span(10..13),
                mode: ReplaceMode::Suffix,
            },
        ),
        (b"echo ${x@P}", ParamOp::Transform(Transform::Prompt)),
        (b"echo ${x@Q}", ParamOp::Transform(Transform::Quote)),
        (b"echo ${x@U}", ParamOp::Transform(Transform::Upper)),
        (b"echo ${x@u}", ParamOp::Transform(Transform::UpperFirst)),
        (b"echo ${x@L}", ParamOp::Transform(Transform::Lower)),
        // A letter this shell does not implement stays an unsupported
        // operator: the parser saw it and declined it, rather than losing it.
        (
            b"echo ${x@E}",
            ParamOp::Unsupported(UnsupportedOp::Transform),
        ),
    ];
    for (src, want) in cases {
        let ast = common::ast(src);
        let cmd = first_simple(&ast);
        let part = &cmd.words[1].parts[0];
        let Part::Param(p) = part else {
            panic!(
                "expected a parameter for {:?}: {part:?}",
                String::from_utf8_lossy(src)
            );
        };
        assert_eq!(p.op, *want, "for {:?}", String::from_utf8_lossy(src));
    }

    for (src, colon) in [(&b"echo ${x:-y}"[..], true), (&b"echo ${x-y}"[..], false)] {
        let ast = common::ast(src);
        let cmd = first_simple(&ast);
        let Part::Param(p) = &cmd.words[1].parts[0] else {
            panic!("expected a parameter");
        };
        let ParamOp::UseDefault { word, colon: got } = &p.op else {
            panic!("expected :-: {:?}", p.op);
        };
        assert_eq!(*got, colon);
        assert_eq!(word.parts.len(), 1);
        assert!(matches!(
            &word.parts[0],
            Part::Lit(Lit { kind: LitKind::Unquoted, bytes }) if bytes == b"y"
        ));
        assert_eq!(&src[word.span.clone()], b"y");
    }
}

#[test]
fn array_subscripts_map_to_the_word_model() {
    let ast = common::ast(b"echo ${x[1]} ${#x[@]} ${!x[@]} ${x[*]}");
    let cmd = first_simple(&ast);
    let parts: Vec<&Part> = cmd.words[1..]
        .iter()
        .flat_map(|word| word.parts.iter())
        .collect();
    assert!(
        matches!(
            &parts[0],
            Part::Param(ParamExp {
                name: ParamName::Element { name, subscript },
                op: ParamOp::Value,
                ..
            }) if name == b"x" && !subscript.parts.is_empty()
        ),
        "{:?}",
        parts[0]
    );
    assert!(matches!(
        &parts[1],
        Part::Param(ParamExp {
            name: ParamName::Elements { name, .. },
            op: ParamOp::Length,
            ..
        }) if name == b"x"
    ));
    assert!(matches!(
        &parts[2],
        Part::Param(ParamExp {
            name: ParamName::Elements { star: false, .. },
            op: ParamOp::Indices,
            ..
        })
    ));
    assert!(matches!(
        &parts[3],
        Part::Param(ParamExp {
            name: ParamName::Elements { star: true, .. },
            op: ParamOp::Value,
            ..
        })
    ));
}

#[test]
fn command_substitutions_carry_source_and_are_validated() {
    let src = b"x=$(echo hi)";
    let ast = common::ast(src);
    let cmd = first_simple(&ast);
    let value = &cmd.assignments[0].value;
    let Part::CmdSub(sub) = &value.parts[0] else {
        panic!("expected a command substitution: {:?}", value.parts[0]);
    };
    assert_eq!(sub.style, CmdSubStyle::DollarParen);
    assert_eq!(sub.source, b"echo hi");
    assert_eq!(&src[sub.span.clone()], b"$(echo hi)");

    // The inner source is parsed for diagnostics even though bsh-lang owns it.
    let ds = common::fail(b"x=$(if true; fi)");
    assert!(
        ds.iter().any(|d| d.message.contains("`then`")),
        "inner parse errors must surface: {:?}",
        ds.iter().map(|d| d.message.clone()).collect::<Vec<_>>()
    );
}

#[test]
fn case_pattern_paren_inside_a_substitution() {
    // DESIGN.md §3.3: the `)` closing a case pattern must not be taken for the
    // `)` closing `$( … )`. The scanner tracks just enough case state.
    let src = b"x=$(case $y in a) echo hi;; esac)";
    let ast = common::ast(src);
    let cmd = first_simple(&ast);
    let Part::CmdSub(sub) = &cmd.assignments[0].value.parts[0] else {
        panic!("expected a command substitution");
    };
    assert_eq!(sub.source, b"case $y in a) echo hi;; esac");
    // The trailing `)` after `esac` is the real closing paren.
    assert_eq!(&src[sub.span.clone()], b"$(case $y in a) echo hi;; esac)");
}

#[test]
fn backtick_source_is_unescaped() {
    let src = b"x=`echo \\`hi\\``";
    let ast = common::ast(src);
    let cmd = first_simple(&ast);
    let Part::CmdSub(sub) = &cmd.assignments[0].value.parts[0] else {
        panic!("expected a command substitution");
    };
    assert_eq!(sub.style, CmdSubStyle::Backtick);
    assert_eq!(sub.source, b"echo `hi`");
}

#[test]
fn process_substitution_and_arith_expansion_parts() {
    let src = b"diff <(a) $((1 + 2))";
    let ast = common::ast(src);
    let cmd = first_simple(&ast);
    let Part::ProcSub(ps) = &cmd.words[1].parts[0] else {
        panic!(
            "expected a process substitution: {:?}",
            cmd.words[1].parts[0]
        );
    };
    assert_eq!(ps.dir, bsh_syntax::ProcSubDir::Input);
    assert_eq!(ps.source, b"a");
    let Part::Arith(ar) = &cmd.words[2].parts[0] else {
        panic!(
            "expected an arithmetic expansion: {:?}",
            cmd.words[2].parts[0]
        );
    };
    assert_eq!(ar.source, b"1 + 2");
}

#[test]
fn assignment_forms() {
    let src = b"x= y+=1 z[2]=v a=(b \"c d\")";
    let ast = common::ast(src);
    let cmd = first_simple(&ast);
    assert_eq!(cmd.assignments.len(), 4);
    assert!(cmd.assignments[1].append);
    assert!(cmd.assignments[2].index.is_some());
    assert_eq!(cmd.assignments[3].array.as_ref().unwrap().len(), 2);
    assert!(cmd.words.is_empty());
}

#[test]
fn comments_and_continuations() {
    let ast = common::ast(b"echo a \\\n b # trailing\n# whole line\necho c");
    let simples = ast
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Simple(_)))
        .count();
    assert_eq!(simples, 2);
    assert_eq!(
        cmd_words(
            &common::ast(b"echo a \\\n b # trailing\n# whole line\necho c"),
            b"echo a \\\n b # trailing\n# whole line\necho c"
        ),
        vec![b"echo".to_vec(), b"a".to_vec(), b"b".to_vec()]
    );
}

#[test]
fn trailing_operator_inside_and_or_allows_newlines() {
    let ast = common::ast(b"a &&\n  b");
    assert!(matches!(
        common::find_node(&ast, |k| matches!(k, NodeKind::AndOr { .. })),
        Some(NodeKind::AndOr { rest, .. }) if rest.len() == 1
    ));
}

// ------------------------------------------------------------------ incomplete

#[test]
fn need_more_input_for_every_open_construct() {
    let cases: &[(&[u8], usize)] = &[
        (b"echo \"abc", 5),
        (b"echo 'abc", 5),
        (b"echo $(abc", 5),
        (b"echo ${abc", 5),
        (b"echo `abc", 5),
        (b"( echo hi", 0),
        (b"{ echo hi", 0),
        (b"if true", 0),
        (b"if true; then echo hi", 0),
        (b"while true; do", 0),
        (b"for f in a; do echo", 0),
        (b"case x in", 0),
        (b"case x in a) echo hi;;", 0),
        (b"cat <<EOF\nbody", 4),
        (b"echo hi >", 8),
        (b"echo hi |", 8),
        (b"echo hi &&", 8),
        (b"echo hi ||", 8),
    ];
    for (src, open) in cases {
        let (o, expected) = common::more(src);
        assert!(
            !expected.is_empty(),
            "no expectations for {:?}",
            String::from_utf8_lossy(src)
        );
        assert_eq!(
            o.start,
            *open,
            "open start for {:?} (got {:?})",
            String::from_utf8_lossy(src),
            o
        );
    }
}

#[test]
fn need_more_input_expectations_are_specific() {
    use bsh_syntax::Expect;
    let (_, expected) = common::more(b"if ");
    assert_eq!(expected.first(), Some(&Expect::Command));
    let (_, expected) = common::more(b"for f in ");
    assert_eq!(expected.first(), Some(&Expect::ForList));
    let (_, expected) = common::more(b"case x in ");
    assert_eq!(expected.first(), Some(&Expect::CasePattern));
    let (_, expected) = common::more(b"echo hi >");
    assert_eq!(expected.first(), Some(&Expect::RedirectTarget));
    let (_, expected) = common::more(b"echo ${x");
    assert_eq!(expected.first(), Some(&Expect::ParamOp));
    let (_, expected) = common::more(b"echo $(x");
    assert_eq!(expected.first(), Some(&Expect::Command));
    let (_, expected) = common::more(b"cat <<EOF\nbody");
    assert_eq!(expected.first(), Some(&Expect::HereDocDelimiter));
}

// ---------------------------------------------------------------- expectations

#[test]
fn expectations_at_the_required_positions() {
    use bsh_syntax::{ParseOpts, expectations_at};
    let opts = ParseOpts::DEFAULT;
    let at = |src: &[u8], off: usize| expectations_at(src, off, &opts);

    assert_eq!(
        at(b"for f in ", 9).first(),
        Some(&bsh_syntax::Expect::ForList)
    );
    assert_eq!(at(b"if ", 3).first(), Some(&bsh_syntax::Expect::Command));
    assert_eq!(
        at(b"echo ${", 7).first(),
        Some(&bsh_syntax::Expect::ParamOp)
    );
    assert_eq!(
        at(b">", 1).first(),
        Some(&bsh_syntax::Expect::RedirectTarget)
    );
    assert_eq!(at(b"echo ", 5).first(), Some(&bsh_syntax::Expect::Word));
    assert_eq!(at(b"a; ", 3).first(), Some(&bsh_syntax::Expect::Command));
    assert_eq!(
        at(b"echo hi | ", 10).first(),
        Some(&bsh_syntax::Expect::Command)
    );
    assert_eq!(
        at(b"case x in ", 10).first(),
        Some(&bsh_syntax::Expect::CasePattern)
    );
    assert_eq!(at(b"$(", 2).first(), Some(&bsh_syntax::Expect::Command));
    assert_eq!(
        at(b"while true; do ", 14).first(),
        Some(&bsh_syntax::Expect::Command)
    );
    // Clamped offsets are accepted.
    assert!(!at(b"echo", 99).is_empty());
}

// ---------------------------------------------------------------------- limits

#[test]
fn deep_nesting_yields_a_diagnostic_not_a_stack_overflow() {
    let mut src = Vec::new();
    for _ in 0..300 {
        src.extend_from_slice(b"( ");
    }
    src.extend_from_slice(b":");
    for _ in 0..300 {
        src.extend_from_slice(b" )");
    }
    let ds = common::fail(&src);
    assert!(
        ds.iter().any(|d| d.code == bsh_syntax::codes::DEPTH),
        "expected a depth diagnostic, got {:?}",
        ds.iter().map(|d| d.message.clone()).collect::<Vec<_>>()
    );

    // Deep substitutions hit the lexer's own guard and must not panic.
    let mut src = Vec::new();
    for _ in 0..200 {
        src.extend_from_slice(b"$(");
    }
    src.extend_from_slice(b"echo hi");
    for _ in 0..200 {
        src.extend_from_slice(b")");
    }
    let _ = common::parse(&src);
}

// ----------------------------------------------------------------------- posix

#[test]
fn posix_mode_rejects_bash_only_syntax_with_a_span() {
    let bashisms: &[&[u8]] = &[
        b"((x))",
        b"[[ -f x ]]",
        b"echo $'a'",
        b"echo <(true)",
        b"echo hi <<<x",
        b"echo |& cat",
        b"echo hi &> f",
        b"function f { :; }",
        b"for ((i=0;i<1;i++)); do :; done",
        b"case x in a) :;& esac",
        b"x=(a)",
        b"a[1]=v",
    ];
    for src in bashisms {
        let ds = match common::parse_posix(src) {
            bsh_syntax::ParseOutcome::Failed(ds) => ds,
            other => panic!(
                "expected {:?} to be rejected in POSIX mode, got {other:?}",
                String::from_utf8_lossy(src)
            ),
        };
        assert!(
            ds.iter().any(|d| d.code == bsh_syntax::codes::BASHISM),
            "no BASHISM diagnostic for {:?}: {:?}",
            String::from_utf8_lossy(src),
            ds.iter().map(|d| d.message.clone()).collect::<Vec<_>>()
        );
        assert!(ds.iter().all(|d| !d.span.is_empty()));
    }

    // POSIX constructs still parse in POSIX mode.
    for src in [
        &b"if a; then b; fi"[..],
        &b"for f in a; do :; done"[..],
        &b"case x in a) :;; esac"[..],
        &b"f() { :; }"[..],
        &b"{ a; }"[..],
        &b"( a )"[..],
    ] {
        assert!(
            matches!(common::parse_posix(src), bsh_syntax::ParseOutcome::Ok(_)),
            "{:?} should parse in POSIX mode",
            String::from_utf8_lossy(src)
        );
    }
}

#[test]
fn array_literal_elements_are_structured() {
    let src = b"a=(1 [5]='2 3' [k]+=v)";
    let ast = common::ast(src);
    let cmd = first_simple(&ast);
    let assignment = &cmd.assignments[0];
    assert_eq!(&src[assignment.name.clone()], b"a");
    let elements = assignment.array.as_ref().expect("array literal");
    assert_eq!(elements.len(), 3, "three elements");

    assert!(elements[0].index.is_none(), "bare element");
    assert_eq!(&src[elements[0].value.span.clone()], b"1");

    let index = elements[1].index.as_ref().expect("subscript");
    assert_eq!(&src[index.subscript.span.clone()], b"5");
    assert!(!index.append);
    assert_eq!(&src[elements[1].value.span.clone()], b"'2 3'");

    let index = elements[2].index.as_ref().expect("subscript");
    assert_eq!(&src[index.subscript.span.clone()], b"k");
    assert!(index.append, "[k]+=v appends");
    assert_eq!(&src[elements[2].value.span.clone()], b"v");
}

#[test]
fn append_array_assignment_and_spaced_subscript() {
    let src = b"a+=(x y)";
    let ast = common::ast(src);
    let cmd = first_simple(&ast);
    let assignment = &cmd.assignments[0];
    assert!(assignment.append);
    assert_eq!(assignment.array.as_ref().map(Vec::len), Some(2));

    // `a[1 + 2]=v`: the subscript is one word even though the lexer split it.
    let src = b"a[1 + 2]=v";
    let ast = common::ast(src);
    let cmd = first_simple(&ast);
    let index = cmd.assignments[0].index.as_ref().expect("subscript");
    assert_eq!(&src[index.span.clone()], b"1 + 2");
}

#[test]
fn declaration_array_arguments_carry_name_and_elements() {
    let src = b"local -a a=(1 '2 3')";
    let ast = common::ast(src);
    let cmd = first_simple(&ast);
    assert_eq!(cmd.array_args.len(), 1);
    let argument = &cmd.array_args[0];
    assert_eq!(&src[argument.name.clone()], b"a");
    assert!(!argument.append);
    assert_eq!(argument.elements.len(), 2);
    // The placeholder keeps ordinary word expansion inert.
    assert!(matches!(
        &cmd.words[argument.word].parts[0],
        Part::Lit(Lit {
            kind: LitKind::Quoted,
            ..
        })
    ));
}
