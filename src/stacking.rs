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

/// The TITLE of the one shell window that sits above even the rest of the shell.
///
/// A note on screen has to be visible over whatever is running - that is the whole
/// point of it - and the shell's own window is not, because an app's window covers
/// it while the app is in front. So one window is exempt from the rule below.
///
/// A title and not an app id, and that is forced: every window of one Chromium
/// process presents the same app id, so the launcher, an app and a note are all
/// `tvbox-shell` and nothing tells them apart from the outside. The title is what a
/// client can vary per window.
const OVERLAY_TITLE: &str = "tvbox-overlay";

/// The app id to treat as the shell, for a box that renames it.
///
/// Resolved once. This is asked for every window of every frame, and reading the
/// environment there would allocate a string per window per frame - and let a
/// mid-run `set_var` disagree with the stacking that is already on screen.
fn shell_app_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        std::env::var("TVBOX_SHELL_APP_ID").unwrap_or_else(|_| SHELL_APP_ID.to_owned())
    })
}

/// The title that marks the always-on-top overlay. Resolved once, as above.
fn overlay_title() -> &'static str {
    static TITLE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TITLE.get_or_init(|| {
        std::env::var("TVBOX_OVERLAY_TITLE").unwrap_or_else(|_| OVERLAY_TITLE.to_owned())
    })
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

/// A window's current title.
///
/// Not latched, unlike the app id: the title is how the ONE overlay window is
/// recognised, and it has to be, because every window of one Chromium process
/// carries the same app id - the shell's launcher, an app and a note are all
/// `tvbox-shell`. The title is the only thing the client can vary per window.
///
/// That is also why the overlay is only granted to a window that is already the
/// SHELL's. It is worth being exact about what that buys: it keeps out every OTHER
/// client - mpv, a native program, anything the user installs - and it does not
/// keep out a page running inside one of the shell's own windows, because that page
/// is behind the shell's app id by construction. A page's document title reaches
/// this function, so what stops one naming itself into the front is the shell
/// refusing the reserved name on the windows it hands a page (`titleAllowed` in the
/// shell's notify.js), not anything here.
pub fn window_title(window: &Window) -> Option<String> {
    let surface = window.wl_surface()?;
    with_states(&surface, |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|data| data.lock().ok().and_then(|data| data.title.clone()))
    })
}

/// The keys a window can be placed by, most specific first.
///
/// A title names one window and an app id names all of a client's, so a title
/// placement has to win - otherwise placing the shell's small note would put the
/// launcher in the same little rectangle.
///
/// A title only names one of the SHELL's, though, and it is gated on the app id for
/// the same reason the rank above is: a title is a string any client can present,
/// and the placements that exist are the shell's own. Without the gate any client
/// could put itself in the note's rectangle by naming itself, and - because a
/// changed location is what re-places a window - could move itself around at will
/// by changing that name.
pub fn place_key(window: &Window) -> Vec<crate::state::PlaceKey> {
    place_keys_for(
        app_id(window).as_deref(),
        window_title(window).as_deref(),
        shell_app_id(),
    )
}

/// The rule itself, away from Wayland.
fn place_keys_for(
    app_id: Option<&str>,
    title: Option<&str>,
    shell: &str,
) -> Vec<crate::state::PlaceKey> {
    use crate::state::PlaceKey;
    let mut keys = Vec::new();
    if app_id == Some(shell) {
        if let Some(title) = title {
            keys.push(PlaceKey::Title(title.to_owned()));
        }
    }
    if let Some(app_id) = app_id {
        keys.push(PlaceKey::AppId(app_id.to_owned()));
    }
    keys
}

/// Back to front: everything else, then the shell's windows, then the overlay.
pub fn stacked(space: &Space<Window>) -> Vec<Window> {
    order(space.elements().cloned().map(|window| {
        let rank = rank(&window);
        (window, rank)
    }))
}

/// Which group a window belongs to.
fn rank(window: &Window) -> Rank {
    rank_of(
        app_id(window).as_deref(),
        window_title(window).as_deref(),
        shell_app_id(),
        overlay_title(),
    )
}

/// The rule itself, away from Wayland: who is allowed in front of what.
fn rank_of(app_id: Option<&str>, title: Option<&str>, shell: &str, overlay: &str) -> Rank {
    if app_id != Some(shell) {
        // Only the shell's own windows can be an overlay. A title is a string any
        // page can set, so without this a web app could name itself to the front.
        return Rank::Other;
    }
    if title == Some(overlay) {
        Rank::Overlay
    } else {
        Rank::Shell
    }
}

/// Back to front, keeping the given order within each group.
fn order<T>(windows: impl IntoIterator<Item = (T, Rank)>) -> Vec<T> {
    let mut windows: Vec<(usize, T, Rank)> = windows
        .into_iter()
        .enumerate()
        .map(|(i, (window, rank))| (i, window, rank))
        .collect();
    // Stable by rank: a sort that lost the order the space gives within a group
    // would reshuffle the windows every frame for no reason anyone asked for.
    windows.sort_by_key(|(i, _, rank)| (*rank, *i));
    windows.into_iter().map(|(_, window, _)| window).collect()
}

/// Is this window on screen yet?
///
/// A toplevel exists before its client has committed a buffer for it, and a window
/// with nothing on screen is not what anyone is looking at. This is the same test
/// that decides whether a window is placed, and it is shared with `commit` rather
/// than written twice: the two must not be able to disagree about which windows
/// have arrived.
pub fn mapped(window: &Window) -> bool {
    let size = window.geometry().size;
    size.w > 0 && size.h > 0
}

/// The window that should hold the keyboard.
///
/// The frontmost one, EXCEPT the overlay: it is a note, not a place to type. Giving
/// it the keyboard because it happens to be in front would take the remote away
/// from whatever the person is actually using, for as long as the note is up.
///
/// A window that has MAPPED is preferred over one that has not, and that is what
/// makes the exception hold for the SECOND note as well as the first. A client
/// creates a toplevel and names it afterwards, so a new window is nameless for a
/// moment - and the app id, unlike the title, is latched per surface, so a note
/// reusing the surface of the last one arrives already carrying the shell's id with
/// no title yet. It ranks as an ordinary shell window and, being the newest, would
/// win outright. Preferring what is on screen is what leaves it behind: the window
/// the person is looking at is mapped, and the note is not yet.
pub fn topmost(space: &Space<Window>) -> Option<Window> {
    focusable(space.elements().cloned().map(|window| {
        let rank = rank(&window);
        let mapped = mapped(&window);
        (window, rank, mapped)
    }))
}

/// The rule itself, away from Wayland: the frontmost window that may hold the
/// keyboard.
///
/// A preference rather than a filter, because with nothing mapped to prefer the
/// answer would be NOBODY. The shell tears the outgoing window down before the
/// incoming one has painted, so every app switch has a gap of tens of milliseconds
/// where the only window on the box has no buffer yet - and a keyboard focus of
/// none there costs a press off an autorepeating arrow. The overlay stays excluded
/// in both tiers: it is never a place to type, mapped or not.
fn focusable<T>(windows: impl IntoIterator<Item = (T, Rank, bool)>) -> Option<T> {
    let ordered = order(
        windows
            .into_iter()
            .map(|(window, rank, mapped)| ((window, rank, mapped), rank)),
    );
    let mut waiting_to_map = None;
    for (window, rank, mapped) in ordered.into_iter().rev() {
        if rank == Rank::Overlay {
            continue;
        }
        if mapped {
            return Some(window);
        }
        if waiting_to_map.is_none() {
            waiting_to_map = Some(window);
        }
    }
    waiting_to_map
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
    fn only_the_shell_may_claim_the_front() {
        // A title is a string any page can set, and every Chromium window shares one
        // app id - so the app id is what has to gate this, not the title alone.
        assert_eq!(
            rank_of(
                Some("tvbox-shell"),
                Some("tvbox-overlay"),
                "tvbox-shell",
                "tvbox-overlay"
            ),
            Rank::Overlay
        );
        assert_eq!(
            rank_of(
                Some("mpv"),
                Some("tvbox-overlay"),
                "tvbox-shell",
                "tvbox-overlay"
            ),
            Rank::Other
        );
        assert_eq!(
            rank_of(
                Some("tvbox-shell"),
                Some("Plex"),
                "tvbox-shell",
                "tvbox-overlay"
            ),
            Rank::Shell
        );
        assert_eq!(
            rank_of(None, None, "tvbox-shell", "tvbox-overlay"),
            Rank::Other
        );
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
    fn a_note_never_holds_the_keyboard() {
        // The overlay is in front of everything, and that is exactly why it must not
        // be asked to answer the remote.
        assert_eq!(
            focusable(vec![
                ("launcher", Rank::Shell, true),
                ("note", Rank::Overlay, true),
            ]),
            Some("launcher")
        );
    }

    #[test]
    fn only_the_shell_may_be_placed_by_a_title() {
        use crate::state::PlaceKey;
        // A title names ONE of the shell's windows, which is what the note needs.
        assert_eq!(
            place_keys_for(Some("tvbox-shell"), Some("tvbox-overlay"), "tvbox-shell"),
            vec![
                PlaceKey::Title("tvbox-overlay".into()),
                PlaceKey::AppId("tvbox-shell".into()),
            ]
        );
        // For anyone else the title is a string they chose. Without this, naming
        // yourself puts you in the note's rectangle - and since a changed location
        // is what re-places a window, renaming moves you about at will.
        assert_eq!(
            place_keys_for(Some("mpv"), Some("tvbox-overlay"), "tvbox-shell"),
            vec![PlaceKey::AppId("mpv".into())]
        );
        // A window that has not named its client is placed by nothing.
        assert_eq!(
            place_keys_for(None, Some("tvbox-overlay"), "tvbox-shell"),
            vec![]
        );
    }

    #[test]
    fn a_note_over_a_fullscreen_app_leaves_the_app_answering_the_remote() {
        // What the box really produces: an app is in front, so the shell's own
        // window has been torn down and is waiting to come back, and a note arrives
        // over the top of it.
        assert_eq!(
            focusable(vec![
                ("launcher", Rank::Shell, false),
                ("plex", Rank::Other, true),
                ("note", Rank::Overlay, true),
            ]),
            Some("plex")
        );
    }

    #[test]
    fn a_window_that_has_not_mapped_is_not_offered_the_keyboard() {
        // A note showing for the second time: the surface is reused, so the app id
        // is already the shell's while the title is still on its way. It ranks as an
        // ordinary shell window here, and it is the newest one - without preferring
        // what is on screen it would take the remote for as long as the note is up.
        assert_eq!(
            focusable(vec![
                ("launcher", Rank::Shell, true),
                ("nameless note", Rank::Shell, false),
            ]),
            Some("launcher")
        );
    }

    #[test]
    fn a_window_that_maps_takes_the_keyboard() {
        // The other side of the same test: once the buffer is there, an app that
        // opened over the launcher is what the remote should be driving.
        assert_eq!(
            focusable(vec![
                ("launcher", Rank::Shell, true),
                ("app", Rank::Shell, true),
            ]),
            Some("app")
        );
    }

    #[test]
    fn a_native_program_answers_the_remote_when_nothing_of_ours_is_left() {
        // The shell unmaps its own windows before RetroArch starts.
        assert_eq!(
            focusable(vec![("retroarch", Rank::Other, true)]),
            Some("retroarch")
        );
    }

    #[test]
    fn a_screen_with_only_a_note_on_it_gives_the_keyboard_to_nobody() {
        // Rather than to the note. There is nothing else to type into, and the note
        // is not a place to type.
        assert_eq!(focusable(vec![("note", Rank::Overlay, true)]), None);
    }

    #[test]
    fn the_window_on_its_way_in_answers_when_nothing_else_is_on_screen() {
        // Every app switch: the shell tears the old window down before the new one
        // has painted, so for a few frames the only window on the box has no buffer.
        // Answering "nobody" there loses a press off an autorepeating arrow.
        assert_eq!(
            focusable(vec![("opening app", Rank::Shell, false)]),
            Some("opening app")
        );
    }

    #[test]
    fn a_note_does_not_answer_even_when_it_is_the_only_thing_on_screen() {
        // The same gap with a note up. The window still coming in gets the keyboard,
        // and the note stays out of it in both tiers.
        assert_eq!(
            focusable(vec![
                ("opening app", Rank::Shell, false),
                ("note", Rank::Overlay, true),
            ]),
            Some("opening app")
        );
    }

    #[test]
    fn a_screen_with_no_shell_window_is_left_alone() {
        // What a native program sees: the shell unmapped everything of its own.
        let order = order(vec![("retroarch", Rank::Other)]);
        assert_eq!(order, vec!["retroarch"]);
    }
}
