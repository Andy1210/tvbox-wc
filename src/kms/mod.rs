//! The parts that talk to the display hardware.
//!
//! This is where the box-specific knowledge lives: what the vc4 display engine can
//! scan out, and how a buffer becomes a framebuffer. Kept separate from the rest
//! because it is the half that changes if the hardware does, and the half worth
//! feeding back upstream.

pub mod framebuffer;
pub mod hdr;
