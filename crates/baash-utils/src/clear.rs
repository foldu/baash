//! `clear`: blank the screen and home the cursor.
//!
//! Not a coreutils utility — `clear` belongs to ncurses — so there is no
//! `uu_clear` to link, and Windows has no `clear.exe` at all: `cls` is a
//! `cmd.exe` built-in, which is not a program anything can find on `PATH`. That
//! combination is why this one body is written here while the rest of the table
//! is linked from uutils.
//!
//! What it writes is the ECMA-48 clear that `terminfo`'s common entries reduce
//! to: home the cursor (`CSI H`), erase the display (`CSI 2 J`), and erase the
//! saved lines above it (`CSI 3 J`) unless `-x` asks for the scrollback to be
//! kept. Nothing here reads `terminfo` or looks up a *capability* in `$TERM`: a
//! terminal that does not implement `CSI 3 J` ignores it, which is the rule
//! `docs/terminal.md` sets for the shell's own sequences, and the alternative —
//! a table of per-terminal quirks — is what that document rejects. `TERM=dumb`
//! is the one value that changes the answer, because a dumb terminal has no
//! cursor addressing to blank: nothing is written and the status is still 0.
//!
//! `clear` is only reached when `PATH` has no `clear`, so on a machine with
//! ncurses installed this never runs; `docs/portability.md` §6 explains why
//! that order is the right one.

use std::ffi::OsString;

/// `CSI H` home, `CSI 2 J` erase the display.
const SCREEN: &[u8] = b"\x1b[H\x1b[2J";
/// The same, then `CSI 3 J` erase the saved lines above the screen.
const WITH_SCROLLBACK: &[u8] = b"\x1b[H\x1b[2J\x1b[3J";

/// What one invocation asks for.
struct Options {
    /// `-x`: the scrollback is the user's history and stays.
    keep_scrollback: bool,
    /// `-T TERM`, or `None` to read `$TERM`.
    term: Option<Vec<u8>>,
}

/// Run `clear`, returning its exit status.
pub(crate) fn main(args: &[OsString]) -> i32 {
    let options = match parse(args) {
        Ok(options) => options,
        // `parse` has already written whatever the operand deserved.
        Err(status) => return status,
    };
    let Some(bytes) = sequence(&options) else {
        return 0;
    };
    match baash_sys::screen::clear(bytes) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("clear: {error}");
            1
        }
    }
}

/// The bytes this invocation writes, or `None` when it must write nothing.
///
/// The only term that answers `None` is `dumb`, which means "no cursor
/// addressing" — writing escapes at it would put them in a log or a transcript.
/// Every other value, including an unset one, gets the same sequence: there is
/// no capability to look up, so there is nothing else `TERM` could change.
fn sequence(options: &Options) -> Option<&'static [u8]> {
    let term = match &options.term {
        Some(term) => term.clone(),
        None => std::env::var_os("TERM").map_or_else(Vec::new, os_bytes),
    };
    if term == b"dumb" {
        return None;
    }
    Some(if options.keep_scrollback {
        SCREEN
    } else {
        WITH_SCROLLBACK
    })
}

/// Parse the operands, or return the status to exit with.
fn parse(args: &[OsString]) -> Result<Options, i32> {
    let mut options = Options {
        keep_scrollback: false,
        term: None,
    };
    let mut index = 1;
    while index < args.len() {
        let arg = os_bytes(&args[index]);
        match arg.as_slice() {
            b"--" => {
                index += 1;
                break;
            }
            b"-x" => {
                options.keep_scrollback = true;
                index += 1;
            }
            b"-h" | b"--help" => {
                print_help();
                return Err(0);
            }
            // `-T TERM` and `-TTERM` are both spellings ncurses accepts.
            other if other.starts_with(b"-T") => {
                if other.len() > 2 {
                    options.term = Some(other[2..].to_vec());
                } else if let Some(next) = args.get(index + 1) {
                    options.term = Some(os_bytes(next));
                    index += 1;
                } else {
                    return Err(usage_error("option requires an argument -- 'T'"));
                }
                index += 1;
            }
            other if other.len() > 1 && other[0] == b'-' => {
                return Err(usage_error(&format!(
                    "invalid option -- '{}'",
                    String::from_utf8_lossy(&other[1..])
                )));
            }
            _ => return Err(unexpected_argument(&arg)),
        }
    }
    if let Some(operand) = args.get(index) {
        return Err(unexpected_argument(&os_bytes(operand)));
    }
    Ok(options)
}

/// An OS string as the shell's bytes, without a lossy step on either platform.
fn os_bytes(value: impl AsRef<std::ffi::OsStr>) -> Vec<u8> {
    baash_sys::os::bytes_from_os(value)
}

/// Report an operand ncurses' `clear` has no use for.
fn unexpected_argument(operand: &[u8]) -> i32 {
    usage_error(&format!(
        "unexpected argument '{}'",
        String::from_utf8_lossy(operand)
    ))
}

/// Report a bad command line and print the usage, like ncurses' `clear` does.
fn usage_error(message: &str) -> i32 {
    eprintln!("clear: {message}");
    eprintln!("Usage: clear [-x] [-T TERM]");
    2
}

fn print_help() {
    println!(
        "Usage: clear [-x] [-T TERM]\n\
         \n\
         Blank the screen and home the cursor.\n\
         \n\
         \x20 -x        keep the scrollback (do not send CSI 3 J)\n\
         \x20 -T TERM   use TERM instead of $TERM; only \"dumb\" changes anything,\n\
         \x20           because the sequence does not vary by terminal\n\
         \x20 -h        print this help and exit"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    /// The bytes a command line writes, or the status it exits with instead.
    fn bytes_of(items: &[&str]) -> Result<Option<&'static [u8]>, i32> {
        parse(&args(items)).map(|options| sequence(&options))
    }

    #[test]
    fn plain_clear_erases_the_scrollback_and_x_keeps_it() {
        // Reference: `TERM=xterm-256color clear | od -c` writes
        // `ESC [ H ESC [ 2 J ESC [ 3 J`, and `clear -x` stops after `2 J`.
        // `-T` is explicit so the assertion does not depend on the `TERM` the
        // test harness happens to export.
        assert_eq!(
            bytes_of(&["clear", "-T", "xterm-256color"]).unwrap(),
            Some(b"\x1b[H\x1b[2J\x1b[3J".as_slice())
        );
        assert_eq!(
            bytes_of(&["clear", "-x", "-T", "xterm-256color"]).unwrap(),
            Some(b"\x1b[H\x1b[2J".as_slice())
        );
        // `--` ends the options, so the same two shapes survive it.
        assert_eq!(
            bytes_of(&["clear", "-T", "xterm", "--"]).unwrap(),
            Some(b"\x1b[H\x1b[2J\x1b[3J".as_slice())
        );
    }

    #[test]
    fn term_is_accepted_in_both_spellings() {
        let long = parse(&args(&["clear", "-T", "dumb"])).unwrap();
        assert_eq!(long.term.as_deref(), Some(b"dumb".as_slice()));
        let joined = parse(&args(&["clear", "-Tdumb"])).unwrap();
        assert_eq!(joined.term.as_deref(), Some(b"dumb".as_slice()));
        // `-T` needs its operand, and says so rather than reading `$TERM`.
        assert_eq!(parse(&args(&["clear", "-T"])).err(), Some(2));
    }

    #[test]
    fn a_bad_command_line_is_a_usage_error() {
        assert_eq!(parse(&args(&["clear", "-q"])).err(), Some(2));
        assert_eq!(parse(&args(&["clear", "file"])).err(), Some(2));
        // `-h` is help, not an error: it exits 0 without clearing anything.
        assert_eq!(parse(&args(&["clear", "-h"])).err(), Some(0));
        assert_eq!(parse(&args(&["clear", "--help"])).err(), Some(0));
    }

    #[test]
    fn a_dumb_terminal_gets_nothing_at_all() {
        // `dumb` is the one value that changes the answer, and `-T` is the
        // spelling that makes the test independent of the harness's `TERM`.
        assert_eq!(bytes_of(&["clear", "-T", "dumb"]).unwrap(), None);
        // Everything else, including `xterm`, `screen` and a terminal this
        // shell has never heard of, is the same two-or-three sequence bytes.
        assert_eq!(
            bytes_of(&["clear", "-T", "screen"]).unwrap(),
            Some(b"\x1b[H\x1b[2J\x1b[3J".as_slice())
        );
        assert_eq!(
            bytes_of(&["clear", "-T", "no-such-terminal"]).unwrap(),
            Some(b"\x1b[H\x1b[2J\x1b[3J".as_slice()),
            "no terminfo means no capability to fail to find"
        );
    }
}
