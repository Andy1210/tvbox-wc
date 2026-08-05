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

pub mod kms;
