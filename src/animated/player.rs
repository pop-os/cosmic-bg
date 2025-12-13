// SPDX-License-Identifier: MPL-2.0

//! Unified animated wallpaper player.
//!
//! This module provides [`AnimatedPlayer`], which handles both GIF and video
//! animated wallpapers through a unified interface. It automatically selects
//! the appropriate backend:
//! - GIF files: CPU-decoded frames cached in memory
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
    /// GIF frames loaded into memory.
    Gif(Vec<AnimatedFrame>),
    /// Video player instance.
    Video(VideoPlayer),
}

/// Unified player for GIF and video animated wallpapers.
///
/// Automatically dispatches to the appropriate backend based on file type.
/// For GIFs, frames are decoded into memory and cycled.
/// For videos, GStreamer handles decoding with hardware acceleration.
pub struct AnimatedPlayer {
    /// The animation source.
    source: PlayerSource,
    /// Source file path.
    source_path: PathBuf,
    /// Current frame index (for GIF).
    current_index: usize,
}

impl AnimatedPlayer {
    /// Create a new animated player from an AnimatedSource.
    ///
    /// Automatically detects whether the file is a GIF or video and initializes
    /// the appropriate backend.
    pub fn new(
        source: AnimatedSource,
        target_width: u32,
        target_height: u32,
    ) -> eyre::Result<Self> {
        let path = source.path();
        info!(path = %path.display(), width = target_width, height = target_height, "Loading animated wallpaper");

        let player_source = match &source {
            AnimatedSource::Gif(p) => {
                debug!(path = %p.display(), "Loading as GIF");
                let frames = Self::load_gif_frames(p)?;
                info!(frames = frames.len(), "Loaded GIF frames");
                PlayerSource::Gif(frames)
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

    /// Load GIF frames into memory.
    fn load_gif_frames(path: &Path) -> eyre::Result<Vec<AnimatedFrame>> {
        use std::fs::File;

        let file = File::open(path)?;
        let decoder = gif::DecodeOptions::new();
        let mut reader = decoder.read_info(file)?;

        let mut frames = Vec::new();
        let global_palette = reader.palette().ok().map(|p| p.to_vec());

        // Get canvas dimensions before the loop to avoid borrow conflicts
        let canvas_width = reader.width() as u32;
        let canvas_height = reader.height() as u32;
        let mut canvas = image::RgbaImage::new(canvas_width, canvas_height);

        while let Some(gif_frame) = reader.read_next_frame()? {
            let frame_width = gif_frame.width as u32;
            let frame_height = gif_frame.height as u32;
            let frame_x = gif_frame.left as u32;
            let frame_y = gif_frame.top as u32;

            let palette = gif_frame
                .palette
                .as_ref()
                .or(global_palette.as_ref())
                .ok_or_else(|| eyre::eyre!("No palette found for GIF frame"))?;

            let transparent_idx = gif_frame.transparent;

            for (i, &pixel_idx) in gif_frame.buffer.iter().enumerate() {
                if Some(pixel_idx) == transparent_idx {
                    continue;
                }
                let x = (i as u32 % frame_width) + frame_x;
                let y = (i as u32 / frame_width) + frame_y;
                if x < canvas_width && y < canvas_height {
                    let base = pixel_idx as usize * 3;
                    if base + 2 < palette.len() {
                        let rgba =
                            image::Rgba([palette[base], palette[base + 1], palette[base + 2], 255]);
                        canvas.put_pixel(x, y, rgba);
                    }
                }
            }

            // GIF delay is in centiseconds (1/100th of a second)
            // delay=0 means "as fast as possible" - we use 10cs (100ms) as a reasonable default
            // delay=2 means 20ms, delay=10 means 100ms, etc.
            let delay = if gif_frame.delay == 0 {
                10 // 100ms default for unspecified delay
            } else {
                gif_frame.delay as u64
            };
            let duration = Duration::from_millis(delay * 10).max(MIN_FRAME_DURATION);

            tracing::debug!(
                frame = frames.len(),
                delay_cs = gif_frame.delay,
                duration_ms = duration.as_millis(),
                "GIF frame loaded"
            );

            frames.push(AnimatedFrame {
                image: DynamicImage::ImageRgba8(canvas.clone()),
                duration,
                pts: None,
            });

            match gif_frame.dispose {
                gif::DisposalMethod::Background => {
                    for y in frame_y..frame_y + frame_height {
                        for x in frame_x..frame_x + frame_width {
                            if x < canvas_width && y < canvas_height {
                                canvas.put_pixel(x, y, image::Rgba([0, 0, 0, 0]));
                            }
                        }
                    }
                }
                gif::DisposalMethod::Previous => {}
                _ => {}
            }
        }

        if frames.is_empty() {
            return Err(eyre::eyre!("No frames found in GIF"));
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
            PlayerSource::Gif(frames) => frames.get(self.current_index).cloned(),
            PlayerSource::Video(player) => player.current_frame(),
        }
    }

    /// Get the current frame index.
    #[must_use]
    pub fn current_frame_index(&self) -> usize {
        self.current_index
    }

    /// Advance to the next frame (for GIF playback).
    ///
    /// Returns `true` if the animation should continue, `false` to stop.
    /// GIFs always loop, so they always return `true` (unless empty).
    pub fn advance(&mut self) -> bool {
        match &mut self.source {
            PlayerSource::Gif(frames) => {
                if frames.is_empty() {
                    return false;
                }
                self.current_index = (self.current_index + 1) % frames.len();
                // GIFs always loop - always return true
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
            PlayerSource::Gif(frames) => frames
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
            PlayerSource::Gif(_) => None,
        }
    }

    /// Pull a frame and write it directly to a destination buffer.
    pub fn pull_frame_to_buffer(&self, dest: &mut [u8]) -> Option<VideoFrameInfo> {
        match &self.source {
            PlayerSource::Video(player) => player.pull_frame_to_buffer(dest),
            PlayerSource::Gif(_) => None,
        }
    }

    /// Pull the last cached frame.
    pub fn pull_cached_frame(&self, dest: &mut [u8]) -> Option<VideoFrameInfo> {
        match &self.source {
            PlayerSource::Video(player) => player.pull_cached_frame(dest),
            PlayerSource::Gif(_) => None,
        }
    }

    /// Try to get a DMA-BUF frame for zero-copy rendering.
    #[must_use]
    pub fn try_get_dmabuf_frame(&self) -> Option<crate::dmabuf::DmaBufBuffer> {
        match &self.source {
            PlayerSource::Video(player) => player.try_get_dmabuf_frame(),
            PlayerSource::Gif(_) => None,
        }
    }

    /// Process GStreamer messages and check for EOS.
    /// Returns true if video ended (EOS reached).
    pub fn process_messages(&mut self) -> bool {
        match &mut self.source {
            PlayerSource::Video(player) => player.check_eos(),
            PlayerSource::Gif(_) => false,
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
                    PlayerSource::Gif(_) => "GIF",
                    PlayerSource::Video(_) => "Video",
                },
            )
            .field("current_index", &self.current_index)
            .finish()
    }
}
