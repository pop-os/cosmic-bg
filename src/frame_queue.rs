// SPDX-License-Identifier: MPL-2.0

//! Bounded frame queue for decoupled video playback.
//!
//! This module provides a bounded frame queue that decouples GStreamer's
//! decoding thread from the Wayland rendering thread:
//!
//! ```text
//! ┌─────────────┐
//! │ GStreamer   │
//! │ decode      │
//! └─────┬───────┘
//!       │ push() - drops oldest if full
//!       ▼
//! ┌─────────────┐
//! │ Frame Queue │  ← bounded (2-4 frames)
//! └─────┬───────┘
//!       │ get_render_frame() - reuses last frame if empty
//!       ▼
//! ┌─────────────┐
//! │ Renderer    │
//! └─────────────┘
//! ```
//!
//! # Key Guarantees
//!
//! - **Renderer never blocks**: Returns immediately, reuses last frame if empty
//! - **Producer never blocks**: Drops oldest frame if queue is full
//! - **Frame drops are invisible**: Wallpapers don't need perfect frame accuracy
//!
//! # Zero-Copy DMA-BUF Support
//!
//! For hardware-accelerated video, frames can be stored as DMA-BUF file
//! descriptors. The renderer imports these directly via zwp_linux_dmabuf_v1.

use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Maximum number of frames to buffer.
///
/// 3-4 frames provides enough buffer to hide decode hiccups while keeping
/// latency low. More frames = more memory usage and higher latency.
pub const DEFAULT_QUEUE_CAPACITY: usize = 3;

/// Per-plane data for DMA-BUF frames.
#[derive(Debug, Clone)]
pub struct DmaBufPlaneData {
    /// File descriptor for this plane (shared Arc for multi-plane in same buffer)
    pub fd: Arc<OwnedFd>,
    /// Offset into the buffer for this plane
    pub offset: u32,
    /// Bytes per row for this plane
    pub stride: u32,
}

/// DMA-BUF frame data for zero-copy GPU rendering.
///
/// This holds the file descriptor and metadata needed to import the frame
/// directly into the compositor via zwp_linux_dmabuf_v1, bypassing CPU entirely.
///
/// Supports multi-plane formats like NV12 (Y + UV planes).
#[derive(Debug)]
pub struct DmaBufFrameData {
    /// DRM fourcc format code (e.g., NV12, XRGB8888)
    pub fourcc: u32,
    /// DRM modifier (e.g., LINEAR, NVIDIA tiled)
    pub modifier: u64,
    /// Per-plane data (1 for RGB formats, 2 for NV12/NV21, 3 for YUV420P)
    pub planes: Vec<DmaBufPlaneData>,
    /// Frame width (needed for calculating plane offsets)
    pub width: u32,
    /// Frame height (needed for calculating plane offsets)
    pub height: u32,
}

impl Clone for DmaBufFrameData {
    fn clone(&self) -> Self {
        Self {
            fourcc: self.fourcc,
            modifier: self.modifier,
            planes: self.planes.clone(),
            width: self.width,
            height: self.height,
        }
    }
}

impl DmaBufFrameData {
    /// Create new DMA-BUF frame data from a raw fd for single-plane formats.
    ///
    /// SAFETY: The fd is duplicated, so caller can keep using the original.
    pub fn from_raw_fd(
        raw_fd: i32,
        fourcc: u32,
        modifier: u64,
        stride: u32,
    ) -> std::io::Result<Self> {
        // Duplicate the fd so we own it independently
        let dup_fd = nix::unistd::dup(raw_fd)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        let fd = Arc::new(unsafe { OwnedFd::from_raw_fd(dup_fd) });

        Ok(Self {
            fourcc,
            modifier,
            planes: vec![DmaBufPlaneData {
                fd,
                offset: 0,
                stride,
            }],
            width: 0,  // Set by caller
            height: 0, // Set by caller
        })
    }

    /// Create NV12 DMA-BUF frame data with two planes (Y + UV).
    ///
    /// For NVIDIA tiled NV12, both planes share the same fd with different offsets.
    /// - Plane 0 (Y): offset = 0, stride = aligned_width
    /// - Plane 1 (UV): offset = aligned_stride * height, stride = aligned_width
    pub fn from_raw_fd_nv12(
        raw_fd: i32,
        fourcc: u32,
        modifier: u64,
        width: u32,
        height: u32,
        y_stride: u32,
    ) -> std::io::Result<Self> {
        // Duplicate the fd so we own it independently
        let dup_fd = nix::unistd::dup(raw_fd)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        let fd = Arc::new(unsafe { OwnedFd::from_raw_fd(dup_fd) });

        // For NVIDIA tiled NV12:
        // - Y plane at offset 0
        // - UV plane at offset = y_stride * height (UV is half-height, interleaved)
        let uv_offset = y_stride * height;

        tracing::debug!(
            y_stride,
            uv_offset,
            width,
            height,
            "Creating NV12 DMA-BUF with 2 planes"
        );

        Ok(Self {
            fourcc,
            modifier,
            planes: vec![
                DmaBufPlaneData {
                    fd: Arc::clone(&fd),
                    offset: 0,
                    stride: y_stride,
                },
                DmaBufPlaneData {
                    fd,
                    offset: uv_offset,
                    stride: y_stride, // UV has same stride as Y for NV12
                },
            ],
            width,
            height,
        })
    }

    /// Create NV12 DMA-BUF frame data with explicit offsets from VideoMeta.
    ///
    /// This is the preferred method when GStreamer provides VideoMeta with
    /// actual plane offsets, which is required for NVIDIA tiled formats.
    #[allow(clippy::too_many_arguments)]
    pub fn from_raw_fd_nv12_with_offsets(
        raw_fd: i32,
        fourcc: u32,
        modifier: u64,
        width: u32,
        height: u32,
        y_stride: u32,
        uv_stride: u32,
        y_offset: u32,
        uv_offset: u32,
    ) -> std::io::Result<Self> {
        // Duplicate the fd so we own it independently
        let dup_fd = nix::unistd::dup(raw_fd)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        let fd = Arc::new(unsafe { OwnedFd::from_raw_fd(dup_fd) });

        tracing::debug!(
            y_stride,
            uv_stride,
            y_offset,
            uv_offset,
            width,
            height,
            "Creating NV12 DMA-BUF with 2 planes (explicit offsets from VideoMeta)"
        );

        Ok(Self {
            fourcc,
            modifier,
            planes: vec![
                DmaBufPlaneData {
                    fd: Arc::clone(&fd),
                    offset: y_offset,
                    stride: y_stride,
                },
                DmaBufPlaneData {
                    fd,
                    offset: uv_offset,
                    stride: uv_stride,
                },
            ],
            width,
            height,
        })
    }
}

/// Frame content - either raw pixel data or DMA-BUF reference.
#[derive(Clone)]
pub enum FrameContent {
    /// Raw pixel data in memory (BGRx format for wl_shm)
    Raw(Vec<u8>),
    /// DMA-BUF for zero-copy GPU rendering
    DmaBuf(DmaBufFrameData),
}

impl std::fmt::Debug for FrameContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Raw(data) => write!(f, "Raw({} bytes)", data.len()),
            Self::DmaBuf(dmabuf) => write!(
                f,
                "DmaBuf(fourcc={:#x}, modifier={:#x})",
                dmabuf.fourcc, dmabuf.modifier
            ),
        }
    }
}

/// A video frame stored in the queue.
///
/// Supports two modes:
/// - **Raw**: Pixel data in BGRx format, copied to wl_shm buffer
/// - **DmaBuf**: Zero-copy GPU buffer, imported via zwp_linux_dmabuf_v1
#[derive(Clone)]
pub struct QueuedFrame {
    /// Frame content (raw pixels or DMA-BUF reference)
    pub content: FrameContent,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Presentation timestamp (nanoseconds from video start).
    pub pts_ns: Option<u64>,
    /// When this frame was queued (for debugging/metrics).
    pub queued_at: Instant,
}

impl QueuedFrame {
    /// Create a new queued frame with raw pixel data.
    pub fn new(data: Vec<u8>, width: u32, height: u32, pts_ns: Option<u64>) -> Self {
        Self {
            content: FrameContent::Raw(data),
            width,
            height,
            pts_ns,
            queued_at: Instant::now(),
        }
    }

    /// Create a new queued frame from DMA-BUF.
    pub fn new_dmabuf(
        dmabuf: DmaBufFrameData,
        width: u32,
        height: u32,
        pts_ns: Option<u64>,
    ) -> Self {
        Self {
            content: FrameContent::DmaBuf(dmabuf),
            width,
            height,
            pts_ns,
            queued_at: Instant::now(),
        }
    }

    /// Get the DMA-BUF data if this is a DMA-BUF frame.
    pub fn dmabuf(&self) -> Option<&DmaBufFrameData> {
        match &self.content {
            FrameContent::DmaBuf(data) => Some(data),
            _ => None,
        }
    }

    /// Get raw pixel data if this is a raw frame.
    #[cfg(test)]
    pub fn raw_data(&self) -> Option<&[u8]> {
        match &self.content {
            FrameContent::Raw(data) => Some(data),
            _ => None,
        }
    }

    /// Write this frame directly to a destination buffer.
    ///
    /// Returns the number of bytes written, or 0 if buffer is too small or this is DMA-BUF.
    pub fn write_to(&self, dest: &mut [u8]) -> usize {
        match &self.content {
            FrameContent::Raw(data) => {
                let frame_size = data.len();
                if dest.len() < frame_size {
                    return 0;
                }
                dest[..frame_size].copy_from_slice(data);
                frame_size
            }
            FrameContent::DmaBuf(_) => {
                // DMA-BUF frames can't be written to CPU buffers
                0
            }
        }
    }
}

/// Statistics about frame queue operations.
#[derive(Debug, Clone, Default)]
pub struct QueueStats {
    /// Total frames pushed to the queue.
    pub frames_pushed: u64,
    /// Frames dropped due to queue being full (producer side).
    pub frames_dropped_full: u64,
    /// Frames successfully popped by renderer.
    pub frames_popped: u64,
    /// Times renderer reused last frame (queue was empty).
    pub frames_reused: u64,
}

/// A bounded, thread-safe frame queue for video playback.
///
/// This queue is designed for a single-producer (GStreamer callback),
/// single-consumer (Wayland render thread) pattern.
pub struct FrameQueue {
    /// Ring buffer of frames.
    frames: Mutex<Vec<Option<QueuedFrame>>>,
    /// Capacity of the queue.
    capacity: usize,
    /// Write position (producer).
    write_pos: AtomicU64,
    /// Read position (consumer).
    read_pos: AtomicU64,
    /// Number of frames currently in the queue.
    count: AtomicU64,
    /// Last successfully rendered frame (for reuse when queue is empty).
    last_frame: Mutex<Option<QueuedFrame>>,
    /// Whether the queue has been stopped (EOS or error).
    stopped: AtomicBool,
    /// Statistics counters.
    stats_pushed: AtomicU64,
    stats_dropped: AtomicU64,
    stats_popped: AtomicU64,
    stats_reused: AtomicU64,
}

impl FrameQueue {
    /// Create a new frame queue with the specified capacity.
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(2); // Minimum 2 frames
        let frames: Vec<Option<QueuedFrame>> = (0..capacity).map(|_| None).collect();

        Self {
            frames: Mutex::new(frames),
            capacity,
            write_pos: AtomicU64::new(0),
            read_pos: AtomicU64::new(0),
            count: AtomicU64::new(0),
            last_frame: Mutex::new(None),
            stopped: AtomicBool::new(false),
            stats_pushed: AtomicU64::new(0),
            stats_dropped: AtomicU64::new(0),
            stats_popped: AtomicU64::new(0),
            stats_reused: AtomicU64::new(0),
        }
    }

    /// Create a new frame queue with default capacity.
    pub fn with_default_capacity() -> Self {
        Self::new(DEFAULT_QUEUE_CAPACITY)
    }

    /// Push a frame into the queue (producer side).
    ///
    /// If the queue is full, the oldest frame is dropped to make room.
    /// This ensures the producer (GStreamer) never blocks.
    ///
    /// Returns `true` if the frame was added, `false` if queue is stopped.
    pub fn push(&self, frame: QueuedFrame) -> bool {
        if self.stopped.load(Ordering::Acquire) {
            return false;
        }

        let mut frames = match self.frames.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                // Lock contention - drop this frame rather than block
                self.stats_dropped.fetch_add(1, Ordering::Relaxed);
                tracing::trace!(
                    pts_ns = ?frame.pts_ns,
                    age_ms = frame.queued_at.elapsed().as_millis(),
                    "Frame dropped: lock contention"
                );
                return true; // Not stopped, just contention
            }
        };

        let current_count = self.count.load(Ordering::Acquire);

        // If queue is full, drop oldest frame
        if current_count >= self.capacity as u64 {
            // Advance read position to drop oldest
            let old_read = self.read_pos.fetch_add(1, Ordering::AcqRel);
            let drop_idx = (old_read % self.capacity as u64) as usize;
            frames[drop_idx] = None;
            self.count.fetch_sub(1, Ordering::AcqRel);
            self.stats_dropped.fetch_add(1, Ordering::Relaxed);
            tracing::trace!(
                pts_ns = ?frame.pts_ns,
                age_ms = frame.queued_at.elapsed().as_millis(),
                "Frame dropped: queue full"
            );
        }

        // Write new frame
        let write_idx = (self.write_pos.load(Ordering::Acquire) % self.capacity as u64) as usize;
        frames[write_idx] = Some(frame);
        self.write_pos.fetch_add(1, Ordering::Release);
        self.count.fetch_add(1, Ordering::Release);
        self.stats_pushed.fetch_add(1, Ordering::Relaxed);

        true
    }

    /// Try to pop a frame from the queue (consumer side).
    ///
    /// Returns `Some(frame)` if a frame is available, `None` otherwise.
    /// This method NEVER blocks - it returns immediately.
    pub fn try_pop(&self) -> Option<QueuedFrame> {
        if self.count.load(Ordering::Acquire) == 0 {
            return None;
        }

        let mut frames = match self.frames.try_lock() {
            Ok(guard) => guard,
            Err(_) => return None, // Lock contention - return None, don't block
        };

        // Double-check count after acquiring lock
        if self.count.load(Ordering::Acquire) == 0 {
            return None;
        }

        let read_idx = (self.read_pos.load(Ordering::Acquire) % self.capacity as u64) as usize;
        let frame = frames[read_idx].take();

        if frame.is_some() {
            self.read_pos.fetch_add(1, Ordering::Release);
            self.count.fetch_sub(1, Ordering::Release);
            self.stats_popped.fetch_add(1, Ordering::Relaxed);

            // Cache this frame as the last rendered frame
            if let Ok(mut last) = self.last_frame.try_lock() {
                *last = frame.clone();
            }
        }

        frame
    }

    /// Get a frame for rendering - tries queue first, falls back to last frame.
    ///
    /// This is the main method for the render loop. It:
    /// 1. Tries to pop a new frame from the queue
    /// 2. If queue is empty, reuses the last successfully rendered frame
    /// 3. Returns None only if no frame has ever been received
    ///
    /// This ensures smooth playback even when decode hiccups cause queue underruns.
    pub fn get_render_frame(&self) -> Option<QueuedFrame> {
        // Try to get a new frame first
        if let Some(frame) = self.try_pop() {
            return Some(frame);
        }

        // Queue empty - reuse last frame
        self.stats_reused.fetch_add(1, Ordering::Relaxed);

        if let Ok(last) = self.last_frame.try_lock() {
            last.clone()
        } else {
            None
        }
    }

    /// Write a frame directly to a destination buffer.
    ///
    /// This combines `get_render_frame()` with the frame write operation,
    /// returning `Some((width, height))` on success.
    pub fn write_frame_to(&self, dest: &mut [u8]) -> Option<(u32, u32)> {
        let frame = self.get_render_frame()?;

        if frame.write_to(dest) > 0 {
            Some((frame.width, frame.height))
        } else {
            None
        }
    }

    /// Get the dimensions of the last frame (if any).
    pub fn last_frame_dimensions(&self) -> Option<(u32, u32)> {
        self.last_frame
            .try_lock()
            .ok()?
            .as_ref()
            .map(|f| (f.width, f.height))
    }

    /// Check if the queue has been stopped.
    #[cfg(test)]
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    /// Stop the queue (called on EOS or error).
    #[cfg(test)]
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }

    /// Reset the queue for pipeline restart (test only).
    #[cfg(test)]
    pub fn reset(&self) {
        self.stopped.store(false, Ordering::Release);
        self.count.store(0, Ordering::Release);
        self.read_pos.store(0, Ordering::Release);
        self.write_pos.store(0, Ordering::Release);

        if let Ok(mut frames) = self.frames.try_lock() {
            for frame in frames.iter_mut() {
                *frame = None;
            }
        }
    }

    /// Get current queue length.
    pub fn len(&self) -> usize {
        self.count.load(Ordering::Acquire) as usize
    }

    /// Check if queue is empty.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get queue statistics.
    pub fn stats(&self) -> QueueStats {
        QueueStats {
            frames_pushed: self.stats_pushed.load(Ordering::Relaxed),
            frames_dropped_full: self.stats_dropped.load(Ordering::Relaxed),
            frames_popped: self.stats_popped.load(Ordering::Relaxed),
            frames_reused: self.stats_reused.load(Ordering::Relaxed),
        }
    }
}

impl Default for FrameQueue {
    fn default() -> Self {
        Self::with_default_capacity()
    }
}

// Thread-safe: uses atomic operations and mutex-protected data
unsafe impl Send for FrameQueue {}
unsafe impl Sync for FrameQueue {}

/// Shared handle to a frame queue.
pub type SharedFrameQueue = Arc<FrameQueue>;

/// Create a new shared frame queue.
pub fn new_shared_queue(capacity: usize) -> SharedFrameQueue {
    Arc::new(FrameQueue::new(capacity))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_push_pop() {
        let queue = FrameQueue::new(3);

        let frame = QueuedFrame::new(vec![1, 2, 3, 4], 1, 1, None);
        assert!(queue.push(frame));
        assert_eq!(queue.len(), 1);

        let popped = queue.try_pop().unwrap();
        assert_eq!(popped.raw_data(), Some(&[1, 2, 3, 4][..]));
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn test_queue_full_drops_oldest() {
        let queue = FrameQueue::new(2);

        queue.push(QueuedFrame::new(vec![1], 1, 1, Some(1)));
        queue.push(QueuedFrame::new(vec![2], 1, 1, Some(2)));
        assert_eq!(queue.len(), 2);

        // This should drop frame 1 and add frame 3
        queue.push(QueuedFrame::new(vec![3], 1, 1, Some(3)));
        assert_eq!(queue.len(), 2);

        // First pop should get frame 2 (frame 1 was dropped)
        let frame = queue.try_pop().unwrap();
        assert_eq!(frame.pts_ns, Some(2));
    }

    #[test]
    fn test_get_render_frame_reuses_last() {
        let queue = FrameQueue::new(2);

        queue.push(QueuedFrame::new(vec![1, 2, 3, 4], 1, 1, None));

        // First get_render_frame pops from queue
        let frame1 = queue.get_render_frame().unwrap();
        assert_eq!(frame1.raw_data(), Some(&[1, 2, 3, 4][..]));
        assert!(queue.is_empty());

        // Second get_render_frame reuses last frame
        let frame2 = queue.get_render_frame().unwrap();
        assert_eq!(frame2.raw_data(), Some(&[1, 2, 3, 4][..]));

        let stats = queue.stats();
        assert_eq!(stats.frames_reused, 1);
    }

    #[test]
    fn test_stop_prevents_push() {
        let queue = FrameQueue::new(2);

        queue.push(QueuedFrame::new(vec![1], 1, 1, None));
        assert_eq!(queue.len(), 1);

        queue.stop();
        assert!(!queue.push(QueuedFrame::new(vec![2], 1, 1, None)));
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn test_reset_clears_queue() {
        let queue = FrameQueue::new(2);

        queue.push(QueuedFrame::new(vec![1], 1, 1, None));
        queue.push(QueuedFrame::new(vec![2], 1, 1, None));
        queue.stop();

        queue.reset();

        assert!(!queue.is_stopped());
        assert!(queue.is_empty());
        // But last_frame should still be available for smooth restart
    }

    #[test]
    fn test_write_frame_to_buffer() {
        let queue = FrameQueue::new(2);

        queue.push(QueuedFrame::new(vec![1, 2, 3, 4], 1, 1, None));

        let mut buffer = [0u8; 4];
        let dims = queue.write_frame_to(&mut buffer).unwrap();

        assert_eq!(dims, (1, 1));
        assert_eq!(buffer, [1, 2, 3, 4]);
    }

    #[test]
    fn test_stats_tracking() {
        let queue = FrameQueue::new(2);

        queue.push(QueuedFrame::new(vec![1], 1, 1, None));
        queue.push(QueuedFrame::new(vec![2], 1, 1, None));
        queue.push(QueuedFrame::new(vec![3], 1, 1, None)); // Drops oldest (frame 1)

        // Queue now has: [2, 3]
        queue.try_pop(); // Pops frame 2
        queue.try_pop(); // Pops frame 3
        // Queue is now empty
        queue.get_render_frame(); // Reuses last frame (frame 3)

        let stats = queue.stats();
        assert_eq!(stats.frames_pushed, 3);
        assert_eq!(stats.frames_dropped_full, 1);
        assert_eq!(stats.frames_popped, 2);
        assert_eq!(stats.frames_reused, 1);
    }
}
