//! The display hardware: session, DRM device, output and the render loop.
//!
//! One device, one output, no hotplug of GPUs. That is not a simplification to be
//! fixed later - the box has a single HDMI connector and a soldered-on GPU, and
//! every branch that pretends otherwise is a branch nobody can test here.
//!
//! The Pi's split between a render-only node (v3d) and a display-only node (vc4) is
//! handled by Mesa: a gbm device on the display node renders through v3d, which is
//! why a single `GbmDevice` and a single renderer are enough.

use std::os::unix::io::AsFd as _;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context as _, Result};
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::drm::compositor::{DrmCompositor, FrameFlags};
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmNode, NodeType};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{ImportDma, ImportEgl};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::Session;
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::drm::control::{connector, crtc, Device as _, ModeTypeFlags};
use smithay::reexports::rustix::fs::OFlags;
use smithay::utils::DeviceFd;
use tracing::{debug, info, trace, warn};

use crate::kms::framebuffer::DirectFramebufferExporter;

/// What `DrmCompositor` is instantiated with here.
pub type Compositor =
    DrmCompositor<GbmAllocator<DrmDeviceFd>, DirectFramebufferExporter, (), DrmDeviceFd>;

/// The single output's scan-out state.
pub struct Surface {
    /// The CRTC this output is driven by.
    pub crtc: crtc::Handle,
    /// Plane assignment, swapchain and page flips.
    pub compositor: Compositor,
    /// A page flip is in flight; the next render waits for its vblank.
    pub frame_pending: bool,
    /// Something changed since the last frame was queued.
    pub redraw_needed: bool,
}

/// An opened DRM device and everything hanging off it.
pub struct Device {
    /// The DRM device itself.
    pub drm: DrmDevice,
    /// Buffer allocation, and the EGL display's platform.
    pub gbm: GbmDevice<DrmDeviceFd>,
    /// The renderer, for whatever cannot go on a plane.
    pub renderer: GlesRenderer,
    /// The output, once a connector is up.
    pub surface: Option<Surface>,
}

/// The display side of the compositor.
pub struct Tty {
    session: LibSeatSession,
    /// The DRM device, once opened.
    pub device: Option<Device>,
    /// The DRM event source, handed to the event loop once after opening.
    pub notifier: Option<smithay::backend::drm::DrmDeviceNotifier>,
}

impl Tty {
    /// Take the session's DRM device and bring up a renderer on it.
    pub fn new(session: LibSeatSession) -> Self {
        Tty {
            session,
            device: None,
            notifier: None,
        }
    }

    /// Open the DRM device that actually drives a display.
    ///
    /// Not "the primary GPU": on this hardware the render node has no connectors at
    /// all, so the device is chosen by asking which one has any.
    pub fn open_device(&mut self) -> Result<()> {
        let path = display_device().context("no DRM device with a connector")?;
        info!(?path, "opening the display device");

        let fd = self
            .session
            .open(
                &path,
                OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
            )
            .context("failed to open the DRM device through the session")?;
        let fd = DrmDeviceFd::new(DeviceFd::from(fd));

        let (drm, notifier) = DrmDevice::new(fd.clone(), true).context("DrmDevice::new")?;
        let gbm = GbmDevice::new(fd).context("GbmDevice::new")?;

        let egl_display = unsafe { EGLDisplay::new(gbm.clone()) }.context("EGLDisplay::new")?;
        let egl_context = EGLContext::new(&egl_display).context("EGLContext::new")?;
        let renderer = unsafe { GlesRenderer::new(egl_context) }.context("GlesRenderer::new")?;

        self.device = Some(Device {
            drm,
            gbm,
            renderer,
            surface: None,
        });

        // The caller inserts this into the event loop; returning it here would make
        // the borrow dance worse than handing it over separately.
        self.notifier = Some(notifier);
        Ok(())
    }

    /// Bring up the first connected connector at its preferred mode.
    pub fn init_output(&mut self) -> Result<Output> {
        let device = self
            .device
            .as_mut()
            .ok_or_else(|| anyhow!("no device opened"))?;

        let resources = device.drm.resource_handles().context("resource_handles")?;
        let connector = resources
            .connectors()
            .iter()
            .filter_map(|handle| device.drm.get_connector(*handle, false).ok())
            .find(|connector| connector.state() == connector::State::Connected)
            .ok_or_else(|| anyhow!("no connected connector"))?;

        // The largest mode a TV advertises can be one the hardware cannot drive
        // (DCI-4K, 4096 wide). The preferred mode is the one it means.
        let mode = *connector
            .modes()
            .iter()
            .find(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
            .or_else(|| connector.modes().first())
            .ok_or_else(|| anyhow!("connector advertises no mode"))?;

        let crtc = connector
            .encoders()
            .iter()
            .filter_map(|handle| device.drm.get_encoder(*handle).ok())
            .flat_map(|encoder| resources.filter_crtcs(encoder.possible_crtcs()))
            .next()
            .ok_or_else(|| anyhow!("no CRTC for the connector"))?;

        info!(
            connector = format!("{:?}-{}", connector.interface(), connector.interface_id()),
            mode = format!("{}x{}@{}", mode.size().0, mode.size().1, mode.vrefresh()),
            "bringing up the output"
        );

        let drm_surface = device
            .drm
            .create_surface(crtc, mode, &[connector.handle()])
            .context("create_surface")?;

        let (physical_width, physical_height) = connector.size().unwrap_or((0, 0));
        let output = Output::new(
            format!("{:?}-{}", connector.interface(), connector.interface_id()),
            PhysicalProperties {
                size: (physical_width as i32, physical_height as i32).into(),
                subpixel: Subpixel::Unknown,
                make: "Unknown".into(),
                model: "Unknown".into(),
                serial_number: "Unknown".into(),
            },
        );
        let output_mode = OutputMode {
            size: (mode.size().0 as i32, mode.size().1 as i32).into(),
            refresh: (mode.vrefresh() * 1000) as i32,
        };
        output.change_current_state(Some(output_mode), None, None, Some((0, 0).into()));
        output.set_preferred(output_mode);

        let allocator = GbmAllocator::new(
            device.gbm.clone(),
            GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
        );
        let renderer_formats = device.renderer.dmabuf_formats();

        let compositor = DrmCompositor::new(
            &output,
            drm_surface,
            None,
            allocator,
            DirectFramebufferExporter,
            // Opaque first: an alpha channel on the primary plane costs bandwidth
            // for a channel nothing below can use.
            [
                smithay::backend::allocator::Fourcc::Xrgb8888,
                smithay::backend::allocator::Fourcc::Argb8888,
            ],
            renderer_formats,
            device.drm.cursor_size(),
            Some(device.gbm.clone()),
        )
        .context("DrmCompositor::new")?;

        device.surface = Some(Surface {
            crtc,
            compositor,
            frame_pending: false,
            redraw_needed: true,
        });

        Ok(output)
    }

    /// The formats the display engine can scan out, from the planes this output
    /// actually uses.
    ///
    /// Advertised to clients as a scan-out tranche: without it a client allocates
    /// whatever its renderer likes (BROADCOM_UIF here), no plane can take that
    /// buffer, and the compositor is back to compositing everything - including the
    /// film that was happily on the primary plane a moment earlier.
    ///
    /// Deliberately NOT intersected with what the renderer can import. The Pi's
    /// 10-bit decoder output (P030 + BROADCOM_SAND128) is a format the planes take
    /// and the GLES renderer does not, so intersecting drops exactly the format a
    /// 4K HDR film arrives in - the client then never offers it and the whole
    /// 10-bit path is closed. A tranche is a preference, not a promise: the main
    /// tranche still carries the renderer's formats for a client that cannot use
    /// this one.
    pub fn scanout_formats(&self) -> Vec<smithay::backend::allocator::Format> {
        let Some(device) = self.device.as_ref() else {
            return Vec::new();
        };
        let Some(surface) = device.surface.as_ref() else {
            return Vec::new();
        };
        let drm_surface = surface.compositor.surface();
        let planes = drm_surface.planes();
        let mut formats: Vec<smithay::backend::allocator::Format> =
            std::iter::once(drm_surface.plane_info())
                .chain(planes.overlay.iter())
                .flat_map(|plane| plane.formats.iter().copied())
                .collect();
        formats.sort_by_key(|format| (format.code as u32, u64::from(format.modifier)));
        formats.dedup();
        formats
    }

    /// The formats clients may hand us.
    pub fn renderer_formats(&self) -> Vec<smithay::backend::allocator::Format> {
        self.device
            .as_ref()
            .map(|device| device.renderer.dmabuf_formats().into_iter().collect())
            .unwrap_or_default()
    }

    /// The DRM node clients should allocate on.
    ///
    /// It has to be a RENDER node. On this hardware it is not derived from the card
    /// we opened: the display device (vc4) has no render node of its own, and
    /// rendering happens on a separate device (v3d) that has no connectors. Handing
    /// out the card node instead sends every client to a device it cannot render on.
    pub fn render_node(&self) -> Option<DrmNode> {
        let card = DrmNode::from_file(self.device.as_ref()?.gbm.as_fd()).ok();
        if let Some(node) = card
            .and_then(|node| node.node_with_type(NodeType::Render))
            .and_then(|node| node.ok())
        {
            return Some(node);
        }
        render_node_on_the_system().or(card)
    }

    /// The card node the output lives on, which is where a buffer must be able to
    /// be scanned out.
    pub fn device_node(&self) -> Option<DrmNode> {
        DrmNode::from_file(self.device.as_ref()?.gbm.as_fd()).ok()
    }

    /// Check that a client's dmabuf is at least renderable.
    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> Result<()> {
        let device = self
            .device
            .as_mut()
            .ok_or_else(|| anyhow!("no device opened"))?;
        device
            .renderer
            .import_dmabuf(dmabuf, None)
            .map(|_| ())
            .context("the renderer refused the dmabuf")
    }

    /// Bind the renderer to the display so clients can use EGL.
    pub fn bind_wl_display(&mut self, display: &smithay::reexports::wayland_server::DisplayHandle) {
        if let Some(device) = self.device.as_mut() {
            if let Err(err) = device.renderer.bind_wl_display(display) {
                warn!(?err, "EGL hardware acceleration is unavailable to clients");
            }
        }
    }

    /// Whether the session currently owns the device.
    pub fn is_active(&self) -> bool {
        self.session.is_active()
    }
}

/// Any render node on the system, for hardware whose display device has none.
fn render_node_on_the_system() -> Option<DrmNode> {
    let mut candidates: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with("renderD"))
                .unwrap_or(false)
        })
        .collect();
    candidates.sort();
    candidates
        .into_iter()
        .find_map(|path| DrmNode::from_path(&path).ok())
}

/// The DRM device that has connectors.
fn display_device() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with("card"))
                .unwrap_or(false)
        })
        .collect();
    candidates.sort();

    candidates.into_iter().find(|path| has_connector(path))
}

fn has_connector(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let device = DumbDevice(file);
    device
        .resource_handles()
        .map(|resources| !resources.connectors().is_empty())
        .unwrap_or(false)
}

/// Just enough of a DRM device to ask a card whether it has connectors, without
/// taking it from the session.
struct DumbDevice(std::fs::File);

impl std::os::unix::io::AsFd for DumbDevice {
    fn as_fd(&self) -> std::os::unix::io::BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl smithay::reexports::drm::Device for DumbDevice {}
impl smithay::reexports::drm::control::Device for DumbDevice {}

/// Render the output, if anything changed and no frame is in flight.
pub fn render(state: &mut crate::state::Tvbox) {
    if !state.tty.is_active() {
        return;
    }
    let Some(output) = state.output.clone() else {
        return;
    };
    let Some(device) = state.tty.device.as_mut() else {
        return;
    };
    let Some(surface) = device.surface.as_mut() else {
        return;
    };
    if surface.frame_pending || !surface.redraw_needed {
        trace!(
            frame_pending = surface.frame_pending,
            redraw_needed = surface.redraw_needed,
            "skipping a render"
        );
        return;
    }

    let elements = crate::render::elements(&mut device.renderer, &state.space, &output);

    trace!(elements = elements.len(), "rendering");

    // ALLOW_PRIMARY_PLANE_SCANOUT_ANY is the load-bearing flag, and it is not in
    // DEFAULT: without it an element may only take the primary plane when its format
    // matches the composition swapchain's. The swapchain is XRGB and a decoded frame
    // is NV12 or P030, so every film would be composited instead of scanned out -
    // which is the entire cost this compositor exists to avoid.
    match surface.compositor.render_frame(
        &mut device.renderer,
        &elements,
        [0.0, 0.0, 0.0, 1.0],
        FrameFlags::DEFAULT | FrameFlags::ALLOW_PRIMARY_PLANE_SCANOUT_ANY,
    ) {
        Ok(result) => {
            // What the plane assignment actually decided, per element. This is the
            // only honest answer to "why is this being composited"; the plane count
            // cannot tell composition from scan-out.
            if tracing::enabled!(tracing::Level::DEBUG) {
                let decisions: Vec<_> = result
                    .states
                    .states
                    .iter()
                    .map(|(id, state)| format!("{:?}:{:?}", id, state.presentation_state))
                    .collect();
                debug!(?decisions, "plane assignment");
            }
            if result.is_empty {
                surface.redraw_needed = false;
            } else if let Err(err) = surface.compositor.queue_frame(()) {
                warn!(?err, "failed to queue a frame");
                surface.redraw_needed = false;
            } else {
                surface.frame_pending = true;
                surface.redraw_needed = false;
            }
        }
        Err(err) => {
            warn!(?err, "failed to render a frame");
            surface.redraw_needed = false;
        }
    }

    crate::render::send_frames(&state.space, &output);
}

/// A page flip completed.
pub fn on_drm_event(state: &mut crate::state::Tvbox, event: DrmEvent) {
    match event {
        DrmEvent::VBlank(crtc) => {
            trace!(?crtc, "vblank");
            if let Some(device) = state.tty.device.as_mut() {
                if let Some(surface) = device.surface.as_mut() {
                    if surface.crtc == crtc {
                        surface.frame_pending = false;
                        if let Err(err) = surface.compositor.frame_submitted() {
                            warn!(?err, "frame_submitted failed");
                        }
                    }
                }
            }
            render(state);
        }
        DrmEvent::Error(err) => {
            warn!(?err, "DRM error");
        }
    }
}

/// The session gained or lost the device.
pub fn on_session_event(state: &mut crate::state::Tvbox, active: bool) {
    debug!(active, "session activity changed");
    let Some(device) = state.tty.device.as_mut() else {
        return;
    };
    if active {
        if let Err(err) = device.drm.activate(false) {
            warn!(?err, "failed to activate the DRM device");
        }
        if let Some(surface) = device.surface.as_mut() {
            surface.frame_pending = false;
            surface.redraw_needed = true;
            if let Err(err) = surface.compositor.reset_state() {
                warn!(?err, "failed to reset the compositor state");
            }
        }
        render(state);
    } else {
        device.drm.pause();
    }
}
