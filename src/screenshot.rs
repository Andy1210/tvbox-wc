//! Rendering the scene to a file, so the screen can be read from a terminal.
//!
//! Not a screen capture protocol. `wlr-screencopy` would let existing tools work,
//! and it is worth having later, but the reason this exists first is measurement:
//! with the video on a plane and the compositor doing no GPU work, every
//! "everything is fine" reading is worthless unless the picture can be checked. On
//! this hardware a frozen screen and a working one look identical in every counter
//! (measured, twice).
//!
//! What it captures is the SCENE, rendered off-screen, not the planes as the display
//! engine composes them. That is the same thing a capture protocol would hand a
//! client, and it is what tells you whether a client is drawing. It cannot tell you
//! whether the display engine put the right thing on the right plane; the plane
//! state does that.

use std::path::Path;

use anyhow::{Context as _, Result};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::{GlesRenderbuffer, GlesRenderer};
use smithay::backend::renderer::{Bind, ExportMem, Offscreen};
use smithay::output::Output;
use smithay::utils::{Buffer as BufferCoords, Rectangle, Size, Transform};

use crate::render::Element;

/// Render the scene off-screen and write it out as a PNG.
pub fn capture(
    renderer: &mut GlesRenderer,
    output: &Output,
    elements: &[Element],
    path: &Path,
) -> Result<(i32, i32)> {
    let mode = output.current_mode().context("the output has no mode")?;
    let size = mode.size;
    let scale = output.current_scale().fractional_scale();

    // The output's mode is in physical pixels, and a buffer is measured the same
    // way here: the compositor never rotates or scales its single output.
    let buffer_size: Size<i32, BufferCoords> = (size.w, size.h).into();
    let mut target: GlesRenderbuffer = renderer
        .create_buffer(Fourcc::Abgr8888, buffer_size)
        .context("failed to create an off-screen buffer")?;
    let mut framebuffer = renderer
        .bind(&mut target)
        .context("failed to bind the off-screen buffer")?;

    let mut damage = OutputDamageTracker::new(size, scale, Transform::Normal);
    damage
        .render_output(
            renderer,
            &mut framebuffer,
            0,
            elements,
            [0.0, 0.0, 0.0, 1.0],
        )
        .map_err(|err| anyhow::anyhow!("failed to render the scene: {err}"))?;

    let region = Rectangle::from_size(buffer_size);
    let mapping = renderer
        .copy_framebuffer(&framebuffer, region, Fourcc::Abgr8888)
        .map_err(|err| anyhow::anyhow!("failed to read the off-screen buffer: {err}"))?;
    let pixels = renderer
        .map_texture(&mapping)
        .map_err(|err| anyhow::anyhow!("failed to map the off-screen buffer: {err}"))?;

    let non_zero = pixels.iter().filter(|byte| **byte != 0).count();
    tracing::debug!(
        bytes = pixels.len(),
        non_zero,
        first = ?&pixels[..16.min(pixels.len())],
        "read the off-screen buffer"
    );

    write_png(path, size.w, size.h, pixels)?;
    Ok((size.w, size.h))
}

fn write_png(path: &Path, width: i32, height: i32, pixels: &[u8]) -> Result<()> {
    let file = open_target(path)?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .context("failed to write the header")?;
    writer
        .write_image_data(pixels)
        .context("failed to write the image")?;
    Ok(())
}

/// Open the file a screenshot is written to.
///
/// The path comes from whoever is at the other end of the control socket, so the
/// compositor does not follow a symlink there, writes only a regular file, and
/// creates it for its own user alone: a screenshot shows whatever is on the screen,
/// a sign-in code included. An absolute path, because the compositor's working
/// directory is nobody's business.
fn open_target(path: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    anyhow::ensure!(path.is_absolute(), "a screenshot path must be absolute");
    // Refuse anything but a regular file before opening it: opening a FIFO for
    // writing blocks until a reader appears, and a device node may act on the open
    // itself. O_NONBLOCK covers the window between this check and the open.
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => anyhow::ensure!(
            metadata.file_type().is_file(),
            "{} is not a regular file",
            path.display()
        ),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("failed to inspect {}", path.display()))
        }
    }
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    let metadata = file
        .metadata()
        .context("failed to read the screenshot file")?;
    anyhow::ensure!(
        metadata.is_file(),
        "{} is not a regular file",
        path.display()
    );
    // An existing file is only replaced when it is ours, and it keeps no other
    // reader: its mode is brought down to the one a new file gets.
    anyhow::ensure!(
        std::os::unix::fs::MetadataExt::uid(&metadata) == unsafe { libc::geteuid() },
        "{} belongs to another user",
        path.display()
    );
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .context("failed to restrict the screenshot file")?;
    file.set_len(0)
        .context("failed to truncate the screenshot file")?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tvbox-wc-shot-{}", std::process::id()));
        let _ = std::fs::create_dir(&dir);
        dir
    }

    #[test]
    fn a_symlink_is_not_followed() {
        let dir = scratch();
        let target = dir.join("target");
        std::fs::write(&target, b"keep").unwrap();
        let link = dir.join("link.png");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(open_target(&link).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
    }

    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let dir = scratch();
        let fifo = dir.join("fifo.png");
        let _ = std::fs::remove_file(&fifo);
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        // With no reader, a blocking open for writing would never return.
        assert!(open_target(&fifo).is_err());
    }

    #[test]
    fn a_relative_path_is_refused() {
        assert!(open_target(Path::new("shot.png")).is_err());
    }

    #[test]
    fn a_new_file_is_private_and_an_old_one_is_replaced() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch();
        let path = dir.join("shot.png");
        std::fs::write(&path, b"an older, longer screenshot").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        drop(open_target(&path).unwrap());
        let metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(metadata.len(), 0);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    }
}
