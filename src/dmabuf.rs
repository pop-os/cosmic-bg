// SPDX-License-Identifier: MPL-2.0

//! DMA-BUF zero-copy GPU rendering support.
//!
//! This module provides true zero-copy video rendering via the linux-dmabuf protocol:
//! ```
//! GPU decode → DMA-BUF fd → zwp_linux_dmabuf_v1 → Compositor
//! ```
//!
//! ## Performance Benefits
//!
//! - **Zero CPU copies**: Video stays in GPU memory throughout
//! - **~0.2-0.5ms per frame**: Compared to 3-5ms for wl_shm path
//! - **Lower memory bandwidth**: No GPU→CPU→GPU roundtrip
//!
//! ## GPU Vendor Support
//!
//! | Vendor | Decoder | DMA-BUF Export | Performance |
//! |--------|---------|----------------|-------------|
//! | AMD    | VAAPI   | ✅ Native      | Excellent   |
//! | Intel  | VAAPI   | ✅ Native      | Excellent   |
//! | NVIDIA | NVDEC   | ✅ Via plugin  | Excellent   |
//! | ARM    | V4L2    | ✅ Native      | Good        |
//!
//! NVIDIA note: NVDEC outputs CUDAMemory which requires conversion to DMA-BUF.
//! Falls back to wl_shm if conversion fails.

use std::os::fd::OwnedFd;
use std::sync::Arc;

use sctk::reexports::client::{QueueHandle, protocol::wl_buffer::WlBuffer};
use tracing::{debug, warn};

// We'll use wayland-client directly for linux-dmabuf protocol
// since sctk doesn't provide high-level bindings yet
use sctk::reexports::client::globals::GlobalList;
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};

/// DMA-BUF format and modifier information
#[derive(Debug, Clone, Copy)]

pub struct DmaBufFormat {
    pub fourcc: u32,
    pub modifier: u64,
}

// NVIDIA tiled modifiers reference (unused but kept for documentation):
// Format: 0x03SSHHBBBBBBBBBB where:
// - SS = sector layout (0x06 for 16Bx2, 0x0e for 32Bx2)
// - HH = height log2 (0x00-0x05)
// - BB = block-linear base
// 16Bx2: 0x0300000000606010..0x0300000000606015
// 32Bx2: 0x0300000000e08010..0x0300000000e08015

impl DmaBufFormat {
    /// Create AR24 (ARGB8888) format - most widely supported
    pub fn ar24() -> Self {
        Self {
            fourcc: drm_fourcc::DrmFourcc::Argb8888 as u32,
            modifier: drm_fourcc::DrmModifier::Linear.into(),
        }
    }

    /// Create XR24 (XRGB8888) format - common for video
    pub fn xr24() -> Self {
        Self {
            fourcc: drm_fourcc::DrmFourcc::Xrgb8888 as u32,
            modifier: drm_fourcc::DrmModifier::Linear.into(),
        }
    }

    /// Create NV12 format - efficient for video (YUV 4:2:0)
    pub fn nv12() -> Self {
        Self {
            fourcc: drm_fourcc::DrmFourcc::Nv12 as u32,
            modifier: drm_fourcc::DrmModifier::Linear.into(),
        }
    }

    /// Create AB24 (ABGR8888) format - NVIDIA GL DMA-BUF output (RGBA → ABGR)
    pub fn ab24() -> Self {
        Self {
            fourcc: drm_fourcc::DrmFourcc::Abgr8888 as u32,
            modifier: drm_fourcc::DrmModifier::Linear.into(),
        }
    }

    /// Create NV12 format with NVIDIA tiled modifier (from cudadmabufupload)
    pub fn nv12_nvidia_tiled(modifier: u64) -> Self {
        Self {
            fourcc: drm_fourcc::DrmFourcc::Nv12 as u32,
            modifier,
        }
    }

    /// Create XR24 (XRGB8888/BGRx) format with NVIDIA tiled modifier
    pub fn xr24_nvidia_tiled(modifier: u64) -> Self {
        Self {
            fourcc: drm_fourcc::DrmFourcc::Xrgb8888 as u32,
            modifier,
        }
    }

    /// Detect format from GStreamer video format string or DRM format string.
    /// Handles NVIDIA tiled modifiers from cudadmabufupload (e.g., "NV12:0x0300000000606010").
    pub fn from_gst_format(format_str: &str) -> Self {
        // Handle DRM format strings with modifiers (e.g., "NV12:0x0300000000606010")
        if let Some((format, modifier_str)) = format_str.split_once(':') {
            let modifier = if modifier_str.starts_with("0x") || modifier_str.starts_with("0X") {
                u64::from_str_radix(&modifier_str[2..], 16).unwrap_or(0)
            } else {
                modifier_str.parse::<u64>().unwrap_or(0)
            };

            return match format {
                "NV12" => Self::nv12_nvidia_tiled(modifier),
                "XR24" => Self::xr24_nvidia_tiled(modifier),
                "XB24" => Self {
                    fourcc: drm_fourcc::DrmFourcc::Xbgr8888 as u32,
                    modifier,
                },
                "AR24" => Self {
                    fourcc: drm_fourcc::DrmFourcc::Argb8888 as u32,
                    modifier,
                },
                "AB24" => Self {
                    fourcc: drm_fourcc::DrmFourcc::Abgr8888 as u32,
                    modifier,
                },
                _ => {
                    tracing::warn!(
                        format,
                        modifier,
                        "Unknown DRM format with modifier, defaulting to XR24"
                    );
                    Self::xr24_nvidia_tiled(modifier)
                }
            };
        }

        // Simple format strings without modifiers
        match format_str {
            "BGRx" | "BGRX" => Self::xr24(),
            "RGBA" => Self::ab24(), // RGBA in GStreamer = ABGR8888 in DRM
            "ARGB" => Self::ar24(),
            "NV12" => Self::nv12(),
            // DRM format strings (from vapostproc with memory:DMABuf)
            "XB24" => Self::xb24(), // XBGR8888
            "XR24" => Self::xr24(), // XRGB8888
            "AB24" => Self::ab24(), // ABGR8888
            "AR24" => Self::ar24(), // ARGB8888
            _ => {
                tracing::warn!(format = format_str, "Unknown format, defaulting to XR24");
                Self::xr24()
            }
        }
    }

    /// Create XB24 (XBGR8888) format - VAAPI DMA-BUF output
    pub fn xb24() -> Self {
        Self {
            fourcc: drm_fourcc::DrmFourcc::Xbgr8888 as u32,
            modifier: drm_fourcc::DrmModifier::Linear.into(),
        }
    }
}

/// DMA-BUF plane descriptor
#[derive(Debug, Clone)]

pub struct DmaBufPlane {
    pub fd: Arc<OwnedFd>,
    pub offset: u32,
    pub stride: u32,
}

/// A DMA-BUF backed buffer for zero-copy rendering
#[derive(Debug)]

pub struct DmaBufBuffer {
    pub width: u32,
    pub height: u32,
    pub format: DmaBufFormat,
    pub planes: Vec<DmaBufPlane>,
    pub wl_buffer: Option<WlBuffer>,
}

impl DmaBufBuffer {
    /// Create wl_buffer from DMA-BUF using zwp_linux_dmabuf_v1.
    ///
    /// This performs the actual zero-copy buffer creation:
    /// 1. Create zwp_linux_buffer_params_v1
    /// 2. Add plane(s) with fd, offset, stride, modifier
    /// 3. Create wl_buffer from params
    pub fn create_wl_buffer(
        &mut self,
        dmabuf: &ZwpLinuxDmabufV1,
        qh: &QueueHandle<crate::CosmicBg>,
    ) -> Option<WlBuffer> {
        if self.planes.is_empty() {
            warn!("No planes available for DMA-BUF buffer");
            return None;
        }

        // Create buffer params
        let params: ZwpLinuxBufferParamsV1 = dmabuf.create_params(qh, ());

        // Add each plane
        for (plane_idx, plane) in self.planes.iter().enumerate() {
            use std::os::fd::AsFd;
            params.add(
                plane.fd.as_fd(),
                plane_idx as u32,
                plane.offset,
                plane.stride,
                (self.format.modifier >> 32) as u32, // modifier_hi
                (self.format.modifier & 0xFFFFFFFF) as u32, // modifier_lo
            );
        }

        // Create the wl_buffer
        let wl_buffer = params.create_immed(
            self.width as i32,
            self.height as i32,
            self.format.fourcc,
            zwp_linux_buffer_params_v1::Flags::empty(),
            qh,
            (),
        );

        debug!(
            width = self.width,
            height = self.height,
            fourcc = self.format.fourcc,
            planes = self.planes.len(),
            "Created DMA-BUF wl_buffer"
        );

        self.wl_buffer = Some(wl_buffer.clone());
        Some(wl_buffer)
    }
}

/// DMA-BUF manager state
#[derive(Debug)]
pub struct DmaBufState {
    /// The zwp_linux_dmabuf_v1 global (if available)
    pub dmabuf_global: Option<ZwpLinuxDmabufV1>,
}

impl Default for DmaBufState {
    fn default() -> Self {
        Self::new()
    }
}

impl DmaBufState {
    /// Create a new DMA-BUF state
    pub fn new() -> Self {
        Self {
            dmabuf_global: None,
        }
    }

    /// Bind to zwp_linux_dmabuf_v1 global and store it for buffer creation.
    ///
    /// Returns the bound global if available, None otherwise.
    pub fn bind_global(
        globals: &GlobalList,
        qh: &QueueHandle<crate::CosmicBg>,
    ) -> Option<ZwpLinuxDmabufV1> {
        // Check if zwp_linux_dmabuf_v1 is advertised
        let global_list = globals.contents().clone_list();
        let dmabuf_global = global_list
            .iter()
            .find(|g| g.interface == "zwp_linux_dmabuf_v1")?;

        // Bind the global at version 3 (supports modifiers)
        let version = dmabuf_global.version.min(3);
        let dmabuf: ZwpLinuxDmabufV1 = globals.registry().bind(dmabuf_global.name, version, qh, ());

        debug!(version, "DMA-BUF protocol (zwp_linux_dmabuf_v1) available");

        Some(dmabuf)
    }
}
