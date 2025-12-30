// SPDX-License-Identifier: MPL-2.0

//! Unified animated wallpaper player.
//!
//! This module provides [`AnimatedPlayer`], which handles animated AVIF and video
//! animated wallpapers through a unified interface. It automatically selects
//! the appropriate backend:
//! - AVIF files: CPU-decoded frames cached in memory
//! - Video files: GStreamer-based hardware-accelerated playback

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use image::DynamicImage;
use tracing::{debug, info};

use super::types::{
    AnimatedFrame, AnimatedSource, DEFAULT_FRAME_DURATION, MIN_FRAME_DURATION, VideoFrameInfo,
};
use super::video_player::VideoPlayer;

/// Internal source holding actual player/frame data.
enum PlayerSource {
    /// AVIF frames loaded into memory.
    Avif(Vec<AnimatedFrame>),
    /// Video player instance.
    Video(VideoPlayer),
}

/// Unified player for animated AVIF and video animated wallpapers.
///
/// Automatically dispatches to the appropriate backend based on file type.
/// For AVIFs, frames are decoded into memory and cycled.
/// For videos, GStreamer handles decoding with hardware acceleration.
pub struct AnimatedPlayer {
    /// The animation source.
    source: PlayerSource,
    /// Source file path.
    source_path: PathBuf,
    /// Current frame index (for AVIF).
    current_index: usize,
}

impl AnimatedPlayer {
    /// Create a new animated player from an AnimatedSource.
    ///
    /// Automatically detects whether the file is an AVIF or video and initializes
    /// the appropriate backend.
    pub fn new(
        source: AnimatedSource,
        target_width: u32,
        target_height: u32,
    ) -> eyre::Result<Self> {
        let path = source.path();
        info!(path = %path.display(), width = target_width, height = target_height, "Loading animated wallpaper");

        let player_source = match &source {
            AnimatedSource::Avif(p) => {
                debug!(path = %p.display(), "Loading as animated AVIF");
                let frames = Self::load_avif_frames(p)?;
                info!(frames = frames.len(), "Loaded AVIF frames");
                PlayerSource::Avif(frames)
            }
            AnimatedSource::Video(p) => {
                debug!(path = %p.display(), "Loading as video");
                let player = VideoPlayer::new(p, target_width, target_height)?;
                // Start playback immediately
                player.play()?;
                PlayerSource::Video(player)
            }
        };

        Ok(Self {
            source: player_source,
            source_path: path.to_path_buf(),
            current_index: 0,
        })
    }

    /// Load animated AVIF (AVIS) frames into memory using libavif.
    fn load_avif_frames(path: &Path) -> eyre::Result<Vec<AnimatedFrame>> {
        use std::ffi::CString;

        use libavif_sys::*;

        let path_str = path
            .to_str()
            .ok_or_else(|| eyre::eyre!("Invalid path encoding"))?;
        let c_path = CString::new(path_str)?;

        let mut frames = Vec::new();

        unsafe {
            // Create decoder
            let decoder = avifDecoderCreate();
            if decoder.is_null() {
                return Err(eyre::eyre!("Failed to create AVIF decoder"));
            }

            // Ensure we clean up on exit
            struct DecoderGuard(*mut avifDecoder);
            impl Drop for DecoderGuard {
                fn drop(&mut self) {
                    unsafe { avifDecoderDestroy(self.0) };
                }
            }
            let _guard = DecoderGuard(decoder);

            // Set IO from file
            let result = avifDecoderSetIOFile(decoder, c_path.as_ptr());
            if result != AVIF_RESULT_OK {
                return Err(eyre::eyre!("Failed to set AVIF IO: {}", result));
            }

            // Parse the file
            let result = avifDecoderParse(decoder);
            if result != AVIF_RESULT_OK {
                return Err(eyre::eyre!("Failed to parse AVIF: {}", result));
            }

            let image_count = (*decoder).imageCount;
            tracing::debug!(image_count, "AVIF has frames");

            // Decode each frame
            for frame_idx in 0..image_count {
                let result = avifDecoderNextImage(decoder);
                if result != AVIF_RESULT_OK {
                    if result == AVIF_RESULT_NO_IMAGES_REMAINING {
                        break;
                    }
                    return Err(eyre::eyre!(
                        "Failed to decode AVIF frame {}: {}",
                        frame_idx,
                        result
                    ));
                }

                let avif_image = (*decoder).image;
                if avif_image.is_null() {
                    return Err(eyre::eyre!("Null image pointer for frame {}", frame_idx));
                }

                let width = (*avif_image).width;
                let height = (*avif_image).height;

                // Create RGB image for conversion
                let mut rgb: avifRGBImage = std::mem::zeroed();
                avifRGBImageSetDefaults(&mut rgb, avif_image);
                rgb.format = AVIF_RGB_FORMAT_RGBA;
                rgb.depth = 8;

                // Allocate pixel buffer
                avifRGBImageAllocatePixels(&mut rgb);

                struct RgbGuard(*mut avifRGBImage);
                impl Drop for RgbGuard {
                    fn drop(&mut self) {
                        unsafe { avifRGBImageFreePixels(self.0) };
                    }
                }
                let _rgb_guard = RgbGuard(&mut rgb);

                // Convert YUV to RGB
                let result = avifImageYUVToRGB(avif_image, &mut rgb);
                if result != AVIF_RESULT_OK {
                    return Err(eyre::eyre!(
                        "Failed to convert AVIF frame {} to RGB: {}",
                        frame_idx,
                        result
                    ));
                }

                // Copy pixel data
                let pixel_count = (width * height * 4) as usize;
                let pixels = std::slice::from_raw_parts(rgb.pixels, pixel_count);
                let rgba_data: Vec<u8> = pixels.to_vec();

                // Create image
                let rgba_image =
                    image::RgbaImage::from_raw(width, height, rgba_data).ok_or_else(|| {
                        eyre::eyre!("Failed to create image from AVIF frame {}", frame_idx)
                    })?;

                // Get frame duration
                // imageTiming.duration is in seconds (f64)
                let duration_secs = (*decoder).imageTiming.duration;
                let duration = if duration_secs > 0.0 {
                    Duration::from_secs_f64(duration_secs)
                } else {
                    // Default to 100ms if no timing info
                    Duration::from_millis(100)
                };
                let duration = duration.max(MIN_FRAME_DURATION);

                tracing::debug!(
                    frame = frame_idx,
                    width,
                    height,
                    duration_ms = duration.as_millis(),
                    "AVIF frame loaded"
                );

                frames.push(AnimatedFrame {
                    image: DynamicImage::ImageRgba8(rgba_image),
                    duration,
                    pts: None,
                });
            }
        }

        if frames.is_empty() {
            return Err(eyre::eyre!("No frames found in AVIF"));
        }

        Ok(frames)
    }

    /// Stop playback.
    pub fn stop(&mut self) -> eyre::Result<()> {
        if let PlayerSource::Video(player) = &self.source {
            player.stop()?;
        }
        Ok(())
    }

    /// Get the current frame.
    #[must_use]
    pub fn current_frame(&self) -> Option<AnimatedFrame> {
        match &self.source {
            PlayerSource::Avif(frames) => frames.get(self.current_index).cloned(),
            PlayerSource::Video(player) => player.current_frame(),
        }
    }

    /// Get the current frame index.
    #[must_use]
    pub fn current_frame_index(&self) -> usize {
        self.current_index
    }

    /// Advance to the next frame (for AVIF playback).
    ///
    /// Returns `true` if the animation should continue, `false` to stop.
    /// AVIFs always loop, so they always return `true` (unless empty).
    pub fn advance(&mut self) -> bool {
        match &mut self.source {
            PlayerSource::Avif(frames) => {
                if frames.is_empty() {
                    return false;
                }
                self.current_index = (self.current_index + 1) % frames.len();
                // AVIFs always loop - always return true
                true
            }
            PlayerSource::Video(player) => {
                // Check for EOS and handle looping
                !player.check_eos()
            }
        }
    }

    /// Get the duration of the current frame.
    #[must_use]
    pub fn current_frame_duration(&self) -> Duration {
        match &self.source {
            PlayerSource::Avif(frames) => frames
                .get(self.current_index)
                .map(|f| f.duration)
                .unwrap_or(DEFAULT_FRAME_DURATION),
            PlayerSource::Video(player) => player.frame_duration(),
        }
    }

    /// Get the duration of the current frame (alias for current_frame_duration).
    #[must_use]
    pub fn current_duration(&self) -> Duration {
        self.current_frame_duration()
    }

    /// Check if this is a video source.
    #[must_use]
    pub fn is_video(&self) -> bool {
        matches!(self.source, PlayerSource::Video(_))
    }

    /// Get video dimensions.
    #[must_use]
    pub fn video_dimensions(&self) -> Option<(u32, u32)> {
        match &self.source {
            PlayerSource::Video(player) => player.video_dimensions(),
            PlayerSource::Avif(_) => None,
        }
    }

    /// Pull a frame and write it directly to a destination buffer.
    pub fn pull_frame_to_buffer(&self, dest: &mut [u8]) -> Option<VideoFrameInfo> {
        match &self.source {
            PlayerSource::Video(player) => player.pull_frame_to_buffer(dest),
            PlayerSource::Avif(_) => None,
        }
    }

    /// Pull the last cached frame.
    pub fn pull_cached_frame(&self, dest: &mut [u8]) -> Option<VideoFrameInfo> {
        match &self.source {
            PlayerSource::Video(player) => player.pull_cached_frame(dest),
            PlayerSource::Avif(_) => None,
        }
    }

    /// Try to get a DMA-BUF frame for zero-copy rendering.
    #[must_use]
    pub fn try_get_dmabuf_frame(&self) -> Option<crate::dmabuf::DmaBufBuffer> {
        match &self.source {
            PlayerSource::Video(player) => player.try_get_dmabuf_frame(),
            PlayerSource::Avif(_) => None,
        }
    }

    /// Process GStreamer messages and check for EOS.
    /// Returns true if video ended (EOS reached).
    pub fn process_messages(&mut self) -> bool {
        match &mut self.source {
            PlayerSource::Video(player) => player.check_eos(),
            PlayerSource::Avif(_) => false,
        }
    }
}

impl std::fmt::Debug for AnimatedPlayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnimatedPlayer")
            .field("source_path", &self.source_path)
            .field(
                "source_type",
                &match &self.source {
                    PlayerSource::Avif(_) => "AVIF",
                    PlayerSource::Video(_) => "Video",
                },
            )
            .field("current_index", &self.current_index)
            .finish()
    }
}
