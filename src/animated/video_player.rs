// SPDX-License-Identifier: MPL-2.0

//! GStreamer-based hardware-accelerated video player.
//!
//! This module provides [`VideoPlayer`], which uses GStreamer for efficient
//! video decoding with support for:
//! - NVIDIA NVDEC (with optional cudadmabufupload for zero-copy)
//! - AMD/Intel VAAPI
//! - Software decode fallback
//!
//! ## Pipeline Priority
//!
//! 1. NVIDIA CUDA→DMA-BUF (optimal zero-copy)
//! 2. VAAPI DMA-BUF (AMD/Intel)
//! 3. NVIDIA GL DMA-BUF
//! 4. VAAPI wl_shm fallback
//! 5. NVIDIA GL wl_shm fallback
//! 6. Software decode

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use image::DynamicImage;
use tracing::{debug, error, info, warn};

use super::types::{AnimatedFrame, DEFAULT_FRAME_DURATION, MIN_FRAME_DURATION, VideoFrameInfo};

/// Shared state for receiving frames from GStreamer pipeline.
pub(crate) struct VideoFrameState {
    /// Most recent decoded frame.
    pub current_frame: Option<AnimatedFrame>,
    /// Frame duration from video metadata.
    pub frame_duration: Duration,
    /// Whether video has reached end of stream.
    pub eos: bool,
    /// Frame counter for FPS measurement.
    pub frame_count: u64,
}

/// Hardware-accelerated video player using GStreamer.
///
/// ## Architecture: Decoupled Frame Delivery
///
/// This player uses a decoupled architecture to prevent rendering stalls:
///
/// ```text
/// GStreamer (async) → FrameQueue (3 frames) → Renderer (non-blocking)
/// ```
///
/// Key design principles:
/// - **GStreamer callback never blocks**: Pushes to queue, drops oldest if full
/// - **Renderer never waits**: Pops from queue, reuses last frame if empty
/// - **appsink bounded buffering**: `max-buffers=4, drop=true` prevents pipeline stalls
/// - **Seek-based looping**: Seeks to start on EOS for seamless loop
pub struct VideoPlayer {
    /// GStreamer pipeline.
    pipeline: gstreamer::Pipeline,
    /// App sink for receiving frames.
    appsink: gstreamer_app::AppSink,
    /// Bounded frame queue for decoupled rendering.
    frame_queue: crate::frame_queue::SharedFrameQueue,
    /// Shared frame state (legacy, used for frame_duration).
    pub(crate) frame_state: Arc<Mutex<VideoFrameState>>,
    /// Source file path.
    source_path: PathBuf,
    /// Whether to loop playback.
    looping: bool,
    /// Number of times the pipeline has been rebuilt (for EOS looping).
    rebuild_count: std::sync::atomic::AtomicU32,
}

impl VideoPlayer {
    /// Log available hardware video decoders for debugging.
    fn log_available_decoders() {
        use gstreamer::prelude::*;

        let registry = gstreamer::Registry::get();

        let hw_decoders = [
            ("cudadmabufupload", "CUDA→DMA-BUF (NVIDIA zero-copy)"),
            ("nvh264dec", "NVDEC H.264 (NVIDIA)"),
            ("nvh265dec", "NVDEC H.265/HEVC (NVIDIA)"),
            ("nvvp9dec", "NVDEC VP9 (NVIDIA)"),
            ("nvav1dec", "NVDEC AV1 (NVIDIA)"),
            ("vaapih264dec", "VAAPI H.264 (AMD/Intel)"),
            ("vaapih265dec", "VAAPI H.265/HEVC (AMD/Intel)"),
            ("vaapivp9dec", "VAAPI VP9 (AMD/Intel)"),
            ("vaapiav1dec", "VAAPI AV1 (AMD/Intel)"),
            ("vaapipostproc", "VAAPI Post-Processing (AMD/Intel)"),
            ("v4l2h264dec", "V4L2 H.264 (ARM)"),
            ("v4l2h265dec", "V4L2 H.265/HEVC (ARM)"),
        ];

        let mut available = Vec::new();
        for (element_name, description) in hw_decoders {
            if registry
                .find_feature(element_name, gstreamer::ElementFactory::static_type())
                .is_some()
            {
                available.push(description);
            }
        }

        if available.is_empty() {
            warn!(
                "No hardware video decoders found. Video will use software decoding. \
                 Install gstreamer1-vaapi (AMD/Intel) or gstreamer1-plugins-bad (NVIDIA) for hardware acceleration."
            );
        } else {
            info!(decoders = ?available, "Available hardware video decoders");
        }
    }

    /// Create a new video player for the given path.
    pub fn new(path: &Path, target_width: u32, target_height: u32) -> eyre::Result<Self> {
        use gstreamer::prelude::*;

        gstreamer::init()?;

        static LOGGED_DECODERS: std::sync::Once = std::sync::Once::new();
        LOGGED_DECODERS.call_once(Self::log_available_decoders);

        let path_str = path
            .to_str()
            .ok_or_else(|| eyre::eyre!("Invalid path: {}", path.display()))?;

        debug!(
            path = %path.display(),
            width = target_width,
            height = target_height,
            "Creating GStreamer video player with GPU scaling"
        );

        let test_pipeline = |pipeline_str: &str| -> bool {
            match gstreamer::parse::launch(pipeline_str) {
                Ok(p) => {
                    let result = p.set_state(gstreamer::State::Paused);
                    if result.is_err() {
                        let _ = p.set_state(gstreamer::State::Null);
                        return false;
                    }
                    let (res, state, _) = p.state(gstreamer::ClockTime::from_mseconds(500));
                    let _ = p.set_state(gstreamer::State::Null);
                    res.is_ok() && state == gstreamer::State::Paused
                }
                Err(_) => false,
            }
        };

        let escaped_path = path_str.replace('\\', "\\\\").replace('"', "\\\"");

        let has_vaapi = gstreamer::ElementFactory::find("vaapipostproc").is_some();
        let has_nvdec = gstreamer::ElementFactory::find("nvh264dec").is_some();
        let has_cuda_dmabuf = gstreamer::ElementFactory::find("cudadmabufupload").is_some();

        if has_cuda_dmabuf {
            info!(
                "NVIDIA CUDA→DMA-BUF plugin (cudadmabufupload) detected - optimal zero-copy path available"
            );
        }

        let _try_dmabuf = true;

        // Try pipelines in priority order
        let pipeline_str = Self::try_cuda_dmabuf_pipeline(
            path,
            &escaped_path,
            has_cuda_dmabuf,
            has_nvdec,
            _try_dmabuf,
            &test_pipeline,
            target_width,
            target_height,
        )
        .or_else(|| {
            Self::try_vaapi_dmabuf_pipeline(
                &escaped_path,
                has_vaapi,
                _try_dmabuf,
                &test_pipeline,
                target_width,
                target_height,
            )
        })
        .or_else(|| {
            Self::try_nvdec_gl_dmabuf_pipeline(
                &escaped_path,
                has_nvdec,
                _try_dmabuf,
                &test_pipeline,
                target_width,
                target_height,
            )
        })
        .or_else(|| {
            Self::try_vaapi_wlshm_pipeline(
                &escaped_path,
                has_vaapi,
                &test_pipeline,
                target_width,
                target_height,
            )
        })
        .or_else(|| Self::try_nvdec_gl_wlshm_pipeline(&escaped_path, has_nvdec, &test_pipeline))
        .or_else(|| Self::try_gl_pipeline(&escaped_path, &test_pipeline))
        .unwrap_or_else(|| {
            Self::software_fallback_pipeline(&escaped_path, target_width, target_height)
        });

        debug!(pipeline = %pipeline_str, "Creating GStreamer pipeline");

        let pipeline = gstreamer::parse::launch(&pipeline_str)?
            .downcast::<gstreamer::Pipeline>()
            .map_err(|_| eyre::eyre!("Failed to create pipeline"))?;

        let appsink = pipeline
            .by_name("sink")
            .ok_or_else(|| eyre::eyre!("Failed to get appsink from pipeline"))?
            .downcast::<gstreamer_app::AppSink>()
            .map_err(|_| eyre::eyre!("Element 'sink' is not an AppSink"))?;

        let initial_frame_duration =
            Self::detect_framerate(&pipeline, &appsink, target_width, target_height);

        let frame_state = Arc::new(Mutex::new(VideoFrameState {
            current_frame: None,
            frame_duration: initial_frame_duration,
            eos: false,
            frame_count: 0,
        }));

        let frame_queue = crate::frame_queue::new_shared_queue(3);

        Self::setup_appsink_callback(&appsink, Arc::clone(&frame_queue), Arc::clone(&frame_state));

        Ok(Self {
            pipeline,
            appsink,
            frame_queue,
            frame_state,
            source_path: path.to_path_buf(),
            looping: true,
            rebuild_count: std::sync::atomic::AtomicU32::new(0),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn try_cuda_dmabuf_pipeline(
        path: &Path,
        escaped_path: &str,
        has_cuda_dmabuf: bool,
        has_nvdec: bool,
        try_dmabuf: bool,
        test_pipeline: &impl Fn(&str) -> bool,
        target_width: u32,
        target_height: u32,
    ) -> Option<String> {
        if !try_dmabuf || !has_cuda_dmabuf || !has_nvdec {
            return None;
        }

        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();

        let pipeline = match ext.as_str() {
            "mp4" | "m4v" | "mov" => format!(
                concat!(
                    "filesrc location=\"{path}\" ! ",
                    "qtdemux ! h264parse ! ",
                    "nvh264dec ! ",
                    "video/x-raw(memory:CUDAMemory) ! ",
                    "cudadmabufupload ! ",
                    "video/x-raw(memory:DMABuf) ! ",
                    "appsink name=sink sync=true max-buffers=4 drop=true"
                ),
                path = escaped_path,
            ),
            "webm" => format!(
                concat!(
                    "filesrc location=\"{path}\" ! ",
                    "matroskademux ! ",
                    "nvvp9dec ! ",
                    "video/x-raw(memory:CUDAMemory) ! ",
                    "cudadmabufupload ! ",
                    "video/x-raw(memory:DMABuf) ! ",
                    "appsink name=sink sync=true max-buffers=4 drop=true"
                ),
                path = escaped_path,
            ),
            "mkv" => format!(
                concat!(
                    "filesrc location=\"{path}\" ! ",
                    "matroskademux ! h264parse ! ",
                    "nvh264dec ! ",
                    "video/x-raw(memory:CUDAMemory) ! ",
                    "cudadmabufupload ! ",
                    "video/x-raw(memory:DMABuf) ! ",
                    "appsink name=sink sync=true max-buffers=4 drop=true"
                ),
                path = escaped_path,
            ),
            _ => format!(
                concat!(
                    "filesrc location=\"{path}\" ! ",
                    "decodebin ! ",
                    "video/x-raw(memory:CUDAMemory) ! ",
                    "cudadmabufupload ! ",
                    "video/x-raw(memory:DMABuf) ! ",
                    "appsink name=sink sync=true max-buffers=4 drop=true"
                ),
                path = escaped_path,
            ),
        };

        debug!(pipeline = %pipeline, "Trying NVIDIA CUDA→DMA-BUF zero-copy pipeline");

        if test_pipeline(&pipeline) {
            info!(
                "🚀 NVIDIA CUDA→DMA-BUF zero-copy pipeline ACTIVE - maximum performance ({}x{})",
                target_width, target_height
            );
            Some(pipeline)
        } else {
            debug!("CUDA→DMA-BUF pipeline failed");
            None
        }
    }

    fn try_vaapi_dmabuf_pipeline(
        escaped_path: &str,
        has_vaapi: bool,
        try_dmabuf: bool,
        test_pipeline: &impl Fn(&str) -> bool,
        target_width: u32,
        target_height: u32,
    ) -> Option<String> {
        if !try_dmabuf || !has_vaapi {
            return None;
        }

        let pipeline = format!(
            concat!(
                "filesrc location=\"{path}\" ! ",
                "decodebin name=dec ! ",
                "videorate drop-only=true max-rate=60 ! video/x-raw,framerate=60/1 ! ",
                "vapostproc ! ",
                "video/x-raw(memory:DMABuf),format=BGRx ! ",
                "appsink name=sink sync=true max-buffers=4 drop=true"
            ),
            path = escaped_path,
        );

        debug!(pipeline = %pipeline, "Trying VAAPI DMA-BUF zero-copy pipeline");

        if test_pipeline(&pipeline) {
            info!(
                "VAAPI DMA-BUF zero-copy pipeline active ({}x{})",
                target_width, target_height
            );
            Some(pipeline)
        } else {
            debug!("VAAPI DMA-BUF pipeline failed");
            None
        }
    }

    fn try_nvdec_gl_dmabuf_pipeline(
        escaped_path: &str,
        has_nvdec: bool,
        try_dmabuf: bool,
        test_pipeline: &impl Fn(&str) -> bool,
        target_width: u32,
        target_height: u32,
    ) -> Option<String> {
        if !try_dmabuf || !has_nvdec {
            return None;
        }

        let pipeline = format!(
            concat!(
                "filesrc location=\"{path}\" ! ",
                "decodebin name=dec ! ",
                "nvh264dec ! ",
                "glcolorconvert ! ",
                "video/x-raw(memory:GLMemory),format=RGBA ! ",
                "gldownload ! ",
                "video/x-raw(memory:DMABuf),format=RGBA ! ",
                "appsink name=sink sync=true max-buffers=4 drop=true"
            ),
            path = escaped_path,
        );

        debug!(pipeline = %pipeline, "Trying NVIDIA NVDEC GL DMA-BUF zero-copy pipeline");

        if test_pipeline(&pipeline) {
            info!(
                "NVIDIA NVDEC GL DMA-BUF zero-copy pipeline active ({}x{})",
                target_width, target_height
            );
            Some(pipeline)
        } else {
            debug!("NVDEC GL DMA-BUF pipeline failed");
            None
        }
    }

    fn try_vaapi_wlshm_pipeline(
        escaped_path: &str,
        has_vaapi: bool,
        test_pipeline: &impl Fn(&str) -> bool,
        target_width: u32,
        target_height: u32,
    ) -> Option<String> {
        if !has_vaapi {
            return None;
        }

        let pipeline = format!(
            concat!(
                "filesrc location=\"{path}\" ! ",
                "decodebin name=dec ! ",
                "videorate drop-only=true max-rate=60 ! video/x-raw,framerate=60/1 ! ",
                "vapostproc ! ",
                "video/x-raw,format=BGRx ! ",
                "appsink name=sink sync=true max-buffers=4 drop=true"
            ),
            path = escaped_path,
        );

        debug!(pipeline = %pipeline, "Trying VAAPI + vapostproc pipeline");

        if test_pipeline(&pipeline) {
            debug!(
                "VAAPI pipeline verified ({}x{})",
                target_width, target_height
            );
            Some(pipeline)
        } else {
            debug!("VAAPI + vapostproc pipeline failed");
            None
        }
    }

    fn try_nvdec_gl_wlshm_pipeline(
        escaped_path: &str,
        has_nvdec: bool,
        test_pipeline: &impl Fn(&str) -> bool,
    ) -> Option<String> {
        if !has_nvdec {
            return None;
        }

        let pipeline = format!(
            concat!(
                "filesrc location=\"{path}\" ! ",
                "decodebin name=dec ! ",
                "videoconvert ! ",
                "videorate drop-only=true max-rate=60 ! video/x-raw,framerate=60/1 ! ",
                "glupload ! ",
                "glcolorconvert ! video/x-raw(memory:GLMemory),format=BGRx ! ",
                "gldownload ! ",
                "appsink name=sink sync=true max-buffers=4 drop=true"
            ),
            path = escaped_path,
        );

        debug!(pipeline = %pipeline, "Trying NVDEC + GL pipeline");

        if test_pipeline(&pipeline) {
            debug!("NVDEC pipeline verified");
            Some(pipeline)
        } else {
            debug!("NVDEC + GL pipeline failed");
            None
        }
    }

    fn try_gl_pipeline(
        escaped_path: &str,
        test_pipeline: &impl Fn(&str) -> bool,
    ) -> Option<String> {
        let pipeline = format!(
            concat!(
                "filesrc location=\"{path}\" ! ",
                "decodebin name=dec ! ",
                "videorate drop-only=true max-rate=60 ! video/x-raw,framerate=60/1 ! ",
                "glupload ! ",
                "glcolorconvert ! video/x-raw(memory:GLMemory),format=BGRx ! ",
                "gldownload ! ",
                "appsink name=sink sync=true max-buffers=4 drop=true"
            ),
            path = escaped_path,
        );

        debug!(pipeline = %pipeline, "Trying decodebin + GL pipeline");

        if test_pipeline(&pipeline) {
            debug!("Decodebin + GL pipeline verified");
            Some(pipeline)
        } else {
            debug!("GL pipeline failed");
            None
        }
    }

    fn software_fallback_pipeline(
        escaped_path: &str,
        target_width: u32,
        target_height: u32,
    ) -> String {
        let pipeline = format!(
            concat!(
                "filesrc location=\"{path}\" ! ",
                "decodebin ! ",
                "videoconvert ! ",
                "video/x-raw,format=BGRx ! ",
                "appsink name=sink sync=true max-buffers=4 drop=true"
            ),
            path = escaped_path,
        );

        debug!(
            pipeline = %pipeline,
            "Using software decodebin fallback ({}x{})",
            target_width, target_height
        );
        pipeline
    }

    fn detect_framerate(
        pipeline: &gstreamer::Pipeline,
        appsink: &gstreamer_app::AppSink,
        target_width: u32,
        target_height: u32,
    ) -> Duration {
        use gstreamer::prelude::*;

        debug!("Setting pipeline to PAUSED to detect framerate");

        if pipeline.set_state(gstreamer::State::Paused).is_err() {
            debug!("Failed to set pipeline to PAUSED");
            return DEFAULT_FRAME_DURATION;
        }

        let (result, _, _) = pipeline.state(gstreamer::ClockTime::from_mseconds(500));
        if result.is_err() {
            debug!("Pipeline failed to reach PAUSED state");
            return DEFAULT_FRAME_DURATION;
        }

        debug!("Pipeline reached PAUSED state, querying caps");

        let Some(pad) = appsink.static_pad("sink") else {
            debug!("No sink pad on appsink");
            return DEFAULT_FRAME_DURATION;
        };

        let Some(caps) = pad.current_caps() else {
            debug!("No current caps on pad");
            return DEFAULT_FRAME_DURATION;
        };

        debug!(caps = %caps, "Got current caps from pad");

        let Some(structure) = caps.structure(0) else {
            debug!("No structure in caps");
            return DEFAULT_FRAME_DURATION;
        };

        // Log video resolution
        if let (Some(w), Some(h)) = (
            structure.get::<i32>("width").ok(),
            structure.get::<i32>("height").ok(),
        ) {
            info!(
                target_resolution = format!("{}x{}", target_width, target_height),
                pipeline_resolution = format!("{}x{}", w, h),
                "Video resolution configuration"
            );
        }

        if let Ok(framerate) = structure.get::<gstreamer::Fraction>("framerate") {
            if framerate.numer() > 0 && framerate.denom() > 0 {
                let detected_duration = Duration::from_secs_f64(
                    f64::from(framerate.denom()) / f64::from(framerate.numer()),
                );
                info!(
                    fps = format!("{}/{}", framerate.numer(), framerate.denom()),
                    duration_ms = detected_duration.as_millis(),
                    "Detected video framerate"
                );
                return detected_duration.max(MIN_FRAME_DURATION);
            }
        }

        debug!("No framerate field in caps");
        DEFAULT_FRAME_DURATION
    }

    fn setup_appsink_callback(
        appsink: &gstreamer_app::AppSink,
        frame_queue: crate::frame_queue::SharedFrameQueue,
        frame_state: Arc<Mutex<VideoFrameState>>,
    ) {
        appsink.set_callbacks(
            gstreamer_app::AppSinkCallbacks::builder()
                .new_sample(move |appsink| Self::handle_sample(appsink, &frame_queue, &frame_state))
                .build(),
        );
    }

    fn handle_sample(
        appsink: &gstreamer_app::AppSink,
        frame_queue: &crate::frame_queue::SharedFrameQueue,
        frame_state: &Arc<Mutex<VideoFrameState>>,
    ) -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
        let sample = match appsink.pull_sample() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Callback pull_sample failed: {:?}", e);
                return Ok(gstreamer::FlowSuccess::Ok);
            }
        };

        let Some(buffer) = sample.buffer() else {
            return Ok(gstreamer::FlowSuccess::Ok);
        };

        let pts_ns = buffer.pts().map(|p| p.nseconds());

        let Some(caps) = sample.caps() else {
            return Ok(gstreamer::FlowSuccess::Ok);
        };

        let Ok(video_info) = gstreamer_video::VideoInfo::from_caps(caps) else {
            return Ok(gstreamer::FlowSuccess::Ok);
        };

        let width = video_info.width();
        let height = video_info.height();

        // Check for DMA-BUF memory first (zero-copy path)
        let frame = if buffer.n_memory() > 0 {
            let mem = buffer.memory(0).unwrap();

            if let Some(dmabuf_mem) =
                mem.downcast_memory_ref::<gstreamer_allocators::DmaBufMemory>()
            {
                Self::create_dmabuf_frame(&sample, buffer, dmabuf_mem, caps, width, height, pts_ns)
            } else {
                None
            }
        } else {
            None
        };

        let frame = match frame {
            Some(f) => f,
            None => {
                // Fallback: Map buffer and copy
                if let Ok(map) = buffer.map_readable() {
                    crate::frame_queue::QueuedFrame::new(
                        map.as_slice().to_vec(),
                        width,
                        height,
                        pts_ns,
                    )
                } else {
                    tracing::trace!("Skipped frame: buffer map blocked");
                    return Ok(gstreamer::FlowSuccess::Ok);
                }
            }
        };

        if !frame_queue.push(frame) {
            return Ok(gstreamer::FlowSuccess::Ok);
        }

        if let Ok(mut state) = frame_state.try_lock() {
            state.frame_count += 1;
            if state.frame_count % 60 == 0 {
                let stats = frame_queue.stats();
                tracing::info!(
                    frames = state.frame_count,
                    queue_len = frame_queue.len(),
                    pushed = stats.frames_pushed,
                    popped = stats.frames_popped,
                    dropped = stats.frames_dropped_full,
                    reused = stats.frames_reused,
                    "Video playback progress"
                );
            }
        }

        Ok(gstreamer::FlowSuccess::Ok)
    }

    fn create_dmabuf_frame(
        _sample: &gstreamer::Sample,
        buffer: &gstreamer::BufferRef,
        dmabuf_mem: &gstreamer_allocators::DmaBufMemoryRef,
        caps: &gstreamer::CapsRef,
        width: u32,
        height: u32,
        pts_ns: Option<u64>,
    ) -> Option<crate::frame_queue::QueuedFrame> {
        let fd = dmabuf_mem.fd();
        let video_meta = buffer.meta::<gstreamer_video::VideoMeta>();

        let format_str = caps
            .structure(0)
            .and_then(|s| s.get::<String>("drm-format").ok())
            .or_else(|| {
                caps.structure(0)
                    .and_then(|s| s.get::<String>("format").ok())
            })
            .unwrap_or_else(|| "NV12".to_string());

        let dmabuf_format = crate::dmabuf::DmaBufFormat::from_gst_format(&format_str);
        let is_nv12 = format_str.starts_with("NV12")
            || dmabuf_format.fourcc == drm_fourcc::DrmFourcc::Nv12 as u32;

        let (strides, offsets) = if let Some(meta) = &video_meta {
            let s: Vec<u32> = meta.stride().iter().map(|&x| x as u32).collect();
            let o: Vec<u32> = meta.offset().iter().map(|&x| x as u32).collect();
            tracing::debug!(strides = ?s, offsets = ?o, "VideoMeta plane info");
            (s, o)
        } else {
            let aligned_width = (width + 63) & !63;
            let y_size = aligned_width * height;
            if is_nv12 {
                (vec![aligned_width, aligned_width], vec![0, y_size])
            } else {
                (vec![aligned_width * 4], vec![0])
            }
        };

        tracing::debug!(
            fd = fd,
            format = %format_str,
            ?strides,
            ?offsets,
            "Zero-copy DMA-BUF frame"
        );

        let dmabuf_result = if is_nv12 && strides.len() >= 2 && offsets.len() >= 2 {
            crate::frame_queue::DmaBufFrameData::from_raw_fd_nv12_with_offsets(
                fd,
                dmabuf_format.fourcc,
                dmabuf_format.modifier,
                width,
                height,
                strides[0],
                strides[1],
                offsets[0],
                offsets[1],
            )
        } else if is_nv12 {
            crate::frame_queue::DmaBufFrameData::from_raw_fd_nv12(
                fd,
                dmabuf_format.fourcc,
                dmabuf_format.modifier,
                width,
                height,
                strides.first().copied().unwrap_or(width),
            )
        } else {
            crate::frame_queue::DmaBufFrameData::from_raw_fd(
                fd,
                dmabuf_format.fourcc,
                dmabuf_format.modifier,
                strides.first().copied().unwrap_or(width * 4),
            )
        };

        match dmabuf_result {
            Ok(mut dmabuf_data) => {
                dmabuf_data.width = width;
                dmabuf_data.height = height;
                Some(crate::frame_queue::QueuedFrame::new_dmabuf(
                    dmabuf_data,
                    width,
                    height,
                    pts_ns,
                ))
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to dup DMA-BUF fd");
                None
            }
        }
    }

    /// Pull the next available frame from the pipeline (non-blocking).
    pub fn pull_frame(&self) -> Option<AnimatedFrame> {
        match self
            .appsink
            .try_pull_sample(gstreamer::ClockTime::from_mseconds(17))
        {
            Some(sample) => {
                debug!("Successfully pulled sample from appsink");
                self.process_sample(&sample)
            }
            None => None,
        }
    }

    /// Pull a frame and write it directly to a destination buffer.
    pub fn pull_frame_to_buffer(&self, dest: &mut [u8]) -> Option<VideoFrameInfo> {
        if let Some((width, height)) = self.frame_queue.write_frame_to(dest) {
            return Some(VideoFrameInfo {
                width,
                height,
                is_bgrx: true,
            });
        }
        None
    }

    fn process_sample(&self, sample: &gstreamer::Sample) -> Option<AnimatedFrame> {
        let buffer = sample.buffer()?;
        let caps = sample.caps()?;
        let video_info = gstreamer_video::VideoInfo::from_caps(caps).ok()?;

        let width = video_info.width();
        let height = video_info.height();

        let fps = video_info.fps();
        let frame_duration = if fps.numer() > 0 && fps.denom() > 0 {
            Duration::from_secs_f64(f64::from(fps.denom()) / f64::from(fps.numer()))
        } else {
            DEFAULT_FRAME_DURATION
        };

        let pts = buffer.pts().map(|p| p.nseconds());
        let map = buffer.map_readable().ok()?;
        let data = map.as_slice();

        let expected_size = (width * height * 4) as usize;
        if data.len() < expected_size {
            error!(
                data_len = data.len(),
                expected = expected_size,
                "Buffer size mismatch"
            );
            return None;
        }

        let image_buffer =
            image::RgbaImage::from_raw(width, height, data[..expected_size].to_vec())?;
        let image = DynamicImage::ImageRgba8(image_buffer);

        let frame = AnimatedFrame {
            image,
            duration: frame_duration.max(MIN_FRAME_DURATION),
            pts,
        };

        if let Ok(mut state) = self.frame_state.lock() {
            state.current_frame = Some(frame.clone());
            state.frame_duration = frame_duration;
        }

        Some(frame)
    }

    /// Start video playback.
    pub fn play(&self) -> eyre::Result<()> {
        use gstreamer::prelude::*;
        self.pipeline
            .set_state(gstreamer::State::Playing)
            .map_err(|e| eyre::eyre!("Failed to start pipeline: {:?}", e))?;
        Ok(())
    }

    /// Stop video playback.
    pub fn stop(&self) -> eyre::Result<()> {
        use gstreamer::prelude::*;
        let _ = self.pipeline.set_state(gstreamer::State::Null);
        Ok(())
    }

    /// Seek to the beginning for looping.
    pub fn seek_to_start(&self) -> eyre::Result<()> {
        use gstreamer::prelude::*;

        let seek_flags = gstreamer::SeekFlags::FLUSH
            | gstreamer::SeekFlags::KEY_UNIT
            | gstreamer::SeekFlags::SNAP_BEFORE;

        self.pipeline
            .seek_simple(seek_flags, gstreamer::ClockTime::ZERO)?;

        if let Ok(mut state) = self.frame_state.lock() {
            state.eos = false;
        }

        Ok(())
    }

    /// Get the current frame if available.
    #[must_use]
    pub fn current_frame(&self) -> Option<AnimatedFrame> {
        match self.pull_frame() {
            Some(frame) => Some(frame),
            None => self
                .frame_state
                .lock()
                .ok()
                .and_then(|state| state.current_frame.clone()),
        }
    }

    /// Get the frame duration.
    #[must_use]
    pub fn frame_duration(&self) -> Duration {
        self.frame_state
            .lock()
            .ok()
            .map(|state| state.frame_duration)
            .unwrap_or(DEFAULT_FRAME_DURATION)
    }

    /// Get video dimensions.
    #[must_use]
    pub fn video_dimensions(&self) -> Option<(u32, u32)> {
        if let Some(dims) = self.frame_queue.last_frame_dimensions() {
            return Some(dims);
        }

        use gstreamer::prelude::*;
        let pad = self.appsink.static_pad("sink")?;
        let caps = pad.current_caps()?;
        let video_info = gstreamer_video::VideoInfo::from_caps(&caps).ok()?;
        Some((video_info.width(), video_info.height()))
    }

    /// Pull last cached frame.
    pub fn pull_cached_frame(&self, dest: &mut [u8]) -> Option<VideoFrameInfo> {
        self.pull_frame_to_buffer(dest)
    }

    /// Try to get a DMA-BUF frame for zero-copy rendering.
    #[must_use]
    pub fn try_get_dmabuf_frame(&self) -> Option<crate::dmabuf::DmaBufBuffer> {
        use std::sync::Arc;

        let frame = self.frame_queue.get_render_frame()?;
        let dmabuf_data = frame.dmabuf()?;

        tracing::info!(
            width = frame.width,
            height = frame.height,
            fourcc = format!("{:#x}", dmabuf_data.fourcc),
            modifier = format!("{:#x}", dmabuf_data.modifier),
            "Got DMA-BUF frame - TRUE ZERO-COPY!"
        );

        let mut planes = Vec::with_capacity(dmabuf_data.planes.len());
        for plane_data in &dmabuf_data.planes {
            use std::os::fd::AsFd;
            let fd = plane_data.fd.as_fd().try_clone_to_owned().ok()?;
            planes.push(crate::dmabuf::DmaBufPlane {
                fd: Arc::new(fd),
                offset: plane_data.offset,
                stride: plane_data.stride,
            });
        }

        Some(crate::dmabuf::DmaBufBuffer {
            width: frame.width,
            height: frame.height,
            format: crate::dmabuf::DmaBufFormat {
                fourcc: dmabuf_data.fourcc,
                modifier: dmabuf_data.modifier,
            },
            planes,
            wl_buffer: None,
        })
    }

    /// Check for EOS and handle looping.
    pub fn check_eos(&mut self) -> bool {
        use gstreamer::prelude::*;

        let Some(bus) = self.pipeline.bus() else {
            return false;
        };

        while let Some(msg) = bus.pop() {
            use gstreamer::MessageView;

            match msg.view() {
                MessageView::Eos(_) => {
                    if self.looping {
                        let loop_num = self
                            .rebuild_count
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                            + 1;
                        debug!(loop_num, path = %self.source_path.display(), "Video EOS, seeking to start");

                        if let Err(e) = self.seek_to_start() {
                            error!(?e, "Failed to seek to start for loop");
                            return true;
                        }
                        return false;
                    }
                    return true;
                }
                MessageView::Error(err) => {
                    error!(
                        src = ?err.src().map(|s| s.path_string()),
                        error = %err.error(),
                        "GStreamer pipeline error"
                    );
                    return true;
                }
                MessageView::Warning(warn) => {
                    warn!(
                        src = ?warn.src().map(|s| s.path_string()),
                        error = %warn.error(),
                        "GStreamer pipeline warning"
                    );
                }
                MessageView::StateChanged(state) => {
                    if state.src().map(|s| s == &self.pipeline).unwrap_or(false) {
                        debug!(old = ?state.old(), new = ?state.current(), "Pipeline state changed");
                    }
                }
                _ => {}
            }
        }

        false
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        if let Err(e) = self.stop() {
            error!(?e, "Failed to stop video pipeline on drop");
        }
    }
}
