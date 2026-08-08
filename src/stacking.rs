//! Which window is in front.
//!
//! A general compositor stacks by whatever the user last raised. This box has a
//! rule instead: the shell's window is always in front of everything else, and the
//! order among the rest is the order they mapped in.
//!
//! That is not a preference, it is the arrangement the whole box is built on. The
//! shell's fullscreen window is TRANSLUCENT and the film plays behind it, so a film
//! that maps later - which it always does, the shell starts first - would otherwise
//! cover the UI that is supposed to be over it. Plane assignment follows the same
//! order, so it also decides which surface gets the primary plane.
//!
//! A native program (RetroArch) is not an exception: the shell unmaps its windows
//! before one starts, so there is nothing of ours left to keep in front.

use smithay::desktop::{Space, Window};
use smithay::wayland::compositor::with_states;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;

/// The shell's Wayland app id, which is its package name.
const SHELL_APP_ID: &str = "tvbox-shell";

/// The app id of the one window that sits above even the shell.
///
/// A note on screen has to be visible over whatever is running - that is the whole
/// point of it - and the shell's own window is not, because an app's window covers
/// it while the app is in front. So one window is exempt from the rule below.
///
/// Deliberately a SEPARATE id rather than another shell window: the exemption is
/// what lets something cover an app, so the narrower the thing holding it, the
/// better. It is also small and never focused - see [`topmost`].
const OVERLAY_APP_ID: &str = "tvbox-overlay";

/// The app id to treat as the shell, for a box that renames it.
fn shell_app_id() -> String {
    std::env::var("TVBOX_SHELL_APP_ID").unwrap_or_else(|_| SHELL_APP_ID.to_owned())
}

/// The app id to treat as the always-on-top overlay.
fn overlay_app_id() -> String {
    std::env::var("TVBOX_OVERLAY_APP_ID").unwrap_or_else(|_| OVERLAY_APP_ID.to_owned())
}

/// Where a window sits, back to front.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    /// Everything else: an app, a film, a native program.
    Other,
    /// The shell's own windows.
    Shell,
    /// The note that has to be seen over all of it.
    Overlay,
}

/// A window's app id, latched the first time the client presents one.
///
/// `xdg_toplevel.set_app_id` can be sent at any time, and two decisions here key off
/// the answer: which windows stay in front, and where a placed window goes. Reading
/// it live would let a client walk in front of the shell by renaming itself to
/// `tvbox-shell` after mapping, or escape a picture-in-picture rectangle by renaming
/// itself out of it. The first answer is the one that counts.
///
/// What is NOT an answer is silence. A toplevel exists before its client has sent
/// anything about it - `set_app_id` is a separate request, and this compositor asks
/// the question the moment the toplevel appears, to work out who should hold the
/// keyboard. Latching that emptiness left every window unnamed for the rest of its
/// life, and with no window matching the shell's id the rule below has nothing to
/// keep in front: a film covered the UI that is supposed to be over it, and took the
/// remote with it.
///
/// This is not a security boundary - a client that maps with the shell's app id from
/// the start still joins that group, and on this box every Wayland client is
/// something the shell or the user installed. It removes the mid-flight change,
/// which is the part that is neither useful nor expected.
pub fn app_id(window: &Window) -> Option<String> {
    let surface = window.wl_surface()?;
    with_states(&surface, |states| {
        let latched = states.data_map.get_or_insert(LatchedAppId::default);
        let mut remembered = latched.0.borrow_mut();
        let committed = states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|data| data.lock().ok().and_then(|data| data.app_id.clone()));
        latch(&mut remembered, committed)
    })
}

/// Keep the first name a client presents, and answer with it from then on.
fn latch(remembered: &mut Option<String>, committed: Option<String>) -> Option<String> {
    if remembered.is_none() {
        *remembered = committed;
    }
    remembered.clone()
}

/// The app id a window first presented, kept per surface.
#[derive(Default)]
struct LatchedAppId(std::cell::RefCell<Option<String>>);

/// Back to front: everything else, then the shell's windows, then the overlay.
pub fn stacked(space: &Space<Window>) -> Vec<Window> {
    order(space.elements().cloned().map(|window| {
        let rank = rank(&window);
        (window, rank)
    }))
}

/// Which group a window belongs to.
fn rank(window: &Window) -> Rank {
    match app_id(window).as_deref() {
        Some(id) if id == overlay_app_id() => Rank::Overlay,
        Some(id) if id == shell_app_id() => Rank::Shell,
        _ => Rank::Other,
    }
}

/// Back to front, keeping the given order within each group.
fn order<T>(windows: impl IntoIterator<Item = (T, Rank)>) -> Vec<T> {
    let mut windows: Vec<(usize, T, Rank)> = windows
        .into_iter()
        .enumerate()
        .map(|(i, (window, rank))| (i, window, rank))
        .collect();
    // Stable by rank: a sort that lost map order within a group would reshuffle the
    // windows every frame for no reason anyone asked for.
    windows.sort_by_key(|(i, _, rank)| (*rank, *i));
    windows.into_iter().map(|(_, window, _)| window).collect()
}

/// The window that should hold the keyboard.
///
/// The frontmost one, EXCEPT the overlay: it is a note, not a place to type. Giving
/// it the keyboard because it happens to be in front would take the remote away
/// from whatever the person is actually using, for as long as the note is up.
pub fn topmost(space: &Space<Window>) -> Option<Window> {
    stacked(space)
        .into_iter()
        .rfind(|window| rank(window) != Rank::Overlay)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shell_ends_up_in_front_of_a_film_that_mapped_later() {
        // The order the box actually produces: the shell starts at boot, mpv maps
        // when a film starts.
        let order = order(vec![("shell", Rank::Shell), ("mpv", Rank::Other)]);
        assert_eq!(order, vec!["mpv", "shell"]);
    }

    #[test]
    fn map_order_survives_within_a_group() {
        let order = order(vec![
            ("shell", Rank::Shell),
            ("mpv", Rank::Other),
            ("popup", Rank::Shell),
            ("retroarch", Rank::Other),
        ]);
        assert_eq!(order, vec!["mpv", "retroarch", "shell", "popup"]);
    }

    #[test]
    fn a_note_is_in_front_of_an_app_that_is_covering_the_shell() {
        // The case it exists for: an app is fullscreen, so the shell's own window is
        // behind it, and the note still has to be seen.
        let order = order(vec![
            ("shell", Rank::Shell),
            ("plex", Rank::Other),
            ("note", Rank::Overlay),
        ]);
        assert_eq!(order, vec!["plex", "shell", "note"]);
    }

    #[test]
    fn two_notes_keep_their_order() {
        let order = order(vec![
            ("first", Rank::Overlay),
            ("app", Rank::Other),
            ("second", Rank::Overlay),
        ]);
        assert_eq!(order, vec!["app", "first", "second"]);
    }

    #[test]
    fn a_window_that_has_not_named_itself_is_asked_again() {
        // The toplevel exists before set_app_id arrives, and the keyboard question is
        // asked in between. Remembering that silence unnames the window forever.
        let mut remembered = None;
        assert_eq!(latch(&mut remembered, None), None);
        assert_eq!(
            latch(&mut remembered, Some("tvbox-shell".into())),
            Some("tvbox-shell".into())
        );
    }

    #[test]
    fn the_first_name_is_the_one_that_counts() {
        let mut remembered = None;
        latch(&mut remembered, Some("mpv".into()));
        assert_eq!(
            latch(&mut remembered, Some("tvbox-shell".into())),
            Some("mpv".into())
        );
    }

    #[test]
    fn a_screen_with_no_shell_window_is_left_alone() {
        // What a native program sees: the shell unmapped everything of its own.
        let order = order(vec![("retroarch", Rank::Other)]);
        assert_eq!(order, vec!["retroarch"]);
    }
}
