# COSMIC Background Service - Copilot Instructions

## Project Overview

`cosmic-bg` is a Wayland background service for the COSMIC desktop environment. It renders wallpapers (static images, GIFs, and videos) to display outputs using `wlr-layer-shell` protocol surfaces.

## Architecture

### Core Components

- **`main.rs`**: Event loop with Wayland client via `smithay-client-toolkit` (sctk). Handles output hotplug, config changes, and fractional scaling.
- **`wallpaper.rs`**: `Wallpaper` struct manages per-output or "all outputs" backgrounds. Contains image queue for slideshows, timer registration, file watching, and animated playback state.
- **`config/`**: Separate crate (`cosmic-bg-config`) defining `Entry`, `Source`, `ScalingMode`, and `Config`. Uses `cosmic-config` for reactive config updates.
- **`draw.rs`**: Renders images to Wayland shared memory buffers (8-bit XRGB8888 or 10-bit XRGB2101010).
- **`scaler.rs`**: Image scaling modes: `fit` (with background color), `zoom` (crop), `stretch`.
- **`colored.rs`**: Generates solid color and gradient backgrounds.
- **`img_source.rs`**: File watcher channel for live directory updates (add/remove wallpapers).
- **`animated.rs`**: GStreamer-based video/GIF player with hardware decode support (NVIDIA NVDEC, AMD/Intel VAAPI).
- **`convert.rs`**: Automatic video format conversion to VP9/WebM for optimal hardware decode on AMD/Intel GPUs.

### Wayland Protocol Stack

Uses `smithay-client-toolkit` (sctk) as the Wayland client abstraction layer:

**Layer Shell (`wlr-layer-shell-unstable-v1`)**
- Creates surfaces on `Layer::Background` - renders behind all windows
- `Anchor::all()` + `set_exclusive_zone(-1)` makes surface fill entire output
- `KeyboardInteractivity::None` - background never receives input

**Fractional Scaling (`wp-fractional-scale-v1`)**
- Handles HiDPI displays with non-integer scale factors (e.g., 1.25x, 1.5x)
- Scale expressed as integer / 120 (e.g., 144 = 1.2x scale)
- Falls back to `wl_output.scale_factor * 120` on older compositors

**Viewporter (`wp-viewporter`)**
- `wp_viewport.set_destination()` maps buffer size to logical surface size
- Enables rendering at scaled resolution then downscaling for display

**Shared Memory (`wl_shm`)**
- Uses `SlotPool` for CPU-side buffer allocation
- Pixel formats: `XRGB8888` (8-bit) or `XRGB2101010` (10-bit HDR)
- Buffer attached to surface, then `wl_surface.commit()` presents

### Rendering Pipeline

**Static Images:**
1. **Image Loading**: `image` crate for standard formats, `jxl-oxide` for JPEG XL
2. **Scaling**: `fast_image_resize` (Lanczos3 filter) with fallback to `image::imageops`
3. **Buffer Write**: Direct pixel copy to `wl_shm` buffer in `draw.rs`
4. **Presentation**: `buffer.attach_to(surface)` → `surface.damage_buffer()` → `surface.commit()`

**Animated Wallpapers (GIF/Video):**
1. **GIF**: Decoded into memory via `image` crate, frames cached, CPU-scaled per output
2. **Video**: GStreamer pipeline → hardware decode (NVDEC/VAAPI) → DMA-BUF or BGRx appsink
3. **Zero-Copy Transfer**: DMA-BUF fd passed directly to compositor (no CPU copy)
4. **Viewport Scaling**: Compositor GPU-scales via `wp_viewport` if needed
5. **Frame Timing**: calloop timer advances frames at video framerate (60fps max)

**Video Pipeline Priority (highest to lowest):**
1. **NVIDIA CUDA→DMA-BUF** (`cudadmabufupload`): NVDEC → CUDA memory → DMA-BUF (<0.5ms)
   - Custom GStreamer plugin: `gst-cuda-dmabuf`
   - Entire pipeline stays in GPU memory, no GL context needed
   - Supports NVIDIA tiled modifiers for optimal compositor import
2. **VAAPI DMA-BUF** (AMD/Intel): Hardware decode → DMA-BUF export (~1ms)
3. **NVIDIA GL DMA-BUF**: NVDEC → GL → gldownload → DMA-BUF (~1ms)
4. **wl_shm fallbacks**: GPU decode → CPU memory → compositor (3-5ms)
5. **Software decode**: CPU decode + convert (not recommended)

**Rendering Paths:**
- **DMA-BUF (preferred)**: GPU decode → DMA-BUF fd → compositor zero-copy (~0.2-1ms)
  - Module: `dmabuf.rs` with `DmaBufBuffer` and NVIDIA tiled modifier support
  - Pipeline: `video/x-raw(memory:DMABuf)` caps in GStreamer
  - Protocol: `zwp_linux_dmabuf_v1` detected and available on COSMIC compositor
  - NVIDIA: Uses `cudadmabufupload` plugin for optimal performance
- **wl_shm (fallback)**: GPU decode → CPU memory → compositor GPU upload (~3-5ms)

### Data Flow

1. Config loaded from `cosmic-config` (key: `com.system76.CosmicBackground`)
2. `ConfigWatchSource` triggers updates on config changes
3. `Wallpaper::load_images()` populates image queue from path/directory
4. On configure/scale change, `Wallpaper::draw()` or `draw_animated_frame()` renders to layer surface
5. Video format auto-conversion via `convert.rs` (H.264→VP9 on AMD/Intel for hw decode support)
6. State persisted to `cosmic-config` state for slideshow resume

### Key Patterns

**Wayland delegation macros** - Use sctk's delegate macros for protocol handling:
```rust
delegate_compositor!(CosmicBg);
delegate_output!(CosmicBg);
delegate_layer!(CosmicBg);
```

**Config keys** - Backgrounds stored with `output.{name}` prefix, "all" for default:
```rust
pub const BACKGROUNDS: &str = "backgrounds";
pub const DEFAULT_BACKGROUND: &str = "all";
```

## Development Commands

```bash
just build-debug      # Debug build
just build-release    # Release build (default)
just run              # Run with RUST_LOG=debug RUST_BACKTRACE=1
just check            # Clippy with pedantic warnings
just install          # Install to /usr/bin
```

## Debugging

Kill existing `cosmic-bg` processes repeatedly to prevent `cosmic-session` respawn, then:
```bash
just run
```

## Code Conventions

- **SPDX headers**: Every source file must have `// SPDX-License-Identifier: MPL-2.0`
- **Error handling**: Use `eyre`/`color-eyre` for errors, `tracing` for logging
- **Memory management**: On glibc, `malloc_trim()` called after config changes to prevent fragmentation
- **Image formats**: Standard formats via `image` crate, JPEG XL via `jxl-oxide`

## Configuration Schema

`Entry` fields in `config/src/lib.rs`:
- `output`: Display name or "all"
- `source`: `Source::Path(PathBuf)` or `Source::Color(Color)`
- `scaling_mode`: `Zoom` (default), `Fit([r,g,b])`, `Stretch`
- `rotation_frequency`: Slideshow interval in seconds
- `sampling_method`: `Alphanumeric` or `Random` for slideshows

## Dependencies

- Rust 1.85+ (2024 edition)
- Wayland: `libwayland-dev`, `libxkbcommon-dev`
- Linker: `mold` (optional, auto-detected)
- Build: `just`, `pkg-config`
- `scaling_mode`: `Zoom` (default), `Fit([r,g,b])`, `Stretch`
- `rotation_frequency`: Slideshow interval in seconds
- `sampling_method`: `Alphanumeric` or `Random` for slideshows

## Dependencies

- Rust 1.85+ (2024 edition)
- Wayland: `libwayland-dev`, `libxkbcommon-dev`
- Linker: `mold` (optional, auto-detected)
- Build: `just`, `pkg-config`
