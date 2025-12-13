// SPDX-License-Identifier: MPL-2.0

//! Unit tests for animated wallpaper functionality.

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::super::detection::{is_animated_file, is_gif_file, is_video_file};

    #[test]
    fn test_is_gif_file() {
        assert!(is_gif_file(Path::new("test.gif")));
        assert!(is_gif_file(Path::new("test.GIF")));
        assert!(is_gif_file(Path::new("/path/to/animation.gif")));
        assert!(!is_gif_file(Path::new("test.mp4")));
        assert!(!is_gif_file(Path::new("test.webm")));
        assert!(!is_gif_file(Path::new("test.png")));
    }

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
        assert!(!is_video_file(Path::new("test.gif")));
        assert!(!is_video_file(Path::new("test.png")));
        assert!(!is_video_file(Path::new("test.jpg")));
    }

    #[test]
    fn test_is_animated_file() {
        // GIF should be animated
        assert!(is_animated_file(Path::new("test.gif")));
        assert!(is_animated_file(Path::new("test.GIF")));

        // Videos should be animated
        assert!(is_animated_file(Path::new("test.mp4")));
        assert!(is_animated_file(Path::new("test.webm")));
        assert!(is_animated_file(Path::new("test.mkv")));

        // Static images should not be animated
        assert!(!is_animated_file(Path::new("test.png")));
        assert!(!is_animated_file(Path::new("test.jpg")));
        assert!(!is_animated_file(Path::new("test.jpeg")));
        assert!(!is_animated_file(Path::new("test.jxl")));
    }

    #[test]
    fn test_animated_extensions() {
        // All known animated extensions should be recognized
        let animated_extensions = ["gif", "mp4", "webm", "mkv", "avi", "mov", "m4v", "ogv"];

        for ext in animated_extensions {
            let filename = format!("test.{ext}");
            assert!(
                is_animated_file(Path::new(&filename)),
                "Extension {ext} should be recognized as animated"
            );
        }
    }

    #[test]
    fn test_case_insensitive_extensions() {
        // Test various case combinations
        let test_cases = [
            ("test.GIF", true),
            ("test.Gif", true),
            ("test.gif", true),
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
        assert!(is_animated_file(Path::new(".hidden.gif")));
        assert!(is_animated_file(Path::new(".hidden.mp4")));

        // Multiple dots
        assert!(is_animated_file(Path::new("test.backup.gif")));
        assert!(is_animated_file(Path::new("my.video.file.mp4")));

        // Weird paths
        assert!(is_animated_file(Path::new("./test.gif")));
        assert!(is_animated_file(Path::new("../test.mp4")));
    }
}
