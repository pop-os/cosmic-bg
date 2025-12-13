// SPDX-License-Identifier: MPL-2.0

//! Automatic video format conversion for optimal hardware decode support.
//!
//! This module provides transparent format conversion to ensure video wallpapers
//! work efficiently on all GPU vendors:
//!
//! - **NVIDIA**: Most formats supported natively (H.264, H.265, VP9, AV1)
//! - **AMD (Mesa/VAAPI)**: VP9 and AV1 work best; H.264/H.265 may require non-free firmware
//! - **Intel (VAAPI)**: Most formats supported
//!
//! When a video file is set as a wallpaper, this module:
//! 1. Checks if the format needs conversion for optimal hardware decode
//! 2. Converts to VP9/WebM if needed (using FFmpeg)
//! 3. Caches the converted file in `~/.local/share/cosmic-bg/converted/`
//! 4. Uses the cached file for subsequent runs
//!
//! # Conversion Strategy
//!
//! | Input Format | NVIDIA Action | AMD/Intel Action |
//! |--------------|---------------|------------------|
//! | VP9/WebM     | Use as-is     | Use as-is        |
//! | AV1          | Use as-is     | Use as-is        |
//! | H.264/MP4    | Use as-is     | Convert to VP9   |
//! | H.265/HEVC   | Use as-is     | Convert to VP9   |
//! | MPEG4/AVI    | Convert to VP9| Convert to VP9   |
//! | GIF          | Use as-is     | Use as-is        |

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use tracing::{debug, error, info, warn};

/// Cache directory for converted videos.
const CACHE_DIR: &str = "cosmic-bg/converted";

/// Target codec for conversion (VP9 is well-supported across all vendors).
const TARGET_CODEC: &str = "libvpx-vp9";

/// Target container format.
const TARGET_EXTENSION: &str = "webm";

/// Video codecs that are universally hardware-accelerated.
const UNIVERSAL_HW_CODECS: &[&str] = &["vp9", "vp8", "av1"];

/// Video codecs that can be converted (FFmpeg can decode these reliably).
/// Note: H.264/H.265 are excluded because Fedora's FFmpeg uses libopenh264
/// which cannot decode High Profile H.264 properly.
const CONVERTIBLE_CODECS: &[&str] = &["mpeg4", "mpeg2video", "msmpeg4", "mpeg1video", "mjpeg"];

/// Video codecs that ideally need conversion but may not be convertible.
/// These will be converted using GStreamer for decode + FFmpeg for encode.
const PROBLEMATIC_CODECS: &[&str] = &["h264", "hevc", "h265"];

/// Check if FFmpeg is available on the system.
#[must_use]
pub fn is_ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Get the cache directory for converted videos.
fn get_cache_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join(CACHE_DIR))
}

/// Generate a cache key for a source file based on its path and modification time.
fn cache_key(source: &Path) -> Option<String> {
    let metadata = source.metadata().ok()?;
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();

    let filename = source.file_stem()?.to_str()?;
    let hash = {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        source.to_string_lossy().hash(&mut hasher);
        modified.hash(&mut hasher);
        hasher.finish()
    };

    Some(format!("{filename}_{hash:016x}"))
}

/// Get the cached file path for a source video.
#[must_use]
pub fn get_cached_path(source: &Path) -> Option<PathBuf> {
    let cache_dir = get_cache_dir()?;
    let key = cache_key(source)?;
    Some(cache_dir.join(format!("{key}.{TARGET_EXTENSION}")))
}

/// Probe the video codec using ffprobe.
fn probe_video_codec(path: &Path) -> Option<String> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output()
        .ok()?;

    if output.status.success() {
        let codec = String::from_utf8_lossy(&output.stdout)
            .trim()
            .to_lowercase();
        Some(codec)
    } else {
        None
    }
}

/// Check if the system has NVIDIA GPU (and thus doesn't need conversion).
fn has_nvidia_gpu() -> bool {
    // Check for nvidia-smi
    if Command::new("nvidia-smi")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return true;
    }

    // Check for NVIDIA in lspci
    Command::new("lspci")
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .to_lowercase()
                .contains("nvidia")
        })
        .unwrap_or(false)
}

/// Determine if a video file needs conversion for optimal playback.
///
/// Returns `true` if the file should be converted to VP9 for better
/// hardware decode compatibility.
#[must_use]
pub fn needs_conversion(path: &Path) -> bool {
    // GIFs don't need conversion (they're decoded differently)
    if path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase() == "gif")
        .unwrap_or(false)
    {
        return false;
    }

    // WebM files are typically VP8/VP9/AV1 - no conversion needed
    if path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase() == "webm")
        .unwrap_or(false)
    {
        return false;
    }

    // NVIDIA GPUs can decode most formats in hardware
    if has_nvidia_gpu() {
        debug!("NVIDIA GPU detected - skipping conversion");
        return false;
    }

    // Probe the actual codec
    let codec = match probe_video_codec(path) {
        Some(c) => c,
        None => {
            warn!(path = %path.display(), "Could not probe video codec, assuming conversion needed");
            return true;
        }
    };

    debug!(path = %path.display(), codec = %codec, "Probed video codec");

    // Check if codec is universally supported
    if UNIVERSAL_HW_CODECS.iter().any(|c| codec.contains(c)) {
        debug!(codec = %codec, "Codec is universally hardware-accelerated");
        return false;
    }

    // Check if codec can be converted reliably (MPEG4, MPEG2, etc.)
    if CONVERTIBLE_CODECS.iter().any(|c| codec.contains(c)) {
        info!(
            codec = %codec,
            path = %path.display(),
            "Codec will be converted to VP9 for AMD/Intel hardware decode"
        );
        return true;
    }

    // Check for problematic codecs (H.264/HEVC) - these need GStreamer-based conversion
    // because FFmpeg's libopenh264 can't decode High Profile H.264 properly,
    // but GStreamer can use software decode or VAAPI if available
    if PROBLEMATIC_CODECS.iter().any(|c| codec.contains(c)) {
        info!(
            codec = %codec,
            path = %path.display(),
            "H.264/HEVC video detected - will convert using GStreamer decode pipeline"
        );
        return true;
    }

    // Unknown codec - try to play as-is
    debug!(codec = %codec, "Unknown codec, will attempt direct playback");
    false
}

/// Check if VAAPI hardware acceleration is available in FFmpeg.
fn has_vaapi_hwaccel() -> bool {
    Command::new("ffmpeg")
        .args(["-hwaccels"])
        .output()
        .map(|o| {
            o.status.success()
                && String::from_utf8_lossy(&o.stdout)
                    .to_lowercase()
                    .contains("vaapi")
        })
        .unwrap_or(false)
}

/// Check if gst-launch-1.0 is available for GStreamer-based conversion.
fn has_gstreamer() -> bool {
    Command::new("gst-launch-1.0")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Convert H.264/HEVC video using GStreamer for decode and FFmpeg for VP9 encode.
///
/// This bypasses FFmpeg's H.264 decoder limitations by using GStreamer's decodebin
/// which can use software decode or VAAPI hardware decode if available.
fn convert_with_gstreamer(source: &Path, target: &Path) -> Result<(), String> {
    use std::process::Stdio;

    let source_str = source.to_str().ok_or("Invalid source path")?;
    let target_str = target.to_str().ok_or("Invalid target path")?;

    // Create a temp file for intermediate raw video
    let temp_dir = std::env::temp_dir();
    let temp_raw = temp_dir.join(format!("cosmic_bg_convert_{}.y4m", std::process::id()));
    let temp_raw_str = temp_raw.to_str().ok_or("Invalid temp path")?;

    debug!(
        source = %source.display(),
        temp = %temp_raw.display(),
        "Starting GStreamer decode pipeline"
    );

    // Step 1: Use GStreamer to decode video to Y4M format (universally readable)
    // decodebin will automatically use VAAPI if available, or software decode
    let filesrc = format!("location={}", source_str);
    let filesink = format!("location={}", temp_raw_str);

    // Run GStreamer - pass pipeline elements as separate arguments
    let gst_output = Command::new("gst-launch-1.0")
        .args([
            "-e",
            "filesrc",
            &filesrc,
            "!",
            "decodebin",
            "!",
            "videoconvert",
            "!",
            "video/x-raw,format=I420",
            "!",
            "y4menc",
            "!",
            "filesink",
            &filesink,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("Failed to run GStreamer: {e}"))?;

    if !gst_output.status.success() {
        let stderr = String::from_utf8_lossy(&gst_output.stderr);
        // Clean up temp file
        let _ = std::fs::remove_file(&temp_raw);
        return Err(format!("GStreamer decode failed: {stderr}"));
    }

    // Check that temp file was created and has content
    let temp_size = std::fs::metadata(&temp_raw).map(|m| m.len()).unwrap_or(0);

    if temp_size == 0 {
        let _ = std::fs::remove_file(&temp_raw);
        return Err("GStreamer produced empty output".to_string());
    }

    info!(
        temp_size_mb = temp_size / (1024 * 1024),
        "GStreamer decode complete, starting FFmpeg encode"
    );

    // Step 2: Use FFmpeg to encode Y4M to VP9/WebM
    // Note: We preserve the original resolution for best quality.
    // The copy bottleneck is addressed by using sync=false and more buffers.

    // Build FFmpeg args
    let mut ffmpeg_args: Vec<&str> = vec!["-i", temp_raw_str];

    // Add encoding options
    ffmpeg_args.extend([
        "-c:v",
        TARGET_CODEC,
        "-crf",
        "23", // Good quality (lower = better, 23 is good for 4K)
        "-b:v",
        "0", // Let CRF control quality
        "-deadline",
        "good",
        "-cpu-used",
        "4", // Faster encoding
        "-row-mt",
        "1", // Multi-threaded row encoding
        "-threads",
        "0",   // Auto thread count
        "-an", // No audio
        "-y",  // Overwrite
        target_str,
    ]);

    // CRF 23 is a good balance between quality and file size for wallpapers
    let ffmpeg_output = Command::new("ffmpeg")
        .args(&ffmpeg_args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| {
            let _ = std::fs::remove_file(&temp_raw);
            format!("Failed to run FFmpeg: {e}")
        })?;

    // Clean up temp file
    let _ = std::fs::remove_file(&temp_raw);

    if !ffmpeg_output.status.success() {
        let stderr = String::from_utf8_lossy(&ffmpeg_output.stderr);
        return Err(format!("FFmpeg encoding failed: {stderr}"));
    }

    // Verify output file exists and has content
    if !target.exists() {
        return Err("Output file was not created".to_string());
    }

    let file_size = std::fs::metadata(target).map(|m| m.len()).unwrap_or(0);

    if file_size == 0 {
        let _ = std::fs::remove_file(target);
        return Err("Output file is empty".to_string());
    }

    info!(
        target = %target.display(),
        size_mb = file_size / (1024 * 1024),
        "GStreamer conversion complete"
    );

    Ok(())
}

/// Convert a video file to VP9/WebM for optimal hardware decode.
///
/// This function:
/// 1. Creates the cache directory if needed
/// 2. For H.264/HEVC: Uses GStreamer decode + FFmpeg VP9 encode (bypasses FFmpeg H.264 limitations)
/// 3. For other codecs: Uses FFmpeg directly with optional VAAPI hardware decode
/// 4. Returns the path to the converted file
///
/// # Arguments
/// * `source` - Path to the source video file
///
/// # Returns
/// * `Ok(PathBuf)` - Path to the converted (or cached) file
/// * `Err(String)` - Error message if conversion failed
pub fn convert_to_vp9(source: &Path) -> Result<PathBuf, String> {
    // Check if already cached
    let cached_path =
        get_cached_path(source).ok_or_else(|| "Failed to generate cache path".to_string())?;

    if cached_path.exists() {
        // Verify cached file is not empty
        let file_size = std::fs::metadata(&cached_path)
            .map(|m| m.len())
            .unwrap_or(0);

        if file_size > 0 {
            info!(
                source = %source.display(),
                cached = %cached_path.display(),
                "Using cached converted video"
            );
            return Ok(cached_path);
        } else {
            // Remove empty cached file
            let _ = std::fs::remove_file(&cached_path);
        }
    }

    // Check FFmpeg availability
    if !is_ffmpeg_available() {
        return Err("FFmpeg not found. Install ffmpeg for automatic video conversion.".to_string());
    }

    // Create cache directory
    let cache_dir = get_cache_dir().ok_or_else(|| "Failed to get cache directory".to_string())?;
    std::fs::create_dir_all(&cache_dir)
        .map_err(|e| format!("Failed to create cache directory: {e}"))?;

    // Probe codec to determine conversion strategy
    let codec = probe_video_codec(source).unwrap_or_default();
    let is_h264_hevc = PROBLEMATIC_CODECS.iter().any(|c| codec.contains(c));

    info!(
        source = %source.display(),
        target = %cached_path.display(),
        codec = %codec,
        use_gstreamer = is_h264_hevc,
        "Converting video to VP9 for hardware decode compatibility"
    );

    // For H.264/HEVC, use GStreamer-based conversion if available
    // This bypasses FFmpeg's H.264 decoder limitations on Fedora
    if is_h264_hevc && has_gstreamer() {
        info!("Using GStreamer decode + FFmpeg encode for H.264/HEVC");

        match convert_with_gstreamer(source, &cached_path) {
            Ok(()) => {
                info!(
                    source = %source.display(),
                    target = %cached_path.display(),
                    "GStreamer-based video conversion complete"
                );
                return Ok(cached_path);
            }
            Err(e) => {
                warn!(error = %e, "GStreamer conversion failed, trying FFmpeg fallback");
                // Fall through to FFmpeg conversion
            }
        }
    }

    // Standard FFmpeg conversion path
    let source_str = source.to_str().unwrap_or("");

    // Try VAAPI hardware decode first (works for some codecs on AMD/Intel)
    let use_vaapi = has_vaapi_hwaccel() && !has_nvidia_gpu() && !is_h264_hevc;

    let output = if use_vaapi {
        info!("Using VAAPI hardware decode for input video");
        // VAAPI path - preserve original resolution
        Command::new("ffmpeg")
            .args([
                "-hwaccel",
                "vaapi",
                "-hwaccel_device",
                "/dev/dri/renderD128",
                "-hwaccel_output_format",
                "vaapi",
                "-i",
                source_str,
                "-vf",
                "scale_vaapi=format=nv12",
                "-c:v",
                TARGET_CODEC,
                "-crf",
                "23",
                "-b:v",
                "0",
                "-deadline",
                "good",
                "-cpu-used",
                "4",
                "-row-mt",
                "1",
                "-an",
                "-y",
            ])
            .arg(&cached_path)
            .output()
            .map_err(|e| format!("Failed to run FFmpeg: {e}"))?
    } else {
        // Standard software decode path - preserve original resolution
        Command::new("ffmpeg")
            .args([
                "-i",
                source_str,
                "-c:v",
                TARGET_CODEC,
                "-crf",
                "23",
                "-b:v",
                "0",
                "-deadline",
                "good",
                "-cpu-used",
                "4",
                "-row-mt",
                "1",
                "-threads",
                "0",
                "-an",
                "-y",
            ])
            .arg(&cached_path)
            .output()
            .map_err(|e| format!("Failed to run FFmpeg: {e}"))?
    };

    // If VAAPI failed, try software decode as fallback
    if !output.status.success() && use_vaapi {
        warn!("VAAPI decode failed, trying software decode fallback");

        let fallback_output = Command::new("ffmpeg")
            .args([
                "-i",
                source_str,
                "-c:v",
                TARGET_CODEC,
                "-crf",
                "23",
                "-b:v",
                "0",
                "-deadline",
                "good",
                "-cpu-used",
                "4",
                "-row-mt",
                "1",
                "-an",
                "-y",
            ])
            .arg(&cached_path)
            .output()
            .map_err(|e| format!("Failed to run FFmpeg fallback: {e}"))?;

        if !fallback_output.status.success() {
            let stderr = String::from_utf8_lossy(&fallback_output.stderr);
            error!(stderr = %stderr, "FFmpeg conversion failed (both VAAPI and software)");
            return Err(format!("FFmpeg conversion failed: {stderr}"));
        }
    } else if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        error!(stderr = %stderr, "FFmpeg conversion failed");
        return Err(format!("FFmpeg conversion failed: {stderr}"));
    }

    // Verify output file
    let file_size = std::fs::metadata(&cached_path)
        .map(|m| m.len())
        .unwrap_or(0);

    if file_size == 0 {
        let _ = std::fs::remove_file(&cached_path);
        return Err("Conversion produced empty file".to_string());
    }

    info!(
        source = %source.display(),
        target = %cached_path.display(),
        size_mb = file_size / (1024 * 1024),
        "Video conversion complete"
    );

    Ok(cached_path)
}

/// Get the optimal video path for playback.
///
/// This is the main entry point for the conversion system. It:
/// 1. Checks if the video needs conversion
/// 2. Converts if necessary (or uses cached version)
/// 3. Returns the path to use for playback
///
/// # Arguments
/// * `source` - Original video file path
///
/// # Returns
/// The path to use for video playback (either original or converted)
pub fn get_optimal_video_path(source: &Path) -> PathBuf {
    // Check if conversion is needed
    if !needs_conversion(source) {
        debug!(path = %source.display(), "No conversion needed");
        return source.to_path_buf();
    }

    // Try to convert
    match convert_to_vp9(source) {
        Ok(converted) => {
            info!(
                original = %source.display(),
                converted = %converted.display(),
                "Using converted video for optimal playback"
            );
            converted
        }
        Err(e) => {
            warn!(
                path = %source.display(),
                error = %e,
                "Conversion failed, will attempt direct playback"
            );
            source.to_path_buf()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_key_generation() {
        let path = Path::new("/tmp/test.mp4");
        // This will fail if the file doesn't exist, which is fine for unit tests
        let _ = cache_key(path);
    }

    #[test]
    fn test_webm_no_conversion() {
        // WebM files should never need conversion (typically VP8/VP9/AV1)
        assert!(!needs_conversion(Path::new("/tmp/test.webm")));
        assert!(!needs_conversion(Path::new("/tmp/TEST.WEBM")));
        assert!(!needs_conversion(Path::new("/path/to/video.WebM")));
    }

    #[test]
    fn test_gif_no_conversion() {
        // GIF files should never need conversion (decoded differently)
        assert!(!needs_conversion(Path::new("/tmp/test.gif")));
        assert!(!needs_conversion(Path::new("/tmp/TEST.GIF")));
        assert!(!needs_conversion(Path::new("/path/to/animation.Gif")));
    }

    #[test]
    fn test_universal_hw_codecs() {
        // Test that universal HW codecs are recognized
        assert!(UNIVERSAL_HW_CODECS.contains(&"vp9"));
        assert!(UNIVERSAL_HW_CODECS.contains(&"vp8"));
        assert!(UNIVERSAL_HW_CODECS.contains(&"av1"));
    }

    #[test]
    fn test_convertible_codecs() {
        // Test that convertible codecs are recognized (can be reliably transcoded)
        assert!(CONVERTIBLE_CODECS.contains(&"mpeg4"));
        assert!(CONVERTIBLE_CODECS.contains(&"mpeg2video"));
        assert!(CONVERTIBLE_CODECS.contains(&"mjpeg"));
    }

    #[test]
    fn test_problematic_codecs() {
        // Test that problematic codecs are recognized (H.264/HEVC need special handling)
        assert!(PROBLEMATIC_CODECS.contains(&"h264"));
        assert!(PROBLEMATIC_CODECS.contains(&"hevc"));
        assert!(PROBLEMATIC_CODECS.contains(&"h265"));
    }

    #[test]
    fn test_cache_dir_generation() {
        // Cache directory should be under user's data dir
        let cache_dir = get_cache_dir();
        if let Some(dir) = cache_dir {
            assert!(dir.to_string_lossy().contains("cosmic-bg"));
            assert!(dir.to_string_lossy().contains("converted"));
        }
    }

    #[test]
    fn test_cached_path_generation() {
        // Cached path should have .webm extension
        let source = Path::new("/tmp/test_video.mp4");
        if let Some(cached) = get_cached_path(source) {
            assert!(cached.extension().and_then(|e| e.to_str()) == Some(TARGET_EXTENSION));
        }
    }

    #[test]
    fn test_target_codec_is_vp9() {
        // Ensure we're using VP9 as the target codec
        assert_eq!(TARGET_CODEC, "libvpx-vp9");
        assert_eq!(TARGET_EXTENSION, "webm");
    }

    #[test]
    fn test_is_ffmpeg_available_returns_bool() {
        // This just tests that the function runs without panic
        let _ = is_ffmpeg_available();
    }

    #[test]
    fn test_has_nvidia_gpu_returns_bool() {
        // This just tests that the function runs without panic
        let _ = has_nvidia_gpu();
    }

    #[test]
    fn test_get_optimal_video_path_returns_path() {
        // For a non-existent file, should return the original path
        let source = Path::new("/nonexistent/video.webm");
        let result = get_optimal_video_path(source);
        // WebM files should be returned as-is (no conversion needed)
        assert_eq!(result, source.to_path_buf());
    }
}
