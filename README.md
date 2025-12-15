# cosmic-bg

COSMIC session service which applies backgrounds to displays. Supports the following features:

- Supports common image formats: JPEG, PNG, WebP, AVIF, JPEG XL, and more via [image-rs](https://github.com/image-rs/image#supported-image-formats)
- **Live/Animated wallpapers** - GIF animations, animated AVIF, and video formats (MP4, WebM, MKV, etc.) with hardware acceleration
- 8 and 10-bit background surface layers
- Use of colors and gradients for backgrounds
- Per-display background application
- Wallpaper slideshows that alternate between backgrounds periodically

## Live Wallpaper Support

The `animated` feature (enabled by default) adds support for animated wallpapers using GStreamer for hardware-accelerated video playback.

### Supported Formats

| Format | Extension | Decode Method |
|--------|-----------|---------------|
| GIF | `.gif` | CPU (frames cached in memory) |
| Animated AVIF | `.avif` | CPU via libavif (frames cached in memory) |
| MPEG-4 | `.mp4`, `.m4v` | NVIDIA (all codecs), AMD/Intel (H.264/H.265 with freeworld drivers, VP9, AV1) |
| WebM | `.webm` | Full (VP8, VP9, AV1) - **Recommended for AMD without freeworld drivers** |
| Matroska | `.mkv` | Depends on contained codec |
| AVI | `.avi` | Depends on contained codec |
| QuickTime | `.mov` | Depends on contained codec |

### Hardware Requirements

#### NVIDIA GPUs
- **Driver**: NVIDIA proprietary driver 470+
- **GStreamer plugins**: `gstreamer1-plugins-bad` (provides `nvh264dec`, `nvh265dec`, etc.)
- **Supported codecs**: H.264, H.265/HEVC, VP9, AV1
- **Optional**: [gst-cuda-dmabuf](https://github.com/Ericky14/gst-cuda-dmabuf) plugin for zero-copy DMA-BUF rendering

#### AMD/Intel GPUs (VAAPI)
- **Driver**: Mesa 21.0+ with VAAPI support
- **GStreamer plugins**: `gstreamer1-plugins-bad` (GStreamer 1.20+ provides `vah264dec`, `vapostproc`, etc.)
- **Zero-copy**: Native DMA-BUF support via `vapostproc` for efficient compositor rendering
- **Supported codecs** (varies by GPU generation):
  - AMD RDNA/RDNA2+: VP9, AV1 (H.264/H.265 requires `mesa-va-drivers-freeworld` on Fedora)
  - Intel Gen9+: H.264, H.265, VP9, AV1
- **Recommendation**: Use **VP9 or AV1** encoded videos for best AMD compatibility, or install freeworld drivers for H.264/H.265

#### Software Fallback
If no hardware decoder is available, the system falls back to software decoding via GStreamer's `decodebin`. For H.264 content on systems without hardware decode (e.g., AMD on Fedora), install `openh264` for software decode support.

### Codec Detection

At startup, `cosmic-bg` automatically detects available hardware decoders and selects the best pipeline:

1. **Probes GStreamer registry** for NVIDIA (NVDEC) and AMD/Intel (VAAPI) decoders
2. **Tests decoder functionality** - demotes non-functional decoders (e.g., NVDEC when CUDA unavailable)
3. **Selects optimal pipeline** based on container format and available decoders
4. **Falls back gracefully** to software decode if no hardware path available

This ensures videos play correctly regardless of GPU vendor or codec availability.

### Performance

| Scenario | CPU Usage | Notes |
|----------|-----------|-------|
| VP9 1080p on AMD (VAAPI) | ~0.2-0.5% | Hardware decode |
| H.264 1080p on AMD (VAAPI + freeworld) | ~0.2-0.5% | Hardware decode with mesa-va-drivers-freeworld |
| H.264 1080p on NVIDIA (NVDEC) | ~0.3-0.5% | Hardware decode |
| H.264 4K on AMD (software) | ~60-80% | Software fallback (no freeworld drivers) |
| GIF animation | ~1-5% | Depends on frame count/size |
| Animated AVIF | ~1-5% | Depends on frame count/size |

### Configuration

Set an animated wallpaper via cosmic-config:

```ron
(
    output: "all",
    source: Path("/path/to/video.webm"),
    filter_by_theme: false,
    rotation_frequency: 3600,
    filter_method: Lanczos,
    scaling_mode: Zoom,
    sampling_method: Alphanumeric,
    animation_settings: (
        loop_playback: true,
        playback_speed: 1.0,
        frame_cache_size: 30,
    ),
)
```

### Building Without Animation Support

To build without video/animation support (smaller binary, no GStreamer dependency):

```bash
cargo build --release --no-default-features
```

## Dependencies

Developers should install Rust from from https://rustup.rs/.

### Build Dependencies

- just
- cargo / rustc
- libwayland-dev
- libxkbcommon-dev
- mold
- pkg-config
- **libdav1d-devel** - Required for static AVIF image decoding
- **nasm** - Required for building the dav1d AV1 decoder (used for animated AVIF)

```bash
# Fedora
sudo dnf install libdav1d-devel nasm

# Ubuntu/Debian
sudo apt install libdav1d-dev nasm

# Arch
sudo pacman -S dav1d nasm
```

### For Live Wallpaper Support (animated feature)

GStreamer 1.20+ with the following plugins:

**Core (required)**:
- `gstreamer1` - Core GStreamer
- `gstreamer1-plugins-base` - Base plugins including `videoconvert`
- `gstreamer1-plugins-good` - Container demuxers (MP4, WebM, MKV)

**Hardware Acceleration (recommended)**:
- `gstreamer1-plugins-bad` - NVIDIA NVDEC (`nvh264dec`, etc.) and AMD/Intel VA-API (`vah264dec`, `vapostproc`, etc.)

> **Note**: GStreamer 1.20+ includes the modern `va` plugin in `gstreamer1-plugins-bad`. The legacy `gstreamer1-vaapi` package is no longer required on modern systems.

**Example installation**:

```bash
# Fedora
sudo dnf install gstreamer1 gstreamer1-plugins-base gstreamer1-plugins-good \
                 gstreamer1-plugins-bad-free gstreamer1-vaapi

# Ubuntu/Debian
sudo apt install gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
                 gstreamer1.0-plugins-bad gstreamer1.0-vaapi

# Arch
sudo pacman -S gstreamer gst-plugins-base gst-plugins-good \
               gst-plugins-bad gstreamer-vaapi
```

**H.264/H.265 Hardware Decode for AMD (Fedora)**:

AMD GPUs on Fedora lack VAAPI H.264/H.265 hardware decode by default due to patent restrictions in the standard Mesa VA drivers. To enable **hardware-accelerated** H.264/H.265 decoding, install the freeworld Mesa VA drivers from RPM Fusion:

```bash
# Requires RPM Fusion free repository to be enabled
# https://rpmfusion.org/Configuration

# Replace standard Mesa VA drivers with freeworld version (includes H.264/H.265)
sudo dnf swap mesa-va-drivers mesa-va-drivers-freeworld
```

Verify hardware decode is working:
```bash
vainfo | grep -i h264
# Should show: VAProfileH264Main : VAEntrypointVLD
```

**Software fallback (if hardware decode unavailable)**:

If you cannot install the freeworld drivers, you can use the OpenH264 software decoder (high CPU usage for HD/4K content):

```bash
sudo dnf --enable-repo=fedora-cisco-openh264 install gstreamer1-plugin-openh264
```

**Recommendation**: Use VP9/WebM or AV1 encoded videos which have full VAAPI hardware support on AMD without needing freeworld drivers.

### Install

A release build can be generated by running `just`, and then installed with `sudo just install`.

If packaging, use the `rootdir` variable to change the root path, in addition to the prefix: `just rootdir=debian/cosmic-bg prefix=/usr install`.

To reduce compile times across COSMIC applications, either use `sccache`, or set `CARGO_TARGET_DIR` to a shared path and install with `sudo -E just install`.

## Debugging

To get debug logs from the service, first kill the `cosmic-bg` process a few times in a row to prevent it from being launched by `cosmic-session`. Then launch it with `just run` to display backtraces and debug logs in the terminal.

## License

Licensed under the [Mozilla Public License Version 2.0](https://choosealicense.com/licenses/mpl-2.0).

### Contribution

Any contribution intentionally submitted for inclusion in the work by you shall be licensed under the Mozilla Public License Version 2.0 (MPL-2.0). Each source file should have a SPDX copyright notice at the top of the file:

```
// SPDX-License-Identifier: MPL-2.0
```
