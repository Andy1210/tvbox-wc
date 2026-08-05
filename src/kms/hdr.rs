//! Claiming the output's colour space for HDR content.
//!
//! Two connector properties do this: `Colorspace` picks the colorimetry the sink is
//! told to expect, and `HDR_OUTPUT_METADATA` carries the infoframe that names the
//! transfer function. Smithay has no colour management of its own at the revision
//! this is built against, so they are set directly.
//!
//! That absence is convenient rather than a gap to work around. wlroots refuses
//! direct scan-out when a buffer's colorimetry does not match the output's, so a
//! compositor there has to advertise a colour-management protocol and the client has
//! to tag its buffers before an HDR film can stay on a plane. Nothing here does
//! that: a P030 buffer keeps its plane, and the connector tells the TV how to read
//! it.
//!
//! The cost is the one the design already accepts. The colour space covers the WHOLE
//! output, so while HDR is claimed the sRGB UI on its overlay plane is read as PQ.
//! That is why this is a claim for the duration of playback rather than a setting.

use anyhow::{Context as _, Result};
use smithay::backend::drm::DrmDeviceFd;
use smithay::reexports::drm::control::atomic::AtomicModeReq;
use smithay::reexports::drm::control::{
    connector, property, AtomicCommitFlags, Device as ControlDevice,
};
use tracing::debug;

/// SMPTE ST 2084, the transfer function every HDR10 film uses.
const EOTF_PQ: u8 = 2;
/// Static metadata type 1, the only type HDMI defines.
const STATIC_METADATA_TYPE_1: u8 = 0;

/// `struct hdr_output_metadata`, as the kernel reads it.
///
/// The mastering display values are left at zero, which means "not stated": the sink
/// then applies its own defaults. Filling them in needs numbers only the player has,
/// so they belong in a later revision of the IPC rather than in invented constants.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct HdrOutputMetadata {
    metadata_type: u32,
    eotf: u8,
    static_metadata_descriptor_id: u8,
    display_primaries: [Chromaticity; 3],
    white_point: Chromaticity,
    max_display_mastering_luminance: u16,
    min_display_mastering_luminance: u16,
    max_cll: u16,
    max_fall: u16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct Chromaticity {
    x: u16,
    y: u16,
}

/// The connector properties that carry an HDR claim.
#[derive(Debug, Clone, Copy)]
pub struct HdrProperties {
    colorspace: property::Handle,
    metadata: property::Handle,
    bt2020_rgb: u64,
    default_colorspace: u64,
    /// The blob currently attached, so it can be freed on release.
    blob: Option<u64>,
}

impl HdrProperties {
    /// Find the properties on a connector, if the driver exposes them.
    pub fn find(drm: &DrmDeviceFd, connector: connector::Handle) -> Option<Self> {
        let props = drm.get_properties(connector).ok()?;

        let mut colorspace = None;
        let mut metadata = None;
        let mut bt2020_rgb = None;
        let mut default_colorspace = None;

        for handle in props.as_props_and_values().0.iter().copied() {
            let Ok(info) = drm.get_property(handle) else {
                continue;
            };
            match info.name().to_str() {
                Ok("Colorspace") => {
                    colorspace = Some(handle);
                    if let property::ValueType::Enum(values) = info.value_type() {
                        let (raw, named) = values.values();
                        for (value, name) in raw.iter().zip(named.iter()) {
                            match name.name().to_str() {
                                Ok("BT2020_RGB") => bt2020_rgb = Some(*value),
                                Ok("Default") => default_colorspace = Some(*value),
                                _ => {}
                            }
                        }
                    }
                }
                Ok("HDR_OUTPUT_METADATA") => metadata = Some(handle),
                _ => {}
            }
        }

        Some(HdrProperties {
            colorspace: colorspace?,
            metadata: metadata?,
            bt2020_rgb: bt2020_rgb?,
            default_colorspace: default_colorspace?,
            blob: None,
        })
    }

    /// Tell the sink to expect PQ in BT.2020, or to go back to what it was.
    pub fn set(&mut self, drm: &DrmDeviceFd, connector: connector::Handle, on: bool) -> Result<()> {
        let new_blob = if on {
            let metadata = HdrOutputMetadata {
                metadata_type: 0,
                eotf: EOTF_PQ,
                static_metadata_descriptor_id: STATIC_METADATA_TYPE_1,
                ..Default::default()
            };
            match drm
                .create_property_blob(&metadata)
                .context("failed to create the HDR metadata blob")?
            {
                property::Value::Blob(id) => id,
                other => return Err(anyhow::anyhow!("unexpected blob value {other:?}")),
            }
        } else {
            0
        };

        let colorspace = if on {
            self.bt2020_rgb
        } else {
            self.default_colorspace
        };

        let mut request = AtomicModeReq::new();
        request.add_raw_property(connector.into(), self.colorspace, colorspace);
        request.add_raw_property(connector.into(), self.metadata, new_blob);

        let result = drm
            .atomic_commit(AtomicCommitFlags::ALLOW_MODESET, request)
            .context("the driver refused the colour space");

        match result {
            Ok(()) => {
                // The kernel holds its own reference while the blob is in use, so the
                // one we replaced can go. Keeping them leaks a blob id per claim.
                if let Some(old) = self.blob.replace(new_blob).filter(|id| *id != 0) {
                    let _ = drm.destroy_property_blob(old);
                }
                if new_blob == 0 {
                    self.blob = None;
                }
                debug!(on, "colour space claimed");
                Ok(())
            }
            Err(err) => {
                if new_blob != 0 {
                    let _ = drm.destroy_property_blob(new_blob);
                }
                Err(err)
            }
        }
    }

    /// Whether a claim is in effect.
    pub fn claimed(&self) -> bool {
        self.blob.is_some()
    }
}
