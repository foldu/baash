//! Key decoding: terminal bytes to editing keys.
//!
//! Raw mode gives the editor a byte stream. This module turns the stream back
//! into keys, including the escape sequences every terminal sends for arrows
//! and friends, and bracketed paste, whose payload must never be interpreted
//! as keys.
//!
//! The decoder keeps one byte of pushback. A control byte arriving in the
//! middle of a CSI sequence cancels that sequence but belongs to the next key;
//! swallowing it would silently lose a keystroke (an `Enter` after a truncated
//! arrow sequence, for instance).

use std::io;

use crate::term::Terminal;

/// One editing key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    /// A decoded Unicode scalar (valid UTF-8 input).
    Char(char),
    /// A byte that is not part of a valid UTF-8 scalar. Shell text is bytes, so
    /// it is inserted rather than dropped.
    Byte(u8),
    /// Carriage return or line feed.
    Enter,
    /// Horizontal tab.
    Tab,
    /// Shift-Tab (CSI `Z`), used to move backwards in the completion menu.
    BackTab,
    /// Backspace, either `DEL` (0x7f) or `BS` (0x08).
    Backspace,
    /// Delete (CSI `3~`).
    Delete,
    /// Escape, when it is not the start of a sequence we understand.
    Esc,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Home.
    Home,
    /// End.
    End,
    /// A control byte, e.g. `Ctrl(0x01)` for Ctrl-A.
    Ctrl(u8),
    /// Alt plus a printable ASCII character, which is what the terminal sends as
    /// `ESC <char>`.
    Alt(char),
    /// Alt-Backspace: `ESC DEL` (0x7f) or `ESC BS` (0x08), readline's
    /// `backward-kill-word`. Terminals differ in which of the two they send, so
    /// both decode to this.
    AltBackspace,
    /// The terminal gained focus (CSI `I`).
    ///
    /// Focus reporting is requested with `?1004h` while a line is read; the
    /// event carries no text and exists so the editor can repaint a frame that
    /// another program may have painted over.
    FocusIn,
    /// The terminal lost focus (CSI `O`).
    FocusOut,
    /// A paste payload, inserted verbatim.
    Paste(Vec<u8>),
    /// A sequence the decoder does not understand. It is ignored whole: its
    /// bytes never reach the line.
    Unknown(Vec<u8>),
}

/// How many bytes of one CSI sequence to accept before giving up.
///
/// Real sequences are short (`\x1b[1;5C`); a longer run is terminal noise and
/// must not become an unbounded allocation.
const MAX_SEQUENCE: usize = 32;

/// The bracketed-paste terminator.
const PASTE_END: &[u8] = b"\x1b[201~";

/// Decodes keys from a [`Terminal`], keeping one byte of pushback.
#[derive(Clone, Debug, Default)]
pub struct Decoder {
    pending: Option<u8>,
}

impl Decoder {
    /// A decoder with no pushback.
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode one key, or `None` at end of input.
    pub fn next(&mut self, term: &mut dyn Terminal) -> io::Result<Option<Key>> {
        let Some(byte) = self.read(term)? else {
            return Ok(None);
        };
        Ok(Some(match byte {
            0x1b => self.decode_escape(term)?,
            b'\r' | b'\n' => Key::Enter,
            b'\t' => Key::Tab,
            0x7f | 0x08 => Key::Backspace,
            b if b < 0x20 => Key::Ctrl(b),
            b if b < 0x7f => Key::Char(char::from(b)),
            b => self.decode_utf8(term, b)?,
        }))
    }

    /// Read one byte, preferring the pushback slot.
    fn read(&mut self, term: &mut dyn Terminal) -> io::Result<Option<u8>> {
        match self.pending.take() {
            Some(byte) => Ok(Some(byte)),
            None => term.read_byte(),
        }
    }

    /// Collect the continuation bytes of a UTF-8 scalar starting with `first`.
    ///
    /// Falls back to [`Key::Byte`] for a malformed or truncated sequence, so an
    /// undecodable name still gets inserted byte for byte.
    fn decode_utf8(&mut self, term: &mut dyn Terminal, first: u8) -> io::Result<Key> {
        let length = match first {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return Ok(Key::Byte(first)),
        };
        let mut buffer = [0u8; 4];
        buffer[0] = first;
        for slot in buffer.iter_mut().take(length).skip(1) {
            match self.read(term)? {
                Some(byte) if byte & 0xc0 == 0x80 => *slot = byte,
                // Truncated or invalid: keep the lead byte, and let the byte
                // that was not a continuation be its own key.
                Some(byte) => {
                    self.pending = Some(byte);
                    return Ok(Key::Byte(first));
                }
                None => return Ok(Key::Byte(first)),
            }
        }
        match std::str::from_utf8(&buffer[..length]) {
            Ok(text) => match text.chars().next() {
                Some(character) => Ok(Key::Char(character)),
                None => Ok(Key::Byte(first)),
            },
            Err(_) => Ok(Key::Byte(first)),
        }
    }

    /// Decode what follows an `ESC`.
    fn decode_escape(&mut self, term: &mut dyn Terminal) -> io::Result<Key> {
        let Some(next) = self.read(term)? else {
            // A lone Escape at end of input (this is how a fake terminal
            // expresses the key; a real tty needs a read timeout to tell it
            // apart from Alt).
            return Ok(Key::Esc);
        };
        match next {
            b'[' => self.decode_csi(term),
            b'O' => self.decode_ss3(term),
            0x1b => Ok(Key::Esc),
            byte if byte.is_ascii_graphic() || byte == b' ' => Ok(Key::Alt(char::from(byte))),
            0x7f | 0x08 => Ok(Key::AltBackspace),
            byte => Ok(Key::Unknown(vec![0x1b, byte])),
        }
    }

    /// Decode a CSI sequence: `ESC [`, parameter bytes, one final byte.
    fn decode_csi(&mut self, term: &mut dyn Terminal) -> io::Result<Key> {
        let mut sequence = Vec::new();
        let mut final_byte = None;
        while sequence.len() < MAX_SEQUENCE {
            let Some(byte) = self.read(term)? else {
                return Ok(Key::Unknown(sequence));
            };
            if (0x40..=0x7e).contains(&byte) {
                final_byte = Some(byte);
                break;
            }
            if byte < 0x20 || byte == 0x7f {
                // Not part of a CSI sequence: abort and keep it for the next
                // key, so an Enter after a truncated sequence still submits.
                self.pending = Some(byte);
                return Ok(Key::Unknown(sequence));
            }
            sequence.push(byte);
        }
        let Some(final_byte) = final_byte else {
            return Ok(Key::Unknown(sequence));
        };
        let parameters = sequence.as_slice();
        Ok(match (parameters, final_byte) {
            (b"", b'A') => Key::Up,
            (b"", b'B') => Key::Down,
            (b"", b'C') => Key::Right,
            (b"", b'D') => Key::Left,
            (b"", b'H') => Key::Home,
            (b"", b'F') => Key::End,
            (b"", b'Z') => Key::BackTab,
            // Focus reporting: `ESC [ I` and `ESC [ O`. They are keys only in
            // the sense that the terminal sends them between keystrokes.
            (b"", b'I') => Key::FocusIn,
            (b"", b'O') => Key::FocusOut,
            (b"1" | b"7", b'~') => Key::Home,
            (b"4" | b"8", b'~') => Key::End,
            (b"3", b'~') => Key::Delete,
            (b"200", b'~') => self.decode_paste(term)?,
            _ => Key::Unknown(sequence),
        })
    }

    /// Decode an SS3 sequence: `ESC O <byte>`, the other encoding for arrows
    /// and Home/End.
    fn decode_ss3(&mut self, term: &mut dyn Terminal) -> io::Result<Key> {
        let Some(byte) = self.read(term)? else {
            return Ok(Key::Unknown(vec![b'O']));
        };
        Ok(match byte {
            b'A' => Key::Up,
            b'B' => Key::Down,
            b'C' => Key::Right,
            b'D' => Key::Left,
            b'H' => Key::Home,
            b'F' => Key::End,
            _ => Key::Unknown(vec![b'O', byte]),
        })
    }

    /// Read bracketed-paste payload up to `ESC [ 201 ~`.
    ///
    /// End of input ends the paste as well: an unterminated paste is still a
    /// paste, and dropping the bytes would lose the user's text.
    fn decode_paste(&mut self, term: &mut dyn Terminal) -> io::Result<Key> {
        let mut payload = Vec::new();
        loop {
            match self.read(term)? {
                None => return Ok(Key::Paste(payload)),
                Some(byte) => payload.push(byte),
            }
            if payload.ends_with(PASTE_END) {
                payload.truncate(payload.len() - PASTE_END.len());
                return Ok(Key::Paste(payload));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::Size;

    /// A terminal whose input is a script; it records nothing.
    struct Script {
        input: std::collections::VecDeque<u8>,
    }

    impl Script {
        fn new(bytes: &[u8]) -> Self {
            Self {
                input: bytes.iter().copied().collect(),
            }
        }
    }

    impl Terminal for Script {
        fn read_byte(&mut self) -> io::Result<Option<u8>> {
            Ok(self.input.pop_front())
        }

        fn write_all(&mut self, _bytes: &[u8]) -> io::Result<()> {
            Ok(())
        }

        fn size(&self) -> Size {
            Size::FALLBACK
        }

        fn enter_raw(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn leave_raw(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn keys(bytes: &[u8]) -> Vec<Key> {
        let mut terminal = Script::new(bytes);
        let mut decoder = Decoder::new();
        let mut keys = Vec::new();
        while let Some(key) = decoder.next(&mut terminal).unwrap() {
            keys.push(key);
        }
        keys
    }

    #[test]
    fn plain_bytes_and_controls() {
        assert_eq!(
            keys(b"a\t\r\x01\x7f"),
            vec![
                Key::Char('a'),
                Key::Tab,
                Key::Enter,
                Key::Ctrl(0x01),
                Key::Backspace
            ]
        );
    }

    #[test]
    fn arrows_have_both_encodings() {
        assert_eq!(
            keys(b"\x1b[A\x1b[B\x1b[C\x1b[D\x1bOA\x1bOB\x1bOC\x1bOD"),
            vec![
                Key::Up,
                Key::Down,
                Key::Right,
                Key::Left,
                Key::Up,
                Key::Down,
                Key::Right,
                Key::Left
            ]
        );
    }

    #[test]
    fn home_end_and_delete() {
        assert_eq!(
            keys(b"\x1b[H\x1b[F\x1b[1~\x1b[4~\x1b[7~\x1b[8~\x1b[3~"),
            vec![
                Key::Home,
                Key::End,
                Key::Home,
                Key::End,
                Key::Home,
                Key::End,
                Key::Delete
            ]
        );
    }

    #[test]
    fn shift_tab_is_its_own_key() {
        assert_eq!(keys(b"\x1b[Z"), vec![Key::BackTab]);
    }

    #[test]
    fn alt_letters() {
        assert_eq!(
            keys(b"\x1bb\x1bf\x1bd"),
            vec![Key::Alt('b'), Key::Alt('f'), Key::Alt('d')]
        );
    }

    #[test]
    fn alt_backspace_is_either_del_or_bs() {
        // Terminals disagree: xterm sends `ESC DEL`, some send `ESC BS`. Both
        // are readline's `backward-kill-word`, so both decode to one key.
        assert_eq!(
            keys(b"\x1b\x7f\x1b\x08"),
            vec![Key::AltBackspace, Key::AltBackspace]
        );
    }

    #[test]
    fn a_lone_escape_at_end_of_input() {
        assert_eq!(keys(b"\x1b"), vec![Key::Esc]);
    }

    #[test]
    fn a_truncated_sequence_is_ignored_whole() {
        assert_eq!(
            keys(b"\x1b[3"),
            vec![Key::Unknown(b"3".to_vec())],
            "a partial CSI must not insert its parameters as text"
        );
        assert_eq!(keys(b"\x1b["), vec![Key::Unknown(Vec::new())]);
        assert_eq!(keys(b"\x1bO"), vec![Key::Unknown(b"O".to_vec())]);
    }

    #[test]
    fn a_control_byte_aborts_a_sequence_and_is_not_swallowed() {
        // The decoder consumes `[3`, aborts on the carriage return, and gives
        // the Enter back as the next key.
        assert_eq!(
            keys(b"\x1b[3\r"),
            vec![Key::Unknown(b"3".to_vec()), Key::Enter]
        );
    }

    #[test]
    fn focus_events_are_decoded() {
        assert_eq!(keys(b"\x1b[I\x1b[O"), vec![Key::FocusIn, Key::FocusOut]);
        // Not to be confused with SS3 `ESC O`, which is an arrow key prefix.
        assert_eq!(keys(b"\x1bOA"), vec![Key::Up]);
    }

    #[test]
    fn bracketed_paste_is_verbatim() {
        let pasted = b"echo \x1b[A hi\nsecond";
        let mut input = b"\x1b[200~".to_vec();
        input.extend_from_slice(pasted);
        input.extend_from_slice(b"\x1b[201~");
        assert_eq!(keys(&input), vec![Key::Paste(pasted.to_vec())]);
    }

    #[test]
    fn an_unterminated_paste_still_arrives() {
        assert_eq!(keys(b"\x1b[200~text"), vec![Key::Paste(b"text".to_vec())]);
    }

    #[test]
    fn multibyte_utf8_is_one_key() {
        assert_eq!(
            keys("é€😀".as_bytes()),
            vec![Key::Char('é'), Key::Char('€'), Key::Char('😀')]
        );
    }

    #[test]
    fn invalid_utf8_is_a_byte_and_does_not_eat_the_next_key() {
        assert_eq!(keys(b"\xff"), vec![Key::Byte(0xff)]);
        assert_eq!(
            keys(b"\xc3("),
            vec![Key::Byte(0xc3), Key::Char('(')],
            "the non-continuation byte is pushed back"
        );
    }
}
