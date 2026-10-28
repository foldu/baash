//! Lexer classification, here-documents, unterminated constructs, losslessness.

mod common;

use bsh_syntax::{TokenKind, UnterminatedKind, lex, lex_full};

fn kinds(src: &[u8]) -> Vec<TokenKind> {
    lex(src).into_iter().map(|t| t.kind).collect()
}

fn texts(src: &[u8]) -> Vec<Vec<u8>> {
    lex(src).iter().map(|t| t.text(src).to_vec()).collect()
}

fn significant(src: &[u8]) -> Vec<(TokenKind, Vec<u8>)> {
    lex(src)
        .into_iter()
        .filter(|t| !t.kind.is_trivia())
        .map(|t| (t.kind, t.text(src).to_vec()))
        .collect()
}

#[test]
fn simple_command_and_layout() {
    assert_eq!(
        kinds(b"echo hi"),
        vec![TokenKind::Word, TokenKind::Whitespace, TokenKind::Word]
    );
    assert_eq!(
        texts(b"echo  hi\n"),
        vec![
            b"echo".to_vec(),
            b"  ".to_vec(),
            b"hi".to_vec(),
            b"\n".to_vec()
        ]
    );
}

#[test]
fn quoting_is_one_token_and_retains_raw_bytes() {
    // Single quotes: no escapes at all.
    assert_eq!(texts(b"a'b c'd"), vec![b"a'b c'd".to_vec()]);
    // Double quotes with escaped quotes and a $ substitution inside.
    assert_eq!(
        texts(b"\"a \\\"b\\\" $x\""),
        vec![b"\"a \\\"b\\\" $x\"".to_vec()]
    );
    // A bare backslash escapes a metacharacter, so the word continues.
    assert_eq!(texts(b"a\\ b"), vec![b"a\\ b".to_vec()]);
    // Backslash-newline is a continuation, not a separator.
    assert_eq!(kinds(b"a\\\nb"), vec![TokenKind::Word]);
    // ANSI-C quoting and locale quoting.
    assert_eq!(texts(b"$'a\\tb'"), vec![b"$'a\\tb'".to_vec()]);
    assert_eq!(texts(b"$\"a\""), vec![b"$\"a\"".to_vec()]);
    // A quote does not end at an escaped quote.
    assert_eq!(texts(b"'a\\'b'"), vec![b"'a\\'b'".to_vec()]);
}

#[test]
fn substitutions_are_single_tokens_with_nesting() {
    for src in [
        &b"$(echo hi)"[..],
        &b"$(echo $(echo hi))"[..],
        &b"$(echo \")\")"[..],
        &b"$(echo ')')"[..],
        &b"$(case x in x) echo y;; esac)"[..],
        &b"${x:-${y:-z}}"[..],
        &b"`a \\` b`"[..],
        &b"$(( 1 + (2 * 3) ))"[..],
        &b"$(( $(echo 1) + 2 ))"[..],
        &b"<(true)"[..],
        &b">(false)"[..],
        &b"a$(b)c`d`e"[..],
    ] {
        assert_eq!(
            kinds(src),
            vec![TokenKind::Word],
            "expected one word token for {:?}",
            String::from_utf8_lossy(src)
        );
    }
}

#[test]
fn extglob_groups_are_part_of_the_word() {
    // All five operators, alone and embedded, stay one word with the bytes
    // exactly as written: the matcher needs active pattern text, not a
    // dequoted literal.
    for src in [
        &b"@(a|b)"[..],
        &b"?(a|b)"[..],
        &b"*(a|b)"[..],
        &b"+(a|b)"[..],
        &b"!(a|b)"[..],
        &b"x@(a|b)y"[..],
        &b"*.@(cc|h)"[..],
        &b"@(a|b)@(c|d)"[..],
        &b"@(a|b)?(c|d)"[..],
    ] {
        assert_eq!(
            texts(src),
            vec![src.to_vec()],
            "expected one word token for {:?}",
            String::from_utf8_lossy(src)
        );
    }
    // An extglob operator inside a word, not only at its start.
    assert_eq!(
        texts(b"g=--@(help|verbose)"),
        vec![b"g=--@(help|verbose)".to_vec()]
    );
    // A fused redirect target keeps the group with its operator.
    assert_eq!(texts(b">@(out|err).txt"), vec![b">@(out|err).txt".to_vec()]);
}

#[test]
fn extglob_groups_scan_nesting_quotes_and_substitutions() {
    // The `)` must match: nested groups, quoted stretches, escapes and
    // substitutions all hide their own parentheses and pipes.
    for src in [
        &b"@(a|@(b|c))"[..],
        &b"@(a|b)@(c|@(d|e))"[..],
        &b"@(\"a b\"|c)"[..],
        &b"@('a b'|c)"[..],
        &b"@(a\\|b|c)"[..],
        &b"@(a\\))b"[..],
        &b"@($(echo a)|b)"[..],
        &b"@($(echo ')')|b)"[..],
        &b"@($(case x in x) echo y;; esac)|b)"[..],
        &b"@(${x:-a}b|${y:-)}c)"[..],
        &b"@(a|`echo b`)"[..],
        &b"@(a|$'b c')"[..],
        &b"@(a|$((1 + 2)))"[..],
        &b"@(a|b)!(*.h)"[..],
    ] {
        assert_eq!(
            texts(src),
            vec![src.to_vec()],
            "expected one word token for {:?}",
            String::from_utf8_lossy(src)
        );
    }
}

#[test]
fn unclosed_extglob_group_leaves_the_paren_as_a_word_byte() {
    // No balanced `)`: the `(` is an ordinary word byte rather than a new
    // diagnostic. A later metacharacter still ends the word normally.
    for src in [
        &b"@(a"[..],
        &b"?(a"[..],
        &b"*(a"[..],
        &b"+(a"[..],
        &b"!(a"[..],
        &b"@("[..],
        &b"x@(a"[..],
        // Several operators inside one unclosed group: every `(` is a word
        // byte, and the scan does not restart for each operator.
        &b"@(a@(b"[..],
        &b"@(a@(b@(c"[..],
    ] {
        assert_eq!(
            texts(src),
            vec![src.to_vec()],
            "expected one literal word for {:?}",
            String::from_utf8_lossy(src)
        );
    }
    // A quote that hides a `)` cannot make the group close.
    assert_eq!(texts(b"@(a')'"), vec![b"@(a')'".to_vec()]);
    // Normal word rules still end the word: a space or a `|` is not swallowed
    // just because an unclosed group preceded it.
    assert_eq!(
        texts(b"@(a b"),
        vec![b"@(a".to_vec(), b" ".to_vec(), b"b".to_vec()]
    );
    assert_eq!(
        significant(b"@(a|b"),
        vec![
            (TokenKind::Word, b"@(a".to_vec()),
            (TokenKind::Operator, b"|".to_vec()),
            (TokenKind::Word, b"b".to_vec()),
        ]
    );
    // A balanced group may contain spaces and semicolons: it is still one
    // word. Without the closing `)`, normal scanning resumes at them.
    assert_eq!(texts(b"@(a; echo b)"), vec![b"@(a; echo b)".to_vec()]);
    assert_eq!(
        kinds(b"@(a; echo b"),
        vec![
            TokenKind::Word,
            TokenKind::Operator,
            TokenKind::Whitespace,
            TokenKind::Word,
            TokenKind::Whitespace,
            TokenKind::Word,
        ]
    );
}

#[test]
fn extglob_group_does_not_steal_the_case_separator() {
    // The group's `|` is inside the word; the pattern still ends at the `)`
    // after it.
    assert_eq!(
        significant(b"case x in @(a|b))"),
        vec![
            (TokenKind::Keyword, b"case".to_vec()),
            (TokenKind::Word, b"x".to_vec()),
            (TokenKind::Word, b"in".to_vec()),
            (TokenKind::Word, b"@(a|b)".to_vec()),
            (TokenKind::Operator, b")".to_vec()),
        ]
    );
    // A real alternative separator after the group is still an operator.
    assert_eq!(
        significant(b"case x in @(a|b)|c)"),
        vec![
            (TokenKind::Keyword, b"case".to_vec()),
            (TokenKind::Word, b"x".to_vec()),
            (TokenKind::Word, b"in".to_vec()),
            (TokenKind::Word, b"@(a|b)".to_vec()),
            (TokenKind::Operator, b"|".to_vec()),
            (TokenKind::Word, b"c".to_vec()),
            (TokenKind::Operator, b")".to_vec()),
        ]
    );
    // `!(…)` is an extglob group, so it is one pattern word too.
    assert_eq!(
        significant(b"case x in !(a|b))"),
        vec![
            (TokenKind::Keyword, b"case".to_vec()),
            (TokenKind::Word, b"x".to_vec()),
            (TokenKind::Word, b"in".to_vec()),
            (TokenKind::Word, b"!(a|b)".to_vec()),
            (TokenKind::Operator, b")".to_vec()),
        ]
    );
}

#[test]
fn arithmetic_command_is_one_token() {
    assert_eq!(
        significant(b"(( x = 1 + 2 ))"),
        vec![(TokenKind::ArithCommand, b"(( x = 1 + 2 ))".to_vec())]
    );
    // `((` at a token boundary is arithmetic even when it runs to EOF.
    assert_eq!(
        kinds(b"a((b"),
        vec![TokenKind::Word, TokenKind::ArithCommand]
    );
    assert_eq!(
        texts(b"a ((b))"),
        vec![b"a".to_vec(), b" ".to_vec(), b"((b))".to_vec()]
    );
}

#[test]
fn operators_are_longest_match() {
    let cases: &[(&[u8], &[u8])] = &[
        (b"a && b", b"&&"),
        (b"a || b", b"||"),
        (b"a | b", b"|"),
        (b"a |& b", b"|&"),
        (b"a; b", b";"),
        (b"a;; b", b";;"),
        (b"a;& b", b";&"),
        (b"a;;& b", b";;&"),
        (b"a & b", b"&"),
        (b"(a)", b"("),
        (b"{ a; }", b"{"),
        (b"{ a; }", b"}"),
    ];
    for (src, op) in cases {
        let ops: Vec<Vec<u8>> = lex(src)
            .into_iter()
            .filter(|t| t.kind == TokenKind::Operator)
            .map(|t| t.text(src).to_vec())
            .collect();
        assert!(
            ops.iter().any(|o| o == op),
            "missing operator {:?} in {:?} (got {ops:?})",
            String::from_utf8_lossy(op),
            String::from_utf8_lossy(src)
        );
    }
}

#[test]
fn redirections_and_fd_prefixes() {
    let cases: &[(&[u8], &[u8])] = &[
        (b">", b">"),
        (b">>", b">>"),
        (b"<", b"<"),
        (b"<>", b"<>"),
        (b">|", b">|"),
        (b"<<", b"<<"),
        (b"<<-", b"<<-"),
        (b"<<<", b"<<<"),
        (b">&", b">&"),
        (b"<&", b"<&"),
        (b"&>", b"&>"),
        (b"&>>", b"&>>"),
        (b"2>", b"2>"),
        (b"2>&1", b"2>&1"),
        (b"10>>f", b"10>>f"),
        (b"cmd >file", b">file"),
        (b"cmd 2>&1", b"2>&1"),
        (b"cmd >&2", b">&2"),
        (b"cmd 2>&-", b"2>&-"),
    ];
    for (src, want) in cases {
        let redirects: Vec<Vec<u8>> = lex(src)
            .into_iter()
            .filter(|t| t.kind == TokenKind::Redirect)
            .map(|t| t.text(src).to_vec())
            .collect();
        assert!(
            redirects.iter().any(|r| r == want),
            "missing redirect {:?} in {:?} (got {redirects:?})",
            String::from_utf8_lossy(want),
            String::from_utf8_lossy(src)
        );
    }
    // A digit only starts an fd prefix when `<`/`>` follows it.
    assert_eq!(
        kinds(b"2 >file"),
        vec![TokenKind::Word, TokenKind::Whitespace, TokenKind::Redirect]
    );
    assert_eq!(kinds(b"2>file"), vec![TokenKind::Redirect]);
    // `a2>f` is command a2, not a redirect from fd 2.
    assert_eq!(kinds(b"a2>f"), vec![TokenKind::Word, TokenKind::Redirect]);
}

#[test]
fn comments_only_at_word_start() {
    assert_eq!(kinds(b"# hi"), vec![TokenKind::Comment]);
    assert_eq!(texts(b"a#b"), vec![b"a#b".to_vec()]);
    assert_eq!(
        kinds(b"echo hi # and more"),
        vec![
            TokenKind::Word,
            TokenKind::Whitespace,
            TokenKind::Word,
            TokenKind::Whitespace,
            TokenKind::Comment
        ]
    );
    // A `#` immediately after an operator starts a comment.
    assert_eq!(kinds(b";#c"), vec![TokenKind::Operator, TokenKind::Comment]);
}

#[test]
fn reserved_words_in_command_position() {
    assert_eq!(kinds(b"if"), vec![TokenKind::Keyword]);
    assert_eq!(kinds(b"ifconfig"), vec![TokenKind::Word]);
    assert_eq!(
        kinds(b"echo if"),
        vec![TokenKind::Word, TokenKind::Whitespace, TokenKind::Word]
    );
    // After an assignment the reserved word is an ordinary command word.
    assert_eq!(
        kinds(b"x=1 fi"),
        vec![TokenKind::Word, TokenKind::Whitespace, TokenKind::Word]
    );
    // After `;` it is reserved again.
    assert_eq!(
        kinds(b"; then"),
        vec![
            TokenKind::Operator,
            TokenKind::Whitespace,
            TokenKind::Keyword
        ]
    );
}

#[test]
fn heredoc_bodies_are_not_shell_code() {
    let src = b"cat <<EOF\nif true; then ;; fi\nEOF\necho done";
    let l = lex_full(src);
    let body_tokens: Vec<&bsh_syntax::Token> = l
        .tokens
        .iter()
        .filter(|t| t.kind == TokenKind::HereDocBody)
        .collect();
    assert_eq!(body_tokens.len(), 1);
    assert_eq!(
        body_tokens[0].text(src),
        b"if true; then ;; fi\n",
        "the body must stay raw"
    );
    let delim = l
        .tokens
        .iter()
        .find(|t| t.kind == TokenKind::HereDocDelimiter)
        .expect("delimiter token");
    assert_eq!(delim.text(src), b"EOF");
    // `if`, `then` etc. inside the body did not become keywords.
    assert!(!l.tokens.iter().any(|t| t.kind == TokenKind::Keyword));
    assert_eq!(l.heredocs.len(), 1);
    let h = &l.heredocs[0];
    assert_eq!(h.delimiter_text, b"EOF");
    assert!(!h.quoted);
    assert_eq!(&src[h.body.clone()], b"if true; then ;; fi\n");
    assert!(h.terminated);
}

#[test]
fn several_heredocs_on_one_line_attach_in_order() {
    let src = b"cat <<A <<B\none\nA\ntwo\nB\n";
    let l = lex_full(src);
    assert_eq!(l.heredocs.len(), 2);
    assert_eq!(l.heredocs[0].delimiter_text, b"A");
    assert_eq!(&src[l.heredocs[0].body.clone()], b"one\n");
    assert_eq!(l.heredocs[1].delimiter_text, b"B");
    assert_eq!(&src[l.heredocs[1].body.clone()], b"two\n");
}

#[test]
fn heredoc_quoted_delimiter_and_tab_stripping() {
    let src = b"cat <<-'E O F'\n\tbody\n\tE O F\n";
    let l = lex_full(src);
    let h = &l.heredocs[0];
    assert_eq!(h.delimiter_text, b"E O F");
    assert!(h.quoted);
    assert!(h.strip_tabs);
    assert_eq!(&src[h.body.clone()], b"\tbody\n");
    assert!(h.terminated);
}

#[test]
fn empty_heredoc_body() {
    let src = b"cat <<EOF\nEOF\n";
    let l = lex_full(src);
    let h = &l.heredocs[0];
    assert_eq!(h.body.start, h.body.end);
    assert!(h.terminated);
    // No zero-length body token.
    assert!(!l.tokens.iter().any(|t| t.kind == TokenKind::HereDocBody));
}

#[test]
fn unterminated_constructs_span_to_end_with_an_opener() {
    let cases: &[(&[u8], UnterminatedKind, usize)] = &[
        (b"echo 'abc", UnterminatedKind::SingleQuote, 5),
        (b"echo \"abc", UnterminatedKind::DoubleQuote, 5),
        (b"echo $'abc", UnterminatedKind::AnsiCQuote, 5),
        (b"echo `abc", UnterminatedKind::Backtick, 5),
        (b"echo $(abc", UnterminatedKind::CmdSub, 5),
        (b"echo ${abc", UnterminatedKind::ParamExp, 5),
        (b"echo $((1 +", UnterminatedKind::Arith, 5),
        (b"echo <(abc", UnterminatedKind::ProcSub, 5),
    ];
    for (src, kind, opener) in cases {
        let l = lex_full(src);
        let tok = l.tokens.last().expect("a token");
        assert_eq!(
            tok.kind,
            TokenKind::WordUnterminated,
            "for {:?}",
            String::from_utf8_lossy(src)
        );
        assert_eq!(tok.span.end, src.len());
        let u = l
            .unterminated
            .iter()
            .find(|u| u.kind == *kind)
            .unwrap_or_else(|| panic!("missing Unterminated {kind:?}"));
        assert_eq!(u.opener.start, *opener, "opener of {kind:?}");
        assert_eq!(u.span.end, src.len());
    }
}

#[test]
fn unterminated_here_doc_is_reported() {
    let src = b"cat <<EOF\nbody\n";
    let l = lex_full(src);
    assert_eq!(l.heredocs.len(), 1);
    assert!(!l.heredocs[0].terminated);
    assert!(
        l.unterminated
            .iter()
            .any(|u| u.kind == UnterminatedKind::HereDoc && u.opener.start == 4)
    );
}

/// Every construct this lexer claims to handle, concatenated. The lossless
/// invariant must hold for the whole thing.
const CORPUS: &[&[u8]] = &[
    b"echo hi",
    b"  # a comment\n",
    b"if true; then echo 'a b'; elif false; then :; else :; fi\n",
    b"for f in a b; do echo \"$f\"; done\n",
    b"for ((i = 0; i < 3; i++)); do :; done\n",
    b"while :; do break; done\n",
    b"until false; do :; done\n",
    b"case $x in (a|b) echo y;; c) echo z;& *) echo w;;& esac\n",
    b"{ echo hi; }\n",
    b"( echo hi )\n",
    b"f() { :; }\n",
    b"function g { :; }\n",
    b"! a |& b && c || d & e; f\n",
    b"cmd >out >>app <in <>rw >|clob 2>&1 3<&0 &>both &>>both2\n",
    b"echo $(echo \"$(echo hi)\") ${x:-${y}} $(( 1 + 2 )) `date`\n",
    b"echo $'a\\tb' $\"locale\" <(true) >(false)\n",
    b"cat <<EOF\nbody $x 'q' ;; (\nEOF\n",
    b"cat <<-A <<B\n\tone\n\tA\ntwo\nB\n",
    b"cat <<'Q'\n$x\nQ\n",
    b"(( x = 1 + 2 ))\n",
    b"[[ -f x && ( -d y ) ]]\n",
    b"x=1 y+=(a b) z[2]=v\n",
    b"echo @(a|b) ?(c) *(d|e) +(f) !(g)\n",
    b"case $x in @(a|b)|c) echo y;; !(d)) echo z;; esac\n",
    b"g=--@(help|verbose) cmd >@(out|err).txt A=(x @(a|b).py)\n",
    b"echo @(\"a b\"|$(echo c)|${d:-e}) @(a|@(b|c))\n",
    b"echo @(unterminated",
    b"echo \"unterminated",
    b"echo 'unterminated",
    b"echo $(unterminated",
    b"echo ${unterminated",
    b"cat <<NOPE\nbody\n",
    b"\xff\xfe non-utf8 \x00 bytes\n",
];

#[test]
fn spans_partition_every_corpus_input() {
    for input in CORPUS {
        let tokens = lex(input);
        let mut at = 0;
        for t in &tokens {
            assert_eq!(
                t.span.start,
                at,
                "gap or overlap at token {:?} in {:?}",
                t.kind,
                String::from_utf8_lossy(input)
            );
            assert!(t.span.end >= t.span.start);
            at = t.span.end;
        }
        assert_eq!(
            at,
            input.len(),
            "tokens do not cover {:?}",
            String::from_utf8_lossy(input)
        );
        // Reconstructing the source from token text is byte-identical.
        let mut rebuilt = Vec::new();
        for t in &tokens {
            rebuilt.extend_from_slice(t.text(input));
        }
        assert_eq!(&rebuilt[..], *input);
    }
}

#[test]
fn spans_partition_random_bytes() {
    let mut rng = common::Lcg(0x5eed_1234_abcd_0001);
    for case in 0..2000 {
        let len = (rng.next_u32() % 48) as usize;
        let mut src = Vec::with_capacity(len);
        for _ in 0..len {
            // Bias toward shell punctuation so constructs actually form.
            let b = rng.byte();
            let b = match b % 4 {
                0 => b" \t\n'\"`$(){}[]<>|&;=#\\"[b as usize % 20],
                _ => b,
            };
            src.push(b);
        }
        let tokens = lex(&src);
        let mut at = 0;
        for t in &tokens {
            assert_eq!(
                t.span.start,
                at,
                "case {case}: gap at {:?} in {:?}",
                t.kind,
                String::from_utf8_lossy(&src)
            );
            at = t.span.end;
        }
        assert_eq!(
            at,
            src.len(),
            "case {case}: incomplete coverage of {:?}",
            String::from_utf8_lossy(&src)
        );
    }
}
