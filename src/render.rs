//! Building the list of things to put on the screen.
//!
//! The list is front to back, which is also the order the DRM compositor walks when
//! it hands elements to hardware planes. That ordering is the whole policy: the
//! shell's UI is a layer surface, so it comes first and takes an overlay plane, and
//! the fullscreen window below it - a film, usually - is left to take the primary
//! plane untouched.
//!
//! Every element is built as a [`Kind::ScanoutCandidate`], and that is not
//! decoration. `try_assign_overlay_plane` refuses any element whose kind is not
//! scanout-candidate or cursor, and refuses it without a reason, so the default
//! (`Kind::Unspecified`) means overlay planes are never even attempted. It compounds:
//! the primary plane is only offered to the LAST element, and only while nothing in
//! front of it has fallen back to composition, so a single unmarked element above
//! the video takes the video off its plane as well. Per frame it is all or nothing.

use smithay::backend::renderer::element::surface::{
    render_elements_from_surface_tree, WaylandSurfaceRenderElement,
};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::desktop::{layer_map_for_output, Space, Window};
use smithay::output::Output;
use smithay::utils::{Physical, Point, Scale};
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::shell::wlr_layer::Layer;

/// What the compositor hands to the DRM compositor, front to back.
pub type Element = WaylandSurfaceRenderElement<GlesRenderer>;

/// The elements to render, front to back.
pub fn elements(
    renderer: &mut GlesRenderer,
    space: &Space<Window>,
    output: &Output,
) -> Vec<Element> {
    let scale = Scale::from(output.current_scale().fractional_scale());
    let mut elements = Vec::new();
    let layers = layer_map_for_output(output);

    let mut push_layer_group =
        |elements: &mut Vec<Element>, renderer: &mut GlesRenderer, wanted: Layer| {
            for layer in layers.layers().rev() {
                if layer.layer() != wanted {
                    continue;
                }
                let Some(geometry) = layers.layer_geometry(layer) else {
                    continue;
                };
                let location: Point<i32, Physical> = geometry.loc.to_physical_precise_round(scale);
                elements.extend(render_elements_from_surface_tree(
                    renderer,
                    layer.wl_surface(),
                    location,
                    scale,
                    1.0,
                    Kind::ScanoutCandidate,
                ));
            }
        };

    push_layer_group(&mut elements, renderer, Layer::Overlay);
    push_layer_group(&mut elements, renderer, Layer::Top);

    for window in space.elements().rev() {
        let Some(surface) = window.wl_surface() else {
            continue;
        };
        let Some(geometry) = space.element_geometry(window) else {
            continue;
        };
        let location: Point<i32, Physical> = geometry.loc.to_physical_precise_round(scale);
        elements.extend(render_elements_from_surface_tree(
            renderer,
            &surface,
            location,
            scale,
            1.0,
            Kind::ScanoutCandidate,
        ));
    }

    push_layer_group(&mut elements, renderer, Layer::Bottom);
    push_layer_group(&mut elements, renderer, Layer::Background);

    elements
}

/// Tell clients they may draw the next frame.
///
/// Without this a client draws once and stops, which looks exactly like the
/// compositor having frozen.
pub fn send_frames(space: &Space<Window>, output: &Output) {
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();

    for window in space.elements() {
        window.send_frame(output, time, None, |_, _| Some(output.clone()));
    }

    let layers = layer_map_for_output(output);
    for layer in layers.layers() {
        layer.send_frame(output, time, None, |_, _| Some(output.clone()));
    }
}
