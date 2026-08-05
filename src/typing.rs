//! Typing a string into whatever has the keyboard.
//!
//! The shell's on-screen keyboard and a paired phone produce whole strings, and the
//! apps that matter (a leanback web UI, a login form) only act on real key events.
//! The compositor owns the seat, so it can send them - but a keycode only produces
//! the character its keymap says it does, and no ordinary layout carries every
//! character a password or an accented Hungarian name needs.
//!
//! So the keymap is built for the string: one keycode per distinct character, that
//! character on the first level, nothing else. Load it, send the keys, put the old
//! one back. It is the same technique `wtype` uses, and it works with any client
//! because there is nothing to negotiate.
//!
//! The alternative was text-input-v3, which is implemented and left in place. It
//! needs the client to enable it and, at this smithay revision, an input-method
//! client to exist before anything is delivered; see `docs/ipc.md`.

use std::fmt::Write as _;

use smithay::input::keyboard::{KeyboardHandle, Keycode, XkbConfig};
use smithay::utils::SERIAL_COUNTER;

use crate::state::Tvbox;

/// xkb counts keycodes from 8, and 8 itself is reserved.
const FIRST_KEYCODE: u32 = 9;
/// A keymap may describe up to 255 keycodes.
const LAST_KEYCODE: u32 = 255;

/// evdev codes a client is entitled to act on by hardware code, whatever the keymap
/// says the key produces.
///
/// This is not theory. Assigning a character to evdev 14 put Backspace under it for
/// Chromium: the character before it was typed and then deleted, and two characters
/// went missing from a string that was otherwise perfect. Editing keys, modifiers
/// and navigation are all handled that way, so none of them may carry a character.
const RESERVED_EVDEV: &[u32] = &[
    1, // escape
    14, 15, 28, // backspace, tab, enter
    29, 42, 54, 56, 58, 97, 100, 125, 126, 127, // modifiers and menu
    59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 87, 88, // F1 to F12
    96, 98, 99, // keypad enter, keypad slash, print
    102, 103, 104, 105, 106, 107, 108, 109, // home, arrows, page keys, end
    110, 111, // insert, delete
    119, // pause
    142, 143, // sleep, wakeup
    158, // back, which the remote sends
];

/// The keycodes a generated keymap may use.
fn usable_keycodes() -> impl Iterator<Item = u32> {
    (FIRST_KEYCODE..=LAST_KEYCODE).filter(|code| !RESERVED_EVDEV.contains(&(code - 8)))
}

/// Build a keymap that can type exactly this string, and the keys to press.
///
/// Returns `None` if the string needs more distinct characters than a keymap holds.
fn keymap_for(text: &str) -> Option<(String, Vec<Keycode>)> {
    let mut distinct: Vec<char> = Vec::new();
    for character in text.chars() {
        if !distinct.contains(&character) {
            distinct.push(character);
        }
    }
    let codes: Vec<u32> = usable_keycodes().take(distinct.len()).collect();
    if codes.len() < distinct.len() {
        return None;
    }

    let mut keymap = String::from(
        "xkb_keymap {\n\
         xkb_keycodes { minimum = 8; maximum = 255;\n",
    );
    for (index, code) in codes.iter().enumerate() {
        let _ = writeln!(keymap, "<K{index}> = {code};");
    }
    keymap.push_str(
        "};\n\
         xkb_types { include \"complete\" };\n\
         xkb_compat { include \"complete\" };\n\
         xkb_symbols \"typing\" {\n",
    );
    for (index, character) in distinct.iter().enumerate() {
        // Unicode keysyms are written U followed by the code point, which covers
        // every character a name or a password can contain.
        let _ = writeln!(
            keymap,
            "key <K{}> {{ [ U{:04X} ] }};",
            index, *character as u32
        );
    }
    keymap.push_str("};\n};\n");

    let keys = text
        .chars()
        .map(|character| {
            let index = distinct.iter().position(|c| *c == character).unwrap();
            Keycode::from(codes[index])
        })
        .collect();

    Some((keymap, keys))
}

/// Type a string, then put the keyboard back the way it was.
pub fn type_text(state: &mut Tvbox, text: &str) -> anyhow::Result<usize> {
    if text.is_empty() {
        return Ok(0);
    }
    let Some(keyboard) = state.seat.get_keyboard() else {
        anyhow::bail!("the seat has no keyboard");
    };
    let (keymap, keys) = keymap_for(text)
        .ok_or_else(|| anyhow::anyhow!("the string needs too many distinct characters"))?;

    keyboard
        .set_keymap_from_string(state, keymap)
        .map_err(|err| anyhow::anyhow!("failed to load the generated keymap: {err}"))?;

    // Wayland delivers a client's events in order, so the keymap is in place on the
    // client's side before these arrive.
    send(state, &keyboard, &keys);

    keyboard
        .set_xkb_config(state, XkbConfig::default())
        .map_err(|err| anyhow::anyhow!("failed to restore the keymap: {err}"))?;

    Ok(keys.len())
}

fn send(state: &mut Tvbox, keyboard: &KeyboardHandle<Tvbox>, keys: &[Keycode]) {
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32;

    for key in keys {
        for pressed in [
            smithay::backend::input::KeyState::Pressed,
            smithay::backend::input::KeyState::Released,
        ] {
            keyboard.input_forward(
                state,
                *key,
                pressed,
                SERIAL_COUNTER.next_serial(),
                time,
                false,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_per_distinct_character() {
        let (keymap, keys) = keymap_for("aba").expect("two distinct characters fit");

        assert!(keymap.contains("key <K0> { [ U0061 ] };"));
        assert!(keymap.contains("key <K1> { [ U0062 ] };"));
        let codes: Vec<u32> = keys.iter().map(|k| u32::from(*k)).collect();
        assert_eq!(codes[0], codes[2]);
        assert_ne!(codes[0], codes[1]);
    }

    #[test]
    fn no_character_lands_on_a_key_a_client_acts_on() {
        // 14 is backspace: a character there is typed and then deleted.
        let text: String = ('a'..='z').collect();
        let (_, keys) = keymap_for(&text).expect("fits");

        for key in keys {
            let evdev = u32::from(key) - 8;
            assert!(
                !RESERVED_EVDEV.contains(&evdev),
                "character placed on reserved evdev {evdev}"
            );
        }
    }

    #[test]
    fn accented_and_symbols_survive() {
        let (keymap, keys) = keymap_for("Árvíz!").expect("fits");

        assert!(keymap.contains("U00C1")); // Á
        assert!(keymap.contains("U00ED")); // í
        assert!(keymap.contains("U0021")); // !
        assert_eq!(keys.len(), "Árvíz!".chars().count());
    }

    #[test]
    fn a_string_too_wide_for_a_keymap_is_refused() {
        let wide: String = (0u32..400).filter_map(char::from_u32).collect();
        assert!(keymap_for(&wide).is_none());
    }
}
