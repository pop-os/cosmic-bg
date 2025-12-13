// SPDX-License-Identifier: MPL-2.0

//! File type detection utilities for animated wallpapers.

use std::path::Path;

/// Supported animated/video file extensions.
///
/// These are checked case-insensitively when determining if a file can be
/// rendered as an animated wallpaper.
pub const ANIMATED_EXTENSIONS: &[&str] = &[
    "gif",  // GIF animation (decoded in CPU, cached in memory)
    "mp4",  // MPEG-4 container (typically H.264/H.265 codec)
    "webm", // WebM container (VP8/VP9/AV1 - best for AMD hardware decode)
    "mkv",  // Matroska container (any codec)
    "avi",  // AVI container (legacy format)
    "mov",  // QuickTime container (typically H.264)
    "m4v",  // MPEG-4 Video (Apple variant of MP4)
    "ogv",  // Ogg Video container (Theora codec)
];

/// Check if a path points to an animated/video file.
#[must_use]
pub fn is_animated_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ANIMATED_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Check if a path points to a GIF file.
#[must_use]
pub fn is_gif_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("gif"))
        .unwrap_or(false)
}

/// Check if a path points to a video file (non-GIF animated).
#[must_use]
pub fn is_video_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            let ext_lower = ext.to_lowercase();
            ANIMATED_EXTENSIONS.contains(&ext_lower.as_str()) && ext_lower != "gif"
        })
        .unwrap_or(false)
}
