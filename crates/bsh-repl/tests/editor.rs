//! Behavioural tests for the line editor, driven through a fake terminal.

mod common;

use std::io::Write;

use bsh_complete::Candidate;
use bsh_repl::{
    Config, Editor, Flow, History, HistoryControl, Key, Prompt, ReadResult, Size, Theme,
};

use common::{
    FakeTerminal, Session, StubCompleter, StubSyntax, apply_keys, chars, clear_output,
    config_with_history, drive, has_sgr, last_frame, output_text, plain_config, run, session,
};

#[test]
fn typing_a_line_and_submitting_returns_it_newline_terminated() {
    let (result, term) = run(
        FakeTerminal::new(b"echo hi\r"),
        &StubCompleter::empty(),
        &bsh_repl::NeverNeedsMore,
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
        &bsh_repl::NeverNeedsMore,
        plain_config(),
    );
    assert_eq!(result, ReadResult::Line(b"acb\n".to_vec()));

    // Home/End and Delete.
    let (result, _) = run(
        FakeTerminal::new(b"bc\x1b[H a\x1b[F!\r"),
        &StubCompleter::empty(),
        &bsh_repl::NeverNeedsMore,
        plain_config(),
    );
    assert_eq!(result, ReadResult::Line(b" abc!\n".to_vec()));

    let (result, _) = run(
        FakeTerminal::new(b"abc\x1b[D\x1b[D\x1b[3~\r"),
        &StubCompleter::empty(),
        &bsh_repl::NeverNeedsMore,
        plain_config(),
    );
    assert_eq!(result, ReadResult::Line(b"ac\n".to_vec()));
}

#[test]
fn alt_b_and_alt_f_move_by_word() {
    let (result, _) = run(
        FakeTerminal::new(b"git commit\x1bbX\r"),
        &StubCompleter::empty(),
        &bsh_repl::NeverNeedsMore,
        plain_config(),
    );
    assert_eq!(result, ReadResult::Line(b"git Xcommit\n".to_vec()));
}

#[test]
fn a_truncated_escape_sequence_is_not_inserted_as_text() {
    let (result, _) = run(
        FakeTerminal::new(b"ab\x1b[3\r"),
        &StubCompleter::empty(),
        &bsh_repl::NeverNeedsMore,
        plain_config(),
    );
    assert_eq!(result, ReadResult::Line(b"ab\n".to_vec()));
}

#[test]
fn a_bracketed_paste_is_inserted_verbatim() {
    let (result, _) = run(
        FakeTerminal::new(b"\x1b[200~echo \x1b[A hi\nsecond\x1b[201~\r"),
        &StubCompleter::empty(),
        &bsh_repl::NeverNeedsMore,
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
        &bsh_repl::NeverNeedsMore,
        plain_config(),
    );
    assert_eq!(result, ReadResult::Eof);
    assert!(term.raw_balanced());

    let (result, _) = run(
        FakeTerminal::new(b"ab\x1b[D\x04\r"),
        &StubCompleter::empty(),
        &bsh_repl::NeverNeedsMore,
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
        &bsh_repl::NeverNeedsMore,
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
        &bsh_repl::NeverNeedsMore,
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
    let mut editor = Editor::with_config(&mut term, &completer, &bsh_repl::NeverNeedsMore, config);
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
        frame.ends_with("\x1b[1B\r\x1b[4C"),
        "cursor at source offset 2 is row 0 column 4, so it moves up one row: {frame:?}"
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
    let mut editor = Editor::with_config(&mut term, &completer, &bsh_repl::NeverNeedsMore, config);
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
    let state = bsh_repl::PromptState {
        user: b"ada".to_vec(),
        root: false,
        ..bsh_repl::PromptState::default()
    };
    let config = Config {
        prompt: Prompt::from_template(b"\\u \\$ ", b"> ", &state),
        ..plain_config()
    };
    let mut editor = Editor::new(&mut term, &completer, &bsh_repl::NeverNeedsMore);
    editor.set_prompt(config.prompt.clone());
    editor.step(Key::Char('x')).unwrap();
    assert!(term.text().contains("ada $ x"));

    let mut term = FakeTerminal::new(b"");
    let root = bsh_repl::PromptState {
        root: true,
        ..bsh_repl::PromptState::default()
    };
    let config = Config {
        prompt: Prompt::from_template(b"\\$ ", b"> ", &root),
        ..plain_config()
    };
    let mut editor = Editor::with_config(&mut term, &completer, &bsh_repl::NeverNeedsMore, config);
    editor.step(Key::Char('x')).unwrap();
    assert!(term.text().contains("# x"));
}

#[test]
fn a_prompt_rendered_for_a_directory_shows_it() {
    let mut term = FakeTerminal::new(b"");
    let completer = StubCompleter::empty();
    let state = bsh_repl::PromptState {
        cwd: b"/tmp/place".to_vec(),
        ..bsh_repl::PromptState::default()
    };
    let config = Config {
        prompt: Prompt::from_template(b"\\w$ ", b"> ", &state),
        ..plain_config()
    };
    let mut editor = Editor::with_config(&mut term, &completer, &bsh_repl::NeverNeedsMore, config);
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
    let mut editor = Editor::with_config(&mut term, &completer, &bsh_repl::NeverNeedsMore, config);
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
        &bsh_repl::NeverNeedsMore,
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
fn duplicates_follow_histcontrol() {
    // bash keeps duplicates unless HISTCONTROL says otherwise; the old ring
    // dropped them unconditionally.
    let driven = session(
        FakeTerminal::new(b"ls\rls\r"),
        &StubCompleter::empty(),
        &bsh_repl::NeverNeedsMore,
        plain_config(),
        3,
    );
    assert_eq!(
        driven.history,
        vec![b"ls".to_vec(), b"ls".to_vec()],
        "no HISTCONTROL means duplicates are kept"
    );

    let mut history = History::new();
    history.set_control(HistoryControl::parse(b"ignoredups"));
    let driven = session(
        FakeTerminal::new(b"ls\rls\r"),
        &StubCompleter::empty(),
        &bsh_repl::NeverNeedsMore,
        Config {
            history,
            ..plain_config()
        },
        3,
    );
    assert_eq!(driven.history, vec![b"ls".to_vec()]);
}

#[test]
fn history_persists_to_histfile_on_finish() {
    let path = std::env::temp_dir().join(format!("bsh-repl-editor-hist-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(b"from-another-shell").unwrap();
    }
    let mut term = FakeTerminal::new(b"echo new\r");
    let completer = StubCompleter::empty();
    let config = Config {
        history: History::load(&path),
        ..plain_config()
    };
    {
        let mut editor =
            Editor::with_config(&mut term, &completer, &bsh_repl::NeverNeedsMore, config);
        assert_eq!(
            editor.read_line().unwrap(),
            ReadResult::Line(b"echo new\n".to_vec())
        );
        editor.finish().unwrap();
    }
    let contents = std::fs::read(&path).unwrap();
    assert_eq!(contents, b"from-another-shell\necho new\n");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_single_candidate_is_inserted_through_the_menu() {
    let completer = StubCompleter::words(&["My File"]);
    let (result, _) = run(
        FakeTerminal::new(b"ls My\t\r"),
        &completer,
        &bsh_repl::NeverNeedsMore,
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
            Editor::with_config(&mut term, &completer, &bsh_repl::NeverNeedsMore, config);
        for key in [
            Key::Char('l'),
            Key::Char('s'),
            Key::Char(' '),
            Key::Char('a'),
        ] {
            editor.step(key).unwrap();
        }
        clear_output(&sink);
        assert_eq!(editor.step(Key::Tab).unwrap(), Flow::Continue);
        let frame = output_text(&sink);
        assert!(frame.contains("> alpha"), "the menu is painted: {frame:?}");
        assert!(frame.contains("  alpine"));
        // Down moves the selection; Enter inserts it.
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
        FakeTerminal::new(b"ls a\t\t\r"),
        &completer,
        &bsh_repl::NeverNeedsMore,
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
    let mut editor = Editor::with_config(&mut term, &completer, &bsh_repl::NeverNeedsMore, config);
    for key in [
        Key::Char('l'),
        Key::Char('s'),
        Key::Char(' '),
        Key::Char('a'),
    ] {
        editor.step(key).unwrap();
    }
    editor.step(Key::Tab).unwrap();
    assert_eq!(editor.step(Key::Esc).unwrap(), Flow::Continue);
    clear_output(&sink);
    editor.step(Key::Char('!')).unwrap();
    assert_eq!(editor.line().buffer(), b"ls a!");
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
        &bsh_repl::NeverNeedsMore,
        plain_config(),
    );
    assert_eq!(result, ReadResult::Line(b"ls zz\n".to_vec()));
    assert!(term.text().contains('\u{7}'), "a bell is the no-op signal");
}

#[test]
fn ghost_text_completes_from_history_and_is_accepted_by_right_or_end() {
    for accept in [Key::Right, Key::End] {
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
            Editor::with_config(&mut term, &completer, &bsh_repl::NeverNeedsMore, config);
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
    let mut editor = Editor::with_config(&mut term, &completer, &bsh_repl::NeverNeedsMore, config);
    for key in chars("echo h") {
        editor.step(key).unwrap();
    }
    editor.step(Key::Right).unwrap();
    assert_eq!(editor.line().buffer(), b"echo h");
}

#[test]
fn a_candidate_display_is_flattened_in_the_menu() {
    let completer = StubCompleter::new(vec![
        Candidate::new(b"bad\nname".to_vec(), bsh_complete::CandKind::Path),
        Candidate::new(b"other".to_vec(), bsh_complete::CandKind::Path),
    ]);
    let mut term = FakeTerminal::new(b"");
    let sink = term.output_handle();
    let mut editor = Editor::new(&mut term, &completer, &bsh_repl::NeverNeedsMore);
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
    let mut editor = Editor::new(&mut term, &completer, &bsh_repl::NeverNeedsMore);
    editor.step(Key::Char('x')).unwrap();
    assert!(output_text(&sink).contains("$ x"));
}

#[test]
fn a_session_leaves_raw_mode_balanced_after_every_line() {
    let driven: Session = session(
        FakeTerminal::new(b"echo one\recho two\r\x04"),
        &StubCompleter::empty(),
        &bsh_repl::NeverNeedsMore,
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
        &bsh_repl::NeverNeedsMore,
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
        &bsh_repl::NeverNeedsMore,
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
        &bsh_repl::NeverNeedsMore,
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
        &bsh_repl::NeverNeedsMore,
        config_with_history(&["echo hello world"]),
    );
    assert_eq!(result, ReadResult::Line(b"echo hello wXorld\n".to_vec()));

    let (result, _) = run(
        FakeTerminal::new(b"\x12wor\x1b[DY\r"),
        &StubCompleter::empty(),
        &bsh_repl::NeverNeedsMore,
        config_with_history(&["echo hello world"]),
    );
    assert_eq!(result, ReadResult::Line(b"echo helloY world\n".to_vec()));

    // Right and then Left, which is where bash's own cursor ends up after a
    // search: on the match, ready to type in front of it.
    let (result, _) = run(
        FakeTerminal::new(b"\x12wor\x1b[C\x1b[DY\r"),
        &StubCompleter::empty(),
        &bsh_repl::NeverNeedsMore,
        config_with_history(&["echo hello world"]),
    );
    assert_eq!(result, ReadResult::Line(b"echo hello Yworld\n".to_vec()));
    let _ = keys;
}
