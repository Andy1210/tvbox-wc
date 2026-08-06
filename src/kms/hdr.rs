//! Claiming the output's colour space for HDR content.
//!
//! Three connector properties do this: `Colorspace` picks the colorimetry the sink
//! is told to expect, `HDR_OUTPUT_METADATA` carries the infoframe that names the
//! transfer function, and `max bpc` decides how many bits actually reach the panel -
//! HDR10 is a 10-bit format, and a link left at 8 bits delivers banded PQ no matter
//! what the infoframe says. Smithay has no colour management of its own at the
//! revision this is built against, so they are set directly.
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
/// Bits per colour the link carries while HDR is claimed. HDR10 is 10-bit; the
/// driver negotiates a subsampling that fits, and drops back if the mode cannot
/// carry it.
const HDR_BPC: u64 = 10;
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
    /// `max bpc`, and what it was before the first claim, so a release puts the
    /// link back where the driver had it rather than at a number of our choosing.
    max_bpc: Option<property::Handle>,
    sdr_bpc: u64,
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
        let mut max_bpc = None;
        let mut sdr_bpc = 8;

        let (handles, values) = props.as_props_and_values();
        for (handle, value) in handles.iter().copied().zip(values.iter().copied()) {
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
                Ok("max bpc") => {
                    max_bpc = Some(handle);
                    sdr_bpc = value;
                }
                _ => {}
            }
        }

        Some(HdrProperties {
            colorspace: colorspace?,
            metadata: metadata?,
            max_bpc,
            sdr_bpc,
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
        if let Some(max_bpc) = self.max_bpc {
            let bpc = if on { HDR_BPC } else { self.sdr_bpc };
            request.add_raw_property(connector.into(), max_bpc, bpc);
        }

        let mut result = drm
            .atomic_commit(AtomicCommitFlags::ALLOW_MODESET, request)
            .context("the driver refused the colour space");

        // A deeper link is what the mode may not have the bandwidth for - 4K60 RGB
        // at 10 bits is past HDMI 2.0. Losing the colour space over that would be
        // the wrong trade: the sink can still read PQ at 8 bits, banding and all.
        //
        // The retry is not limited to the claim. A RELEASE the driver refuses leaves
        // the set in BT.2020 + PQ with an SDR launcher on it - washed out until
        // something else claims and releases successfully - so it is worth trying
        // without the bit depth there too.
        if result.is_err() && self.max_bpc.is_some() {
            let mut retry = AtomicModeReq::new();
            retry.add_raw_property(connector.into(), self.colorspace, colorspace);
            retry.add_raw_property(connector.into(), self.metadata, new_blob);
            result = drm
                .atomic_commit(AtomicCommitFlags::ALLOW_MODESET, retry)
                .context("the driver refused the colour space");
            if result.is_ok() {
                debug!("the link would not take 10 bits - PQ at the depth it has");
            }
        }

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
