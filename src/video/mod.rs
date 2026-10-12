//! Video playback for `video` layout nodes.
//!
//! Each mounted node is a [`VideoView`] that owns one GStreamer pipeline
//! ([`player`]) and paints its frames into a canvas ([`paint`]). The view
//! re-renders itself per frame, so a playing clip never re-renders the rest of
//! the layout.
//!
//! Frames reach the screen one of two ways, decided once per process:
//!
//! - **DMA-BUF** — VA-API decodes, `vapostproc` converts to RGB on the GPU's
//!   video engine, and the frame is imported straight into a GL texture. No
//!   pixel crosses the CPU, which is what makes 4K on an integrated GPU work.
//!   Needs VA-API and Freya's OpenGL renderer.
//! - **Raster** — anything else: frames are converted to RGBA in system memory
//!   and copied. Fine at 1080p, heavy at 4K, but it always works.

pub mod clock;
mod paint;
mod player;
mod playlist;
pub mod sync;
mod view;

use std::sync::OnceLock;

use gstreamer as gst;

pub use view::VideoView;

/// How decoded frames are handed to the painter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameKind {
    /// Zero-copy, as a DMA-BUF in this `fourcc:modifier` DRM format.
    Dmabuf(String),
    /// RGBA in system memory.
    Raster,
}

/// Initialize GStreamer. `false` means video nodes render a notice instead of
/// playing; nothing else in the app depends on it.
pub fn init() -> bool {
    match gst::init() {
        Ok(()) => {
            log::info!("video: frames via {:?}", frame_kind());
            true
        }
        Err(e) => {
            log::warn!("video: GStreamer is unavailable, `video` nodes will not play: {e}");
            false
        }
    }
}

pub fn available() -> bool {
    gst::INITIALIZED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The frame path this process uses; probed once.
pub fn frame_kind() -> FrameKind {
    static KIND: OnceLock<FrameKind> = OnceLock::new();
    KIND.get_or_init(|| {
        if std::env::var("FREYA_RENDERER").as_deref() != Ok("opengl") {
            log::info!("video: Freya is not on its OpenGL renderer, copying frames");
            return FrameKind::Raster;
        }
        match dmabuf_format() {
            Ok(format) => FrameKind::Dmabuf(format),
            Err(why) => {
                log::info!("video: no zero-copy path ({why}), copying frames");
                FrameKind::Raster
            }
        }
    })
    .clone()
}

/// Packed RGB DRM fourccs in preference order. The painter imports any of them
/// the same way, so the order only breaks ties.
const RGB_FOURCCS: [&str; 4] = ["XB24", "AB24", "XR24", "AR24"];

/// The first packed-RGB `fourcc:modifier` vapostproc can export.
///
/// The modifier is the driver's tiling layout — radeonsi offers *only* tiled
/// RGB, with no linear option — and caps must name it exactly, so it is read
/// from vapostproc's pad template, which lists what this GPU produces, rather
/// than hard-coded.
fn dmabuf_format() -> Result<String, String> {
    let factory = gst::ElementFactory::find("vapostproc").ok_or("no VA-API postprocessor")?;
    let caps = factory
        .static_pad_templates()
        .into_iter()
        .find(|t| t.direction() == gst::PadDirection::Src)
        .ok_or("vapostproc has no source pad")?
        .caps();
    let offered: Vec<String> = caps
        .iter_with_features()
        .filter(|(_, features)| features.contains("memory:DMABuf"))
        .flat_map(|(s, _)| match s.value("drm-format") {
            Ok(v) => match v.get::<gst::List>() {
                Ok(list) => list.iter().filter_map(|v| v.get::<String>().ok()).collect(),
                Err(_) => v.get::<String>().into_iter().collect(),
            },
            Err(_) => Vec::new(),
        })
        .collect();
    RGB_FOURCCS
        .iter()
        .find_map(|fourcc| {
            offered
                .iter()
                .find(|f| f.split(':').next() == Some(fourcc))
                .cloned()
        })
        .ok_or_else(|| format!("vapostproc exports no packed RGB DMA-BUF (offers {offered:?})"))
}
