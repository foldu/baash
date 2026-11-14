//! Key decoding for a program that owns the screen.
//!
//! The line editor has its own decoder (`baash-repl`) and asks the terminal for
//! the kitty and `modifyOtherKeys` encodings, because editing a line needs to
//! tell `Ctrl-I` from `Tab`. A builtin that takes the whole screen (`baashzf`)
//! is not in the editor's position: it reads the *legacy* bytes a console sends
//! and it needs eight keys, so it decodes those here — the same [`Action`]s the
//! editor produces, so the two cannot drift about what `Ctrl-P` means, only
//! about how it arrives.
//!
//! A read from a terminal can end in the middle of a sequence: `ESC` arrives,
//! and the `[` of `ESC [ A` only after it. So [`Decoder::feed`] keeps an
//! unfinished tail, and [`Decoder::settle`] is what a timeout calls when the
//! tail never completes — which is also how a *bare* `Esc` press is recognised,
//! there being no byte after it to say so.

use crate::pick::Action;

/// Bytes that are handed straight to the query.
///
/// Everything above `0x1f` except `0x7f`, plus the two escape payloads that
/// carry text (a bracketed paste, and a terminal that sends a whole run of
/// characters as bytes).
const CONTROL_LIMIT: u8 = 0x20;

/// A streaming decoder for the picker's keys.
#[derive(Debug, Default)]
pub struct Decoder {
    /// Bytes that are not yet a key: an unfinished escape sequence, or the
    /// start of a bracketed paste.
    pending: Vec<u8>,
}

impl Decoder {
    /// A decoder with nothing pending.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes from the terminal; the actions they complete.
    ///
    /// An unfinished tail stays in the decoder. [`Decoder::pending`] reports
    /// whether there is one, so a caller looping on a read timeout knows it is
    /// waiting for something that may never come.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Action> {
        self.pending.extend_from_slice(bytes);
        let mut actions = Vec::new();
        while let Some(action) = self.next() {
            actions.push(action);
        }
        actions
    }

    /// Whether a partial key is waiting for more bytes.
    pub fn is_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Give up on a partial key.
    ///
    /// A lone `ESC` is the `Esc` key: a terminal sends no terminator for it, so
    /// the only way to tell it from the start of a sequence is that nothing
    /// followed. Any other unfinished sequence is dropped — there is no
    /// meaning to give it, and guessing would invent a keystroke.
    pub fn settle(&mut self) -> Option<Action> {
        let escape = self.pending == [0x1b];
        self.pending.clear();
        escape.then_some(Action::Cancel)
    }

    /// Consume the next complete key, if there is one.
    ///
    /// `None` means "nothing to hand over yet": either the buffer is empty or
    /// the bytes at its front cannot be decoded without more of them.
    fn next(&mut self) -> Option<Action> {
        loop {
            match self.step()? {
                Step::Action(action) => return Some(action),
                Step::Skip => continue,
                Step::Wait => return None,
            }
        }
    }

    /// Decode one byte, sequence or run of text from the front of the buffer.
    fn step(&mut self) -> Option<Step> {
        let first = *self.pending.first()?;
        let action = match first {
            0x1b => return Some(self.escape()),
            b'\r' | b'\n' | b'\t' => {
                self.pending.remove(0);
                Action::Accept
            }
            0x7f | 0x08 => {
                self.pending.remove(0);
                Action::Backspace
            }
            0x03 | 0x07 => {
                self.pending.remove(0);
                Action::Cancel
            }
            // `Ctrl-N`/`Ctrl-P` are down/up everywhere else too, and `Ctrl-R`
            // keeps the meaning it has on the shell's line, where it opens this
            // picker: the next older match.
            0x0e | 0x12 => {
                self.pending.remove(0);
                Action::Down
            }
            0x10 => {
                self.pending.remove(0);
                Action::Up
            }
            0x06 => {
                self.pending.remove(0);
                Action::PageDown
            }
            0x02 => {
                self.pending.remove(0);
                Action::PageUp
            }
            0x15 => {
                self.pending.remove(0);
                Action::Clear
            }
            0x17 => {
                self.pending.remove(0);
                Action::KillWord
            }
            // Any other control byte is not a key the picker has a use for;
            // swallowing it is better than inserting it into the query, where
            // it would paint as a space and match nothing.
            byte if byte < CONTROL_LIMIT => {
                self.pending.remove(0);
                return Some(Step::Skip);
            }
            _ => Action::Insert(self.text()),
        };
        Some(Step::Action(action))
    }

    /// The run of text at the front of the buffer, up to the next key.
    ///
    /// Text is inserted as bytes, not as characters: shell text is bytes, and a
    /// byte that is not valid UTF-8 has to reach the query exactly as typed.
    fn text(&mut self) -> Vec<u8> {
        let mut end = 0;
        while end < self.pending.len() {
            let byte = self.pending[end];
            if byte < CONTROL_LIMIT || byte == 0x7f {
                break;
            }
            end += 1;
        }
        self.pending.drain(..end).collect()
    }

    /// Decode a sequence that starts with `ESC`.
    fn escape(&mut self) -> Step {
        match self.pending.get(1) {
            None => Step::Wait,
            Some(b'[') => self.csi(),
            // `ESC O A` is the other spelling of an arrow, from a terminal in
            // application-cursor mode.
            Some(b'O') => match self.pending.get(2) {
                None => Step::Wait,
                Some(&final_byte) => {
                    self.pending.drain(..3);
                    match cursor(final_byte) {
                        Some(action) => Step::Action(action),
                        None => Step::Skip,
                    }
                }
            },
            // `Alt` + a key: not a gesture in the picker, and the key itself is
            // not text either. Drop both bytes.
            Some(_) => {
                self.pending.drain(..2);
                Step::Skip
            }
        }
    }

    /// Decode `ESC [ …`, whose final byte is in `@`..=`~`.
    fn csi(&mut self) -> Step {
        let mut end = 2;
        while end < self.pending.len() {
            if (0x40..=0x7e).contains(&self.pending[end]) {
                break;
            }
            end += 1;
        }
        let Some(&last) = self.pending.get(end) else {
            return Step::Wait;
        };
        let parameters = self.pending[2..end].to_vec();
        // A paste's payload must not be consumed as a sequence: the final byte
        // belongs to the header, and the body follows it.
        self.pending.drain(..=end);
        match last {
            b'~' => self.tilde(&parameters),
            _ => match cursor(last) {
                Some(action) => Step::Action(action),
                // A sequence with no meaning here (a mouse report, a
                // terminal's answer to a query) is swallowed whole rather than
                // typed.
                None => Step::Skip,
            },
        }
    }

    /// Decode `ESC [ <number> ~`.
    fn tilde(&mut self, parameters: &[u8]) -> Step {
        let action = match parameters {
            b"5" => Action::PageUp,
            b"6" => Action::PageDown,
            b"1" | b"7" => Action::First,
            b"4" | b"8" => Action::Last,
            // A bracketed paste: everything up to `ESC [ 201 ~` is text.
            b"200" => {
                let marker = b"\x1b[201~";
                let Some(at) = self
                    .pending
                    .windows(marker.len())
                    .position(|window| window == marker)
                else {
                    // The paste has not finished arriving; the body stays
                    // pending until the terminator does.
                    return Step::Wait;
                };
                let payload: Vec<u8> = self.pending.drain(..at).collect();
                self.pending.drain(..marker.len());
                return Step::Action(Action::Insert(payload));
            }
            _ => return Step::Skip,
        };
        Step::Action(action)
    }
}

/// What one step of decoding produced.
enum Step {
    /// A key.
    Action(Action),
    /// Bytes that are not a key here, already consumed.
    Skip,
    /// An unfinished sequence: keep the bytes and wait for the rest.
    Wait,
}

/// The action for one of the arrow-ish final bytes.
fn cursor(byte: u8) -> Option<Action> {
    match byte {
        b'A' => Some(Action::Up),
        b'B' => Some(Action::Down),
        b'H' => Some(Action::First),
        b'F' => Some(Action::Last),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actions(bytes: &[u8]) -> Vec<Action> {
        Decoder::new().feed(bytes)
    }

    #[test]
    fn text_is_one_insert() {
        assert_eq!(actions(b"git co"), vec![Action::Insert(b"git co".to_vec())]);
    }

    #[test]
    fn enter_tab_and_escape_are_the_three_ways_out() {
        assert_eq!(actions(b"\r"), vec![Action::Accept]);
        assert_eq!(actions(b"\n"), vec![Action::Accept]);
        assert_eq!(actions(b"\t"), vec![Action::Accept]);
        // A bare Esc is only Esc once nothing follows it.
        let mut decoder = Decoder::new();
        assert!(decoder.feed(b"\x1b").is_empty());
        assert!(decoder.is_pending());
        assert_eq!(decoder.settle(), Some(Action::Cancel));
        assert!(!decoder.is_pending());
    }

    #[test]
    fn the_picker_keys() {
        let cases: Vec<(&[u8], Action)> = vec![
            (b"\x7f", Action::Backspace),
            (b"\x08", Action::Backspace),
            (b"\x03", Action::Cancel),
            (b"\x07", Action::Cancel),
            (b"\x0e", Action::Down),
            (b"\x10", Action::Up),
            (b"\x12", Action::Down),
            (b"\x06", Action::PageDown),
            (b"\x02", Action::PageUp),
            (b"\x15", Action::Clear),
            (b"\x17", Action::KillWord),
        ];
        for (bytes, action) in cases {
            assert_eq!(actions(bytes), vec![action.clone()], "{bytes:?}");
        }
    }

    #[test]
    fn arrows_arrive_either_way() {
        assert_eq!(actions(b"\x1b[A"), vec![Action::Up]);
        assert_eq!(actions(b"\x1b[B"), vec![Action::Down]);
        assert_eq!(actions(b"\x1bOA"), vec![Action::Up]);
        assert_eq!(actions(b"\x1b[5~"), vec![Action::PageUp]);
        assert_eq!(actions(b"\x1b[6~"), vec![Action::PageDown]);
        assert_eq!(actions(b"\x1b[H"), vec![Action::First]);
        assert_eq!(actions(b"\x1b[F"), vec![Action::Last]);
    }

    #[test]
    fn a_sequence_split_across_reads_waits_for_its_tail() {
        let mut decoder = Decoder::new();
        assert!(decoder.feed(b"\x1b").is_empty());
        assert!(decoder.feed(b"[").is_empty());
        assert_eq!(decoder.feed(b"A"), vec![Action::Up]);
        assert!(!decoder.is_pending());
    }

    #[test]
    fn text_and_keys_in_one_read_are_decoded_in_order() {
        let cases = actions(b"ab\x1b[Bcd\r");
        assert_eq!(
            cases,
            vec![
                Action::Insert(b"ab".to_vec()),
                Action::Down,
                Action::Insert(b"cd".to_vec()),
                Action::Accept,
            ]
        );
    }

    #[test]
    fn a_sequence_the_picker_does_not_use_is_swallowed() {
        // A mouse report and an unknown CSI: neither may reach the query.
        assert_eq!(
            actions(b"\x1b[<0;12;3Mx"),
            vec![Action::Insert(b"x".to_vec())]
        );
        assert_eq!(actions(b"\x1b[2Jx"), vec![Action::Insert(b"x".to_vec())]);
        // `Alt-b` is not a gesture here, and its `b` is not text.
        assert_eq!(actions(b"\x1bbx"), vec![Action::Insert(b"x".to_vec())]);
    }

    #[test]
    fn a_bracketed_paste_becomes_one_insert() {
        assert_eq!(
            actions(b"\x1b[200~git status\x1b[201~"),
            vec![Action::Insert(b"git status".to_vec())]
        );
        assert_eq!(
            actions(b"\x1b[200~a b\tc\x1b[201~"),
            vec![Action::Insert(b"a b\tc".to_vec())],
            "the payload is passed through byte for byte"
        );
    }

    #[test]
    fn a_paste_that_has_not_finished_stays_pending() {
        let mut decoder = Decoder::new();
        assert!(decoder.feed(b"\x1b[200~half").is_empty());
        assert!(decoder.is_pending());
        assert_eq!(
            decoder.feed(b" done\x1b[201~"),
            vec![Action::Insert(b"half done".to_vec())]
        );
    }

    #[test]
    fn settling_drops_an_unfinished_sequence_without_inventing_a_key() {
        let mut decoder = Decoder::new();
        assert!(decoder.feed(b"\x1b[1").is_empty());
        assert_eq!(decoder.settle(), None);
        assert!(!decoder.is_pending());
    }
}
