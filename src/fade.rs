// SPDX-License-Identifier: MPL-2.0

use std::time::Duration;

/// A wallpaper fading in over the one beneath it.
#[derive(Debug, Clone, Copy)]
pub struct Fade {
    duration: Duration,
    start: Option<u32>,
}

impl Fade {
    #[must_use]
    pub fn new(duration: Duration) -> Self {
        Self {
            duration,
            start: None,
        }
    }

    /// How far through the fade the frame at `time` is, and whether it is over.
    #[must_use]
    pub fn progress(&mut self, time: u32) -> (f32, bool) {
        let start = *self.start.get_or_insert(time);
        let duration = u32::try_from(self.duration.as_millis()).unwrap_or(u32::MAX);
        let elapsed = time.wrapping_sub(start);

        if duration == 0 || elapsed >= duration {
            (1.0, true)
        } else {
            (elapsed as f32 / duration as f32, false)
        }
    }
}

/// A fast smooth interpolation calculation function.
/// p = `percent_complete` in decimal form
#[must_use]
pub fn slerp(start: f32, end: f32, p: f32) -> f32 {
    let t = smootherstep(p);
    (1.0 - t) * start + t * end
}

/// utility function which maps a value [0, 1] -> [0, 1] using the smootherstep function
#[must_use]
pub fn smootherstep(t: f32) -> f32 {
    (6.0 * t.powi(5) - 15.0 * t.powi(4) + 10.0 * t.powi(3)).clamp(0.0, 1.0)
}

/// The alpha factor `wp_alpha_modifier_v1` takes for an alpha in [0, 1].
#[must_use]
pub fn multiplier(alpha: f32) -> u32 {
    (alpha.clamp(0.0, 1.0) * u32::MAX as f32) as u32
}
