//! The `baashzf` built-in: pick one line out of a list, fuzzily.
//!
//! ```text
//! ls | baashzf            → prints the chosen file name
//! printf 'a\nb\n' | baashzf
//! ```
//!
//! This is the picker `Ctrl-R` opens (`baash-picker`), applied to whatever
//! arrives on standard input: type to fuzzy-filter, `Up`/`Down` to choose,
//! `Enter` to print the selected line.
//!
//! # What it deliberately does not do
//!
//! This is the basic shape of `fzf`, not a reimplementation of it: no preview
//! window, no multi-select, no `--bind`, no `--height`, no
//! `$FZF_DEFAULT_COMMAND`. What it has is what the shell already had — the
//! matcher and the screen — plus the line protocol: a list in, one line out.
//!
//! The whole list is read before the screen opens, so a very long list is held
//! in memory rather than streamed. That is the one place this is simpler than
//! `fzf`, and it is also why nothing touches the list while the picker is up:
//! the keystroke path only ever filters what is already in hand.
//!
//! # Exit status
//!
//! `0` printed a line, `1` chose nothing (no match, or an empty list), `2` could
//! not run here (no terminal, a dumb one, standard input is the terminal), `130`
//! was cancelled.

use std::time::Duration;

use baash_picker::{Decoder, Outcome, Picker};
use baash_sys::Tty;

use super::{codes, diagnose, usage};
use crate::{ShellHost, Status};

const BAASHZF_USAGE: &str = "usage: baashzf";

/// How long a read waits before a partial key is given up on.
///
/// A terminal sends one key as one write, so this only has to out-last the gap
/// between the bytes of a key; it is also how long a bare `Esc` waits for the
/// `[` that never comes, which is the part a user feels. 25 ms is below the
/// perceptible threshold and far above any real inter-byte gap.
const KEY_WAIT: Duration = Duration::from_millis(25);

/// What the interaction ended with.
enum Picked {
    /// This line was chosen.
    Line(Vec<u8>),
    /// `Enter` with nothing selected.
    Nothing,
    /// `Esc` or `Ctrl-C`.
    Cancelled,
}

/// `baashzf`.
pub(super) fn bi_baashzf(host: &mut dyn ShellHost, argv: &[&[u8]]) -> Status {
    if argv[1..]
        .iter()
        .any(|argument| matches!(*argument, b"-h" | b"--help"))
    {
        help(host);
        return Status::SUCCESS;
    }
    if argv.len() > 1 {
        return usage(host, argv, BAASHZF_USAGE);
    }
    // A picker over the terminal it is reading keys from would take the keys
    // as candidates. `fzf` runs a file walker in this case; this one says so
    // rather than guessing at what the user meant.
    if baash_sys::is_tty(0) {
        return usage(host, argv, "baashzf: standard input is a terminal");
    }
    if baash_sys::is_dumb_terminal() {
        return usage(
            host,
            argv,
            "baashzf: needs a terminal that understands escape sequences",
        );
    }
    let input = match baash_sys::read_all(0) {
        Ok(input) => input,
        Err(error) => {
            diagnose(host, codes::USAGE, format!("baashzf: {error}"));
            return Status::FAILURE;
        }
    };
    let mut picker = Picker::new(b"", lines_of(&input));
    if picker.is_empty() {
        return Status::FAILURE;
    }
    let mut tty = match Tty::open() {
        Ok(tty) => tty,
        Err(error) => {
            diagnose(
                host,
                codes::USAGE,
                format!("baashzf: cannot open the terminal: {error}"),
            );
            return Status::USAGE;
        }
    };
    // `NO_COLOR` is the one colour switch a program that is not the line
    // editor can see; the shell's own theme is decided above this layer.
    let color = std::env::var_os("NO_COLOR").is_none();
    let picked = interact(&mut tty, &mut picker, color);
    // The terminal mode is restored by dropping the `Tty`, and that has to
    // happen before anything is printed: while raw mode is on, a `\n` reaches
    // the terminal without the carriage return the tty driver would add, and
    // the chosen line would be printed as a stair-step.
    drop(tty);
    match picked {
        Ok(Picked::Line(line)) => {
            let mut line = line;
            line.push(b'\n');
            host.write_out(&line)
        }
        Ok(Picked::Nothing) => Status::FAILURE,
        Ok(Picked::Cancelled) => Status(130),
        Err(error) => {
            diagnose(host, codes::USAGE, format!("baashzf: {error}"));
            Status::USAGE
        }
    }
}

/// Take the screen, run the picker, and give the screen back.
///
/// The leave sequence is written however the loop ends — a cancel, an I/O
/// error, a terminal that went away — because a shell prompt painted onto the
/// picker's screen looks like a corrupted terminal, and no exit status is worth
/// that.
fn interact(tty: &mut Tty, picker: &mut Picker, color: bool) -> Result<Picked, std::io::Error> {
    tty.write(baash_picker::ENTER_SCREEN)?;
    let picked = event_loop(tty, picker, color);
    let left = tty.write(baash_picker::LEAVE_SCREEN);
    let picked = picked?;
    left?;
    Ok(picked)
}

/// Paint, read, filter — until a key chooses something.
///
/// The screen is painted when it is out of date and not otherwise: once at the
/// start, after every key, and after a resize (which the read deadline notices
/// within [`KEY_WAIT`]). Repainting on the deadline as well would put a frame on
/// the wire forty times a second while the picker sat idle, which costs a
/// terminal redraw and delays the first frame *after* a key by a whole deadline.
fn event_loop(tty: &mut Tty, picker: &mut Picker, color: bool) -> Result<Picked, std::io::Error> {
    let mut decoder = Decoder::new();
    let mut bytes = Vec::new();
    let mut actions = Vec::new();
    let mut painted: Option<(u16, u16)> = None;
    loop {
        let size = tty.size().unwrap_or((80, 24));
        if painted != Some(size) {
            tty.write(&picker.frame(size.0, size.1, color))?;
            painted = Some(size);
        }
        bytes.clear();
        actions.clear();
        match tty.read(KEY_WAIT, &mut bytes)? {
            // Nothing arrived: a partial key has run out of time, and a lone
            // `Esc` is an `Esc`.
            None => actions.extend(decoder.settle()),
            // The terminal went away under us (a closed pty, a killed
            // multiplexer): there is no screen left to paint or to restore.
            Some(0) => return Ok(Picked::Nothing),
            Some(_) => actions.extend(decoder.feed(&bytes)),
        }
        for action in actions.drain(..) {
            match picker.act(action) {
                // The query or the selection changed: the screen is stale.
                Outcome::Continue => painted = None,
                Outcome::Accept => {
                    // A chosen line is the buffer's bytes, not the display
                    // string the screen shows: the caller gets what came in.
                    return Ok(match picker.selected() {
                        Some(line) => Picked::Line(line.to_vec()),
                        None => Picked::Nothing,
                    });
                }
                Outcome::Cancel => return Ok(Picked::Cancelled),
            }
        }
    }
}

/// Split `input` into the candidates the picker will show.
///
/// One line each; a trailing newline does not make an empty last candidate
/// (`printf 'a\nb\n'` is two, not three), and a `\r` before the newline is
/// dropped so a list produced on Windows behaves like one produced on Unix.
/// Blank lines *inside* the list are candidates: they were in the input.
fn lines_of(input: &[u8]) -> Vec<Vec<u8>> {
    let mut lines = Vec::new();
    let mut rest = input;
    while let Some(at) = rest.iter().position(|byte| *byte == b'\n') {
        let line = &rest[..at];
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        lines.push(line.to_vec());
        rest = &rest[at + 1..];
    }
    if !rest.is_empty() {
        let line = rest.strip_suffix(b"\r").unwrap_or(rest);
        lines.push(line.to_vec());
    }
    lines
}

/// The `--help` text.
fn help(host: &mut dyn ShellHost) {
    let text = "\
usage: baashzf

Pick one line from standard input, fuzzily.

  list | baashzf          choose from the list on standard input

Keys: type to filter, Up/Down to move, Enter to choose, Esc to cancel.
Prints the chosen line. Status: 0 chosen, 1 nothing chosen, 2 cannot run,
130 cancelled.
";
    let _ = host.write_out(text.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(input: &[u8]) -> Vec<String> {
        lines_of(input)
            .iter()
            .map(|line| String::from_utf8_lossy(line).into_owned())
            .collect()
    }

    #[test]
    fn a_trailing_newline_does_not_add_an_empty_candidate() {
        assert_eq!(lines(b"a\nb\n"), ["a", "b"]);
        assert_eq!(lines(b"a\nb"), ["a", "b"]);
        assert_eq!(lines(b""), Vec::<String>::new());
        assert_eq!(lines(b"\n"), [""]);
    }

    #[test]
    fn a_carriage_return_is_not_part_of_the_line() {
        assert_eq!(lines(b"a\r\nb\r\n"), ["a", "b"]);
        assert_eq!(lines(b"a\r"), ["a"]);
        // Only *before* the newline: a `\r` in the middle is the line's own.
        assert_eq!(lines(b"a\rb\n"), ["a\rb"]);
    }

    #[test]
    fn blank_lines_inside_the_list_are_candidates() {
        assert_eq!(lines(b"a\n\nb\n"), ["a", "", "b"]);
    }

    #[test]
    fn a_line_is_bytes_not_utf8() {
        assert_eq!(lines_of(b"\xff\xfe\n"), vec![b"\xff\xfe".to_vec()]);
    }
}
