//! Terminal announcements that are written *outside* the prompt.
//!
//! The prompt itself is the user's or the stock one; these are messages to the
//! terminal that must not be part of it. They have no display width, a user's
//! `PS1` has no way to express them, and they belong to the prompt event — not
//! to a repaint — so the editor writes them once per line.
//!
//! **OSC 7** (`ESC ] 7 ; file://HOST/PATH BEL`) tells the terminal which
//! directory the prompt belongs to. Terminals use it for a new tab, split or
//! tmux pane starting where the user is, and for session restore. It is sent
//! on every prompt rather than only after `cd`, because a child process can
//! leave the terminal's copy stale while the shell's `PWD` is right: an `ssh`
//! session that ends, a container, a full-screen program that changed
//! directory.
//!
//! The payload is a file URI, so it is not a shell string:
//!
//! * the host is included, and is not `localhost` — the terminal may be on a
//!   different machine than the shell;
//! * the path is percent-encoded **byte by byte**, so a name that is not
//!   UTF-8 still reproduces exactly at the other end;
//! * only `A-Z a-z 0-9 / . ~ - _` stay literal (plus `:` for a Windows drive
//!   letter). That is stricter than RFC 3986, which would leave `?`, `#`, `@`
//!   and `&` alone; those are legal in a POSIX filename and would turn a
//!   directory name into a query or fragment. Percent-encoding them is always
//!   valid in a URI and never loses information.
//!
//! Windows also gets ConEmu's `OSC 9;9;"NATIVE\PATH"` form, which Windows
//! Terminal reads for the same purpose. That half is best-effort: the native
//! Windows path is not a URI and is written as-is between quotes.

use crate::paths::Flavour;

/// The OSC 7 report for `cwd`, or `None` when the path has no URI form.
///
/// `None` is a real answer, not an error: a relative path (no `PWD`), an empty
/// one, or a Windows UNC path (`\\server\share`) cannot be expressed as
/// `file://HOST/...` without inventing a host, so the caller writes nothing
/// rather than a wrong hint.
pub fn cwd_report(cwd: &[u8], host: &[u8], flavour: Flavour) -> Option<Vec<u8>> {
    let path = uri_path(cwd, flavour)?;
    let mut report = Vec::with_capacity(path.len() + host.len() + 8);
    report.extend_from_slice(b"\x1b]7;file://");
    percent_encode_into(host, flavour, &mut report);
    report.extend_from_slice(&path);
    // BEL, not ST: both are legal, and the terminal in front of the user is
    // the one that has to accept it.
    report.push(0x07);
    // `OSC 9;9` is ConEmu's, and Windows Terminal reads it too. The path is
    // native (backslashes, drive letter) and quoted, so no escaping is
    // possible — `"` cannot appear in a Win32 file name.
    if flavour == Flavour::Windows {
        report.extend_from_slice(b"\x1b]9;9;\"");
        report.extend_from_slice(cwd);
        report.extend_from_slice(b"\"\x1b\\");
    }
    Some(report)
}

/// `OSC 133 ; A`: the prompt starts at this point.
///
/// Terminals use the four OSC 133 marks to treat a command line as a unit:
/// prompt-jump, "select this command's output", per-command cwd for new tabs,
/// and the exit status shown next to the prompt. A and B bracket the prompt, so
/// the terminal can tell the prompt text from what the user typed; C and D
/// bracket execution.
///
/// The `click_events=1` parameter is kitty's extension: it asks the terminal to
/// report mouse clicks on the prompt and command line as SGR mouse events, so
/// the editor can put the cursor where the user clicked. A terminal that does
/// not know the parameter ignores it and sends no events.
pub const PROMPT_START: &[u8] = b"\x1b]133;A;click_events=1\x1b\\";

/// `OSC 133 ; B`: the prompt ends here, and the command line begins.
pub const PROMPT_END: &[u8] = b"\x1b]133;B\x1b\\";

/// OSC 0 (window title) and OSC 1 (tab title), both set to `text`.
///
/// The two are separate so a terminal can put the shell's directory in the
/// tab and something else in the window; baash sets both, because a window
/// with one title and a tab with another is a configuration the shell cannot
/// guess. Control bytes become `?`: a directory name may contain `ESC`, and a
/// title is not a place to inject a sequence. Bytes `0x80..=0x9f` are left
/// alone — they are legal UTF-8 continuation bytes, and a title is not
/// required to be ASCII.
pub fn title(text: &[u8]) -> Vec<u8> {
    let text: Vec<u8> = text
        .iter()
        .map(|&byte| {
            if byte < 0x20 || byte == 0x7f {
                b'?'
            } else {
                byte
            }
        })
        .collect();
    let mut out = Vec::with_capacity(text.len() * 2 + 12);
    for selector in *b"01" {
        out.extend_from_slice(b"\x1b]");
        out.push(selector);
        out.push(b';');
        out.extend_from_slice(&text);
        out.push(0x07);
    }
    out
}

/// Whether an OSC 11 background-colour reply means a light terminal.
///
/// The reply is `ESC ] 11 ; rgb:RRRR/GGGG/BBBB BEL` (xterm uses four hex
/// digits; two are also common, and `rgba:` adds an alpha component that is
/// ignored). Returns `None` for anything else, so an unparseable or truncated
/// answer leaves the shell on its default palette rather than guessing.
pub fn background_is_light(reply: &[u8]) -> Option<bool> {
    let body = reply.strip_prefix(b"\x1b]11;")?;
    let body = body
        .strip_suffix(b"\x07")
        .or_else(|| body.strip_suffix(b"\x1b\\"))
        .unwrap_or(body);
    let colon = body.iter().position(|&byte| byte == b':')?;
    let (kind, value) = (&body[..colon], &body[colon + 1..]);
    if kind != b"rgb" && kind != b"rgba" {
        return None;
    }
    let mut components = value.split(|&byte| byte == b'/');
    let red = parse_hex(components.next()?)?;
    let green = parse_hex(components.next()?)?;
    let blue = parse_hex(components.next()?)?;
    let red = f32::from(u16::try_from(red).ok()?) / 65535.0;
    let green = f32::from(u16::try_from(green).ok()?) / 65535.0;
    let blue = f32::from(u16::try_from(blue).ok()?) / 65535.0;
    // Rec. 601 luma on the gamma-encoded values: good enough to separate a
    // white background from a black one, which is the only decision this
    // makes, and it does not need a colour-management library.
    let luma = 0.299 * red + 0.587 * green + 0.114 * blue;
    Some(luma > 0.5)
}

/// Parse one hex colour component (`f`, `ff`, `fff` or `ffff`) as 16-bit.
fn parse_hex(digits: &[u8]) -> Option<u32> {
    if digits.is_empty() || digits.len() > 4 {
        return None;
    }
    let mut value: u32 = 0;
    for &byte in digits {
        let digit = char::from(byte).to_digit(16)?;
        value = value * 16 + digit;
    }
    // Scale to 16 bits so `f`, `ff`, `fff` and `ffff` are the same colour:
    // each precision's own maximum maps to `0xffff`.
    let maximum = (1u32 << (4 * digits.len())) - 1;
    Some(value * 0xffff / maximum)
}

/// `OSC 133 ; C`: a command is starting; `command` is its text.
///
/// The text rides along as kitty's `cmdline_url` extension, percent-encoded so
/// a `;`, a newline or an `ESC` in the command cannot end the sequence early.
/// A terminal that does not know the parameter ignores it; the status is the
/// part every OSC 133 consumer reads.
pub fn command_start(command: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(command.len() + 24);
    out.extend_from_slice(b"\x1b]133;C;cmdline_url=");
    percent_encode_into(command, Flavour::Posix, &mut out);
    out.extend_from_slice(b"\x1b\\");
    out
}

/// `OSC 133 ; D`: the command finished, with its exit status.
pub fn command_finished(status: i32) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(b"\x1b]133;D;");
    out.extend_from_slice(status.to_string().as_bytes());
    out.extend_from_slice(b"\x1b\\");
    out
}

/// `OSC 52 ; c ; BASE64 BEL`: put `text` on the terminal's clipboard.
///
/// The clipboard belongs to the terminal, not to the process, which is what
/// makes this work over `ssh` where no local clipboard API exists. The payload
/// is base64 because the rest of the sequence is text: a newline, a `BEL` or a
/// non-UTF-8 byte would otherwise end it early. There is no acknowledgement in
/// the protocol, so delivery cannot be confirmed — a terminal may ignore the
/// sequence (xterm needs `allowWindowOps`) or refuse a payload above its own
/// size limit.
pub fn clipboard(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len().saturating_mul(4) / 3 + 16);
    out.extend_from_slice(b"\x1b]52;c;");
    base64_into(text, &mut out);
    out.push(0x07);
    out
}

/// Append `bytes` base64-encoded (RFC 4648 alphabet, `=` padding).
fn base64_into(bytes: &[u8], out: &mut Vec<u8>) {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in bytes.chunks(3) {
        let first = u32::from(chunk[0]);
        let second = chunk.get(1).copied().map(u32::from);
        let third = chunk.get(2).copied().map(u32::from);
        out.push(TABLE[(first >> 2) as usize]);
        out.push(TABLE[((first & 0x03) << 4 | second.unwrap_or(0) >> 4) as usize]);
        match (second, third) {
            (Some(second), Some(third)) => {
                out.push(TABLE[((second & 0x0f) << 2 | third >> 6) as usize]);
                out.push(TABLE[(third & 0x3f) as usize]);
            }
            (Some(second), None) => {
                out.push(TABLE[((second & 0x0f) << 2) as usize]);
                out.push(b'=');
            }
            (None, _) => {
                out.push(b'=');
                out.push(b'=');
            }
        }
    }
}

/// The path part of the URI, already encoded, with a leading `/`.
fn uri_path(cwd: &[u8], flavour: Flavour) -> Option<Vec<u8>> {
    if cwd.is_empty() {
        return None;
    }
    match flavour {
        Flavour::Posix => {
            if !cwd.starts_with(b"/") {
                return None;
            }
            let mut encoded = Vec::with_capacity(cwd.len());
            percent_encode_into(cwd, flavour, &mut encoded);
            Some(encoded)
        }
        Flavour::Windows => {
            // Only `C:\x` and `C:/x` are absolute on Windows. `C:x` is
            // drive-relative, `\x` is rooted but its drive is whatever the
            // process happens to be on, and `\\server\share` would need the
            // server as the URI's authority rather than the shell's host;
            // none of the three can be spelled truthfully here.
            let absolute = cwd.len() >= 3
                && cwd[0].is_ascii_alphabetic()
                && cwd[1] == b':'
                && (cwd[2] == b'\\' || cwd[2] == b'/');
            if !absolute {
                return None;
            }
            // `C:\a\b` and `C:/a/b` name the same directory; the URI wants
            // the latter and a leading slash, because the host comes first.
            let mut normalised = Vec::with_capacity(cwd.len() + 1);
            normalised.push(b'/');
            normalised.extend(cwd.iter().map(|&byte| match byte {
                b'\\' => b'/',
                byte => byte,
            }));
            let mut encoded = Vec::with_capacity(normalised.len());
            percent_encode_into(&normalised, flavour, &mut encoded);
            Some(encoded)
        }
    }
}

/// Append `bytes` percent-encoded to `out`.
fn percent_encode_into(bytes: &[u8], flavour: Flavour, out: &mut Vec<u8>) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &byte in bytes {
        let literal = byte.is_ascii_alphanumeric()
            || matches!(byte, b'/' | b'.' | b'~' | b'-' | b'_')
            || (flavour == Flavour::Windows && byte == b':');
        if literal {
            out.push(byte);
        } else {
            out.push(b'%');
            out.push(HEX[usize::from(byte >> 4)]);
            out.push(HEX[usize::from(byte & 0x0f)]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn posix(cwd: &[u8], host: &[u8]) -> Vec<u8> {
        cwd_report(cwd, host, Flavour::Posix).expect("a report")
    }

    #[test]
    fn a_command_start_carries_its_text_encoded() {
        assert_eq!(
            command_start(b"echo 'a;b'"),
            b"\x1b]133;C;cmdline_url=echo%20%27a%3Bb%27\x1b\\".to_vec()
        );
    }

    #[test]
    fn a_command_end_carries_the_status() {
        assert_eq!(command_finished(0), b"\x1b]133;D;0\x1b\\".to_vec());
        assert_eq!(command_finished(130), b"\x1b]133;D;130\x1b\\".to_vec());
    }

    #[test]
    fn a_background_reply_says_light_or_dark() {
        assert_eq!(
            background_is_light(b"\x1b]11;rgb:ffff/ffff/ffff\x07"),
            Some(true)
        );
        assert_eq!(
            background_is_light(b"\x1b]11;rgb:0000/0000/0000\x07"),
            Some(false)
        );
        assert_eq!(
            background_is_light(b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\"),
            Some(true)
        );
        // Two-digit and one-digit components are the same colour.
        assert_eq!(background_is_light(b"\x1b]11;rgb:ff/ff/ff\x07"), Some(true));
        assert_eq!(background_is_light(b"\x1b]11;rgb:f/f/f\x07"), Some(true));
        // `rgba:` carries an alpha the decision ignores.
        assert_eq!(
            background_is_light(b"\x1b]11;rgba:0000/0000/0000/ffff\x07"),
            Some(false)
        );
        // Below mid grey is dark; a real terminal is rarely near the boundary.
        assert_eq!(
            background_is_light(b"\x1b]11;rgb:7000/7000/7000\x07"),
            Some(false)
        );
        // Anything malformed leaves the palette alone.
        for reply in [
            &b"\x1b]11;?\x07"[..],
            b"\x1b]11;rgb:zz/00/00\x07",
            b"\x1b]11;rgb:ffff/ffff\x07",
            b"\x1b]11;cmyk:0/0/0/0\x07",
            b"\x1b]10;rgb:ffff/ffff/ffff\x07",
            b"nonsense",
        ] {
            assert_eq!(background_is_light(reply), None, "{reply:?}");
        }
    }

    #[test]
    fn the_clipboard_sequence_is_base64() {
        assert_eq!(clipboard(b""), b"\x1b]52;c;\x07".to_vec());
        assert_eq!(clipboard(b"f"), b"\x1b]52;c;Zg==\x07".to_vec());
        assert_eq!(clipboard(b"fo"), b"\x1b]52;c;Zm8=\x07".to_vec());
        assert_eq!(clipboard(b"foo"), b"\x1b]52;c;Zm9v\x07".to_vec());
        assert_eq!(
            clipboard(b"echo hi"),
            b"\x1b]52;c;ZWNobyBoaQ==\x07".to_vec()
        );
        // Bytes that are not text still round-trip; base64 is byte-oriented.
        assert_eq!(clipboard(b"\xff"), b"\x1b]52;c;/w==\x07".to_vec());
    }

    #[test]
    fn the_title_is_set_for_the_window_and_the_tab() {
        assert_eq!(
            title(b"~/src/lab"),
            b"\x1b]0;~/src/lab\x07\x1b]1;~/src/lab\x07".to_vec()
        );
    }

    #[test]
    fn control_bytes_in_a_title_cannot_start_a_sequence() {
        // A directory named with an `ESC` must not be able to append to the
        // title sequence and issue a command of its own.
        assert_eq!(
            title(b"a\x1b]2;owned\x07b\nc"),
            b"\x1b]0;a?]2;owned?b?c\x07\x1b]1;a?]2;owned?b?c\x07".to_vec()
        );
    }

    #[test]
    fn an_empty_title_is_still_two_sequences() {
        assert_eq!(title(b""), b"\x1b]0;\x07\x1b]1;\x07".to_vec());
    }

    #[test]
    fn a_posix_directory_becomes_a_file_uri() {
        assert_eq!(
            posix(b"/home/ada/src", b"lab"),
            b"\x1b]7;file://lab/home/ada/src\x07".to_vec()
        );
    }

    #[test]
    fn the_root_directory_is_a_uri_too() {
        assert_eq!(posix(b"/", b"lab"), b"\x1b]7;file://lab/\x07".to_vec());
    }

    #[test]
    fn spaces_and_uri_metacharacters_are_encoded() {
        // `?` and `#` are legal in a POSIX name and would otherwise start a
        // query or a fragment; `%` must survive a round trip.
        assert_eq!(
            posix(b"/tmp/a b?#c%d", b"lab"),
            b"\x1b]7;file://lab/tmp/a%20b%3F%23c%25d\x07".to_vec()
        );
    }

    #[test]
    fn bytes_that_are_not_utf8_are_encoded_one_by_one() {
        assert_eq!(
            posix(b"/tmp/\xff\xfe", b"lab"),
            b"\x1b]7;file://lab/tmp/%FF%FE\x07".to_vec()
        );
    }

    #[test]
    fn the_host_is_encoded_and_may_be_empty() {
        assert_eq!(posix(b"/x", b""), b"\x1b]7;file:///x\x07".to_vec());
        assert_eq!(posix(b"/x", b"a b"), b"\x1b]7;file://a%20b/x\x07".to_vec());
    }

    #[test]
    fn a_relative_path_has_no_report() {
        assert_eq!(cwd_report(b"src", b"lab", Flavour::Posix), None);
        assert_eq!(cwd_report(b"", b"lab", Flavour::Posix), None);
    }

    #[test]
    fn a_windows_drive_path_keeps_its_colon_and_gets_the_native_form() {
        let mut expected = b"\x1b]7;file://PC/C:/Users/ada%20x/src\x07".to_vec();
        expected.extend_from_slice(b"\x1b]9;9;\"C:\\Users\\ada x\\src\"\x1b\\");
        assert_eq!(
            cwd_report(b"C:\\Users\\ada x\\src", b"PC", Flavour::Windows),
            Some(expected)
        );
    }

    #[test]
    fn a_windows_path_with_forward_slashes_is_the_same_uri() {
        assert_eq!(
            cwd_report(b"C:/Users/ada", b"PC", Flavour::Windows)
                .expect("a report")
                .split(|&byte| byte == 0x07)
                .next()
                .expect("the OSC 7 half"),
            b"\x1b]7;file://PC/C:/Users/ada"
        );
    }

    #[test]
    fn the_paths_that_have_no_uri_are_not_guessed_at() {
        // UNC, drive-relative, drive-rooted, plain relative, empty.
        for path in [
            &b"\\\\server\\share\\x"[..],
            b"C:Users",
            b"\\Windows",
            b"src",
            b"",
        ] {
            assert_eq!(cwd_report(path, b"PC", Flavour::Windows), None);
        }
    }
}
