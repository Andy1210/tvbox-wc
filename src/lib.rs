//! tvbox-wc: the Wayland compositor for the tvbox.
//!
//! Not a general-purpose compositor. The whole point is to make the decisions a
//! general one refuses to make: the film takes the display's primary plane, the
//! shell's translucent fullscreen UI takes an overlay plane, the output's colour
//! space follows the content, and the compositor itself does no per-frame GPU work.
//!
//! See `README.md` for why this exists and `docs/measurements.md` for what was
//! measured on the hardware before a line of it was written.

#![warn(missing_docs)]

pub mod cli;
pub mod kms;

mod backend;
mod cursor;
mod input;
mod ipc;
mod render;
mod screenshot;
mod session;
mod stacking;
mod state;
mod typing;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::Session;
use smithay::backend::udev::{UdevBackend, UdevEvent};
use smithay::input::keyboard::XkbConfig;
use smithay::input::pointer::CursorImageStatus;
use smithay::input::SeatState;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::signals::{Signal, Signals};
use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay::reexports::wayland_server::{Display, DisplayHandle};
use smithay::wayland::compositor::CompositorState;
use smithay::wayland::dmabuf::{DmabufFeedbackBuilder, DmabufState};
use smithay::wayland::idle_inhibit::IdleInhibitManagerState;
use smithay::wayland::input_method::InputMethodManagerState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::presentation::PresentationState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::wlr_layer::WlrLayerShellState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::single_pixel_buffer::SinglePixelBufferState;
use smithay::wayland::socket::ListeningSocketSource;
use smithay::wayland::text_input::TextInputManagerState;
use smithay::wayland::viewporter::ViewporterState;
use tracing::{info, warn};

use crate::backend::Tty;
use crate::state::{ClientState, Tvbox};

/// How long a button must be held before it starts repeating, in milliseconds.
const REPEAT_DELAY_MS: i32 = 450;

/// How many repeats a second once it does.
const REPEAT_RATE_HZ: i32 = 9;

/// Run the compositor until it is asked to stop.
pub fn run(options: cli::Options) -> Result<()> {
    let mut event_loop: EventLoop<Tvbox> = EventLoop::try_new().context("EventLoop::try_new")?;
    let display: Display<Tvbox> = Display::new().context("Display::new")?;
    let display_handle = display.handle();

    let (session, session_notifier) =
        LibSeatSession::new().context("failed to open a libseat session")?;
    let seat_name = session.seat();

    let mut tty = Tty::new(session.clone());
    tty.open_device()?;

    // One seat, made before the state so no second wl_seat global is ever created:
    // a client that sees two seats picks one and may end up with neither keyboard
    // nor pointer.
    let mut seat_state = SeatState::<Tvbox>::new();
    let mut seat = seat_state.new_wl_seat(&display_handle, seat_name.clone());
    // Key repeat is a remote's repeat, not a keyboard's. The client does the
    // repeating from these numbers, and a desktop's (200 ms, 25 a second) turns an
    // ordinary press of a TV remote button - 300 ms or so, the button is stiff and
    // the hand is on a sofa - into five events: five characters typed, or five rows
    // scrolled past the one that was wanted. Worse, the repeat runs in a client that
    // may be busy, so the extra keys arrive after the button is back up and it reads
    // as a flaky remote rather than as a setting. Holding a direction still scrolls,
    // at a rate someone can follow.
    seat.add_keyboard(XkbConfig::default(), REPEAT_DELAY_MS, REPEAT_RATE_HZ)
        .context("failed to add a keyboard")?;
    seat.add_pointer();

    let mut state = Tvbox {
        running: Arc::new(AtomicBool::new(true)),
        display_handle: display_handle.clone(),
        loop_handle: event_loop.handle(),
        tty,
        output: None,
        space: Default::default(),
        popups: Default::default(),
        compositor_state: CompositorState::new::<Tvbox>(&display_handle),
        shm_state: ShmState::new::<Tvbox>(&display_handle, Vec::new()),
        seat_state,
        data_device_state: DataDeviceState::new::<Tvbox>(&display_handle),
        xdg_shell_state: XdgShellState::new::<Tvbox>(&display_handle),
        // Advertised so a client does not conclude it has to decorate itself: the
        // toolkits that do reach for libdecor, and RetroArch hangs in it here
        // before it ever creates a window.
        xdg_decoration_state: XdgDecorationState::new::<Tvbox>(&display_handle),
        // A client that cannot ask for this over Wayland asks over D-Bus instead,
        // and RetroArch's fallback waits out the full 25-second D-Bus timeout
        // before the game starts.
        idle_inhibit_state: IdleInhibitManagerState::new::<Tvbox>(&display_handle),
        idle_inhibitors: Vec::new(),
        layer_shell_state: WlrLayerShellState::new::<Tvbox>(&display_handle),
        dmabuf_state: DmabufState::new(),
        dmabuf_global: None,
        seat,
        cursor_status: CursorImageStatus::default_named(),
        pointer_location: (0.0, 0.0).into(),
        pointer_visible: true,
        pointer_moved_at: std::time::Instant::now(),
        focus: Default::default(),
        placements: Default::default(),
    };

    // A box plugged in while the TV is off has no connector yet, and that is a wait
    // rather than a failure - the udev source below brings the output up when one
    // appears. Everything past this point works with no output; nothing renders
    // until there is one.
    if let Some(output) = state.tty.init_output()? {
        output.create_global::<Tvbox>(&display_handle);
        state.space.map_output(&output, (0, 0));
        state.output = Some(output);
    }

    let _output_manager = OutputManagerState::new_with_xdg_output::<Tvbox>(&display_handle);
    // mpv's dmabuf output refuses to start without it.
    let _viewporter = ViewporterState::new::<Tvbox>(&display_handle);
    // A video player wants to know when its frame was actually shown, and builds
    // its background out of a single-pixel buffer rather than an shm surface.
    let _presentation =
        PresentationState::new::<Tvbox>(&display_handle, libc::CLOCK_MONOTONIC as u32);
    let _single_pixel = SinglePixelBufferState::new::<Tvbox>(&display_handle);
    // The shell types into a focused field from its on-screen keyboard or a paired
    // phone. Chromium acts on text-input-v3, so the compositor sends the text there
    // rather than synthesising key events, which would need a keymap carrying every
    // character in the string.
    let _text_input = TextInputManagerState::new::<Tvbox>(&display_handle);
    // Smithay only activates a text input while an input method exists, so one is
    // advertised even though nothing else uses it.
    let _input_method = InputMethodManagerState::new::<Tvbox, _>(&display_handle, |_client| true);

    state.tty.bind_wl_display(&display_handle);
    if let Some(node) = state.tty.render_node() {
        let formats = state.tty.renderer_formats();
        info!(
            node = ?node,
            formats = formats.len(),
            nv12 = formats
                .iter()
                .filter(|f| f.code == smithay::backend::allocator::Fourcc::Nv12)
                .count(),
            "advertising dmabuf formats"
        );
        let builder = DmabufFeedbackBuilder::new(node.dev_id(), formats);

        match builder.build() {
            Ok(feedback) => {
                let global = state
                    .dmabuf_state
                    .create_global_with_default_feedback::<Tvbox>(&display_handle, &feedback);
                state.dmabuf_global = Some(global);
            }
            Err(err) => warn!(?err, "failed to build dmabuf feedback"),
        }
    }

    // Event sources.
    if let Some(notifier) = state.tty.notifier.take() {
        event_loop
            .handle()
            .insert_source(notifier, |event, _, state| {
                backend::on_drm_event(state, event);
            })
            .map_err(|err| anyhow::anyhow!("failed to insert the DRM source: {err}"))?;
    }

    event_loop
        .handle()
        .insert_source(session_notifier, |event, _, state| {
            backend::on_session_event(
                state,
                matches!(event, smithay::backend::session::Event::ActivateSession),
            );
        })
        .map_err(|err| anyhow::anyhow!("failed to insert the session source: {err}"))?;

    // A TV being switched off and on arrives as a udev change on the DRM device.
    let udev = UdevBackend::new(&seat_name).context("failed to open the udev backend")?;
    event_loop
        .handle()
        .insert_source(udev, |event, _, state| {
            if let UdevEvent::Changed { .. } = event {
                state.on_connector_change();
            }
        })
        .map_err(|err| anyhow::anyhow!("failed to insert the udev source: {err}"))?;

    cursor::hide_when_idle(&event_loop.handle())?;

    let libinput = input::init(&session, &seat_name)?;
    event_loop
        .handle()
        .insert_source(libinput, |event, _, state| {
            input::handle(state, event);
        })
        .map_err(|err| anyhow::anyhow!("failed to insert the input source: {err}"))?;

    let socket = ListeningSocketSource::new_auto().context("failed to bind a wayland socket")?;
    let socket_name = socket.socket_name().to_os_string();
    event_loop
        .handle()
        .insert_source(socket, move |stream, _, state| {
            if let Err(err) = state
                .display_handle
                .insert_client(stream, Arc::new(ClientState::default()))
            {
                warn!(?err, "failed to accept a client");
            }
        })
        .map_err(|err| anyhow::anyhow!("failed to insert the socket source: {err}"))?;

    event_loop
        .handle()
        .insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, state| {
                // Safety: the display is not dropped while the loop runs.
                unsafe { display.get_mut().dispatch_clients(state) }?;
                Ok(PostAction::Continue)
            },
        )
        .map_err(|err| anyhow::anyhow!("failed to insert the wayland source: {err}"))?;

    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
    let control_socket = ipc::listen(
        &event_loop.handle(),
        std::path::PathBuf::from(runtime_dir).join("tvbox-wc.sock"),
    )?;

    unsafe { std::env::set_var("WAYLAND_DISPLAY", &socket_name) };
    // The shell finds the control socket the same way it finds the display.
    unsafe { std::env::set_var("TVBOX_WC_SOCKET", &control_socket) };
    info!(socket = ?socket_name, control = ?control_socket, "tvbox-wc is up");

    backend::render(&mut state);

    // The session starts only now: it opens the display connection in its first
    // milliseconds, and a socket that is not listening yet is a client that exits.
    let session = match options.session {
        Some(command) => Some(session::spawn(
            &event_loop.handle(),
            command,
            state.running.clone(),
        )?),
        None => None,
    };

    // SIGTERM and SIGINT end the loop the same way the quit combination does, so
    // the session's Drop runs. Without this, systemd stopping the session leaves
    // the shell and mpv behind.
    {
        let running = state.running.clone();
        event_loop
            .handle()
            .insert_source(
                Signals::new(&[Signal::SIGTERM, Signal::SIGINT])
                    .context("failed to watch for signals")?,
                move |event, _, _state| {
                    info!(signal = ?event.signal(), "asked to stop");
                    running.store(false, Ordering::SeqCst);
                },
            )
            .map_err(|err| anyhow::anyhow!("failed to insert the signal source: {err}"))?;
    }

    let running = state.running.clone();
    let signal = event_loop.get_signal();
    let result = event_loop
        .run(Some(Duration::from_millis(16)), &mut state, |state| {
            if !running.load(Ordering::SeqCst) {
                signal.stop();
                state.loop_handle.insert_idle(|_| {});
            }
            ipc::register_pending(state);
            state.space.refresh();
            state.popups.cleanup();
            state::refresh_primary_scanout_output(state);
            let _ = state.display_handle.flush_clients();
        })
        .context("the event loop failed");

    // Not conditional on the loop's result: `session` is dropped here either way,
    // and its Drop stops the session. An error path that skipped this would leave
    // the shell running against a display that is gone.
    drop(session);
    result?;

    Ok(())
}

/// The display handle, for the few places that need one without the state.
pub type Handle = DisplayHandle;
