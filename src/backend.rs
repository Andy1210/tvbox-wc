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
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmNode};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{ImportDma, ImportEgl};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::Session;
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::drm::control::{connector, crtc, Device as _, ModeTypeFlags};
use smithay::reexports::rustix::fs::OFlags;
use smithay::utils::DeviceFd;
use tracing::{debug, info, warn};

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

    /// The formats clients may hand us.
    pub fn renderer_formats(&self) -> Vec<smithay::backend::allocator::Format> {
        self.device
            .as_ref()
            .map(|device| device.renderer.dmabuf_formats().into_iter().collect())
            .unwrap_or_default()
    }

    /// The DRM node the renderer lives on, for dmabuf feedback.
    pub fn render_node(&self) -> Option<DrmNode> {
        self.device
            .as_ref()
            .and_then(|device| DrmNode::from_file(device.gbm.as_fd()).ok())
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
        return;
    }

    let elements = crate::render::elements(&mut device.renderer, &state.space, &output);

    match surface.compositor.render_frame(
        &mut device.renderer,
        &elements,
        [0.0, 0.0, 0.0, 1.0],
        FrameFlags::DEFAULT,
    ) {
        Ok(result) => {
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
