//! One test per parser-API gap, plus the word-level entry point.
//!
//! These are the "reality check" gaps in `DESIGN.md` §6.3: each test failed
//! against the API as it was when the completer landed, and passes now. They
//! are integration tests rather than unit tests because they are about the
//! published contract, not about one module's internals.

use bsh_syntax::{
    Expect, ParseOpts, ParseWordError, TokenKind, TokenRole, classify_tokens, expectation_sites_at,
    expectations_after, expectations_at, lex, lex_full, open_constructs_at, parse_word,
    parse_word_at, redirect_op_len,
};

const OPTS: ParseOpts = ParseOpts::DEFAULT;

fn at(src: &[u8], offset: usize) -> Vec<Expect> {
    expectations_at(src, offset, &OPTS)
}

fn first(src: &[u8], offset: usize) -> Expect {
    *at(src, offset).first().expect("at least one expectation")
}

// ---------------------------------------------------------------- gap 1

/// `expectations_at` answers for the cursor, not for the slot after the prefix
/// it parsed.
#[test]
fn expectations_answer_at_the_cursor() {
    // A half-typed command word is still in command position.
    assert_eq!(at(b"ec", 2), vec![Expect::Command]);
    assert_eq!(at(b"ec", 1), vec![Expect::Command]);
    // After the command word's separator the argument slot begins.
    assert_eq!(at(b"ls ", 3), vec![Expect::Word]);
    assert_eq!(at(b"for f in ", 9), vec![Expect::ForList]);
    assert_eq!(at(b"for f in sr", 11), vec![Expect::ForList]);
    // A cursor inside a longer word asks about that word's slot.
    assert_eq!(at(b"git checkout", 6), vec![Expect::Word]);
}

/// The pre-cursor-semantics answer is kept for callers that want it.
#[test]
fn expectations_after_is_the_legacy_answer() {
    assert_eq!(expectations_after(b"ec", 2, &OPTS), vec![Expect::Word]);
    assert_eq!(
        expectations_after(b"echo > fi", 9, &OPTS),
        vec![Expect::Word]
    );
    assert_eq!(at(b"ec", 2), vec![Expect::Command]);
}

// ---------------------------------------------------------------- gap 2

/// `[[ … ]]` bodies are no longer opaque: the operator slot is reported, with
/// the operator's position.
#[test]
fn test_operator_expectations_come_from_the_parser() {
    assert_eq!(at(b"[[ -", 4), vec![Expect::TestOp]);
    assert_eq!(at(b"[[ x == ", 9), vec![Expect::TestOp]);

    let partial = expectation_sites_at(b"[[ -", 4, &OPTS);
    assert_eq!(partial[0].expect, Expect::TestOp);
    assert_eq!(partial[0].span, 3..4);

    let complete = expectation_sites_at(b"[[ x == ", 9, &OPTS);
    assert_eq!(complete[0].expect, Expect::TestOp);
    assert_eq!(complete[0].span, 5..7, "the operator just typed");

    // A closed test is not a test context.
    assert_eq!(at(b"[[ -f x ]] ", 12), vec![Expect::Command]);
}

// ---------------------------------------------------------------- gap 3

/// A redirection target is a target whether or not it has been typed.
#[test]
fn redirect_targets_are_reported_when_present() {
    assert_eq!(at(b"echo > fi", 9), vec![Expect::RedirectTarget]);
    assert_eq!(at(b"echo > ", 7), vec![Expect::RedirectTarget]);
    assert_eq!(at(b"echo >fi", 8), vec![Expect::RedirectTarget]);
    // A target already written means the next word is an argument again.
    assert_eq!(at(b"echo > foo bar", 14), vec![Expect::Word]);
    assert_eq!(
        expectation_sites_at(b"echo > fi", 9, &OPTS)[0].span,
        7..9,
        "the target word's span"
    );
}

// ---------------------------------------------------------------- gap 4

/// The lexer exposes where a redirection's operator ends, so no consumer keeps
/// its own operator table.
#[test]
fn redirect_operator_and_target_split() {
    let src = b"cat >out 2>&1 &>>log <<-EOF <<<x";
    let redirects: Vec<_> = lex(src)
        .into_iter()
        .filter(|token| token.kind == TokenKind::Redirect)
        .collect();
    let texts: Vec<&[u8]> = redirects.iter().map(|token| token.text(src)).collect();
    assert_eq!(
        texts,
        vec![&b">out"[..], b"2>&1", b"&>>log", b"<<-EOF", b"<<<x"]
    );
    let lengths: Vec<Option<usize>> = redirects
        .iter()
        .map(|token| token.redirect_op_len(src))
        .collect();
    assert_eq!(lengths, vec![Some(1), Some(3), Some(3), Some(3), Some(3)]);
    let targets: Vec<Option<&[u8]>> = redirects
        .iter()
        .map(|token| token.redirect_target(src).map(|span| &src[span]))
        .collect();
    assert_eq!(
        targets,
        vec![
            Some(&b"out"[..]),
            Some(b"1"),
            Some(b"log"),
            Some(b"EOF"),
            Some(b"x")
        ]
    );

    // A target written separately is not inside the token.
    let separate = lex(b"cat > out");
    let redirect = separate
        .iter()
        .find(|token| token.kind == TokenKind::Redirect)
        .expect("a redirect token");
    assert_eq!(redirect.redirect_target(b"cat > out"), None);
    assert_eq!(redirect.redirect_op_len(b"cat > out"), Some(1));

    // The free function is the one definition of the operator set.
    assert_eq!(redirect_op_len(b">>"), Some(2));
    assert_eq!(redirect_op_len(b"2>&1"), Some(3));
    assert_eq!(redirect_op_len(b"fi"), None);
}

// ---------------------------------------------------------------- gap 5

/// The lexer keeps a nesting stack, not one flat "something is open" note.
#[test]
fn open_constructs_are_a_chain() {
    let chain = open_constructs_at(b"echo \"$(ls ", 11);
    let kinds: Vec<_> = chain.iter().map(|open| open.kind).collect();
    assert_eq!(
        kinds,
        vec![
            bsh_syntax::UnterminatedKind::CmdSub,
            bsh_syntax::UnterminatedKind::DoubleQuote
        ],
        "innermost first: a `$(` inside a quoted word"
    );
    assert_eq!(chain[0].content_start, 8);
    assert_eq!(chain[1].content_start, 6);
    assert_eq!(chain[0].opener, 6..8);
    assert_eq!(chain[1].opener, 5..6);

    // Three levels, through a backtick body that never closed.
    let deep = open_constructs_at(b"echo `echo \"$(x", 15);
    let kinds: Vec<_> = deep.iter().map(|open| open.kind).collect();
    assert_eq!(
        kinds,
        vec![
            bsh_syntax::UnterminatedKind::CmdSub,
            bsh_syntax::UnterminatedKind::DoubleQuote,
            bsh_syntax::UnterminatedKind::Backtick,
        ]
    );

    // `${` inside `"…"`.
    let param = open_constructs_at(b"echo \"${x", 9);
    let kinds: Vec<_> = param.iter().map(|open| open.kind).collect();
    assert_eq!(
        kinds,
        vec![
            bsh_syntax::UnterminatedKind::ParamExp,
            bsh_syntax::UnterminatedKind::DoubleQuote,
        ]
    );

    // A complete line has none, and `Lexed` carries the same chain.
    assert!(open_constructs_at(b"echo hi", 7).is_empty());
    assert_eq!(
        lex_full(b"echo \"$(ls ").open,
        open_constructs_at(b"echo \"$(ls ", 11)
    );

    // The innermost construct (and the region it opens) is what the
    // expectation answers for.
    assert_eq!(at(b"echo \"$(l", 9), vec![Expect::Command]);
    assert_eq!(at(b"echo \"$(ls ", 11), vec![Expect::Word]);
}

// ---------------------------------------------------------------- gap 6

/// `Unterminated` reports where the content begins, so no caller encodes
/// opener widths.
#[test]
fn unterminated_reports_where_content_begins() {
    use bsh_syntax::UnterminatedKind;
    let cases: &[(&[u8], UnterminatedKind, std::ops::Range<usize>, usize)] = &[
        (b"echo 'ab", UnterminatedKind::SingleQuote, 5..6, 6),
        (b"echo \"ab", UnterminatedKind::DoubleQuote, 5..6, 6),
        (b"echo $'ab", UnterminatedKind::AnsiCQuote, 5..7, 7),
        (b"echo `ab", UnterminatedKind::Backtick, 5..6, 6),
        (b"echo ${x", UnterminatedKind::ParamExp, 5..7, 7),
        (b"echo $(x", UnterminatedKind::CmdSub, 5..7, 7),
        (b"echo $((1", UnterminatedKind::Arith, 5..8, 8),
        (b"echo <(x", UnterminatedKind::ProcSub, 5..7, 7),
        (b"((x", UnterminatedKind::ArithCommand, 0..2, 2),
    ];
    for (src, kind, opener, content_start) in cases {
        let lexed = lex_full(src);
        let unterminated = lexed
            .unterminated
            .iter()
            .find(|u| u.kind == *kind)
            .unwrap_or_else(|| panic!("missing {kind:?}"));
        assert_eq!(&unterminated.opener, opener, "opener of {kind:?}");
        assert_eq!(
            unterminated.content_start, *content_start,
            "content start of {kind:?}"
        );
        let chain = &open_constructs_at(src, src.len());
        assert_eq!(
            chain[0].content_start, *content_start,
            "chain content start of {kind:?}"
        );
    }
}

// ---------------------------------------------------------------- gap 7

/// The closing keywords are reachable, and an open compound reports the
/// keyword that would close it.
#[test]
fn closing_keywords_are_reachable() {
    assert_eq!(first(b"if true; ", 9), Expect::Then);
    assert_eq!(first(b"while true; ", 12), Expect::Do);
    assert_eq!(first(b"for x in a; ", 12), Expect::Do);
    assert_eq!(first(b"if true; then echo; ", 20), Expect::Fi);
    assert_eq!(first(b"while true; do echo; ", 21), Expect::Done);
    assert_eq!(first(b"case x in a) echo;; ", 20), Expect::Esac);
    assert_eq!(first(b"case x ", 7), Expect::In);
    assert_eq!(first(b"f() ", 4), Expect::FunctionBody);
    assert_eq!(first(b"function f ", 11), Expect::FunctionBody);
    assert_eq!(first(b"for f ", 6), Expect::In);

    // A keyword needs a list boundary before it: an empty or unfinished body
    // still expects a command (`while true; do done` is a syntax error).
    assert_eq!(at(b"while true; do ", 14), vec![Expect::Command]);
    assert_eq!(at(b"if true; then ", 14), vec![Expect::Command]);
    assert_eq!(at(b"if true; then echo ", 19), vec![Expect::Word]);
    assert_eq!(at(b"case x in a) echo ", 19), vec![Expect::Word]);

    // Every variant named in the gap appears somewhere.
    let produced: Vec<Expect> = [
        b"if true; ".as_slice(),
        b"while true; ".as_slice(),
        b"if true; then echo; ".as_slice(),
        b"while true; do echo; ".as_slice(),
        b"case x in a) echo;; ".as_slice(),
        b"case x ".as_slice(),
        b"f() ".as_slice(),
    ]
    .into_iter()
    .flat_map(|src| at(src, src.len()))
    .collect();
    for expect in [
        Expect::Then,
        Expect::Do,
        Expect::Done,
        Expect::Fi,
        Expect::Esac,
        Expect::In,
        Expect::FunctionBody,
    ] {
        assert!(
            produced.contains(&expect),
            "{expect:?} is still unreachable"
        );
    }
}

/// Every `Expect` variant is produced somewhere; none is a lie.
#[test]
fn every_expectation_is_reachable() {
    let produced: Vec<Expect> = [
        b"if true; ".as_slice(),
        b"while true; ".as_slice(),
        b"if true; then echo; ".as_slice(),
        b"while true; do echo; ".as_slice(),
        b"case x in a) echo;; ".as_slice(),
        b"case x ".as_slice(),
        b"f() ".as_slice(),
        b"for f in ".as_slice(),
        b"echo > ".as_slice(),
        b"echo ${x".as_slice(),
        b"[[ -".as_slice(),
        b"echo $((x".as_slice(),
        b"{ echo; ".as_slice(),
        b"case x in a)".as_slice(),
        b"cat <<EOF\nbody".as_slice(),
        b"echo ".as_slice(),
        b"".as_slice(),
    ]
    .into_iter()
    .flat_map(|src| at(src, src.len()))
    .collect();
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
        assert!(produced.contains(&expect), "{expect:?} is unreachable");
    }
}

// ---------------------------------------------------------------- gap 8

/// Tokens carry a simple-command role, so consumers stop re-deriving command
/// boundaries from kinds and text.
#[test]
fn token_roles_classify_a_simple_command() {
    let src = b"FOO=1 git commit -m \"x y\" >out 2>&1; echo hi";
    let tokens = lex(src);
    let roles = classify_tokens(&tokens, src);
    assert_eq!(roles.len(), tokens.len());

    let significant: Vec<(TokenRole, Vec<u8>)> = roles
        .into_iter()
        .zip(&tokens)
        .filter(|(role, _)| *role != TokenRole::Filler)
        .map(|(role, token)| (role, token.text(src).to_vec()))
        .collect();
    assert_eq!(
        significant,
        vec![
            (TokenRole::Assignment, b"FOO=1".to_vec()),
            (TokenRole::CommandWord, b"git".to_vec()),
            (TokenRole::Argument, b"commit".to_vec()),
            (TokenRole::Argument, b"-m".to_vec()),
            (TokenRole::Argument, b"\"x y\"".to_vec()),
            (TokenRole::RedirectOperator, b">out".to_vec()),
            (TokenRole::RedirectOperator, b"2>&1".to_vec()),
            (TokenRole::Boundary, b";".to_vec()),
            (TokenRole::CommandWord, b"echo".to_vec()),
            (TokenRole::Argument, b"hi".to_vec()),
        ]
    );

    // A separate redirect target is not an argument.
    let src = b"cat > file arg";
    let roles = classify_tokens(&lex(src), src);
    let significant: Vec<(TokenRole, Vec<u8>)> = roles
        .into_iter()
        .zip(lex(src))
        .filter(|(role, _)| *role != TokenRole::Filler)
        .map(|(role, token)| (role, token.text(src).to_vec()))
        .collect();
    assert_eq!(
        significant,
        vec![
            (TokenRole::CommandWord, b"cat".to_vec()),
            (TokenRole::RedirectOperator, b">".to_vec()),
            (TokenRole::RedirectTarget, b"file".to_vec()),
            (TokenRole::Argument, b"arg".to_vec()),
        ]
    );
    assert!(TokenRole::CommandWord.is_command_word());
    assert!(TokenRole::Assignment.is_word());
    assert!(TokenRole::RedirectTarget.is_redirect());
}

// ---------------------------------------------------------------- gap 9

/// `parse_word` reads exactly one word, at the caller's offsets.
#[test]
fn parse_word_consumes_exactly_one_word() {
    let word = parse_word(b"  \"a b\"", &OPTS).expect("one quoted word");
    assert_eq!(word.span, 2..7, "spans are the caller's, not synthetic");

    let word = parse_word(b"$x/$(cmd)", &OPTS).expect("one substitution word");
    assert_eq!(word.span, 0..9);

    let (word, end) = parse_word_at(b"a b", 2, &OPTS).expect("word at offset 2");
    assert_eq!(word.span, 2..3);
    assert_eq!(end, 3);

    assert!(matches!(
        parse_word(b"foo; rm", &OPTS),
        Err(ParseWordError::Trailing { span }) if span == (3..4)
    ));
    assert!(matches!(
        parse_word(b"echo hi", &OPTS),
        Err(ParseWordError::Trailing { span }) if span == (5..7)
    ));
    assert!(matches!(
        parse_word(b"", &OPTS),
        Err(ParseWordError::Missing { at: 0 })
    ));
    assert!(matches!(
        parse_word(b"; foo", &OPTS),
        Err(ParseWordError::Missing { at: 0 })
    ));
    assert!(matches!(
        parse_word_at(b"abc", 1, &OPTS),
        Err(ParseWordError::Missing { at: 1 })
    ));
    assert!(matches!(
        parse_word(b"$(", &OPTS),
        Err(ParseWordError::Unterminated {
            opener,
            content_start: 2,
        }) if opener == (0..2)
    ));

    // Diagnostics point at the caller's bytes.
    let Err(ParseWordError::Invalid(diagnostics)) = parse_word(b"$'x'", &ParseOpts::POSIX) else {
        panic!("expected POSIX rejection");
    };
    assert_eq!(diagnostics[0].span, 0..4);
}
