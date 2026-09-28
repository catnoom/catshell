//! Turning egui input events into the bytes a terminal program expects.
//!
//! The split between egui's `Text` and `Key` events matters here. A character that the
//! keyboard layout produced arrives as `Text` already resolved — dead keys composed,
//! AltGr applied — so that is what gets sent for ordinary typing. `Key` events carry the
//! things `Text` cannot express: arrows, function keys, and control combinations. Acting
//! on both for the same press would send every letter twice, so each event kind owns a
//! disjoint set of keys.

use catshell_term::keys::{self, Key, Modifiers};
use catshell_term::term::TermMode;

/// What a frame of input asks the app to do, beyond writing bytes to the terminal.
#[derive(Debug, Default, PartialEq)]
pub struct InputActions {
    /// Bytes to send to the focused terminal.
    pub bytes: Vec<u8>,
    /// The user asked to copy the selection.
    pub copy: bool,
}

/// Translate one frame's events for a terminal in `mode`.
///
/// `app_shortcut` is consulted first for each key press; when it returns true the key is
/// the app's (a new tab, a split) and is not forwarded to the program.
pub fn translate(
    events: &[egui::Event],
    mode: TermMode,
    mut app_shortcut: impl FnMut(egui::Key, egui::Modifiers) -> bool,
) -> InputActions {
    let mut actions = InputActions::default();

    for event in events {
        match event {
            egui::Event::Text(text) => {
                // egui also emits `Text` alongside some modified presses; those are
                // handled from the `Key` event, so ignore the duplicate here.
                if !text.is_empty() {
                    actions.bytes.extend_from_slice(text.as_bytes());
                }
            }

            egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => {
                if app_shortcut(*key, *modifiers) {
                    continue;
                }
                if let Some(bytes) = encode_key(*key, *modifiers, mode) {
                    actions.bytes.extend_from_slice(&bytes);
                }
            }

            egui::Event::Paste(text) => {
                actions
                    .bytes
                    .extend_from_slice(&keys::encode_paste(text, mode));
            }

            egui::Event::Copy | egui::Event::Cut => actions.copy = true,

            _ => {}
        }
    }

    actions
}

/// Encode a key press, or `None` if egui's `Text` event will carry it instead.
fn encode_key(key: egui::Key, modifiers: egui::Modifiers, mode: TermMode) -> Option<Vec<u8>> {
    let mods = Modifiers {
        shift: modifiers.shift,
        alt: modifiers.alt,
        // `ctrl` rather than `command`, so that Cmd on macOS stays an application
        // shortcut and does not become a control character.
        ctrl: modifiers.ctrl,
    };

    if let Some(special) = special_key(key) {
        return keys::encode(special, mods, mode);
    }

    // A character key only needs handling here when a modifier changes what it sends;
    // unmodified, the `Text` event already carries it, correctly for the user's layout.
    if mods.ctrl || mods.alt {
        let c = character(key)?;
        return keys::encode(Key::Char(c), mods, mode);
    }

    None
}

/// Keys with no character of their own.
fn special_key(key: egui::Key) -> Option<Key> {
    use egui::Key as E;
    Some(match key {
        E::ArrowUp => Key::Up,
        E::ArrowDown => Key::Down,
        E::ArrowLeft => Key::Left,
        E::ArrowRight => Key::Right,
        E::Enter => Key::Enter,
        E::Tab => Key::Tab,
        E::Backspace => Key::Backspace,
        E::Escape => Key::Escape,
        E::Insert => Key::Insert,
        E::Delete => Key::Delete,
        E::Home => Key::Home,
        E::End => Key::End,
        E::PageUp => Key::PageUp,
        E::PageDown => Key::PageDown,
        E::F1 => Key::Function(1),
        E::F2 => Key::Function(2),
        E::F3 => Key::Function(3),
        E::F4 => Key::Function(4),
        E::F5 => Key::Function(5),
        E::F6 => Key::Function(6),
        E::F7 => Key::Function(7),
        E::F8 => Key::Function(8),
        E::F9 => Key::Function(9),
        E::F10 => Key::Function(10),
        E::F11 => Key::Function(11),
        E::F12 => Key::Function(12),
        E::F13 => Key::Function(13),
        E::F14 => Key::Function(14),
        E::F15 => Key::Function(15),
        E::F16 => Key::Function(16),
        E::F17 => Key::Function(17),
        E::F18 => Key::Function(18),
        E::F19 => Key::Function(19),
        E::F20 => Key::Function(20),
        _ => return None,
    })
}

/// The unshifted character a key produces, for building control sequences.
///
/// Shift is not applied: `Ctrl+Shift+C` and `Ctrl+C` send the same control code, so the
/// lowercase form is the right input to the encoder.
fn character(key: egui::Key) -> Option<char> {
    use egui::Key as E;
    Some(match key {
        E::A => 'a',
        E::B => 'b',
        E::C => 'c',
        E::D => 'd',
        E::E => 'e',
        E::F => 'f',
        E::G => 'g',
        E::H => 'h',
        E::I => 'i',
        E::J => 'j',
        E::K => 'k',
        E::L => 'l',
        E::M => 'm',
        E::N => 'n',
        E::O => 'o',
        E::P => 'p',
        E::Q => 'q',
        E::R => 'r',
        E::S => 's',
        E::T => 't',
        E::U => 'u',
        E::V => 'v',
        E::W => 'w',
        E::X => 'x',
        E::Y => 'y',
        E::Z => 'z',
        E::Num0 => '0',
        E::Num1 => '1',
        E::Num2 => '2',
        E::Num3 => '3',
        E::Num4 => '4',
        E::Num5 => '5',
        E::Num6 => '6',
        E::Num7 => '7',
        E::Num8 => '8',
        E::Num9 => '9',
        E::Space => ' ',
        E::Minus => '-',
        E::Equals => '=',
        E::OpenBracket => '[',
        E::CloseBracket => ']',
        E::Backslash => '\\',
        E::Semicolon => ';',
        E::Quote => '\'',
        E::Comma => ',',
        E::Period => '.',
        E::Slash => '/',
        E::Backtick => '`',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_event(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    fn run(events: &[egui::Event]) -> InputActions {
        translate(events, TermMode::empty(), |_, _| false)
    }

    fn bytes(events: &[egui::Event]) -> Vec<u8> {
        run(events).bytes
    }

    #[test]
    fn typed_text_is_sent_as_is() {
        assert_eq!(
            bytes(&[egui::Event::Text("hello".into())]),
            b"hello".to_vec()
        );
        // Whatever the layout produced, including non-ASCII, goes through untouched.
        assert_eq!(
            bytes(&[egui::Event::Text("é".into())]),
            "é".as_bytes().to_vec()
        );
    }

    #[test]
    fn an_unmodified_letter_is_not_sent_twice() {
        // egui reports a typed letter as both a Key and a Text event. Only Text may act
        // on it, or every keystroke would be doubled.
        let events = [
            key_event(egui::Key::A, egui::Modifiers::default()),
            egui::Event::Text("a".into()),
        ];
        assert_eq!(bytes(&events), b"a".to_vec());
    }

    #[test]
    fn control_combinations_come_from_the_key_event() {
        let ctrl = egui::Modifiers {
            ctrl: true,
            ..Default::default()
        };
        assert_eq!(bytes(&[key_event(egui::Key::C, ctrl)]), vec![0x03]);
        assert_eq!(bytes(&[key_event(egui::Key::D, ctrl)]), vec![0x04]);
        // Shift must not change the control code.
        let ctrl_shift = egui::Modifiers {
            ctrl: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(bytes(&[key_event(egui::Key::C, ctrl_shift)]), vec![0x03]);
    }

    #[test]
    fn alt_combinations_are_escape_prefixed() {
        let alt = egui::Modifiers {
            alt: true,
            ..Default::default()
        };
        assert_eq!(bytes(&[key_event(egui::Key::B, alt)]), b"\x1bb".to_vec());
    }

    #[test]
    fn special_keys_are_encoded() {
        let none = egui::Modifiers::default();
        assert_eq!(
            bytes(&[key_event(egui::Key::ArrowUp, none)]),
            b"\x1b[A".to_vec()
        );
        assert_eq!(bytes(&[key_event(egui::Key::Enter, none)]), b"\r".to_vec());
        assert_eq!(bytes(&[key_event(egui::Key::Backspace, none)]), vec![0x7f]);
        assert_eq!(
            bytes(&[key_event(egui::Key::F5, none)]),
            b"\x1b[15~".to_vec()
        );
        assert_eq!(
            bytes(&[key_event(egui::Key::Home, none)]),
            b"\x1b[H".to_vec()
        );
    }

    #[test]
    fn application_cursor_mode_is_honoured() {
        let events = [key_event(egui::Key::ArrowUp, egui::Modifiers::default())];
        let actions = translate(&events, TermMode::APP_CURSOR, |_, _| false);
        assert_eq!(actions.bytes, b"\x1bOA".to_vec());
    }

    #[test]
    fn app_shortcuts_are_not_forwarded_to_the_program() {
        // Ctrl+Shift+T opens a tab; the shell must never see a control code for it.
        let modifiers = egui::Modifiers {
            ctrl: true,
            shift: true,
            ..Default::default()
        };
        let events = [key_event(egui::Key::T, modifiers)];
        let actions = translate(&events, TermMode::empty(), |key, mods| {
            key == egui::Key::T && mods.ctrl && mods.shift
        });
        assert!(actions.bytes.is_empty(), "shortcut leaked to the terminal");
    }

    #[test]
    fn paste_is_bracketed_when_the_program_asked() {
        let events = [egui::Event::Paste("ls\n".into())];
        let plain = translate(&events, TermMode::empty(), |_, _| false);
        assert_eq!(plain.bytes, b"ls\r".to_vec());

        let bracketed = translate(&events, TermMode::BRACKETED_PASTE, |_, _| false);
        assert_eq!(bracketed.bytes, b"\x1b[200~ls\r\x1b[201~".to_vec());
    }

    #[test]
    fn copy_is_reported_rather_than_sent() {
        let actions = run(&[egui::Event::Copy]);
        assert!(actions.copy);
        assert!(
            actions.bytes.is_empty(),
            "copy was forwarded to the program"
        );
    }

    #[test]
    fn key_releases_are_ignored() {
        let event = egui::Event::Key {
            key: egui::Key::A,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        };
        assert!(bytes(&[event]).is_empty());
    }

    #[test]
    fn events_are_applied_in_order() {
        let none = egui::Modifiers::default();
        let events = [
            egui::Event::Text("ls".into()),
            key_event(egui::Key::Enter, none),
        ];
        assert_eq!(bytes(&events), b"ls\r".to_vec());
    }
}
