//! What the display path has been doing, for `get_state`.
//!
//! Counters since start and a summary of the last frame that was rendered. The
//! point is to answer "is this being scanned out or composited" and "has the
//! output been stalling" from the control socket, without turning on debug logs
//! and reproducing the problem. Everything here is updated on paths that already
//! run, and costs a handful of integer increments per frame.

use serde::Serialize;
use smithay::backend::renderer::element::RenderElementPresentationState;

/// Longest error text kept for `last_error`.
const MAX_ERROR_LEN: usize = 200;

/// Counters and the last frame, as reported by `get_state` under `health`.
#[derive(Debug, Default, Clone, Serialize)]
pub struct Health {
    /// Frames handed to the display.
    pub frames_queued: u64,
    /// Frames the renderer or the plane assignment failed to build.
    pub render_failures: u64,
    /// Frames that were built but that the driver would not take.
    pub queue_failures: u64,
    /// Page flips that never reported completion and were given up on.
    pub flip_watchdog_fired: u64,
    /// HDR claims and releases that the driver accepted.
    pub hdr_claims: u64,
    pub hdr_releases: u64,
    /// HDR claims or releases the driver refused.
    pub hdr_failures: u64,
    /// The most recent failure of any of the above, shortened.
    pub last_error: Option<String>,
    /// The last frame that put something new on screen.
    pub last_frame: Option<FrameSummary>,
}

impl Health {
    /// Record a failure's text.
    pub fn note_error(&mut self, err: &dyn std::fmt::Debug) {
        let mut text = format!("{err:?}");
        if text.len() > MAX_ERROR_LEN {
            let mut cut = MAX_ERROR_LEN;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
        }
        self.last_error = Some(text);
    }
}

/// How the plane assignment placed the elements of one frame.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct FrameSummary {
    /// Elements in the frame.
    pub elements: usize,
    /// Elements that went to a plane as they are, with no GPU pass.
    pub scanned_out: usize,
    /// Elements drawn by the renderer into the composition buffer.
    pub composited: usize,
    /// Elements that were not visible.
    pub skipped: usize,
    /// What the primary plane shows: `"element"` when a client buffer is scanned
    /// out on it directly, `"swapchain"` when it shows the composition buffer.
    pub primary: &'static str,
    /// Overlay planes in use.
    pub overlays: usize,
    /// Whether the cursor plane is in use.
    pub cursor_plane: bool,
}

impl FrameSummary {
    /// Summarise a frame from its elements' presentation states.
    pub fn tally(
        states: impl IntoIterator<Item = RenderElementPresentationState>,
        primary_is_element: bool,
        overlays: usize,
        cursor_plane: bool,
    ) -> Self {
        let mut summary = FrameSummary {
            primary: if primary_is_element {
                "element"
            } else {
                "swapchain"
            },
            overlays,
            cursor_plane,
            ..Default::default()
        };
        for state in states {
            summary.elements += 1;
            match state {
                RenderElementPresentationState::ZeroCopy => summary.scanned_out += 1,
                RenderElementPresentationState::Rendering { .. } => summary.composited += 1,
                RenderElementPresentationState::Skipped => summary.skipped += 1,
            }
        }
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_is_counted_by_where_each_element_went() {
        let summary = FrameSummary::tally(
            [
                RenderElementPresentationState::ZeroCopy,
                RenderElementPresentationState::Rendering { reason: None },
                RenderElementPresentationState::Rendering { reason: None },
                RenderElementPresentationState::Skipped,
            ],
            true,
            1,
            false,
        );
        assert_eq!(summary.elements, 4);
        assert_eq!(summary.scanned_out, 1);
        assert_eq!(summary.composited, 2);
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.primary, "element");
        assert_eq!(summary.overlays, 1);
    }

    #[test]
    fn an_empty_frame_shows_the_swapchain() {
        let summary = FrameSummary::tally([], false, 0, false);
        assert_eq!(summary.elements, 0);
        assert_eq!(summary.primary, "swapchain");
    }

    #[test]
    fn a_long_error_is_cut_on_a_character_boundary() {
        let mut health = Health::default();
        let long = "é".repeat(MAX_ERROR_LEN);
        health.note_error(&long);
        let kept = health.last_error.unwrap();
        assert!(kept.len() <= MAX_ERROR_LEN);
        assert!(kept.starts_with("\"é"));
    }
}
