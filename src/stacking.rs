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

/// The app id to treat as the shell, for a box that renames it.
fn shell_app_id() -> String {
    std::env::var("TVBOX_SHELL_APP_ID").unwrap_or_else(|_| SHELL_APP_ID.to_owned())
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

/// Back to front: everything else first, the shell's windows last.
pub fn stacked(space: &Space<Window>) -> Vec<Window> {
    let shell = shell_app_id();
    let marked: Vec<(Window, bool)> = space
        .elements()
        .cloned()
        .map(|window| {
            let is_shell = app_id(&window).as_deref() == Some(shell.as_str());
            (window, is_shell)
        })
        .collect();
    order(marked)
}

/// Back to front, keeping the given order within each group.
fn order<T>(windows: Vec<(T, bool)>) -> Vec<T> {
    let (shell, rest): (Vec<_>, Vec<_>) = windows.into_iter().partition(|(_, is_shell)| *is_shell);
    rest.into_iter()
        .chain(shell)
        .map(|(window, _)| window)
        .collect()
}

/// The window that should hold the keyboard: the frontmost one.
pub fn topmost(space: &Space<Window>) -> Option<Window> {
    stacked(space).pop()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shell_ends_up_in_front_of_a_film_that_mapped_later() {
        // The order the box actually produces: the shell starts at boot, mpv maps
        // when a film starts.
        let order = order(vec![("shell", true), ("mpv", false)]);
        assert_eq!(order, vec!["mpv", "shell"]);
    }

    #[test]
    fn map_order_survives_within_a_group() {
        let order = order(vec![
            ("shell", true),
            ("mpv", false),
            ("popup", true),
            ("retroarch", false),
        ]);
        assert_eq!(order, vec!["mpv", "retroarch", "shell", "popup"]);
    }

    #[test]
    fn a_window_that_has_not_named_itself_is_asked_again() {
        // The toplevel exists before set_app_id arrives, and the keyboard question is
        // asked in between. Remembering that silence unnames the window forever.
        let mut remembered = None;
        assert_eq!(latch(&mut remembered, None), None);
        assert_eq!(latch(&mut remembered, Some("tvbox-shell".into())), Some("tvbox-shell".into()));
    }

    #[test]
    fn the_first_name_is_the_one_that_counts() {
        let mut remembered = None;
        latch(&mut remembered, Some("mpv".into()));
        assert_eq!(latch(&mut remembered, Some("tvbox-shell".into())), Some("mpv".into()));
    }

    #[test]
    fn a_screen_with_no_shell_window_is_left_alone() {
        // What a native program sees: the shell unmapped everything of its own.
        let order = order(vec![("retroarch", false)]);
        assert_eq!(order, vec!["retroarch"]);
    }
}
