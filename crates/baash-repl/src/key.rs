//! Key decoding: terminal bytes to editing keys.
//!
//! Raw mode gives the editor a byte stream. This module turns the stream back
//! into keys, including the escape sequences every terminal sends for arrows
//! and friends, and bracketed paste, whose payload must never be interpreted
//! as keys.
//!
//! A key is a [`Code`] and the [`Modifiers`] held with it. The legacy encoding
//! cannot express most combinations — `Ctrl-I` is a tab, `Ctrl-M` is Enter,
//! `Ctrl-Shift-R` is `Ctrl-R` — so while a line is read the terminal is asked
//! for two unambiguous encodings instead (`docs/terminal.md` §12): the kitty
//! keyboard protocol's `CSI u`, and xterm's `modifyOtherKeys` for keys with no
//! legacy encoding at all. Legacy bytes still decode to the same keys, so a
//! terminal that answers neither request keeps working with the ambiguity it
//! always had.
//!
//! The decoder keeps one byte of pushback. A control byte arriving in the
//! middle of a CSI sequence cancels that sequence but belongs to the next key;
//! swallowing it would silently lose a keystroke (an `Enter` after a truncated
//! arrow sequence, for instance).

use std::io;

use crate::term::Terminal;

/// One editing key: what was pressed, and which modifiers were held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    /// What was pressed.
    pub code: Code,
    /// The modifiers held with it.
    pub mods: Modifiers,
}

/// The key itself, without its modifiers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Code {
    /// A decoded Unicode scalar (valid UTF-8 input).
    Char(char),
    /// A byte that is not part of a valid UTF-8 scalar. Shell text is bytes, so
    /// it is inserted rather than dropped.
    Byte(u8),
    /// Carriage return or line feed.
    Enter,
    /// Horizontal tab.
    Tab,
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
    /// The left mouse button was pressed on the prompt or command line
    /// (SGR mouse, `CSI < 0 ; column ; row M`), both coordinates one-based.
    ///
    /// Only sent because the prompt mark asks for `click_events`; a terminal
    /// with general mouse tracking on would send these for anything.
    MouseLeft {
        /// The column, one-based, as the terminal reports it.
        column: u16,
        /// The row, one-based, as the terminal reports it.
        row: u16,
    },
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

/// The modifier keys held for one key event.
///
/// The bit values are the kitty keyboard protocol's; xterm's
/// `modifyOtherKeys` encodes the same four it knows in the same order, so one
/// type decodes both. Lock modifiers are decoded for completeness: a terminal
/// that reports them should not make `A` stop looking like the letter `A`.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers(u8);

impl Modifiers {
    /// No modifiers.
    pub const NONE: Self = Self(0);
    /// Shift.
    pub const SHIFT: Self = Self(1);
    /// Alt, or Option.
    pub const ALT: Self = Self(2);
    /// Control.
    pub const CTRL: Self = Self(4);
    /// Super, or Command.
    pub const SUPER: Self = Self(8);
    /// Hyper.
    pub const HYPER: Self = Self(16);
    /// Meta.
    pub const META: Self = Self(32);
    /// Caps Lock was on.
    pub const CAPS_LOCK: Self = Self(64);
    /// Num Lock was on.
    pub const NUM_LOCK: Self = Self(128);

    /// The bits as one byte.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether `other` is part of this set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether no modifier is held.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether this key event still produces the character it carries.
    ///
    /// Shift, Caps Lock and Num Lock change *which* character a key produces,
    /// and the terminal has already applied that when it hands over a code
    /// point. The four that turn a key into a command — Ctrl, Alt, Super,
    /// Hyper, Meta — mean the character itself is not text to insert.
    pub const fn is_text(self) -> bool {
        self.0 & (Self::CTRL.0 | Self::ALT.0 | Self::SUPER.0 | Self::HYPER.0 | Self::META.0) == 0
    }

    /// The modifiers that decide what a key *means*.
    ///
    /// Caps Lock and Num Lock say which character a key produces, never which
    /// command it is, and a binding is written without them. They are dropped
    /// here because the kitty protocol reports them in the modifier field:
    /// ghostty sends `Ctrl-D` as `CSI 100 ; 5 u` with the locks off and
    /// `CSI 100 ; 133 u` with Num Lock on, and matching on the raw bits would
    /// make every binding depend on a lock key. Between one press and the next
    /// nothing about the command has changed.
    pub const fn binding(self) -> Self {
        Self(self.0 & !(Self::CAPS_LOCK.0 | Self::NUM_LOCK.0))
    }

    /// Decode the protocol's `1 + bits` encoding.
    ///
    /// A missing field means no modifiers; the caller passes the default. The
    /// encoding of 0 is not valid (it would mean `-1`), and is treated as no
    /// modifiers for the same reason a malformed field is: the key still
    /// happened.
    pub const fn from_encoded(value: u32) -> Self {
        Self(value.saturating_sub(1) as u8)
    }
}

impl std::ops::BitOr for Modifiers {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl std::fmt::Debug for Modifiers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_empty() {
            return f.write_str("NONE");
        }
        let names = [
            (Self::SHIFT, "SHIFT"),
            (Self::ALT, "ALT"),
            (Self::CTRL, "CTRL"),
            (Self::SUPER, "SUPER"),
            (Self::HYPER, "HYPER"),
            (Self::META, "META"),
            (Self::CAPS_LOCK, "CAPS_LOCK"),
            (Self::NUM_LOCK, "NUM_LOCK"),
        ];
        let mut first = true;
        for (bit, name) in names {
            if self.contains(bit) {
                if !first {
                    f.write_str("|")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        Ok(())
    }
}

impl Key {
    /// A key with no modifiers.
    pub const fn of(code: Code) -> Self {
        Self {
            code,
            mods: Modifiers::NONE,
        }
    }

    /// A key with explicit modifiers.
    pub const fn with(code: Code, mods: Modifiers) -> Self {
        Self { code, mods }
    }

    /// A character as typed.
    pub const fn char(character: char) -> Self {
        Self::of(Code::Char(character))
    }

    /// Control plus a character, which is how both protocols report `Ctrl-A`.
    pub const fn ctrl(character: char) -> Self {
        Self::with(Code::Char(character), Modifiers::CTRL)
    }

    /// Alt plus a character.
    pub const fn alt(character: char) -> Self {
        Self::with(Code::Char(character), Modifiers::ALT)
    }

    /// A raw byte that is not valid UTF-8.
    pub const fn byte(byte: u8) -> Self {
        Self::of(Code::Byte(byte))
    }

    /// Whether this is `code` with no modifiers held.
    pub fn is(&self, code: Code) -> bool {
        self.code == code && self.mods.is_empty()
    }
}

/// How many bytes of one CSI sequence to accept before giving up.
///
/// Real sequences are short (`\x1b[1;5C`); a longer run is terminal noise and
/// must not become an unbounded allocation.
const MAX_SEQUENCE: usize = 32;

/// The bracketed-paste terminator.
const PASTE_END: &[u8] = b"\x1b[201~";

/// How long the rest of a terminal reply may take to arrive.
///
/// A reply is written in one burst, so this is generous for a real one and
/// short enough that a stray `ESC ]` from a keyboard cannot park the line: a
/// person pressing Alt-`]` waits this long at most, and keeps typing.
const OSC_WAIT: std::time::Duration = std::time::Duration::from_millis(150);

/// How much of a reply to accept before giving up on its terminator.
///
/// A real one is tens of bytes (`ESC ] 11 ; rgb: … BEL`); anything longer is
/// not a reply, and must not become an unbounded allocation.
const MAX_OSC: usize = 4096;

/// Decodes keys from a [`Terminal`], keeping one byte of pushback.
#[derive(Clone, Debug, Default)]
pub struct Decoder {
    /// Bytes read but not decoded yet. A one-byte slot became a queue when the
    /// cursor-position query arrived: a query that reads ahead and finds a
    /// keystroke instead of a report must put every byte back, and a keystroke
    /// can be a whole UTF-8 scalar.
    pending: std::collections::VecDeque<u8>,
}

impl Decoder {
    /// A decoder with no pushback.
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode one key, or `None` at end of input.
    pub fn next(&mut self, term: &mut dyn Terminal) -> io::Result<Option<Key>> {
        loop {
            let Some(byte) = self.read(term)? else {
                return Ok(None);
            };
            let key = match byte {
                0x1b => self.decode_escape(term)?,
                b'\r' | b'\n' => Some(Key::of(Code::Enter)),
                b'\t' => Some(Key::of(Code::Tab)),
                0x7f | 0x08 => Some(Key::of(Code::Backspace)),
                b if b < 0x20 => control_key(b),
                b if b < 0x7f => Some(Key::char(char::from(b))),
                b => Some(self.decode_utf8(term, b)?),
            };
            // `None` means the bytes were an event the editor should not see,
            // such as a key-release report: read the next one.
            if let Some(key) = key {
                return Ok(Some(key));
            }
        }
    }

    /// Read one byte, preferring the pushback slot.
    fn read(&mut self, term: &mut dyn Terminal) -> io::Result<Option<u8>> {
        match self.pending.pop_front() {
            Some(byte) => Ok(Some(byte)),
            None => term.read_byte(),
        }
    }

    /// Whether bytes read ahead are waiting to be decoded.
    ///
    /// A caller that waits on the terminal before decoding must ask this
    /// first: the cursor-position query can push a keystroke back, and the
    /// kernel then has nothing to wake a read on.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Put bytes back, in the order they were read.
    fn unread(&mut self, bytes: &[u8]) {
        for &byte in bytes.iter().rev() {
            self.pending.push_front(byte);
        }
    }

    /// Wait for a cursor-position report: `ESC [ row ; col R`.
    ///
    /// The report is how the prompt learns what the command before it left on
    /// screen. Returns `None` when the terminal does not answer within
    /// `timeout` or when the bytes that arrived are not a report — a keystroke
    /// typed while the prompt waits is put back for the next
    /// [`Decoder::next`], never swallowed.
    pub fn cursor_position(
        &mut self,
        term: &mut dyn Terminal,
        timeout: std::time::Duration,
    ) -> io::Result<Option<(u16, u16)>> {
        let deadline = std::time::Instant::now() + timeout;
        let mut seen = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let Some(byte) = term.read_byte_timeout(remaining)? else {
                self.unread(&seen);
                return Ok(None);
            };
            seen.push(byte);
            match cursor_report(&seen) {
                Report::Incomplete => {}
                Report::Position(row, column) => return Ok(Some((row, column))),
                Report::Other => {
                    self.unread(&seen);
                    return Ok(None);
                }
            }
        }
    }

    /// Collect the continuation bytes of a UTF-8 scalar starting with `first`.
    ///
    /// Falls back to [`Code::Byte`] for a malformed or truncated sequence, so
    /// an undecodable name still gets inserted byte for byte.
    fn decode_utf8(&mut self, term: &mut dyn Terminal, first: u8) -> io::Result<Key> {
        let length = match first {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return Ok(Key::byte(first)),
        };
        let mut buffer = [0u8; 4];
        buffer[0] = first;
        for slot in buffer.iter_mut().take(length).skip(1) {
            match self.read(term)? {
                Some(byte) if byte & 0xc0 == 0x80 => *slot = byte,
                // Truncated or invalid: keep the lead byte, and let the byte
                // that was not a continuation be its own key.
                Some(byte) => {
                    self.pending.push_front(byte);
                    return Ok(Key::byte(first));
                }
                None => return Ok(Key::byte(first)),
            }
        }
        match std::str::from_utf8(&buffer[..length]) {
            Ok(text) => match text.chars().next() {
                Some(character) => Ok(Key::char(character)),
                None => Ok(Key::byte(first)),
            },
            Err(_) => Ok(Key::byte(first)),
        }
    }

    /// Decode what follows an `ESC`.
    fn decode_escape(&mut self, term: &mut dyn Terminal) -> io::Result<Option<Key>> {
        let Some(next) = self.read(term)? else {
            // A lone Escape at end of input (this is how a fake terminal
            // expresses the key; a real tty needs a read timeout to tell it
            // apart from Alt, unless the terminal speaks `CSI 27 u`).
            return Ok(Some(Key::of(Code::Esc)));
        };
        match next {
            b'[' => self.decode_csi(term),
            // An operating-system command on this channel is the terminal
            // answering a question, never a key: see `decode_osc`.
            b']' => self.decode_osc(term),
            b'O' => Ok(Some(self.decode_ss3(term)?)),
            0x1b => Ok(Some(Key::of(Code::Esc))),
            byte if byte.is_ascii_graphic() || byte == b' ' => Ok(Some(Key::alt(char::from(byte)))),
            // Alt-Backspace. Terminals differ in which of `ESC DEL` and
            // `ESC BS` they send, and readline calls both
            // `backward-kill-word`, so one key comes out of either.
            0x7f | 0x08 => Ok(Some(Key::with(Code::Backspace, Modifiers::ALT))),
            byte => Ok(Some(Key::of(Code::Unknown(vec![0x1b, byte])))),
        }
    }

    /// Decode a CSI sequence: `ESC [`, parameter bytes, one final byte.
    fn decode_csi(&mut self, term: &mut dyn Terminal) -> io::Result<Option<Key>> {
        let mut sequence = Vec::new();
        let mut final_byte = None;
        while sequence.len() < MAX_SEQUENCE {
            let Some(byte) = self.read(term)? else {
                return Ok(Some(Key::of(Code::Unknown(sequence))));
            };
            if (0x40..=0x7e).contains(&byte) {
                final_byte = Some(byte);
                break;
            }
            if byte < 0x20 || byte == 0x7f {
                // Not part of a CSI sequence: abort and keep it for the next
                // key, so an Enter after a truncated sequence still submits.
                self.pending.push_front(byte);
                return Ok(Some(Key::of(Code::Unknown(sequence))));
            }
            sequence.push(byte);
        }
        let Some(final_byte) = final_byte else {
            return Ok(Some(Key::of(Code::Unknown(sequence))));
        };
        let parameters = sequence.as_slice();
        Ok(match (parameters, final_byte) {
            (b"", b'A') => Some(Key::of(Code::Up)),
            (b"", b'B') => Some(Key::of(Code::Down)),
            (b"", b'C') => Some(Key::of(Code::Right)),
            (b"", b'D') => Some(Key::of(Code::Left)),
            (b"", b'H') => Some(Key::of(Code::Home)),
            (b"", b'F') => Some(Key::of(Code::End)),
            // Shift-Tab. The kitty protocol reports `Tab` with the shift bit
            // (`CSI 9 ; 2 u`); this is the form every other terminal sends.
            (b"", b'Z') => Some(Key::with(Code::Tab, Modifiers::SHIFT)),
            // Focus reporting: `ESC [ I` and `ESC [ O`. They are keys only in
            // the sense that the terminal sends them between keystrokes.
            (b"", b'I') => Some(Key::of(Code::FocusIn)),
            (b"", b'O') => Some(Key::of(Code::FocusOut)),
            // SGR mouse: `< button ; column ; row` then `M` for a press and
            // `m` for a release. Only the left button with no modifiers is an
            // editing gesture yet.
            (parameters, b'M') if parameters.first() == Some(&b'<') => {
                Some(mouse_left(&parameters[1..]))
            }
            (b"200", b'~') => Some(self.decode_paste(term)?),
            (parameters, final_byte) => self.decode_parameters(parameters, final_byte)?,
        })
    }

    /// Decode the parameterised forms: `CSI code ; mods u`,
    /// `CSI 1 ; mods A`, `CSI code ; mods ~` and xterm's
    /// `CSI 27 ; mods ; code ~`.
    ///
    /// Returning `None` means the sequence was a key event the editor must not
    /// act on, such as a release report.
    fn decode_parameters(&mut self, parameters: &[u8], final_byte: u8) -> io::Result<Option<Key>> {
        let Some(fields) = Fields::parse(parameters) else {
            return Ok(Some(Key::of(Code::Unknown(parameters.to_vec()))));
        };
        let key = match final_byte {
            b'u' => csi_u(&fields),
            // modifyOtherKeys: the first field is 27 (ESC's code), the second
            // the modifiers, the third the key's code point.
            b'~' if fields.get(0) == Some(27) && fields.get(2).is_some() => other_key(&fields),
            b'~' => cursor_key(&fields),
            b'A' | b'B' | b'C' | b'D' | b'H' | b'F' | b'Z'
                if fields.get(0).is_none_or(|number| number == 1) =>
            {
                functional_key(&fields, final_byte)
            }
            _ => Some(Key::of(Code::Unknown(parameters.to_vec()))),
        };
        Ok(key)
    }

    /// Decode an SS3 sequence: `ESC O <byte>`, the other encoding for arrows
    /// and Home/End.
    fn decode_ss3(&mut self, term: &mut dyn Terminal) -> io::Result<Key> {
        let Some(byte) = self.read(term)? else {
            return Ok(Key::of(Code::Unknown(vec![b'O'])));
        };
        Ok(match byte {
            b'A' => Key::of(Code::Up),
            b'B' => Key::of(Code::Down),
            b'C' => Key::of(Code::Right),
            b'D' => Key::of(Code::Left),
            b'H' => Key::of(Code::Home),
            b'F' => Key::of(Code::End),
            _ => Key::of(Code::Unknown(vec![b'O', byte])),
        })
    }

    /// Consume an operating-system command sent by the terminal.
    ///
    /// `ESC ]` on the way in is a reply: the shell asks the terminal for its
    /// background colour while the prompt is painted, and a command can ask
    /// for other things while it runs. A reply that arrives after the question
    /// gave up waiting is not text to insert — typing `11;rgb:…` into the line
    /// leaves it looking empty while holding bytes, and `Ctrl-D` then deletes
    /// instead of ending the session.
    ///
    /// A reply starts with a numeric code, which is how it is told apart from
    /// Alt-`]`: when the byte after `ESC ]` is not a digit it belongs to the
    /// next key and goes back in the queue. The wait is bounded, so a reply
    /// that stops mid-way costs one deadline rather than the line.
    fn decode_osc(&mut self, term: &mut dyn Terminal) -> io::Result<Option<Key>> {
        let deadline = std::time::Instant::now() + OSC_WAIT;
        let mut payload = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let byte = match term.read_byte_timeout(remaining)? {
                Some(byte) => byte,
                // Nothing came at all: this was Alt-`]`, not a reply.
                None if payload.is_empty() => return Ok(Some(Key::alt(']'))),
                // A reply that never finished. It is still not text.
                None => break,
            };
            if payload.is_empty() && !byte.is_ascii_digit() {
                self.pending.push_front(byte);
                return Ok(Some(Key::alt(']')));
            }
            match byte {
                // BEL and `ESC \` end a reply; the bytes between them are the
                // terminal's answer, not the user's typing.
                0x07 => break,
                0x1b => {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    match term.read_byte_timeout(remaining)? {
                        Some(b'\\') => break,
                        Some(other) => payload.extend_from_slice(&[0x1b, other]),
                        None => break,
                    }
                }
                byte => payload.push(byte),
            }
            if payload.len() >= MAX_OSC {
                break;
            }
        }
        Ok(Some(Key::of(Code::Unknown(payload))))
    }

    /// Read bracketed-paste payload up to `ESC [ 201 ~`.
    ///
    /// End of input ends the paste as well: an unterminated paste is still a
    /// paste, and dropping the bytes would lose the user's text.
    fn decode_paste(&mut self, term: &mut dyn Terminal) -> io::Result<Key> {
        let mut payload = Vec::new();
        loop {
            match self.read(term)? {
                None => return Ok(Key::of(Code::Paste(payload))),
                Some(byte) => payload.push(byte),
            }
            if payload.ends_with(PASTE_END) {
                payload.truncate(payload.len() - PASTE_END.len());
                return Ok(Key::of(Code::Paste(payload)));
            }
        }
    }
}

/// The key a legacy control byte stands for.
///
/// The mapping is the one every terminal uses: `Ctrl-A` is `0x01`, and the
/// letters run in order to `Ctrl-Z` at `0x1a`. The rest are the keys that have
/// no letter to name them. Tab, Enter, Backspace and Escape are their own keys
/// and are decoded before this function sees them.
fn control_key(byte: u8) -> Option<Key> {
    let character = match byte {
        // Ctrl-Space, also reported as Ctrl-@ by terminals that have no
        // separate space mapping.
        0x00 => ' ',
        0x09 | 0x0a | 0x0d | 0x1b => return None,
        0x1c => '\\',
        0x1d => ']',
        0x1e => '^',
        0x1f => '_',
        0x01..=0x1a => char::from(b'a' + byte - 0x01),
        _ => return None,
    };
    Some(Key::ctrl(character))
}

/// The numeric fields of a CSI parameter string, as far as the decoder reads
/// them.
///
/// The kitty protocol has subfields (`code:shifted:base`), so each field is
/// split on `:` and only its first subfield is used: the shifted and base
/// layout keys exist for shortcut matching across layouts, which this editor
/// does not do. The one subfield that matters is the event type after the
/// modifiers, because a release report must not edit the line.
#[derive(Clone, Copy, Default)]
struct Fields {
    values: [u32; 4],
    present: [bool; 4],
    /// `mods:event` — 1 press, 2 repeat, 3 release.
    event: Option<u32>,
}

impl Fields {
    /// Split `parameters` into fields, or `None` if it is not all numbers.
    fn parse(parameters: &[u8]) -> Option<Self> {
        let mut fields = Self::default();
        if parameters.is_empty() {
            return Some(fields);
        }
        for (position, field) in parameters.split(|&byte| byte == b';').enumerate() {
            if position >= fields.values.len() {
                return None;
            }
            let mut subfields = field.split(|&byte| byte == b':');
            let first = subfields.next()?;
            if !first.is_empty() {
                fields.values[position] = number(first)?;
                fields.present[position] = true;
            }
            if position == 1
                && let Some(event) = subfields.next()
                && !event.is_empty()
            {
                fields.event = Some(number(event)?);
            }
        }
        Some(fields)
    }

    /// Field `index` if it was present and non-empty.
    fn get(&self, index: usize) -> Option<u32> {
        match self.present.get(index).copied() {
            Some(true) => Some(self.values[index]),
            _ => None,
        }
    }

    /// The modifiers carried by the second field, defaulting to none.
    fn modifiers(&self) -> Modifiers {
        self.get(1)
            .map(Modifiers::from_encoded)
            .unwrap_or(Modifiers::NONE)
    }
}

/// Parse one decimal field, saturating rather than overflowing into a panic.
fn number(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() {
        return None;
    }
    let mut value: u32 = 0;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value
            .saturating_mul(10)
            .saturating_add(u32::from(byte - b'0'));
    }
    Some(value)
}

/// Decode the code point a protocol reports as a key.
///
/// The C0 values are the keys that keep their legacy meaning in both
/// protocols; everything else is a character. A code in the Private Use Area
/// is a functional key (F1, Left Shift, keypad) that this editor does not
/// bind, and must not be inserted as if it were text.
fn code_point(code: u32) -> Option<Code> {
    const FUNCTIONAL_START: u32 = 0xe000;
    const FUNCTIONAL_END: u32 = 0xf8ff;
    match code {
        9 => Some(Code::Tab),
        13 => Some(Code::Enter),
        27 => Some(Code::Esc),
        127 => Some(Code::Backspace),
        (FUNCTIONAL_START..=FUNCTIONAL_END) => None,
        _ => char::from_u32(code).map(Code::Char),
    }
}

/// `CSI code ; mods u` — the kitty protocol's canonical key event.
///
/// `None` for a release report: the editor edits on presses, and a terminal
/// that reports releases is only doing so because some other program asked it
/// to.
fn csi_u(fields: &Fields) -> Option<Key> {
    if fields.event == Some(3) {
        return None;
    }
    let Some(code) = fields.get(0) else {
        // A bare `CSI u` is SCORC, a cursor restore, not a key.
        return Some(Key::of(Code::Unknown(Vec::new())));
    };
    match code_point(code) {
        Some(code) => Some(Key::with(code, fields.modifiers())),
        None => Some(Key::of(Code::Unknown(Vec::new()))),
    }
}

/// `CSI 27 ; mods ; code ~` — xterm's `modifyOtherKeys`.
///
/// Level 1 is what the editor asks for: only keys with no legacy encoding are
/// reported this way, so the shifted-character soup of level 2 never arrives.
fn other_key(fields: &Fields) -> Option<Key> {
    let (Some(encoded), Some(code)) = (fields.get(1), fields.get(2)) else {
        return Some(Key::of(Code::Unknown(Vec::new())));
    };
    match code_point(code) {
        Some(code) => Some(Key::with(code, Modifiers::from_encoded(encoded))),
        None => Some(Key::of(Code::Unknown(Vec::new()))),
    }
}

/// `CSI number ; mods ~` for Home, End and Delete.
fn cursor_key(fields: &Fields) -> Option<Key> {
    let code = match fields.get(0) {
        Some(1 | 7) => Code::Home,
        Some(4 | 8) => Code::End,
        Some(3) => Code::Delete,
        _ => return Some(Key::of(Code::Unknown(Vec::new()))),
    };
    Some(Key::with(code, fields.modifiers()))
}

/// `CSI 1 ; mods A` and friends: the arrows and Home/End with modifiers.
fn functional_key(fields: &Fields, final_byte: u8) -> Option<Key> {
    let code = match final_byte {
        b'A' => Code::Up,
        b'B' => Code::Down,
        b'C' => Code::Right,
        b'D' => Code::Left,
        b'H' => Code::Home,
        b'F' => Code::End,
        b'Z' => Code::Tab,
        _ => return Some(Key::of(Code::Unknown(Vec::new()))),
    };
    let mut mods = fields.modifiers();
    if final_byte == b'Z' && mods.is_empty() {
        // Shift-Tab with the number written out and no modifier field.
        mods = Modifiers::SHIFT;
    }
    Some(Key::with(code, mods))
}

/// Decode the parameters of an SGR mouse press.
///
/// Anything that is not a plain left click — a release, a modified button, a
/// scroll wheel — is reported as an unknown sequence, which the editor
/// ignores.
fn mouse_left(parameters: &[u8]) -> Key {
    let mut numbers = [0u32; 3];
    let mut count = 0;
    for part in parameters.split(|&byte| byte == b';') {
        if count == numbers.len() || part.is_empty() {
            return Key::of(Code::Unknown(parameters.to_vec()));
        }
        let Some(value) = number(part) else {
            return Key::of(Code::Unknown(parameters.to_vec()));
        };
        numbers[count] = value;
        count += 1;
    }
    let [button, column, row] = numbers;
    if count != 3 || button != 0 || column == 0 || row == 0 {
        return Key::of(Code::Unknown(parameters.to_vec()));
    }
    Key::of(Code::MouseLeft {
        column: u16::try_from(column).unwrap_or(u16::MAX),
        row: u16::try_from(row).unwrap_or(u16::MAX),
    })
}

/// What a partial byte string says about a cursor-position report.
enum Report {
    /// Still a valid prefix; more bytes are needed.
    Incomplete,
    /// A complete `ESC [ row ; col R`, both coordinates one-based.
    Position(u16, u16),
    /// Not a report. The bytes belong to something else.
    Other,
}

/// Classify `bytes` as a prefix, a whole cursor-position report, or neither.
///
/// The grammar is strict on purpose: anything that is not exactly
/// `ESC [ digits ; digits R` is someone else's bytes, and the caller must read
/// them as keys instead.
fn cursor_report(bytes: &[u8]) -> Report {
    if bytes.is_empty() {
        return Report::Incomplete;
    }
    if bytes[0] != 0x1b {
        return Report::Other;
    }
    if bytes.len() == 1 {
        return Report::Incomplete;
    }
    if bytes[1] != b'[' {
        return Report::Other;
    }
    let params = &bytes[2..];
    if params.is_empty() {
        return Report::Incomplete;
    }
    let mut row: u32 = 0;
    let mut column: u32 = 0;
    for (index, &byte) in params.iter().enumerate() {
        match byte {
            b'0'..=b'9' => {
                let slot = if column > 0 || params[..index].contains(&b';') {
                    &mut column
                } else {
                    &mut row
                };
                *slot = slot
                    .saturating_mul(10)
                    .saturating_add(u32::from(byte - b'0'));
                if *slot > 9999 {
                    return Report::Other;
                }
            }
            b';' if row > 0 && column == 0 => {}
            b'R' if index + 1 == params.len() && row > 0 && column > 0 => {
                return Report::Position(
                    u16::try_from(row).unwrap_or(u16::MAX),
                    u16::try_from(column).unwrap_or(u16::MAX),
                );
            }
            _ => return Report::Other,
        }
    }
    Report::Incomplete
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

        fn read_byte_timeout(&mut self, _timeout: std::time::Duration) -> io::Result<Option<u8>> {
            self.read_byte()
        }

        fn reports_cursor_position(&self) -> bool {
            true
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
                Key::char('a'),
                Key::of(Code::Tab),
                Key::of(Code::Enter),
                Key::ctrl('a'),
                Key::of(Code::Backspace)
            ]
        );
    }

    #[test]
    fn legacy_control_bytes_name_the_letter_they_hold() {
        // The mapping both protocols use: Ctrl-A is 0x01 and the alphabet runs
        // in order. Ctrl-Space is the NUL byte, and the four punctuation keys
        // that have a control byte of their own are the tail of the table.
        let cases: &[(u8, char)] = &[
            (0x00, ' '),
            (0x01, 'a'),
            (0x07, 'g'),
            (0x0b, 'k'),
            (0x12, 'r'),
            (0x1a, 'z'),
            (0x1c, '\\'),
            (0x1d, ']'),
            (0x1e, '^'),
            (0x1f, '_'),
        ];
        for &(byte, character) in cases {
            assert_eq!(
                keys(&[byte]),
                vec![Key::ctrl(character)],
                "byte {byte:#04x}"
            );
        }
    }

    #[test]
    fn arrows_have_both_encodings() {
        assert_eq!(
            keys(b"\x1b[A\x1b[B\x1b[C\x1b[D\x1bOA\x1bOB\x1bOC\x1bOD"),
            vec![
                Key::of(Code::Up),
                Key::of(Code::Down),
                Key::of(Code::Right),
                Key::of(Code::Left),
                Key::of(Code::Up),
                Key::of(Code::Down),
                Key::of(Code::Right),
                Key::of(Code::Left)
            ]
        );
    }

    #[test]
    fn home_end_and_delete() {
        assert_eq!(
            keys(b"\x1b[H\x1b[F\x1b[1~\x1b[4~\x1b[7~\x1b[8~\x1b[3~"),
            vec![
                Key::of(Code::Home),
                Key::of(Code::End),
                Key::of(Code::Home),
                Key::of(Code::End),
                Key::of(Code::Home),
                Key::of(Code::End),
                Key::of(Code::Delete)
            ]
        );
    }

    #[test]
    fn shift_tab_is_its_own_key() {
        assert_eq!(
            keys(b"\x1b[Z\x1b[1;2Z\x1b[9;2u"),
            vec![
                Key::with(Code::Tab, Modifiers::SHIFT),
                Key::with(Code::Tab, Modifiers::SHIFT),
                Key::with(Code::Tab, Modifiers::SHIFT)
            ]
        );
    }

    #[test]
    fn alt_letters() {
        assert_eq!(
            keys(b"\x1bb\x1bf\x1bd"),
            vec![Key::alt('b'), Key::alt('f'), Key::alt('d')]
        );
    }

    #[test]
    fn alt_backspace_is_either_del_or_bs() {
        // Terminals disagree: xterm sends `ESC DEL`, some send `ESC BS`. Both
        // are readline's `backward-kill-word`, so both decode to one key.
        assert_eq!(
            keys(b"\x1b\x7f\x1b\x08"),
            vec![
                Key::with(Code::Backspace, Modifiers::ALT),
                Key::with(Code::Backspace, Modifiers::ALT)
            ]
        );
    }

    #[test]
    fn a_lone_escape_at_end_of_input() {
        assert_eq!(keys(b"\x1b"), vec![Key::of(Code::Esc)]);
    }

    #[test]
    fn a_truncated_sequence_is_ignored_whole() {
        assert_eq!(
            keys(b"\x1b[3"),
            vec![Key::of(Code::Unknown(b"3".to_vec()))],
            "a partial CSI must not insert its parameters as text"
        );
        assert_eq!(keys(b"\x1b["), vec![Key::of(Code::Unknown(Vec::new()))]);
        assert_eq!(keys(b"\x1bO"), vec![Key::of(Code::Unknown(b"O".to_vec()))]);
    }

    #[test]
    fn a_control_byte_aborts_a_sequence_and_is_not_swallowed() {
        // The decoder consumes `[3`, aborts on the carriage return, and gives
        // the Enter back as the next key.
        assert_eq!(
            keys(b"\x1b[3\r"),
            vec![Key::of(Code::Unknown(b"3".to_vec())), Key::of(Code::Enter)]
        );
    }

    #[test]
    fn a_cursor_report_is_parsed() {
        let mut terminal = Script::new(b"\x1b[12;5R");
        let mut decoder = Decoder::new();
        assert_eq!(
            decoder
                .cursor_position(&mut terminal, std::time::Duration::from_millis(50))
                .unwrap(),
            Some((12, 5))
        );
    }

    #[test]
    fn bytes_that_are_not_a_cursor_report_are_put_back() {
        // The query reads ahead; a keystroke that arrived meanwhile must reach
        // the line, and the report that follows is then just an unknown key.
        let mut terminal = Script::new(b"ab\x1b[1;1R");
        let mut decoder = Decoder::new();
        assert_eq!(
            decoder
                .cursor_position(&mut terminal, std::time::Duration::from_millis(50))
                .unwrap(),
            None
        );
        let mut keys = Vec::new();
        while let Some(key) = decoder.next(&mut terminal).unwrap() {
            keys.push(key);
        }
        assert_eq!(
            keys,
            vec![
                Key::char('a'),
                Key::char('b'),
                Key::of(Code::Unknown(b"1;1".to_vec()))
            ]
        );
    }

    #[test]
    fn a_truncated_report_is_not_a_report() {
        assert!(matches!(cursor_report(b"\x1b[1;"), Report::Incomplete));
        assert!(matches!(cursor_report(b"\x1b[1;2"), Report::Incomplete));
        assert!(matches!(
            cursor_report(b"\x1b[1;2R"),
            Report::Position(1, 2)
        ));
        assert!(matches!(cursor_report(b"\x1b[1R"), Report::Other));
        assert!(matches!(cursor_report(b"\x1b[;2R"), Report::Other));
        assert!(matches!(cursor_report(b"\x1b[A"), Report::Other));
        assert!(matches!(cursor_report(b"x"), Report::Other));
    }

    #[test]
    fn a_left_click_is_a_mouse_key_and_nothing_else_is() {
        assert_eq!(
            keys(b"\x1b[<0;12;5M"),
            vec![Key::of(Code::MouseLeft { column: 12, row: 5 })]
        );
        // Release, modified buttons (`4` is shift+left) and scroll (`64`) are
        // not editing gestures.
        for sequence in [&b"\x1b[<0;12;5m"[..], b"\x1b[<4;12;5M", b"\x1b[<64;12;5M"] {
            assert!(
                matches!(
                    keys(sequence).as_slice(),
                    [Key {
                        code: Code::Unknown(_),
                        ..
                    }]
                ),
                "{:?}",
                sequence
            );
        }
    }

    #[test]
    fn focus_events_are_decoded() {
        assert_eq!(
            keys(b"\x1b[I\x1b[O"),
            vec![Key::of(Code::FocusIn), Key::of(Code::FocusOut)]
        );
        // Not to be confused with SS3 `ESC O`, which is an arrow key prefix.
        assert_eq!(keys(b"\x1bOA"), vec![Key::of(Code::Up)]);
    }

    #[test]
    fn bracketed_paste_is_verbatim() {
        let pasted = b"echo \x1b[A hi\nsecond";
        let mut input = b"\x1b[200~".to_vec();
        input.extend_from_slice(pasted);
        input.extend_from_slice(b"\x1b[201~");
        assert_eq!(keys(&input), vec![Key::of(Code::Paste(pasted.to_vec()))]);
    }

    #[test]
    fn an_unterminated_paste_still_arrives() {
        assert_eq!(
            keys(b"\x1b[200~text"),
            vec![Key::of(Code::Paste(b"text".to_vec()))]
        );
    }

    #[test]
    fn multibyte_utf8_is_one_key() {
        assert_eq!(
            keys("é€😀".as_bytes()),
            vec![Key::char('é'), Key::char('€'), Key::char('😀')]
        );
    }

    #[test]
    fn invalid_utf8_is_a_byte_and_does_not_eat_the_next_key() {
        assert_eq!(keys(b"\xff"), vec![Key::byte(0xff)]);
        assert_eq!(
            keys(b"\xc3("),
            vec![Key::byte(0xc3), Key::char('(')],
            "the non-continuation byte is pushed back"
        );
    }

    #[test]
    fn csi_u_names_the_key_that_legacy_bytes_cannot() {
        // The whole point of asking for the protocol: `Ctrl-I` is no longer
        // Tab, and `Ctrl-M` is no longer Enter. The protocol always reports
        // the unshifted code point and puts shift in the modifier field.
        assert_eq!(
            keys(b"\x1b[105;5u\x1b[109;5u\x1b[114;6u\x1b[97;3u"),
            vec![
                Key::ctrl('i'),
                Key::ctrl('m'),
                Key::with(Code::Char('r'), Modifiers::CTRL | Modifiers::SHIFT),
                Key::alt('a'),
            ]
        );
    }

    #[test]
    fn csi_u_carries_the_keys_that_keep_a_legacy_byte() {
        // Escape, Enter, Tab and Backspace keep their bytes in the legacy
        // protocol; the unambiguous one reports them as code points.
        assert_eq!(
            keys(b"\x1b[27u\x1b[13u\x1b[9u\x1b[127u\x1b[32;5u"),
            vec![
                Key::of(Code::Esc),
                Key::of(Code::Enter),
                Key::of(Code::Tab),
                Key::of(Code::Backspace),
                Key::ctrl(' '),
            ]
        );
    }

    #[test]
    fn csi_u_ignores_alternate_keys_but_reads_the_event_type() {
        // `97;6;65u` is Ctrl-Shift-A with the shifted key reported as `A`.
        // The shifted key would matter to a shortcut matcher that wanted
        // `Ctrl-+` to match `Ctrl-Shift-=`; this editor matches on the
        // modifier bits, so it is skipped.
        assert_eq!(
            keys(b"\x1b[97;6;65u"),
            vec![Key::with(
                Code::Char('a'),
                Modifiers::CTRL | Modifiers::SHIFT
            )]
        );
        // Press and repeat edit; release does nothing and must not swallow the
        // key after it.
        assert_eq!(
            keys(b"\x1b[105;5:2u"),
            vec![Key::ctrl('i')],
            "a repeat is a press"
        );
        assert_eq!(
            keys(b"\x1b[105;5:3ux"),
            vec![Key::char('x')],
            "a release is not an edit"
        );
    }

    #[test]
    fn csi_u_functional_keys_are_not_inserted_as_text() {
        // F1 is U+E000 + 1 in the functional range; a terminal that reports
        // every key sends it, and it is not a character to put in the line.
        assert!(matches!(
            keys(b"\x1b[57353;5u").as_slice(),
            [Key {
                code: Code::Unknown(_),
                ..
            }]
        ));
        // A bare `CSI u` is SCORC, not a key.
        assert!(matches!(
            keys(b"\x1b[u").as_slice(),
            [Key {
                code: Code::Unknown(_),
                ..
            }]
        ));
    }

    #[test]
    fn modified_functional_keys_carry_their_modifiers() {
        assert_eq!(
            keys(b"\x1b[1;5C\x1b[1;3D\x1b[1;5H\x1b[3;5~\x1b[3;2~"),
            vec![
                Key::with(Code::Right, Modifiers::CTRL),
                Key::with(Code::Left, Modifiers::ALT),
                Key::with(Code::Home, Modifiers::CTRL),
                Key::with(Code::Delete, Modifiers::CTRL),
                Key::with(Code::Delete, Modifiers::SHIFT),
            ]
        );
    }

    #[test]
    fn modify_other_keys_reports_what_has_no_legacy_encoding() {
        // `CSI 27 ; mods ; code ~`: Ctrl-Tab, Alt-. and Ctrl-Shift-I.
        assert_eq!(
            keys(b"\x1b[27;5;9~\x1b[27;3;46~\x1b[27;6;105~"),
            vec![
                Key::with(Code::Tab, Modifiers::CTRL),
                Key::alt('.'),
                Key::with(Code::Char('i'), Modifiers::CTRL | Modifiers::SHIFT),
            ]
        );
    }

    #[test]
    fn a_modifier_field_of_one_means_none() {
        assert_eq!(
            keys(b"\x1b[1;1D"),
            vec![Key::with(Code::Left, Modifiers::NONE)]
        );
        assert_eq!(Modifiers::from_encoded(1), Modifiers::NONE);
        assert_eq!(
            Modifiers::from_encoded(2 | 4 | 1),
            Modifiers::CTRL | Modifiers::ALT
        );
        // Zero is not a valid encoding; it is read as no modifiers rather
        // than losing the key.
        assert_eq!(Modifiers::from_encoded(0), Modifiers::NONE);
    }

    #[test]
    fn the_decoder_keeps_lock_state_and_a_binding_drops_it() {
        // ghostty reports the locks in the modifier field, so `Ctrl-D` reads
        // `CSI 100 ; 133 u` with Num Lock on. The bits are decoded — they are
        // what the terminal said — and dropped when a binding is matched.
        assert_eq!(
            keys(b"\x1b[100;133u\x1b[98;131u\x1b[97;65u"),
            vec![
                Key::with(Code::Char('d'), Modifiers::CTRL | Modifiers::NUM_LOCK),
                Key::with(Code::Char('b'), Modifiers::ALT | Modifiers::NUM_LOCK),
                Key::with(Code::Char('a'), Modifiers::CAPS_LOCK),
            ]
        );
        assert_eq!(
            (Modifiers::CTRL | Modifiers::NUM_LOCK | Modifiers::CAPS_LOCK).binding(),
            Modifiers::CTRL
        );
        assert_eq!(
            (Modifiers::SHIFT | Modifiers::NUM_LOCK).binding(),
            Modifiers::SHIFT
        );
        assert_eq!(Modifiers::NUM_LOCK.binding(), Modifiers::NONE);
    }

    #[test]
    fn a_terminal_reply_is_not_typed_into_the_line() {
        // The prompt asks the terminal for its background colour; the answer
        // is `ESC ] 11 ; rgb:…` with BEL or `ESC \`. A reply that misses the
        // question's timeout used to be decoded as Alt-`]` plus text, which
        // left the line non-empty and made `Ctrl-D` delete instead of exit.
        assert_eq!(
            keys(b"\x1b]11;rgb:1f1f/1f1f/2828\x1b\\a"),
            vec![
                Key::of(Code::Unknown(b"11;rgb:1f1f/1f1f/2828".to_vec())),
                Key::char('a'),
            ],
            "a reply with an ESC-backslash terminator"
        );
        assert_eq!(
            keys(b"\x1b]11;rgb:ffff/ffff/ffff\x07b"),
            vec![
                Key::of(Code::Unknown(b"11;rgb:ffff/ffff/ffff".to_vec())),
                Key::char('b'),
            ],
            "a reply with a BEL terminator"
        );
        // `ESC ]` from a keyboard is Alt-`]`: the byte after it is not a digit
        // and belongs to the next key.
        assert_eq!(
            keys(b"\x1b]x"),
            vec![Key::alt(']'), Key::char('x')],
            "Alt-] is still a key"
        );
    }

    #[test]
    fn a_malformed_parameter_string_is_unknown_not_a_key() {
        for sequence in [
            // `?` is a parameter byte; a letter would have ended the CSI.
            &b"\x1b[1;?u"[..],
            b"\x1b[;;u",
            b"\x1b[1;2;3;4;5;6u",
            b"\x1b[1;?~",
        ] {
            assert!(
                matches!(
                    keys(sequence).as_slice(),
                    [Key {
                        code: Code::Unknown(_),
                        ..
                    }]
                ),
                "{:?}",
                sequence
            );
        }
    }

    #[test]
    fn modifiers_render_for_test_failures() {
        assert_eq!(format!("{:?}", Modifiers::NONE), "NONE");
        assert_eq!(
            format!("{:?}", Modifiers::CTRL | Modifiers::SHIFT),
            "SHIFT|CTRL"
        );
        assert_eq!(format!("{:?}", Modifiers::CAPS_LOCK), "CAPS_LOCK");
    }
}
