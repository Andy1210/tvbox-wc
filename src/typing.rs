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

use smithay::input::keyboard::{FilterResult, KeyboardHandle, Keycode, XkbConfig};
use smithay::utils::SERIAL_COUNTER;
use tracing::error;

use crate::state::Tvbox;

/// The longest string this will type.
///
/// Not a protocol limit but a physical one: every character becomes a press and a
/// release, each serialised into the focused client's buffer before the event loop
/// gets another turn, so a caller asking for a megabyte freezes the picture and the
/// remote while it is delivered. The shell's own limit is 400 characters (a login
/// field, not a document).
const MAX_TEXT: usize = 4096;

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

/// The select-all chord under the layout the seat normally carries: xkb counts
/// keycodes from 8, so these are evdev 29 (left control) and 30 (a).
const CTRL_KEYCODE: u32 = 37;
const A_KEYCODE: u32 = 38;
/// Delete, under that same layout: evdev 111.
const DELETE_KEYCODE: u32 = 119;

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

/// ctrl+a as press/release pairs.
fn select_all_chord() -> Vec<(Keycode, bool)> {
    vec![
        (Keycode::from(CTRL_KEYCODE), true),
        (Keycode::from(A_KEYCODE), true),
        (Keycode::from(A_KEYCODE), false),
        (Keycode::from(CTRL_KEYCODE), false),
    ]
}

/// Delete, as a press and a release.
///
/// The chord only SELECTS. Emptying a field takes a key that removes the selection,
/// and it is deliberately one of the codes `RESERVED_EVDEV` keeps free: a client acts
/// on those by hardware code whatever a keymap claims, which is exactly what is
/// wanted here and exactly why no character may sit on one.
fn clear_stroke() -> Vec<(Keycode, bool)> {
    vec![
        (Keycode::from(DELETE_KEYCODE), true),
        (Keycode::from(DELETE_KEYCODE), false),
    ]
}

/// One press and one release per character, in order.
fn strokes_for(keys: &[Keycode]) -> Vec<(Keycode, bool)> {
    keys.iter()
        .flat_map(|key| [(*key, true), (*key, false)])
        .collect()
}

/// Type a string, then put the keyboard back the way it was.
///
/// `select_all` sends ctrl+a first, which is what a caller replacing a field's
/// contents wants: the field usually already holds something (a prefilled address,
/// the last search, the typo being corrected) and typing would append to it. It
/// goes out under the seat's own keymap, before the generated one is loaded, since
/// the generated keymap has no control key at all.
///
/// An EMPTY string with `select_all` means "empty this field", and it is the one case
/// where the chord is not enough on its own - selecting text deletes nothing. That
/// request is real: the shell's keyboard now opens ON what the field already holds,
/// so clearing it there and confirming has to reach the page, and answering it with
/// silence would be a dead end on a screen with no other way to do it. Without
/// `select_all` an empty string still means nothing, because then there is neither
/// anything to type nor a selection to remove.
pub fn type_text(state: &mut Tvbox, text: &str, select_all: bool) -> anyhow::Result<usize> {
    let characters = text.chars().count();
    if characters > MAX_TEXT {
        anyhow::bail!("{characters} characters is more than this will type at once");
    }
    let Some(keyboard) = state.seat.get_keyboard() else {
        anyhow::bail!("the seat has no keyboard");
    };
    if text.is_empty() {
        if select_all {
            send(state, &keyboard, &select_all_chord());
            send(state, &keyboard, &clear_stroke());
        }
        // The count is of CHARACTERS typed, which is what the chord was never in
        // either - nothing was typed here.
        return Ok(0);
    }
    let (keymap, keys) = keymap_for(text)
        .ok_or_else(|| anyhow::anyhow!("the string needs too many distinct characters"))?;

    if select_all {
        send(state, &keyboard, &select_all_chord());
    }

    keyboard
        .set_keymap_from_string(state, keymap)
        .map_err(|err| anyhow::anyhow!("failed to load the generated keymap: {err}"))?;

    // Wayland delivers a client's events in order, so the keymap is in place on the
    // client's side before these arrive.
    send(state, &keyboard, &strokes_for(&keys));

    // Restoring matters more than typing did. The generated keymap has one keycode
    // per character of THIS string and NoSymbol everywhere else, so a seat left on
    // it is a remote that does nothing at all - and set_xkb_config compiles from
    // RMLVO names, which can fail for reasons that have nothing to do with us (an
    // unattended upgrade replacing xkb-data underneath). So: try the same layout
    // again, then the barest keymap that can exist, and say so loudly either way.
    if let Err(err) = keyboard.set_xkb_config(state, XkbConfig::default()) {
        error!(?err, "failed to restore the keymap - retrying");
        if let Err(err) = keyboard.set_xkb_config(state, XkbConfig::default()) {
            error!(
                ?err,
                "still cannot restore the keymap - falling back to a plain us layout"
            );
            let plain = XkbConfig {
                layout: "us",
                ..Default::default()
            };
            keyboard.set_xkb_config(state, plain).map_err(|err| {
                anyhow::anyhow!("the seat is left on the generated keymap: {err}")
            })?;
        }
    }

    Ok(keys.len())
}

fn send(state: &mut Tvbox, keyboard: &KeyboardHandle<Tvbox>, strokes: &[(Keycode, bool)]) {
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32;

    for (key, pressed) in strokes {
        let key_state = if *pressed {
            smithay::backend::input::KeyState::Pressed
        } else {
            smithay::backend::input::KeyState::Released
        };
        // `input`, the same entry point real key events take, and NOT a bare
        // `input_forward`: forwarding only sends the client a `modifiers` event when
        // it is told one changed, and working that out is what the state update
        // inside `input` does. Forwarding alone with a hardcoded `false` typed the
        // characters perfectly - the generated keymap puts every one of them on the
        // first level, so not one of them needs a modifier - and silently broke the
        // only chord there is. A client takes its modifier state from that event and
        // from nothing else, so ctrl+a arrived as a bare `a` and landed in the field
        // as a literal character instead of selecting its contents: the replace the
        // caller asked for became "insert an `a`, then append to what was there".
        //
        // The filter is where the compositor's own key bindings live. Typing must
        // trigger none of them, so this one always forwards.
        keyboard.input::<(), _>(
            state,
            *key,
            key_state,
            SERIAL_COUNTER.next_serial(),
            time,
            |_, _, _| FilterResult::Forward,
        );
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
    fn every_key_that_goes_down_comes_back_up() {
        // A modifier left down would apply to everything typed after it, and a
        // character key left down repeats.
        let (_, keys) = keymap_for("hello").expect("fits");
        let mut strokes = select_all_chord();
        strokes.extend(strokes_for(&keys));

        let mut held: Vec<Keycode> = Vec::new();
        for (key, pressed) in strokes {
            if pressed {
                assert!(!held.contains(&key), "pressed twice without a release");
                held.push(key);
            } else {
                let at = held
                    .iter()
                    .position(|k| *k == key)
                    .expect("released unpressed key");
                held.remove(at);
            }
        }
        assert!(held.is_empty(), "keys left down: {held:?}");
    }

    #[test]
    fn select_all_finishes_before_the_first_character() {
        // Control must be up again by then: with it still held the string would go
        // out as shortcuts rather than text.
        let chord = select_all_chord();
        assert_eq!(
            chord.last().map(|(k, down)| (u32::from(*k), *down)),
            Some((CTRL_KEYCODE, false))
        );
    }

    #[test]
    fn the_chord_is_really_ctrl_and_a_under_the_seats_own_keymap() {
        // The two keycodes are hand-computed evdev codes + 8, and the chord's whole
        // job rests on them: an `a` that goes out with no control held is not a
        // failed select-all, it is a literal character inserted into the field ahead
        // of everything the user typed. Checking the stroke SHAPE cannot see that -
        // the shape was right the whole time the bug was live - so this asks a real
        // xkb state what the keys mean and what they leave depressed.
        use smithay::input::keyboard::{xkb, Keysym};

        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("the seat's default keymap compiles");
        let mut xkb_state = xkb::State::new(&keymap);

        assert_eq!(
            xkb_state.key_get_one_sym(Keycode::from(A_KEYCODE)),
            Keysym::a,
            "the chord's second key is not the letter a"
        );

        let mut a_saw_control = false;
        for (key, pressed) in select_all_chord() {
            xkb_state.update_key(
                key,
                if pressed {
                    xkb::KeyDirection::Down
                } else {
                    xkb::KeyDirection::Up
                },
            );
            if u32::from(key) == A_KEYCODE && pressed {
                a_saw_control =
                    xkb_state.mod_name_is_active(xkb::MOD_NAME_CTRL, xkb::STATE_MODS_EFFECTIVE);
            }
        }

        assert!(
            a_saw_control,
            "the a of ctrl+a goes out with no control held"
        );
        // And the seat is handed back the way it was found: a control left depressed
        // would turn the string that follows into shortcuts.
        assert!(
            !xkb_state.mod_name_is_active(xkb::MOD_NAME_CTRL, xkb::STATE_MODS_EFFECTIVE),
            "control is still down after the chord"
        );
    }

    #[test]
    fn emptying_a_field_sends_a_key_that_removes_the_selection() {
        // Selecting is not clearing, so the chord alone would leave the field exactly
        // as it was and the caller would never know. The key that does the work has
        // to be one a client acts on by hardware code - which is what RESERVED_EVDEV
        // holds - or the generated keymap could put a character on it.
        use smithay::input::keyboard::{xkb, Keysym};

        assert!(
            RESERVED_EVDEV.contains(&(DELETE_KEYCODE - 8)),
            "a character could be placed on the key that clears the field"
        );

        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("the seat's default keymap compiles");
        let xkb_state = xkb::State::new(&keymap);
        assert_eq!(
            xkb_state.key_get_one_sym(Keycode::from(DELETE_KEYCODE)),
            Keysym::Delete
        );

        let stroke = clear_stroke();
        assert_eq!(stroke.len(), 2);
        assert!(
            stroke[0].1 && !stroke[1].1,
            "pressed and released, in order"
        );
    }

    #[test]
    fn a_string_too_wide_for_a_keymap_is_refused() {
        let wide: String = (0u32..400).filter_map(char::from_u32).collect();
        assert!(keymap_for(&wide).is_none());
    }
}
