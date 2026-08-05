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
use crate::state::Tvbox;

/// How long the pointer sits still before it is taken off the screen.
pub const IDLE: std::time::Duration = std::time::Duration::from_secs(5);

/// Take the pointer off the screen while it is not being used.
///
/// A wireless TV remote often presents a mouse endpoint too, so a box nobody has
/// ever plugged a mouse into still shows a pointer that never moves. Checked on a
/// timer rather than armed per motion event: a moving pointer fires hundreds of
/// those a second, and rescheduling on each one costs more than one check a second.
pub fn hide_when_idle(
    loop_handle: &smithay::reexports::calloop::LoopHandle<'static, Tvbox>,
) -> anyhow::Result<()> {
    let timer = smithay::reexports::calloop::timer::Timer::from_duration(IDLE);
    loop_handle
        .insert_source(timer, |_, _, state| {
            if state.pointer_visible && state.pointer_moved_at.elapsed() >= IDLE {
                state.pointer_visible = false;
                state.queue_redraw();
            }
            smithay::reexports::calloop::timer::TimeoutAction::ToDuration(
                std::time::Duration::from_secs(1),
            )
        })
        .map_err(|err| anyhow::anyhow!("failed to insert the pointer-idle timer: {err}"))?;
    Ok(())
}

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
