//! Translating key presses into the bytes a terminal program expects.
//!
//! The encoding is xterm's, which is what `TERM=xterm-256color` promises. It is not a
//! fixed table: several keys change meaning with the terminal's current mode, so
//! [`encode`] takes the live [`TermMode`] rather than deciding once at startup.
//!
//! Deliberately independent of any GUI toolkit — [`Key`] and [`Modifiers`] are our own —
//! so the encoding can be tested without a window, and so the SSH sessions in later
//! milestones reuse it unchanged.

use alacritty_terminal::term::TermMode;

/// A key the terminal understands, as identified by the GUI layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A character-producing key, already resolved through the keyboard layout.
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Insert,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    Up,
    Down,
    Right,
    Left,
    /// Function key, `1` for F1. Numbers beyond F20 are not encodable.
    Function(u8),
}

/// Modifier keys held during a key press.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    /// The Alt/Option key.
    pub alt: bool,
    pub ctrl: bool,
}

impl Modifiers {
    pub const NONE: Self = Self {
        shift: false,
        alt: false,
        ctrl: false,
    };

    fn is_empty(self) -> bool {
        self == Self::NONE
    }

    /// xterm's modifier parameter: a bitfield offset by one, so "no modifiers" is 1.
    fn param(self) -> u8 {
        1 + (self.shift as u8) + ((self.alt as u8) << 1) + ((self.ctrl as u8) << 2)
    }
}

/// The bytes to send for a key press, or `None` if the key sends nothing.
pub fn encode(key: Key, mods: Modifiers, mode: TermMode) -> Option<Vec<u8>> {
    match key {
        Key::Char(c) => encode_char(c, mods),

        // The cursor and editing keys split into two families. `CSI`-style keys take a
        // modifier parameter directly; `SS3`-style keys have no room for one, so they
        // switch to their CSI form as soon as a modifier is held.
        Key::Up => cursor_key(b'A', mods, mode),
        Key::Down => cursor_key(b'B', mods, mode),
        Key::Right => cursor_key(b'C', mods, mode),
        Key::Left => cursor_key(b'D', mods, mode),
        Key::Home => cursor_key(b'H', mods, mode),
        Key::End => cursor_key(b'F', mods, mode),

        Key::Insert => tilde_key(2, mods),
        Key::Delete => tilde_key(3, mods),
        Key::PageUp => tilde_key(5, mods),
        Key::PageDown => tilde_key(6, mods),

        Key::Function(n) => function_key(n, mods),

        Key::Enter => Some(prefix_alt(b"\r".to_vec(), mods)),
        Key::Tab => {
            if mods.shift {
                // Back-tab.
                Some(b"\x1b[Z".to_vec())
            } else {
                Some(prefix_alt(b"\t".to_vec(), mods))
            }
        }
        // DEL, not BS: this is what `stty erase` defaults to on every modern Unix, and
        // sending BS instead is the classic cause of backspace "not working" over SSH.
        Key::Backspace => {
            let byte: &[u8] = if mods.ctrl { b"\x08" } else { b"\x7f" };
            Some(prefix_alt(byte.to_vec(), mods))
        }
        Key::Escape => Some(prefix_alt(b"\x1b".to_vec(), mods)),
    }
}

/// Wrap text for bracketed paste when the program asked for it.
///
/// Without the brackets a program cannot tell pasted text from typing, so a pasted
/// newline runs a command — the reason pasting into a shell can be dangerous.
pub fn encode_paste(text: &str, mode: TermMode) -> Vec<u8> {
    // Normalise line endings: a terminal delivers Return as CR, and a pasted LF would
    // otherwise be seen as a different key.
    let text = text.replace("\r\n", "\r").replace('\n', "\r");

    if mode.contains(TermMode::BRACKETED_PASTE) {
        let mut out = Vec::with_capacity(text.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        // The terminating sequence must not appear inside the payload, or the paste
        // ends early and the rest is executed as input.
        out.extend_from_slice(text.replace("\x1b[201~", "").as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        text.into_bytes()
    }
}

fn encode_char(c: char, mods: Modifiers) -> Option<Vec<u8>> {
    let mut bytes = if mods.ctrl {
        // Control characters exist only for a subset of keys; for the rest, Ctrl is
        // ignored and the plain character is sent.
        match control_code(c) {
            Some(code) => vec![code],
            None => c.to_string().into_bytes(),
        }
    } else {
        c.to_string().into_bytes()
    };

    if mods.alt {
        bytes = prefix_alt(bytes, mods);
    }
    Some(bytes)
}

/// The C0 control character produced by Ctrl with `c`, following xterm.
fn control_code(c: char) -> Option<u8> {
    match c {
        // Ctrl-A..Ctrl-Z, case-insensitive.
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        // The remaining C0 codes, on the keys xterm assigns them to.
        '@' | ' ' => Some(0x00),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' | '?' => Some(0x1f),
        // Ctrl-/ is another spelling of Ctrl-_ on many layouts.
        '/' => Some(0x1f),
        _ => None,
    }
}

/// Alt is transmitted as an ESC prefix, the convention every Unix shell expects.
fn prefix_alt(bytes: Vec<u8>, mods: Modifiers) -> Vec<u8> {
    if mods.alt {
        let mut out = Vec::with_capacity(bytes.len() + 1);
        out.push(0x1b);
        out.extend_from_slice(&bytes);
        out
    } else {
        bytes
    }
}

/// Arrows plus Home/End: `SS3` form in application mode, `CSI` otherwise, and always
/// `CSI` once a modifier is involved because `SS3` cannot carry one.
fn cursor_key(final_byte: u8, mods: Modifiers, mode: TermMode) -> Option<Vec<u8>> {
    if mods.is_empty() {
        let introducer: &[u8] = if mode.contains(TermMode::APP_CURSOR) {
            b"\x1bO"
        } else {
            b"\x1b["
        };
        let mut out = introducer.to_vec();
        out.push(final_byte);
        Some(out)
    } else {
        Some(format!("\x1b[1;{}{}", mods.param(), final_byte as char).into_bytes())
    }
}

/// Keys encoded as `CSI <number> ~`, with the modifier as a second parameter.
fn tilde_key(number: u8, mods: Modifiers) -> Option<Vec<u8>> {
    if mods.is_empty() {
        Some(format!("\x1b[{number}~").into_bytes())
    } else {
        Some(format!("\x1b[{number};{}~", mods.param()).into_bytes())
    }
}

fn function_key(n: u8, mods: Modifiers) -> Option<Vec<u8>> {
    // F1-F4 are SS3 keys; the rest are numbered CSI-tilde keys, on a sequence with two
    // gaps in it (there is no 16, 22, 27, 30 or 35) for historical reasons.
    match n {
        1..=4 => {
            let final_byte = b'P' + (n - 1);
            if mods.is_empty() {
                Some(vec![0x1b, b'O', final_byte])
            } else {
                Some(format!("\x1b[1;{}{}", mods.param(), final_byte as char).into_bytes())
            }
        }
        5..=20 => {
            const NUMBERS: [u8; 16] = [
                15, 17, 18, 19, 20, 21, 23, 24, 25, 26, 28, 29, 31, 32, 33, 34,
            ];
            tilde_key(NUMBERS[usize::from(n - 5)], mods)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(key: Key, mods: Modifiers) -> String {
        String::from_utf8(encode(key, mods, TermMode::empty()).unwrap()).unwrap()
    }

    fn enc_mode(key: Key, mods: Modifiers, mode: TermMode) -> String {
        String::from_utf8(encode(key, mods, mode).unwrap()).unwrap()
    }

    const CTRL: Modifiers = Modifiers {
        ctrl: true,
        alt: false,
        shift: false,
    };
    const ALT: Modifiers = Modifiers {
        ctrl: false,
        alt: true,
        shift: false,
    };
    const SHIFT: Modifiers = Modifiers {
        ctrl: false,
        alt: false,
        shift: true,
    };

    #[test]
    fn plain_characters_pass_through() {
        assert_eq!(enc(Key::Char('a'), Modifiers::NONE), "a");
        // Multi-byte characters must survive as UTF-8.
        assert_eq!(enc(Key::Char('é'), Modifiers::NONE), "é");
        assert_eq!(enc(Key::Char('日'), Modifiers::NONE), "日");
    }

    #[test]
    fn control_characters() {
        assert_eq!(
            encode(Key::Char('c'), CTRL, TermMode::empty()).unwrap(),
            vec![0x03]
        );
        // Case must not matter: Ctrl-Shift-C is still Ctrl-C.
        assert_eq!(
            encode(Key::Char('C'), CTRL, TermMode::empty()).unwrap(),
            vec![0x03]
        );
        assert_eq!(
            encode(Key::Char('d'), CTRL, TermMode::empty()).unwrap(),
            vec![0x04]
        );
        assert_eq!(
            encode(Key::Char('['), CTRL, TermMode::empty()).unwrap(),
            vec![0x1b]
        );
        assert_eq!(
            encode(Key::Char(' '), CTRL, TermMode::empty()).unwrap(),
            vec![0x00]
        );
        // A key with no control code sends the character unchanged.
        assert_eq!(enc(Key::Char('1'), CTRL), "1");
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(enc(Key::Char('b'), ALT), "\x1bb");
        assert_eq!(enc(Key::Enter, ALT), "\x1b\r");
        // Ctrl and Alt together: the control code, still escape-prefixed.
        let both = Modifiers {
            ctrl: true,
            alt: true,
            shift: false,
        };
        assert_eq!(
            encode(Key::Char('c'), both, TermMode::empty()).unwrap(),
            vec![0x1b, 0x03]
        );
    }

    #[test]
    fn backspace_sends_del_not_backspace() {
        // The single most common source of a broken backspace over SSH.
        assert_eq!(
            encode(Key::Backspace, Modifiers::NONE, TermMode::empty()).unwrap(),
            vec![0x7f]
        );
        assert_eq!(
            encode(Key::Backspace, CTRL, TermMode::empty()).unwrap(),
            vec![0x08]
        );
    }

    #[test]
    fn arrows_follow_application_cursor_mode() {
        assert_eq!(enc(Key::Up, Modifiers::NONE), "\x1b[A");
        assert_eq!(
            enc_mode(Key::Up, Modifiers::NONE, TermMode::APP_CURSOR),
            "\x1bOA"
        );
        // A modifier forces the CSI form even in application mode, since SS3 has no
        // room for a parameter.
        assert_eq!(enc_mode(Key::Up, CTRL, TermMode::APP_CURSOR), "\x1b[1;5A");
    }

    #[test]
    fn modifier_parameters_match_xterm() {
        assert_eq!(enc(Key::Left, SHIFT), "\x1b[1;2D");
        assert_eq!(enc(Key::Left, ALT), "\x1b[1;3D");
        assert_eq!(enc(Key::Left, CTRL), "\x1b[1;5D");
        let ctrl_shift = Modifiers {
            ctrl: true,
            alt: false,
            shift: true,
        };
        assert_eq!(enc(Key::Left, ctrl_shift), "\x1b[1;6D");
        let all = Modifiers {
            ctrl: true,
            alt: true,
            shift: true,
        };
        assert_eq!(enc(Key::Left, all), "\x1b[1;8D");
    }

    #[test]
    fn home_and_end() {
        assert_eq!(enc(Key::Home, Modifiers::NONE), "\x1b[H");
        assert_eq!(enc(Key::End, Modifiers::NONE), "\x1b[F");
        assert_eq!(
            enc_mode(Key::Home, Modifiers::NONE, TermMode::APP_CURSOR),
            "\x1bOH"
        );
    }

    #[test]
    fn editing_keys() {
        assert_eq!(enc(Key::Insert, Modifiers::NONE), "\x1b[2~");
        assert_eq!(enc(Key::Delete, Modifiers::NONE), "\x1b[3~");
        assert_eq!(enc(Key::PageUp, Modifiers::NONE), "\x1b[5~");
        assert_eq!(enc(Key::PageDown, Modifiers::NONE), "\x1b[6~");
        assert_eq!(enc(Key::Delete, CTRL), "\x1b[3;5~");
    }

    #[test]
    fn function_keys() {
        assert_eq!(enc(Key::Function(1), Modifiers::NONE), "\x1bOP");
        assert_eq!(enc(Key::Function(4), Modifiers::NONE), "\x1bOS");
        assert_eq!(enc(Key::Function(5), Modifiers::NONE), "\x1b[15~");
        // The numbering skips 16: F6 is 17, not 16.
        assert_eq!(enc(Key::Function(6), Modifiers::NONE), "\x1b[17~");
        assert_eq!(enc(Key::Function(12), Modifiers::NONE), "\x1b[24~");
        assert_eq!(enc(Key::Function(1), CTRL), "\x1b[1;5P");
        assert_eq!(
            encode(Key::Function(21), Modifiers::NONE, TermMode::empty()),
            None
        );
    }

    #[test]
    fn shift_tab_is_back_tab() {
        assert_eq!(enc(Key::Tab, SHIFT), "\x1b[Z");
        assert_eq!(enc(Key::Tab, Modifiers::NONE), "\t");
    }

    #[test]
    fn paste_is_bracketed_only_when_requested() {
        assert_eq!(encode_paste("hi", TermMode::empty()), b"hi".to_vec());
        assert_eq!(
            encode_paste("hi", TermMode::BRACKETED_PASTE),
            b"\x1b[200~hi\x1b[201~".to_vec()
        );
    }

    #[test]
    fn paste_normalises_newlines() {
        assert_eq!(
            encode_paste("a\r\nb\nc", TermMode::empty()),
            b"a\rb\rc".to_vec()
        );
    }

    #[test]
    fn paste_cannot_be_escaped_by_its_own_terminator() {
        // Text containing the end marker must not be able to close the paste early and
        // have the remainder run as typed input.
        let hostile = "safe\x1b[201~rm -rf /\r";
        let encoded = encode_paste(hostile, TermMode::BRACKETED_PASTE);
        let encoded = String::from_utf8(encoded).unwrap();
        assert_eq!(encoded.matches("\x1b[201~").count(), 1);
        assert!(encoded.ends_with("\x1b[201~"));
    }
}
