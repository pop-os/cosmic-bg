// SPDX-License-Identifier: MPL-2.0

//! Animated wallpaper support using GStreamer for hardware-accelerated video playback.
//!
//! This module provides smooth video and GIF wallpaper playback by leveraging:
//! - **Hardware video decoding** via GStreamer's `decodebin` with automatic codec selection
//! - **Multi-vendor GPU support**: NVIDIA (NVDEC), AMD/Intel (VAAPI), ARM (V4L2)
//! - **Zero-copy DMA-BUF rendering** for maximum performance (no GPU→CPU→GPU roundtrip)
//! - **60fps playback** via `videorate` capped at display refresh rate
//! - **Efficient memory handling** with direct DMA-BUF to compositor
//!
//! # Module Structure
//!
//! - [`types`]: Core types (AnimatedFrame, RawVideoFrame, VideoFrameInfo, AnimatedSource)
//! - [`detection`]: File type detection utilities
//! - [`video_player`]: GStreamer-based hardware-accelerated video player
//! - [`player`]: Unified animated player supporting GIF and video
//!
//! # Supported Formats
//!
//! | Format | Extension | Hardware Decode Support |
//! |--------|-----------|------------------------|
//! | GIF    | `.gif`    | N/A (CPU decoded, cached in memory) |
//! | AVIF   | `.avif`   | AV1 hardware decode (VAAPI, NVDEC) |
//! | MPEG-4 | `.mp4`, `.m4v` | NVIDIA (all codecs), AMD/Intel (VP9, AV1) |
//! | WebM   | `.webm`   | Full (VP8, VP9, AV1) |
//! | Matroska | `.mkv`  | Depends on contained codec |
//! | AVI    | `.avi`    | Depends on contained codec |
//! | QuickTime | `.mov` | Depends on contained codec |
//!
//! # Pipeline Priority (Highest to Lowest)
//!
//! 1. **NVIDIA CUDA→DMA-BUF** (`cudadmabufupload`): Optimal zero-copy, no GL context
//! 2. **VAAPI DMA-BUF** (AMD/Intel): Native DMA-BUF export
//! 3. **NVIDIA GL DMA-BUF**: NVDEC → GL → gldownload DMA-BUF
//! 4. **VAAPI wl_shm**: Fallback with CPU buffer copy
//! 5. **NVIDIA GL wl_shm**: Fallback with CPU buffer copy
//! 6. **Software decode**: CPU decode + CPU convert

mod detection;
mod player;
mod types;
#[cfg(feature = "animated")]
mod video_player;

// Re-export public API
pub use detection::{demote_broken_nvidia_decoders, get_codec_support, is_animated_file};
pub use player::AnimatedPlayer;
pub use types::AnimatedSource;

#[cfg(feature = "animated")]
#[allow(unused_imports)]
pub(crate) use video_player::VideoPlayer;

#[cfg(test)]
mod tests;
