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
use smithay::reexports::drm;
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
    /// The connector, kept so modes can be looked up again later.
    pub connector: connector::Handle,
    /// Plane assignment, swapchain and page flips.
    pub compositor: Compositor,
    /// The connector's HDR properties, if the driver has them.
    pub hdr: Option<crate::kms::hdr::HdrProperties>,
    /// The mode the shell asked for, re-applied when the display comes back.
    ///
    /// A TV switched off and on again reconnects as a fresh connector, and the
    /// default is to fall back to its preferred mode. That is how the display ends
    /// up at 1360x768 in the middle of a film.
    pub wanted_mode: Option<drm::control::Mode>,
    /// The connector is currently disconnected, so there is nothing to draw on.
    pub asleep: bool,
    /// A page flip is in flight; the next render waits for its vblank.
    pub frame_pending: bool,
    /// Something changed since the last frame was queued.
    pub redraw_needed: bool,
    /// A render is already scheduled for this turn of the event loop.
    pub redraw_queued: bool,
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
    ///
    /// `Ok(None)` means there is nothing plugged in yet. That is not a failure: a
    /// box is routinely powered on with the TV off, and exiting would leave greetd
    /// restarting a session that cannot start until someone reaches for a remote.
    /// The udev source calls this again on every connector change.
    ///
    /// The connectors are FORCE-probed here, the same way the hotplug path does it.
    /// Without that this depends on the kernel's initial probe having already seen a
    /// set that may still have been asleep when the driver looked.
    pub fn init_output(&mut self) -> Result<Option<Output>> {
        let device = self
            .device
            .as_mut()
            .ok_or_else(|| anyhow!("no device opened"))?;
        if device.surface.is_some() {
            return Ok(None); // already up; a hotplug is on_connector_change's business
        }

        let resources = device.drm.resource_handles().context("resource_handles")?;
        let connector = resources
            .connectors()
            .iter()
            .filter_map(|handle| device.drm.get_connector(*handle, true).ok())
            .find(|connector| connector.state() == connector::State::Connected);
        let Some(connector) = connector else {
            info!("nothing connected yet - waiting for a display");
            return Ok(None);
        };

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

        let hdr = crate::kms::hdr::HdrProperties::find(device.drm.device_fd(), connector.handle());
        if hdr.is_none() {
            info!("the connector exposes no HDR properties; HDR requests will be refused");
        }

        device.surface = Some(Surface {
            crtc,
            connector: connector.handle(),
            hdr,
            wanted_mode: None,
            asleep: false,
            compositor,
            frame_pending: false,
            redraw_needed: true,
            redraw_queued: false,
        });

        Ok(Some(output))
    }

    /// A mode's refresh in mHz, computed from its timings.
    ///
    /// `vrefresh()` is an integer, so it reports 24 for both 23.976 and 24 - two
    /// modes a film must be able to tell apart, since playing 23.976 content on a
    /// 24 Hz output judders with nothing dropped. This is the same arithmetic
    /// wlr-randr prints.
    fn refresh_mhz(mode: &drm::control::Mode) -> i32 {
        refresh_mhz_from(
            mode.clock(),
            mode.hsync().2 as u32,
            mode.vsync().2 as u32,
            mode.flags().contains(drm::control::ModeFlags::INTERLACE),
            mode.flags().contains(drm::control::ModeFlags::DBLSCAN),
            mode.vrefresh(),
        )
    }

    /// What the output is doing and what it could do, for the shell.
    pub fn outputs(&self) -> Vec<crate::ipc::OutputInfo> {
        let Some(device) = self.device.as_ref() else {
            return Vec::new();
        };
        let Some(surface) = device.surface.as_ref() else {
            return Vec::new();
        };
        let Ok(connector) = device.drm.get_connector(surface.connector, false) else {
            return Vec::new();
        };

        let current = surface.compositor.surface().pending_mode();
        let describe = |mode: &drm::control::Mode| crate::ipc::ModeInfo {
            w: mode.size().0 as i32,
            h: mode.size().1 as i32,
            refresh: Self::refresh_mhz(mode),
            preferred: mode.mode_type().contains(ModeTypeFlags::PREFERRED),
        };

        let (hdr_supported, hdr_on) = self.hdr_state();
        vec![crate::ipc::OutputInfo {
            name: format!("{:?}-{}", connector.interface(), connector.interface_id()),
            current: Some(describe(&current)),
            modes: connector.modes().iter().map(describe).collect(),
            connected: self.awake(),
            hdr: crate::ipc::HdrInfo {
                supported: hdr_supported,
                on: hdr_on,
            },
        }]
    }

    /// Claim or release the output's colour space for HDR content.
    pub fn set_hdr(&mut self, name: &str, on: bool) -> Result<()> {
        let device = self
            .device
            .as_mut()
            .ok_or_else(|| anyhow!("no device opened"))?;
        let surface = device
            .surface
            .as_mut()
            .ok_or_else(|| anyhow!("no output"))?;
        let connector = device
            .drm
            .get_connector(surface.connector, false)
            .context("failed to read the connector")?;

        let output_name = format!("{:?}-{}", connector.interface(), connector.interface_id());
        if output_name != name {
            return Err(anyhow!("no output named {name}"));
        }

        let hdr = surface
            .hdr
            .as_mut()
            .ok_or_else(|| anyhow!("this connector has no HDR properties"))?;
        hdr.set(device.drm.device_fd(), surface.connector, on)?;
        info!(output = name, on, "HDR claim");
        Ok(())
    }

    /// Whether the output has a colour space claimed.
    pub fn hdr_state(&self) -> (bool, bool) {
        let Some(surface) = self
            .device
            .as_ref()
            .and_then(|device| device.surface.as_ref())
        else {
            return (false, false);
        };
        match surface.hdr.as_ref() {
            Some(hdr) => (true, hdr.claimed()),
            None => (false, false),
        }
    }

    /// Drive the output at a different mode.
    ///
    /// A refresh rate is optional because a TV usually offers one rate per size,
    /// and the shell should not have to know whether the kernel calls it 59.94 or
    /// 60.
    pub fn set_mode(
        &mut self,
        name: &str,
        w: i32,
        h: i32,
        refresh: Option<i32>,
    ) -> Result<OutputMode> {
        let device = self
            .device
            .as_mut()
            .ok_or_else(|| anyhow!("no device opened"))?;
        let surface = device
            .surface
            .as_mut()
            .ok_or_else(|| anyhow!("no output"))?;
        let connector = device
            .drm
            .get_connector(surface.connector, false)
            .context("failed to read the connector")?;

        // A DRM mode size is a u16, so a request that does not fit one cannot match
        // anything - and casting it would silently wrap: 67456 becomes 1920, so a
        // caller's arithmetic bug would change the mode and be told it worked.
        let size = match (u16::try_from(w), u16::try_from(h)) {
            (Ok(w), Ok(h)) if w > 0 && h > 0 => (w, h),
            _ => anyhow::bail!("{w}x{h} is not a mode size"),
        };

        let output_name = format!("{:?}-{}", connector.interface(), connector.interface_id());
        if output_name != name {
            return Err(anyhow!("no output named {name}"));
        }

        let mode = connector
            .modes()
            .iter()
            .filter(|mode| mode.size() == (size.0, size.1))
            .find(|mode| match refresh {
                // Within a millihertz: the shell round-trips what we reported.
                Some(wanted) => (Self::refresh_mhz(mode) - wanted).abs() <= 1,
                None => true,
            })
            .copied()
            .ok_or_else(|| anyhow!("no mode {w}x{h} on {name}"))?;

        surface
            .compositor
            .use_mode(mode)
            .map_err(|err| anyhow!("failed to set the mode: {err}"))?;
        surface.wanted_mode = Some(mode);
        surface.frame_pending = false;
        surface.redraw_needed = true;

        info!(
            output = name,
            mode = format!("{w}x{h}@{}", mode.vrefresh()),
            "mode set"
        );
        Ok(OutputMode {
            size: (w, h).into(),
            refresh: Self::refresh_mhz(&mode),
        })
    }

    /// The formats the display engine can scan out, from the planes this output
    /// actually uses.
    ///
    /// NOT advertised to clients any more - see the comment where the dmabuf
    /// feedback is built for what a scan-out tranche did to Vulkan. Kept because it
    /// answers "what can this display actually take", which is the first question
    /// when a buffer is being composited instead of scanned out.
    #[allow(dead_code)]
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

    /// The device the picture is actually scanned out on.
    ///
    /// Not the same as [`Self::render_node`] here: vc4 drives the display and has no
    /// render node, v3d renders and has no connectors.
    #[allow(dead_code)]
    pub fn scanout_node(&self) -> Option<DrmNode> {
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

    /// A connector changed state: the TV was switched off, or came back.
    ///
    /// Returns the mode now in use, if the output is alive, so the caller can put
    /// the scene back on it.
    pub fn on_connector_change(&mut self) -> Option<OutputMode> {
        let device = self.device.as_mut()?;
        let surface = device.surface.as_mut()?;
        let connector = device.drm.get_connector(surface.connector, true).ok()?;
        let connected = connector.state() == connector::State::Connected;

        if !connected {
            if !surface.asleep {
                info!("the display went away");
                surface.asleep = true;
            }
            return None;
        }

        if !surface.asleep {
            return None;
        }
        info!("the display came back");
        surface.asleep = false;
        // Whatever happens below, drawing has to be possible again: every early
        // return here used to leave frame_pending set from the frame that was in
        // flight when the TV went away, and then nothing ever drew - the set comes
        // back on to a black screen.
        surface.frame_pending = false;
        surface.redraw_needed = true;

        // Re-apply what the shell asked for if the display still offers it. Falling
        // back to the preferred mode is what makes a TV that was switched off during
        // a film come back at the wrong size.
        let wanted = surface
            .wanted_mode
            .filter(|wanted| connector.modes().iter().any(|mode| mode == wanted))
            .or_else(|| {
                connector
                    .modes()
                    .iter()
                    .find(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
                    .copied()
            })
            .or_else(|| connector.modes().first().copied())?;

        if let Err(err) = surface.compositor.use_mode(wanted) {
            warn!(?err, "failed to restore the mode");
            return None;
        }
        if let Err(err) = surface.compositor.reset_state() {
            // The next commit will be built on state the driver no longer agrees
            // with, so say it at a level that reaches the log rather than carrying
            // on quietly.
            warn!(
                ?err,
                "failed to reset the compositor state - the next frame may be refused"
            );
        }

        info!(
            mode = format!(
                "{}x{}@{}",
                wanted.size().0,
                wanted.size().1,
                wanted.vrefresh()
            ),
            "restored the mode"
        );
        Some(OutputMode {
            size: (wanted.size().0 as i32, wanted.size().1 as i32).into(),
            refresh: Self::refresh_mhz(&wanted),
        })
    }

    /// Whether the output has something to draw on.
    pub fn awake(&self) -> bool {
        self.device
            .as_ref()
            .and_then(|device| device.surface.as_ref())
            .map(|surface| !surface.asleep)
            .unwrap_or(false)
    }

    /// Whether the session currently owns the device.
    pub fn is_active(&self) -> bool {
        self.session.is_active()
    }
}

/// A mode's refresh in mHz, from its timings.
///
/// Separate from the `Mode` it is read out of so it can be tested: drm-rs offers no
/// way to build one, and this is the arithmetic that decides whether 23.976 and 24
/// stay two modes.
fn refresh_mhz_from(
    clock_khz: u32,
    htotal: u32,
    vtotal: u32,
    interlace: bool,
    doublescan: bool,
    vrefresh_fallback: u32,
) -> i32 {
    let per_frame = htotal as u64 * vtotal as u64;
    if per_frame == 0 {
        return (vrefresh_fallback * 1000) as i32;
    }
    let mut refresh = (clock_khz as u64 * 1_000_000 + per_frame / 2) / per_frame;
    if interlace {
        refresh *= 2;
    }
    if doublescan {
        refresh /= 2;
    }
    refresh as i32
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
/// How long to wait before trying a frame again after one failed. Long enough not
/// to spin on a device that is busy, short enough that the picture comes back on
/// its own rather than waiting for a client to commit something.
const RETRY_AFTER: std::time::Duration = std::time::Duration::from_millis(50);

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
    if surface.asleep {
        return;
    }
    if surface.frame_pending || !surface.redraw_needed {
        trace!(
            frame_pending = surface.frame_pending,
            redraw_needed = surface.redraw_needed,
            "skipping a render"
        );
        return;
    }

    let elements = crate::render::elements(
        &mut device.renderer,
        &state.space,
        &output,
        &state.cursor_status,
        state.pointer_location,
        state.pointer_visible,
    );

    trace!(elements = elements.len(), "rendering");

    // Set by the failure paths below: the damage is consumed by then, so the frame
    // is gone and only another attempt brings the picture back. Nothing else would
    // ask for one - a vblank never arrives for a frame that was never queued, and a
    // static launcher screen commits nothing.
    let mut retry = false;

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
                retry = true;
            } else {
                surface.frame_pending = true;
                surface.redraw_needed = false;
            }
        }
        Err(err) => {
            warn!(?err, "failed to render a frame");
            retry = true;
        }
    }

    crate::render::send_frames(&state.space, &output);

    if retry {
        let timer = smithay::reexports::calloop::timer::Timer::from_duration(RETRY_AFTER);
        let _ = state.loop_handle.insert_source(timer, |_, _, state| {
            state.queue_redraw();
            smithay::reexports::calloop::timer::TimeoutAction::Drop
        });
    }
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

#[cfg(test)]
mod tests {
    use super::refresh_mhz_from;

    #[test]
    fn refresh_comes_from_the_timings_not_the_rounded_field() {
        // The two 1080p film modes an LG offers. They differ by 1000/1001, and the
        // kernel's integer vrefresh calls both of them 24 - playing 23.976 content
        // on a 24 Hz output judders with nothing dropped, so they must stay apart.
        assert_eq!(refresh_mhz_from(74250, 2750, 1125, false, false, 24), 24000);
        assert_eq!(refresh_mhz_from(74176, 2750, 1125, false, false, 24), 23976);
    }

    #[test]
    fn the_panel_rate_that_is_not_quite_sixty() {
        // 1360x768 on the box's LG: 60.015 Hz, which no integer field can carry.
        assert_eq!(refresh_mhz_from(85500, 1792, 795, false, false, 60), 60015);
    }

    #[test]
    fn interlace_and_doublescan_are_accounted_for() {
        assert_eq!(refresh_mhz_from(74250, 2200, 1125, true, false, 60), 60000);
        assert_eq!(refresh_mhz_from(74250, 2200, 1125, false, true, 15), 15000);
    }

    #[test]
    fn a_mode_with_no_timings_falls_back_to_the_reported_rate() {
        assert_eq!(refresh_mhz_from(0, 0, 0, false, false, 50), 50000);
    }
}
