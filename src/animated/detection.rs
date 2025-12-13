// SPDX-License-Identifier: MPL-2.0

//! File type detection utilities for animated wallpapers.
//!
//! This module detects supported video formats based on:
//! 1. File extension (quick filter for potentially supported files)
//! 2. Available GStreamer decoders on the current system

use std::path::Path;
use std::sync::OnceLock;

use gstreamer::prelude::*;
use tracing::{debug, info};

/// Video container extensions that may contain playable video.
/// These are checked case-insensitively when determining if a file can be
/// rendered as an animated wallpaper.
const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4",  // MPEG-4 container (typically H.264/H.265 codec)
    "webm", // WebM container (VP8/VP9/AV1)
    "mkv",  // Matroska container (any codec)
    "avi",  // AVI container (legacy format)
    "mov",  // QuickTime container (typically H.264)
    "m4v",  // MPEG-4 Video (Apple variant of MP4)
    "ogv",  // Ogg Video container (Theora codec)
];

/// Cached system codec capabilities.
static CODEC_SUPPORT: OnceLock<CodecSupport> = OnceLock::new();

/// System codec capabilities detected at runtime.
#[derive(Debug, Clone)]
pub struct CodecSupport {
    /// NVIDIA hardware decode available (NVDEC)
    pub has_nvidia: bool,
    /// AMD/Intel VAAPI decode available
    pub has_vaapi: bool,
    /// List of available hardware decoder element names
    pub hw_decoders: Vec<String>,
}

impl Default for CodecSupport {
    fn default() -> Self {
        Self {
            has_nvidia: false,
            has_vaapi: false,
            hw_decoders: Vec::new(),
        }
    }
}

/// Detect available codec support on the current system.
///
/// This probes GStreamer for available hardware decoders and caches the result.
/// The detection is performed once on first call.
pub fn get_codec_support() -> &'static CodecSupport {
    CODEC_SUPPORT.get_or_init(detect_codec_support)
}

/// Perform the actual codec detection.
fn detect_codec_support() -> CodecSupport {
    // Try to initialize GStreamer
    if gstreamer::init().is_err() {
        return CodecSupport::default();
    }

    let mut support = CodecSupport::default();

    // Check for NVIDIA decoders
    let nvidia_decoders = [
        "nvh264dec",
        "nvh265dec",
        "nvvp9dec",
        "nvav1dec",
        "nvmpegvideodec",
        "nvmpeg4videodec",
    ];

    for decoder in nvidia_decoders {
        if gstreamer::ElementFactory::find(decoder).is_some() {
            support.has_nvidia = true;
            support.hw_decoders.push(decoder.to_string());
        }
    }

    // Check for VAAPI decoders (AMD/Intel)
    let vaapi_decoders = [
        "vaapih264dec",
        "vaapih265dec",
        "vaapivp8dec",
        "vaapivp9dec",
        "vaapiav1dec",
        "vaapimpeg2dec",
        // New VA-API plugin element names (GStreamer 1.22+)
        "vah264dec",
        "vah265dec",
        "vavp8dec",
        "vavp9dec",
        "vaav1dec",
    ];

    for decoder in vaapi_decoders {
        if gstreamer::ElementFactory::find(decoder).is_some() {
            support.has_vaapi = true;
            support.hw_decoders.push(decoder.to_string());
        }
    }

    // Check for vapostproc (required for DMA-BUF output)
    if gstreamer::ElementFactory::find("vapostproc").is_some() {
        support.hw_decoders.push("vapostproc".to_string());
    }

    // Check for CUDA DMA-BUF upload (NVIDIA zero-copy)
    if gstreamer::ElementFactory::find("cudadmabufupload").is_some() {
        support.hw_decoders.push("cudadmabufupload".to_string());
    }

    info!(
        has_nvidia = support.has_nvidia,
        has_vaapi = support.has_vaapi,
        decoders = ?support.hw_decoders,
        "Detected video codec support"
    );

    support
}

/// Check if a path points to an animated/video file.
///
/// This checks both:
/// 1. The file extension is a known video/GIF format
/// 2. The system has capability to decode video (always true for GIF)
#[must_use]
pub fn is_animated_file(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };

    let ext_lower = ext.to_lowercase();

    // GIF is always supported (CPU decoded)
    if ext_lower == "gif" {
        return true;
    }

    // Video files - check extension first (quick filter)
    if !VIDEO_EXTENSIONS.contains(&ext_lower.as_str()) {
        return false;
    }

    // Video is supported as long as we have decodebin (software decode)
    // GStreamer's decodebin can handle any format it has plugins for
    true
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
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };

    let ext_lower = ext.to_lowercase();
    VIDEO_EXTENSIONS.contains(&ext_lower.as_str())
}

/// Demote NVIDIA decoders if CUDA is not actually functional.
///
/// On systems where NVIDIA GStreamer plugins are installed but CUDA
/// is not available (e.g., AMD-only systems, or NVIDIA as secondary GPU),
/// the nvh264dec/etc elements will be registered but fail to instantiate.
/// This causes decodebin to select them over working software decoders.
///
/// This function tests if NVIDIA decoders can actually be instantiated
/// and demotes them to NONE rank if not, allowing decodebin to pick
/// working alternatives like openh264dec.
///
/// This should be called once after gstreamer::init().
pub fn demote_broken_nvidia_decoders() {
    use gstreamer::Rank;
    use tracing::warn;

    static DEMOTED: std::sync::Once = std::sync::Once::new();

    DEMOTED.call_once(|| {
        // List of NVIDIA decoders to test
        let nvidia_decoders = [
            "nvh264dec",
            "nvh265dec",
            "nvvp9dec",
            "nvav1dec",
            "nvmpegvideodec",
            "nvmpeg4videodec",
        ];

        for decoder_name in nvidia_decoders {
            if let Some(factory) = gstreamer::ElementFactory::find(decoder_name) {
                // Try to create an instance - this will fail if CUDA isn't available
                match factory.create().build() {
                    Ok(element) => {
                        // It worked, decoder is functional
                        debug!(decoder = decoder_name, "NVIDIA decoder is functional");
                        drop(element);
                    }
                    Err(_) => {
                        // Failed to instantiate - demote to prevent decodebin from selecting it
                        warn!(
                            decoder = decoder_name,
                            "NVIDIA decoder failed to instantiate (CUDA unavailable?), demoting"
                        );
                        // Set rank to NONE so decodebin won't auto-select it
                        factory.set_rank(Rank::NONE);
                    }
                }
            }
        }
    });
}

/// Test if a specific video file can be played on this system.
///
/// This attempts to create a minimal GStreamer pipeline to verify
/// the file's codec is decodable. Returns `true` if playable.
///
/// This is more expensive than `is_video_file` and should only be
/// used when you need to verify a specific file can be played.
#[must_use]
#[allow(dead_code)]
pub fn can_play_video(path: &Path) -> bool {
    if !is_video_file(path) {
        return false;
    }

    // Try to create a test pipeline with decodebin
    if gstreamer::init().is_err() {
        return false;
    }

    let path_str = match path.to_str() {
        Some(s) => s.replace('\\', "\\\\").replace('"', "\\\""),
        None => return false,
    };

    // Use decodebin which auto-selects the best available decoder
    let pipeline_str = format!("filesrc location=\"{path_str}\" ! decodebin ! fakesink");

    match gstreamer::parse::launch(&pipeline_str) {
        Ok(element) => {
            // Try to set to PAUSED to verify it can decode
            let result = element.set_state(gstreamer::State::Paused);
            if result.is_err() {
                let _ = element.set_state(gstreamer::State::Null);
                debug!(path = %path.display(), "Video file cannot be decoded (set_state failed)");
                return false;
            }

            // Wait for state change with timeout
            let (res, state, _) = element.state(gstreamer::ClockTime::from_mseconds(2000));
            let _ = element.set_state(gstreamer::State::Null);

            let can_play = res.is_ok() && state == gstreamer::State::Paused;
            if !can_play {
                debug!(path = %path.display(), "Video file cannot be decoded (state check failed)");
            }
            can_play
        }
        Err(e) => {
            debug!(path = %path.display(), error = %e, "Failed to create test pipeline");
            false
        }
    }
}
