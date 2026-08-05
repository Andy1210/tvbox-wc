//! Building the list of things to put on the screen.
//!
//! The list is front to back, which is also the order the DRM compositor walks when
//! it hands elements to hardware planes. That ordering is the whole policy: the
//! shell's UI is a layer surface, so it comes first and takes an overlay plane, and
//! the fullscreen window below it - a film, usually - is left to take the primary
//! plane untouched.

use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::AsRenderElements;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::desktop::{layer_map_for_output, Space, Window};
use smithay::output::Output;
use smithay::utils::{Physical, Point, Scale};
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
    tracing::trace!(
        mode = ?output.current_mode(),
        scale = output.current_scale().fractional_scale(),
        layers = layers.layers().count(),
        wanted = ?layers
            .layers()
            .map(|l| {
                let state = l.cached_state();
                (l.layer(), state.size, state.anchor, layers.layer_geometry(l))
            })
            .collect::<Vec<_>>(),
        windows = space.elements().count(),
        "scene"
    );
    for wanted in [Layer::Overlay, Layer::Top] {
        for layer in layers.layers().rev() {
            if layer.layer() != wanted {
                continue;
            }
            let Some(geometry) = layers.layer_geometry(layer) else {
                continue;
            };
            let location: Point<i32, Physical> = geometry.loc.to_physical_precise_round(scale);
            elements.extend(AsRenderElements::<GlesRenderer>::render_elements(
                layer, renderer, location, scale, 1.0,
            ));
        }
    }

    for window in space.elements().rev() {
        let Some(geometry) = space.element_geometry(window) else {
            continue;
        };
        let location: Point<i32, Physical> = geometry.loc.to_physical_precise_round(scale);
        elements.extend(AsRenderElements::<GlesRenderer>::render_elements(
            window, renderer, location, scale, 1.0,
        ));
    }

    for wanted in [Layer::Bottom, Layer::Background] {
        for layer in layers.layers().rev() {
            if layer.layer() != wanted {
                continue;
            }
            let Some(geometry) = layers.layer_geometry(layer) else {
                continue;
            };
            let location: Point<i32, Physical> = geometry.loc.to_physical_precise_round(scale);
            elements.extend(AsRenderElements::<GlesRenderer>::render_elements(
                layer, renderer, location, scale, 1.0,
            ));
        }
    }

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
