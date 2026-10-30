//! The `history` built-in.
//!
//! The file-era syntax is gone with the file: `-a`/`-n`/`-r`/`-w` (which only
//! moved lines between a ring and `$HISTFILE`), `-p`/`-s` (which only existed
//! to drive history expansion's tests) and `fc` (an editor launcher built on
//! the same text file). What is left is what a shell user actually types:
//!
//! ```text
//! history      →  "    1  echo a"     (five-wide number, two spaces)
//! history 2    →  the last two entries
//! history -d 3 →  forget entry 3
//! history -c   →  forget everything
//! ```
//!
//! Numbers are the session's, as [`bsh_history::History`] explains: they do not
//! renumber when an entry is deleted, and they are not database ids.

use super::{codes, diagnose, display, usage};
use crate::{HistoryEntry, ShellHost, Status};

const HISTORY_USAGE: &str = "usage: history [-c] [-d offset] [n]";

/// Write one `history` line: a five-wide number, two spaces, the command.
fn write_history_line(out: &mut Vec<u8>, entry: &HistoryEntry) {
    out.extend_from_slice(format!("{:>5}", entry.number()).as_bytes());
    out.extend_from_slice(b"  ");
    out.extend_from_slice(entry.text());
    out.push(b'\n');
}

/// `history [-c] [-d offset] [n]`.
pub(super) fn bi_history(host: &mut dyn ShellHost, argv: &[&[u8]]) -> Status {
    let mut clear = false;
    let mut delete: Option<Vec<u8>> = None;
    let mut operands: Vec<&[u8]> = Vec::new();
    let mut options_done = false;

    let mut index = 1;
    while index < argv.len() {
        let argument = argv[index];
        index += 1;
        // A bare `-` and a lone `-3` are operands, not options: the first is
        // the conventional end-of-options spelling, the second a negative
        // history offset.
        if options_done
            || argument == b"-"
            || !argument.starts_with(b"-")
            || argument.len() == 1
            || argument[1..].iter().all(u8::is_ascii_digit)
        {
            operands.push(argument);
            continue;
        }
        if argument == b"--" {
            options_done = true;
            continue;
        }
        let letters = &argument[1..];
        let mut position = 0;
        while position < letters.len() {
            match letters[position] {
                b'c' => clear = true,
                b'd' => {
                    let attached = &letters[position + 1..];
                    if attached.is_empty() {
                        match argv.get(index) {
                            Some(value) => {
                                delete = Some(value.to_vec());
                                index += 1;
                            }
                            None => {
                                diagnose(
                                    host,
                                    codes::USAGE,
                                    "history: -d: option requires an argument",
                                );
                                diagnose(host, codes::USAGE, format!("history: {HISTORY_USAGE}"));
                                return Status::USAGE;
                            }
                        }
                    } else {
                        delete = Some(attached.to_vec());
                    }
                    position = letters.len();
                    continue;
                }
                other => {
                    diagnose(
                        host,
                        codes::USAGE,
                        format!("history: -{}: invalid option", char::from(other)),
                    );
                    diagnose(host, codes::USAGE, format!("history: {HISTORY_USAGE}"));
                    return Status::USAGE;
                }
            }
            position += 1;
        }
    }

    if clear {
        host.state().history.clear();
    }

    if let Some(offset) = &delete {
        let Some(reference) = parse_reference(offset) else {
            diagnose(
                host,
                codes::NOT_FOUND,
                format!("history: {}: invalid number", display(offset)),
            );
            return Status::FAILURE;
        };
        if !host.state().history.delete(reference) {
            diagnose(
                host,
                codes::NOT_FOUND,
                format!(
                    "history: {}: history position out of range",
                    display(offset)
                ),
            );
            return Status::FAILURE;
        }
    }

    // `history -c` and `history -d` without a count do not also print the
    // list; bash agrees.
    let any_action = clear || delete.is_some();
    if any_action && operands.is_empty() {
        return Status::SUCCESS;
    }

    let limit = match operands.as_slice() {
        [] => None,
        [one] => match parse_count(one) {
            Some(limit) => Some(limit),
            None => {
                diagnose(
                    host,
                    codes::USAGE,
                    format!("history: {}: numeric argument required", display(one)),
                );
                return Status::USAGE;
            }
        },
        _ => return usage(host, argv, "too many arguments"),
    };

    let mut out = Vec::new();
    for entry in host.state().history.list(limit) {
        write_history_line(&mut out, entry);
    }
    let _ = host.write_out(&out);
    Status::SUCCESS
}

/// Parse an integer history reference, accepting a leading `-`.
fn parse_reference(value: &[u8]) -> Option<i64> {
    let (negative, digits) = match value.first() {
        Some(b'-') => (true, &value[1..]),
        _ => (false, value),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let magnitude: i64 = std::str::from_utf8(digits).ok()?.parse().ok()?;
    Some(if negative { -magnitude } else { magnitude })
}

/// Parse the `history n` count: a non-negative decimal.
fn parse_count(value: &[u8]) -> Option<usize> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(value).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Opts, ShellHost, ShellState};

    /// A host with no parser: `history` only needs state and two byte sinks.
    struct Host {
        state: ShellState,
        out: Vec<u8>,
        err: Vec<u8>,
    }

    impl Host {
        fn new() -> Self {
            Self {
                state: ShellState::new(Opts::default()),
                out: Vec::new(),
                err: Vec::new(),
            }
        }

        fn seed(&mut self, lines: &[&str]) {
            for line in lines {
                self.state.history.push(line.as_bytes());
            }
        }

        fn out(&self) -> String {
            String::from_utf8_lossy(&self.out).into_owned()
        }

        fn err(&self) -> String {
            String::from_utf8_lossy(&self.err).into_owned()
        }

        fn run(&mut self, argv: &[&str]) -> Status {
            let argv: Vec<Vec<u8>> = argv.iter().map(|arg| arg.as_bytes().to_vec()).collect();
            let refs: Vec<&[u8]> = argv.iter().map(Vec::as_slice).collect();
            bi_history(self, &refs)
        }
    }

    impl ShellHost for Host {
        fn state(&mut self) -> &mut ShellState {
            &mut self.state
        }

        fn run_source(&mut self, _src: &[u8]) -> Status {
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
    }

    #[test]
    fn prints_a_five_wide_number_and_two_spaces() {
        let mut host = Host::new();
        host.seed(&["echo a", "echo b", "echo c"]);
        assert_eq!(host.run(&["history"]), Status(0));
        assert_eq!(host.out(), "    1  echo a\n    2  echo b\n    3  echo c\n");
    }

    #[test]
    fn a_count_shows_the_newest_entries() {
        let mut host = Host::new();
        host.seed(&["echo a", "echo b", "echo c"]);
        assert_eq!(host.run(&["history", "2"]), Status(0));
        assert_eq!(host.out(), "    2  echo b\n    3  echo c\n");
        host.out.clear();
        assert_eq!(host.run(&["history", "99"]), Status(0));
        assert_eq!(host.out().lines().count(), 3);
    }

    #[test]
    fn an_empty_ring_prints_nothing() {
        let mut host = Host::new();
        assert_eq!(host.run(&["history"]), Status(0));
        assert!(host.out().is_empty());
    }

    #[test]
    fn clearing_empties_the_ring_and_starts_numbering_over() {
        let mut host = Host::new();
        host.seed(&["a", "b"]);
        assert_eq!(host.run(&["history", "-c"]), Status(0));
        assert_eq!(host.state.history.len(), 0);
        assert_eq!(host.state.history.next_number(), 1);
        assert!(host.out().is_empty(), "`-c` does not print the list");
    }

    #[test]
    fn deleting_leaves_the_remaining_numbers_alone() {
        let mut host = Host::new();
        host.seed(&["a", "b", "c"]);
        assert_eq!(host.run(&["history", "-d", "2"]), Status(0));
        assert_eq!(host.state.history.len(), 2);
        let numbers: Vec<u32> = host
            .state
            .history
            .entries()
            .iter()
            .map(HistoryEntry::number)
            .collect();
        assert_eq!(numbers, [1, 3]);
        assert!(host.out().is_empty(), "`-d` does not print the list");
    }

    #[test]
    fn a_bad_reference_is_a_failure() {
        let mut host = Host::new();
        host.seed(&["a"]);
        assert_eq!(host.run(&["history", "-d", "zz"]), Status(1));
        assert!(host.err().contains("invalid number"));
        assert_eq!(host.run(&["history", "-d", "7"]), Status(1));
        assert!(host.err().contains("out of range"));
        assert_eq!(host.run(&["history", "-d", "0"]), Status(1));
    }

    #[test]
    fn an_unknown_option_is_a_usage_error() {
        let mut host = Host::new();
        assert_eq!(host.run(&["history", "-z"]), Status(2));
        assert!(host.err().contains("invalid option"));
        assert_eq!(host.run(&["history", "-d"]), Status(2));
        assert_eq!(host.run(&["history", "1", "2"]), Status(2));
        assert_eq!(host.run(&["history", "x"]), Status(2));
    }

    #[test]
    fn the_file_options_are_gone() {
        // `-a`, `-n`, `-r`, `-w`, `-p` and `-s` moved lines between the ring
        // and `$HISTFILE`. There is no file, so the option is not accepted.
        for option in ["-a", "-n", "-r", "-w", "-p", "-s"] {
            let mut host = Host::new();
            host.seed(&["echo a"]);
            assert_eq!(
                host.run(&["history", option]),
                Status(2),
                "`history {option}` must not be accepted"
            );
            assert_eq!(host.state.history.len(), 1);
        }
    }
}
