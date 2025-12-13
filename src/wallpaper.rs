// SPDX-License-Identifier: MPL-2.0

use crate::{CosmicBg, CosmicBgLayer};

#[cfg(feature = "animated")]
use crate::animated::{AnimatedPlayer, is_animated_file};

// When animated feature is disabled, provide a simple video file check
// to skip video files that can't be rendered as static images
#[cfg(not(feature = "animated"))]
fn is_video_file(path: &std::path::Path) -> bool {
    const VIDEO_EXTENSIONS: &[&str] = &["mp4", "webm", "mkv", "avi", "mov", "m4v"];
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| VIDEO_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
        .unwrap_or(false)
}

use std::{
    collections::VecDeque,
    fs::{self, File},
    path::PathBuf,
    time::{Duration, Instant},
};

use cosmic_bg_config::{Color, Entry, SamplingMethod, ScalingMode, Source, state::State};
use cosmic_config::CosmicConfigEntry;
use eyre::eyre;
use image::{DynamicImage, ImageReader};
use jxl_oxide::integration::JxlDecoder;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use rand::{rng, seq::SliceRandom};
use sctk::reexports::{
    calloop::{
        self, RegistrationToken,
        timer::{TimeoutAction, Timer},
    },
    client::QueueHandle,
};
use tracing::error;
use walkdir::WalkDir;

// TODO filter images by whether they seem to match dark / light mode
// Alternatively only load from light / dark subdirectories given this is active

/// Calculate viewport source and destination based on scaling mode.
///
/// Returns: (src_x, src_y, src_width, src_height, dst_width, dst_height)
/// - Source coordinates are in buffer pixel coordinates
/// - Destination sizes are in logical surface coordinates
fn calculate_viewport(
    buffer_width: u32,
    buffer_height: u32,
    logical_width: u32,
    logical_height: u32,
    scaling_mode: &ScalingMode,
) -> (f64, f64, f64, f64, u32, u32) {
    match scaling_mode {
        ScalingMode::Stretch => {
            // Stretch: use entire buffer, scale to fill entire logical surface
            (
                0.0,
                0.0,
                buffer_width as f64,
                buffer_height as f64,
                logical_width,
                logical_height,
            )
        }

        ScalingMode::Zoom => {
            // Zoom: fill surface and crop buffer to maintain aspect ratio
            let buffer_aspect = buffer_width as f64 / buffer_height as f64;
            let surface_aspect = logical_width as f64 / logical_height as f64;

            if buffer_aspect > surface_aspect {
                // Buffer is wider - crop width
                let visible_width = (buffer_height as f64 * surface_aspect).round();
                let crop_x = ((buffer_width as f64 - visible_width) / 2.0).round();
                (
                    crop_x,
                    0.0,
                    visible_width,
                    buffer_height as f64,
                    logical_width,
                    logical_height,
                )
            } else {
                // Buffer is taller - crop height
                let visible_height = (buffer_width as f64 / surface_aspect).round();
                let crop_y = ((buffer_height as f64 - visible_height) / 2.0).round();
                (
                    0.0,
                    crop_y,
                    buffer_width as f64,
                    visible_height,
                    logical_width,
                    logical_height,
                )
            }
        }

        ScalingMode::Fit(_color) => {
            // Fit: scale buffer to fit inside surface, maintain aspect ratio
            // Note: We don't render the background color - compositor background shows through
            let buffer_aspect = buffer_width as f64 / buffer_height as f64;
            let surface_aspect = logical_width as f64 / logical_height as f64;

            if buffer_aspect > surface_aspect {
                // Buffer is wider - fit to width, reduce height
                let fitted_height = (logical_width as f64 / buffer_aspect).round() as u32;
                (
                    0.0,
                    0.0,
                    buffer_width as f64,
                    buffer_height as f64,
                    logical_width,
                    fitted_height,
                )
            } else {
                // Buffer is taller - fit to height, reduce width
                let fitted_width = (logical_height as f64 * buffer_aspect).round() as u32;
                (
                    0.0,
                    0.0,
                    buffer_width as f64,
                    buffer_height as f64,
                    fitted_width,
                    logical_height,
                )
            }
        }
    }
}

/// Animation playback state
#[cfg(feature = "animated")]
#[derive(Debug)]
pub struct AnimationState {
    /// The animated player managing frame decoding
    pub player: AnimatedPlayer,
    /// Timer token for frame advancement
    pub frame_timer_token: Option<RegistrationToken>,
    /// Last rendered frame index (to detect frame changes)
    pub last_frame_index: usize,
}

#[cfg(feature = "animated")]
impl AnimationState {
    pub fn new(player: AnimatedPlayer) -> Self {
        Self {
            player,
            frame_timer_token: None,
            last_frame_index: usize::MAX,
        }
    }
}

#[derive(Debug)]
pub struct Wallpaper {
    pub entry: Entry,
    pub layers: Vec<CosmicBgLayer>,
    pub image_queue: VecDeque<PathBuf>,
    loop_handle: calloop::LoopHandle<'static, CosmicBg>,
    queue_handle: QueueHandle<CosmicBg>,
    current_source: Option<Source>,
    // Cache of source image, if `current_source` is a `Source::Path`
    current_image: Option<image::DynamicImage>,
    timer_token: Option<RegistrationToken>,
    /// Animation state for animated wallpapers (GIF, video) - appsink mode
    #[cfg(feature = "animated")]
    animation_state: Option<AnimationState>,
}

impl Drop for Wallpaper {
    fn drop(&mut self) {
        if let Some(token) = self.timer_token.take() {
            self.loop_handle.remove(token);
        }
        #[cfg(feature = "animated")]
        if let Some(ref mut anim) = self.animation_state {
            if let Some(token) = anim.frame_timer_token.take() {
                self.loop_handle.remove(token);
            }
        }
    }
}

impl Wallpaper {
    pub fn new(
        entry: Entry,
        queue_handle: QueueHandle<CosmicBg>,
        loop_handle: calloop::LoopHandle<'static, CosmicBg>,
        source_tx: calloop::channel::SyncSender<(String, notify::Event)>,
    ) -> Self {
        let mut wallpaper = Wallpaper {
            entry,
            layers: Vec::new(),
            current_source: None,
            current_image: None,
            image_queue: VecDeque::default(),
            timer_token: None,
            loop_handle,
            queue_handle,
            #[cfg(feature = "animated")]
            animation_state: None,
        };

        wallpaper.load_images();
        wallpaper.register_timer();
        wallpaper.watch_source(source_tx);
        wallpaper
    }

    pub fn save_state(&self) -> Result<(), cosmic_config::Error> {
        let Some(cur_source) = self.current_source.clone() else {
            return Ok(());
        };
        let state_helper = State::state()?;
        let mut state = State::get_entry(&state_helper).unwrap_or_default();
        for l in &self.layers {
            let name = l.output_info.name.clone().unwrap_or_default();
            if let Some((_, source)) = state
                .wallpapers
                .iter_mut()
                .find(|(output, _)| *output == name)
            {
                *source = cur_source.clone();
            } else {
                state.wallpapers.push((name, cur_source.clone()))
            }
        }
        state.write_entry(&state_helper)
    }

    #[allow(clippy::too_many_lines)]
    pub fn draw(&mut self) {
        let start = Instant::now();
        let mut cur_resized_img: Option<DynamicImage> = None;

        for layer in self.layers.iter_mut().filter(|layer| layer.needs_redraw) {
            let Some(pool) = layer.pool.as_mut() else {
                continue;
            };

            let Some(fractional_scale) = layer.fractional_scale else {
                continue;
            };

            let Some((width, height)) = layer.size else {
                continue;
            };

            let width = width * fractional_scale / 120;
            let height = height * fractional_scale / 120;

            if cur_resized_img
                .as_ref()
                .is_none_or(|img| img.width() != width || img.height() != height)
            {
                let Some(source) = self.current_source.as_ref() else {
                    tracing::info!("No source for wallpaper");
                    continue;
                };

                cur_resized_img = match source {
                    Source::Path(path) => {
                        // Skip animated files - they're handled by draw_animated_frame()
                        #[cfg(feature = "animated")]
                        if is_animated_file(path) {
                            continue;
                        }

                        // Skip video files when animated feature is disabled
                        #[cfg(not(feature = "animated"))]
                        if is_video_file(path) {
                            continue;
                        }

                        if self.current_image.is_none() {
                            self.current_image = Some(match path.extension() {
                                Some(ext) if ext == "jxl" => match decode_jpegxl(path) {
                                    Ok(image) => image,
                                    Err(why) => {
                                        tracing::warn!(
                                            ?why,
                                            "jpegl-xl image decode failed: {}",
                                            path.display()
                                        );
                                        continue;
                                    }
                                },

                                _ => match ImageReader::open(path) {
                                    Ok(img) => {
                                        match img
                                            .with_guessed_format()
                                            .ok()
                                            .and_then(|f| f.decode().ok())
                                        {
                                            Some(img) => img,
                                            None => {
                                                tracing::warn!(
                                                    "could not decode image: {}",
                                                    path.display()
                                                );
                                                continue;
                                            }
                                        }
                                    }
                                    Err(_) => continue,
                                },
                            });
                        }
                        let img = self.current_image.as_ref().unwrap();

                        match self.entry.scaling_mode {
                            ScalingMode::Fit(color) => Some(crate::scaler::fit(
                                img,
                                &color,
                                width,
                                height,
                                &self.entry.filter_method,
                            )),

                            ScalingMode::Zoom => Some(crate::scaler::zoom(
                                img,
                                width,
                                height,
                                &self.entry.filter_method,
                            )),

                            ScalingMode::Stretch => Some(crate::scaler::stretch(
                                img,
                                width,
                                height,
                                &self.entry.filter_method,
                            )),
                        }
                    }

                    Source::Color(Color::Single([r, g, b])) => Some(image::DynamicImage::from(
                        crate::colored::single([*r, *g, *b], width, height),
                    )),

                    Source::Color(Color::Gradient(gradient)) => {
                        match crate::colored::gradient(gradient, width, height) {
                            Ok(buffer) => Some(image::DynamicImage::from(buffer)),
                            Err(why) => {
                                tracing::error!(
                                    ?gradient,
                                    ?why,
                                    "color gradient in config is invalid"
                                );
                                None
                            }
                        }
                    }
                };
            }

            let image = cur_resized_img.as_ref().unwrap();
            let buffer_result =
                crate::draw::canvas(pool, image, width as i32, height as i32, width as i32 * 4);

            match buffer_result {
                Ok(buffer) => {
                    crate::draw::layer_surface(
                        layer,
                        &self.queue_handle,
                        &buffer,
                        (width as i32, height as i32),
                    );
                    layer.needs_redraw = false;

                    let elapsed = Instant::now().duration_since(start);

                    tracing::debug!(?elapsed, source = ?self.entry.source, "wallpaper draw");
                }

                Err(why) => {
                    tracing::error!(?why, "wallpaper could not be drawn");
                }
            }
        }
    }

    /// Draw animated wallpaper frame (GIF or video).
    ///
    /// For video: Uses wl_shm path (DMA-BUF is handled by timer callback with access to dmabuf_global).
    /// For GIF: Uses CPU scaling with per-resolution frame caching.
    #[cfg(feature = "animated")]
    fn draw_animated_frame(&mut self, start: Instant) {
        let Some(mut anim_state) = self.animation_state.take() else {
            return;
        };

        let is_video = anim_state.player.is_video();

        // Process GStreamer bus messages for video (handles EOS/looping/errors)
        if is_video && anim_state.player.process_messages() {
            tracing::warn!("Video playback stopped (EOS or error)");
            self.animation_state = Some(anim_state);
            return;
        }

        let current_frame_idx = anim_state.player.current_frame_index();

        // Track frame changes for logging
        if anim_state.last_frame_index != current_frame_idx {
            anim_state.last_frame_index = current_frame_idx;
        }

        if is_video {
            // Video: wl_shm fallback path (DMA-BUF handled by timer callback)
            let _ = self.draw_video_frame_zero_copy(&anim_state, current_frame_idx, start);
            self.animation_state = Some(anim_state);
            return;
        }

        // GIF: viewport scaling (GPU-accelerated)
        self.draw_gif_frame(&mut anim_state, current_frame_idx, start);
        self.animation_state = Some(anim_state);
    }

    /// Draw a GIF frame using viewport scaling (GPU-accelerated).
    ///
    /// Similar to video rendering:
    /// 1. Write native-resolution GIF frame to wl_shm buffer (small, fast)
    /// 2. Use wp_viewport to GPU-scale to screen resolution
    ///
    /// This is much faster than CPU scaling each frame.
    #[cfg(feature = "animated")]
    fn draw_gif_frame(
        &mut self,
        anim_state: &mut AnimationState,
        current_frame_idx: usize,
        start: Instant,
    ) {
        use sctk::reexports::client::protocol::wl_shm;
        use sctk::shell::WaylandSurface;

        // Get current frame from player
        let gif_frame = match anim_state.player.current_frame() {
            Some(f) => f,
            None => return,
        };

        let frame_width = gif_frame.image.width();
        let frame_height = gif_frame.image.height();

        // Find layers that need redraw and have pools
        let layers_needing_redraw: Vec<usize> = self
            .layers
            .iter()
            .enumerate()
            .filter(|(_, layer)| {
                layer.needs_redraw
                    && layer.pool.is_some()
                    && layer.fractional_scale.is_some()
                    && layer.size.is_some()
            })
            .map(|(i, _)| i)
            .collect();

        if layers_needing_redraw.is_empty() {
            return;
        }

        // Create buffer at native GIF resolution (not screen resolution)
        let first_idx = layers_needing_redraw[0];
        let pool = self.layers[first_idx].pool.as_mut().unwrap();

        let buffer_result = pool.create_buffer(
            frame_width as i32,
            frame_height as i32,
            frame_width as i32 * 4,
            wl_shm::Format::Xrgb8888,
        );

        let (buffer, canvas) = match buffer_result {
            Ok(b) => b,
            Err(why) => {
                tracing::error!(?why, "Failed to create GIF buffer");
                return;
            }
        };

        // Write native-resolution frame to buffer (fast - small buffer)
        crate::draw::xrgb888_canvas(canvas, &gif_frame.image);

        let wl_buffer = buffer.wl_buffer();

        // Attach to all surfaces with viewport scaling
        for &layer_idx in &layers_needing_redraw {
            let layer = &mut self.layers[layer_idx];
            let (logical_width, logical_height) = layer.size.unwrap();

            let wl_surface = layer.layer.wl_surface();

            // Damage the buffer
            wl_surface.damage_buffer(0, 0, frame_width as i32, frame_height as i32);

            // Request next frame callback
            layer
                .layer
                .wl_surface()
                .frame(&self.queue_handle, wl_surface.clone());

            // Attach the buffer
            wl_surface.attach(Some(wl_buffer), 0, 0);

            // Calculate viewport for GPU scaling
            let (src_x, src_y, src_w, src_h, dst_w, dst_h) = calculate_viewport(
                frame_width,
                frame_height,
                logical_width,
                logical_height,
                &self.entry.scaling_mode,
            );

            // Set viewport source and destination for GPU scaling
            layer.viewport.set_source(src_x, src_y, src_w, src_h);
            layer.viewport.set_destination(dst_w as i32, dst_h as i32);

            wl_surface.commit();
            layer.needs_redraw = false;
        }

        tracing::debug!(
            frame = current_frame_idx,
            src_w = frame_width,
            src_h = frame_height,
            total = ?start.elapsed(),
            "GIF frame drawn (viewport scaling)"
        );
    }

    /// Draw a video frame using viewport scaling with SHARED BUFFER and ZERO-COPY.
    ///
    /// This is the fastest possible path:
    /// 1. Create ONE wl_shm buffer
    /// 2. Pull frame directly from GStreamer into that buffer (single copy)
    /// 3. Attach the same wl_buffer to ALL surfaces
    /// 4. Let compositor GPU-scale via viewport protocol
    ///
    /// Total: ONE memory copy regardless of output count!
    #[cfg(feature = "animated")]
    fn draw_video_frame_zero_copy(
        &mut self,
        anim_state: &AnimationState,
        frame_idx: usize,
        start: Instant,
    ) -> bool {
        use sctk::reexports::client::protocol::wl_shm;
        use sctk::shell::WaylandSurface;

        // Find layers that need redraw
        let layers_needing_redraw: Vec<usize> = self
            .layers
            .iter()
            .enumerate()
            .filter(|(_, layer)| {
                layer.needs_redraw
                    && layer.pool.is_some()
                    && layer.fractional_scale.is_some()
                    && layer.size.is_some()
            })
            .map(|(i, _)| i)
            .collect();

        if layers_needing_redraw.is_empty() {
            return false;
        }

        // Get the video dimensions from the pipeline.
        // If not available yet (pipeline not prerolled), skip this frame.
        let (frame_width, frame_height) = match anim_state.player.video_dimensions() {
            Some(dims) => dims,
            None => {
                tracing::trace!("Video dimensions not available yet");
                return false;
            }
        };

        let first_idx = layers_needing_redraw[0];
        let draw_start = Instant::now();

        // wl_shm path: GPU decode → system memory → wl_shm buffer
        // Create buffer at the ACTUAL video dimensions (not max 4K).
        // This ensures the wl_buffer dimensions match what we're writing.
        let pool = self.layers[first_idx].pool.as_mut().unwrap();
        let buffer_result = pool.create_buffer(
            frame_width as i32,
            frame_height as i32,
            frame_width as i32 * 4,
            wl_shm::Format::Xrgb8888,
        );

        let (buffer, canvas) = match buffer_result {
            Ok(b) => b,
            Err(why) => {
                tracing::error!(?why, "failed to create buffer");
                return false;
            }
        };

        // PRE-TOUCH: Force allocation of physical pages by writing to the entire buffer.
        // Without this, the first write to newly allocated wl_shm memory triggers
        // page faults and kernel allocation, causing 30ms+ latency on first frame.
        // We fill with black (0x00000000) which is fast and avoids visual glitches.
        if canvas.len() >= 8_000_000 {
            // Only pre-touch for large buffers (>8MB, i.e., ~2K+ resolution)
            let touch_start = Instant::now();
            // Fill entire buffer with zeros (black) to force page allocation
            canvas.fill(0);
            let touch_time = touch_start.elapsed();
            if touch_time.as_millis() > 1 {
                tracing::debug!(
                    ?touch_time,
                    buffer_size = canvas.len(),
                    "Pre-filled wl_shm buffer"
                );
            }
        }

        // ZERO-COPY: Pull frame directly into wl_shm buffer
        // Try to get new frame first, fall back to cached frame if none available
        let frame_info = match anim_state.player.pull_frame_to_buffer(canvas) {
            Some(info) => {
                tracing::trace!(
                    width = info.width,
                    height = info.height,
                    is_bgrx = info.is_bgrx,
                    "Pulled new video frame"
                );
                info
            }
            None => {
                // No new frame available, try cached frame to maintain smooth playback
                match anim_state.player.pull_cached_frame(canvas) {
                    Some(info) => {
                        tracing::trace!("Reusing cached frame (no new frame available)");
                        info
                    }
                    None => {
                        tracing::trace!("No frame available yet (no cache)");
                        return false;
                    }
                }
            }
        };

        let canvas_time = draw_start.elapsed();

        // Verify the frame dimensions match what we expected
        debug_assert_eq!(frame_info.width, frame_width, "Frame width mismatch");
        debug_assert_eq!(frame_info.height, frame_height, "Frame height mismatch");

        // Get the underlying wl_buffer for sharing
        let wl_buffer = buffer.wl_buffer();

        // Attach the SAME buffer to ALL surfaces
        let surface_start = Instant::now();
        let mut surfaces_updated = 0;

        for &layer_idx in &layers_needing_redraw {
            let layer = &mut self.layers[layer_idx];

            let (logical_width, logical_height) = layer.size.unwrap();

            let wl_surface = layer.layer.wl_surface();

            // Damage the entire buffer
            wl_surface.damage_buffer(0, 0, frame_width as i32, frame_height as i32);

            // Request our next frame
            layer
                .layer
                .wl_surface()
                .frame(&self.queue_handle, wl_surface.clone());

            // Attach the SHARED buffer
            wl_surface.attach(Some(wl_buffer), 0, 0);

            // Calculate viewport based on scaling_mode
            let (src_x, src_y, src_w, src_h, dst_w, dst_h) = calculate_viewport(
                frame_width,
                frame_height,
                logical_width,
                logical_height,
                &self.entry.scaling_mode,
            );

            // Set viewport source (which part of buffer to use)
            layer.viewport.set_source(src_x, src_y, src_w, src_h);

            // Set viewport destination (logical size to scale to)
            layer.viewport.set_destination(dst_w as i32, dst_h as i32);

            wl_surface.commit();
            layer.needs_redraw = false;
            surfaces_updated += 1;
        }

        let surface_time = surface_start.elapsed();
        let total_elapsed = Instant::now().duration_since(start);

        // Log the first layer's dimensions for debugging
        let (log_dest_w, log_dest_h) = self
            .layers
            .get(layers_needing_redraw[0])
            .and_then(|l| l.size)
            .unwrap_or((0, 0));

        tracing::debug!(
            frame = frame_idx,
            ?canvas_time,
            ?surface_time,
            ?total_elapsed,
            surfaces = surfaces_updated,
            src_w = frame_width,
            src_h = frame_height,
            dest_w = log_dest_w,
            dest_h = log_dest_h,
            "draw timing (ZERO-COPY, shared buffer, viewport GPU scaling)"
        );

        true
    }

    /// Try to draw a video frame using DMA-BUF zero-copy (true GPU-only rendering).
    ///
    /// This is the ultimate performance path:
    /// 1. Extract DMA-BUF fd from GStreamer
    /// 2. Create wl_buffer from DMA-BUF via zwp_linux_dmabuf_v1
    /// 3. Attach to ALL surfaces (compositor GPU-reads directly)
    /// 4. GPU-scale via viewport
    ///
    /// Total: ZERO CPU copies, all data stays in GPU memory!
    ///
    /// Returns true if successfully rendered, false to trigger wl_shm fallback.
    #[cfg(feature = "animated")]
    pub fn try_draw_video_frame_dmabuf(
        &mut self,
        anim_state: &AnimationState,
        _frame_idx: usize,
        start: Instant,
        dmabuf_global: Option<&wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
    ) -> bool {
        use sctk::shell::WaylandSurface;

        let Some(dmabuf_global) = dmabuf_global else {
            return false;
        };

        // Try to get DMA-BUF frame from player
        let mut dmabuf_frame = match anim_state.player.try_get_dmabuf_frame() {
            Some(f) => {
                tracing::debug!(
                    width = f.width,
                    height = f.height,
                    planes = f.planes.len(),
                    "Got DMA-BUF frame"
                );
                f
            }
            None => return false,
        };

        // Create wl_buffer from DMA-BUF
        let wl_buffer = match dmabuf_frame.create_wl_buffer(dmabuf_global, &self.queue_handle) {
            Some(b) => b,
            None => {
                tracing::warn!("Failed to create DMA-BUF wl_buffer - falling back to wl_shm");
                return false;
            }
        };

        // Find layers that need redraw
        let layers_needing_redraw: Vec<usize> = self
            .layers
            .iter()
            .enumerate()
            .filter(|(_, layer)| {
                layer.needs_redraw && layer.fractional_scale.is_some() && layer.size.is_some()
            })
            .map(|(i, _)| i)
            .collect();

        if layers_needing_redraw.is_empty() {
            return false;
        }

        // Attach the DMA-BUF buffer to all surfaces
        for &layer_idx in &layers_needing_redraw {
            let layer = &mut self.layers[layer_idx];
            let (logical_width, logical_height) = layer.size.unwrap();

            let wl_surface = layer.layer.wl_surface();

            // Damage the entire buffer
            wl_surface.damage_buffer(0, 0, dmabuf_frame.width as i32, dmabuf_frame.height as i32);

            // Request our next frame
            layer
                .layer
                .wl_surface()
                .frame(&self.queue_handle, wl_surface.clone());

            // Attach the DMA-BUF buffer
            wl_surface.attach(Some(&wl_buffer), 0, 0);

            // Calculate viewport based on scaling_mode
            let (src_x, src_y, src_w, src_h, dst_w, dst_h) = calculate_viewport(
                dmabuf_frame.width,
                dmabuf_frame.height,
                logical_width,
                logical_height,
                &self.entry.scaling_mode,
            );

            // Set viewport source (which part of buffer to use)
            layer.viewport.set_source(src_x, src_y, src_w, src_h);

            // Set viewport destination (logical size to scale to)
            layer.viewport.set_destination(dst_w as i32, dst_h as i32);

            wl_surface.commit();
            layer.needs_redraw = false;
        }

        let total_elapsed = Instant::now().duration_since(start);
        tracing::debug!(
            ?total_elapsed,
            width = dmabuf_frame.width,
            height = dmabuf_frame.height,
            "DMA-BUF zero-copy render"
        );

        true
    }

    /// Initialize animation playback for animated files (GIF, video).
    ///
    /// For video files, the video is scaled during decode to match the largest
    /// output resolution. This is more efficient than scaling during render.
    ///
    /// If the video format is not optimal for the current GPU, it will be
    /// automatically converted to VP9/WebM for better hardware decode support.
    #[cfg(feature = "animated")]
    pub fn init_animation(&mut self, path: &std::path::Path) -> bool {
        use crate::animated::{AnimatedSource, is_video_file};
        use crate::convert::get_optimal_video_path;

        // For video files, check if format conversion is needed for optimal playback
        let playback_path = if is_video_file(path) {
            let optimal = get_optimal_video_path(path);
            if optimal != path {
                tracing::info!(
                    original = %path.display(),
                    converted = %optimal.display(),
                    "Using converted video for optimal hardware decode"
                );
            }
            optimal
        } else {
            path.to_path_buf()
        };

        let Some(source) = AnimatedSource::from_path(&playback_path) else {
            tracing::warn!(path = %playback_path.display(), "Not an animated file");
            return false;
        };

        // Get the largest output dimensions from layers
        // Video will be scaled to this resolution during decode for efficiency
        let (target_width, target_height) = self
            .layers
            .iter()
            .filter_map(|layer| {
                let (w, h) = layer.size?;
                let scale = layer.fractional_scale.unwrap_or(120);
                Some((w * scale / 120, h * scale / 120))
            })
            .max_by_key(|(w, h)| w * h)
            .unwrap_or((1920, 1080)); // Default to 1080p if no layers yet

        tracing::debug!(
            path = %playback_path.display(),
            target_width,
            target_height,
            "Initializing animated wallpaper (appsink mode)"
        );

        match AnimatedPlayer::new(source, target_width, target_height) {
            Ok(player) => {
                tracing::debug!(path = %playback_path.display(), "Initialized animated wallpaper");
                self.animation_state = Some(AnimationState::new(player));
                self.register_frame_timer();
                true
            }
            Err(e) => {
                tracing::error!(?e, path = %playback_path.display(), "Failed to initialize animated wallpaper");
                false
            }
        }
    }

    /// Register the frame advancement timer for animation playback.
    #[cfg(feature = "animated")]
    fn register_frame_timer(&mut self) {
        // Remove existing timer if any
        if let Some(ref mut anim) = self.animation_state {
            if let Some(token) = anim.frame_timer_token.take() {
                self.loop_handle.remove(token);
            }
        }

        let Some(ref anim_state) = self.animation_state else {
            return;
        };

        let frame_duration = anim_state.player.current_duration();
        let output_name = self.entry.output.clone();

        tracing::debug!(?frame_duration, %output_name, "Registering frame timer");

        let token = self
            .loop_handle
            .insert_source(
                Timer::from_duration(frame_duration),
                move |_, _, state: &mut CosmicBg| {
                    use std::sync::atomic::{AtomicU64, Ordering};
                    use std::time::{SystemTime, UNIX_EPOCH};
                    let timer_start = Instant::now();

                    static LAST_TICK_US: AtomicU64 = AtomicU64::new(0);
                    static EXPECTED_TICK_US: AtomicU64 = AtomicU64::new(0);

                    let now_us = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_micros() as u64;
                    let last_us = LAST_TICK_US.swap(now_us, Ordering::Relaxed);

                    if last_us > 0 {
                        let actual_interval_us = now_us.saturating_sub(last_us);
                        let expected_us = EXPECTED_TICK_US.load(Ordering::Relaxed);
                        if expected_us > 0 {
                            let drift_us = actual_interval_us.saturating_sub(expected_us) as i64;
                            tracing::debug!(
                                "Timer: actual={}μs, expected={}μs, drift={}μs",
                                actual_interval_us,
                                expected_us,
                                drift_us
                            );
                        }
                    }

                    let Some(wallpaper) = state
                        .wallpapers
                        .iter_mut()
                        .find(|w| w.entry.output == output_name)
                    else {
                        return TimeoutAction::Drop;
                    };

                    // Take animation state to work with it
                    let Some(mut anim_state) = wallpaper.animation_state.take() else {
                        return TimeoutAction::Drop;
                    };

                    // Check if playback should continue (handles EOS/looping)
                    if !anim_state.player.advance() {
                        tracing::warn!("Animation ended (no loop), stopping");
                        wallpaper.animation_state = Some(anim_state);
                        return TimeoutAction::Drop;
                    }

                    // Get next duration before putting state back
                    let next_duration = anim_state.player.current_duration();

                    // Store expected interval for drift calculation (uses static from closure above)
                    EXPECTED_TICK_US.store(next_duration.as_micros() as u64, Ordering::Relaxed);

                    // Mark all layers for redraw
                    for layer in &mut wallpaper.layers {
                        layer.needs_redraw = true;
                    }

                    // Try DMA-BUF zero-copy first (if available)
                    let dmabuf_rendered = wallpaper.try_draw_video_frame_dmabuf(
                        &anim_state,
                        anim_state.player.current_frame_index(),
                        timer_start,
                        state.dmabuf_state.dmabuf_global.as_ref(),
                    );

                    // Put state back
                    wallpaper.animation_state = Some(anim_state);

                    // If DMA-BUF didn't work, fallback to regular draw
                    if !dmabuf_rendered {
                        wallpaper.draw_animated_frame(timer_start);
                    }

                    let render_time = timer_start.elapsed();

                    // Compensate for render time: schedule next frame sooner
                    // to maintain correct animation speed
                    let adjusted_duration = next_duration.saturating_sub(render_time);
                    let adjusted_duration = adjusted_duration.max(Duration::from_millis(1));

                    tracing::debug!(
                        render_time_ms = render_time.as_millis(),
                        frame_duration_ms = next_duration.as_millis(),
                        adjusted_ms = adjusted_duration.as_millis(),
                        frame_idx = wallpaper
                            .animation_state
                            .as_ref()
                            .map(|a| a.player.current_frame_index())
                            .unwrap_or(999),
                        "Frame timer tick"
                    );

                    // Schedule next frame with adjusted duration
                    TimeoutAction::ToDuration(adjusted_duration)
                },
            )
            .ok();

        if let Some(ref mut anim) = self.animation_state {
            anim.frame_timer_token = token;
        }
    }

    /// Stop animation playback and clean up resources.
    #[cfg(feature = "animated")]
    pub fn stop_animation(&mut self) {
        tracing::debug!("Stopping animation for wallpaper: {}", self.entry.output);
        if let Some(ref mut anim_state) = self.animation_state {
            // Stop the GStreamer pipeline first
            let _ = anim_state.player.stop();

            // Remove frame timer
            if let Some(token) = anim_state.frame_timer_token.take() {
                self.loop_handle.remove(token);
            }
        }
        self.animation_state = None;
    }

    /// Stop animation playback (no-op when animated feature is disabled).
    #[cfg(not(feature = "animated"))]
    pub fn stop_animation(&mut self) {
        // No animation support compiled in
    }

    pub fn load_images(&mut self) {
        let mut image_queue = VecDeque::new();
        let xdg_data_dirs: Vec<String> = match std::env::var("XDG_DATA_DIRS") {
            Ok(raw_xdg_data_dirs) => raw_xdg_data_dirs
                .split(':')
                .map(|s| format!("{}/backgrounds/", s))
                .collect(),
            Err(_) => Vec::new(),
        };

        match self.entry.source.clone() {
            Source::Path(source) => {
                // Check if this Path source points to a video/animated file
                #[cfg(feature = "animated")]
                if is_animated_file(&source) {
                    tracing::debug!(?source, "Animated file detected - initializing animation");
                    if self.init_animation(&source) {
                        self.current_source = Some(Source::Path(source));
                        return;
                    }
                    tracing::warn!(?source, "Failed to init animation, falling back to static");
                }

                // Without animated feature, skip video files (they cannot be rendered as static images)
                #[cfg(not(feature = "animated"))]
                if is_video_file(&source) {
                    tracing::debug!(
                        ?source,
                        "Video file in Path source - skipping (no animated feature)"
                    );
                    return;
                }

                tracing::debug!(?source, "loading images");

                if let Ok(source) = source.canonicalize() {
                    if source.is_dir() {
                        if xdg_data_dirs
                            .iter()
                            .any(|xdg_data_dir| source.starts_with(xdg_data_dir))
                        {
                            // Store paths of wallpapers to be used for the slideshow.
                            for img_path in WalkDir::new(source)
                                .follow_links(true)
                                .into_iter()
                                .filter_map(Result::ok)
                                .filter(|p| p.path().is_file())
                            {
                                image_queue.push_front(img_path.path().into());
                            }
                        } else if let Ok(dir) = source.read_dir() {
                            for entry in dir.filter_map(Result::ok) {
                                let Ok(path) = entry.path().canonicalize() else {
                                    continue;
                                };

                                if path.is_file() {
                                    image_queue.push_front(path);
                                }
                            }
                        }
                    } else if source.is_file() {
                        image_queue.push_front(source);
                    }
                }

                if image_queue.len() > 1 {
                    let image_slice = image_queue.make_contiguous();
                    match self.entry.sampling_method {
                        SamplingMethod::Alphanumeric => {
                            image_slice
                                .sort_by(|a, b| a.to_string_lossy().cmp(&b.to_string_lossy()));
                        }
                        SamplingMethod::Random => image_slice.shuffle(&mut rng()),
                    };

                    // If a wallpaper from this slideshow was previously set, resume with that wallpaper.
                    if let Some(Source::Path(last_path)) = current_image(&self.entry.output) {
                        if image_queue.contains(&last_path) {
                            while let Some(path) = image_queue.pop_front() {
                                if path == last_path {
                                    image_queue.push_front(path);
                                    break;
                                }

                                image_queue.push_back(path);
                            }
                        }
                    }
                }

                if let Some(current_image_path) = image_queue.pop_front() {
                    // Check if this is an animated file and initialize animation
                    #[cfg(feature = "animated")]
                    {
                        // For animated files, init_animation handles the setup
                        // For non-animated files, just set the source normally
                        let _ = is_animated_file(&current_image_path)
                            && self.init_animation(&current_image_path);
                    }

                    self.current_source = Some(Source::Path(current_image_path.clone()));
                    image_queue.push_back(current_image_path);
                }
            }

            Source::Color(ref c) => {
                self.current_source = Some(Source::Color(c.clone()));
            }
        };
        if let Err(err) = self.save_state() {
            error!("{err}");
        }
        self.image_queue = image_queue;
    }

    fn watch_source(&self, tx: calloop::channel::SyncSender<(String, notify::Event)>) {
        let Source::Path(ref source) = self.entry.source else {
            return;
        };

        let output = self.entry.output.clone();
        let mut watcher = match RecommendedWatcher::new(
            move |res| {
                if let Ok(e) = res {
                    let _ = tx.send((output.clone(), e));
                }
            },
            notify::Config::default(),
        ) {
            Ok(w) => w,
            Err(_) => return,
        };

        tracing::debug!(output = self.entry.output, "watching source");

        if let Ok(m) = fs::metadata(source) {
            if m.is_dir() {
                let _ = watcher.watch(source, RecursiveMode::Recursive);
            } else if m.is_file() {
                let _ = watcher.watch(source, RecursiveMode::NonRecursive);
            }
        }
    }

    fn register_timer(&mut self) {
        let rotation_freq = self.entry.rotation_frequency;
        let cosmic_bg_clone = self.entry.output.clone();
        // set timer for rotation
        if rotation_freq > 0 {
            self.timer_token = self
                .loop_handle
                .insert_source(
                    Timer::from_duration(Duration::from_secs(rotation_freq)),
                    move |_, _, state: &mut CosmicBg| {
                        let span = tracing::debug_span!("Wallpaper::timer");
                        let _handle = span.enter();

                        let Some(item) = state
                            .wallpapers
                            .iter_mut()
                            .find(|w| w.entry.output == cosmic_bg_clone)
                        else {
                            return TimeoutAction::Drop; // Drop if no item found for this timer
                        };

                        if let Some(next) = item.image_queue.pop_front() {
                            item.current_source = Some(Source::Path(next.clone()));
                            if let Err(err) = item.save_state() {
                                error!("{err}");
                            }

                            item.image_queue.push_back(next.clone());
                            item.clear_image();

                            // Check if the next image is an animated file
                            #[cfg(feature = "animated")]
                            if is_animated_file(&next) && item.init_animation(&next) {
                                item.draw();
                                return TimeoutAction::ToDuration(Duration::from_secs(
                                    rotation_freq,
                                ));
                            }

                            item.draw();

                            return TimeoutAction::ToDuration(Duration::from_secs(rotation_freq));
                        }

                        TimeoutAction::Drop
                    },
                )
                .ok();
        }
    }

    fn clear_image(&mut self) {
        // Stop any running animation
        #[cfg(feature = "animated")]
        self.stop_animation();

        self.current_image = None;
        for l in &mut self.layers {
            l.needs_redraw = true;
        }
    }
}

fn current_image(output: &str) -> Option<Source> {
    let state = State::state().ok()?;
    let mut wallpapers = State::get_entry(&state)
        .unwrap_or_default()
        .wallpapers
        .into_iter();

    let wallpaper = if output == "all" {
        wallpapers.next()
    } else {
        wallpapers.find(|(name, _path)| name == output)
    };

    wallpaper.map(|(_name, path)| path)
}

/// Decodes JPEG XL image files into `image::DynamicImage` via `jxl-oxide`.
fn decode_jpegxl(path: &std::path::Path) -> eyre::Result<DynamicImage> {
    let file = File::open(path).map_err(|why| eyre!("failed to open jxl image file: {why}"))?;

    let decoder =
        JxlDecoder::new(file).map_err(|why| eyre!("failed to read jxl image header: {why}"))?;

    image::DynamicImage::from_decoder(decoder)
        .map_err(|why| eyre!("failed to decode jxl image: {why}"))
}
