//! Compositor state and the Wayland protocol handlers.
//!
//! The window management here is deliberately thin. A TV box shows one thing at a
//! time: a toplevel is always fullscreen on the single output, and the shell's UI
//! rides on layer-shell above it. There is no stacking to speak of, no focus
//! follows anything, and no decorations.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use smithay::backend::renderer::element::default_primary_scanout_output_compare;
use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::desktop::{
    layer_map_for_output, LayerSurface, PopupManager, Space, Window, WindowSurfaceType,
};
use smithay::input::{pointer::CursorImageStatus, Seat, SeatHandler, SeatState};
use smithay::output::Output;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::{wl_seat, wl_surface::WlSurface};
use smithay::reexports::wayland_server::{Client, DisplayHandle};
use smithay::utils::{Logical, Point, Serial};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    get_parent, is_sync_subsurface, with_states, CompositorClientState, CompositorHandler,
    CompositorState,
};
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::selection::data_device::{
    DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler,
};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::shell::wlr_layer::{
    Layer, LayerSurface as WlrLayerSurface, WlrLayerShellHandler, WlrLayerShellState,
};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::{
    delegate_compositor, delegate_data_device, delegate_dmabuf, delegate_layer_shell,
    delegate_output, delegate_seat, delegate_shm, delegate_xdg_shell,
};
use tracing::{debug, warn};

use crate::backend::Tty;

/// Per-client state the compositor keeps.
#[derive(Default)]
pub struct ClientState {
    /// Surface bookkeeping smithay does on our behalf.
    pub compositor_state: CompositorClientState,
}

impl smithay::reexports::wayland_server::backend::ClientData for ClientState {}

/// Everything the compositor owns.
pub struct Tvbox {
    /// Set to false to leave the event loop.
    pub running: Arc<AtomicBool>,
    /// Handle to the Wayland display, for creating globals and sending events.
    pub display_handle: DisplayHandle,
    /// Handle to the event loop, for timers and new event sources.
    pub loop_handle: LoopHandle<'static, Tvbox>,

    /// The display hardware.
    pub tty: Tty,
    /// The single output. `None` until a connector comes up.
    pub output: Option<Output>,

    /// Mapped toplevels. A TV box shows one at a time, but a client may map a
    /// second one before unmapping the first.
    pub space: Space<Window>,
    /// Popups (menus in a browser, mostly).
    pub popups: PopupManager,

    pub compositor_state: CompositorState,
    pub shm_state: ShmState,
    pub seat_state: SeatState<Tvbox>,
    pub data_device_state: DataDeviceState,
    pub xdg_shell_state: XdgShellState,
    pub layer_shell_state: WlrLayerShellState,
    pub dmabuf_state: DmabufState,
    /// The dmabuf global, once the renderer's formats are known.
    pub dmabuf_global: Option<DmabufGlobal>,

    /// The one seat. Input devices are all merged into it.
    pub seat: Seat<Tvbox>,
    /// What the pointer should look like, as the focused client asked.
    pub cursor_status: CursorImageStatus,
    /// Where the pointer is, in output coordinates.
    pub pointer_location: Point<f64, Logical>,
}

impl Tvbox {
    /// Find a mapped surface under a point, for pointer focus.
    pub fn surface_under(
        &self,
        position: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let output = self.output.as_ref()?;
        let output_position = self.space.output_geometry(output)?.loc.to_f64();

        // Layer surfaces on the upper layers sit above every window, so they are
        // asked first - that is where the shell's UI lives.
        let layers = layer_map_for_output(output);
        let upper = layers
            .layer_under(Layer::Overlay, position)
            .or_else(|| layers.layer_under(Layer::Top, position));
        if let Some(layer) = upper {
            let layer_position = layers.layer_geometry(layer)?.loc.to_f64() + output_position;
            if let Some((surface, offset)) =
                layer.surface_under(position - layer_position, WindowSurfaceType::ALL)
            {
                return Some((surface, layer_position + offset.to_f64()));
            }
        }
        drop(layers);

        if let Some((window, location)) = self.space.element_under(position) {
            if let Some((surface, offset)) =
                window.surface_under(position - location.to_f64(), WindowSurfaceType::ALL)
            {
                return Some((surface, location.to_f64() + offset.to_f64()));
            }
        }

        let layers = layer_map_for_output(output);
        let lower = layers
            .layer_under(Layer::Bottom, position)
            .or_else(|| layers.layer_under(Layer::Background, position));
        if let Some(layer) = lower {
            let layer_position = layers.layer_geometry(layer)?.loc.to_f64() + output_position;
            if let Some((surface, offset)) =
                layer.surface_under(position - layer_position, WindowSurfaceType::ALL)
            {
                return Some((surface, layer_position + offset.to_f64()));
            }
        }

        None
    }

    /// The window a `wl_surface` belongs to, if any.
    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .find(|window| window.wl_surface().map(|s| *s == *surface).unwrap_or(false))
            .cloned()
    }

    /// Ask for a frame. Nothing else schedules one: without damage the compositor
    /// sits still, which is the point, but it also means every change has to say so.
    pub fn queue_redraw(&mut self) {
        if let Some(device) = self.tty.device.as_mut() {
            if let Some(surface) = device.surface.as_mut() {
                surface.redraw_needed = true;
            }
        }
        crate::backend::render(self);
    }

    /// Give the keyboard to whatever should have it now: the topmost layer surface
    /// that asked for it, otherwise the newest window.
    pub fn refresh_keyboard_focus(&mut self) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };

        let mut target = None;
        if let Some(output) = self.output.as_ref() {
            let layers = layer_map_for_output(output);
            for layer in layers.layers() {
                if layer.can_receive_keyboard_focus() {
                    target = Some(layer.wl_surface().clone());
                }
            }
        }
        let target = target.or_else(|| {
            self.space
                .elements()
                .next_back()
                .and_then(|window| window.wl_surface().map(|s| s.into_owned()))
        });

        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        keyboard.set_focus(self, target, serial);
    }
}

impl CompositorHandler for Tvbox {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        // Import the newly attached buffer into smithay's per-surface renderer
        // state. Without this the surface has no buffer as far as rendering is
        // concerned: clients commit, the scene stays empty, and nothing is ever
        // released back to them.
        on_commit_buffer_handler::<Self>(surface);

        // A subsurface commit is applied with its parent, so wait for that.
        if is_sync_subsurface(surface) {
            return;
        }

        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }
        if let Some(window) = self.window_for_surface(&root) {
            window.on_commit();
            // A TV box has one screen and one thing on it.
            self.fullscreen(&window);
        }

        self.popups.commit(surface);
        self.ensure_initial_configure(surface);
        self.queue_redraw();
    }
}

impl Tvbox {
    /// Put a window on the output at its full size.
    fn fullscreen(&mut self, window: &Window) {
        let Some(output) = self.output.clone() else {
            return;
        };
        let Some(geometry) = self.space.output_geometry(&output) else {
            return;
        };

        if let Some(toplevel) = window.toplevel() {
            let changed = toplevel.with_pending_state(|state| {
                let wanted = Some(geometry.size);
                let already =
                    state.size == wanted && state.states.contains(xdg_toplevel::State::Fullscreen);
                state.size = wanted;
                state.states.set(xdg_toplevel::State::Fullscreen);
                state.states.set(xdg_toplevel::State::Activated);
                !already
            });
            if changed && toplevel.is_initial_configure_sent() {
                toplevel.send_pending_configure();
            }
        }

        self.space.map_element(window.clone(), geometry.loc, true);
    }

    /// Send the first configure a surface is waiting for.
    fn ensure_initial_configure(&mut self, surface: &WlSurface) {
        if let Some(window) = self.window_for_surface(surface) {
            if let Some(toplevel) = window.toplevel() {
                let sent = with_states(surface, |states| {
                    states
                        .data_map
                        .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                        .map(|data| data.lock().unwrap().initial_configure_sent)
                        .unwrap_or(true)
                });
                if !sent {
                    toplevel.send_configure();
                }
            }
            return;
        }

        if let Some(output) = self.output.clone() {
            let mut layers = layer_map_for_output(&output);
            let layer = layers
                .layers()
                .find(|layer| layer.wl_surface() == surface)
                .cloned();
            if let Some(layer) = layer {
                let sent = with_states(surface, |states| {
                    states
                        .data_map
                        .get::<smithay::wayland::shell::wlr_layer::LayerSurfaceData>()
                        .map(|data| data.lock().unwrap().initial_configure_sent)
                        .unwrap_or(true)
                });
                layers.arrange();
                if !sent {
                    layer.layer_surface().send_configure();
                }
            }
        }
    }
}

impl BufferHandler for Tvbox {
    fn buffer_destroyed(
        &mut self,
        _buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    ) {
    }
}

impl ShmHandler for Tvbox {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl OutputHandler for Tvbox {}

impl SeatHandler for Tvbox {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}
}

impl SelectionHandler for Tvbox {
    type SelectionUserData = ();
}

impl DataDeviceHandler for Tvbox {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

// A TV box has nothing to drag and nowhere to drop it; the default refuses the
// grab.
impl WaylandDndGrabHandler for Tvbox {}

impl XdgShellHandler for Tvbox {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let window = Window::new_wayland_window(surface);
        self.space.map_element(window.clone(), (0, 0), true);
        self.fullscreen(&window);
        self.refresh_keyboard_focus();
        self.queue_redraw();
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        if let Err(err) = self.popups.track_popup(surface.into()) {
            warn!(?err, "failed to track a popup");
        }
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        surface.send_repositioned(token);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let window = self
            .space
            .elements()
            .find(|window| window.toplevel().map(|t| *t == surface).unwrap_or(false))
            .cloned();
        if let Some(window) = window {
            self.space.unmap_elem(&window);
        }
        self.refresh_keyboard_focus();
        self.queue_redraw();
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        // Everything is fullscreen here anyway; just acknowledge it.
        let window = self
            .space
            .elements()
            .find(|window| window.toplevel().map(|t| *t == surface).unwrap_or(false))
            .cloned();
        if let Some(window) = window {
            self.fullscreen(&window);
        }
    }

    fn unfullscreen_request(&mut self, _surface: ToplevelSurface) {}
}

impl WlrLayerShellHandler for Tvbox {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        wl_output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        let output = wl_output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| self.output.clone());
        let Some(output) = output else {
            warn!(namespace, "a layer surface arrived before any output");
            return;
        };

        debug!(namespace, "new layer surface");
        let mut layers = layer_map_for_output(&output);
        if let Err(err) = layers.map_layer(&LayerSurface::new(surface, namespace)) {
            warn!(?err, "failed to map a layer surface");
        }
        drop(layers);
        self.refresh_keyboard_focus();
        self.queue_redraw();
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        if let Some(output) = self.output.clone() {
            let mut layers = layer_map_for_output(&output);
            let layer = layers
                .layers()
                .find(|layer| layer.layer_surface() == &surface)
                .cloned();
            if let Some(layer) = layer {
                layers.unmap_layer(&layer);
            }
        }
        self.refresh_keyboard_focus();
        self.queue_redraw();
    }
}

impl DmabufHandler for Tvbox {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: ImportNotifier,
    ) {
        // Importing into the renderer is what tells us we could composite this
        // buffer if it turns out we cannot scan it out.
        match self.tty.import_dmabuf(&dmabuf) {
            Ok(()) => {
                let _ = notifier.successful::<Tvbox>();
            }
            Err(err) => {
                debug!(?err, "rejected a client dmabuf");
                notifier.failed();
            }
        }
    }
}

delegate_compositor!(Tvbox);
delegate_shm!(Tvbox);
smithay::delegate_viewporter!(Tvbox);
smithay::delegate_presentation!(Tvbox);
smithay::delegate_single_pixel_buffer!(Tvbox);
delegate_seat!(Tvbox);
delegate_data_device!(Tvbox);
delegate_output!(Tvbox);
delegate_xdg_shell!(Tvbox);
delegate_layer_shell!(Tvbox);
delegate_dmabuf!(Tvbox);

/// Keep smithay's idea of which output a surface is presented on up to date, so
/// frame callbacks and presentation feedback go out at the right rate.
pub fn refresh_primary_scanout_output(state: &mut Tvbox) {
    let Some(output) = state.output.clone() else {
        return;
    };
    for window in state.space.elements() {
        window.with_surfaces(|surface, states| {
            smithay::desktop::utils::update_surface_primary_scanout_output(
                surface,
                &output,
                states,
                None,
                &Default::default(),
                default_primary_scanout_output_compare,
            );
        });
    }
}
