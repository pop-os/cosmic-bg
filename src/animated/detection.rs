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
/// Note: AVIF is handled specially - only animated AVIF (AVIS) is treated as video
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
#[derive(Debug, Clone, Default)]
pub struct CodecSupport {
    /// NVIDIA hardware decode available (NVDEC)
    pub has_nvidia: bool,
    /// AMD/Intel VAAPI decode available
    pub has_vaapi: bool,
    /// List of available hardware decoder element names
    pub hw_decoders: Vec<String>,
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
/// 1. The file extension is a known video/GIF/AVIF format
/// 2. The system has capability to decode it
///
/// Supported formats:
/// - GIF: CPU decoded via the `gif` crate
/// - Animated AVIF (AVIS): CPU decoded via libavif
/// - Video files: Hardware-accelerated via GStreamer
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

    // AVIF is treated as animated if it's an AVIS (AVIF Image Sequence)
    // Animated AVIF is CPU-decoded using libavif, similar to GIF
    if ext_lower == "avif" {
        if is_animated_avif(path) {
            debug!(path = %path.display(), "Animated AVIF detected - will use CPU decoding");
            return true;
        }
        return false;
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

/// Check if a path points to a video file (non-GIF, non-AVIF animated).
#[must_use]
pub fn is_video_file(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };

    let ext_lower = ext.to_lowercase();

    // AVIF is handled separately as its own source type
    if ext_lower == "avif" {
        return false;
    }

    VIDEO_EXTENSIONS.contains(&ext_lower.as_str())
}

/// Check if an AVIF file is animated (AVIF Image Sequence).
///
/// AVIF files use the ISO Base Media File Format (ISOBMFF). The file starts
/// with an 'ftyp' box containing a major brand:
/// - `avif` = static AVIF image
/// - `avis` = AVIF Image Sequence (animated)
///
/// This function reads the file header to detect animated AVIF files.
#[must_use]
pub fn is_animated_avif(path: &Path) -> bool {
    use std::fs::File;
    use std::io::Read;

    let Ok(mut file) = File::open(path) else {
        return false;
    };

    // ISOBMFF structure: [4 bytes size][4 bytes type][4 bytes major_brand]...
    // We need to read the ftyp box and check the major brand
    let mut header = [0u8; 12];
    if file.read_exact(&mut header).is_err() {
        return false;
    }

    // Check if this is an ftyp box
    let box_type = &header[4..8];
    if box_type != b"ftyp" {
        return false;
    }

    // Check the major brand (bytes 8-12)
    let major_brand = &header[8..12];

    // 'avis' indicates AVIF Image Sequence (animated)
    if major_brand == b"avis" {
        debug!(path = %path.display(), "Detected animated AVIF (AVIS)");
        return true;
    }

    // Also check compatible_brands for 'avis' in case major_brand is different
    // Read more of the ftyp box to check compatible brands
    let box_size = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
    if box_size > 12 && box_size <= 256 {
        let remaining = box_size - 12;
        let mut brands_data = vec![0u8; remaining];
        if file.read_exact(&mut brands_data).is_ok() {
            // Skip minor_version (4 bytes), then check compatible_brands
            if remaining >= 4 {
                let compatible_brands = &brands_data[4..];
                // Each brand is 4 bytes
                for chunk in compatible_brands.chunks_exact(4) {
                    if chunk == b"avis" {
                        debug!(path = %path.display(), "Detected animated AVIF (AVIS in compatible_brands)");
                        return true;
                    }
                }
            }
        }
    }

    false
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
