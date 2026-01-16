// SPDX-License-Identifier: MPL-2.0

//! Core types for animated wallpaper playback.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use image::DynamicImage;

use super::detection::{is_animated_avif, is_video_file};

/// Default frame duration if video metadata is unavailable (60 FPS).
pub(crate) const DEFAULT_FRAME_DURATION: Duration = Duration::from_millis(16);

/// Minimum frame duration to prevent excessive CPU/GPU usage (60 FPS cap).
pub(crate) const MIN_FRAME_DURATION: Duration = Duration::from_millis(16);

/// A decoded video frame with timing information.
#[derive(Clone)]
pub struct AnimatedFrame {
    /// The decoded image data (RGBA).
    #[allow(dead_code)]
    pub image: DynamicImage,
    /// How long this frame should be displayed.
    pub duration: Duration,
    /// Presentation timestamp (nanoseconds). Used for synchronization and debugging.
    #[allow(dead_code)]
    pub pts: Option<u64>,
}

/// Metadata about a video frame without the pixel data.
/// Used for zero-copy frame writing.
#[cfg(feature = "animated")]
#[derive(Clone, Debug)]
pub struct VideoFrameInfo {
    /// Frame width.
    pub width: u32,
    /// Frame height.
    pub height: u32,
    /// Whether the data is in BGRx format (true) or RGBA format (false).
    pub is_bgrx: bool,
}

/// Source type for animated content (for path identification).
#[derive(Debug, Clone)]
pub enum AnimatedSourceType {
    /// AVIF Image Sequence (animated AVIF).
    Avif(PathBuf),
    /// Video file (MP4, WebM, etc.).
    Video(PathBuf),
}

/// Re-export as AnimatedSource for backwards compatibility.
pub type AnimatedSource = AnimatedSourceType;

impl AnimatedSourceType {
    /// Create an animated source from a path.
    #[must_use]
    pub fn from_path(path: &Path) -> Option<Self> {
        if is_animated_avif(path) {
            Some(AnimatedSourceType::Avif(path.to_path_buf()))
        } else if is_video_file(path) {
            Some(AnimatedSourceType::Video(path.to_path_buf()))
        } else {
            None
        }
    }

    /// Get the file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            AnimatedSourceType::Avif(p) | AnimatedSourceType::Video(p) => p,
        }
    }
}
