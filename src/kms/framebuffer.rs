//! Turning buffers into KMS framebuffers, without gbm in the way for client
//! buffers.
//!
//! Smithay's own [`GbmFramebufferExporter`] imports a client dmabuf into gbm and
//! then builds the framebuffer from the **gbm bo's** per-plane GEM handles. gbm can
//! only describe the planes of a format the render driver knows, and Mesa's
//! v3d/vc4 knows no YUV format at all, so for a decoded video frame the handle for
//! the second plane cannot be right and `drmModeAddFB2` answers EINVAL. Measured on
//! a Pi 5 (see `docs/measurements.md`):
//!
//! ```text
//! gbm import succeeded for NV12 + Broadcom_sand128
//! drmModeAddFB2 from the gbm bo failed ... (Invalid argument, EINVAL)
//! ```
//!
//! So client buffers take the same route wlroots takes: import each dmabuf plane fd
//! with `drmPrimeFDToHandle` and pass the dmabuf's own pitches and offsets. With
//! that, the decoder's frame lands on the primary plane and the shell's translucent
//! UI on an overlay plane, with the compositor doing no GPU work at all.
//!
//! Buffers we allocated ourselves still go through gbm: they are RGB, gbm describes
//! them correctly, and it keeps the bo's own metadata in play.
//!
//! [`GbmFramebufferExporter`]: smithay::backend::drm::exporter::gbm::GbmFramebufferExporter

use smithay::backend::allocator::{
    dmabuf::Dmabuf, format::get_opaque, gbm::GbmBuffer, Buffer, Format, Fourcc, Modifier,
};
use smithay::backend::drm::{
    exporter::{ExportBuffer, ExportFramebuffer},
    gbm::framebuffer_from_bo,
    DrmAccessError, DrmDeviceFd, Framebuffer,
};
use smithay::reexports::drm::{
    buffer::PlanarBuffer,
    control::{framebuffer, Device as ControlDevice, FbCmd2Flags},
};
use tracing::{trace, warn};

/// A framebuffer we own: destroyed with the handle when dropped.
#[derive(Debug)]
pub struct DirectFramebuffer {
    handle: framebuffer::Handle,
    format: Format,
    drm: DrmDeviceFd,
}

impl AsRef<framebuffer::Handle> for DirectFramebuffer {
    fn as_ref(&self) -> &framebuffer::Handle {
        &self.handle
    }
}

impl Framebuffer for DirectFramebuffer {
    fn format(&self) -> Format {
        self.format
    }
}

impl Drop for DirectFramebuffer {
    fn drop(&mut self) {
        if let Err(err) = self.drm.destroy_framebuffer(self.handle) {
            warn!(fb = ?self.handle, ?err, "failed to destroy framebuffer");
        }
    }
}

/// Either kind of framebuffer this exporter produces.
///
/// Our own buffers keep the gbm path, so both variants have to satisfy
/// [`Framebuffer`].
#[derive(Debug)]
pub enum ExportedFramebuffer {
    /// Built from a dmabuf we imported ourselves.
    Direct(DirectFramebuffer),
    /// Built from one of our own gbm buffer objects.
    Gbm(smithay::backend::drm::gbm::GbmFramebuffer),
}

impl AsRef<framebuffer::Handle> for ExportedFramebuffer {
    fn as_ref(&self) -> &framebuffer::Handle {
        match self {
            Self::Direct(fb) => fb.as_ref(),
            Self::Gbm(fb) => fb.as_ref(),
        }
    }
}

impl Framebuffer for ExportedFramebuffer {
    fn format(&self) -> Format {
        match self {
            Self::Direct(fb) => fb.format(),
            Self::Gbm(fb) => fb.format(),
        }
    }
}

/// Errors this exporter can produce.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A plane's dmabuf fd could not be imported on the display device.
    #[error("failed to import a dmabuf plane: {0}")]
    Import(std::io::Error),
    /// The kernel refused the framebuffer.
    #[error("failed to add a framebuffer for {format:?}: {source}")]
    AddFramebuffer {
        /// The format we asked for.
        format: Format,
        /// What the kernel said.
        source: std::io::Error,
    },
    /// The gbm path failed for one of our own buffers.
    #[error("failed to add a framebuffer for an owned buffer: {0}")]
    OwnedBuffer(#[from] DrmAccessError),
}

/// Exports framebuffers for the compositor's DRM surface.
///
/// Client dmabufs are imported directly; our own buffers go through gbm.
#[derive(Debug, Clone)]
pub struct DirectFramebufferExporter;

impl ExportFramebuffer<GbmBuffer> for DirectFramebufferExporter {
    type Framebuffer = ExportedFramebuffer;
    type Error = Error;

    fn add_framebuffer(
        &self,
        drm: &DrmDeviceFd,
        buffer: ExportBuffer<'_, GbmBuffer>,
        use_opaque: bool,
    ) -> Result<Option<Self::Framebuffer>, Self::Error> {
        match buffer {
            ExportBuffer::Wayland(wl_buffer) => {
                let Ok(dmabuf) = smithay::wayland::dmabuf::get_dmabuf(wl_buffer) else {
                    return Ok(None);
                };

                // A buffer allocated with no modifier may still be tiled under the
                // hood, and neither we nor the KMS driver can know how. Handing it
                // to KMS would display garbage, so leave it to the renderer.
                if Buffer::format(dmabuf).modifier == Modifier::Invalid {
                    return Ok(None);
                }

                framebuffer_from_dmabuf(drm, dmabuf, use_opaque)
                    .map(|fb| Some(ExportedFramebuffer::Direct(fb)))
            }
            ExportBuffer::Allocator(bo) => framebuffer_from_bo(drm, bo, use_opaque)
                .map(|fb| Some(ExportedFramebuffer::Gbm(fb)))
                .map_err(Error::OwnedBuffer),
        }
    }

    fn can_add_framebuffer(&self, buffer: &ExportBuffer<'_, GbmBuffer>) -> bool {
        match buffer {
            // Deliberately not filtered by device node. Smithay's gbm exporter
            // compares the dmabuf's recorded node against a filter, which rejects
            // buffers carrying no node - and whether a node is recorded says nothing
            // about whether KMS will take the buffer. Let the kernel decide: a
            // refused framebuffer costs one failed ioctl and falls back to
            // composition.
            ExportBuffer::Wayland(_) => true,
            ExportBuffer::Allocator(_) => true,
        }
    }
}

/// A [`PlanarBuffer`] over a dmabuf whose planes we imported as GEM handles.
struct ImportedDmabuf<'a> {
    dmabuf: &'a Dmabuf,
    handles: [Option<smithay::reexports::drm::buffer::Handle>; 4],
    use_opaque: bool,
}

impl PlanarBuffer for ImportedDmabuf<'_> {
    fn size(&self) -> (u32, u32) {
        (self.dmabuf.width(), self.dmabuf.height())
    }

    fn format(&self) -> Fourcc {
        let format = Buffer::format(self.dmabuf).code;
        if self.use_opaque {
            get_opaque(format).unwrap_or(format)
        } else {
            format
        }
    }

    fn modifier(&self) -> Option<Modifier> {
        Some(Buffer::format(self.dmabuf).modifier).filter(|modifier| *modifier != Modifier::Invalid)
    }

    fn pitches(&self) -> [u32; 4] {
        let mut pitches = [0u32; 4];
        for (index, stride) in self.dmabuf.strides().enumerate().take(4) {
            pitches[index] = stride;
        }
        pitches
    }

    fn handles(&self) -> [Option<smithay::reexports::drm::buffer::Handle>; 4] {
        self.handles
    }

    fn offsets(&self) -> [u32; 4] {
        let mut offsets = [0u32; 4];
        for (index, offset) in self.dmabuf.offsets().enumerate().take(4) {
            offsets[index] = offset;
        }
        offsets
    }
}

/// Build a framebuffer from a dmabuf by importing its plane fds on `drm`.
pub fn framebuffer_from_dmabuf(
    drm: &DrmDeviceFd,
    dmabuf: &Dmabuf,
    use_opaque: bool,
) -> Result<DirectFramebuffer, Error> {
    let mut handles = [None; 4];
    let mut imported = Vec::with_capacity(4);
    for (index, fd) in dmabuf.handles().enumerate().take(4) {
        match drm.prime_fd_to_buffer(fd) {
            Ok(handle) => {
                handles[index] = Some(handle);
                // A multi-plane video buffer hands over the same dma_buf once per
                // plane, and the kernel answers with the SAME GEM handle without
                // taking a second reference. Closing it twice fails on the second
                // go, once per imported frame, at warn level - which is every film.
                if !imported.contains(&handle) {
                    imported.push(handle);
                }
            }
            Err(source) => {
                close_all(drm, &imported);
                return Err(Error::Import(source));
            }
        }
    }

    let buffer = ImportedDmabuf {
        dmabuf,
        handles,
        use_opaque,
    };
    let format = Format {
        code: PlanarBuffer::format(&buffer),
        modifier: Buffer::format(dmabuf).modifier,
    };
    let flags = if PlanarBuffer::modifier(&buffer).is_some() {
        FbCmd2Flags::MODIFIERS
    } else {
        FbCmd2Flags::empty()
    };

    let result = drm.add_planar_framebuffer(&buffer, flags);

    // The framebuffer holds its own reference to the memory, so the GEM handles are
    // not needed past this point - and keeping them leaks the frame.
    close_all(drm, &imported);

    let handle = result.map_err(|source| Error::AddFramebuffer { format, source })?;
    trace!(?format, ?handle, "imported a dmabuf for scan-out");

    Ok(DirectFramebuffer {
        handle,
        format,
        drm: drm.clone(),
    })
}

fn close_all(drm: &DrmDeviceFd, handles: &[smithay::reexports::drm::buffer::Handle]) {
    for handle in handles {
        if let Err(err) = drm.close_buffer(*handle) {
            warn!(?handle, ?err, "failed to close an imported buffer handle");
        }
    }
}
