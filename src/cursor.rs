//! The pointer, drawn from whatever the focused client asked for.
//!
//! Only a client-provided cursor surface is drawn. A named cursor would need an
//! XCursor theme loaded and parsed, and on a box where something always covers the
//! screen there is nothing to draw it over: every client that wants a pointer sets
//! its own surface. When none is set there is no pointer, which is also what a
//! remote-driven UI wants.
//!
//! The element is built as [`Kind::Cursor`], which is what lets the display engine
//! put it on the cursor plane instead of composing it. That is not a nicety here:
//! without it, moving the mouse would take the whole scene off its planes for as
//! long as the pointer moves, film included.

use smithay::backend::renderer::element::surface::render_elements_from_surface_tree;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::input::pointer::{CursorImageStatus, CursorImageSurfaceData};
use smithay::utils::IsAlive;
use smithay::utils::{Logical, Physical, Point, Scale};
use smithay::wayland::compositor::with_states;

use crate::render::Element;

/// Render elements for the pointer, front-most in the scene.
pub fn elements(
    renderer: &mut GlesRenderer,
    status: &CursorImageStatus,
    location: Point<f64, Logical>,
    scale: Scale<f64>,
) -> Vec<Element> {
    let CursorImageStatus::Surface(surface) = status else {
        return Vec::new();
    };
    if !surface.alive() {
        return Vec::new();
    }

    // The hotspot is where the client says the pointer actually points, so the
    // surface is drawn offset by it.
    let hotspot = with_states(surface, |states| {
        states
            .data_map
            .get::<CursorImageSurfaceData>()
            .map(|data| data.lock().unwrap().hotspot)
            .unwrap_or_default()
    });

    let position: Point<i32, Physical> =
        (location - hotspot.to_f64()).to_physical_precise_round(scale);
    render_elements_from_surface_tree(renderer, surface, position, scale, 1.0, Kind::Cursor)
}
