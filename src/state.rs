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
use smithay::utils::{Logical, Point, Rectangle, Serial};
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
use smithay::wayland::text_input::TextInputSeat;
use smithay::{
    delegate_compositor, delegate_data_device, delegate_dmabuf, delegate_layer_shell,
    delegate_output, delegate_seat, delegate_shm, delegate_xdg_shell,
};
use tracing::{debug, warn};

use crate::backend::Tty;

/// Who owns the screen, as the shell reports it.
///
/// The compositor cannot work this out for itself: the launcher and an app can be
/// windows of the same process, and "an app is on screen" is the shell's own state
/// machine, not a property of any surface.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Focus {
    /// The launcher's own UI.
    #[default]
    Launcher,
    /// An app, named so logs and later per-app policy can tell them apart.
    App(String),
}

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
    /// Whether the pointer is currently drawn.
    ///
    /// A wireless TV remote often presents a mouse endpoint as well, so a box with
    /// no mouse at all still gets a pointer parked on screen that never moves. It is
    /// hidden after [`crate::cursor::IDLE`] without motion and comes back on the
    /// first move.
    pub pointer_visible: bool,
    /// When the pointer last moved.
    pub pointer_moved_at: std::time::Instant,
    /// What the shell says is on screen.
    pub focus: Focus,
    /// Where a client's windows go, by app id. A window with no entry takes the
    /// whole output, which is what a TV box does with almost everything.
    pub placements: std::collections::HashMap<String, Rectangle<i32, Logical>>,
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

        // Front to back by the box's own stacking rule, not map order: the pointer
        // must land on the shell's UI even when a film mapped after it.
        for window in crate::stacking::stacked(&self.space).into_iter().rev() {
            let Some(geometry) = self.space.element_geometry(&window) else {
                continue;
            };
            if !geometry.to_f64().contains(position) {
                continue;
            }
            let location = geometry.loc.to_f64();
            if let Some((surface, offset)) =
                window.surface_under(position - location, WindowSurfaceType::ALL)
            {
                return Some((surface, location + offset.to_f64()));
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

    /// Where a client's windows go from now on. `rect` of `None` puts them back on
    /// the whole output.
    ///
    /// Set BEFORE the client starts: a window is placed as it maps, so a player
    /// launched into a rectangle never appears fullscreen first.
    pub fn set_placement(&mut self, app_id: String, rect: Option<Rectangle<i32, Logical>>) {
        match rect {
            Some(rect) => self.placements.insert(app_id.clone(), rect),
            None => self.placements.remove(&app_id),
        };
        let windows: Vec<Window> = self
            .space
            .elements()
            .filter(|window| crate::stacking::app_id(window).as_deref() == Some(app_id.as_str()))
            .cloned()
            .collect();
        for window in windows {
            self.place(&window);
        }
        self.queue_redraw();
    }

    /// Drive the output at a different mode, and put everything back on it.
    ///
    /// The compositor's own surfaces have to follow: a layer surface is laid out
    /// against the output size, and a fullscreen window is sized to it, so both need
    /// a fresh configure or the screen keeps the old geometry with a new mode under
    /// it.
    pub fn set_mode(
        &mut self,
        name: &str,
        w: i32,
        h: i32,
        refresh: Option<i32>,
    ) -> anyhow::Result<()> {
        let mode = self.tty.set_mode(name, w, h, refresh)?;

        if let Some(output) = self.output.clone() {
            output.change_current_state(Some(mode), None, None, None);
            output.set_preferred(mode);
            self.space.map_output(&output, (0, 0));
            layer_map_for_output(&output).arrange();

            let windows: Vec<Window> = self.space.elements().cloned().collect();
            for window in windows {
                self.fullscreen(&window);
            }
        }

        self.queue_redraw();
        Ok(())
    }

    /// Type a string into whatever field has the keyboard.
    ///
    /// This is how the on-screen keyboard and a paired phone deliver text. The
    /// alternative is synthesising key events, which needs an xkb keymap carrying
    /// every character in the string - accented Hungarian and password symbols
    /// included - generated per string.
    ///
    /// Returns how many keys were sent.
    ///
    /// The text input is offered the string first, because a client that speaks the
    /// protocol takes it whole and keeps its own idea of the caret. Whether it acts
    /// on that is out of our hands, so the keys go out either way; see docs/ipc.md.
    ///
    /// `select_all` replaces the field's contents rather than appending to them.
    pub fn type_text(&mut self, text: &str, select_all: bool) -> anyhow::Result<usize> {
        let text_input = self.seat.text_input();
        let mut offered = false;
        text_input.with_focused_text_input(|input, _surface| {
            input.commit_string(Some(text.to_owned()));
            offered = true;
        });
        if offered {
            text_input.done(false);
        }

        crate::typing::type_text(self, text, select_all)
    }

    /// Render the scene to a PNG.
    pub fn screenshot(&mut self, path: &std::path::Path) -> anyhow::Result<(i32, i32)> {
        let output = self
            .output
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no output"))?;
        let device = self
            .tty
            .device
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no device opened"))?;
        let elements = crate::render::elements(
            &mut device.renderer,
            &self.space,
            &output,
            &self.cursor_status,
            self.pointer_location,
            self.pointer_visible,
        );
        crate::screenshot::capture(&mut device.renderer, &output, &elements, path)
    }

    /// The display went away or came back.
    ///
    /// A TV switched off and on again is the ordinary case here, not an edge case:
    /// the connector reappears and everything laid out against the output has to be
    /// told the size again.
    pub fn on_connector_change(&mut self) {
        let Some(mode) = self.tty.on_connector_change() else {
            return;
        };
        if let Some(output) = self.output.clone() {
            output.change_current_state(Some(mode), None, None, None);
            output.set_preferred(mode);
            self.space.map_output(&output, (0, 0));
            layer_map_for_output(&output).arrange();

            let windows: Vec<Window> = self.space.elements().cloned().collect();
            for window in windows {
                self.fullscreen(&window);
            }
        }
        self.queue_redraw();
    }

    /// Ask for a frame. Nothing else schedules one: without damage the compositor
    /// sits still, which is the point, but it also means every change has to say so.
    ///
    /// The frame itself is rendered from an idle callback rather than here. Several
    /// clients commit within one turn of the event loop, and rendering from each
    /// commit means building the scene once per commit instead of once per frame -
    /// wasted even when the result is thrown away as unchanged. Page flips are
    /// already paced by the vblank; this paces the work in between.
    pub fn queue_redraw(&mut self) {
        let Some(surface) = self
            .tty
            .device
            .as_mut()
            .and_then(|device| device.surface.as_mut())
        else {
            return;
        };
        surface.redraw_needed = true;
        if surface.redraw_queued {
            return;
        }
        surface.redraw_queued = true;

        self.loop_handle.insert_idle(|state| {
            if let Some(surface) = state
                .tty
                .device
                .as_mut()
                .and_then(|device| device.surface.as_mut())
            {
                surface.redraw_queued = false;
            }
            crate::backend::render(state);
        });
    }

    /// Give the keyboard to whatever should have it now: the topmost layer surface
    /// that asked for it, otherwise the frontmost window.
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
            crate::stacking::topmost(&self.space)
                .and_then(|window| window.wl_surface().map(|s| s.into_owned()))
        });

        // The text input follows the keyboard, and smithay does not wire that up:
        // without `enter` a client has nothing to enable, so it ignores anything the
        // compositor commits to it. That is what an empty field looks like when
        // everything else is right.
        let text_input = self.seat.text_input();
        text_input.leave();
        text_input.set_focus(target.clone());
        text_input.enter();

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
            // A TV box has one screen and one thing on it - but only once the client
            // has mapped. Forcing a size and the fullscreen state in the FIRST
            // configure makes a client resize before it has configured its video,
            // and mpv dereferences a null `target_params` when that happens
            // (vo_dmabuf_wayland.c:541). The protocol says the same thing: let the
            // client pick its first size.
            let mapped = window.geometry().size.w > 0 && window.geometry().size.h > 0;
            if mapped {
                self.place(&window);
            }
        }

        self.popups.commit(surface);
        self.ensure_initial_configure(surface);
        self.queue_redraw();
    }
}

impl Tvbox {
    /// Put a window where it belongs: the whole output, or the rectangle the shell
    /// asked for.
    ///
    /// The rectangle is how picture-in-picture works. A Wayland client cannot place
    /// itself, which is why the shell used to run the player under XWayland for
    /// this; the compositor can, so it does.
    pub fn place(&mut self, window: &Window) {
        let wanted = crate::stacking::app_id(window)
            .and_then(|app_id| self.placements.get(&app_id).copied());
        match wanted {
            Some(rect) => self.place_at(window, rect),
            None => self.fullscreen(window),
        }
    }

    /// Put a window in a rectangle, and do not give it the keyboard: the shell's UI
    /// stays in front and keeps the remote, which is the whole point of a small
    /// player - you browse while it plays.
    fn place_at(&mut self, window: &Window, rect: Rectangle<i32, Logical>) {
        if let Some(toplevel) = window.toplevel() {
            let changed = toplevel.with_pending_state(|state| {
                let wanted = Some(rect.size);
                let already =
                    state.size == wanted && !state.states.contains(xdg_toplevel::State::Fullscreen);
                state.size = wanted;
                state.states.unset(xdg_toplevel::State::Fullscreen);
                !already
            });
            if changed && toplevel.is_initial_configure_sent() {
                toplevel.send_pending_configure();
            }
        }
        self.space.map_element(window.clone(), rect.loc, false);
    }

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
        // No state forced here: the first configure goes out empty, and the window
        // is fullscreened once it has mapped. See the comment in commit().
        let window = Window::new_wayland_window(surface);
        self.space.map_element(window.clone(), (0, 0), true);
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

impl smithay::wayland::input_method::InputMethodHandler for Tvbox {
    // No input method popup is expected: the on-screen keyboard is part of the
    // shell's own UI, not a separate client, so there is nothing to place.
    fn new_popup(&mut self, _surface: smithay::wayland::input_method::PopupSurface) {}

    fn dismiss_popup(&mut self, _surface: smithay::wayland::input_method::PopupSurface) {}

    fn popup_repositioned(&mut self, _surface: smithay::wayland::input_method::PopupSurface) {}

    fn parent_geometry(&self, parent: &WlSurface) -> smithay::utils::Rectangle<i32, Logical> {
        self.window_for_surface(parent)
            .map(|window| window.geometry())
            .unwrap_or_default()
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
smithay::delegate_text_input_manager!(Tvbox);
smithay::delegate_input_method_manager!(Tvbox);
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
