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
    let file = std::fs::File::create(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
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
