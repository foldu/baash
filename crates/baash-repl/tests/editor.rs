//! Behavioural tests for the line editor, driven through a fake terminal.

mod common;

use baash_complete::Candidate;
use baash_repl::{Config, Editor, Flow, History, Key, Prompt, ReadResult, Size, Theme};

use common::{
    FakeTerminal, Screen, Session, StubCompleter, StubSyntax, apply_keys, chars, clear_output,
    config_with_history, drive, has_sgr, last_frame, output_text, plain_config, run,
    seeded_history, session,
};

#[test]
fn typing_a_line_and_submitting_returns_it_newline_terminated() {
    let (result, term) = run(
        FakeTerminal::new(b"echo hi\r"),
        &StubCompleter::empty(),
        &baash_repl::NeverNeedsMore,
        plain_config(),
    );
    assert_eq!(result, ReadResult::Line(b"echo hi\n".to_vec()));
    let output = term.text();
    assert!(
        output.contains("$ echo hi"),
        "prompt and line echoed: {output:?}"
    );
    assert!(
        term.raw_balanced(),
        "raw mode is restored on the normal path"
    );
}

#[test]
fn control_keys_edit_the_buffer() {
    // (keys after typing "abc", expected buffer, expected cursor)
    let cases: Vec<(Vec<Key>, &[u8], usize)> = vec![
        (vec![Key::Left, Key::Char('X')], b"abXc", 3),
        (vec![Key::Home, Key::Char('X')], b"Xabc", 1),
        (vec![Key::Home, Key::End, Key::Char('X')], b"abcX", 4),
        (vec![Key::Left, Key::Left, Key::Backspace], b"bc", 0),
        (vec![Key::Home, Key::Delete], b"bc", 0),
        (vec![Key::Ctrl(0x17)], b"", 0),
        (vec![Key::Home, Key::Ctrl(0x0b)], b"", 0),
        (vec![Key::Ctrl(0x15)], b"", 0),
        (vec![Key::Ctrl(0x14)], b"acb", 3),
        (vec![Key::Alt('b')], b"abc", 0),
        (vec![Key::Ctrl(0x01), Key::Ctrl(0x05)], b"abc", 3),
    ];
    for (keys, buffer, cursor) in cases {
        let mut all = chars("abc");
        all.extend(keys.iter().cloned());
        let (actual_buffer, actual_cursor) = apply_keys(&all);
        assert_eq!(
            (actual_buffer.as_slice(), actual_cursor),
            (buffer, cursor),
            "keys {keys:?} from \"abc\""
        );
    }
}

#[test]
fn alt_keys_kill_words_and_move_by_them() {
    // Alt-Backspace is `backward-kill-word`: word characters only, so the
    // dash stops it where Ctrl-W would have taken `foo-bar` whole.
    let mut keys = chars("foo-bar");
    keys.push(Key::AltBackspace);
    assert_eq!(apply_keys(&keys).0, b"foo-");

    // Alt-D kills forward from the cursor to the end of the next word, and two
    // presses accumulate into one kill that a single yank puts back.
    let mut keys = chars("one two three");
    keys.extend([Key::Home, Key::Alt('f')]);
    assert_eq!(apply_keys(&keys).0, b"one two three");
    keys.extend([Key::Alt('d'), Key::Alt('d')]);
    assert_eq!(apply_keys(&keys).0, b"one");
    keys.push(Key::Ctrl(0x19));
    assert_eq!(apply_keys(&keys).0, b"one two three");
}

#[test]
fn kill_and_yank_round_trip_through_the_editor() {
    let mut keys = chars("one two");
    keys.push(Key::Ctrl(0x17));
    assert_eq!(apply_keys(&keys).0, b"one ");
    keys.push(Key::Ctrl(0x19));
    assert_eq!(apply_keys(&keys).0, b"one two");
}

#[test]
fn utf8_is_inserted_and_deleted_by_character() {
    let mut keys = chars("héllo");
    keys.push(Key::Backspace);
    assert_eq!(apply_keys(&keys).0, "héll".as_bytes());

    // A wide character is one editing unit even though it is three bytes.
    let keys = vec![Key::Char('你'), Key::Char('好'), Key::Left, Key::Backspace];
    assert_eq!(apply_keys(&keys).0, "好".as_bytes());
}

#[test]
fn escape_sequences_move_and_delete_through_the_terminal() {
    // Left then insert.
    let (result, _) = run(
        FakeTerminal::new(b"ab\x1b[Dc\r"),
        &StubCompleter::empty(),
        &baash_repl::NeverNeedsMore,
        plain_config(),
    );
    assert_eq!(result, ReadResult::Line(b"acb\n".to_vec()));

    // Home/End and Delete.
    let (result, _) = run(
        FakeTerminal::new(b"bc\x1b[H a\x1b[F!\r"),
        &StubCompleter::empty(),
        &baash_repl::NeverNeedsMore,
        plain_config(),
    );
    assert_eq!(result, ReadResult::Line(b" abc!\n".to_vec()));

    let (result, _) = run(
        FakeTerminal::new(b"abc\x1b[D\x1b[D\x1b[3~\r"),
        &StubCompleter::empty(),
        &baash_repl::NeverNeedsMore,
        plain_config(),
    );
    assert_eq!(result, ReadResult::Line(b"ac\n".to_vec()));
}

#[test]
fn alt_b_and_alt_f_move_by_word() {
    let (result, _) = run(
        FakeTerminal::new(b"git commit\x1bbX\r"),
        &StubCompleter::empty(),
        &baash_repl::NeverNeedsMore,
        plain_config(),
    );
    assert_eq!(result, ReadResult::Line(b"git Xcommit\n".to_vec()));
}

#[test]
fn a_truncated_escape_sequence_is_not_inserted_as_text() {
    let (result, _) = run(
        FakeTerminal::new(b"ab\x1b[3\r"),
        &StubCompleter::empty(),
        &baash_repl::NeverNeedsMore,
        plain_config(),
    );
    assert_eq!(result, ReadResult::Line(b"ab\n".to_vec()));
}

#[test]
fn a_bracketed_paste_is_inserted_verbatim() {
    let (result, _) = run(
        FakeTerminal::new(b"\x1b[200~echo \x1b[A hi\nsecond\x1b[201~\r"),
        &StubCompleter::empty(),
        &baash_repl::NeverNeedsMore,
        plain_config(),
    );
    assert_eq!(
        result,
        ReadResult::Line(b"echo \x1b[A hi\nsecond\n".to_vec()),
        "the paste payload must not be decoded as arrows or Enter"
    );
}

#[test]
fn ctrl_d_eof_and_delete() {
    let (result, term) = run(
        FakeTerminal::new(b"\x04"),
        &StubCompleter::empty(),
        &baash_repl::NeverNeedsMore,
        plain_config(),
    );
    assert_eq!(result, ReadResult::Eof);
    assert!(term.raw_balanced());

    let (result, _) = run(
        FakeTerminal::new(b"ab\x1b[D\x04\r"),
        &StubCompleter::empty(),
        &baash_repl::NeverNeedsMore,
        plain_config(),
    );
    assert_eq!(
        result,
        ReadResult::Line(b"a\n".to_vec()),
        "Ctrl-D on a non-empty line deletes instead of exiting"
    );
}

#[test]
fn ctrl_c_abandons_the_line_and_restores_raw_mode() {
    let (result, term) = run(
        FakeTerminal::new(b"partial\x03\r"),
        &StubCompleter::empty(),
        &baash_repl::NeverNeedsMore,
        plain_config(),
    );
    assert_eq!(result, ReadResult::Interrupted);
    assert!(term.text().contains("^C\r\n"));
    assert!(term.raw_balanced());
}

#[test]
fn ctrl_l_clears_the_screen_and_keeps_editing() {
    let (result, term) = run(
        FakeTerminal::new(b"ab\x0c\r"),
        &StubCompleter::empty(),
        &baash_repl::NeverNeedsMore,
        plain_config(),
    );
    assert_eq!(result, ReadResult::Line(b"ab\n".to_vec()));
    assert!(term.text().contains("\x1b[H\x1b[2J"));
}

#[test]
fn a_coloured_prompt_does_not_count_its_non_printing_stretches() {
    // PS1='\[\e[31m\]$\[\e[0m\] ': two visible cells (`$` and a space)
    // around ANSI colour sequences, which go to the terminal but not to the
    // cursor column.
    let mut term = FakeTerminal::new(b"x");
    let completer = StubCompleter::empty();
    let config = Config {
        color: false,
        prompt: Prompt::new(b"\\[\\e[31m\\]$\\[\\e[0m\\] ".to_vec(), b"> ".to_vec()),
        ..plain_config()
    };
    let mut editor =
        Editor::with_config(&mut term, &completer, &baash_repl::NeverNeedsMore, config);
    editor.step(Key::Char('x')).expect("step");
    let frame = term.take_output();
    assert!(
        frame.contains("\x1b[31m$\x1b[0m x"),
        "the prompt is painted with its colour: {frame:?}"
    );
    assert!(
        frame.ends_with("\r\x1b[3C"),
        "two visible prompt cells plus one character put the cursor at column 3: {frame:?}"
    );
}

#[test]
fn the_line_wraps_at_the_terminal_width() {
    let frame = last_frame(&chars("abcdef"), 5);
    assert!(
        frame.contains("$ abc\r\ndef"),
        "five columns: the prompt and three characters fill the row, the rest wraps: {frame:?}"
    );
    assert!(
        frame.ends_with("\r\x1b[3C"),
        "cursor lands at column 3 of the second row: {frame:?}"
    );
}

#[test]
fn a_repaint_moves_up_over_a_multi_row_edit_region() {
    let line = "a".repeat(100);
    let frame = last_frame(&chars(&line), 40);
    assert!(
        frame.starts_with("\x1b[2A\r\x1b[J"),
        "the previous cursor sat on row 2, so the repaint starts two rows up: {frame:?}"
    );
    assert!(
        frame.ends_with("\r\x1b[22C"),
        "2 + 100 cells is row 2, column 22: {frame:?}"
    );
}

#[test]
fn the_cursor_is_placed_on_the_right_wrapped_row() {
    let mut keys = chars("abcdef");
    keys.push(Key::Home);
    keys.push(Key::Right);
    keys.push(Key::Right);
    let frame = last_frame(&keys, 5);
    assert!(
        frame.ends_with("\x1b[1A\r\x1b[4C"),
        "cursor at source offset 2 is row 0 column 4, so it moves up from the second row: {frame:?}"
    );
}

#[test]
fn plain_rendering_emits_no_sgr() {
    let frame = last_frame(&chars("if x; then echo hi"), 80);
    assert!(!has_sgr(&frame), "no colour was requested: {frame:?}");
}

#[test]
fn colour_rendering_highlights_keywords() {
    let mut term = FakeTerminal::new(b"");
    let completer = StubCompleter::empty();
    let config = Config {
        color: true,
        theme: Theme::DARK,
        ..plain_config()
    };
    let mut editor =
        Editor::with_config(&mut term, &completer, &baash_repl::NeverNeedsMore, config);
    for key in chars("if x") {
        editor.step(key).unwrap();
    }
    let frame = term.take_output();
    assert!(
        frame.contains("\x1b[1;35mif\x1b[0m"),
        "a keyword is bold magenta: {frame:?}"
    );
}

#[test]
fn the_prompt_expands_its_escapes() {
    // The shell renders `PS1`; the editor paints what it is given. These tests
    // go through the adapter so the escape table stays covered from here too.
    let mut term = FakeTerminal::new(b"");
    let completer = StubCompleter::empty();
    let state = baash_repl::PromptState {
        user: b"ada".to_vec(),
        root: false,
        ..baash_repl::PromptState::default()
    };
    let config = Config {
        prompt: Prompt::from_template(b"\\u \\$ ", b"> ", &state),
        ..plain_config()
    };
    let mut editor = Editor::new(&mut term, &completer, &baash_repl::NeverNeedsMore);
    editor.set_prompt(config.prompt.clone());
    editor.step(Key::Char('x')).unwrap();
    assert!(term.text().contains("ada $ x"));

    let mut term = FakeTerminal::new(b"");
    let root = baash_repl::PromptState {
        root: true,
        ..baash_repl::PromptState::default()
    };
    let config = Config {
        prompt: Prompt::from_template(b"\\$ ", b"> ", &root),
        ..plain_config()
    };
    let mut editor =
        Editor::with_config(&mut term, &completer, &baash_repl::NeverNeedsMore, config);
    editor.step(Key::Char('x')).unwrap();
    assert!(term.text().contains("# x"));
}

#[test]
fn a_prompt_rendered_for_a_directory_shows_it() {
    let mut term = FakeTerminal::new(b"");
    let completer = StubCompleter::empty();
    let state = baash_repl::PromptState {
        cwd: b"/tmp/place".to_vec(),
        ..baash_repl::PromptState::default()
    };
    let config = Config {
        prompt: Prompt::from_template(b"\\w$ ", b"> ", &state),
        ..plain_config()
    };
    let mut editor =
        Editor::with_config(&mut term, &completer, &baash_repl::NeverNeedsMore, config);
    editor.step(Key::Char('x')).unwrap();
    assert!(term.text().contains("/tmp/place$ x"));
}

#[test]
fn continuation_reads_ps2_until_the_parser_is_satisfied() {
    let syntax =
        StubSyntax::new(|buffer: &[u8]| buffer.starts_with(b"if") && !buffer.ends_with(b"fi\n"));
    let driven = session(
        FakeTerminal::new(b"if true\rthen echo hi\rfi\r"),
        &StubCompleter::empty(),
        &syntax,
        plain_config(),
        5,
    );
    assert_eq!(
        driven.results[0],
        ReadResult::Line(b"if true\nthen echo hi\nfi\n".to_vec())
    );
    assert_eq!(driven.results.len(), 2, "second read hits end of input");
    assert_eq!(driven.results[1], ReadResult::Eof);
    let output = driven.term.text();
    assert!(
        output.contains("> then echo hi") && output.contains("> fi"),
        "PS2 is shown for continuation lines: {output:?}"
    );
    assert_eq!(
        driven.history,
        vec![b"if true\nthen echo hi\nfi".to_vec()],
        "the whole logical command is one history entry"
    );
}

#[test]
fn continuation_handles_an_unclosed_here_document() {
    // A here-document is unfinished until its terminator line arrives; the
    // heredoc *header* `<<EOF` must not count, so look for a line that is
    // exactly `EOF`.
    let syntax = StubSyntax::new(|buffer: &[u8]| {
        buffer.starts_with(b"cat") && !buffer.windows(5).any(|window| window == b"\nEOF\n")
    });
    let driven = session(
        FakeTerminal::new(b"cat <<EOF\rhello world\rEOF\r"),
        &StubCompleter::empty(),
        &syntax,
        plain_config(),
        5,
    );
    assert_eq!(
        driven.results[0],
        ReadResult::Line(b"cat <<EOF\nhello world\nEOF\n".to_vec())
    );
    assert!(driven.term.text().contains("> hello world"));
}

#[test]
fn ctrl_d_on_an_empty_continuation_line_abandons_the_command() {
    let syntax = StubSyntax::new(|buffer: &[u8]| !buffer.ends_with(b"fi\n"));
    let driven: Session = session(
        FakeTerminal::new(b"if true\r\x04"),
        &StubCompleter::empty(),
        &syntax,
        plain_config(),
        3,
    );
    assert_eq!(
        driven.results,
        vec![ReadResult::Interrupted],
        "the shell returns to PS1 instead of submitting a partial command"
    );
    assert!(driven.term.text().contains("^C"));
    assert!(driven.term.raw_balanced());
}

#[test]
fn a_multi_line_prompt_keeps_the_cursor_on_the_right_row() {
    let mut term = FakeTerminal::new(b"");
    let completer = StubCompleter::empty();
    let config = Config {
        prompt: Prompt::new(b"one\\ntwo$ ", b"> "),
        ..plain_config()
    };
    let mut editor =
        Editor::with_config(&mut term, &completer, &baash_repl::NeverNeedsMore, config);
    editor.step(Key::Char('x')).unwrap();
    let output = term.text();
    assert!(
        output.contains("one\r\ntwo$ x"),
        "the escape becomes a real line break: {output:?}"
    );
}

#[test]
fn history_navigation_restores_the_draft() {
    let driven = session(
        FakeTerminal::new(b"ls\rcd\rec\x1b[A\x1b[B\r"),
        &StubCompleter::empty(),
        &baash_repl::NeverNeedsMore,
        plain_config(),
        4,
    );
    assert_eq!(
        driven.results[0..3],
        [
            ReadResult::Line(b"ls\n".to_vec()),
            ReadResult::Line(b"cd\n".to_vec()),
            ReadResult::Line(b"ec\n".to_vec()),
        ],
        "Up recalled `cd`, Down restored the `ec` draft"
    );
    assert_eq!(
        driven.history,
        vec![b"ls".to_vec(), b"cd".to_vec(), b"ec".to_vec()]
    );
}

#[test]
fn every_submitted_line_is_kept_including_duplicates() {
    let driven = session(
        FakeTerminal::new(b"ls\rls\r"),
        &StubCompleter::empty(),
        &baash_repl::NeverNeedsMore,
        plain_config(),
        3,
    );
    assert_eq!(
        driven.history,
        vec![b"ls".to_vec(), b"ls".to_vec()],
        "there is no HISTCONTROL to filter a line any more"
    );
}

#[test]
fn history_reaches_the_database_on_finish() {
    let path = std::env::temp_dir().join(format!(
        "baash-repl-editor-history-{}/history.db",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
    {
        let mut term = FakeTerminal::new(b"echo new\r");
        let completer = StubCompleter::empty();
        let config = Config {
            history: History::open(&path, 100).unwrap(),
            ..plain_config()
        };
        let mut editor =
            Editor::with_config(&mut term, &completer, &baash_repl::NeverNeedsMore, config);
        assert_eq!(
            editor.read_line().unwrap(),
            ReadResult::Line(b"echo new\n".to_vec())
        );
        editor.finish();
    }
    let history = History::open(&path, 100).unwrap();
    assert_eq!(
        history.commands().collect::<Vec<_>>(),
        vec![b"echo new".as_slice()]
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn a_single_candidate_is_inserted_through_the_menu() {
    let completer = StubCompleter::words(&["My File"]);
    let (result, _) = run(
        FakeTerminal::new(b"ls My\t\r"),
        &completer,
        &baash_repl::NeverNeedsMore,
        plain_config(),
    );
    assert_eq!(
        result,
        ReadResult::Line(b"ls My\\ File \n".to_vec()),
        "quoting and the trailing space come from `Menu::apply`"
    );
}

#[test]
fn several_candidates_open_a_menu_and_insert_the_selection() {
    let completer = StubCompleter::words(&["alpha", "alpine"]);
    let mut term = FakeTerminal::new(b"");
    let sink = term.output_handle();
    {
        let config = plain_config();
        let mut editor =
            Editor::with_config(&mut term, &completer, &baash_repl::NeverNeedsMore, config);
        for key in [
            Key::Char('l'),
            Key::Char('s'),
            Key::Char(' '),
            Key::Char('a'),
        ] {
            editor.step(key).unwrap();
        }
        clear_output(&sink);
        // The first Tab inserts the unambiguous prefix and opens nothing.
        assert_eq!(editor.step(Key::Tab).unwrap(), Flow::Continue);
        assert_eq!(editor.line().buffer(), b"ls alp");
        assert!(
            !output_text(&sink).contains("> alpha"),
            "the first Tab must not paint the menu"
        );
        // The second opens it; Down moves the selection and Enter inserts.
        assert_eq!(editor.step(Key::Tab).unwrap(), Flow::Continue);
        let frame = output_text(&sink);
        assert!(frame.contains("> alpha"), "the menu is painted: {frame:?}");
        assert!(frame.contains("  alpine"));
        editor.step(Key::Down).unwrap();
        assert_eq!(editor.step(Key::Enter).unwrap(), Flow::Continue);
        assert_eq!(editor.line().buffer(), b"ls alpine");
        assert_eq!(editor.step(Key::Enter).unwrap(), Flow::Submit);
    }
    assert!(term.text().contains("ls alpine"));
}

#[test]
fn tab_accepts_the_selected_candidate() {
    let completer = StubCompleter::words(&["alpha", "alpine"]);
    let (result, _) = run(
        FakeTerminal::new(b"ls a\t\t\r\r"),
        &completer,
        &baash_repl::NeverNeedsMore,
        plain_config(),
    );
    assert_eq!(result, ReadResult::Line(b"ls alpha\n".to_vec()));
}

#[test]
fn escape_cancels_the_menu_without_inserting() {
    let completer = StubCompleter::words(&["alpha", "alpine"]);
    let mut term = FakeTerminal::new(b"");
    let sink = term.output_handle();
    let config = plain_config();
    let mut editor =
        Editor::with_config(&mut term, &completer, &baash_repl::NeverNeedsMore, config);
    for key in [
        Key::Char('l'),
        Key::Char('s'),
        Key::Char(' '),
        Key::Char('a'),
    ] {
        editor.step(key).unwrap();
    }
    // Two Tabs: the prefix, then the menu. Escape closes the menu without
    // inserting the selection; the prefix is already part of the word.
    editor.step(Key::Tab).unwrap();
    editor.step(Key::Tab).unwrap();
    assert_eq!(editor.step(Key::Esc).unwrap(), Flow::Continue);
    clear_output(&sink);
    editor.step(Key::Char('!')).unwrap();
    assert_eq!(editor.line().buffer(), b"ls alp!");
    assert!(
        !output_text(&sink).contains("alpha"),
        "the menu is gone after Escape"
    );
}

#[test]
fn no_candidates_bells_and_leaves_the_line_alone() {
    let completer = StubCompleter::empty();
    let (result, term) = run(
        FakeTerminal::new(b"ls zz\t\r"),
        &completer,
        &baash_repl::NeverNeedsMore,
        plain_config(),
    );
    assert_eq!(result, ReadResult::Line(b"ls zz\n".to_vec()));
    assert!(term.text().contains('\u{7}'), "a bell is the no-op signal");
}

#[test]
fn ghost_text_completes_from_history_and_is_accepted_by_movement_keys() {
    // `Right`/`End` and their readline spellings `Ctrl-F`/`Ctrl-E`: forward
    // past the end of the line is how the suggestion is taken, so the four
    // mean the same thing at the end of the line.
    for accept in [Key::Right, Key::End, Key::Ctrl(0x06), Key::Ctrl(0x05)] {
        let mut history = History::new();
        history.push(b"echo hello\n");
        let mut term = FakeTerminal::sized(b"", 80, 24);
        let completer = StubCompleter::empty();
        let config = Config {
            color: true,
            history,
            ..plain_config()
        };
        let sink = term.output_handle();
        let mut editor =
            Editor::with_config(&mut term, &completer, &baash_repl::NeverNeedsMore, config);
        clear_output(&sink);
        for key in chars("echo h") {
            editor.step(key).unwrap();
        }
        let frame = output_text(&sink);
        assert!(
            frame.contains("\x1b[2mello\x1b[22m"),
            "the suggestion is dim: {frame:?}"
        );
        editor.step(accept.clone()).unwrap();
        assert_eq!(
            editor.line().buffer(),
            b"echo hello",
            "{accept:?} accepts the ghost text"
        );
    }
}

#[test]
fn ctrl_f_and_ctrl_e_move_before_they_accept() {
    // Away from the end of the line the two are plain movement: they carry
    // the cursor to where an accept would start, and only the next press
    // takes the suggestion.
    for (key, presses) in [(Key::Ctrl(0x05), 2), (Key::Ctrl(0x06), 3)] {
        let mut history = History::new();
        history.push(b"echo hello\n");
        let mut term = FakeTerminal::new(b"");
        let completer = StubCompleter::empty();
        let config = Config {
            color: true,
            history,
            ..plain_config()
        };
        let mut editor =
            Editor::with_config(&mut term, &completer, &baash_repl::NeverNeedsMore, config);
        for key in chars("echo h") {
            editor.step(key).unwrap();
        }
        editor.step(Key::Left).unwrap();
        editor.step(Key::Left).unwrap();
        // `Ctrl-E` moves to the end in one press; `Ctrl-F` needs one per byte
        // moved. The first press of either must not insert anything.
        editor.step(key.clone()).unwrap();
        assert_eq!(editor.line().buffer(), b"echo h", "{key:?} only moves");
        for _ in 1..presses {
            editor.step(key.clone()).unwrap();
        }
        assert_eq!(
            editor.line().buffer(),
            b"echo hello",
            "{key:?} at the end accepts the ghost text"
        );
    }
}

#[test]
fn without_colour_there_is_no_ghost_text_to_accept() {
    let mut history = History::new();
    history.push(b"echo hello\n");
    let mut term = FakeTerminal::new(b"");
    let completer = StubCompleter::empty();
    let config = Config {
        color: false,
        history,
        ..plain_config()
    };
    let mut editor =
        Editor::with_config(&mut term, &completer, &baash_repl::NeverNeedsMore, config);
    for key in chars("echo h") {
        editor.step(key).unwrap();
    }
    editor.step(Key::Right).unwrap();
    assert_eq!(editor.line().buffer(), b"echo h");
}

#[test]
fn escape_dismisses_the_ghost_and_typing_brings_it_back() {
    let mut history = History::new();
    history.push(b"echo hello\n");
    let mut term = FakeTerminal::new(b"");
    let sink = term.output_handle();
    let completer = StubCompleter::empty();
    let config = Config {
        color: true,
        history,
        ..plain_config()
    };
    let mut editor =
        Editor::with_config(&mut term, &completer, &baash_repl::NeverNeedsMore, config);
    for key in chars("echo h") {
        editor.step(key).unwrap();
    }
    assert!(output_text(&sink).contains("\x1b[2mello\x1b[22m"));

    // Escape removes it for this line; Right/End cannot accept text that is
    // not painted.
    clear_output(&sink);
    editor.step(Key::Esc).unwrap();
    assert!(
        !output_text(&sink).contains("\x1b[2m"),
        "the ghost is gone after Escape"
    );
    editor.step(Key::Right).unwrap();
    assert_eq!(editor.line().buffer(), b"echo h");

    // Typing refines the line and the suggestion returns.
    editor.step(Key::Char('e')).unwrap();
    assert_eq!(editor.line().buffer(), b"echo he");
    assert!(
        output_text(&sink).contains("\x1b[2mllo\x1b[22m"),
        "typing after Escape re-enables the suggestion"
    );
    editor.step(Key::End).unwrap();
    assert_eq!(editor.line().buffer(), b"echo hello");
}

#[test]
fn a_whitespace_only_line_has_no_ghost_text() {
    let mut history = History::new();
    history.push(b"echo hello\n");
    let mut term = FakeTerminal::new(b"");
    let sink = term.output_handle();
    let completer = StubCompleter::empty();
    let config = Config {
        color: true,
        history,
        ..plain_config()
    };
    let mut editor =
        Editor::with_config(&mut term, &completer, &baash_repl::NeverNeedsMore, config);
    clear_output(&sink);
    for key in chars("   ") {
        editor.step(key).unwrap();
    }
    assert!(!output_text(&sink).contains("\x1b[2m"));
}

#[test]
fn a_candidate_display_is_flattened_in_the_menu() {
    let completer = StubCompleter::new(vec![
        Candidate::new(b"bad\nname".to_vec(), baash_complete::CandKind::Path),
        Candidate::new(b"other".to_vec(), baash_complete::CandKind::Path),
    ]);
    let mut term = FakeTerminal::new(b"");
    let sink = term.output_handle();
    let mut editor = Editor::new(&mut term, &completer, &baash_repl::NeverNeedsMore);
    for key in chars("ls ba") {
        editor.step(key).unwrap();
    }
    clear_output(&sink);
    editor.step(Key::Tab).unwrap();
    let frame = output_text(&sink);
    assert!(
        frame.contains("bad name"),
        "a newline in a filename cannot break the menu: {frame:?}"
    );
    assert!(!frame.contains("bad\nname"));
}

#[test]
fn the_menu_cycles_with_tab_and_inserts_with_enter() {
    let completer = StubCompleter::words(&["alpha", "alpine"]);
    let mut term = FakeTerminal::new(b"");
    let mut editor = Editor::new(&mut term, &completer, &baash_repl::NeverNeedsMore);
    for key in chars("ls a") {
        editor.step(key).unwrap();
    }
    // Tab: common prefix; Tab: menu; Tab: next candidate; Enter: insert it.
    assert_eq!(editor.step(Key::Tab).unwrap(), Flow::Continue);
    assert_eq!(editor.line().buffer(), b"ls alp");
    assert_eq!(editor.step(Key::Tab).unwrap(), Flow::Continue);
    assert_eq!(editor.step(Key::Tab).unwrap(), Flow::Continue);
    assert_eq!(editor.step(Key::Enter).unwrap(), Flow::Continue);
    assert_eq!(editor.line().buffer(), b"ls alpine");
    // Shift-Tab walks backwards, wrapping to the same entry here.
    assert_eq!(editor.step(Key::BackTab).unwrap(), Flow::Continue);
    assert_eq!(editor.line().buffer(), b"ls alpine");
}

#[test]
fn a_menu_description_column_is_painted() {
    let completer = StubCompleter::new(vec![
        Candidate::new(b"checkout".to_vec(), baash_complete::CandKind::Command)
            .with_desc("switch branches"),
        Candidate::new(b"cherry-pick".to_vec(), baash_complete::CandKind::Command)
            .with_desc("apply a commit"),
    ]);
    let mut term = FakeTerminal::sized(b"", 80, 24);
    let sink = term.output_handle();
    let mut editor = Editor::new(&mut term, &completer, &baash_repl::NeverNeedsMore);
    for key in chars("git ch") {
        editor.step(key).unwrap();
    }
    editor.step(Key::Tab).unwrap(); // common prefix `che`
    clear_output(&sink);
    editor.step(Key::Tab).unwrap(); // menu
    let frame = output_text(&sink);
    assert!(frame.contains("> checkout"), "{frame:?}");
    assert!(
        frame.contains("switch branches"),
        "the description column is painted: {frame:?}"
    );
}

#[test]
fn a_non_utf8_candidate_is_inserted_byte_for_byte() {
    let completer = StubCompleter::new(vec![
        Candidate::new(b"caf\xe9".to_vec(), baash_complete::CandKind::Path),
        Candidate::new(b"cafe".to_vec(), baash_complete::CandKind::Path),
    ]);
    let (result, term) = run(
        FakeTerminal::new(b"ls caf\t\r\r"),
        &completer,
        &baash_repl::NeverNeedsMore,
        plain_config(),
    );
    // The byte is inserted as it is. Escaping it (`\\` + `0xE9`) would lex
    // back to the same argument, so it was never load-bearing: it only put a
    // visible backslash in front of a character the reader cannot identify
    // anyway. Bash inserts filename bytes verbatim, and so does this now —
    // including for the valid sequences that used to be split down the middle
    // (`ß` became `\\` `0xC3` `\\` `0x9F`).
    assert_eq!(result, ReadResult::Line(b"ls caf\xe9\n".to_vec()));
    // The display projection is UTF-8, so painting it cannot panic either.
    assert!(term.text().contains("caf"));
}

#[test]
fn the_menu_never_pushes_the_prompt_off_a_short_screen() {
    let completer = StubCompleter::words(&[
        "alpha",
        "alpine",
        "alt",
        "alto",
        "altoids",
        "alphabet",
        "alphanumeric",
    ]);
    let mut term = FakeTerminal::sized(b"", 20, 4);
    let mut editor = Editor::new(&mut term, &completer, &baash_repl::NeverNeedsMore);
    for key in chars("ls a") {
        editor.step(key).unwrap();
    }
    editor.step(Key::Tab).unwrap(); // prefix
    editor.step(Key::Tab).unwrap(); // menu
    let screen = Screen::of(&term);
    assert_eq!(screen.row(0), "$ ls al", "the prompt is still painted");
    assert_eq!(screen.scrolls(), 0, "the frame fits: {:?}", screen.lines());
    assert_eq!(
        screen.cursor(),
        (0, 7),
        "the cursor is on the line, not below the menu"
    );
}

/// Open the menu for `ls a`, which offers three candidates, and return the
/// screen.
fn open_menu(columns: u16, rows: u16) -> Screen {
    let completer = StubCompleter::words(&["alpha", "alpine", "alt"]);
    let mut term = FakeTerminal::sized(b"", columns, rows);
    let sink = term.output_handle();
    {
        let mut editor = Editor::new(&mut term, &completer, &baash_repl::NeverNeedsMore);
        for key in chars("ls a") {
            editor.step(key).unwrap();
        }
        editor.step(Key::Tab).unwrap(); // common prefix: `ls al`
        editor.step(Key::Tab).unwrap(); // the menu
    }
    let mut screen = Screen::new(columns, rows);
    screen.feed(&sink.borrow());
    screen
}

#[test]
fn the_menu_is_painted_below_the_line_and_the_cursor_stays_on_it() {
    // Ten columns is one menu column, so each candidate gets its own row.
    let screen = open_menu(10, 12);
    assert_eq!(
        screen.used_lines(),
        ["$ ls al", "> alpha", "  alpine", "  alt"],
        "the menu sits under the line, one candidate per row"
    );
    assert_eq!(
        screen.cursor(),
        (0, 7),
        "the cursor is where the user is typing"
    );
}

#[test]
fn typing_after_the_menu_repaints_over_it_instead_of_below_it() {
    let completer = StubCompleter::words(&["alpha", "alpine", "alt"]);
    let mut term = FakeTerminal::sized(b"", 20, 12);
    let sink = term.output_handle();
    {
        let mut editor = Editor::new(&mut term, &completer, &baash_repl::NeverNeedsMore);
        for key in chars("ls a") {
            editor.step(key).unwrap();
        }
        editor.step(Key::Tab).unwrap(); // common prefix
        editor.step(Key::Tab).unwrap(); // the menu
        editor.step(Key::Char('t')).unwrap();
    }
    let mut screen = Screen::new(20, 12);
    screen.feed(&sink.borrow());
    assert_eq!(
        screen.occurrences("$ "),
        1,
        "the prompt is painted once, not once per keystroke: {:?}",
        screen.lines()
    );
    assert_eq!(screen.used_lines(), ["$ ls alt"], "the menu was cleared");
    assert_eq!(screen.cursor(), (0, 8));
}

#[test]
fn a_long_line_leaves_the_menu_only_the_rows_below_it() {
    // Three rows of line on a six-row screen: three rows are left for the
    // menu, and the frame stays put instead of scrolling its own top away.
    let completer = StubCompleter::words(&["alpha", "alpine", "alt"]);
    let mut term = FakeTerminal::sized(b"", 20, 6);
    let mut editor = Editor::new(&mut term, &completer, &baash_repl::NeverNeedsMore);
    let mut line = chars(&"a".repeat(38));
    line.extend(chars(" ls a"));
    for key in line {
        editor.step(key).unwrap();
    }
    editor.step(Key::Tab).unwrap(); // common prefix
    editor.step(Key::Tab).unwrap(); // menu
    let screen = Screen::of(&term);
    assert_eq!(
        screen.scrolls(),
        0,
        "nothing scrolled: {:?}",
        screen.lines()
    );
    assert!(
        screen.row(0).starts_with("$ aaaa"),
        "the prompt is on the first row: {:?}",
        screen.lines()
    );
    assert!(
        screen.row(3).starts_with("> alpha"),
        "the menu starts right under the line: {:?}",
        screen.lines()
    );
    assert_eq!(
        screen.cursor(),
        (2, 6),
        "the cursor is at the end of the line, on its last row"
    );
}

#[test]
fn a_suggestion_that_would_not_fit_is_not_painted() {
    let mut term = FakeTerminal::sized(b"", 20, 2);
    let sink = term.output_handle();
    let config = Config {
        color: true,
        history: seeded_history(&["echo one two three four five six seven eight"]),
        ..plain_config()
    };
    let completer = StubCompleter::empty();
    {
        let mut editor =
            Editor::with_config(&mut term, &completer, &baash_repl::NeverNeedsMore, config);
        for key in chars("echo o") {
            editor.step(key).unwrap();
        }
    }
    let mut screen = Screen::new(20, 2);
    screen.feed(&sink.borrow());
    assert_eq!(
        screen.lines(),
        ["$ echo o", ""],
        "the suggestion is decoration; the line keeps the screen"
    );
}

#[test]
fn the_terminal_size_is_respected_when_it_is_unknown() {
    let mut term = FakeTerminal::sized(b"", 0, 0);
    let sink = term.output_handle();
    assert_eq!(
        term.size,
        Size {
            columns: 0,
            rows: 0
        }
    );
    let completer = StubCompleter::empty();
    let mut editor = Editor::new(&mut term, &completer, &baash_repl::NeverNeedsMore);
    editor.step(Key::Char('x')).unwrap();
    assert!(output_text(&sink).contains("$ x"));
}

#[test]
fn a_session_leaves_raw_mode_balanced_after_every_line() {
    let driven: Session = session(
        FakeTerminal::new(b"echo one\recho two\r\x04"),
        &StubCompleter::empty(),
        &baash_repl::NeverNeedsMore,
        plain_config(),
        4,
    );
    assert!(driven.term.raw_balanced());
    assert_eq!(driven.results.len(), 3);
    assert!(matches!(driven.results[2], ReadResult::Eof));
}

// ---------------------------------------------------------------------------
// Ctrl-R reverse incremental search
// ---------------------------------------------------------------------------

/// The keys a search uses, spelled out so the tests read like the interaction.
const CTRL_R: Key = Key::Ctrl(0x12);
const CTRL_G: Key = Key::Ctrl(0x07);

/// `Ctrl-R` followed by `text`.
fn search(text: &str) -> Vec<Key> {
    let mut keys = vec![CTRL_R];
    keys.extend(chars(text));
    keys
}

#[test]
fn ctrl_r_shows_the_newest_entry_and_typing_narrows() {
    let entries = ["echo hello world", "echo hi there"];
    let (buffer, _, _) = drive(&entries, &[CTRL_R]);
    assert_eq!(buffer, b"echo hi there", "the newest entry is shown");

    let (buffer, cursor, output) = drive(&entries, &search("hello"));
    assert_eq!(buffer, b"echo hello world", "typing narrows the match");
    assert_eq!(cursor, 5, "the cursor is at the start of the match");
    assert!(
        output.contains("(reverse-i-search)`hello': echo hello world"),
        "the search prompt names the query and the match: {output:?}"
    );
}

#[test]
fn ctrl_r_again_walks_to_the_next_older_match() {
    let entries = ["a one", "a two", "m three"];
    let (buffer, _, _) = drive(&entries, &search("a"));
    assert_eq!(buffer, b"a two");
    let (buffer, _, _) = drive(&entries, &[CTRL_R, Key::Char('a'), CTRL_R]);
    assert_eq!(buffer, b"a one", "a second Ctrl-R finds the older match");
}

#[test]
fn a_query_with_no_older_match_reports_failure_and_keeps_the_last_match() {
    let entries = ["first alpha", "second alpha"];
    let (buffer, _, output) = drive(&entries, &[CTRL_R, Key::Char('a'), CTRL_R, CTRL_R]);
    assert_eq!(buffer, b"first alpha", "the last match stays on the line");
    assert!(
        output.contains("(failed reverse-i-search)`a': first alpha"),
        "the prompt says the search failed: {output:?}"
    );
}

#[test]
fn enter_accepts_the_match_and_executes_it() {
    let (result, _) = run(
        FakeTerminal::new(b"\x12hi\r"),
        &StubCompleter::empty(),
        &baash_repl::NeverNeedsMore,
        config_with_history(&["echo hello", "echo hi there"]),
    );
    assert_eq!(result, ReadResult::Line(b"echo hi there\n".to_vec()));
}

#[test]
fn escape_restores_the_line_the_search_started_from() {
    let mut keys = chars("draft");
    keys.extend(search("hi"));
    keys.push(Key::Esc);
    let (buffer, cursor, _) = drive(&["echo hi there"], &keys);
    assert_eq!(buffer, b"draft", "the original line comes back");
    assert_eq!(cursor, 5);
    assert!(
        !drive(&["echo hi there"], &keys)
            .2
            .contains("reverse-i-search`hi': echo hi there\r")
    );
}

#[test]
fn ctrl_g_also_aborts_the_search() {
    let mut keys = chars("draft");
    keys.extend(search("hi"));
    keys.push(CTRL_G);
    let (buffer, cursor, _) = drive(&["echo hi there"], &keys);
    assert_eq!((buffer.as_slice(), cursor), (b"draft".as_slice(), 5));
}

#[test]
fn ctrl_c_abandons_the_whole_line() {
    let (result, term) = run(
        FakeTerminal::new(b"\x12hi\x03"),
        &StubCompleter::empty(),
        &baash_repl::NeverNeedsMore,
        config_with_history(&["echo hi there"]),
    );
    assert_eq!(result, ReadResult::Interrupted);
    assert!(term.text().contains("^C"));
}

#[test]
fn a_query_that_matches_nothing_says_so_and_does_not_change_the_line() {
    let mut keys = search("zzz");
    keys.push(Key::Esc);
    let (buffer, cursor, output) = drive(&["echo hi there"], &keys);
    assert_eq!(
        (buffer.as_slice(), cursor),
        (b"".as_slice(), 0),
        "line unchanged"
    );
    assert!(
        output.contains("(failed reverse-i-search)`zzz': echo hi there"),
        "the failing spelling is used: {output:?}"
    );
}

#[test]
fn empty_history_does_not_panic_and_leaves_the_line_alone() {
    // Ctrl-R, Ctrl-G with nothing to search.
    let mut keys = chars("keep");
    keys.push(CTRL_R);
    keys.push(CTRL_G);
    let (buffer, cursor, output) = drive(&[], &keys);
    assert_eq!((buffer.as_slice(), cursor), (b"keep".as_slice(), 4));
    assert!(
        output.contains("(failed reverse-i-search)`': keep"),
        "{output:?}"
    );

    // ... and Enter on an empty ring runs what was typed.
    let (result, _) = run(
        FakeTerminal::new(b"\x12\r"),
        &StubCompleter::empty(),
        &baash_repl::NeverNeedsMore,
        config_with_history(&[]),
    );
    assert_eq!(result, ReadResult::Line(b"\n".to_vec()));
}

#[test]
fn backspace_edits_the_query_and_searches_again() {
    let entries = ["echo hi there"];
    let mut keys = search("hiz");
    keys.push(Key::Backspace);
    let (buffer, _, output) = drive(&entries, &keys);
    assert_eq!(buffer, b"echo hi there");
    assert!(
        output.contains("(reverse-i-search)`hi': echo hi there"),
        "{output:?}"
    );
}

#[test]
fn left_and_right_accept_the_match_and_put_the_cursor_on_it() {
    // `wor` matches at index 11 of "echo hello world"; `->` puts the cursor at
    // 12, so the typed byte lands between `w` and `o`.
    let mut keys = search("wor");
    keys.push(Key::Right);
    keys.push(Key::Char('X'));
    keys.push(Key::Enter);
    let (result, _) = run(
        FakeTerminal::new(b"\x12wor\x1b[CX\r"),
        &StubCompleter::empty(),
        &baash_repl::NeverNeedsMore,
        config_with_history(&["echo hello world"]),
    );
    assert_eq!(result, ReadResult::Line(b"echo hello wXorld\n".to_vec()));

    let (result, _) = run(
        FakeTerminal::new(b"\x12wor\x1b[DY\r"),
        &StubCompleter::empty(),
        &baash_repl::NeverNeedsMore,
        config_with_history(&["echo hello world"]),
    );
    assert_eq!(result, ReadResult::Line(b"echo helloY world\n".to_vec()));

    // Right and then Left, which is where bash's own cursor ends up after a
    // search: on the match, ready to type in front of it.
    let (result, _) = run(
        FakeTerminal::new(b"\x12wor\x1b[C\x1b[DY\r"),
        &StubCompleter::empty(),
        &baash_repl::NeverNeedsMore,
        config_with_history(&["echo hello world"]),
    );
    assert_eq!(result, ReadResult::Line(b"echo hello Yworld\n".to_vec()));
    let _ = keys;
}

/// A history entry that spans lines is painted as those lines.
///
/// Recalling one used to show `for i in 1 2; do^Jecho $i^Jdone`: the newlines
/// were caret notation, so a command that runs read as something the shell
/// would reject. They are row breaks now, and the cursor sits at the end of the
/// last row.
#[test]
fn a_multiline_history_entry_is_painted_as_lines() {
    let mut history = History::new();
    history.push(b"for i in 1 2; do\necho $i\ndone\n");
    let mut term = FakeTerminal::sized(b"", 80, 24);
    let completer = StubCompleter::empty();
    let config = Config {
        history,
        ..plain_config()
    };
    let sink = term.output_handle();
    let mut editor =
        Editor::with_config(&mut term, &completer, &baash_repl::NeverNeedsMore, config);
    clear_output(&sink);
    editor.step(Key::Up).unwrap();
    let frame = output_text(&sink);
    assert!(
        !frame.contains("^J"),
        "no caret notation in a recalled line: {frame:?}"
    );
    assert!(
        frame.contains("do\r\necho $i\r\ndone"),
        "one row per line: {frame:?}"
    );
    // The ring drops the entry's trailing newline; the ones inside it are the
    // command's own and are what the shell will run.
    assert_eq!(
        editor.line().buffer(),
        b"for i in 1 2; do\necho $i\ndone",
        "the buffer keeps the newlines it will run with"
    );
}

/// A shell-backed completion hook, as `complete -F` needs.
struct FuncHook {
    /// Answer only for this line, so the fallback path stays visible.
    line: Vec<u8>,
    candidates: Vec<&'static str>,
    /// What the function prints, which bash paints above the prompt.
    output: Vec<u8>,
    seen: Vec<Vec<u8>>,
}

impl baash_repl::ShellCompletion for FuncHook {
    fn complete(&mut self, line: &[u8], cursor: usize) -> Option<baash_repl::ShellMenu> {
        self.seen.push(line.to_vec());
        if line != self.line.as_slice() {
            return None;
        }
        let engine = baash_complete::CompletionEngine::new();
        let ctx = engine.context(line, cursor);
        let candidates = self
            .candidates
            .iter()
            .map(|word| {
                baash_complete::Candidate::new(word.as_bytes(), baash_complete::CandKind::Other)
            })
            .collect();
        Some(baash_repl::ShellMenu {
            menu: baash_complete::Menu::new(ctx, candidates),
            output: self.output.clone(),
        })
    }
}

/// `complete -F` runs shell code: the editor asks the shell before the data
/// completer, and a hook that declines leaves the data path intact.
#[test]
fn a_shell_completion_hook_answers_before_the_data_completer() {
    let mut hook = FuncHook {
        line: b"cmd al".to_vec(),
        candidates: vec!["shell-alpha"],
        output: Vec::new(),
        seen: Vec::new(),
    };
    let mut term = FakeTerminal::new(b"cmd al\t\r");
    let completer = StubCompleter::words(&["data-alpha"]);
    let result = {
        let mut editor = Editor::with_config(
            &mut term,
            &completer,
            &baash_repl::NeverNeedsMore,
            plain_config(),
        );
        editor
            .read_line_with_shell(&mut baash_repl::NeverRefreshes, Some(&mut hook))
            .expect("read_line")
    };
    assert_eq!(result, ReadResult::Line(b"cmd shell-alpha \n".to_vec()));
    assert_eq!(hook.seen, vec![b"cmd al".to_vec()], "the hook saw the line");
    assert!(!term.text().contains("data-alpha"));

    // Declining falls back to the data completer.
    let mut declining = FuncHook {
        line: b"other".to_vec(),
        candidates: vec!["shell-alpha"],
        output: Vec::new(),
        seen: Vec::new(),
    };
    let mut term = FakeTerminal::new(b"cmd al\t\r");
    let result = {
        let mut editor = Editor::with_config(
            &mut term,
            &completer,
            &baash_repl::NeverNeedsMore,
            plain_config(),
        );
        editor
            .read_line_with_shell(&mut baash_repl::NeverRefreshes, Some(&mut declining))
            .expect("read_line")
    };
    assert_eq!(result, ReadResult::Line(b"cmd data-alpha \n".to_vec()));
    assert_eq!(declining.seen, vec![b"cmd al".to_vec()]);
}

/// A completion function's `echo` is painted above the prompt, and the line is
/// repainted under it — bash's order, so a warning does not garble the frame.
#[test]
fn a_completion_functions_output_is_painted_above_the_prompt() {
    let mut hook = FuncHook {
        line: b"cmd al".to_vec(),
        candidates: vec!["shell-alpha"],
        output: b"warning: careful\n".to_vec(),
        seen: Vec::new(),
    };
    let mut term = FakeTerminal::new(b"cmd al\t\r");
    let completer = StubCompleter::empty();
    let result = {
        let mut editor = Editor::with_config(
            &mut term,
            &completer,
            &baash_repl::NeverNeedsMore,
            plain_config(),
        );
        editor
            .read_line_with_shell(&mut baash_repl::NeverRefreshes, Some(&mut hook))
            .expect("read_line")
    };
    assert_eq!(result, ReadResult::Line(b"cmd shell-alpha \n".to_vec()));
    let frame = term.text();
    let note = frame
        .find("warning: careful")
        .expect("the output is painted");
    let repainted = frame[note..]
        .find("cmd shell-alpha")
        .expect("the line is repainted below the output");
    assert!(
        repainted > 0,
        "the note comes first, then the repainted line: {frame:?}"
    );
}
