// SPDX-License-Identifier: MPL-2.0

//! Unit tests for animated wallpaper functionality.

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::super::detection::{is_animated_avif, is_animated_file, is_video_file};

    #[test]
    fn test_is_video_file() {
        assert!(is_video_file(Path::new("test.mp4")));
        assert!(is_video_file(Path::new("test.MP4")));
        assert!(is_video_file(Path::new("test.webm")));
        assert!(is_video_file(Path::new("test.WEBM")));
        assert!(is_video_file(Path::new("test.mkv")));
        assert!(is_video_file(Path::new("test.m4v")));
        assert!(is_video_file(Path::new("test.mov")));
        assert!(is_video_file(Path::new("test.ogv")));
        // Note: AVIF detection requires reading the file, so non-existent files return false
        assert!(!is_video_file(Path::new("test.avif")));
        assert!(!is_video_file(Path::new("test.png")));
        assert!(!is_video_file(Path::new("test.jpg")));
    }

    #[test]
    fn test_is_animated_file() {
        // Videos should be animated
        assert!(is_animated_file(Path::new("test.mp4")));
        assert!(is_animated_file(Path::new("test.webm")));
        assert!(is_animated_file(Path::new("test.mkv")));
        // Note: AVIF detection requires reading the file, tested separately

        // Static images should not be animated
        assert!(!is_animated_file(Path::new("test.png")));
        assert!(!is_animated_file(Path::new("test.jpg")));
        assert!(!is_animated_file(Path::new("test.jpeg")));
        assert!(!is_animated_file(Path::new("test.jxl")));
        // Non-existent AVIF files return false (can't read header)
        assert!(!is_animated_file(Path::new("test.avif")));
    }

    #[test]
    fn test_animated_extensions() {
        // All known animated extensions should be recognized (except AVIF which needs file reading)
        let animated_extensions = ["mp4", "webm", "mkv", "avi", "mov", "m4v", "ogv"];

        for ext in animated_extensions {
            let filename = format!("test.{ext}");
            assert!(
                is_animated_file(Path::new(&filename)),
                "Extension {ext} should be recognized as animated"
            );
        }
    }

    #[test]
    fn test_animated_avif_detection() {
        // Test with the fixture animated AVIF file
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let animated_avif = Path::new(manifest_dir).join("tests/fixtures/animated.avif");

        if animated_avif.exists() {
            assert!(
                is_animated_avif(&animated_avif),
                "animated.avif should be detected as animated AVIF"
            );
            assert!(
                is_animated_file(&animated_avif),
                "animated.avif should be detected as animated file"
            );
            // Note: is_video_file() returns false for AVIF - AVIF is its own source type
            assert!(
                !is_video_file(&animated_avif),
                "AVIF should NOT be detected as video file (separate source type)"
            );
        } else {
            eprintln!(
                "Skipping test: animated AVIF fixture not found at {:?}",
                animated_avif
            );
        }

        // Non-existent files should return false
        assert!(!is_animated_avif(Path::new("nonexistent.avif")));
    }

    #[test]
    fn test_case_insensitive_extensions() {
        // Test various case combinations
        let test_cases = [
            ("test.MP4", true),
            ("test.Mp4", true),
            ("test.mp4", true),
            ("test.WEBM", true),
            ("test.WebM", true),
            ("test.webm", true),
        ];

        for (path, expected) in test_cases {
            assert_eq!(
                is_animated_file(Path::new(path)),
                expected,
                "Path {path} should return {expected}"
            );
        }
    }

    #[test]
    fn test_edge_cases() {
        // No extension
        assert!(!is_animated_file(Path::new("test")));
        assert!(!is_animated_file(Path::new("/path/to/file")));

        // Hidden files with extensions
        assert!(is_animated_file(Path::new(".hidden.mp4")));

        // Multiple dots
        assert!(is_animated_file(Path::new("my.video.file.mp4")));

        // Weird paths
        assert!(is_animated_file(Path::new("./test.mp4")));
        assert!(is_animated_file(Path::new("../test.mp4")));
    }

    // Tests for load_avif_frames (unsafe code)
    mod avif_loading {
        use std::path::PathBuf;
        use std::time::Duration;

        use super::super::super::player::AnimatedPlayer;
        use super::super::super::types::AnimatedSource;

        /// Get the path to the animated AVIF test fixture.
        fn animated_avif_fixture() -> PathBuf {
            let manifest_dir = env!("CARGO_MANIFEST_DIR");
            PathBuf::from(manifest_dir).join("tests/fixtures/animated.avif")
        }

        /// Test loading a valid animated AVIF file.
        /// This exercises the unsafe libavif-sys FFI code.
        #[test]
        fn test_load_animated_avif() {
            let animated_avif = animated_avif_fixture();
            if !animated_avif.exists() {
                eprintln!(
                    "Skipping test: animated AVIF fixture not found at {:?}",
                    animated_avif
                );
                return;
            }

            // Create source - this should detect it as animated AVIF
            let source = AnimatedSource::from_path(&animated_avif);
            assert!(
                matches!(source, Some(AnimatedSource::Avif(_))),
                "Should detect as Avif source"
            );

            // Create player - this calls load_avif_frames internally
            let player = AnimatedPlayer::new(source.unwrap(), 1920, 1080);
            assert!(player.is_ok(), "Should load animated AVIF successfully");

            let player = player.unwrap();

            // Verify we got frames
            let frame = player.current_frame();
            assert!(frame.is_some(), "Should have at least one frame");

            let frame = frame.unwrap();
            // Check image dimensions are valid
            assert!(frame.image.width() > 0, "Frame width should be > 0");
            assert!(frame.image.height() > 0, "Frame height should be > 0");

            // Check duration is reasonable (not zero, not absurdly long)
            assert!(
                frame.duration >= Duration::from_millis(10),
                "Frame duration should be at least 10ms"
            );
            assert!(
                frame.duration <= Duration::from_secs(10),
                "Frame duration should be less than 10s"
            );
        }

        /// Test that loading a non-existent AVIF file fails gracefully.
        #[test]
        fn test_load_nonexistent_avif() {
            let fake_path = PathBuf::from("/nonexistent/fake.avif");
            let source = AnimatedSource::Avif(fake_path);

            let result = AnimatedPlayer::new(source, 1920, 1080);
            assert!(result.is_err(), "Should fail for non-existent file");
        }

        /// Test that the decoder properly cleans up resources.
        /// This tests the DecoderGuard and RgbGuard drop implementations.
        #[test]
        fn test_avif_decoder_cleanup() {
            let animated_avif = animated_avif_fixture();
            if !animated_avif.exists() {
                eprintln!(
                    "Skipping test: animated AVIF fixture not found at {:?}",
                    animated_avif
                );
                return;
            }

            // Load and drop multiple times to check for leaks/crashes
            for _ in 0..5 {
                let source = AnimatedSource::Avif(animated_avif.clone());
                let player = AnimatedPlayer::new(source, 1920, 1080);
                assert!(player.is_ok());
                // Player is dropped here, should clean up properly
            }
        }

        /// Test advancing through all AVIF frames.
        #[test]
        fn test_avif_frame_advancement() {
            let animated_avif = animated_avif_fixture();
            if !animated_avif.exists() {
                eprintln!(
                    "Skipping test: animated AVIF fixture not found at {:?}",
                    animated_avif
                );
                return;
            }

            let source = AnimatedSource::Avif(animated_avif);
            let mut player = AnimatedPlayer::new(source, 1920, 1080).unwrap();

            // Advance through frames and verify each is valid
            let mut frame_count = 0;
            let mut seen_indices = std::collections::HashSet::new();

            // Advance enough times to loop through all frames at least once
            for _ in 0..100 {
                let idx = player.current_frame_index();
                seen_indices.insert(idx);

                let frame = player.current_frame();
                assert!(frame.is_some(), "Frame {} should exist", idx);

                let should_continue = player.advance();
                assert!(should_continue, "AVIF should always loop");

                frame_count += 1;
                if frame_count > 10 && player.current_frame_index() == 0 {
                    // We've looped back to the beginning
                    break;
                }
            }

            assert!(
                seen_indices.len() > 1,
                "Animated AVIF should have multiple frames"
            );
        }

        /// Test that RGBA pixel data is valid (not all zeros or garbage).
        #[test]
        fn test_avif_pixel_data_validity() {
            let animated_avif = animated_avif_fixture();
            if !animated_avif.exists() {
                eprintln!(
                    "Skipping test: animated AVIF fixture not found at {:?}",
                    animated_avif
                );
                return;
            }

            let source = AnimatedSource::Avif(animated_avif);
            let player = AnimatedPlayer::new(source, 1920, 1080).unwrap();

            let frame = player.current_frame().unwrap();
            let rgba = frame.image.to_rgba8();
            let pixels = rgba.as_raw();

            // Check that we have actual pixel data
            assert!(!pixels.is_empty(), "Should have pixel data");

            // Check that it's not all zeros (fully transparent/black)
            let non_zero_count = pixels.iter().filter(|&&p| p != 0).count();
            assert!(
                non_zero_count > pixels.len() / 10,
                "At least 10% of pixels should be non-zero"
            );

            // Verify RGBA layout (4 bytes per pixel)
            assert_eq!(
                pixels.len(),
                (frame.image.width() * frame.image.height() * 4) as usize,
                "Pixel buffer size should match width * height * 4"
            );
        }
    }
}
